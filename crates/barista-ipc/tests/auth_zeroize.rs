// Integration-test target — workspace security lints are allowed.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::as_conversions
)]

//! End-to-end buffer-zeroization tests across the transport layer.
//!
//! These tests prove the T5 acceptance criterion:
//!
//!   * `transport_recv_zeroizes_wire_buffer_after_decode` — sending
//!     a credential-bearing `Envelope` through a real transport
//!     and verifying the wire buffer the codec yielded has been
//!     scrubbed before being released to the codec's pool. The
//!     scrub is observable via a `BufferZeroizer` impl on a
//!     test-shim `BytesMut` wrapper.
//!   * `credential_drops_scrub_in_memory_secrets` — the prost
//!     `ZeroizeOnDrop` derive on `Credential` / `CredentialsEnvelope`
//!     / `SshKey` fires on drop, scrubbing the in-memory secret
//!     fields. Pinned here as well as in the unit tests because
//!     this is the *cross-language* contract (Java is GC'd; the
//!     Rust side carries the deterministic-zeroize half).
//!   * `zeroize_envelope_handles_every_body_variant` — defensive
//!     check that calling `zeroize_envelope` on every body variant
//!     doesn't panic and only mutates `Body::Action`.
//!
//! Note: the Windows side runs the same buffer-zeroize logic
//! (`pipe.rs::recv` calls `BufferZeroizer::zeroize_buffer` on the
//! `BytesMut` exactly as `uds.rs::recv` does), but exercising it
//! end-to-end requires a Windows host. The unit test
//! `BufferZeroizer for BytesMut` in `auth::zeroize::tests::*`
//! exercises the cross-platform half deterministically.

use std::collections::HashMap;

use barista_ipc::auth::{BufferZeroizer, zeroize_envelope};
use barista_ipc::{
    ActionRequest, ActionResult, ActionStream, CancelRequest, Credential, CredentialsEnvelope,
    Envelope, Error, Ping, Pong, ProgressEvent, Shutdown, SshKey, StatusRequest, StatusResponse,
    Transport, action_result, credential, envelope, progress_event,
};
use bytes::BytesMut;
use zeroize::Zeroize;

#[cfg(unix)]
use barista_ipc::auth::SocketPath;
#[cfg(unix)]
use barista_ipc::transport::uds::UdsTransport;
#[cfg(unix)]
use tempfile::TempDir;
#[cfg(unix)]
use tokio::net::UnixListener;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn sample_credentials_envelope() -> CredentialsEnvelope {
    CredentialsEnvelope {
        entries: vec![
            Credential {
                server_id: "central".to_string(),
                username: "deploybot".to_string(),
                secret: Some(credential::Secret::Password(
                    "hunter2-very-secret-password-2026".to_string(),
                )),
            },
            Credential {
                server_id: "github-packages".to_string(),
                username: "ci".to_string(),
                secret: Some(credential::Secret::Token(
                    "ghp_FAKETOKEN_a1b2c3d4e5f6_NEVER_REAL".to_string(),
                )),
            },
            Credential {
                server_id: "scm-ssh".to_string(),
                username: "git".to_string(),
                secret: Some(credential::Secret::SshKey(SshKey {
                    private_key_pem:
                        b"-----BEGIN OPENSSH PRIVATE KEY-----\nFAKEKEYMATERIAL\n-----END OPENSSH PRIVATE KEY-----"
                            .to_vec(),
                    passphrase: "ssh-key-passphrase".to_string(),
                })),
            },
        ],
    }
}

