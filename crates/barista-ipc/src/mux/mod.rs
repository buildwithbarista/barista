//! Streaming, multiplexing, and cancellation on top of [`crate::Transport`].
//!
//! This module wraps a single bidirectional [`crate::Transport`] (UDS on
//! Unix, named pipe on Windows) and turns it into a multiplexed
//! connection-level protocol per PRD §12.4. Multiple concurrent
//! [`ActionHandle`]s can be in flight on one connection without head-of-
//! line blocking; cancellation propagates within 100 ms (M4.1 AC #2);
//! all inbound + outbound IO is funnelled through two background tasks
//! to keep the codec single-owner.
//!
//! # Architecture
//!
//! ```text
//!                        ┌─────────────────────────────┐
//!                        │  application code (CLI /    │
//!                        │  barback daemon body)       │
//!                        └─────────────────────────────┘
//!                          │ submit_action /          ▲ next_event /
//!                          │ next_action              │ send_progress
//!                          ▼                          │ send_chunk
//!  ┌────────────────────────────────────────────────────────────────┐
//!  │ MuxClient / MuxServer + per-stream                            │
//!  │   mpsc::channel<StreamEvent> (inbound)                        │
//!  │   mpsc::channel<Envelope>    (outbound, OUTBOUND_BUFFER = 64) │
//!  └─────────────┬───────────────────────────────────┬─────────────┘
//!                │ Sender<Envelope>                  │ Receiver<StreamEvent>
//!                ▼                                   ▲
//!         ┌──────────────────┐               ┌──────────────────┐
//!         │ writer task      │               │ reader task      │
//!         │ owns the Sink<>  │               │ owns the Stream<>│
//!         │ half of Framed   │               │ half of Framed   │
//!         └────────┬─────────┘               └──────────┬───────┘
//!                  │ Transport::send                    │ Transport::recv
//!                  └────────────────────────────────────┘
//!                            │
//!                            ▼
//!                  ┌─────────────────────┐
//!                  │  Framed<UDS|Pipe>   │
//!                  │ LengthDelimitedCodec│
//!                  └─────────────────────┘
//! ```
//!
//! The writer and reader tasks each own exactly one half of the
//! `Framed`, produced by [`SplitTransport::split`]. The application
//! code never touches the codec directly — it talks to per-stream
//! channels. This is what makes 32 concurrent actions work without
//! corruption: only the reader task ever calls `recv` on the wire, and
//! only the writer task ever calls `send`. The wire is serialized; the
//! application layer is parallel.
//!
//! Splitting is mandatory (not a `Mutex` wrap): a single mutex held
//! across `recv().await` would starve the writer for the entire
//! duration of an idle wait on inbound data, deadlocking the round
//! trip. `SplitSink` / `SplitStream` from `futures-util` use an
//! internal `BiLock` that is released between polls, so the two halves
//! can poll their respective socket directions in parallel.
//!
//! # Stream IDs
//!
//! Each action gets a UUIDv7 (`uuid::Uuid::now_v7()`) as its correlation
//! id. UUIDv7 is **timestamp-first**: the first 48 bits are unix-millis,
//! so:
//!
//!  * Sort order matches submission order — log correlation across
//!    multiple actions stays chronological without per-line timestamps.
//!  * Collisions are vanishingly unlikely even with bulk-submit
//!    bursts (the 74 random bits after the timestamp give ~2^37 IDs/ms
//!    before a 1% collision probability).
//!  * No coordination needed across processes — two CLIs hitting the
//!    same daemon won't collide on action ids the way a `u64` counter
//!    would.
//!
//! Alternatives considered:
//!
//!  * **`u64` monotonic counter.** Cheapest, but needs a per-process
//!    seed (e.g. process-start unix-nanos) to avoid id reuse across
//!    daemon restarts. Logs sort by counter order, not time.
//!  * **Snowflake.** Needs a worker-id assignment step the workspace
//!    doesn't have.
//!  * **UUIDv4 (random).** Drops the temporal ordering benefit for
//!    zero gain.
//!
//! UUIDv7 wins on the two axes that matter: zero coordination + sortable.
//!
//! # Backpressure model
//!
//! The connection has two bounded `tokio::sync::mpsc` channels:
//!
//!  * Per-action **inbound** (server → client; client-side mpsc):
//!    buffer = [`PER_ACTION_BUFFER`](crate::mux::PER_ACTION_BUFFER)
//!    (32 events). A slow client falls
//!    behind on `next_event` → the reader task's `Sender::send().await`
//!    blocks → the reader stops calling `recv` on the wire → the
//!    server's writer task fills its own outbound buffer → server-side
//!    `send_chunk` / `send_progress` block on `Sender::send().await`.
//!    This is the natural backpressure chain that PRD §12.10 calls
//!    out: a slow consumer slows the producer without dropping bytes.
//!
//!  * Connection-wide **outbound** (this side → wire): buffer =
//!    [`OUTBOUND_BUFFER`](crate::mux::OUTBOUND_BUFFER) (64 envelopes).
//!    Multiple per-action
//!    response channels share a single writer. If the writer is
//!    slow (e.g. peer is back-pressuring at the TCP/UDS layer), all
//!    response-side `send_*` calls eventually block on `send().await`.
//!
//! 64 outbound is sized to absorb a short burst from each of 32
//! concurrent actions (2 envelopes/action of headroom) without
//! blocking on the steady-state. 32 per-action inbound is sized to
//! decouple the wire from a UI redraw cycle (a renderer that takes
//! 30 ms to render a chunk has room for ~32 chunks of buffered
//! lookahead). Both are v0.1 defaults; PRD §12.10's profiler hook
//! will inform the v0.2 retune.
//!
//! # Cancel safety
//!
//! Every public `async fn` in this module is cancel-safe in the
//! `select!`-drop sense:
//!
//!  * [`ActionHandle::next_event`] — wraps `mpsc::Receiver::recv`
//!    (cancel-safe by tokio's contract).
//!  * [`ActionHandle::cancel`] — wraps `mpsc::Sender::send` (cancel-
//!    safe; partial sends don't exist for mpsc).
//!  * [`MuxClient::submit_action`] — wraps `mpsc::Sender::send`.
//!  * [`MuxServer::next_action`] — wraps `mpsc::Receiver::recv`.
//!  * [`handle::ResponseChannel::send_progress` /
//!    `send_chunk` / `send_result` / `send_error`] — all wrap
//!    `mpsc::Sender::send`.
//!
//! Cancel-safety holds because no public method makes a state mutation
//! *between* the first `.await` and a subsequent one — every method is
//! either (a) a single `.await` on a cancel-safe primitive, or (b) a
//! pre-checked guard flag + single `.await`. Tests in `tests/mux_*`
//! exercise the select-drop paths to confirm.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::{mpsc, Mutex};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::envelope;
use crate::{
    ActionRequest, CancelRequest, Envelope, SplitTransport, TransportError, TransportReceiver,
    TransportSender,
};

