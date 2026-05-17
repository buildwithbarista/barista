//! Per-action client + server handles.
//!
//! The multiplex layer's public surface is the trio of types in this
//! module:
//!
//!  * [`StreamEvent`] — the discriminated union of envelopes that flow
//!    from server to client for a single action: progress, raw stream
//!    chunks, terminal result, terminal error.
//!  * [`ActionHandle`] — client-side handle returned from
//!    [`super::MuxClient::submit_action`]. Owns the per-action
//!    `mpsc::Receiver<StreamEvent>` and an outbound `Sender<Envelope>`
//!    for cancel. Drop sends a `CancelRequest` if the action has not
//!    already terminated.
//!  * [`IncomingAction`] — server-side bundle returned from
//!    [`super::MuxServer::next_action`]. Carries the inbound
//!    `ActionRequest`, a [`ResponseChannel`] for sending progress /
//!    chunks / the final result back, and a [`super::cancel::
//!    CancelToken`] the body polls to learn about cancel.
//!  * [`ResponseChannel`] — the server-side write handle. Wraps the
//!    same outbound `Sender<Envelope>` the dispatcher uses but pins
//!    the `action_id` so the body cannot accidentally cross-route.

use tokio::sync::mpsc;

use crate::envelope;
use crate::{
    ActionRequest, ActionResult, ActionStream, CancelRequest, Envelope, Error, ProgressEvent,
};

use super::cancel::{CancelToken, TerminationGuard};
use super::error::{MuxError, Result};

/// One inbound message addressed to a specific client-side action.
///
/// The dispatch loop in [`super::Multiplexer`] routes each envelope to
/// the right per-stream channel, unwrapping the `Envelope.body` so
/// callers don't have to re-match the oneof tag (the multiplexer has
/// already done that to route).
#[derive(Debug)]
pub enum StreamEvent {
    /// Structured progress signal (kind + optional coord/phase/etc.).
    /// Multiple per action; the action terminates with `Result` or
    /// `Error`.
    Progress(ProgressEvent),

    /// Raw stdout/stderr chunk. `payload` carries the bytes; `end ==
    /// true` on the last chunk for the stream. Multiple per action,
    /// possibly interleaved across stdout/stderr.
    Stream(ActionStream),

    /// Terminal outcome. Exactly one per action; receipt closes the
    /// per-action channel.
    Result(ActionResult),

    /// Terminal protocol-level error (e.g. `BAR-DAEMON-CANCEL-UNKNOWN`).
    /// Distinct from `ActionResult.error`: this is the daemon
    /// signalling that *the protocol itself* failed for this action,
    /// not that the mojo failed. Closes the per-action channel.
    Error(Error),
}

/// Client-side handle for a single in-flight action.
///
/// Returned from [`super::MuxClient::submit_action`]. Yields
/// [`StreamEvent`]s via [`next_event`](Self::next_event) until the
/// action terminates (an `ActionResult` or `Error` arrives, the
/// transport closes, or the handle is dropped/cancelled).
///
/// # Drop-cancels-the-action
///
/// If the handle is dropped before a terminal `StreamEvent` arrives,
/// the drop sends a `CancelRequest` on the outbound channel. This is
/// the intended ergonomic: a `let _h = client.submit_action(...)`
/// that goes out of scope at the end of a function does the
/// right thing automatically. Tests in `tests/mux_drop_cancels.rs`
/// verify the server observes the cancel.
///
/// The drop is best-effort: if the outbound channel is already
/// closed (the multiplexer has shut down), the drop silently does
/// nothing — there is no useful recovery, and panicking in `Drop` is
/// a footgun.
pub struct ActionHandle {
    action_id: String,
    inbound: mpsc::Receiver<StreamEvent>,
    outbound: mpsc::Sender<Envelope>,
    request_id: u64,
    terminated: TerminationGuard,
}

impl ActionHandle {
    /// Construct an `ActionHandle`. Crate-private: only the multiplexer
    /// builds these.
    pub(crate) fn new(
        action_id: String,
        inbound: mpsc::Receiver<StreamEvent>,
        outbound: mpsc::Sender<Envelope>,
        request_id: u64,
        terminated: TerminationGuard,
    ) -> Self {
        Self {
            action_id,
            inbound,
            outbound,
            request_id,
            terminated,
        }
    }

    /// The correlation id assigned to this action. Stable for the
    /// lifetime of the handle; matches the `action_id` on every
    /// inbound `StreamEvent` for this action.
    #[must_use]
    pub fn action_id(&self) -> &str {
        &self.action_id
    }

    /// Wait for the next inbound `StreamEvent` for this action.
    ///
    /// Returns `Ok(None)` after the terminal event has been delivered
    /// (the channel was closed by the dispatcher). Returns
    /// `Err(MuxError::MultiplexerShutDown)` if the multiplexer shut
    /// down before this action terminated.
    ///
    /// # Cancel safety
    ///
    /// `mpsc::Receiver::recv` is cancel-safe by tokio's contract:
    /// dropping the returned future before completion leaves the
    /// channel state intact, so the next `recv` resumes correctly.
    /// This means `ActionHandle::next_event` is safe to use inside a
    /// `select!` arm.
    pub async fn next_event(&mut self) -> Result<Option<StreamEvent>> {
        Ok(self.inbound.recv().await)
    }

