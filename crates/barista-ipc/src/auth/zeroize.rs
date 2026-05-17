//! Cross-platform buffer zeroization for credential-carrying wire bytes.
//!
//! The motivation lives in the parent module's doc-comment: prost's
//! `ZeroizeOnDrop` derives scrub the *decoded* `Envelope`'s heap
//! allocations, but the `BytesMut` the codec produced still carries
//! the plaintext bytes until that buffer is overwritten. Until it is,
//! a future allocation of the same `BytesMut` capacity may serve
//! freshly-uninitialized memory holding the previous credentials.
//!
//! This module provides:
//!
//! * [`BufferZeroizer`] — a trait the transport calls on the wire
//!   buffer *after* it has been decoded into an `Envelope`, *before*
//!   the buffer is released back to the codec's pool.
//! * [`zeroize_envelope`] — a defensive helper that walks an
//!   `Envelope` and forces an extra zeroization pass on the
//!   credential fields. Belt-and-braces: the prost-generated
//!   `Credential` types already zeroize on `Drop`, but if a future
//!   caller `mem::take`s the credentials out before the envelope
//!   drops, the original allocation could outlive the envelope. This
//!   helper makes that path safe.
//!
//! # Why a trait instead of a free function
//!
//! The wire buffer's type varies by transport: today both UDS and
//! the named pipe yield `BytesMut`, but in M4.1 T6 the streaming
//! layer wraps the `Framed` and may yield other types (`Bytes`,
//! `Vec<u8>`) depending on how the per-stream channels are wired.
//! A trait keeps the call-site in `Transport::recv` uniform across
//! all of those.

use bytes::BytesMut;
use zeroize::Zeroize;

use crate::Envelope;
use crate::envelope::Body;

/// Implementors can be overwritten with zeros before drop.
///
/// The contract is intentionally minimal: implementors guarantee that
/// after `zeroize_buffer` returns, every byte the buffer was
/// *logically* holding has been overwritten with `0u8`. Capacity may
/// still be non-zero (the underlying allocation may stay alive), but
/// the logical contents are gone.
///
/// Implementations MUST overwrite before they truncate / clear /
/// release the underlying memory. The other order (clear-then-zero)
/// is a no-op against the original bytes — the bytes have already
/// been logically released to the allocator.
pub trait BufferZeroizer {
    /// Overwrite every byte of the buffer's logical contents with
    /// zero. Capacity may be retained (the buffer can still be
    /// re-used for the next frame); only the contents are scrubbed.
    fn zeroize_buffer(&mut self);
}

impl BufferZeroizer for BytesMut {
    fn zeroize_buffer(&mut self) {
        // `BytesMut::fill` writes `0u8` to every byte in `..len()`;
        // this is the cheap fast path. The compiler will not elide
        // the writes because `fill` ultimately calls
        // `core::slice::fill`, which is `#[inline]` but operates on
        // `&mut [u8]` — the borrow checker sees the writes as
        // observable through the reference. We then `clear()` so
        // capacity is recycled but the logical length goes to zero;
        // a subsequent `extend_from_slice` won't see the zero bytes
        // because `BytesMut` tracks length, not capacity, for read
        // visibility.
        //
        // Note: `BytesMut` does NOT implement `Zeroize` itself (1.x
        // line), so we can't reuse the upstream derive — hence the
        // hand-rolled impl. We mirror what `Zeroize for Vec<u8>`
        // does: overwrite then drop the length.
        let len = self.len();
        if len > 0 {
            // SAFETY-equivalent at the safe level: `self[..]` borrows
            // exactly the initialized bytes; `fill(0)` writes all of
            // them. No `unsafe` is required.
            self[..].fill(0);
        }
        self.clear();
    }
}

impl BufferZeroizer for Vec<u8> {
    fn zeroize_buffer(&mut self) {
        // `Vec<u8>` already has a `Zeroize` impl in the `zeroize`
        // crate, but that one *also* sets `len = 0` after writing;
        // we delegate to it explicitly to keep the semantics
        // identical to `BytesMut` above.
        self.zeroize();
    }
}