pub mod cancel;
pub mod error;
pub mod handle;

pub use cancel::CancelToken;
pub use error::{MuxError, Result};
pub use handle::{ActionHandle, IncomingAction, ResponseChannel, StreamEvent};

use cancel::TerminationGuard;

// ---------------------------------------------------------------------------
// Tunables
// ---------------------------------------------------------------------------

/// Bounded buffer size for the connection-wide outbound mpsc.
///
/// 64 is the v0.1 default. See the module-level "Backpressure model"
/// section for the rationale; in short, it absorbs a short burst from
/// each of 32 concurrent actions without blocking the steady-state,
/// and stays small enough that backpressure surfaces promptly on a
/// slow peer.
pub const OUTBOUND_BUFFER: usize = 64;

/// Bounded buffer size for each per-action inbound mpsc.
///
/// 32 events is the v0.1 default. Large enough to decouple the wire
/// from a typical 30 ms UI redraw cycle; small enough that a runaway
/// producer is bounded in memory at 32 envelopes × 32 actions =
/// ~1024 in-flight events per connection.
pub const PER_ACTION_BUFFER: usize = 32;

// ---------------------------------------------------------------------------
// Connection-level state shared between the writer task, the reader
// task, the client API, and the server API.
// ---------------------------------------------------------------------------

/// Per-action client-side state held by the multiplexer.
struct ClientActionState {
    inbound_tx: mpsc::Sender<StreamEvent>,
    terminated: TerminationGuard,
}

/// Per-action server-side state held by the multiplexer.
struct ServerActionState {
    cancel_token: CancellationToken,
}

