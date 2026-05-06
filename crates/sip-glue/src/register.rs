//! Outbound REGISTER mode — UAC client per `[[register]]` block.
//!
//! In UAS / "trunk" deployments the daemon just listens. In UAC /
//! "registered phone on PBX" deployments (CLAUDE.md §7.2) the
//! daemon also REGISTERs against one or more upstream PBX/SBC
//! servers, so they treat us as a regular SIP endpoint and send
//! INVITEs over the registered transport.
//!
//! ## What v1 implements
//!
//! - One async task per `[[register]]` block.
//! - Initial REGISTER + Digest auth retry (handled by
//!   `IntegratedUAC::register` upstream).
//! - Periodic refresh at `expires - 60s` margin.
//! - Fixed 30 s backoff on registration failure.
//! - Per-registration `RegistrationState` snapshot for telemetry.
//! - `resolve_source(peer)` so the routing layer's
//!   [`crate::handler::RegisterSourceResolver`] can map an inbound
//!   request's peer address back to the `[[register]].name` it
//!   arrived on.
//!
//! ## What v1 doesn't do
//!
//! - DNS-resolved registrar hostnames (literal IP only — see
//!   `siphon_ai_config::compile.rs` for the deferral note).
//! - SRV / NAPTR resolution.
//! - SIGHUP reload (registrations are static for the process
//!   lifetime).
//! - Per-registration TLS client roots (the daemon's webpki roots
//!   apply to all TLS registrations).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use parking_lot::RwLock;
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

/// Primitive shape of one `[[register]]` block as the manager
/// consumes it.
///
/// Decoupled from `siphon_ai_config::RegistrationEntry` to avoid a
/// dep-graph cycle (`sip-glue → config → core → sip-glue`). The
/// daemon binary translates between the two at the seam.
#[derive(Debug, Clone)]
pub struct RegistrationEntry {
    pub name: String,
    pub server_addr: SocketAddr,
    pub register_on_startup: bool,
}

/// High-level state of one registration. Maps directly to the
/// `state` label on the `siphon_ai_registrations` gauge so dashboards
/// don't need a re-mapping table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegistrationStatus {
    /// First REGISTER not yet attempted (or process just started).
    Pending,
    /// Last REGISTER returned 2xx; the daemon will refresh before
    /// `expires_at`.
    Registered,
    /// Last REGISTER attempt failed (4xx/5xx, timeout, transport
    /// error). The daemon will retry after a backoff.
    Failed,
    /// `[[register]].register_on_startup = false` — the block is
    /// configured but the daemon won't drive REGISTER until told to.
    /// v1 has no "tell to register" RPC; this is reserved for the
    /// admin-endpoint follow-up.
    Disabled,
}

impl RegistrationStatus {
    /// Stable wire-format string mirrored on the metrics label.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Registered => "registered",
            Self::Failed => "failed",
            Self::Disabled => "disabled",
        }
    }
}

/// One-row snapshot of a registration's status. `Clone` so callers
/// can pull a snapshot without holding the manager's lock.
#[derive(Debug, Clone)]
pub struct RegistrationState {
    pub name: String,
    pub server_addr: SocketAddr,
    pub status: RegistrationStatus,
    /// When the most recent REGISTER attempt completed (success or
    /// failure). `None` before the first attempt.
    pub last_attempt_at: Option<DateTime<Utc>>,
    /// When the current registration expires (registrar's view).
    /// `Some` only when status is `Registered`.
    pub expires_at: Option<DateTime<Utc>>,
    /// Free-form description of the most recent error. Used for
    /// admin/debug introspection; not load-bearing for routing.
    pub last_error: Option<String>,
}

impl RegistrationState {
    fn new_pending(cfg: &RegistrationEntry) -> Self {
        let status = if cfg.register_on_startup {
            RegistrationStatus::Pending
        } else {
            RegistrationStatus::Disabled
        };
        Self {
            name: cfg.name.clone(),
            server_addr: cfg.server_addr,
            status,
            last_attempt_at: None,
            expires_at: None,
            last_error: None,
        }
    }
}

/// Process-wide table of registration tasks. Cheap to clone — Arc
/// inside.
#[derive(Debug, Clone, Default)]
pub struct RegistrationManager {
    inner: Arc<RegistrationManagerInner>,
}

#[derive(Debug, Default)]
struct RegistrationManagerInner {
    /// `name → state`. Snapshots are cloned out under the read lock.
    states: RwLock<HashMap<String, RegistrationState>>,
    /// Reverse map: `server_addr → name`. Used by
    /// [`RegistrationManager::resolve_source`] to identify which
    /// registration an inbound INVITE arrived on.
    by_addr: RwLock<HashMap<SocketAddr, String>>,
    /// Fires on shutdown — every task awaits it via `notified()`.
    shutdown: Notify,
}

