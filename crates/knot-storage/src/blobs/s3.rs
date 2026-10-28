//! S3-compatible blob backend.
//!
//! Built on the `rust-s3` crate (`s3`). Works with AWS S3, MinIO, Cloudflare
//! R2, Backblaze B2, Wasabi, Hetzner Object Storage, and anything else that
//! speaks the S3 API. Always compiled in — runtime-selected via
//! `KNOT_BLOB_BACKEND=s3`.
//!
//! Credentials: callers pass an `s3::creds::Credentials`. Typically built
//! from env vars (`AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, optional
//! `AWS_SESSION_TOKEN`) via `Credentials::default()`.

use std::str::FromStr;

use async_trait::async_trait;
use s3::Bucket;
use s3::Region;
use s3::creds::Credentials;
use s3::error::S3Error;
use s3::request::ResponseData;
use uuid::Uuid;

use super::{BlobStore, BlobStoreError, Result};

pub struct S3Store {
    bucket: Box<Bucket>,
    prefix: String,
}

impl S3Store {
    /// Build from a bucket name, region, endpoint, prefix.
    ///
    /// Credentials are read from the standard AWS env vars
    /// (`AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, optional
    /// `AWS_SESSION_TOKEN`). On platforms with instance profiles / IRSA
    /// the env vars are usually injected for you.
    ///
    /// - Empty `endpoint` means native AWS S3 in the given region.
    /// - Non-empty `endpoint` forces path-style addressing (required by
    ///   MinIO and most S3-compatible providers).
    pub fn from_env(
        bucket_name: String,
        region: String,
        endpoint: String,
        prefix: String,
    ) -> std::result::Result<Self, BlobStoreError> {
        let creds = Credentials::default()
            .map_err(|e| BlobStoreError::Backend(format!("s3 credentials: {e}")))?;
        Self::new(bucket_name, region, endpoint, prefix, creds)
    }

    /// Build with an explicit set of credentials.
    pub fn new(
        bucket_name: String,
        region: String,
        endpoint: String,
        prefix: String,
        creds: Credentials,
    ) -> std::result::Result<Self, BlobStoreError> {
        let region = if endpoint.is_empty() {
            Region::from_str(&region).unwrap_or(Region::UsEast1)
        } else {
            Region::Custom { region, endpoint }
        };
        let bucket = Bucket::new(&bucket_name, region, creds)
            .map_err(|e| BlobStoreError::Backend(format!("s3 bucket: {e}")))?
            .with_path_style();
        Ok(Self { bucket, prefix })
    }

    fn key(&self, id: Uuid) -> String {
        if self.prefix.is_empty() {
            id.to_string()
        } else {
            format!("{}/{}", self.prefix.trim_end_matches('/'), id)
        }
    }

    /// The size of the object at `key` (`None` when the store doesn't say),
    /// read with `HeadObject`. `NotFound` only for a 404 (see `head_size`).
    async fn size(&self, key: &str) -> Result<Option<i64>> {
        let head = self.bucket.head_object(key).await;
        head_size(
            key,
            head.map(|(head, status)| (head.content_length, status)),
        )
    }
}

/// What a `HeadObject` of `key` answered: its `Content-Length` and status,
/// or a request that failed.
///
/// Only a 404 status means the object is missing. Without rust-s3's
/// `fail-on-err` feature (off here), every status arrives as `Ok`, so an
/// `Err` is a transport or signing failure. Its text holds the URL, whose
/// key can contain "404", so it is never read as `NotFound`.
fn head_size(
    key: &str,
    head: std::result::Result<(Option<i64>, u16), S3Error>,
) -> Result<Option<i64>> {
    let (size, status) =
        head.map_err(|e| BlobStoreError::Backend(format!("s3 head {key}: {e}")))?;
    match status {
        200..=299 => Ok(size),
        404 => Err(BlobStoreError::NotFound),
        status => Err(BlobStoreError::Backend(format!(
            "s3 head {key}: status {status}"
        ))),
    }
}