/// Connection-level shared state. Wrapped in `Arc<Mutex<_>>` and
/// shared by both the reader task and the public API.
///
/// The mutex is short-held — every method touching it does an O(1)
/// HashMap op + (optional) channel clone, then drops the guard before
/// any `.await`. This keeps the mutex from serialising the entire
/// dispatch loop.
struct MuxState {
    /// Per-action client-side state. Keyed by action id.
    clients: HashMap<String, ClientActionState>,
    /// Per-action server-side state. Keyed by action id.
    servers: HashMap<String, ServerActionState>,
    /// Set once the connection has been torn down (recv loop exited
    /// or writer loop exited). New submissions fail with
    /// `MultiplexerShutDown`.
    shutdown: bool,
}

impl MuxState {
    fn new() -> Self {
        Self {
            clients: HashMap::new(),
            servers: HashMap::new(),
            shutdown: false,
        }
    }
}

// ---------------------------------------------------------------------------
// The Multiplexer
// ---------------------------------------------------------------------------

/// Connection-level multiplexer.
///
/// Owns the underlying [`crate::Transport`] (split into its read +
/// write halves) and provides:
///
///  * a client-facing [`MuxClient`] for submitting `ActionRequest`s
///    and receiving `StreamEvent`s, and
///  * a server-facing [`MuxServer`] for accepting incoming actions
///    and writing responses.
///
/// The same `Multiplexer::new` is used on both sides of the
/// connection; the roles are symmetric at the IPC layer — a single
/// `MuxClient` + `MuxServer` pair lives on each side. The CLI uses
/// `client.submit_action()` and the daemon uses `server.next_action()`,
/// but the protocol does not enforce that asymmetry (both sides could
/// submit + accept, e.g. for future peer-driven flows).
pub struct Multiplexer {
    /// Background reader task handle. Kept so `Multiplexer::shutdown`
    /// (future) can abort it. For v0.1 we let the task exit naturally
    /// on socket close.
    _reader: JoinHandle<()>,
    /// Background writer task handle. Same lifecycle as `_reader`.
    _writer: JoinHandle<()>,
    /// Outbound channel sender. Cloned for every `ActionHandle` and
    /// `ResponseChannel` so the connection-wide writer is the sole
    /// consumer.
    outbound_tx: mpsc::Sender<Envelope>,
}

impl Multiplexer {
    /// Spawn the reader + writer tasks around `transport`. Returns the
    /// multiplexer plus a paired (`MuxClient`, `MuxServer`).
    ///
    /// `transport` is consumed and immediately split via
    /// [`SplitTransport::split`] into independently-owned sender and
    /// receiver halves. The two halves run in dedicated background
    /// tasks; the application code talks only to mpsc channels.
    ///
    /// The split is what makes concurrent `submit_action` +
    /// `next_event` deadlock-free. A shared-mutex approach would let
    /// the reader block on `recv().await` while holding the lock,
    /// starving the writer; the split lets both halves poll their
    /// respective socket directions independently.
    pub fn spawn<T>(transport: T) -> (Self, MuxClient, MuxServer)
    where
        T: SplitTransport + Send + 'static,
    {
        let state = Arc::new(Mutex::new(MuxState::new()));
        let (outbound_tx, outbound_rx) = mpsc::channel::<Envelope>(OUTBOUND_BUFFER);
        let (incoming_tx, incoming_rx) = mpsc::channel::<IncomingAction>(PER_ACTION_BUFFER);
        let incoming_rx = Arc::new(Mutex::new(incoming_rx));

        let (sender, receiver) = transport.split();

        let reader = tokio::spawn(reader_loop(
            receiver,
            Arc::clone(&state),
            outbound_tx.clone(),
            incoming_tx,
        ));
        let writer = tokio::spawn(writer_loop(sender, Arc::clone(&state), outbound_rx));

        let mux = Self {
            _reader: reader,
            _writer: writer,
            outbound_tx: outbound_tx.clone(),
        };

        let client = MuxClient {
            state: Arc::clone(&state),
            outbound_tx: outbound_tx.clone(),
        };
        let server = MuxServer {
            state,
            incoming_rx,
        };

        (mux, client, server)
    }

    /// Borrow the connection-wide outbound sender. Useful for
    /// integration tests that need to splice raw envelopes onto the
    /// wire without going through the higher-level API; not part of
    /// the production surface.
    #[doc(hidden)]
    pub fn outbound_sender(&self) -> mpsc::Sender<Envelope> {
        self.outbound_tx.clone()
    }
}

// ---------------------------------------------------------------------------
// Reader loop
// ---------------------------------------------------------------------------