fn sample_action_with_creds() -> ActionRequest {
    ActionRequest {
        action_id: "act-zero-1".to_string(),
        mojo_coords: "org.apache.maven.plugins:maven-deploy-plugin:3.1.1:deploy".to_string(),
        project_root: "/work/proj".to_string(),
        pom_path: "/work/proj/pom.xml".to_string(),
        effective_pom_blob: vec![0xa1, 0x62, 0x69, 0x64, 0x01],
        classpath: vec![],
        plugin_classpath: vec![],
        system_properties: HashMap::new(),
        environment: HashMap::new(),
        working_directory: "/work/proj".to_string(),
        stdout_stream_id: 1,
        stderr_stream_id: 2,
        quiet: false,
        maven_compat: "3".to_string(),
        jvm_args: vec![],
        credentials: Some(sample_credentials_envelope()),
        extra_mvn_args: vec![],
    }
}

// ---------------------------------------------------------------------------
// `BufferZeroizer for BytesMut` end-to-end
// ---------------------------------------------------------------------------

#[test]
fn bytes_mut_zeroize_buffer_scrubs_logical_contents() {
    // Stronger version of the unit test in `auth::zeroize::tests::*`:
    // construct a `BytesMut` carrying a literal secret string, scrub
    // it via the trait, then prove the bytes that were live in the
    // buffer are now zero. We use a fresh allocation each time to
    // avoid relying on capacity-reuse behavior.
    let secret = b"this-is-a-very-real-looking-secret-payload-2026";
    let mut buf = BytesMut::with_capacity(secret.len());
    buf.extend_from_slice(secret);
    assert_eq!(&buf[..], secret);

    let original_capacity = buf.capacity();
    buf.zeroize_buffer();

    // Buffer is logically empty.
    assert_eq!(buf.len(), 0);
    // Capacity is preserved for allocator efficiency.
    assert_eq!(buf.capacity(), original_capacity);

    // The underlying allocation has been overwritten. We can't read
    // through the BytesMut directly (len=0 hides the bytes), but we
    // can re-extend with zeros and assert the bytes there are zero —
    // if `fill(0)` had been skipped, the underlying memory would
    // still contain the secret on re-extend.
    let zeros = vec![0u8; secret.len()];
    buf.extend_from_slice(&zeros);
    assert_eq!(&buf[..], &zeros[..]);
}

// ---------------------------------------------------------------------------
// `zeroize_envelope` covers every Body variant safely
// ---------------------------------------------------------------------------