impl RegistrationManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// Pre-populate the state table from the parsed configs. Called
    /// at startup before [`Self::spawn_with`] so a `/metrics` scrape
    /// during the cold-start window already shows `pending`/`disabled`
    /// rows rather than a blank gauge.
    pub fn seed(&self, configs: &[RegistrationEntry]) {
        let mut states = self.inner.states.write();
        let mut by_addr = self.inner.by_addr.write();
        for cfg in configs {
            states
                .entry(cfg.name.clone())
                .or_insert_with(|| RegistrationState::new_pending(cfg));
            by_addr.entry(cfg.server_addr).or_insert(cfg.name.clone());
        }
    }

    /// Snapshot every registration's state. Order is unspecified.
    pub fn snapshot(&self) -> Vec<RegistrationState> {
        self.inner.states.read().values().cloned().collect()
    }

    /// State of one registration by name.
    pub fn get(&self, name: &str) -> Option<RegistrationState> {
        self.inner.states.read().get(name).cloned()
    }

    /// Map an inbound peer address back to the registration name it
    /// matches, if any. Used by the routing layer's
    /// `RegisterSourceResolver`. Returns `None` for unregistered
    /// inbound (treated as `"trunk"` by the caller).
    pub fn resolve_source(&self, peer: SocketAddr) -> Option<String> {
        self.inner.by_addr.read().get(&peer).cloned()
    }

    /// Signal every running task to shut down. Tasks observe this
    /// via the shared [`Notify`] and exit at their next loop iter.
    pub fn shutdown(&self) {
        self.inner.shutdown.notify_waiters();
    }

    /// Internal hook used by the per-registration task to update
    /// state. Public-but-unstable; the daemon binary's task driver
    /// calls it.
    pub fn set_status(
        &self,
        name: &str,
        status: RegistrationStatus,
        last_error: Option<String>,
        expires_at: Option<DateTime<Utc>>,
    ) {
        let mut guard = self.inner.states.write();
        if let Some(state) = guard.get_mut(name) {
            state.status = status;
            state.last_attempt_at = Some(Utc::now());
            state.last_error = last_error;
            // `expires_at` is only set when the new status is
            // `Registered`; otherwise we clear it so dashboards
            // don't show a stale expiry.
            state.expires_at = if status == RegistrationStatus::Registered {
                expires_at
            } else {
                None
            };
        } else {
            warn!(
                name,
                "set_status for unknown registration; this is a programming bug"
            );
        }
    }

    /// Future for tasks to await. Returns when [`Self::shutdown`]
    /// is called.
    pub fn shutdown_signal(&self) -> ShutdownSignal {
        ShutdownSignal {
            inner: Arc::clone(&self.inner),
        }
    }
}

/// Cheap, cloneable handle to the manager's shutdown notify. Tasks
/// hold one to react to shutdown.
pub struct ShutdownSignal {
    inner: Arc<RegistrationManagerInner>,
}

impl ShutdownSignal {
    pub async fn cancelled(&self) {
        self.inner.shutdown.notified().await;
    }
}

/// Refresh / retry timing. The daemon's task driver consults these
/// constants — exposed so they can be overridden in tests.
pub mod timing {
    use std::time::Duration;

    /// Subtract from registrar-reported expires to compute when
    /// to re-REGISTER. Standard "refresh half" practice would be
    /// safer but 60s is what most enterprise PBXes expect.
    pub const REFRESH_MARGIN: Duration = Duration::from_secs(60);

    /// Fixed retry interval after a failed REGISTER. v1 doesn't
    /// implement exponential backoff — a fixed 30s is gentler on
    /// the registrar than a tight loop and predictable for
    /// operators reading logs.
    pub const FAILURE_BACKOFF: Duration = Duration::from_secs(30);

    /// Floor on the refresh delay. If a registrar grants an
    /// unreasonably short expires we don't want to hammer it; sleep
    /// at least this long even if `expires - REFRESH_MARGIN` is
    /// below it.
    pub const MIN_REFRESH_DELAY: Duration = Duration::from_secs(5);
}

/// Compute when to refresh given a registrar-reported expires.
/// Pulled out so the per-task driver and tests share the same
/// math.
pub fn refresh_delay(expires: Duration) -> Duration {
    let raw = expires.saturating_sub(timing::REFRESH_MARGIN);
    raw.max(timing::MIN_REFRESH_DELAY)
}

