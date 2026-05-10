//! Process-wide map from SIP `Call-ID` to the per-call
//! [`CallHandle`].
//!
//! The registry exists so the SIP-side method handlers (BYE, CANCEL)
//! can find the call task that owns a given dialog and ask it to
//! shut down. Without it a BYE arrives, the UAS sends 200 OK, and
//! the controller task keeps running unaware that the SIP leg is
//! gone — the call only ends when the WS server hangs up or the
//! forge tap notices RTP stop.
//!
//! ## Why SIP `Call-ID` and not the dialog id
//!
//! The dialog id is `(Call-ID, local_tag, remote_tag)`. It changes
//! on dialog-fork events (rare, but possible) and isn't fully
//! formed until the local tag is generated. SIP `Call-ID` is
//! present on every message in the same dialog tree from INVITE
//! through BYE, so it's the simplest correlator for our v1
//! single-dialog-per-call model. Per RFC 3261 §8.1.1.4 the Call-ID
//! is unique across the dialog's lifetime, which is the property
//! we need.
//!
//! ## Why not per-call state
//!
//! CLAUDE.md §4.4 says we never share per-call state across calls.
//! [`CallHandle`] is an `Arc<Notify>` — a fire-and-forget shutdown
//! signal, not state — so storing it in a process-wide map doesn't
//! violate the rule. Inserting and removing happens at call
//! setup/teardown (not hot path); lookup happens once per BYE.
//!
//! ## Concurrency
//!
//! Backed by `parking_lot::RwLock<HashMap<...>>`. CLAUDE.md §4.3
//! prohibits `std::sync::Mutex` on the audio path; the registry
//! never touches audio. `parking_lot` is already a workspace dep
//! and avoids the `tokio::sync::RwLock` overhead for what is
//! always a short, contention-free critical section.

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::RwLock;
use siphon_ai_sip_glue::DialogTerminator;
use tracing::{debug, warn};

use crate::call::CallHandle;

/// Process-wide handle table. Cheap to clone (`Arc` inside).
#[derive(Debug, Clone, Default)]
pub struct CallRegistry {
    inner: Arc<RwLock<HashMap<String, CallHandle>>>,
}

impl CallRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of currently-tracked calls. Useful for metrics and
    /// tests; not load-bearing for correctness.
    pub fn len(&self) -> usize {
        self.inner.read().len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.read().is_empty()
    }

    /// Insert `handle` under `sip_call_id`. If a handle for the
    /// same Call-ID already existed, the previous one is dropped
    /// and a warning is logged — that situation is a bug in the
    /// caller (two concurrent acceptances of the same Call-ID).
    pub fn insert(&self, sip_call_id: impl Into<String>, handle: CallHandle) {
        let key = sip_call_id.into();
        let mut guard = self.inner.write();
        if let Some(prev) = guard.insert(key.clone(), handle) {
            warn!(
                sip_call_id = %key,
                bridge_call_id = %prev.call_id(),
                "registry insert collided with existing entry; previous handle dropped"
            );
        } else {
            debug!(sip_call_id = %key, "registered call");
        }
    }

    /// Look up a handle by SIP Call-ID. Returns a clone — the
    /// underlying `CallHandle` is itself an `Arc`-of-`Notify`, so
    /// cloning is essentially free.
    pub fn lookup(&self, sip_call_id: &str) -> Option<CallHandle> {
        self.inner.read().get(sip_call_id).cloned()
    }

    /// Remove and return the handle for `sip_call_id`, if any. The
    /// controller task calls this on its way out so the entry
    /// doesn't leak after the call ends.
    pub fn remove(&self, sip_call_id: &str) -> Option<CallHandle> {
        let removed = self.inner.write().remove(sip_call_id);
        if removed.is_some() {
            debug!(sip_call_id = %sip_call_id, "deregistered call");
        }
        removed
    }

    /// Snapshot every currently-registered Call-ID. Order is
    /// unspecified. Intended for admin / debug introspection.
    pub fn snapshot_call_ids(&self) -> Vec<String> {
        self.inner.read().keys().cloned().collect()
    }
}

