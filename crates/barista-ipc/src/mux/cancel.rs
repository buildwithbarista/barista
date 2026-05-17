//! Per-action cancellation primitive.
//!
//! [`CancelToken`] is the server-side handle that an action body holds
//! to learn when the client has cancelled. It wraps a
//! [`tokio_util::sync::CancellationToken`] so the action body can either:
//!
//!  * poll synchronously via `is_cancelled()` (cheap atomic load),
//!  * `await cancelled()` to be woken when cancel fires (used in
//!    `select!` arms), or
//!  * derive a child token for sub-task cancellation propagation.
//!
//! The client-side cancellation flow lives on [`super::handle::
//! ActionHandle`] and is structured separately: dropping the handle
//! enqueues a `CancelRequest` on the outbound channel, which the server
//! observes via its dispatch loop and uses to trigger the matching
//! `CancelToken`.
//!
//! # The 100 ms acceptance bound
//!
//! PRD §12.6 and M4.1 milestone AC #2 require that a cancel observed by
//! the daemon abort the in-flight action body within 100 ms. The
//! mechanism breaks down into two phases:
//!
//!   1. **Cancel propagation** (CLI → daemon): one `Envelope` send +
//!      one read on the daemon's recv loop. On a localhost UDS this
//!      is sub-millisecond at the 99th percentile.
//!   2. **Cancel observation** (daemon's dispatch → action body):
//!      `CancellationToken::cancel()` is wait-free and wakes every
//!      registered `cancelled()` future on the next runtime poll —
//!      microseconds on a multi-threaded tokio runtime.
//!
//! The 100 ms budget is therefore dominated by whatever the action
//! body does between `tokio::select!` checkpoints. Bodies that hold
//! the runtime in a tight CPU loop without yielding can violate the
//! bound; the convention in barback is that every action body wraps
//! its long-running work in `tokio::select!` with `token.cancelled()`
//! as one arm, or polls `token.is_cancelled()` at obvious yield
//! points. The conformance test (`tests/mux_cancel.rs`) enforces this
//! contract by spawning a server body that does exactly that.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio_util::sync::CancellationToken;

/// Server-side cancellation observer for a single in-flight action.
///
/// Construction is private to the multiplex layer; bodies receive a
/// `CancelToken` from `IncomingAction::cancel_token()`. Bodies should
/// either:
///
/// ```ignore
/// // Pattern A — await + select!
/// tokio::select! {
///     _ = token.cancelled() => return Status::Cancelled,
///     result = do_work() => return result,
/// }
///
/// // Pattern B — periodic polling at yield points
/// for chunk in stream {
///     if token.is_cancelled() { return Status::Cancelled; }
///     process(chunk).await;
/// }
/// ```
///
/// `Clone` is cheap (`Arc` clone) and required so the body can hand
/// a sub-task its own token reference without surrendering its own.
#[derive(Debug, Clone)]
pub struct CancelToken {
    inner: CancellationToken,
}

impl CancelToken {
    /// Wrap a `tokio_util::sync::CancellationToken`. Only the multiplex
    /// layer constructs these; bodies receive them.
    pub(crate) fn from_inner(inner: CancellationToken) -> Self {
        Self { inner }
    }

    /// `true` if the client has cancelled the action. Cheap atomic load
    /// — safe to call in a tight loop, but prefer `cancelled().await`
    /// in `select!` arms for prompt wake-up.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.inner.is_cancelled()
    }

    /// Returns a future that completes when the token is cancelled.
    ///
    /// # Cancel safety
    ///
    /// `cancelled()` is cancel-safe: dropping the future before it
    /// completes leaves the underlying token unobserved (the token's
    /// internal `Notify` queue handles drop correctly). Bodies can
    /// place this future in any number of `select!` arms without
    /// leaking.
    pub async fn cancelled(&self) {
        self.inner.cancelled().await;
    }
}

/// Atomic "has this `ActionHandle` already had `cancel` or `Drop` run"
/// guard. Shared between [`super::handle::ActionHandle`] and its `Drop`
/// impl so a manual `.cancel()` doesn't double-send a `CancelRequest`
/// when the handle is then dropped.
///
/// Implemented as a tiny `AtomicBool` wrapper so the guard is `Clone`
/// (the `ActionHandle` holds one ref and the inbound dispatcher holds
/// another, allowing the dispatcher to mark a handle terminated if the
/// server sends `ActionResult` first).
#[derive(Debug, Clone, Default)]
pub(crate) struct TerminationGuard {
    flagged: Arc<AtomicBool>,
}

impl TerminationGuard {
    /// Build a fresh, unflagged guard.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Atomically transition `false -> true` and return `true` if this
    /// call won the race. Subsequent callers see `false`.
    ///
    /// `Ordering::AcqRel` is the cheapest ordering that gives both
    /// callers a happens-before edge with whatever they paired with the
    /// flag flip (handle drop publishes "cancel sent" before any
    /// observer reads the flag).
    pub(crate) fn try_flag(&self) -> bool {
        self.flagged
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    /// Read the current flag state without modifying it.
    ///
    /// Made non-test because `ActionHandle`'s `Debug` impl needs to
    /// inspect the flag state without flipping it; gating it on
    /// `#[cfg(test)]` would force the Debug impl to do its own
    /// atomic load.
    pub(crate) fn is_flagged(&self) -> bool {
        self.flagged.load(Ordering::Acquire)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn termination_guard_only_one_caller_wins() {
        let g = TerminationGuard::new();
        assert!(!g.is_flagged());
        assert!(g.try_flag(), "first caller should win");
        assert!(g.is_flagged());
        assert!(!g.try_flag(), "second caller should lose");
        assert!(!g.try_flag(), "third caller should lose");
    }

    #[test]
    fn termination_guard_clones_share_state() {
        let g1 = TerminationGuard::new();
        let g2 = g1.clone();
        assert!(g1.try_flag(), "g1 wins");
        assert!(g2.is_flagged(), "g2 sees the flag");
        assert!(!g2.try_flag(), "g2 cannot re-flag");
    }

    #[tokio::test]
    async fn cancel_token_observes_cancel() {
        let inner = CancellationToken::new();
        let tok = CancelToken::from_inner(inner.clone());
        assert!(!tok.is_cancelled());
        inner.cancel();
        assert!(tok.is_cancelled());
        // The `cancelled()` future should complete immediately when
        // the token is already in the cancelled state.
        tok.cancelled().await;
    }
}
