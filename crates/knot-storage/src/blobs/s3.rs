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
        let resp = self.bucket.get_object(&key).await.map_err(|e| {
            let s = format!("{e}");
            if s.contains("404")
                || s.to_lowercase().contains("not found")
                || s.contains("NoSuchKey")
            {
                BlobStoreError::NotFound
            } else {
                BlobStoreError::Backend(format!("s3 get {key}: {e}"))
            }
        })?;
        if resp.status_code() == 404 {
            return Err(BlobStoreError::NotFound);
        }
        Ok(resp.bytes().to_vec())
    }

    async fn delete(&self, id: Uuid) -> Result<()> {
        let key = self.key(id);
        self.bucket
            .delete_object(&key)
            .await
            .map_err(|e| BlobStoreError::Backend(format!("s3 delete {key}: {e}")))?;
        Ok(())
    }
}