/// The file's bytes, from what a `GetObject` of `key` answered.
///
/// As for `head_size`, only a 404 status means the object is missing, and
/// an `Err` is a request that failed, whose text (the URL, with a key that
/// can contain "404") says nothing about the object. Any other status that
/// isn't 2xx is a refusal or a failure: its body is the store's error
/// document, not the file.
fn got(key: &str, resp: std::result::Result<ResponseData, S3Error>) -> Result<Vec<u8>> {
    let resp = resp.map_err(|e| BlobStoreError::Backend(format!("s3 get {key}: {e}")))?;
    match resp.status_code() {
        200..=299 => Ok(resp.to_vec()),
        404 => Err(BlobStoreError::NotFound),
        status => Err(BlobStoreError::Backend(format!(
            "s3 get {key}: status {status}"
        ))),
    }
}

/// Whether the copy of `from` (`size` bytes, by its HEAD) arrived at `to`,
/// given the HEAD of `to`. Only two sizes that are known and equal count:
/// without them, a failed copy can't be told from a good one.
fn copied(from: &str, to: &str, size: Option<i64>, head: Result<Option<i64>>) -> Result<()> {
    match (size, head) {
        (Some(size), Ok(Some(copied))) if copied == size => Ok(()),
        (_, Err(BlobStoreError::NotFound)) => Err(BlobStoreError::Backend(format!(
            "s3 copy {from} to {to}: no object after the copy"
        ))),
        (_, Err(e)) => Err(e),
        (size, Ok(copied)) => Err(BlobStoreError::Backend(format!(
            "s3 copy {from} to {to}: {copied:?} bytes, expected {size:?}"
        ))),
    }
}

/// What a `DeleteObject` status means. A 404 is fine: the object is gone
/// either way. Anything else that isn't 2xx (a 403, a 5xx) is a refusal, so
/// the caller keeps its record of the object and tries again later.
fn deleted(key: &str, status: u16) -> Result<()> {
    match status {
        200..=299 | 404 => Ok(()),
        status => Err(BlobStoreError::Backend(format!(
            "s3 delete {key}: status {status}"
        ))),
    }
}

#[async_trait]
impl BlobStore for S3Store {
    async fn put(&self, id: Uuid, bytes: &[u8], content_type: &str) -> Result<()> {
        let key = self.key(id);
        self.bucket
            .put_object_with_content_type(&key, bytes, content_type)
            .await
            .map_err(|e| BlobStoreError::Backend(format!("s3 put {key}: {e}")))?;
        Ok(())
    }

    async fn get(&self, id: Uuid) -> Result<Vec<u8>> {
        let key = self.key(id);
        got(&key, self.bucket.get_object(&key).await)
    }

    async fn delete(&self, id: Uuid) -> Result<()> {
        let key = self.key(id);
        let resp = self
            .bucket
            .delete_object(&key)
            .await
            .map_err(|e| BlobStoreError::Backend(format!("s3 delete {key}: {e}")))?;
        deleted(&key, resp.status_code())
    }

