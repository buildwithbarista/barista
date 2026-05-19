//! Stubbed GCS-backed [`Cas`].
//!
//! Same rationale as [`crate::storage::s3`]: the type exists so the
//! trait surface can be exercised by tests and so config files
//! referencing `ROASTERY_STORAGE_BACKEND=gcs` parse cleanly today.
//! Every trait method returns [`StorageError::NotImplemented`] —
//! real Google Cloud Storage support is scheduled for v0.2 behind a
//! Cargo feature flag.

use async_trait::async_trait;

use crate::error::StorageError;
use crate::storage::{Cas, CasReader, Digest, Result, Stat};

/// Stub GCS-backed CAS. Trait methods always return
/// [`StorageError::NotImplemented`].
#[derive(Debug, Clone)]
pub struct GcsCas {
    bucket: String,
    project: String,
}

impl GcsCas {
    /// Construct a stub configured for `bucket` in `project`. Does
    /// **not** contact Google Cloud or validate credentials. Values
    /// are stored verbatim so a future real implementation can lift
    /// `new` into something that does work without changing call
    /// sites.
    pub fn new(bucket: String, project: String) -> Result<Self> {
        Ok(Self { bucket, project })
    }

    /// Bucket this stub was configured for.
    pub fn bucket(&self) -> &str {
        &self.bucket
    }

    /// Project this stub was configured for.
    pub fn project(&self) -> &str {
        &self.project
    }
}

#[async_trait]
impl Cas for GcsCas {
    async fn stat(&self, _digest: Digest) -> Result<Option<Stat>> {
        Err(StorageError::NotImplemented { backend: "gcs" })
    }

    async fn get(&self, _digest: Digest) -> Result<Option<CasReader>> {
        Err(StorageError::NotImplemented { backend: "gcs" })
    }

    async fn put(
        &self,
        _expected_digest: Digest,
        _source: CasReader,
    ) -> Result<Stat> {
        Err(StorageError::NotImplemented { backend: "gcs" })
    }

    async fn delete(&self, _digest: Digest) -> Result<bool> {
        Err(StorageError::NotImplemented { backend: "gcs" })
    }

    async fn list(&self, _prefix: Option<&str>) -> Result<Vec<Digest>> {
        Err(StorageError::NotImplemented { backend: "gcs" })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use std::io::Cursor;

    fn stub() -> GcsCas {
        GcsCas::new("test-bucket".to_string(), "test-project".to_string()).unwrap()
    }

    #[tokio::test]
    async fn gcs_cas_not_implemented() {
        let cas = stub();
        let digest = Digest::of_bytes(b"x");

        let err = cas.stat(digest).await.unwrap_err();
        assert!(matches!(err, StorageError::NotImplemented { backend: "gcs" }));

        match cas.get(digest).await {
            Err(StorageError::NotImplemented { backend: "gcs" }) => {}
            Err(other) => panic!("expected NotImplemented(gcs), got {other:?}"),
            Ok(_) => panic!("expected NotImplemented(gcs), got Ok"),
        }

        let err = cas
            .put(digest, Box::new(Cursor::new(b"x".to_vec())))
            .await
            .unwrap_err();
        assert!(matches!(err, StorageError::NotImplemented { backend: "gcs" }));

        let err = cas.delete(digest).await.unwrap_err();
        assert!(matches!(err, StorageError::NotImplemented { backend: "gcs" }));

        let err = cas.list(None).await.unwrap_err();
        assert!(matches!(err, StorageError::NotImplemented { backend: "gcs" }));
    }

    #[test]
    fn gcs_cas_preserves_construction_params() {
        let cas = stub();
        assert_eq!(cas.bucket(), "test-bucket");
        assert_eq!(cas.project(), "test-project");
    }
}