/// Background task that owns the inbound half of the transport.
///
/// Loops `Transport::recv().await` and dispatches each envelope:
///
///  * `ActionRequest` → register a new server-side action and push
///    an `IncomingAction` onto the accept channel.
///  * `ActionStream` / `Progress` / `Result` / `Error` (with an
///    `action_id`) → route to the matching client-side per-action
///    inbound channel.
///  * `CancelRequest` → signal the matching server-side
///    `CancellationToken`.
///  * Other envelope variants (Ping/Pong/Status/Shutdown) →
///    dropped for now (v0.1). These flow through a sibling control
///    channel in a future revision; for the streaming work T6 owns,
///    the four shapes above are sufficient.
///
/// On `TransportError::Closed` or any other error the loop sets
/// `state.shutdown = true`, drops every per-action `Sender`, and
/// exits. Client-side `recv` consequently sees `None`; server-side
/// `next_action` sees `None`.
async fn reader_loop<R>(
    mut receiver: R,
    state: Arc<Mutex<MuxState>>,
    outbound_tx: mpsc::Sender<Envelope>,
    incoming_tx: mpsc::Sender<IncomingAction>,
) where
    R: TransportReceiver,
{
    loop {
        match receiver.recv().await {
            Ok(env) => {
                dispatch_inbound(env, &state, &outbound_tx, &incoming_tx).await;
            }
            Err(e) => {
                // Connection closed (or hard error). Surface as state
                // shutdown so client `next_event` sees `None` and
                // server `next_action` sees `None`.
                shutdown_state(&state, Some(e)).await;
                break;
            }
        }
    }
}

/// Dispatch one inbound envelope to its destination.
///
/// Crate-private so unit tests can exercise the routing logic in
/// isolation (synthesise an `Envelope`, call `dispatch_inbound`,
/// assert which channel received what).
async fn dispatch_inbound(
    env: Envelope,
    state: &Arc<Mutex<MuxState>>,
    _outbound_tx: &mpsc::Sender<Envelope>,
    incoming_tx: &mpsc::Sender<IncomingAction>,
) {
    let Some(body) = env.body else {
        return;
    };

    match body {
        envelope::Body::Action(req) => {
            handle_inbound_action(req, env.request_id, state, _outbound_tx, incoming_tx)
                .await;
        }
        envelope::Body::Stream(chunk) => {
            let id = chunk.action_id.clone();
            route_to_client(state, &id, StreamEvent::Stream(chunk)).await;
        }
        envelope::Body::Progress(prog) => {
            let id = prog.action_id.clone();
            route_to_client(state, &id, StreamEvent::Progress(prog)).await;
        }
        envelope::Body::Result(res) => {
            let id = res.action_id.clone();
            route_to_client(state, &id, StreamEvent::Result(res)).await;
            // Receipt of a terminal Result cleans up the per-action
            // client state (drop the Sender so the Receiver sees
            // None after this event drains).
            cleanup_client(state, &id).await;
        }
        envelope::Body::Error(err) => {
            // An action-scoped Error is terminal for the action; a
            // connection-scoped Error (empty action_id) is logged
            // and otherwise ignored at this layer.
            if err.action_id.is_empty() {
                return;
            }
            let id = err.action_id.clone();
            route_to_client(state, &id, StreamEvent::Error(err)).await;
            cleanup_client(state, &id).await;
        }
        envelope::Body::Cancel(CancelRequest { action_id, .. }) => {
            handle_inbound_cancel(&action_id, state).await;
        }
        // Other variants (Ping/Pong/StatusRequest/StatusResponse/
        // Shutdown) are not part of the streaming surface T6 owns.
        // They flow through a sibling control channel in a future
        // revision; for now we drop them silently. A SAST-style
        // assertion ("everything we don't handle is intentional")
        // lives in tests/mux_basic.rs.
        envelope::Body::Ping(_)
        | envelope::Body::Pong(_)
        | envelope::Body::Shutdown(_)
        | envelope::Body::StatusRequest(_)
        | envelope::Body::Status(_) => {}
    }
}

