//! Minimal HAR (HTTP Archive) 1.2 model for the analysis pipeline.
//!
//! The reference schema lives at
//! [`ahmadnassri/har-spec`](https://github.com/ahmadnassri/har-spec)
//! and is itself a JSON-Schema rendition of Jan Odvarko's HAR 1.2
//! draft. We deliberately keep this in-house model **minimal** for
//! v0.1: only the fields the rule-based analyzers actually inspect
//! are surfaced, everything else uses `#[serde(default)]` and is
//! permitted-but-ignored.
//!
//! ## What's modelled
//!
//! - `log.entries[]` — the per-request records.
//! - `request.method`, `request.url`, `request.headers`,
//!   `request.bodySize`.
//! - `response.status`, `response.statusText`, `response.headers`,
//!   `response.bodySize`, `response.content.size`,
//!   `response.content.mimeType`.
//! - `time` (total ms) + `timings.{dns,connect,send,wait,receive,ssl}`.
//! - `serverIPAddress`, `connection` (where mitmproxy emits them — both
//!   are HAR 1.2 *optional* fields).
//!
//! ## What's deliberately skipped
//!
//! - `response.content.text` / `response.content.encoding` — finding
//!   detection looks at headers + sizes, never response bodies.
//! - `request.postData`, `request.queryString`, `request.cookies`,
//!   `response.cookies`, `response.redirectURL` — none of the v0.1
//!   analyzers consume them.
//! - `log.creator`, `log.browser`, `log.pages`, `pageref` — capture
//!   provenance is recorded in the surrounding session metadata
//!   (B.1 T3), not in the HAR itself.
//! - `cache` (per-entry cache info) — repo managers don't populate it.
//!
//! When a future analyzer needs one of the skipped fields, add it to
//! the struct here rather than reparsing the raw JSON — keeping a
//! single shared model avoids drift between analyzers.

use serde::Deserialize;

/// Top-level HAR document. HAR 1.2 wraps everything in a `log`
/// object — we follow that shape exactly so a captured `.har` parses
/// directly into [`Har`] without an intermediate envelope.
#[derive(Debug, Clone, Deserialize)]
pub struct Har {
    /// The HAR log envelope.
    pub log: HarLog,
}

/// The `log` envelope. `version` is informational; we accept any HAR
/// 1.2-compatible value (mitmproxy emits `"1.2"`).
#[derive(Debug, Clone, Deserialize)]
pub struct HarLog {
    /// HAR specification version. Informational only.
    #[serde(default)]
    pub version: String,
    /// Per-request entries. Order matches capture order — analyzers
    /// rely on this for windowed checks.
    #[serde(default)]
    pub entries: Vec<HarEntry>,
}

/// One captured request/response pair.
#[derive(Debug, Clone, Deserialize)]
pub struct HarEntry {
    /// ISO-8601 start timestamp. Informational; analyzers use
    /// `timings` / `time` for duration math instead.
    #[serde(default, rename = "startedDateTime")]
    pub started_date_time: String,
    /// Total elapsed time in milliseconds (DNS + connect + send +
    /// wait + receive + SSL). HAR encodes this as a number.
    #[serde(default)]
    pub time: f64,
    /// Captured request.
    pub request: HarRequest,
    /// Captured response.
    pub response: HarResponse,
    /// Per-phase timings. Negative values mean "not applicable" per
    /// HAR 1.2 — `connect = -1` is the canonical signal that a
    /// connection was reused.
    #[serde(default)]
    pub timings: HarTimings,
    /// IP address of the server, if mitmproxy recorded it.
    #[serde(default, rename = "serverIPAddress")]
    pub server_ip_address: Option<String>,
    /// Connection-identifier the server-side stack assigned to this
    /// flow. mitmproxy emits a `host:port` string here; analyzers use
    /// it to bucket entries by unique TCP/TLS connection.
    #[serde(default)]
    pub connection: Option<String>,
}

/// Subset of the HAR `request` object the analyzers consume.
#[derive(Debug, Clone, Deserialize)]
pub struct HarRequest {
    /// HTTP method (`GET`, `HEAD`, `POST`, ...). Uppercased by
    /// convention; we tolerate any case.
    #[serde(default)]
    pub method: String,
    /// Full request URL including scheme, host, path, and query.
    #[serde(default)]
    pub url: String,
    /// Request headers. HAR encodes them as an array of `{name,
    /// value}` records; duplicates may appear (e.g. multiple
    /// `Cookie` headers) — we preserve that.
    #[serde(default)]
    pub headers: Vec<HarHeader>,
    /// Size of the request body in bytes, or `-1` when unknown.
    #[serde(default = "minus_one_i64", rename = "bodySize")]
    pub body_size: i64,
}