/// `DialogTerminator` impl: BYE / CANCEL look up the handle and
/// fire its shutdown notification. The actual entry removal happens
/// inside the spawned controller task on its way out (see
/// `crate::acceptor`), not here — that keeps "the controller exited
/// cleanly" as the single trigger for deregistration.
impl DialogTerminator for CallRegistry {
    fn terminate(&self, sip_call_id: &str) -> bool {
        match self.lookup(sip_call_id) {
            Some(handle) => {
                handle.shutdown();
                true
            }
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::call::{CallController, CallControllerConfig};
    use forge_core::CallId as ForgeCallId;
    use forge_engine::MediaBridgeManager;
    use siphon_ai_bridge::{
        AudioEncoding, AudioFormat, BridgeConfig, CallId as BridgeCallId, Direction, SipMeta,
        StartMsg,
    };
    use siphon_ai_media_glue::MediaTap;
    use std::collections::HashMap as StdHashMap;
    use std::sync::Arc;

    /// Build a real `CallHandle` by constructing a (never-run)
    /// `CallController`. Cheaper than mocking and exercises the
    /// actual handle plumbing.
    fn fresh_handle(bridge_call_id: &str) -> CallHandle {
        let manager = Arc::new(MediaBridgeManager::new());
        let tap =
            MediaTap::attach(&manager, &::std::sync::Arc::new(forge_core::EventBus::new()), ForgeCallId::new(bridge_call_id), 8000).expect("attach tap");
        let cfg = CallControllerConfig {
            call_id: BridgeCallId::new(bridge_call_id),
            bridge: BridgeConfig {
                ws_url: "ws://test/".into(),
                ..Default::default()
            },
            start: StartMsg {
                version: "1".into(),
                call_id: BridgeCallId::new(bridge_call_id),
                seq: 0,
                from: "x".into(),
                to: "y".into(),
                direction: Direction::Inbound,
                audio: AudioFormat {
                    encoding: AudioEncoding::Pcm16le,
                    sample_rate: 8000,
                    channels: 1,
                    frame_ms: 20,
                },
                sip: SipMeta {
                    call_id: "x@y".into(),
                    headers: StdHashMap::new(),
                },
            },
            media_tap: tap,
        };
        let (controller, handle) = CallController::new(cfg);
        // Drop the controller without running it; the handle still
        // works for shutdown signalling.
        drop(controller);
        handle
    }

    #[test]
    fn insert_lookup_remove_round_trip() {
        let reg = CallRegistry::new();
        assert_eq!(reg.len(), 0);

        let h = fresh_handle("siphon-1");
        reg.insert("abc@pbx", h.clone());
        assert_eq!(reg.len(), 1);

        let looked_up = reg.lookup("abc@pbx").expect("present");
        assert_eq!(looked_up.call_id().as_str(), "siphon-1");

        let removed = reg.remove("abc@pbx").expect("present");
        assert_eq!(removed.call_id().as_str(), "siphon-1");
        assert!(reg.is_empty());

        // Removing again is a no-op.
        assert!(reg.remove("abc@pbx").is_none());
    }

    #[test]
    fn lookup_returns_none_for_unknown_call_id() {
        let reg = CallRegistry::new();
        reg.insert("abc@pbx", fresh_handle("siphon-1"));
        assert!(reg.lookup("never-seen@pbx").is_none());
    }

    #[test]
    fn duplicate_insert_replaces_and_warns() {
        // Same Call-ID inserted twice — second insert wins; first
        // handle is dropped. (Tracing assertions are out of scope;
        // the regression we care about is that lookup returns the
        // *new* handle.)
        let reg = CallRegistry::new();
        reg.insert("dupe@pbx", fresh_handle("siphon-old"));
        reg.insert("dupe@pbx", fresh_handle("siphon-new"));
        assert_eq!(
            reg.lookup("dupe@pbx").unwrap().call_id().as_str(),
            "siphon-new"
        );
        assert_eq!(reg.len(), 1);
    }

    #[test]
    fn snapshot_lists_all_known_call_ids() {
        let reg = CallRegistry::new();
        reg.insert("a@pbx", fresh_handle("siphon-a"));
        reg.insert("b@pbx", fresh_handle("siphon-b"));
        let mut ids = reg.snapshot_call_ids();
        ids.sort();
        assert_eq!(ids, vec!["a@pbx".to_string(), "b@pbx".to_string()]);
    }

    #[test]
    fn cloned_registry_shares_state() {
        // CallRegistry is Clone; the Arc inside means clones see
        // the same underlying map. This is what lets the acceptor
        // and the BYE handler share one registry.
        let a = CallRegistry::new();
        let b = a.clone();
        a.insert("x@pbx", fresh_handle("siphon-1"));
        assert!(b.lookup("x@pbx").is_some());
        b.remove("x@pbx");
        assert!(a.is_empty());
    }

    // The "looked-up handle wakes the same controller" property is
    // exercised end-to-end in `tests/controller_lifecycle.rs` —
    // here the structural test (`cloned_registry_shares_state`)
    // pins the shared-Arc invariant and is enough.
}
