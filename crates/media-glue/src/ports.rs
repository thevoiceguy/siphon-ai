//! A held RTP port pair for media forge does not run itself.
//!
//! Every classic call gets its ports as a side effect of allocating a
//! forge `MediaSession`: the session owns the pair, and releasing the
//! session releases it. A **WebRTC leg has no session** — forge-webrtc
//! owns its own socket — so nothing in that chain applies, and a
//! browser call would otherwise consume a port that the pool never
//! knew it handed out.
//!
//! That matters for three separate reasons, and it is worth being
//! precise about which, because only one of them is bookkeeping:
//!
//! 1. **Reachability.** `[media].rtp_port_range` is what an operator
//!    opened in a firewall. A socket bound outside it is unreachable in
//!    exactly the deployments that need it most.
//! 2. **Capacity.** The pool's size *is* the concurrent-call ceiling,
//!    and `siphon_ai_rtp_port_pairs_allocated` is how an operator
//!    watches it approach. Media outside the pool makes the gauge
//!    under-report the real load.
//! 3. **The reserved band.** `[media].reserved_outbound_calls` (#556)
//!    protects origination from being starved by inbound. A browser
//!    call that draws from nowhere ignores the floor while still
//!    consuming a real slot — the exact starvation the setting exists
//!    to prevent, made invisible.
//!
//! [`PortReservation`] closes all three: the pair comes from the same
//! pool, under the same floor, and the socket is bound to it.
//!
//! # Release is on `Drop`, deliberately
//!
//! Forge's release is `async`, which `Drop` cannot await, so this
//! spawns the release. The alternative — an explicit `release().await`
//! on the teardown path — is what a leak looks like the first time
//! someone adds an early `return` above it. `DEV_PLAN_WebRTC.md`
//! Phase 0 exists because that class of bug is hard to see and easy to
//! write; a guard makes forgetting impossible rather than merely
//! discouraged.

use std::sync::Arc;

use forge_engine::SessionManager;
use forge_rtp::PortPair;
use tracing::{debug, warn};

/// A port pair held for the life of a non-session media leg.
///
/// Dropping it returns the pair to the pool.
pub struct PortReservation {
    pair: PortPair,
    manager: Arc<SessionManager>,
}

impl std::fmt::Debug for PortReservation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PortReservation")
            .field("rtp_port", &self.pair.rtp_port)
            .finish_non_exhaustive()
    }
}

impl PortReservation {
    /// Take a pair from `manager`'s pool, leaving `min_free` behind.
    ///
    /// The floor is evaluated inside the pool's own critical section,
    /// so concurrent callers cannot each see the same free count and
    /// collectively dip below it.
    pub async fn take(
        manager: Arc<SessionManager>,
        min_free: usize,
    ) -> Result<Self, forge_core::ForgeError> {
        let pair = manager.reserve_port_pair(min_free).await?;
        debug!(
            rtp_port = pair.rtp_port,
            min_free, "reserved an RTP port pair for a non-session media leg"
        );
        Ok(Self { pair, manager })
    }

    /// The even port of the pair — where the media socket binds.
    ///
    /// A WebRTC leg needs only this one (BUNDLE plus `a=rtcp-mux` put
    /// RTCP on the same port). The odd port is still held rather than
    /// handed to someone else, because a *pair* is the unit the pool,
    /// the capacity gauge, and the reserved band all count in; drawing
    /// half a pair would make a browser call look cheaper than a SIP
    /// call when it occupies the same slot.
    pub fn rtp_port(&self) -> u16 {
        self.pair.rtp_port
    }
}

impl Drop for PortReservation {
    fn drop(&mut self) {
        let manager = Arc::clone(&self.manager);
        let pair = self.pair;
        // Release needs the pool's async mutex; hand it to the runtime.
        // Outside a runtime (a test dropping one synchronously, or
        // shutdown) there is nothing to release *into* that will
        // outlive the process, so a warning is the honest outcome.
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(async move {
                    manager.release_port_pair(pair).await;
                    debug!(rtp_port = pair.rtp_port, "released RTP port pair");
                });
            }
            Err(_) => {
                warn!(
                    rtp_port = pair.rtp_port,
                    "port reservation dropped outside a tokio runtime; \
                     pair not returned to the pool"
                );
            }
        }
    }
}