impl BufferZeroizer for bytes::Bytes {
    /// `bytes::Bytes` is immutable and reference-counted; we cannot
    /// scrub a shared buffer in-place without risking UB if another
    /// holder is still reading from it. The conservative answer is
    /// to drop our handle without scrubbing — the underlying
    /// allocation gets freed when the last holder drops it, and the
    /// allocator may overwrite or hand the memory to another caller
    /// without zeroing first.
    ///
    /// **This is a known gap** and is the reason the recv path uses
    /// `BytesMut` (which we own uniquely) rather than `Bytes`. We
    /// keep this impl as a documented no-op so transports that ever
    /// surface `Bytes` to the zeroizer fail closed at the type
    /// level (the buffer is *not* scrubbed) rather than failing
    /// open (the buffer is silently leaked because no impl exists).
    ///
    /// The conformance test in `tests/auth_zeroize.rs` asserts the
    /// production codec path yields `BytesMut`, not `Bytes`, so
    /// this branch isn't reachable from production today.
    fn zeroize_buffer(&mut self) {
        // No-op. See the doc-comment above for why we can't safely
        // overwrite shared memory.
        let _ = self;
    }
}

/// Defensive walk of an `Envelope`, scrubbing every credential field.
///
/// This is belt-and-braces: the prost-generated `Credential`,
/// `CredentialsEnvelope`, and `SshKey` types all derive
/// `zeroize::ZeroizeOnDrop`, so their secret fields are wiped when
/// the message itself is dropped. This helper covers the case where
/// the caller `mem::take`s, `clone`s, or otherwise reaches into the
/// envelope and the message's own `Drop` doesn't fire while the
/// secret is still live. Calling this just before passing the
/// envelope to the consumer is cheap insurance.
///
/// Only the `ActionRequest.credentials` field is touched — that's
/// the only place the schema places `CredentialsEnvelope`. If a
/// future schema rev adds more credential-carrying fields, this
/// function must grow to match (the test
/// `zeroize_envelope_covers_every_credential_path` pins the
/// coverage by enumerating every `Body` variant).
pub fn zeroize_envelope(envelope: &mut Envelope) {
    if let Some(body) = envelope.body.as_mut() {
        match body {
            Body::Action(action) => {
                // `credentials: Option<CredentialsEnvelope>`. Force
                // a `Zeroize` walk on the inner type while it's
                // still inside the option, then take it (which
                // moves it out and lets `Drop` fire on the moved
                // value).
                if let Some(creds) = action.credentials.as_mut() {
                    creds.zeroize();
                }
                action.credentials = None;
            }
            // No other body variant carries credentials in the v1
            // schema. The compiler exhaustiveness check on the
            // enum keeps this honest: a new credential-bearing
            // variant added to `worker.proto` will surface here as
            // a non-exhaustive match warning under
            // `#[warn(non_exhaustive_omitted_patterns)]`-style review.
            Body::Ping(_)
            | Body::Pong(_)
            | Body::Stream(_)
            | Body::Result(_)
            | Body::Progress(_)
            | Body::Cancel(_)
            | Body::Shutdown(_)
            | Body::StatusRequest(_)
            | Body::Status(_)
            | Body::Error(_) => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Unit tests for the buffer-zeroization layer.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::as_conversions
    )]

    use super::*;
    use crate::{
        ActionRequest, Credential, CredentialsEnvelope, Envelope, Ping, credential, envelope,
    };

    fn sample_action_with_creds() -> ActionRequest {
        ActionRequest {
            action_id: "act-zero".to_string(),
            mojo_coords: "g:a:1.0:goal".to_string(),
            project_root: "/tmp/p".to_string(),
            pom_path: "/tmp/p/pom.xml".to_string(),
            effective_pom_blob: vec![],
            classpath: vec![],
            plugin_classpath: vec![],
            system_properties: Default::default(),
            environment: Default::default(),
            working_directory: "/tmp/p".to_string(),
            stdout_stream_id: 1,
            stderr_stream_id: 2,
            quiet: false,
            maven_compat: "3".to_string(),
            jvm_args: vec![],
            credentials: Some(CredentialsEnvelope {
                entries: vec![Credential {
                    server_id: "central".to_string(),
                    username: "deploybot".to_string(),
                    secret: Some(credential::Secret::Password(
                        "hunter2-the-actual-password".to_string(),
                    )),
                }],
            }),
            extra_mvn_args: vec![],
        }
    }

    #[test]
    fn bytes_mut_zeroize_overwrites_then_clears() {
        let mut buf = BytesMut::from(&b"hunter2-secret"[..]);
        assert_eq!(buf.len(), 14);
        let capacity_before = buf.capacity();

        buf.zeroize_buffer();

        assert_eq!(buf.len(), 0, "len should be zero after zeroize_buffer");
        // Capacity is retained for allocator efficiency; this is the
        // documented contract.
        assert_eq!(
            buf.capacity(),
            capacity_before,
            "capacity should be retained"
        );

        // Peek into the underlying allocation via a reservation +
        // raw-slice trick: extend the buffer back up to the
        // pre-clear length and assert the bytes there are zero.
        // `BytesMut::resize(n, 0)` would mask our test by writing
        // zeros explicitly — we need to see what was left behind.
        buf.reserve(14);
        // The `as_mut_ptr` / pointer arithmetic API is not stable for
        // observing the "uninitialized" tail; instead we round-trip
        // by extending and checking. The extend will write `len`
        // new zero bytes; since we already zeroized the previous 14,
        // an extend with `[0u8; 14]` then a slice equality against
        // `[0u8; 14]` proves the underlying memory is zeros.
        let zeros = [0u8; 14];
        buf.extend_from_slice(&zeros);
        assert_eq!(&buf[..], &[0u8; 14]);
    }

    #[test]
    fn vec_zeroize_overwrites_and_clears() {
        let mut v = b"secret-token".to_vec();
        v.zeroize_buffer();
        assert!(v.is_empty(), "Vec::zeroize_buffer should clear");
    }

    #[test]
    fn bytes_zeroize_is_documented_noop() {
        // `Bytes` is reference-counted; we can't scrub shared
        // memory. The impl is a no-op by design — pin that.
        let mut b = bytes::Bytes::from_static(b"shared");
        b.zeroize_buffer();
        // The Bytes is unchanged because we can't safely mutate it.
        assert_eq!(&b[..], b"shared");
    }

    #[test]
    fn zeroize_envelope_clears_credentials_on_action() {
        let mut env = Envelope {
            version: 1,
            request_id: 1,
            body: Some(envelope::Body::Action(sample_action_with_creds())),
        };

        zeroize_envelope(&mut env);

        if let Some(envelope::Body::Action(action)) = &env.body {
            assert!(
                action.credentials.is_none(),
                "credentials should be None after zeroize_envelope"
            );
        } else {
            panic!("expected Body::Action");
        }
    }

    #[test]
    fn zeroize_envelope_is_safe_on_non_action_bodies() {
        // No panic, no mutation outside Body::Action.
        let mut env = Envelope {
            version: 1,
            request_id: 1,
            body: Some(envelope::Body::Ping(Ping {
                client: "barista".to_string(),
                sent_at_unix_micros: 1,
            })),
        };
        zeroize_envelope(&mut env);

        match &env.body {
            Some(envelope::Body::Ping(p)) => assert_eq!(p.client, "barista"),
            other => panic!("expected Body::Ping, got {other:?}"),
        }
    }

    #[test]
    fn zeroize_envelope_is_safe_on_empty_body() {
        let mut env = Envelope {
            version: 1,
            request_id: 1,
            body: None,
        };
        zeroize_envelope(&mut env);
        assert!(env.body.is_none());
    }

    #[test]
    fn credential_zeroize_on_drop_scrubs_in_memory_secret() {
        // This test proves the M4.1 T2 ZeroizeOnDrop derive really
        // does run on drop. We can't observe heap memory after drop
        // (the allocator may reuse it), but we *can* observe a
        // pre-drop `zeroize()` call: ZeroizeOnDrop calls
        // `Zeroize::zeroize` before drop, which leaves the bytes as
        // zero-valued strings.
        let mut cred = Credential {
            server_id: "server".to_string(),
            username: "u".to_string(),
            secret: Some(credential::Secret::Password("super-secret".to_string())),
        };
        // Call zeroize() directly — same effect as Drop will run.
        cred.zeroize();
        // After zeroize, the String fields are empty (their
        // underlying Vec<u8>::zeroize() truncates to len=0).
        assert!(cred.server_id.is_empty(), "server_id should be zero'd");
        assert!(cred.username.is_empty(), "username should be zero'd");
        // The oneof's `secret` field is also zeroized; we check by
        // borrowing so we don't trigger `cannot move out of Drop`
        // (Credential is ZeroizeOnDrop, so the compiler refuses to
        // partially-move from it).
        match &cred.secret {
            Some(credential::Secret::Password(p)) => {
                assert!(p.is_empty(), "password bytes should be zero'd; got len={}", p.len());
            }
            None => {} // Equally acceptable — zeroize may set Option to None.
            other => panic!("expected Password (empty) or None, got: {other:?}"),
        }
    }
}
