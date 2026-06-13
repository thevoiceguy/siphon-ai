//! Admin conference CRUD — the core-side impl of
//! [`ConferenceAdminHandle`] (DEV_PLAN_0.7.0.md §2.3).
//!
//! Ties the two daemon-wide structures the operator surface needs:
//! - [`ConferenceRegistry`] — room lifecycle (list / pre-create / end),
//! - [`CallControlRegistry`] — resolve any active call by its bridge
//!   `call_id` and *signal* it to join/leave a room.
//!
//! CLAUDE.md §4.4 holds end-to-end: cross-call add/remove never reaches
//! into another call's state — it pushes a [`ConferenceCommand`] onto
//! the target's [`CallHandle`](crate::CallHandle), and that call's own
//! controller runs the same join/leave path a WS `conference_join`
//! would. So `add`/`remove` are dispatch-and-return (202): the actual
//! join outcome surfaces on the target call's WS
//! (`conference_joined` / `error`), not in the admin HTTP response.

use siphon_ai_telemetry::{
    ConferenceAdminError, ConferenceAdminHandle, ConferenceRow, CreateConferenceRequest,
};

use crate::conference::ConferenceRegistry;
use crate::registry::CallControlRegistry;

/// Sample rates a room may lock to (v1: 8 kHz / 16 kHz; no resampling).
const SUPPORTED_RATES: [u32; 2] = [8000, 16000];
/// Default rate for an admin pre-create that omits `sample_rate` — the
/// most common PSTN rate.
const DEFAULT_RATE: u32 = 8000;

/// Wires the conference registry + the bridge-id call-control registry
/// into the admin trait. Cheap to clone (both inner registries are
/// `Arc`-backed).
#[derive(Clone)]
pub struct ConferenceAdmin {
    conference: ConferenceRegistry,
    control: CallControlRegistry,
    /// Monotonic-ish suffix for daemon-generated room ids on a
    /// `POST /conferences` with no `room_id`. Not security-sensitive;
    /// collisions are caught by `create_room`'s exists check.
    next_id: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl ConferenceAdmin {
    pub fn new(conference: ConferenceRegistry, control: CallControlRegistry) -> Self {
        Self {
            conference,
            control,
            next_id: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(1)),
        }
    }
}

impl ConferenceAdminHandle for ConferenceAdmin {
    fn list(&self) -> Vec<ConferenceRow> {
        self.conference
            .snapshot()
            .into_iter()
            .map(|s| ConferenceRow {
                room_id: s.room_id,
                sample_rate: s.sample_rate,
                participants: s.participants,
            })
            .collect()
    }

    fn create(&self, req: CreateConferenceRequest) -> Result<String, ConferenceAdminError> {
        let sample_rate = req.sample_rate.unwrap_or(DEFAULT_RATE);
        if !SUPPORTED_RATES.contains(&sample_rate) {
            return Err(ConferenceAdminError::BadRequest(format!(
                "sample_rate must be one of {SUPPORTED_RATES:?}, got {sample_rate}"
            )));
        }
        let room_id = match req.room_id.filter(|s| !s.is_empty()) {
            Some(id) => id,
            None => {
                let n = self
                    .next_id
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                format!("room-{n}")
            }
        };
        self.conference
            .create_room(&room_id, sample_rate)
            .map(|()| room_id)
            .map_err(map_conf_err)
    }

    fn end(&self, room_id: &str) -> Result<(), ConferenceAdminError> {
        if self.conference.end_room(room_id) {
            Ok(())
        } else {
            Err(ConferenceAdminError::RoomNotFound)
        }
    }

    fn add_participant(&self, room_id: &str, call_id: &str) -> Result<(), ConferenceAdminError> {
        if !self.conference.limits().enabled {
            return Err(ConferenceAdminError::Disabled);
        }
        // Resolve the target call and signal it to join. The join's
        // success/failure (cap, rate mismatch, room gone) surfaces on
        // that call's own WS — we only confirm the call exists.
        match self.control.lookup(call_id) {
            Some(handle) => {
                handle.request_conference_join(room_id);
                Ok(())
            }
            None => Err(ConferenceAdminError::UnknownCall(call_id.to_string())),
        }
    }

