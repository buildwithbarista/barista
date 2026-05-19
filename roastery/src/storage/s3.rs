//! Stubbed S3-backed [`Cas`].
//!
//! The type exists at v0.1 so:
//!
//! 1. The [`Cas`] trait surface can be exercised end-to-end by tests
//!    against a non-filesystem implementor (catches accidental
//!    `FsCas`-specific assumptions in trait-using code).
//! 2. `roastery.toml` / env-var config files can reference an S3
//!    backend (`ROASTERY_STORAGE_BACKEND=s3`) without breaking
//!    parsing. The server still refuses to actually run with this
//!    backend selected — every method returns
//!    [`StorageError::NotImplemented`].
//!
//! Real S3 support is scheduled for v0.2 and will pull in the
//! `aws-sdk-s3` crate behind a Cargo feature flag so the dep doesn't
//! leak into builds that only use the filesystem backend.

use async_trait::async_trait;

use crate::error::StorageError;
use crate::storage::{Cas, CasReader, Digest, Result, Stat};

/// Stub S3-backed CAS. Trait methods always return
/// [`StorageError::NotImplemented`].
#[derive(Debug, Clone)]
pub struct S3Cas {
    bucket: String,
    region: String,
}

impl S3Cas {
    /// Construct a stub configured for `bucket` in `region`. Does
    /// **not** contact AWS or validate credentials — that's a v0.2
    /// concern. The values are stored verbatim so a future real
    /// implementation can lift `new` into something that does work
    /// without changing call sites.
    pub fn new(bucket: String, region: String) -> Result<Self> {
        Ok(Self { bucket, region })
    }

    /// Bucket this stub was configured for.
    pub fn bucket(&self) -> &str {
        &self.bucket
    }

    /// Region this stub was configured for.
    pub fn region(&self) -> &str {
        &self.region
    }
}

#[async_trait]
impl Cas for S3Cas {
    async fn stat(&self, _digest: Digest) -> Result<Option<Stat>> {
        Err(StorageError::NotImplemented { backend: "s3" })
    }

    async fn get(&self, _digest: Digest) -> Result<Option<CasReader>> {
        Err(StorageError::NotImplemented { backend: "s3" })
    }

    async fn put(
        &self,
        _expected_digest: Digest,
        _source: CasReader,
    ) -> Result<Stat> {
        Err(StorageError::NotImplemented { backend: "s3" })
    }

    async fn delete(&self, _digest: Digest) -> Result<bool> {
        Err(StorageError::NotImplemented { backend: "s3" })
    }

    async fn list(&self, _prefix: Option<&str>) -> Result<Vec<Digest>> {
        Err(StorageError::NotImplemented { backend: "s3" })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use std::io::Cursor;

    fn stub() -> S3Cas {
        S3Cas::new("test-bucket".to_string(), "us-east-1".to_string()).unwrap()
    }

    #[tokio::test]
    async fn s3_cas_not_implemented() {
        let cas = stub();
        let digest = Digest::of_bytes(b"x");

        let err = cas.stat(digest).await.unwrap_err();
        assert!(matches!(err, StorageError::NotImplemented { backend: "s3" }));

        match cas.get(digest).await {
            Err(StorageError::NotImplemented { backend: "s3" }) => {}
            Err(other) => panic!("expected NotImplemented(s3), got {other:?}"),
            Ok(_) => panic!("expected NotImplemented(s3), got Ok"),
        }

        let err = cas
            .put(digest, Box::new(Cursor::new(b"x".to_vec())))
            .await
            .unwrap_err();
        assert!(matches!(err, StorageError::NotImplemented { backend: "s3" }));

        let err = cas.delete(digest).await.unwrap_err();
        assert!(matches!(err, StorageError::NotImplemented { backend: "s3" }));

        let err = cas.list(None).await.unwrap_err();
        assert!(matches!(err, StorageError::NotImplemented { backend: "s3" }));
    }

    #[test]
    fn s3_cas_preserves_construction_params() {
        let cas = stub();
        assert_eq!(cas.bucket(), "test-bucket");
        assert_eq!(cas.region(), "us-east-1");
    }
}
