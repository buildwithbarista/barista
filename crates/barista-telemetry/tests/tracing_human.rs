// SPDX-License-Identifier: MIT OR Apache-2.0

//! Integration tests for the human-friendly log format produced
//! by [`barista_telemetry::tracing`].
//!
//! The human format is the default the user sees in their
//! terminal. The contract is intentionally looser than the JSON
//! format — we don't pin column layout — but we do assert:
//!
//! * Output goes through the provided writer (stderr in the
//!   default install path), not stdout.
//! * Each event renders on its own line.
//! * The rendered text contains the log level, the target, and
//!   the message — the minimum a human (or a `grep`) would
//!   expect from a tty-friendly layout.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::io;
use std::sync::{Arc, Mutex};

use barista_telemetry::tracing::{LogFormat, build_subscriber_with_writer};
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::MakeWriter;

#[derive(Clone, Default)]
struct SharedBuffer(Arc<Mutex<Vec<u8>>>);

impl SharedBuffer {
    fn contents(&self) -> String {
        let bytes = self.0.lock().unwrap().clone();
        String::from_utf8(bytes).expect("subscriber wrote non-utf8 bytes")
    }
}

impl io::Write for SharedBuffer {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for SharedBuffer {
    type Writer = SharedBuffer;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

#[test]
fn human_format_renders_tty_friendly_layout() {
    let buf = SharedBuffer::default();
    let subscriber =
        build_subscriber_with_writer(LogFormat::Human, EnvFilter::new("trace"), buf.clone());

    tracing::subscriber::with_default(subscriber, || {
        info!(target: "barista_test", "hello from human format");
        warn!(target: "barista_test", "second line");
    });

    let contents = buf.contents();
    assert!(!contents.is_empty(), "human format produced no output");

    // One line per event.
    let lines: Vec<&str> = contents
        .trim_end_matches('\n')
        .split('\n')
        .filter(|l| !l.is_empty())
        .collect();
    assert_eq!(
        lines.len(),
        2,
        "expected two rendered lines, got {}: {contents:?}",
        lines.len()
    );

    // First line carries INFO level + the target + the message.
    let first = lines[0];
    assert!(
        first.contains("INFO"),
        "missing INFO level in human output: {first}"
    );
    assert!(
        first.contains("barista_test"),
        "missing target in human output: {first}"
    );
    assert!(
        first.contains("hello from human format"),
        "missing message in human output: {first}"
    );

    // Second line is WARN.
    let second = lines[1];
    assert!(
        second.contains("WARN"),
        "missing WARN level in human output: {second}"
    );
    assert!(
        second.contains("second line"),
        "missing message in human output: {second}"
    );

    // Human format is *not* one valid JSON object per line — pin
    // that distinction so a regression that flips the formatter
    // surfaces here.
    assert!(
        serde_json::from_str::<serde_json::Value>(first).is_err(),
        "human format unexpectedly parses as JSON: {first}"
    );
}

#[test]
fn human_format_respects_level_filter() {
    let buf = SharedBuffer::default();
    let subscriber =
        build_subscriber_with_writer(LogFormat::Human, EnvFilter::new("warn"), buf.clone());

    tracing::subscriber::with_default(subscriber, || {
        info!(target: "barista_test", "should be filtered out");
        warn!(target: "barista_test", "should appear");
    });

    let contents = buf.contents();
    assert!(
        !contents.contains("should be filtered out"),
        "INFO event leaked past WARN filter: {contents:?}"
    );
    assert!(
        contents.contains("should appear"),
        "WARN event missing under WARN filter: {contents:?}"
    );
}