    fn remove_participant(
        &self,
        _room_id: &str,
        call_id: &str,
    ) -> Result<(), ConferenceAdminError> {
        // Self-scoped leave on the target call — it leaves whatever
        // room it's in (the controller's LeaveRoom is a no-op if it
        // isn't in one). We don't cross-check it's in `room_id`: the
        // operator's intent ("get this call out") is unambiguous.
        match self.control.lookup(call_id) {
            Some(handle) => {
                handle.request_conference_leave();
                Ok(())
            }
            None => Err(ConferenceAdminError::UnknownCall(call_id.to_string())),
        }
    }
}

/// Map the registry's create errors onto the admin trait's errors.
fn map_conf_err(e: crate::conference::ConferenceError) -> ConferenceAdminError {
    use crate::conference::ConferenceError as E;
    match e {
        E::Disabled => ConferenceAdminError::Disabled,
        E::RoomExists => ConferenceAdminError::RoomExists,
        E::TooManyRooms { .. } => ConferenceAdminError::TooManyRooms,
        // create_room only returns the above; a Join error here would
        // be a logic bug — surface it as a 400 rather than panic.
        E::Join(j) => ConferenceAdminError::BadRequest(j.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conference::ConferenceLimits;

    fn admin(enabled: bool, max_rooms: usize) -> ConferenceAdmin {
        let conf = ConferenceRegistry::new(ConferenceLimits {
            enabled,
            max_rooms,
            max_participants_per_room: 8,
            join_tones: false,
        });
        ConferenceAdmin::new(conf, CallControlRegistry::new())
    }

    #[tokio::test]
    async fn create_generates_id_and_lists_it() {
        let a = admin(true, 8);
        let id = a
            .create(CreateConferenceRequest {
                room_id: None,
                sample_rate: None,
            })
            .expect("created");
        let rooms = a.list();
        assert_eq!(rooms.len(), 1);
        assert_eq!(rooms[0].room_id, id);
        assert_eq!(rooms[0].sample_rate, 8000);
        assert!(rooms[0].participants.is_empty());
    }

    #[tokio::test]
    async fn create_rejects_bad_rate_and_duplicate() {
        let a = admin(true, 8);
        let bad = a
            .create(CreateConferenceRequest {
                room_id: Some("r".into()),
                sample_rate: Some(44100),
            })
            .unwrap_err();
        assert!(matches!(bad, ConferenceAdminError::BadRequest(_)));

        a.create(CreateConferenceRequest {
            room_id: Some("r".into()),
            sample_rate: None,
        })
        .expect("first create");
        let dup = a
            .create(CreateConferenceRequest {
                room_id: Some("r".into()),
                sample_rate: None,
            })
            .unwrap_err();
        assert_eq!(dup, ConferenceAdminError::RoomExists);
    }

    #[tokio::test]
    async fn create_disabled_is_501_mapped() {
        let a = admin(false, 8);
        let err = a
            .create(CreateConferenceRequest {
                room_id: Some("r".into()),
                sample_rate: None,
            })
            .unwrap_err();
        assert_eq!(err, ConferenceAdminError::Disabled);
    }

    #[tokio::test]
    async fn end_unknown_room_is_not_found() {
        let a = admin(true, 8);
        assert_eq!(
            a.end("nope").unwrap_err(),
            ConferenceAdminError::RoomNotFound
        );
    }

    #[tokio::test]
    async fn add_remove_unknown_call_is_not_found() {
        let a = admin(true, 8);
        let add = a.add_participant("r", "ghost").unwrap_err();
        assert_eq!(add, ConferenceAdminError::UnknownCall("ghost".into()));
        let rm = a.remove_participant("r", "ghost").unwrap_err();
        assert_eq!(rm, ConferenceAdminError::UnknownCall("ghost".into()));
    }

    #[tokio::test]
    async fn add_disabled_is_disabled() {
        let a = admin(false, 8);
        assert_eq!(
            a.add_participant("r", "c").unwrap_err(),
            ConferenceAdminError::Disabled
        );
    }
}
