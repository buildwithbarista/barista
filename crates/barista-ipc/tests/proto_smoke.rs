// Integration-test target — workspace security lints are allowed here.
// Panic-on-misuse (`unwrap()`/`expect()`/`panic!`) is the documented
// contract for failing a test loudly. `as_conversions` is allowed because
// prost-generated enums use `as i32` for the on-wire tag value, which is
// the canonical conversion pattern in `prost::Message` consumers.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::as_conversions
)]

//! Smoke tests for the generated worker-protocol bindings.
//!
//! Scope:
//!
//! 1. Every top-level message (15 of them) round-trips through
//!    `prost::Message::encode_to_vec()` / `Message::decode(&bytes)` with
//!    a non-trivial payload, asserting equality between original and
//!    decoded values.
//! 2. Every `Envelope.body` `oneof` variant (11 of them) round-trips
//!    inside an `Envelope` so the discriminator wire encoding is
//!    exercised end-to-end.
//! 3. The redacted `Debug` impls on credential-bearing types do not
//!    print decrypted secret material.
//!
//! These are byte-shape tests against the Rust binding only. The full
//! Rust↔Java parity check that closes the milestone's primary AC lives
//! in a later task of the same milestone.

use std::collections::HashMap;

use barista_ipc::{
    ActionRequest, ActionResult, ActionStream, CancelRequest, Credential, CredentialsEnvelope,
    Envelope, Error, Mojo, Ping, Pong, ProducedArtifact, ProgressEvent, Shutdown, SshKey,
    StatusRequest, StatusResponse, action_result, credential, envelope, progress_event,
};
use prost::Message;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Encode `msg`, decode the bytes back into `T`, assert equality, and
/// return the encoded buffer for further inspection.
fn round_trip<T>(msg: &T) -> Vec<u8>
where
    T: Message + Default + PartialEq + std::fmt::Debug,
{
    let bytes = msg.encode_to_vec();
    let decoded = T::decode(bytes.as_slice()).expect("decode should succeed");
    assert_eq!(*msg, decoded, "round-trip mismatch");
    bytes
}

fn sample_mojo() -> Mojo {
    Mojo {
        group_id: "org.apache.maven.plugins".to_string(),
        artifact_id: "maven-compiler-plugin".to_string(),
        version: "3.11.0".to_string(),
        goal: "compile".to_string(),
        execution_id: "default-compile".to_string(),
    }
}

fn sample_action_request() -> ActionRequest {
    let mut system_properties = HashMap::new();
    system_properties.insert("maven.compiler.source".to_string(), "21".to_string());
    let mut environment = HashMap::new();
    environment.insert("LANG".to_string(), "en_US.UTF-8".to_string());

    ActionRequest {
        action_id: "act-1234".to_string(),
        mojo_coords: "org.apache.maven.plugins:maven-compiler-plugin:3.11.0:compile".to_string(),
        project_root: "/work/proj".to_string(),
        pom_path: "/work/proj/pom.xml".to_string(),
        effective_pom_blob: vec![0xa1, 0x62, 0x69, 0x64, 0x01],
        classpath: vec!["/cas/a.jar".to_string(), "/cas/b.jar".to_string()],
        plugin_classpath: vec!["/cas/plugin.jar".to_string()],
        system_properties,
        environment,
        working_directory: "/work/proj".to_string(),
        stdout_stream_id: 1,
        stderr_stream_id: 2,
        quiet: false,
        maven_compat: "3".to_string(),
        jvm_args: vec!["-Xmx512m".to_string()],
        credentials: None,
    }
}

// Distinct test passwords for the redaction tests. Naming them as
// constants keeps the leak-check `contains` assertions readable.
const SECRET_PASSWORD: &str = "hunter2-very-secret-pa55word";
const SECRET_TOKEN: &str = "ghp_TOPSECRET_DEPLOY_TOKEN_xyz";
const SECRET_KEY_PEM: &[u8] = b"-----BEGIN OPENSSH PRIVATE KEY-----TOPSECRET-----END-----";
const SECRET_PASSPHRASE: &str = "ssh-key-passphrase-redact-me";

// ---------------------------------------------------------------------------
// 1. Per-message round trips (15 top-level types from the proto schema).
// ---------------------------------------------------------------------------

#[test]
fn round_trip_envelope_empty() {
    // Envelope with no body — proves the bare envelope encodes/decodes.
    let env = Envelope {
        version: 1,
        request_id: 42,
        body: None,
    };
    round_trip(&env);
}