async fn handle_inbound_action(
    req: ActionRequest,
    request_id: u64,
    state: &Arc<Mutex<MuxState>>,
    outbound_tx: &mpsc::Sender<Envelope>,
    incoming_tx: &mpsc::Sender<IncomingAction>,
) {
    let token = CancellationToken::new();
    {
        let mut s = state.lock().await;
        if s.shutdown {
            return;
        }
        s.servers.insert(
            req.action_id.clone(),
            ServerActionState {
                cancel_token: token.clone(),
            },
        );
    }

    let response = ResponseChannel::new(req.action_id.clone(), request_id, outbound_tx.clone());
    let incoming = IncomingAction::new(req, response, CancelToken::from_inner(token));

    // If the accept channel is full / closed, drop the action. The
    // application has no business binding the accept channel to a
    // slow consumer; the deepest backpressure path here is
    // `MuxServer::next_action`-callers being slow, which is the
    // intended pressure point.
    let _ = incoming_tx.send(incoming).await;
}

async fn handle_inbound_cancel(action_id: &str, state: &Arc<Mutex<MuxState>>) {
    let token = {
        let s = state.lock().await;
        s.servers.get(action_id).map(|st| st.cancel_token.clone())
    };
    if let Some(token) = token {
        token.cancel();
    }
}

/// Route an event to the per-action client channel, if registered.
async fn route_to_client(state: &Arc<Mutex<MuxState>>, action_id: &str, ev: StreamEvent) {
    let tx = {
        let s = state.lock().await;
        s.clients.get(action_id).map(|st| st.inbound_tx.clone())
    };
    if let Some(tx) = tx {
        // `send().await` provides the inbound backpressure: a slow
        // consumer blocks the reader loop, which blocks the wire.
        // See the module-level Backpressure model section.
        let _ = tx.send(ev).await;
    }
    // Unknown action id → drop. Production logs would emit a metric
    // here; for v0.1 we keep it silent (the conformance test
    // tests/mux_basic.rs covers the negative case).
}

async fn cleanup_client(state: &Arc<Mutex<MuxState>>, action_id: &str) {
    let mut s = state.lock().await;
    if let Some(st) = s.clients.remove(action_id) {
        st.terminated.try_flag();
        // Dropping st here closes the inbound Sender — the receiver
        // will see None after draining the buffered events.
        drop(st);
    }
    s.servers.remove(action_id);
}

async fn shutdown_state(state: &Arc<Mutex<MuxState>>, _cause: Option<TransportError>) {
    let mut s = state.lock().await;
    s.shutdown = true;
    // Drop every per-action sender — clients see Receiver::recv()
    // return None. Server cancel tokens are cancelled so in-flight
    // bodies exit promptly.
    for (_, st) in s.servers.drain() {
        st.cancel_token.cancel();
    }
    s.clients.clear();
}

// ---------------------------------------------------------------------------
// Writer loop
// ---------------------------------------------------------------------------

/// Background task that owns the outbound half of the transport.
///
/// Drains the connection-wide outbound mpsc; each envelope is handed
/// to `Transport::send`. On send failure the loop flags shutdown
/// (consistent with the reader loop's behavior) and exits.
async fn writer_loop<S>(
    mut sender: S,
    state: Arc<Mutex<MuxState>>,
    mut outbound_rx: mpsc::Receiver<Envelope>,
) where
    S: TransportSender,
{
    while let Some(env) = outbound_rx.recv().await {
        if let Err(e) = sender.send(env).await {
            shutdown_state(&state, Some(e)).await;
            break;
        }
    }
}

// ---------------------------------------------------------------------------
// Client API
// ---------------------------------------------------------------------------

/// Client-side view of the multiplexer.
///
/// Holds a clone of the connection-wide outbound sender + the shared
/// state handle. `submit_action` allocates a per-action inbound
/// channel, registers it in the state, and returns the corresponding
/// `ActionHandle`.
///
/// `MuxClient: Clone` so multiple application threads can share a
/// single connection (concurrent submissions are routed correctly by
/// action id; the writer task serialises onto the wire).
#[derive(Clone)]
pub struct MuxClient {
    state: Arc<Mutex<MuxState>>,
    outbound_tx: mpsc::Sender<Envelope>,
}