/// Subset of the HAR `response` object the analyzers consume.
#[derive(Debug, Clone, Deserialize)]
pub struct HarResponse {
    /// HTTP status code.
    #[serde(default)]
    pub status: u16,
    /// HTTP status text. Informational.
    #[serde(default, rename = "statusText")]
    pub status_text: String,
    /// Response headers.
    #[serde(default)]
    pub headers: Vec<HarHeader>,
    /// Response content metadata. We never read `.text` — see module
    /// docs for the deliberate-skip rationale.
    #[serde(default)]
    pub content: HarContent,
    /// Compressed (on-the-wire) body size in bytes, or `-1` when
    /// unknown. Compare with `content.size` to detect "wire bytes <
    /// payload bytes" — the signal compression was applied.
    #[serde(default = "minus_one_i64", rename = "bodySize")]
    pub body_size: i64,
}

/// `response.content` payload metadata.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct HarContent {
    /// Decompressed payload size in bytes.
    #[serde(default)]
    pub size: i64,
    /// MIME type (`application/json`, `application/xml`, `text/html`,
    /// ...). Used by the uncompressed-transfer analyzer to decide
    /// which responses are worth compressing.
    #[serde(default, rename = "mimeType")]
    pub mime_type: String,
}

/// One HAR header record.
#[derive(Debug, Clone, Deserialize)]
pub struct HarHeader {
    /// Header field name. HAR preserves the casing the server / client
    /// sent; analyzers compare case-insensitively.
    pub name: String,
    /// Header value.
    pub value: String,
}

/// Per-phase timing breakdown for one entry. All values are
/// milliseconds; `-1` means "not applicable" (canonical: `connect =
/// -1` ↔ connection reused).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct HarTimings {
    /// DNS lookup duration.
    #[serde(default = "minus_one_f64")]
    pub dns: f64,
    /// TCP connect duration. `-1` ↔ reused connection.
    #[serde(default = "minus_one_f64")]
    pub connect: f64,
    /// TLS handshake duration (subset of `connect` in HAR 1.2;
    /// mitmproxy emits it separately as an extension).
    #[serde(default = "minus_one_f64")]
    pub ssl: f64,
    /// Time spent sending the request bytes.
    #[serde(default = "minus_one_f64")]
    pub send: f64,
    /// Time waiting for the first byte of the response (TTFB).
    #[serde(default = "minus_one_f64")]
    pub wait: f64,
    /// Time spent receiving the response body.
    #[serde(default = "minus_one_f64")]
    pub receive: f64,
}

fn minus_one_i64() -> i64 {
    -1
}

fn minus_one_f64() -> f64 {
    -1.0
}

impl HarEntry {
    /// Returns the header value matching `name` (case-insensitive) from
    /// the request, or `None` when the header is absent.
    #[must_use]
    pub fn request_header(&self, name: &str) -> Option<&str> {
        find_header(&self.request.headers, name)
    }

    /// Returns the header value matching `name` (case-insensitive) from
    /// the response, or `None` when the header is absent.
    #[must_use]
    pub fn response_header(&self, name: &str) -> Option<&str> {
        find_header(&self.response.headers, name)
    }

    /// Extracts the host portion of `request.url`, lower-cased. Returns
    /// `None` when the URL has no parseable authority component (e.g.
    /// a relative URL slipped into the capture, which shouldn't happen
    /// but is tolerated).
    #[must_use]
    pub fn host(&self) -> Option<String> {
        parse_host(&self.request.url)
    }

    /// Returns the path-and-query component of `request.url`, lower-
    /// cased. Used by the duplicate-request analyzer's URL key.
    #[must_use]
    pub fn path_and_query(&self) -> Option<String> {
        parse_path(&self.request.url)
    }

    /// True iff this entry's HAR `connect` timing indicates a fresh
    /// TCP/TLS handshake (any non-negative value). A negative
    /// `connect` means the connection was reused — the strongest
    /// signal HAR 1.2 gives us.
    #[must_use]
    pub fn opened_new_connection(&self) -> bool {
        self.timings.connect >= 0.0
    }
}

fn find_header<'a>(headers: &'a [HarHeader], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|h| h.name.eq_ignore_ascii_case(name))
        .map(|h| h.value.as_str())
}

/// Lightweight URL parser scoped to what the analyzers need: scheme,
/// host, port, path, query. We avoid pulling `url` as a dep for one
/// helper — the input is always `scheme://host[:port]/path[?query]`
/// (HAR records absolute URLs by spec).
fn parse_host(url: &str) -> Option<String> {
    let after_scheme = url.split_once("://")?.1;
    let authority_end = after_scheme.find('/').unwrap_or(after_scheme.len());
    let authority = &after_scheme[..authority_end];
    // Strip optional `user:pass@` prefix.
    let host_port = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    // Strip optional `:port` suffix; preserve IPv6 brackets.
    let host = if host_port.starts_with('[') {
        host_port
    } else {
        host_port.rsplit_once(':').map_or(host_port, |(h, _)| h)
    };
    if host.is_empty() {
        None
    } else {
        Some(host.to_ascii_lowercase())
    }
}

