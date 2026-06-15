//! Call park — daemon-wide registry + per-call context (DEV_PLAN_0.7.0.md §2.4).
//!
//! Park is media-only: the WS session detaches, the caller hears hold
//! music, and the SIP dialog + RTP stay up until the call is retrieved
//! onto a fresh WS session (or times out / hangs up). The controller
//! owns the lifecycle (see `call.rs`); this module provides:
//!
//! - [`ParkRegistry`] — `call_id → ParkedEntry` (slot + parked-at), the
//!   §4.4-compliant table behind `GET /admin/v1/parked` and the
//!   `max_parked` cap. Same shape as `ConferenceRegistry` /
//!   `CallControlRegistry`: exact-id, insert on park, remove on
//!   retrieve/teardown, no reach into call internals.
//! - [`ParkContext`] — the per-call park config the controller needs
//!   (MOH file, timeout policy) plus a clone of the registry. Mapped by
//!   the daemon from `siphon-ai-config`'s `ParkConfig` (core doesn't dep
//!   on config — same pattern as `ConferenceLimits`).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::RwLock;
use thiserror::Error;
use tracing::{debug, warn};

/// What happens when a parked call hits its timeout. Core mirror of
/// `siphon-ai-config::ParkTimeoutAction`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParkTimeoutAction {
    /// Tear the call down (the default).
    Hangup,
    /// Leave it parked; the operator must retrieve or hang up.
    Keep,
}

/// Per-call park behaviour the controller applies on a park request.
/// Cheap to clone (`PathBuf` + small scalars).
#[derive(Debug, Clone)]
pub struct ParkSettings {
    /// Hold-music file (validated at load). `None` → comfort noise.
    pub moh_file: Option<PathBuf>,
    /// How long a call may stay parked. `None` = no timeout.
    pub timeout: Option<Duration>,
    pub timeout_action: ParkTimeoutAction,
}

/// Everything a [`CallController`](crate::CallController) needs to honour
/// park requests: the per-call settings, a clone of the daemon-wide
/// registry (for the cap + the admin list), and an optional webhook sink
/// for `call_parked` / `call_retrieved` / `park_timeout`. `None` on the
/// controller config means park is disabled for the call.
#[derive(Clone)]
pub struct ParkContext {
    pub settings: ParkSettings,
    pub registry: ParkRegistry,
    pub webhooks: Option<siphon_ai_webhooks::WebhookSinkHandle>,
}

impl std::fmt::Debug for ParkContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ParkContext")
            .field("settings", &self.settings)
            .field("webhooks", &self.webhooks.is_some())
            .finish()
    }
}

/// Why a park was refused.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ParkError {
    #[error("[park].max_parked reached ({max_parked})")]
    AtCapacity { max_parked: usize },
}

/// One parked call's metadata.
#[derive(Debug, Clone)]
struct ParkedEntry {
    slot: Option<String>,
    parked_at: Instant,
}

/// A parked call in the `GET /admin/v1/parked` view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParkSnapshot {
    pub call_id: String,
    pub slot: Option<String>,
    /// Whole seconds the call has been parked.
    pub parked_secs: u64,
}

/// Process-wide parked-call table. Cheap to clone (`Arc` inside).
#[derive(Debug, Clone)]
pub struct ParkRegistry {
    max_parked: usize,
    inner: Arc<RwLock<HashMap<String, ParkedEntry>>>,
}

impl ParkRegistry {
    pub fn new(max_parked: usize) -> Self {
        Self {
            max_parked,
            inner: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Register `call_id` as parked, enforcing `max_parked`. Called by
    /// the controller when it enters the parked state.
    pub fn try_park(&self, call_id: &str, slot: Option<String>) -> Result<(), ParkError> {
        let mut guard = self.inner.write();
        // A re-park of an already-parked id (shouldn't happen — the
        // controller guards on its own `parked` flag) just refreshes
        // the entry and doesn't count against the cap.
        if !guard.contains_key(call_id) && guard.len() >= self.max_parked {
            return Err(ParkError::AtCapacity {
                max_parked: self.max_parked,
            });
        }
        guard.insert(
            call_id.to_string(),
            ParkedEntry {
                slot,
                parked_at: Instant::now(),
            },
        );
        debug!(call_id, parked = guard.len(), "call parked");
        Ok(())
    }

    /// True if `call_id` is currently parked — the admin retrieve path
    /// checks this before signalling the call.
    pub fn is_parked(&self, call_id: &str) -> bool {
        self.inner.read().contains_key(call_id)
    }

    /// Drop the entry on retrieve / teardown. Unknown id is a no-op
    /// (teardown paths may race).
    pub fn remove(&self, call_id: &str) {
        if self.inner.write().remove(call_id).is_some() {
            debug!(call_id, "call unparked");
        } else {
            // Not an error, but worth a trace-level note for races.
            warn!(call_id, "park remove for an unknown call (already gone)");
        }
    }

    /// Live parked count.
    pub fn len(&self) -> usize {
        self.inner.read().len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.read().is_empty()
    }

    /// Snapshot for `GET /admin/v1/parked`. Sorted by `call_id`.
    pub fn snapshot(&self) -> Vec<ParkSnapshot> {
        let mut rows: Vec<ParkSnapshot> = self
            .inner
            .read()
            .iter()
            .map(|(id, e)| ParkSnapshot {
                call_id: id.clone(),
                slot: e.slot.clone(),
                parked_secs: e.parked_at.elapsed().as_secs(),
            })
            .collect();
        rows.sort_by(|a, b| a.call_id.cmp(&b.call_id));
        rows
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn try_park_enforces_cap() {
        let reg = ParkRegistry::new(2);
        assert!(reg.try_park("a", None).is_ok());
        assert!(reg.try_park("b", Some("lot-1".into())).is_ok());
        assert_eq!(
            reg.try_park("c", None).unwrap_err(),
            ParkError::AtCapacity { max_parked: 2 }
        );
        assert_eq!(reg.len(), 2);
    }

    #[test]
    fn re_park_same_id_does_not_count_against_cap() {
        let reg = ParkRegistry::new(1);
        assert!(reg.try_park("a", None).is_ok());
        // Same id again — refresh, still under cap.
        assert!(reg.try_park("a", Some("lot-2".into())).is_ok());
        assert_eq!(reg.len(), 1);
    }

    #[test]
    fn remove_frees_a_slot() {
        let reg = ParkRegistry::new(1);
        reg.try_park("a", None).unwrap();
        assert!(reg.try_park("b", None).is_err());
        reg.remove("a");
        assert!(reg.try_park("b", None).is_ok());
    }

    #[test]
    fn snapshot_lists_parked_calls_sorted() {
        let reg = ParkRegistry::new(8);
        reg.try_park("z", None).unwrap();
        reg.try_park("a", Some("lot-3".into())).unwrap();
        let snap = reg.snapshot();
        assert_eq!(snap.len(), 2);
        assert_eq!(snap[0].call_id, "a");
        assert_eq!(snap[0].slot.as_deref(), Some("lot-3"));
        assert_eq!(snap[1].call_id, "z");
    }

    #[test]
    fn is_parked_tracks_membership() {
        let reg = ParkRegistry::new(4);
        assert!(!reg.is_parked("a"));
        reg.try_park("a", None).unwrap();
        assert!(reg.is_parked("a"));
        reg.remove("a");
        assert!(!reg.is_parked("a"));
    }
}
