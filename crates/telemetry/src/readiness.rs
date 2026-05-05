//! Process-wide readiness flag.
//!
//! Liveness vs. readiness in this daemon:
//!
//! - **Liveness** (`/health`) is "the process is responding." Always
//!   200; if you can't even handle the GET, k8s sends SIGTERM. We
//!   don't gate this — the HTTP server is up = the daemon is alive.
//! - **Readiness** (`/ready`) is "the daemon is serving calls."
//!   Flips to true once the SIP transport socket is bound and the
//!   listener spawned. Before that, we accept connections to the
//!   admin port (so probes work) but reject `/ready`.
//!
//! Cheap to clone (Arc-of-AtomicBool); the daemon hands out clones
//! to whichever component is in charge of declaring readiness. v1
//! has just the SIP listener; future components (pending REGISTER,
//! Homer connection) can AND-into the same flag with a small
//! readiness-aggregator if needed.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

#[derive(Debug, Clone, Default)]
pub struct ReadinessFlag {
    inner: Arc<AtomicBool>,
}

impl ReadinessFlag {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_ready(&self) -> bool {
        self.inner.load(Ordering::Acquire)
    }

    /// Mark the daemon as ready. Idempotent.
    pub fn mark_ready(&self) {
        self.inner.store(true, Ordering::Release);
    }

    /// Mark the daemon as not-yet-ready (useful in tests; the daemon
    /// itself only ever flips one direction during a process lifetime).
    pub fn mark_not_ready(&self) {
        self.inner.store(false, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_to_not_ready() {
        let f = ReadinessFlag::new();
        assert!(!f.is_ready());
    }

    #[test]
    fn mark_ready_is_idempotent() {
        let f = ReadinessFlag::new();
        f.mark_ready();
        assert!(f.is_ready());
        f.mark_ready();
        assert!(f.is_ready());
    }

    #[test]
    fn clone_shares_state() {
        let a = ReadinessFlag::new();
        let b = a.clone();
        assert!(!a.is_ready() && !b.is_ready());
        a.mark_ready();
        assert!(a.is_ready() && b.is_ready());
        b.mark_not_ready();
        assert!(!a.is_ready() && !b.is_ready());
    }
}
