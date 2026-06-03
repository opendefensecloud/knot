//! S3-compatible blob backend.
//!
//! Works with native AWS S3, MinIO, Cloudflare R2 — anything that speaks the
//! S3 API. Feature-gated behind `s3` so default builds don't pull the SDK.

use async_trait::async_trait;
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::Client;
use uuid::Uuid;

use super::{BlobStore, BlobStoreError, Result};

pub struct S3Store {
    client: Client,
    bucket: String,
    prefix: String,
}

impl S3Store {
    /// Build with a pre-configured client. The caller is responsible for
    /// loading credentials / endpoint / region via aws-config.
    pub fn new(client: Client, bucket: String, prefix: String) -> Self {
        Self { client, bucket, prefix }
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
        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(self.key(id))
            .body(ByteStream::from(bytes.to_vec()))
            .content_type(content_type)
            .send()
            .await
            .map_err(|e| BlobStoreError::Backend(format!("s3 put: {e}")))?;
        Ok(())
    }

    async fn get(&self, id: Uuid) -> Result<Vec<u8>> {
        let resp = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(self.key(id))
            .send()
            .await
            .map_err(|e| {
                let s = format!("{e}");
                if s.contains("NoSuchKey") || s.contains("NotFound") {
                    BlobStoreError::NotFound
                } else {
                    BlobStoreError::Backend(format!("s3 get: {e}"))
                }
            })?;
        let bytes = resp
            .body
            .collect()
            .await
            .map_err(|e| BlobStoreError::Backend(format!("s3 body: {e}")))?;
        Ok(bytes.to_vec())
    }

    async fn delete(&self, id: Uuid) -> Result<()> {
        self.client
            .delete_object()
            .bucket(&self.bucket)
            .key(self.key(id))
            .send()
            .await
            .map_err(|e| BlobStoreError::Backend(format!("s3 delete: {e}")))?;
        Ok(())
    }
}