/// Convenience: spawn a no-op registration task that just sets the
/// state to `Disabled` and waits for shutdown. The daemon binary
/// uses this for blocks where `register_on_startup = false`. Real
/// register-on-startup tasks live in the daemon binary because they
/// need an `IntegratedUAC` whose construction sip-glue doesn't own.
pub fn spawn_disabled_task(manager: RegistrationManager, name: String) -> JoinHandle<()> {
    let signal = manager.shutdown_signal();
    tokio::spawn(async move {
        info!(name = %name, "registration disabled by config");
        signal.cancelled().await;
        debug!(name = %name, "disabled registration task exiting");
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(name: &str, addr: &str, on_startup: bool) -> RegistrationEntry {
        RegistrationEntry {
            name: name.into(),
            server_addr: addr.parse().unwrap(),
            register_on_startup: on_startup,
        }
    }

    #[test]
    fn seed_creates_pending_states_for_register_on_startup() {
        let mgr = RegistrationManager::new();
        mgr.seed(&[cfg("trunk-a", "10.0.0.1:5060", true)]);
        let s = mgr.get("trunk-a").unwrap();
        assert_eq!(s.status, RegistrationStatus::Pending);
        assert_eq!(s.server_addr.port(), 5060);
        assert!(s.last_attempt_at.is_none());
        assert!(s.expires_at.is_none());
    }

    #[test]
    fn seed_creates_disabled_state_when_register_on_startup_false() {
        let mgr = RegistrationManager::new();
        mgr.seed(&[cfg("trunk-b", "10.0.0.2:5060", false)]);
        let s = mgr.get("trunk-b").unwrap();
        assert_eq!(s.status, RegistrationStatus::Disabled);
    }

    #[test]
    fn resolve_source_matches_registrar_addr() {
        let mgr = RegistrationManager::new();
        mgr.seed(&[
            cfg("trunk-a", "10.0.0.1:5060", true),
            cfg("trunk-b", "10.0.0.2:5060", true),
        ]);
        assert_eq!(
            mgr.resolve_source("10.0.0.1:5060".parse().unwrap())
                .as_deref(),
            Some("trunk-a")
        );
        assert_eq!(
            mgr.resolve_source("10.0.0.2:5060".parse().unwrap())
                .as_deref(),
            Some("trunk-b")
        );
        assert!(mgr
            .resolve_source("10.0.0.99:5060".parse().unwrap())
            .is_none());
    }

    #[test]
    fn set_status_records_last_attempt_and_clears_expires_on_failure() {
        let mgr = RegistrationManager::new();
        mgr.seed(&[cfg("t", "10.0.0.1:5060", true)]);
        let now = Utc::now();
        mgr.set_status(
            "t",
            RegistrationStatus::Registered,
            None,
            Some(now + chrono::Duration::seconds(3600)),
        );
        let s = mgr.get("t").unwrap();
        assert_eq!(s.status, RegistrationStatus::Registered);
        assert!(s.expires_at.is_some());
        assert!(s.last_attempt_at.is_some());

        mgr.set_status(
            "t",
            RegistrationStatus::Failed,
            Some("403 Forbidden".into()),
            // Even if the caller passes Some, we clear it on failure.
            Some(now + chrono::Duration::seconds(3600)),
        );
        let s = mgr.get("t").unwrap();
        assert_eq!(s.status, RegistrationStatus::Failed);
        assert!(
            s.expires_at.is_none(),
            "expires_at must be cleared on failure"
        );
        assert_eq!(s.last_error.as_deref(), Some("403 Forbidden"));
    }

    #[test]
    fn snapshot_returns_all_seeded_states() {
        let mgr = RegistrationManager::new();
        mgr.seed(&[
            cfg("a", "10.0.0.1:5060", true),
            cfg("b", "10.0.0.2:5060", false),
        ]);
        let mut names: Vec<String> = mgr.snapshot().into_iter().map(|s| s.name).collect();
        names.sort();
        assert_eq!(names, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn refresh_delay_subtracts_margin() {
        assert_eq!(
            refresh_delay(Duration::from_secs(3600)),
            Duration::from_secs(3540)
        );
    }

    #[test]
    fn refresh_delay_floors_at_min_for_short_expires() {
        // Registrar grants 30s — margin of 60s would give negative;
        // we floor at MIN_REFRESH_DELAY.
        assert_eq!(
            refresh_delay(Duration::from_secs(30)),
            timing::MIN_REFRESH_DELAY
        );
    }

    #[test]
    fn cloned_manager_shares_state() {
        let a = RegistrationManager::new();
        a.seed(&[cfg("x", "10.0.0.1:5060", true)]);
        let b = a.clone();
        assert!(b.get("x").is_some());
        b.set_status("x", RegistrationStatus::Registered, None, None);
        assert_eq!(a.get("x").unwrap().status, RegistrationStatus::Registered);
    }

    #[tokio::test]
    async fn shutdown_signal_wakes_waiters() {
        let mgr = RegistrationManager::new();
        let signal = mgr.shutdown_signal();
        let waiter = tokio::spawn(async move {
            signal.cancelled().await;
        });
        // Give the waiter a tick to register itself with the
        // Notify before we send.
        tokio::task::yield_now().await;
        mgr.shutdown();
        tokio::time::timeout(Duration::from_millis(500), waiter)
            .await
            .expect("waiter wakes within 500ms")
            .expect("task does not panic");
    }
}