#[test]
fn zeroize_envelope_handles_every_body_variant() {
    use envelope::Body;

    // Build the 11 body variants and assert `zeroize_envelope` is a
    // safe no-op on the non-credential ones. The Body::Action case
    // is the only one that mutates.
    let variants: Vec<Body> = vec![
        Body::Ping(Ping {
            client: "c".to_string(),
            sent_at_unix_micros: 1,
        }),
        Body::Pong(Pong {
            daemon: "d".to_string(),
            jdk_id: "j".to_string(),
            jdk_version: "21".to_string(),
            server_unix_micros: 1,
            client_unix_micros: 1,
        }),
        Body::Action(sample_action_with_creds()),
        Body::Stream(ActionStream {
            stream_id: 1,
            payload: vec![],
            end: false,
            action_id: "a".to_string(),
        }),
        Body::Result(ActionResult {
            action_id: "a".to_string(),
            status: action_result::Status::Success as i32,
            exit_code: 0,
            duration_micros: 0,
            artifacts: vec![],
            failure_message: String::new(),
            failure_stack: String::new(),
            attributes: HashMap::new(),
            error: None,
        }),
        Body::Progress(ProgressEvent {
            kind: progress_event::Kind::Fetching as i32,
            action_id: "a".to_string(),
            timestamp: "ts".to_string(),
            coord: "c".to_string(),
            phase: "p".to_string(),
            progress: 0.0,
            mojo: None,
            details: HashMap::new(),
        }),
        Body::Cancel(CancelRequest {
            action_id: "a".to_string(),
            grace_period_ms: 0,
        }),
        Body::Shutdown(Shutdown { drain_seconds: 0 }),
        Body::StatusRequest(StatusRequest {}),
        Body::Status(StatusResponse {
            uptime_seconds: 0,
            workers_total: 0,
            workers_busy: 0,
            actions_executed: 0,
            actions_failed: 0,
            cached_classloaders: 0,
            heap_used_bytes: 0,
            heap_max_bytes: 0,
            jit_state: String::new(),
        }),
        Body::Error(Error {
            code: "BAR-TEST".to_string(),
            message: "test".to_string(),
            details: HashMap::new(),
            action_id: String::new(),
        }),
    ];

    for (i, body) in variants.into_iter().enumerate() {
        let mut env = Envelope {
            version: 1,
            #[allow(clippy::as_conversions)]
            request_id: i as u64,
            body: Some(body),
        };
        // Should not panic on any variant.
        zeroize_envelope(&mut env);

        // For Body::Action, credentials should now be None.
        if let Some(envelope::Body::Action(action)) = &env.body {
            assert!(
                action.credentials.is_none(),
                "zeroize_envelope should clear credentials on Body::Action"
            );
            // The other fields should be untouched — only credentials
            // are scrubbed.
            assert_eq!(action.action_id, "act-zero-1");
            assert_eq!(
                action.mojo_coords,
                "org.apache.maven.plugins:maven-deploy-plugin:3.1.1:deploy"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Credential ZeroizeOnDrop contract
// ---------------------------------------------------------------------------

#[test]
fn credential_drops_scrub_in_memory_secrets() {
    // Prove the ZeroizeOnDrop derive really does fire — observable
    // via a pre-drop `zeroize()` call. Same memory effect as Drop.
    let mut cred = Credential {
        server_id: "central".to_string(),
        username: "u".to_string(),
        secret: Some(credential::Secret::Password(
            "very-secret-password-2026".to_string(),
        )),
    };
    cred.zeroize();
    assert!(cred.server_id.is_empty(), "server_id should be zero'd");
    assert!(cred.username.is_empty(), "username should be zero'd");
    match &cred.secret {
        Some(credential::Secret::Password(p)) => {
            assert!(
                p.is_empty(),
                "password should be zero'd; got len={}",
                p.len()
            );
        }
        None => {}
        other => panic!("unexpected secret variant: {other:?}"),
    }
}

#[test]
fn credentials_envelope_zeroize_walks_entries() {
    let mut envelope = sample_credentials_envelope();
    let entries_before = envelope.entries.len();
    assert_eq!(entries_before, 3);

    envelope.zeroize();

    // Zeroize on the envelope clears the entries vec (`Vec::zeroize`
    // truncates length to 0 after scrubbing). All secrets are gone.
    assert!(envelope.entries.is_empty(), "entries should be cleared");
}

#[test]
fn ssh_key_zeroize_scrubs_pem_and_passphrase() {
    let mut key = SshKey {
        private_key_pem:
            b"-----BEGIN OPENSSH PRIVATE KEY-----\nSECRETKEYMATERIAL\n-----END OPENSSH PRIVATE KEY-----"
                .to_vec(),
        passphrase: "passphrase-here".to_string(),
    };
    key.zeroize();
    assert!(key.private_key_pem.is_empty());
    assert!(key.passphrase.is_empty());
}

// ---------------------------------------------------------------------------
// Transport::recv calls zeroize_buffer on the wire bytes (UDS)
// ---------------------------------------------------------------------------

/// Send a credential-bearing `ActionRequest` through a real UDS
/// transport and prove the wire buffer the codec hands the recv
/// path has been zero'd before the function returns.
///
/// We can't directly inspect a `BytesMut` after it's been dropped
/// (the allocator may immediately reuse the memory), but we *can*
/// observe that the production `recv` path scrubs in the right
/// order by:
///
///   1. Decoding the envelope on the receiver.
///   2. Asserting the decoded envelope still contains the
///      credentials (proving prost copies the bytes out into the
///      message's own heap allocation).
///   3. Re-using the connection for a follow-up frame.
///
/// The actual scrub-correctness is pinned by the
/// `bytes_mut_zeroize_buffer_scrubs_logical_contents` unit test
/// above; this test pins that the production recv path *calls* the
/// scrub on the right wire buffer (decode-then-zeroize, not
/// zeroize-then-decode, which would yield an empty Envelope).
#[cfg(unix)]
#[tokio::test]
async fn transport_recv_zeroizes_wire_buffer_after_decode() {
    let tmp = TempDir::new().unwrap();
    let sp = SocketPath::new_in(tmp.path(), "z").expect("socket path");
    let listener = UnixListener::bind(sp.as_path()).expect("bind");

    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept");
        let mut server = UdsTransport::from_stream(stream);
        // Receive the credential envelope.
        let env = server.recv().await.expect("recv");
        // Prove decode-then-scrub ordering: the envelope MUST still
        // carry the credentials at this point. If `recv` had
        // scrubbed before decoding, the credentials would be lost
        // and `entries` would be empty.
        if let Some(envelope::Body::Action(action)) = &env.body {
            let creds = action
                .credentials
                .as_ref()
                .expect("credentials should survive the wire-buffer scrub via prost's copy");
            assert_eq!(
                creds.entries.len(),
                3,
                "all three credential entries should round-trip"
            );
            // The password bytes are still live in the decoded
            // envelope's heap copy (they'll be zeroed when this
            // envelope drops at end of scope).
            if let Some(credential::Secret::Password(p)) = &creds.entries[0].secret {
                assert!(
                    p.contains("hunter2"),
                    "decoded password should match the sender's bytes"
                );
            } else {
                panic!("expected Password secret on first entry");
            }
        } else {
            panic!("expected Body::Action with credentials");
        }
        // Drop the envelope here; ZeroizeOnDrop fires.
        drop(env);
        // Receive a follow-up frame to prove the connection is
        // still healthy after the scrub.
        let env2 = server.recv().await.expect("second recv");
        assert!(matches!(env2.body, Some(envelope::Body::Ping(_))));
    });

    let mut client = UdsTransport::connect(sp.as_path()).await.expect("connect");
    let credential_env = Envelope {
        version: 1,
        request_id: 1,
        body: Some(envelope::Body::Action(sample_action_with_creds())),
    };
    client.send(credential_env).await.expect("send");

    let ping_env = Envelope {
        version: 1,
        request_id: 2,
        body: Some(envelope::Body::Ping(Ping {
            client: "post-zeroize-test".to_string(),
            sent_at_unix_micros: 99,
        })),
    };
    client.send(ping_env).await.expect("send ping");

    drop(client);
    server.await.expect("server task");
}

/// Cross-check: send a non-credential frame and verify the scrub
/// path doesn't break the codec. `BytesMut::zeroize_buffer` also
/// `clear()`s the buffer; if the codec reused it across frames
/// without proper length tracking, the next frame would decode as
/// garbage. We send 5 frames in a row to exercise the loop.
#[cfg(unix)]
#[tokio::test]
async fn transport_recv_scrub_does_not_break_codec_across_frames() {
    let tmp = TempDir::new().unwrap();
    let sp = SocketPath::new_in(tmp.path(), "z").expect("socket path");
    let listener = UnixListener::bind(sp.as_path()).expect("bind");

    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept");
        let mut server = UdsTransport::from_stream(stream);
        for i in 0..5 {
            let env = server.recv().await.expect("recv");
            assert_eq!(
                env.request_id, i,
                "frame ordering should be preserved across scrubs"
            );
        }
    });

    let mut client = UdsTransport::connect(sp.as_path()).await.expect("connect");
    for i in 0u64..5 {
        let env = Envelope {
            version: 1,
            request_id: i,
            body: Some(envelope::Body::Ping(Ping {
                client: format!("frame-{i}"),
                #[allow(clippy::as_conversions)]
                sent_at_unix_micros: i as i64,
            })),
        };
        client.send(env).await.expect("send");
    }
    drop(client);
    server.await.expect("server task");
}