    /// A server-side `CopyObject`, so the bytes never pass through knot.
    /// The content type travels with the object. CopyObject can answer 200
    /// with an error in its body, so the copy only counts once the HEADs of
    /// `src` and `dst` report the same size (`copied`).
    async fn copy(&self, src: Uuid, dst: Uuid, _content_type: &str) -> Result<()> {
        let (from, to) = (self.key(src), self.key(dst));
        let size = self.size(&from).await?;
        let status = self
            .bucket
            .copy_object_internal(&from, &to)
            .await
            .map_err(|e| BlobStoreError::Backend(format!("s3 copy {from} to {to}: {e}")))?;
        if !(200..300).contains(&status) {
            return Err(BlobStoreError::Backend(format!(
                "s3 copy {from} to {to}: status {status}"
            )));
        }
        copied(&from, &to, size, self.size(&to).await)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "knot/bf404a1e-0c4d-4b1e-9f3a-2d6e8c7b5a40";

    fn is_backend<T: std::fmt::Debug>(result: &Result<T>) -> bool {
        matches!(result, Err(BlobStoreError::Backend(_)))
    }

    fn is_not_found<T: std::fmt::Debug>(result: &Result<T>) -> bool {
        matches!(result, Err(BlobStoreError::NotFound))
    }

    /// A request that failed carries its URL in its text, and a key's UUID
    /// contains "404" now and then (as `KEY` does). It says nothing about
    /// the object.
    #[test]
    fn a_failed_head_request_is_a_backend_error() {
        for text in [
            format!("error sending request for url (https://s3.example/{KEY}): connection reset"),
            "operation timed out".to_string(),
        ] {
            let failed = Err(S3Error::Io(std::io::Error::other(text.clone())));
            let result = head_size(KEY, failed);
            assert!(is_backend(&result), "{text}: {result:?}");
        }
    }

    #[test]
    fn head_status_decides_the_size() {
        assert!(matches!(head_size(KEY, Ok((Some(5), 200))), Ok(Some(5))));
        assert!(matches!(head_size(KEY, Ok((None, 200))), Ok(None)));
        assert!(is_not_found(&head_size(KEY, Ok((None, 404)))));
        for status in [301, 403, 500, 503] {
            let result = head_size(KEY, Ok((Some(5), status)));
            assert!(is_backend(&result), "{status}: {result:?}");
        }
    }

    /// A request that failed says nothing about the object, whatever its
    /// text holds: the URL, whose key can contain "404", or a message that
    /// mentions "not found" or "NoSuchKey".
    #[test]
    fn a_failed_get_request_is_a_backend_error() {
        for text in [
            format!("error sending request for url (https://s3.example/{KEY}): connection reset"),
            "dns error: host not found".to_string(),
            "NoSuchKey retry budget exhausted".to_string(),
        ] {
            let failed = Err(S3Error::Io(std::io::Error::other(text.clone())));
            let result = got(KEY, failed);
            assert!(is_backend(&result), "{text}: {result:?}");
        }
    }

    /// Only a 404 means the object is missing, and only a 2xx carries its
    /// bytes. Without `fail-on-err` every other status arrives as `Ok`, its
    /// body an error document, not the file.
    #[test]
    fn get_status_decides_the_result() {
        let answer = |status: u16, body: &[u8]| {
            Ok(ResponseData::new(
                body.to_vec().into(),
                status,
                std::collections::HashMap::new(),
            ))
        };
        assert_eq!(got(KEY, answer(200, b"bytes")).unwrap(), b"bytes");
        assert!(is_not_found(&got(
            KEY,
            answer(404, b"<Error>NoSuchKey</Error>")
        )));
        for status in [301, 403, 500, 503] {
            let result = got(KEY, answer(status, b"<Error>AccessDenied</Error>"));
            assert!(is_backend(&result), "{status}: {result:?}");
        }
    }

    /// The copy counts only when both HEADs report the same size.
    #[test]
    fn a_copy_counts_only_with_two_equal_known_sizes() {
        assert!(copied("a", "b", Some(5), Ok(Some(5))).is_ok());
        for (size, head) in [
            (None, Ok(None)),
            (Some(5), Ok(None)),
            (None, Ok(Some(5))),
            (Some(5), Ok(Some(4))),
            (Some(5), Err(BlobStoreError::NotFound)),
            (Some(5), Err(BlobStoreError::Backend("timeout".into()))),
        ] {
            let shown = format!("{size:?}, {head:?}");
            let result = copied("a", "b", size, head);
            assert!(is_backend(&result), "{shown}: {result:?}");
        }
    }

    /// A refused delete is an error, so the purge keeps the entry and tries
    /// again; an object already gone is fine.
    #[test]
    fn delete_status_decides_the_result() {
        for status in [200, 204, 404] {
            assert!(deleted(KEY, status).is_ok(), "{status}");
        }
        for status in [301, 400, 403, 409, 500, 503] {
            let result = deleted(KEY, status);
            assert!(is_backend(&result), "{status}: {result:?}");
        }
    }
}
