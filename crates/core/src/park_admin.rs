//! Admin park CRUD — the core-side impl of [`ParkAdminHandle`]
//! (DEV_PLAN_0.7.0.md §2.4).
//!
//! Mirrors [`ConferenceAdmin`](crate::ConferenceAdmin): it ties the
//! daemon-wide [`ParkRegistry`] (the parked-call list + cap) to the
//! [`CallControlRegistry`] (resolve any active call by bridge id and
//! signal it). `park` / `retrieve` are dispatch-and-return (202): the
//! admin signals the call's [`CallHandle`](crate::CallHandle) and the
//! call's own controller does the work (§4.4) — the outcome surfaces on
//! the call's WS + the `call_parked` / `call_retrieved` webhooks.

use siphon_ai_telemetry::{ParkAdminError, ParkAdminHandle, ParkedRow};

use crate::park::ParkRegistry;
use crate::registry::CallControlRegistry;

/// Wires the park registry + the bridge-id call-control registry into
/// the admin trait. Cheap to clone (both inner registries are
/// `Arc`-backed).
#[derive(Clone)]
pub struct ParkAdmin {
    park: ParkRegistry,
    control: CallControlRegistry,
}

impl ParkAdmin {
    pub fn new(park: ParkRegistry, control: CallControlRegistry) -> Self {
        Self { park, control }
    }
}

impl ParkAdminHandle for ParkAdmin {
    fn list(&self) -> Vec<ParkedRow> {
        self.park
            .snapshot()
            .into_iter()
            .map(|s| ParkedRow {
                call_id: s.call_id,
                slot: s.slot,
                parked_secs: s.parked_secs,
            })
            .collect()
    }

    fn park(&self, call_id: &str, slot: Option<String>) -> Result<(), ParkAdminError> {
        // Resolve the target call and signal it to park. The cap is
        // enforced inside the controller's park path (ParkRegistry::
        // try_park); a refusal surfaces on the call's WS, not here. We
        // only confirm the call exists.
        match self.control.lookup(call_id) {
            Some(handle) => {
                handle.request_park(slot);
                Ok(())
            }
            None => Err(ParkAdminError::UnknownCall(call_id.to_string())),
        }
    }

    fn retrieve(&self, call_id: &str, ws_url: Option<String>) -> Result<(), ParkAdminError> {
        // Resolve the call; reject if it isn't actually parked (a
        // retrieve on a live call is operator error worth a 409, not a
        // silent no-op).
        let Some(handle) = self.control.lookup(call_id) else {
            return Err(ParkAdminError::UnknownCall(call_id.to_string()));
        };
        if !self.park.is_parked(call_id) {
            return Err(ParkAdminError::NotParked(call_id.to_string()));
        }
        handle.request_retrieve(ws_url);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn admin(max_parked: usize) -> ParkAdmin {
        ParkAdmin::new(ParkRegistry::new(max_parked), CallControlRegistry::new())
    }

    #[test]
    fn park_unknown_call_is_not_found() {
        let a = admin(8);
        assert_eq!(
            a.park("ghost", None).unwrap_err(),
            ParkAdminError::UnknownCall("ghost".into())
        );
    }

    #[test]
    fn retrieve_unknown_call_is_not_found() {
        let a = admin(8);
        assert_eq!(
            a.retrieve("ghost", None).unwrap_err(),
            ParkAdminError::UnknownCall("ghost".into())
        );
    }

    #[test]
    fn list_reflects_registry() {
        let park = ParkRegistry::new(8);
        park.try_park("siphon-a", Some("lot-1".into())).unwrap();
        let a = ParkAdmin::new(park, CallControlRegistry::new());
        let rows = a.list();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].call_id, "siphon-a");
        assert_eq!(rows[0].slot.as_deref(), Some("lot-1"));
    }
}
