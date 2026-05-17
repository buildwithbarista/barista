//! Minimal HAR (HTTP Archive) validator.
//!
//! Scope is **deliberately limited**: this module exists to answer a
//! single question after `CaptureSession::stop` returns — *did the
//! capture produce a structurally sound HAR file?* It does **not**
//! interpret entries, surface timing data, or model the HAR 1.2 spec.
//! That richer analysis is owned by `barista-netanalyze` (Workstream B
//! Task 2). Doing it twice would mean two crates drift out of sync the
//! first time HAR 1.2 ships an erratum.
//!
//! ## What "structurally sound" means here
//!
//! 1. The file exists.
//! 2. It is non-empty.
//! 3. It parses as JSON.
//! 4. The top level is an object containing a `log` object.
//! 5. `log.entries` is an array (possibly empty — empty is valid for
//!    sessions where the build tool happened not to make any requests,
//!    which is rare but legal).
//!
//! Anything richer than that is a job for the analysis pipeline.

use std::path::{Path, PathBuf};

use crate::error::NetcapError;

/// Lightweight summary of a HAR file's surface shape — enough to gate
/// `CaptureSession::stop` on a "did we capture anything?" check, no more.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HarSummary {
    /// Absolute path of the HAR file that produced this summary.
    pub path: PathBuf,
    /// Number of entries in `log.entries`. A non-zero count is the
    /// strongest signal that the capture round-tripped at least one
    /// request.
    pub entry_count: usize,
}

/// Open the HAR at `path`, run the structural checks listed in the
/// module docs, and return a [`HarSummary`].
pub fn validate(path: &Path) -> Result<HarSummary, NetcapError> {
    if !path.exists() {
        return Err(NetcapError::HarInvalid {
            path: path.to_path_buf(),
            reason: "file does not exist".to_string(),
        });
    }

    let bytes = std::fs::read(path).map_err(|source| NetcapError::HarInvalid {
        path: path.to_path_buf(),
        reason: format!("could not read file: {source}"),
    })?;

    if bytes.is_empty() {
        return Err(NetcapError::HarInvalid {
            path: path.to_path_buf(),
            reason: "file is empty (mitmproxy may have exited before flushing)".to_string(),
        });
    }

    let value: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|source| NetcapError::HarInvalid {
            path: path.to_path_buf(),
            reason: format!("not valid JSON: {source}"),
        })?;

    let log = value.get("log").ok_or_else(|| NetcapError::HarInvalid {
        path: path.to_path_buf(),
        reason: "missing top-level `log` object (not a HAR file?)".to_string(),
    })?;

    let entries = log.get("entries").ok_or_else(|| NetcapError::HarInvalid {
        path: path.to_path_buf(),
        reason: "missing `log.entries` array".to_string(),
    })?;

    let entry_count = match entries.as_array() {
        Some(arr) => arr.len(),
        None => {
            return Err(NetcapError::HarInvalid {
                path: path.to_path_buf(),
                reason: "`log.entries` is not an array".to_string(),
            });
        }
    };

    Ok(HarSummary {
        path: path.to_path_buf(),
        entry_count,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::io::Write;

    use tempfile::NamedTempFile;

    fn write_har(body: &str) -> NamedTempFile {
        let mut f = NamedTempFile::new().expect("tempfile");
        f.write_all(body.as_bytes()).expect("write");
        f
    }

    #[test]
    fn validates_minimal_har() {
        let f = write_har(r#"{"log":{"version":"1.2","entries":[]}}"#);
        let summary = validate(f.path()).expect("validate");
        assert_eq!(summary.entry_count, 0);
    }

    #[test]
    fn counts_entries() {
        let f = write_har(
            r#"{"log":{"version":"1.2","entries":[
                {"request":{}},{"request":{}},{"request":{}}
            ]}}"#,
        );
        let summary = validate(f.path()).expect("validate");
        assert_eq!(summary.entry_count, 3);
    }

    #[test]
    fn rejects_non_json() {
        let f = write_har("this is not JSON");
        let err = validate(f.path()).expect_err("should fail");
        assert!(matches!(err, NetcapError::HarInvalid { .. }));
    }

    #[test]
    fn rejects_missing_log() {
        let f = write_har(r#"{"not_a_har":true}"#);
        let err = validate(f.path()).expect_err("should fail");
        match err {
            NetcapError::HarInvalid { reason, .. } => assert!(reason.contains("log")),
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn rejects_empty_file() {
        let f = write_har("");
        let err = validate(f.path()).expect_err("should fail");
        match err {
            NetcapError::HarInvalid { reason, .. } => assert!(reason.contains("empty")),
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn rejects_nonexistent_file() {
        let path = PathBuf::from("/this/path/does/not/exist.har");
        let err = validate(&path).expect_err("should fail");
        assert!(matches!(err, NetcapError::HarInvalid { .. }));
    }
}