#[test]
fn round_trip_ping() {
    round_trip(&Ping {
        client: "barista 0.1.0".to_string(),
        sent_at_unix_micros: 1_700_000_000_000_000,
    });
}

#[test]
fn round_trip_pong() {
    round_trip(&Pong {
        daemon: "barback 0.1.0".to_string(),
        jdk_id: "temurin-21".to_string(),
        jdk_version: "21.0.4".to_string(),
        server_unix_micros: 1_700_000_000_000_100,
        client_unix_micros: 1_700_000_000_000_000,
    });
}

#[test]
fn round_trip_action_request() {
    round_trip(&sample_action_request());
}

#[test]
fn round_trip_mojo() {
    round_trip(&sample_mojo());
}

#[test]
fn round_trip_credentials_envelope_password() {
    let env = CredentialsEnvelope {
        entries: vec![Credential {
            server_id: "central".to_string(),
            username: "deploy-bot".to_string(),
            secret: Some(credential::Secret::Password(SECRET_PASSWORD.to_string())),
        }],
    };
    round_trip(&env);
}

#[test]
fn round_trip_credentials_envelope_token() {
    let env = CredentialsEnvelope {
        entries: vec![Credential {
            server_id: "github-packages".to_string(),
            username: String::new(),
            secret: Some(credential::Secret::Token(SECRET_TOKEN.to_string())),
        }],
    };
    round_trip(&env);
}

#[test]
fn round_trip_credentials_envelope_ssh_key() {
    let env = CredentialsEnvelope {
        entries: vec![Credential {
            server_id: "scm:git:ssh://git@example.com/x.git".to_string(),
            username: "git".to_string(),
            secret: Some(credential::Secret::SshKey(SshKey {
                private_key_pem: SECRET_KEY_PEM.to_vec(),
                passphrase: SECRET_PASSPHRASE.to_string(),
            })),
        }],
    };
    round_trip(&env);
}

#[test]
fn round_trip_ssh_key_standalone() {
    round_trip(&SshKey {
        private_key_pem: SECRET_KEY_PEM.to_vec(),
        passphrase: SECRET_PASSPHRASE.to_string(),
    });
}

#[test]
fn round_trip_action_stream() {
    round_trip(&ActionStream {
        stream_id: 1,
        payload: b"[INFO] Compiling...\n".to_vec(),
        end: false,
        action_id: "act-1234".to_string(),
    });
}

#[test]
fn round_trip_progress_event() {
    let mut details = HashMap::new();
    details.insert("artifact".to_string(), "junit-5.10.0.jar".to_string());

    round_trip(&ProgressEvent {
        kind: progress_event::Kind::Fetching as i32,
        action_id: "act-1234".to_string(),
        timestamp: "2026-05-14T12:34:56.789Z".to_string(),
        coord: "org.junit.jupiter:junit-jupiter-api:5.10.0".to_string(),
        phase: "fetch".to_string(),
        progress: 42.5,
        mojo: Some(sample_mojo()),
        details,
    });
}

#[test]
fn round_trip_cancel_request() {
    round_trip(&CancelRequest {
        action_id: "act-1234".to_string(),
        grace_period_ms: 5000,
    });
}

#[test]
fn round_trip_action_result() {
    let mut attributes = HashMap::new();
    attributes.insert("tests.run".to_string(), "142".to_string());

    round_trip(&ActionResult {
        action_id: "act-1234".to_string(),
        status: action_result::Status::Success as i32,
        exit_code: 0,
        duration_micros: 1_234_567,
        artifacts: vec![ProducedArtifact {
            path: "/work/proj/target/foo.jar".to_string(),
            size_bytes: 2048,
            sha256: "abc123".to_string(),
        }],
        failure_message: String::new(),
        failure_stack: String::new(),
        attributes,
        error: None,
    });
}

#[test]
fn round_trip_produced_artifact() {
    round_trip(&ProducedArtifact {
        path: "/work/proj/target/foo.jar".to_string(),
        size_bytes: 2048,
        sha256: "abc123".to_string(),
    });
}

#[test]
fn round_trip_shutdown() {
    round_trip(&Shutdown { drain_seconds: 30 });
}

#[test]
fn round_trip_status_request() {
    round_trip(&StatusRequest {});
}

