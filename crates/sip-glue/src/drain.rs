//! Process-wide drain flag for graceful shutdown (0.17.0).
//!
//! Shaped like `telemetry::ReadinessFlag` (an `Arc<AtomicBool>`
//! newtype) but deliberately *separate*: "not ready" and "actively
//! draining, reject new work" are distinct states. A node can be
//! not-ready at startup without draining, and the INVITE handler wants
//! the precise "are we draining" signal — the `/ready` flip is then
//! just one action the drain phase takes, not the drain flag itself.
//! See `docs/design/DESIGN_GRACEFUL_SHUTDOWN.md` §3.2.
//!
//! One flag is built in the runtime, cloned into (a) the `run()` drain
//! logic that flips it and (b) the inbound INVITE handler that reads
//! it. It only ever flips one direction during a process lifetime.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Cheap-to-clone "are we draining for shutdown?" flag.
#[derive(Debug, Clone, Default)]
pub struct DrainFlag {
    inner: Arc<AtomicBool>,
}

impl DrainFlag {
    /// A fresh flag in the not-draining state.
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether the daemon is currently draining. Read by the inbound
    /// INVITE handler on every new INVITE.
    pub fn is_draining(&self) -> bool {
        self.inner.load(Ordering::Acquire)
    }

    /// Enter the draining state. Idempotent; the daemon only flips one
    /// direction per process lifetime.
    pub fn begin(&self) {
        self.inner.store(true, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_to_not_draining() {
        assert!(!DrainFlag::new().is_draining());
    }

    #[test]
    fn begin_is_idempotent_and_shared_across_clones() {
        let a = DrainFlag::new();
        let b = a.clone();
        assert!(!b.is_draining());
        a.begin();
        assert!(a.is_draining() && b.is_draining());
        a.begin();
        assert!(b.is_draining());
    }
}
