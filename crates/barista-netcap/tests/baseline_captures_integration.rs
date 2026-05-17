#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! `#[ignore]`-gated integration test for the baseline capture
//! driver script (`scripts/run-baseline-captures.sh`).
//!
//! The driver script lives outside this crate (it orchestrates `mvn` /
//! `mvnd` against a real corpus checkout) but the **contract** it
//! upholds — a `capture.har` plus a `metadata.toml` per cell, dropped
//! under `bench/captures/<corpus-id>/<tool>-<version>/<timestamp>/` —
//! is observable from this crate's perspective: any consumer of
//! `barista-netcap` that wraps the same lifecycle should produce the
//! same artifact layout.
//!
//! This test runs the driver against a single small cell
//! (`spring-boot-starter-web-app` × `mvn`) to validate:
//!
//!   1. The driver script exits 0.
//!   2. A `capture.har` exists and is non-empty.
//!   3. The HAR parses as JSON with a `log.entries` array.
//!   4. The `metadata.toml` exists and contains the required keys
//!      (`corpus_id`, `tool`, `tool_version`, `start_utc`, `end_utc`,
//!      `exit_code`, `har_bytes`).
//!
//! It is `#[ignore]`-gated because:
//!
//!   * It requires `mitmproxy`, `mvn`, and a fully-materialized
//!     corpus checkout, none of which are on the default CI image.
//!   * A real cold-fetch resolution of the Spring Boot starter-web
//!     transitive closure takes ~30 s and downloads ~80 MB of POMs
//!     and jars — not appropriate for the inner test loop.
//!
//! Run manually with:
//!
//! ```text
//! cargo test -p barista-netcap --test baseline_captures_integration -- --ignored
//! ```

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::Command;

/// Walk up from the crate's `CARGO_MANIFEST_DIR` until we find the
/// repository root — i.e. the directory that contains both
/// `scripts/run-baseline-captures.sh` and `test-corpus/`. This is the
/// same approach `barista-bench` uses to locate fixture trees.
fn repo_root() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut cur: &Path = &manifest;
    loop {
        if cur.join("scripts/run-baseline-captures.sh").exists()
            && cur.join("test-corpus").is_dir()
        {
            return cur.to_path_buf();
        }
        cur = cur
            .parent()
            .expect("walked past filesystem root without finding repo");
    }
}

#[test]
#[ignore = "requires mitmproxy + mvn + corpus materialized; run with `cargo test -- --ignored`"]
fn driver_script_produces_har_and_metadata_for_one_cell() {
    let root = repo_root();
    let tmp = tempfile::tempdir().expect("tempdir for capture output root");
    let output_root = tmp.path();

    let status = Command::new("bash")
        .arg(root.join("scripts/run-baseline-captures.sh"))
        .arg("--projects")
        .arg("spring-boot-starter-web-app")
        .arg("--tools")
        .arg("mvn")
        .arg("--output-root")
        .arg(output_root)
        .arg("--timeout-seconds")
        .arg("300")
        .current_dir(&root)
        .status()
        .expect("spawn driver script");
    assert!(status.success(), "driver script exited {status}");

    // Discover the single timestamped output directory the driver
    // produced.
    let project_dir = output_root.join("spring-boot-starter-web-app");
    let tool_dir = project_dir
        .read_dir()
        .expect("project dir readable")
        .next()
        .and_then(Result::ok)
        .expect("at least one tool subdir written")
        .path();
    let ts_dir = tool_dir
        .read_dir()
        .expect("tool dir readable")
        .next()
        .and_then(Result::ok)
        .expect("at least one timestamped capture written")
        .path();

    let har = ts_dir.join("capture.har");
    let meta = ts_dir.join("metadata.toml");

    let har_bytes = std::fs::metadata(&har)
        .expect("capture.har exists")
        .len();
    assert!(har_bytes > 0, "capture.har is empty — proxy didn't capture");

    // Parse the HAR via the crate's own validator so we exercise the
    // same code path that `CaptureSession::stop` runs in production.
    let summary = barista_netcap::validate_har(&har).expect("HAR parses");
    assert!(
        summary.entry_count > 0,
        "HAR has zero entries — proxy started but nothing went through it"
    );

    let meta_text = std::fs::read_to_string(&meta).expect("metadata.toml readable");
    for required_key in [
        "corpus_id",
        "tool",
        "tool_version",
        "start_utc",
        "end_utc",
        "exit_code",
        "har_bytes",
    ] {
        assert!(
            meta_text.contains(required_key),
            "metadata.toml missing key `{required_key}`:\n{meta_text}"
        );
    }
}