#[test]
fn round_trip_status_response() {
    round_trip(&StatusResponse {
        uptime_seconds: 3600,
        workers_total: 8,
        workers_busy: 3,
        actions_executed: 1024,
        actions_failed: 4,
        cached_classloaders: 12,
        heap_used_bytes: 268_435_456,
        heap_max_bytes: 1_073_741_824,
        jit_state: "warm".to_string(),
    });
}

#[test]
fn round_trip_error() {
    let mut details = HashMap::new();
    details.insert("server_id".to_string(), "central".to_string());

    round_trip(&Error {
        code: "BAR-DEPLOY-AUTH-MISSING".to_string(),
        message: "no credentials configured for server 'central'".to_string(),
        details,
        action_id: "act-1234".to_string(),
    });
}

// ---------------------------------------------------------------------------
// 2. Every Envelope.body variant exercised through Envelope (11 variants).
// ---------------------------------------------------------------------------

/// Construct an `Envelope` for each `Body` variant, round-trip through
/// `prost::Message`, and assert the decoded variant matches the encoded
/// one. This is the single test that proves the `oneof` discriminator wire
/// encoding is correct for every variant.
#[test]
fn round_trip_envelope_for_every_body_variant() {
    let variants: Vec<envelope::Body> = vec![
        envelope::Body::Ping(Ping {
            client: "barista 0.1.0".to_string(),
            sent_at_unix_micros: 1,
        }),
        envelope::Body::Pong(Pong {
            daemon: "barback 0.1.0".to_string(),
            jdk_id: "temurin-21".to_string(),
            jdk_version: "21.0.4".to_string(),
            server_unix_micros: 2,
            client_unix_micros: 1,
        }),
        envelope::Body::Action(sample_action_request()),
        envelope::Body::Stream(ActionStream {
            stream_id: 1,
            payload: b"[INFO]".to_vec(),
            end: true,
            action_id: "act-stream".to_string(),
        }),
        envelope::Body::Result(ActionResult {
            action_id: "act-result".to_string(),
            status: action_result::Status::Success as i32,
            exit_code: 0,
            duration_micros: 1000,
            artifacts: vec![],
            failure_message: String::new(),
            failure_stack: String::new(),
            attributes: HashMap::new(),
            error: None,
        }),
        envelope::Body::Progress(ProgressEvent {
            kind: progress_event::Kind::Started as i32,
            action_id: "act-progress".to_string(),
            timestamp: "2026-05-14T12:34:56.789Z".to_string(),
            coord: String::new(),
            phase: "start".to_string(),
            progress: 0.0,
            mojo: None,
            details: HashMap::new(),
        }),
        envelope::Body::Cancel(CancelRequest {
            action_id: "act-cancel".to_string(),
            grace_period_ms: 5000,
        }),
        envelope::Body::Shutdown(Shutdown { drain_seconds: 5 }),
        envelope::Body::StatusRequest(StatusRequest {}),
        envelope::Body::Status(StatusResponse {
            uptime_seconds: 60,
            workers_total: 4,
            workers_busy: 0,
            actions_executed: 0,
            actions_failed: 0,
            cached_classloaders: 0,
            heap_used_bytes: 0,
            heap_max_bytes: 0,
            jit_state: "cold".to_string(),
        }),
        envelope::Body::Error(Error {
            code: "BAR-PROTO-001".to_string(),
            message: "version mismatch".to_string(),
            details: HashMap::new(),
            action_id: String::new(),
        }),
    ];

    assert_eq!(
        variants.len(),
        11,
        "Envelope.body should have exactly 11 variants (per the proto schema)"
    );

    for (i, body) in variants.into_iter().enumerate() {
        let envelope_in = Envelope {
            version: 1,
            // Encode the variant index in `request_id` so a test failure
            // points at which variant misbehaved.
            request_id: i as u64,
            body: Some(body),
        };
        let bytes = envelope_in.encode_to_vec();
        let decoded = Envelope::decode(bytes.as_slice()).expect("envelope should decode");
        assert_eq!(envelope_in, decoded, "envelope variant {i} mismatch");
    }
}

// ---------------------------------------------------------------------------
// 3. Redacted Debug for credential-bearing types.
// ---------------------------------------------------------------------------