    /// Cancel the in-flight action explicitly.
    ///
    /// Sends a `CancelRequest` on the outbound channel; the server
    /// observes it via its dispatch loop and triggers the matching
    /// `CancelToken`. The action's body is expected to exit within
    /// 100 ms (per M4.1 AC #2); see [`super::cancel`] for the
    /// propagation model.
    ///
    /// Consumes the handle: there is no usable post-cancel state.
    /// The `Drop` impl is a no-op after this call thanks to the
    /// `TerminationGuard`.
    ///
    /// Returns `Ok(())` even if the cancel envelope could not be sent
    /// (multiplexer already shut down) — by the time you've decided
    /// to cancel, you don't want failure to send to throw.
    ///
    /// # Cancel safety
    ///
    /// `mpsc::Sender::send` is cancel-safe at the granularity of a
    /// single message — dropping the future before it completes
    /// either delivers the message or leaves it un-sent (no partial
    /// state). In practice the outbound mpsc has a 64-message buffer
    /// (see [`super::OUTBOUND_BUFFER`](crate::mux::OUTBOUND_BUFFER))
    /// so this future almost always
    /// completes synchronously without yielding.
    pub async fn cancel(mut self) -> Result<()> {
        self.do_cancel().await;
        Ok(())
    }

    /// Internal cancel — shared between `cancel()` and `Drop`.
    /// Best-effort: errors swallowed (Drop can't propagate them, and
    /// `cancel()` documents that send failure is non-fatal).
    async fn do_cancel(&mut self) {
        if !self.terminated.try_flag() {
            // Already terminated (terminal event arrived, or cancel
            // already issued). Nothing to do.
            return;
        }
        let env = Envelope {
            version: 1,
            request_id: self.request_id,
            body: Some(envelope::Body::Cancel(CancelRequest {
                action_id: self.action_id.clone(),
                grace_period_ms: 0,
            })),
        };
        // `try_send` (not `send().await`) on Drop: we cannot `.await`
        // inside `Drop`. `cancel()` uses `send().await`. The
        // duplicated send logic is intentional — see the `Drop` impl.
        let _ = self.outbound.send(env).await;
    }
}

impl std::fmt::Debug for ActionHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ActionHandle")
            .field("action_id", &self.action_id)
            .field("request_id", &self.request_id)
            .field("terminated", &self.terminated.is_flagged())
            .finish()
    }
}

impl Drop for ActionHandle {
    fn drop(&mut self) {
        // `try_flag` returns `true` if we are the first to flip the
        // termination guard from false → true. That is the path
        // where we MUST send a CancelRequest: the action has not
        // observed a terminal Result, an explicit `.cancel()`, or
        // a server-side teardown, so the daemon still has the body
        // running and needs to be told to stop.
        if self.terminated.try_flag() {
            // We cannot `.await` here, so use `try_send` (the
            // outbound mpsc has a 64-message buffer, comfortably
            // large enough that a single CancelRequest on Drop
            // never trips backpressure in practice).
            let env = Envelope {
                version: 1,
                request_id: self.request_id,
                body: Some(envelope::Body::Cancel(CancelRequest {
                    action_id: std::mem::take(&mut self.action_id),
                    grace_period_ms: 0,
                })),
            };
            // Errors are intentionally swallowed:
            //   * `TrySendError::Full` — the writer task is wedged;
            //     a single dropped CancelRequest is a recoverable
            //     leak (action will eventually time out server-side).
            //   * `TrySendError::Closed` — multiplexer shut down;
            //     the connection is gone, server-side actions will
            //     be torn down by socket-close anyway.
            // Panicking in Drop is a non-starter; the leak is the
            // cost of the no-async constraint.
            let _ = self.outbound.try_send(env);
        }
    }
}

/// Server-side bundle yielded by [`super::MuxServer::next_action`].
///
/// Carries the inbound `ActionRequest` and the response channel the
/// body uses to send progress / chunks / the final result back. The
/// `CancelToken` is the body's view of the per-action cancel signal.
///
/// `Debug` is not derived — `ActionRequest` may carry a
/// `CredentialsEnvelope` and the redacted-Debug impls live on the
/// generated types, but printing the full `IncomingAction` adds no
/// useful diagnostic over just printing the `ActionRequest` itself.
pub struct IncomingAction {
    request: ActionRequest,
    response: ResponseChannel,
    cancel_token: CancelToken,
}

impl IncomingAction {
    /// Construct an `IncomingAction`. Crate-private: only the
    /// multiplexer builds these.
    pub(crate) fn new(
        request: ActionRequest,
        response: ResponseChannel,
        cancel_token: CancelToken,
    ) -> Self {
        Self {
            request,
            response,
            cancel_token,
        }
    }

    /// Borrow the inbound request. The body needs `mojo_coords`,
    /// classpath, working_directory, etc. — all on this struct.
    #[must_use]
    pub fn request(&self) -> &ActionRequest {
        &self.request
    }