fn parse_path(url: &str) -> Option<String> {
    let after_scheme = url.split_once("://")?.1;
    let slash = after_scheme.find('/')?;
    Some(after_scheme[slash..].to_ascii_lowercase())
}

/// Parse a HAR file from raw bytes.
///
/// On failure, returns an [`AnalyzeError::HarInvalid`] with a
/// human-readable reason. Callers that want path-bearing diagnostics
/// should use [`load_har`] instead.
///
/// [`AnalyzeError::HarInvalid`]: crate::error::AnalyzeError::HarInvalid
/// [`load_har`]: crate::load_har
pub fn parse_har_bytes(bytes: &[u8]) -> Result<Har, String> {
    if bytes.is_empty() {
        return Err("file is empty".to_string());
    }
    serde_json::from_slice::<Har>(bytes).map_err(|e| format!("not a valid HAR 1.2 document: {e}"))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    fn sample_har() -> &'static str {
        r#"{
          "log": {
            "version": "1.2",
            "entries": [
              {
                "startedDateTime": "2026-05-15T12:00:00Z",
                "time": 42.5,
                "request": {
                  "method": "GET",
                  "url": "https://repo.maven.apache.org/maven2/org/example/lib/1.0/lib-1.0.pom",
                  "headers": [
                    {"name": "Accept-Encoding", "value": "gzip, br"},
                    {"name": "User-Agent", "value": "Apache-Maven/3.9.9"}
                  ],
                  "bodySize": 0
                },
                "response": {
                  "status": 200,
                  "statusText": "OK",
                  "headers": [
                    {"name": "Content-Encoding", "value": "gzip"},
                    {"name": "Content-Type", "value": "application/xml"}
                  ],
                  "content": {"size": 12345, "mimeType": "application/xml"},
                  "bodySize": 4096
                },
                "timings": {"dns": 5.0, "connect": 12.0, "ssl": 8.0, "send": 0.5, "wait": 18.0, "receive": 7.0},
                "serverIPAddress": "151.101.0.215",
                "connection": "repo.maven.apache.org:443"
              }
            ]
          }
        }"#
    }

    #[test]
    fn parses_sample_har() {
        let har = parse_har_bytes(sample_har().as_bytes()).expect("parse");
        assert_eq!(har.log.entries.len(), 1);
        let entry = &har.log.entries[0];
        assert_eq!(entry.request.method, "GET");
        assert_eq!(entry.response.status, 200);
        assert_eq!(entry.response.content.size, 12345);
        assert_eq!(entry.response.body_size, 4096);
        assert_eq!(entry.timings.connect, 12.0);
    }

    #[test]
    fn header_lookup_is_case_insensitive() {
        let har = parse_har_bytes(sample_har().as_bytes()).expect("parse");
        let entry = &har.log.entries[0];
        assert_eq!(entry.response_header("content-encoding"), Some("gzip"));
        assert_eq!(entry.request_header("ACCEPT-ENCODING"), Some("gzip, br"));
        assert_eq!(entry.response_header("missing"), None);
    }

    #[test]
    fn host_and_path_extraction() {
        let har = parse_har_bytes(sample_har().as_bytes()).expect("parse");
        let entry = &har.log.entries[0];
        assert_eq!(entry.host().as_deref(), Some("repo.maven.apache.org"));
        assert_eq!(
            entry.path_and_query().as_deref(),
            Some("/maven2/org/example/lib/1.0/lib-1.0.pom")
        );
    }

    #[test]
    fn opened_new_connection_reads_timings_connect() {
        let entry = HarEntry {
            started_date_time: String::new(),
            time: 0.0,
            request: HarRequest {
                method: String::new(),
                url: String::new(),
                headers: vec![],
                body_size: -1,
            },
            response: HarResponse {
                status: 0,
                status_text: String::new(),
                headers: vec![],
                content: HarContent::default(),
                body_size: -1,
            },
            timings: HarTimings {
                connect: -1.0,
                ..HarTimings::default()
            },
            server_ip_address: None,
            connection: None,
        };
        assert!(!entry.opened_new_connection());

        let entry_fresh = HarEntry {
            timings: HarTimings {
                connect: 14.0,
                ..HarTimings::default()
            },
            ..entry
        };
        assert!(entry_fresh.opened_new_connection());
    }

    #[test]
    fn rejects_non_json() {
        let err = parse_har_bytes(b"not json").expect_err("should fail");
        assert!(err.contains("not a valid HAR"));
    }

    #[test]
    fn rejects_missing_log_envelope() {
        let err = parse_har_bytes(br#"{"entries": []}"#).expect_err("should fail");
        assert!(err.contains("not a valid HAR"));
    }
}