#[test]
fn debug_credential_password_does_not_leak() {
    let cred = Credential {
        server_id: "central".to_string(),
        username: "deploy-bot".to_string(),
        secret: Some(credential::Secret::Password(SECRET_PASSWORD.to_string())),
    };
    let s = format!("{cred:?}");
    assert!(
        !s.contains(SECRET_PASSWORD),
        "Credential Debug must not contain the password: {s}"
    );
    // Server ID is a non-secret diagnostic key — confirm it's still present.
    assert!(
        s.contains("central"),
        "Credential Debug should retain server_id for diagnostics: {s}"
    );
    // Username is also redacted per the schema's logging contract.
    assert!(
        !s.contains("deploy-bot"),
        "Credential Debug must not contain username: {s}"
    );
}

#[test]
fn debug_credential_token_does_not_leak() {
    let cred = Credential {
        server_id: "github-packages".to_string(),
        username: String::new(),
        secret: Some(credential::Secret::Token(SECRET_TOKEN.to_string())),
    };
    let s = format!("{cred:?}");
    assert!(
        !s.contains(SECRET_TOKEN),
        "Credential Debug must not contain the token: {s}"
    );
}

#[test]
fn debug_ssh_key_does_not_leak() {
    let key = SshKey {
        private_key_pem: SECRET_KEY_PEM.to_vec(),
        passphrase: SECRET_PASSPHRASE.to_string(),
    };
    let s = format!("{key:?}");
    let pem_str = std::str::from_utf8(SECRET_KEY_PEM).unwrap();
    assert!(
        !s.contains("TOPSECRET"),
        "SshKey Debug must not contain key material: {s}"
    );
    assert!(
        !s.contains(SECRET_PASSPHRASE),
        "SshKey Debug must not contain passphrase: {s}"
    );
    // Sanity — the PEM string body should not appear either.
    assert!(
        !s.contains(pem_str),
        "SshKey Debug must not contain full PEM: {s}"
    );
}

#[test]
fn debug_credential_ssh_key_does_not_leak() {
    let cred = Credential {
        server_id: "scm-ssh".to_string(),
        username: "git".to_string(),
        secret: Some(credential::Secret::SshKey(SshKey {
            private_key_pem: SECRET_KEY_PEM.to_vec(),
            passphrase: SECRET_PASSPHRASE.to_string(),
        })),
    };
    let s = format!("{cred:?}");
    assert!(!s.contains("TOPSECRET"), "leak via SshKey variant: {s}");
    assert!(
        !s.contains(SECRET_PASSPHRASE),
        "passphrase leaked through Credential Debug: {s}"
    );
}

#[test]
fn debug_credentials_envelope_does_not_leak() {
    let env = CredentialsEnvelope {
        entries: vec![
            Credential {
                server_id: "central".to_string(),
                username: "u1".to_string(),
                secret: Some(credential::Secret::Password(SECRET_PASSWORD.to_string())),
            },
            Credential {
                server_id: "ghcr".to_string(),
                username: String::new(),
                secret: Some(credential::Secret::Token(SECRET_TOKEN.to_string())),
            },
        ],
    };
    let s = format!("{env:?}");
    assert!(!s.contains(SECRET_PASSWORD), "password leaked: {s}");
    assert!(!s.contains(SECRET_TOKEN), "token leaked: {s}");
    // Server IDs are surfaced for diagnostics — they should be present.
    assert!(s.contains("central"), "server_id missing: {s}");
    assert!(s.contains("ghcr"), "server_id missing: {s}");
}

#[test]
fn debug_action_request_with_credentials_does_not_leak() {
    // The most realistic leak vector: an ActionRequest containing a
    // CredentialsEnvelope, formatted via the default-derived Debug on
    // ActionRequest. The default impl calls Debug on each field, so the
    // credentials field MUST flow through our redacted impl.
    let mut req = sample_action_request();
    req.credentials = Some(CredentialsEnvelope {
        entries: vec![Credential {
            server_id: "central".to_string(),
            username: "deploy-bot".to_string(),
            secret: Some(credential::Secret::Password(SECRET_PASSWORD.to_string())),
        }],
    });
    let s = format!("{req:?}");
    assert!(
        !s.contains(SECRET_PASSWORD),
        "password leaked through ActionRequest Debug: {s}"
    );
}

// ---------------------------------------------------------------------------
// 4. Mojo's Eq + Hash derive is usable as a HashMap key.
// ---------------------------------------------------------------------------

#[test]
fn mojo_usable_as_hashmap_key() {
    let mut map: HashMap<Mojo, &'static str> = HashMap::new();
    map.insert(sample_mojo(), "compile");
    assert_eq!(map.get(&sample_mojo()), Some(&"compile"));
}
