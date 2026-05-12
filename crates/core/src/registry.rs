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

/// Per-call session state the registry tracks. Beyond the shutdown
/// handle, we cache the answer SDP we sent for the initial INVITE so
/// re-INVITEs (hold/resume) can produce a matching answer with a
/// flipped direction without re-allocating ports or re-running
/// codec negotiation.
#[derive(Debug, Clone)]
pub struct CallEntry {
    pub handle: CallHandle,
    /// The SDP body sent in the 200 OK to the initial INVITE.
    /// `None` for legacy tests / callers that don't track it; the
    /// re-INVITE handler returns 501 in that case.
    pub answer_text: Option<String>,
}

/// Process-wide handle table. Cheap to clone (`Arc` inside).
#[derive(Debug, Clone, Default)]
pub struct CallRegistry {
    inner: Arc<RwLock<HashMap<String, CallEntry>>>,
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

    /// Insert a call entry under `sip_call_id`. If an entry for the
    /// same Call-ID already existed, the previous one is dropped
    /// and a warning is logged — that situation is a bug in the
    /// caller (two concurrent acceptances of the same Call-ID).
    pub fn insert(&self, sip_call_id: impl Into<String>, entry: CallEntry) {
        let key = sip_call_id.into();
        let mut guard = self.inner.write();
        if let Some(prev) = guard.insert(key.clone(), entry) {
            warn!(
                sip_call_id = %key,
                bridge_call_id = %prev.handle.call_id(),
                "registry insert collided with existing entry; previous handle dropped"
            );
        } else {
            debug!(sip_call_id = %key, "registered call");
        }
    }

    /// Look up a [`CallHandle`] by SIP Call-ID. Returns a clone —
    /// the underlying handle is itself an `Arc`-of-`Notify`, so
    /// cloning is essentially free.
    pub fn lookup(&self, sip_call_id: &str) -> Option<CallHandle> {
        self.inner.read().get(sip_call_id).map(|e| e.handle.clone())
    }

    /// Borrow the full [`CallEntry`] for a Call-ID. Used by the
    /// re-INVITE handler to read back the cached answer SDP.
    pub fn entry(&self, sip_call_id: &str) -> Option<CallEntry> {
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
        removed.map(|e| e.handle)
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

    /// BYE-driven shutdown: same as [`Self::terminate`], plus mark
    /// the handle so the post-controller cleanup knows the peer
    /// has already taken down the SIP dialog and skips sending an
    /// outbound BYE. Without this, a controller that exited because
    /// of a remote BYE would still try to BYE the peer back —
    /// harmless but redundant. The asymmetry with CANCEL is
    /// deliberate: a CANCEL is pre-2xx and the dialog never confirms,
    /// so no outbound BYE is needed there either.
    fn terminate_from_bye(&self, sip_call_id: &str) -> bool {
        match self.lookup(sip_call_id) {
            Some(handle) => {
                handle.mark_remote_bye();
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
    fn fresh_entry(bridge_call_id: &str) -> CallEntry {
        CallEntry {
            handle: fresh_handle(bridge_call_id),
            answer_text: None,
        }
    }

    fn fresh_handle(bridge_call_id: &str) -> CallHandle {
        let manager = Arc::new(MediaBridgeManager::new());
        let tap = MediaTap::attach(
            &manager,
            &::std::sync::Arc::new(forge_core::EventBus::new()),
            ForgeCallId::new(bridge_call_id),
            8000,
        )
        .expect("attach tap");
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
            transfer: None,
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
        reg.insert(
            "abc@pbx",
            CallEntry {
                handle: h.clone(),
                answer_text: None,
            },
        );
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
        reg.insert("abc@pbx", fresh_entry("siphon-1"));
        assert!(reg.lookup("never-seen@pbx").is_none());
    }

    #[test]
    fn duplicate_insert_replaces_and_warns() {
        // Same Call-ID inserted twice — second insert wins; first
        // handle is dropped. (Tracing assertions are out of scope;
        // the regression we care about is that lookup returns the
        // *new* handle.)
        let reg = CallRegistry::new();
        reg.insert("dupe@pbx", fresh_entry("siphon-old"));
        reg.insert("dupe@pbx", fresh_entry("siphon-new"));
        assert_eq!(
            reg.lookup("dupe@pbx").unwrap().call_id().as_str(),
            "siphon-new"
        );
        assert_eq!(reg.len(), 1);
    }

    #[test]
    fn snapshot_lists_all_known_call_ids() {
        let reg = CallRegistry::new();
        reg.insert("a@pbx", fresh_entry("siphon-a"));
        reg.insert("b@pbx", fresh_entry("siphon-b"));
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
        a.insert("x@pbx", fresh_entry("siphon-1"));
        assert!(b.lookup("x@pbx").is_some());
        b.remove("x@pbx");
        assert!(a.is_empty());
    }

    // The "looked-up handle wakes the same controller" property is
    // exercised end-to-end in `tests/controller_lifecycle.rs` —
    // here the structural test (`cloned_registry_shares_state`)
    // pins the shared-Arc invariant and is enough.

    #[test]
    fn terminate_from_bye_marks_handle_then_signals_shutdown() {
        // The acceptor's cleanup task reads `remote_bye_received()`
        // to decide whether it needs to send an outbound BYE.
        // Confirm the registry's BYE path flips that flag — without
        // it, every BYE-terminated call would also send a redundant
        // outbound BYE to the peer.
        let reg = CallRegistry::new();
        let handle = fresh_handle("siphon-1");
        assert!(!handle.remote_bye_received());

        reg.insert(
            "abc@pbx",
            CallEntry {
                handle: handle.clone(),
                answer_text: None,
            },
        );

        let signalled = reg.terminate_from_bye("abc@pbx");
        assert!(signalled);
        assert!(handle.remote_bye_received());
    }

    #[test]
    fn terminate_does_not_mark_remote_bye() {
        // CANCEL goes through `terminate`, not `terminate_from_bye`,
        // because CANCEL is pre-2xx and the dialog never confirmed.
        // The flag must stay false so the cleanup task sees "local
        // shutdown" semantics.
        let reg = CallRegistry::new();
        let handle = fresh_handle("siphon-2");
        reg.insert(
            "xyz@pbx",
            CallEntry {
                handle: handle.clone(),
                answer_text: None,
            },
        );

        let signalled = reg.terminate("xyz@pbx");
        assert!(signalled);
        assert!(!handle.remote_bye_received());
    }

    #[test]
    fn terminate_from_bye_unknown_returns_false() {
        let reg = CallRegistry::new();
        assert!(!reg.terminate_from_bye("never-seen@pbx"));
    }
}
