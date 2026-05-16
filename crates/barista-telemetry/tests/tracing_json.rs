//! Integration tests for the JSON log format produced by
//! [`barista_telemetry::tracing`].
//!
//! These tests pin the contract that an IDE / AI-agent reader can
//! rely on:
//!
//! * Output is one event per line.
//! * Each line is valid JSON.
//! * Each line carries the structured fields required by
//!   downstream consumers — `timestamp`, `level`, `target`,
//!   `fields`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::io;
use std::sync::{Arc, Mutex};

use barista_telemetry::tracing::{LogFormat, build_subscriber_with_writer};
use tracing::{Level, info, warn};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::MakeWriter;

/// Thread-safe in-memory writer that the subscriber can write
/// into during a test, and that the test body can read back from
/// after the subscriber drops.
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
fn json_format_is_one_event_per_line_and_valid_json() {
    let buf = SharedBuffer::default();
    let subscriber = build_subscriber_with_writer(
        LogFormat::Json,
        EnvFilter::new("trace"),
        buf.clone(),
    );

    tracing::subscriber::with_default(subscriber, || {
        info!(target: "barista_test", first_event = 1, "first event");
        warn!(target: "barista_test", second_event = "yes", "second event");
    });

    let contents = buf.contents();
    // Strip trailing newline so split() doesn't yield an empty
    // tail element.
    let lines: Vec<&str> = contents.trim_end_matches('\n').split('\n').collect();
    assert_eq!(
        lines.len(),
        2,
        "expected exactly two JSON lines, got: {contents:?}"
    );

    for line in &lines {
        let value: serde_json::Value =
            serde_json::from_str(line).unwrap_or_else(|err| {
                panic!("line is not valid JSON: {line:?} ({err})")
            });

        assert!(value.is_object(), "JSON line is not an object: {line}");

        for required in ["timestamp", "level", "target", "fields"] {
            assert!(
                value.get(required).is_some(),
                "missing required field {required:?} in: {line}"
            );
        }

        // Target should be the static target we used for the
        // event, not some accidental override.
        assert_eq!(
            value
                .get("target")
                .and_then(|v| v.as_str())
                .expect("target must be a string"),
            "barista_test"
        );

        // `fields` is itself an object with at minimum the
        // `message` key set from the format string.
        let fields = value
            .get("fields")
            .and_then(|v| v.as_object())
            .expect("`fields` must be a JSON object");
        assert!(
            fields.contains_key("message"),
            "`fields` missing `message` key: {line}"
        );
    }
}

#[test]
fn json_format_includes_structured_field_values() {
    let buf = SharedBuffer::default();
    let subscriber = build_subscriber_with_writer(
        LogFormat::Json,
        EnvFilter::new("trace"),
        buf.clone(),
    );

    tracing::subscriber::with_default(subscriber, || {
        info!(target: "barista_test", count = 7_u64, name = "pour", "structured");
    });

    let contents = buf.contents();
    let line = contents.trim_end_matches('\n');
    let value: serde_json::Value = serde_json::from_str(line).expect("valid JSON");

    let fields = value
        .get("fields")
        .and_then(|v| v.as_object())
        .expect("fields object");

    assert_eq!(
        fields.get("count").and_then(|v| v.as_u64()),
        Some(7),
        "structured u64 field missing or wrong: {fields:?}"
    );
    assert_eq!(
        fields.get("name").and_then(|v| v.as_str()),
        Some("pour"),
        "structured str field missing or wrong: {fields:?}"
    );
}

#[test]
fn json_format_respects_level_filter() {
    let buf = SharedBuffer::default();
    let subscriber = build_subscriber_with_writer(
        LogFormat::Json,
        EnvFilter::new("warn"),
        buf.clone(),
    );

    tracing::subscriber::with_default(subscriber, || {
        info!(target: "barista_test", "below threshold");
        warn!(target: "barista_test", "above threshold");
    });

    let contents = buf.contents();
    let lines: Vec<&str> = contents.trim_end_matches('\n').split('\n').collect();
    assert_eq!(lines.len(), 1, "filter not applied: {contents:?}");

    let value: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(
        value.get("level").and_then(|v| v.as_str()),
        Some(Level::WARN.as_str())
    );
}