    /// Consume and return `(request, response, cancel_token)` so the
    /// body owns each piece independently. The most common pattern
    /// is: take the request to extract the mojo, hand the cancel
    /// token to the worker pool, retain the response channel to send
    /// progress + result.
    #[must_use]
    pub fn split(self) -> (ActionRequest, ResponseChannel, CancelToken) {
        (self.request, self.response, self.cancel_token)
    }

    /// Borrow the cancel token without taking ownership. Used when
    /// the body needs to peek `is_cancelled()` early (e.g. to fail
    /// fast if cancel arrived before the action was scheduled).
    #[must_use]
    pub fn cancel_token(&self) -> &CancelToken {
        &self.cancel_token
    }
}

/// Server-side handle for writing responses back to the client for a
/// single action.
///
/// Pins the `action_id` so the body cannot accidentally send a
/// `ProgressEvent` with a mismatched id — the channel sets the
/// envelope's correlation fields, the body just provides the payload.
///
/// `request_id` is the connection-scoped id the client used when
/// submitting the action; we echo it on every response envelope so the
/// client's dispatcher routes correctly.
#[derive(Debug)]
pub struct ResponseChannel {
    action_id: String,
    request_id: u64,
    outbound: mpsc::Sender<Envelope>,
    /// Set once the body has sent a terminal `ActionResult` / `Error`,
    /// so the channel rejects further sends with `MultiplexerShutDown`
    /// (or, more precisely, with an error indicating the channel is
    /// closed for this action). Per-action only; doesn't affect other
    /// actions on the same connection.
    terminated: TerminationGuard,
}

impl ResponseChannel {
    pub(crate) fn new(action_id: String, request_id: u64, outbound: mpsc::Sender<Envelope>) -> Self {
        Self {
            action_id,
            request_id,
            outbound,
            terminated: TerminationGuard::new(),
        }
    }

    /// Send a structured progress event.
    ///
    /// The channel overwrites `progress.action_id` with the bound
    /// `action_id` (so bodies cannot misroute by accident). All other
    /// fields are taken verbatim.
    ///
    /// # Cancel safety
    ///
    /// `mpsc::Sender::send` is cancel-safe; see [`ActionHandle::cancel`].
    pub async fn send_progress(&self, mut event: ProgressEvent) -> Result<()> {
        event.action_id = self.action_id.clone();
        self.send_envelope(envelope::Body::Progress(event)).await
    }

    /// Send a stdout/stderr chunk.
    ///
    /// The channel overwrites `chunk.action_id` for the same reason
    /// as `send_progress`. `chunk.stream_id` is unchanged — the body
    /// is the authority on which of `stdout_stream_id` /
    /// `stderr_stream_id` this chunk belongs to.
    pub async fn send_chunk(&self, mut chunk: ActionStream) -> Result<()> {
        chunk.action_id = self.action_id.clone();
        self.send_envelope(envelope::Body::Stream(chunk)).await
    }

    /// Send the terminal `ActionResult`. After this returns, the
    /// channel rejects further sends. The dispatcher's per-action
    /// state is torn down by the matching inbound dispatcher when it
    /// observes the result.
    ///
    /// Calling `send_result` a second time returns
    /// `MultiplexerShutDown` (the channel is no longer usable). The
    /// `terminated` flag flip is atomic-and-checked, so concurrent
    /// callers race correctly.
    pub async fn send_result(self, mut result: ActionResult) -> Result<()> {
        if !self.terminated.try_flag() {
            return Err(MuxError::MultiplexerShutDown);
        }
        result.action_id = self.action_id.clone();
        let env = Envelope {
            version: 1,
            request_id: self.request_id,
            body: Some(envelope::Body::Result(result)),
        };
        self.outbound
            .send(env)
            .await
            .map_err(|_| MuxError::MultiplexerShutDown)
    }

    /// Send a terminal protocol-level `Error` (e.g. when the daemon
    /// can't even start the mojo). Like `send_result`, this is a
    /// terminator: after it returns, the channel rejects further
    /// sends.
    pub async fn send_error(self, mut err: Error) -> Result<()> {
        if !self.terminated.try_flag() {
            return Err(MuxError::MultiplexerShutDown);
        }
        err.action_id = self.action_id.clone();
        let env = Envelope {
            version: 1,
            request_id: self.request_id,
            body: Some(envelope::Body::Error(err)),
        };
        self.outbound
            .send(env)
            .await
            .map_err(|_| MuxError::MultiplexerShutDown)
    }

    /// The correlation id this channel is bound to. Stable for the
    /// lifetime of the channel.
    #[must_use]
    pub fn action_id(&self) -> &str {
        &self.action_id
    }

    async fn send_envelope(&self, body: envelope::Body) -> Result<()> {
        if self.terminated.is_flagged() {
            return Err(MuxError::MultiplexerShutDown);
        }
        let env = Envelope {
            version: 1,
            request_id: self.request_id,
            body: Some(body),
        };
        self.outbound
            .send(env)
            .await
            .map_err(|_| MuxError::MultiplexerShutDown)
    }
}