impl MuxClient {
    /// Submit an `ActionRequest` on this connection. Returns the
    /// `ActionHandle` for receiving the action's stream events.
    ///
    /// Overwrites `req.action_id` with a freshly-minted UUIDv7 — the
    /// caller's `action_id` is ignored. We do this rather than honor
    /// the caller's choice because the multiplexer needs a guaranteed-
    /// unique id to register in its `clients` HashMap; honoring a
    /// caller-supplied id would require a uniqueness check + an error
    /// path on collision, neither of which buys us anything.
    ///
    /// # Cancel safety
    ///
    /// Single `.await` on `mpsc::Sender::send`. Dropping the returned
    /// future before completion either delivers the request envelope
    /// (in which case the next `next_action` on the peer side sees it
    /// and the registered per-action channel will collect any
    /// response that arrives) or doesn't (in which case the handle
    /// would never be returned anyway). The state-registration step
    /// happens *before* the `.await`, so a cancelled future leaves
    /// a registered per-action entry — that's a leak the connection-
    /// scoped cleanup (recv loop exit, shutdown_state) reaps.
    ///
    /// For v0.1 this leak is acceptable: a cancelled submit means
    /// the caller is throwing the handle away; the next action's
    /// recv loop won't see anything for this id; and the entry is
    /// cleared on connection close.
    pub async fn submit_action(&self, mut req: ActionRequest) -> Result<ActionHandle> {
        let action_id = uuid::Uuid::now_v7().to_string();
        req.action_id = action_id.clone();

        let (inbound_tx, inbound_rx) = mpsc::channel::<StreamEvent>(PER_ACTION_BUFFER);
        let terminated = TerminationGuard::new();
        let request_id = next_request_id();

        // Register before sending — the response could land before
        // submit_action returns if the peer is fast.
        {
            let mut s = self.state.lock().await;
            if s.shutdown {
                return Err(MuxError::MultiplexerShutDown);
            }
            s.clients.insert(
                action_id.clone(),
                ClientActionState {
                    inbound_tx,
                    terminated: terminated.clone(),
                },
            );
        }

        let env = Envelope {
            version: 1,
            request_id,
            body: Some(envelope::Body::Action(req)),
        };
        self.outbound_tx
            .send(env)
            .await
            .map_err(|_| MuxError::MultiplexerShutDown)?;

        Ok(ActionHandle::new(
            action_id,
            inbound_rx,
            self.outbound_tx.clone(),
            request_id,
            terminated,
        ))
    }
}

// ---------------------------------------------------------------------------
// Server API
// ---------------------------------------------------------------------------

/// Server-side view of the multiplexer.
///
/// `next_action` yields the next inbound `IncomingAction`. Each
/// `IncomingAction` carries the request + a `ResponseChannel` + a
/// `CancelToken` — see the [`handle`] module for details.
pub struct MuxServer {
    state: Arc<Mutex<MuxState>>,
    incoming_rx: Arc<Mutex<mpsc::Receiver<IncomingAction>>>,
}

impl MuxServer {
    /// Wait for the next inbound action on this connection.
    ///
    /// Returns `Ok(None)` when the connection has been torn down
    /// (the reader task has exited). Returns `Ok(Some(action))` on
    /// every fresh `ActionRequest`.
    ///
    /// # Cancel safety
    ///
    /// `mpsc::Receiver::recv` is cancel-safe; dropping the future
    /// before completion leaves the next action queued for the
    /// next `next_action` call.
    pub async fn next_action(&self) -> Result<Option<IncomingAction>> {
        let mut rx = self.incoming_rx.lock().await;
        Ok(rx.recv().await)
    }

    /// Returns `true` if the connection has been torn down. Cheap
    /// snapshot; tolerant of a small race against an in-flight close.
    pub async fn is_shutdown(&self) -> bool {
        self.state.lock().await.shutdown
    }
}

// ---------------------------------------------------------------------------
// Per-connection request-id counter
// ---------------------------------------------------------------------------

/// Connection-scoped envelope request_id allocator.
///
/// PRD §12.1 says the `request_id` is CLI-generated, unique per
/// connection. For v0.1 a simple atomic counter suffices — every
/// submission gets a fresh u64 and the daemon echoes it. Even at
/// 10^9 submissions/sec it would take >500 years to exhaust u64,
/// so wrap-around is not a real concern.
fn next_request_id() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

// ---------------------------------------------------------------------------
// Tests for the pure helpers. Integration tests covering multi-action
// flows + cancellation + concurrency live under `tests/mux_*.rs`.
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn next_request_id_is_monotonic() {
        let a = next_request_id();
        let b = next_request_id();
        let c = next_request_id();
        assert!(b > a);
        assert!(c > b);
    }

    #[test]
    fn outbound_buffer_is_64() {
        assert_eq!(OUTBOUND_BUFFER, 64);
    }

    #[test]
    fn per_action_buffer_is_32() {
        assert_eq!(PER_ACTION_BUFFER, 32);
    }
}
