//! Daemon-wide conference-room registry (DEV_PLAN_0.7.0.md §2.1).
//!
//! Maps `room_id → RoomHandle` and enforces the `[conference]` caps.
//! The registry is the only way calls reach a room: `join` creates
//! the room on first use (locked to the first joiner's sample rate)
//! and hands back the [`RoomMembership`] the call's tap re-plumbs
//! with (`TapCommand::JoinRoom`).
//!
//! CLAUDE.md §4.4 stance: like `CallRegistry` / `ConsultRegistry`,
//! this stores channel-bearing handles under exact ids — no
//! enumeration of call internals, no reach into another call's
//! state. A room is an explicit rendezvous point a call opts into.
//!
//! Rooms remove *themselves* (the room task exits when its last
//! member leaves); the registry holds a stale closed handle until
//! the next `join` for that id prunes and replaces it. `max_rooms`
//! is enforced against live (non-closed) rooms only.

use std::collections::HashMap;
use std::sync::Arc;

use chrono::Utc;
use parking_lot::RwLock;
use thiserror::Error;
use tracing::debug;

use siphon_ai_media_glue::{
    spawn_room, RoomConfig, RoomHandle, RoomJoinError, RoomLifecycle, RoomMembership, RoomObserver,
};
use siphon_ai_webhooks::{
    ConferenceCreatedEvent, ConferenceEndedEvent, WebhookEvent, WebhookSinkHandle, WEBHOOK_VERSION,
};

/// Metric name; the literal must match the const in
/// `siphon-ai-telemetry::metrics` (same pattern as the tap metrics).
const METRIC_JOINS_TOTAL: &str = "siphon_ai_conference_joins_total";

/// Compiled `[conference]` knobs the registry enforces. The daemon
/// maps `siphon-ai-config`'s `ConferenceConfig` onto this 1:1 (core
/// deliberately doesn't depend on the config crate — same pattern
/// as the outbound guardrails).
#[derive(Debug, Clone)]
pub struct ConferenceLimits {
    /// `[conference].enabled` — fail-closed: every join is refused
    /// while false.
    pub enabled: bool,
    /// `[conference].max_rooms` — live rooms across the daemon.
    pub max_rooms: usize,
    /// `[conference].max_participants_per_room` — member *calls*
    /// per room (each call contributes 2 mixer participants).
    pub max_participants_per_room: usize,
    /// `[conference].join_tones` — chime on join/leave.
    pub join_tones: bool,
}

impl Default for ConferenceLimits {
    /// Mirrors the config defaults: disabled, 16 rooms, 8 calls per
    /// room, no tones.
    fn default() -> Self {
        Self {
            enabled: false,
            max_rooms: 16,
            max_participants_per_room: 8,
            join_tones: false,
        }
    }
}

/// Why a conference join failed. Chunk 2 maps these onto the WS
/// protocol's `error { code: "conference_failed" }`.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ConferenceError {
    #[error("conferencing is disabled ([conference].enabled = false)")]
    Disabled,

    #[error("conference room limit reached ({max_rooms})")]
    TooManyRooms { max_rooms: usize },

    #[error("a conference room with that id already exists")]
    RoomExists,

    #[error(transparent)]
    Join(#[from] RoomJoinError),
}

impl ConferenceError {
    /// Bounded label for `siphon_ai_conference_joins_total{result=…}`.
    fn metric_result(&self) -> &'static str {
        match self {
            ConferenceError::Disabled => "disabled",
            ConferenceError::TooManyRooms { .. } => "too_many_rooms",
            ConferenceError::RoomExists => "room_exists",
            ConferenceError::Join(RoomJoinError::RoomFull { .. }) => "room_full",
            ConferenceError::Join(RoomJoinError::SampleRateMismatch { .. }) => "rate_mismatch",
            ConferenceError::Join(RoomJoinError::AlreadyJoined) => "already_joined",
            ConferenceError::Join(RoomJoinError::RoomClosed) => "error",
        }
    }
}

/// A live conference room and its members — the
/// `GET /admin/v1/conferences` view (DEV_PLAN_0.7.0.md §2.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConferenceSnapshot {
    pub room_id: String,
    pub sample_rate: u32,
    /// Member call-ids (bridge ids), sorted.
    pub participants: Vec<String>,
}

/// Process-wide room table. Cheap to clone (`Arc` inside).
#[derive(Clone)]
pub struct ConferenceRegistry {
    limits: ConferenceLimits,
    inner: Arc<RwLock<HashMap<String, RoomHandle>>>,
    /// When set, the registry fires `conference_created` /
    /// `conference_ended` webhooks via a per-room observer. `None` in
    /// tests / when webhooks aren't configured.
    webhook_sink: Option<WebhookSinkHandle>,
}

impl std::fmt::Debug for ConferenceRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `WebhookSinkHandle` is a trait object without Debug; redact it.
        f.debug_struct("ConferenceRegistry")
            .field("limits", &self.limits)
            .field("live_rooms", &self.live_rooms())
            .field("webhooks", &self.webhook_sink.is_some())
            .finish()
    }
}

impl ConferenceRegistry {
    pub fn new(limits: ConferenceLimits) -> Self {
        Self {
            limits,
            inner: Arc::new(RwLock::new(HashMap::new())),
            webhook_sink: None,
        }
    }

    /// Emit `conference_created` / `conference_ended` webhooks for the
    /// rooms this registry spawns.
    pub fn with_webhooks(mut self, sink: WebhookSinkHandle) -> Self {
        self.webhook_sink = Some(sink);
        self
    }

    pub fn limits(&self) -> &ConferenceLimits {
        &self.limits
    }

    /// Build the per-room lifecycle observer that fires the
    /// conference webhooks. `None` when no sink is configured (so
    /// `spawn_room` skips the callback entirely).
    fn room_observer(&self) -> Option<RoomObserver> {
        let sink = self.webhook_sink.clone()?;
        Some(Arc::new(move |ev: RoomLifecycle| {
            // The observer runs on the room task; the webhook send is
            // async + best-effort, so spawn it rather than block.
            let sink = sink.clone();
            let event = match ev {
                RoomLifecycle::Created {
                    room_id,
                    sample_rate,
                } => WebhookEvent::ConferenceCreated(ConferenceCreatedEvent {
                    version: WEBHOOK_VERSION,
                    room_id,
                    sample_rate,
                    timestamp: Utc::now(),
                }),
                RoomLifecycle::Ended {
                    room_id,
                    duration_ms,
                    peak_participants,
                } => WebhookEvent::ConferenceEnded(ConferenceEndedEvent {
                    version: WEBHOOK_VERSION,
                    room_id,
                    timestamp: Utc::now(),
                    duration_ms,
                    peak_participants,
                }),
            };
            tokio::spawn(async move { sink.emit(event).await });
        }))
    }

    /// Snapshot of every live room and its members — the admin list
    /// view. Off the audio path; reads each room's shared member list.
    pub fn snapshot(&self) -> Vec<ConferenceSnapshot> {
        let mut rooms: Vec<ConferenceSnapshot> = self
            .inner
            .read()
            .values()
            .filter(|h| !h.is_closed())
            .map(|h| ConferenceSnapshot {
                room_id: h.room_id().to_string(),
                sample_rate: h.sample_rate(),
                participants: h.participants(),
            })
            .collect();
        rooms.sort_by(|a, b| a.room_id.cmp(&b.room_id));
        rooms
    }

    /// Pre-create an empty room at `sample_rate` (operator
    /// `POST /admin/v1/conferences`). Errors if conferencing is off, a
    /// live room with that id already exists, or the room cap is
    /// reached. The room survives empty until force-ended or its
    /// first-and-last member leaves.
    pub fn create_room(&self, room_id: &str, sample_rate: u32) -> Result<(), ConferenceError> {
        if !self.limits.enabled {
            return Err(ConferenceError::Disabled);
        }
        if self
            .inner
            .read()
            .get(room_id)
            .is_some_and(|h| !h.is_closed())
        {
            return Err(ConferenceError::RoomExists);
        }
        // `live_handle_or_create` does the cap check + prune under the
        // write lock and spawns the room with the webhook observer.
        self.live_handle_or_create(room_id, sample_rate).map(|_| ())
    }

    /// Force-end a room (operator `DELETE /admin/v1/conferences/:id`).
    /// Returns `false` when no live room with that id exists.
    pub fn end_room(&self, room_id: &str) -> bool {
        let mut guard = self.inner.write();
        match guard.get(room_id) {
            Some(h) if !h.is_closed() => {
                h.end();
                guard.remove(room_id);
                true
            }
            _ => false,
        }
    }

    /// Join `call_id` (negotiated at `sample_rate`) to `room_id`,
    /// creating the room if absent — subject to the `[conference]`
    /// caps. On success the membership is handed to the call's tap
    /// via `TapCommand::JoinRoom`.
    pub async fn join(
        &self,
        room_id: &str,
        call_id: &str,
        sample_rate: u32,
    ) -> Result<RoomMembership, ConferenceError> {
        let result = self.join_inner(room_id, call_id, sample_rate).await;
        let label = match &result {
            Ok(_) => "joined",
            Err(e) => e.metric_result(),
        };
        metrics::counter!(METRIC_JOINS_TOTAL, "result" => label).increment(1);
        result
    }

    async fn join_inner(
        &self,
        room_id: &str,
        call_id: &str,
        sample_rate: u32,
    ) -> Result<RoomMembership, ConferenceError> {
        if !self.limits.enabled {
            return Err(ConferenceError::Disabled);
        }
        // Two attempts: the room found on the first pass may have
        // exited (last member left) between lookup and join — prune
        // it and create a fresh one. A second RoomClosed means
        // something is genuinely wrong; surface it.
        for attempt in 0..2 {
            let handle = self.live_handle_or_create(room_id, sample_rate)?;
            match handle.join(call_id, sample_rate).await {
                Ok(membership) => return Ok(membership),
                Err(RoomJoinError::RoomClosed) if attempt == 0 => {
                    debug!(
                        room_id,
                        "room closed between lookup and join; retrying fresh"
                    );
                    self.prune_if_closed(room_id);
                }
                Err(e) => return Err(e.into()),
            }
        }
        Err(ConferenceError::Join(RoomJoinError::RoomClosed))
    }

    /// Remove `call_id` from `room_id`. Best-effort and idempotent —
    /// the room's own reap paths are the backstop.
    pub fn leave(&self, room_id: &str, call_id: &str) {
        if let Some(handle) = self.inner.read().get(room_id) {
            handle.leave(call_id);
        }
    }

    /// Live (non-closed) rooms. The cap check and tests use this;
    /// stale closed handles don't count.
    pub fn live_rooms(&self) -> usize {
        self.inner
            .read()
            .values()
            .filter(|h| !h.is_closed())
            .count()
    }

    /// Get a live handle for `room_id`, spawning the room (locked
    /// to `sample_rate`) if absent or dead. Stale entries are pruned
    /// inside the same write lock so the cap counts live rooms only.
    fn live_handle_or_create(
        &self,
        room_id: &str,
        sample_rate: u32,
    ) -> Result<RoomHandle, ConferenceError> {
        // Fast path: live room already exists.
        if let Some(h) = self.inner.read().get(room_id) {
            if !h.is_closed() {
                return Ok(h.clone());
            }
        }
        let mut guard = self.inner.write();
        // Re-check under the write lock (a concurrent join may have
        // just created it).
        if let Some(h) = guard.get(room_id) {
            if !h.is_closed() {
                return Ok(h.clone());
            }
        }
        guard.retain(|_, h| !h.is_closed());
        if guard.len() >= self.limits.max_rooms {
            return Err(ConferenceError::TooManyRooms {
                max_rooms: self.limits.max_rooms,
            });
        }
        let handle = spawn_room(
            RoomConfig {
                room_id: room_id.to_string(),
                sample_rate,
                max_calls: self.limits.max_participants_per_room,
                join_tones: self.limits.join_tones,
            },
            self.room_observer(),
        );
        guard.insert(room_id.to_string(), handle.clone());
        Ok(handle)
    }

    fn prune_if_closed(&self, room_id: &str) {
        let mut guard = self.inner.write();
        if guard.get(room_id).is_some_and(|h| h.is_closed()) {
            guard.remove(room_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::time::{timeout, Duration};

    const WAIT: Duration = Duration::from_secs(2);

    fn enabled(max_rooms: usize, max_calls: usize) -> ConferenceLimits {
        ConferenceLimits {
            enabled: true,
            max_rooms,
            max_participants_per_room: max_calls,
            join_tones: false,
        }
    }

    #[tokio::test]
    async fn disabled_refuses_every_join() {
        let reg = ConferenceRegistry::new(ConferenceLimits::default());
        let err = reg.join("r", "call-a", 8000).await.unwrap_err();
        assert_eq!(err, ConferenceError::Disabled);
        assert_eq!(reg.live_rooms(), 0);
    }

    #[tokio::test]
    async fn join_creates_room_and_second_call_shares_it() {
        let reg = ConferenceRegistry::new(enabled(4, 8));
        let _a = reg.join("support-7", "call-a", 8000).await.expect("a");
        let _b = reg.join("support-7", "call-b", 8000).await.expect("b");
        assert_eq!(reg.live_rooms(), 1);
    }

    #[tokio::test]
    async fn max_rooms_is_enforced_against_live_rooms() {
        let reg = ConferenceRegistry::new(enabled(2, 8));
        let _a = reg.join("r1", "call-a", 8000).await.expect("a");
        let _b = reg.join("r2", "call-b", 8000).await.expect("b");
        let err = reg.join("r3", "call-c", 8000).await.unwrap_err();
        assert_eq!(err, ConferenceError::TooManyRooms { max_rooms: 2 });
    }

    #[tokio::test]
    async fn room_caps_and_rate_mismatch_bubble_up() {
        let reg = ConferenceRegistry::new(enabled(4, 1));
        let _a = reg.join("r", "call-a", 8000).await.expect("a");
        let full = reg.join("r", "call-b", 8000).await.unwrap_err();
        assert_eq!(
            full,
            ConferenceError::Join(RoomJoinError::RoomFull { max_calls: 1 })
        );

        let reg2 = ConferenceRegistry::new(enabled(4, 8));
        let _a = reg2.join("r", "call-a", 8000).await.expect("a");
        let mismatch = reg2.join("r", "call-b", 16000).await.unwrap_err();
        assert_eq!(
            mismatch,
            ConferenceError::Join(RoomJoinError::SampleRateMismatch {
                room_rate: 8000,
                call_rate: 16000
            })
        );
    }

    #[tokio::test]
    async fn dead_room_is_replaced_on_next_join_for_same_id() {
        let reg = ConferenceRegistry::new(enabled(2, 8));
        let a = reg.join("r", "call-a", 8000).await.expect("a");

        // Last member leaves → room task exits.
        drop(a);
        timeout(WAIT, async {
            while reg.live_rooms() != 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("room dies after last leave");

        // Same id joins again → fresh room, even at a different
        // rate (the lock-to-first-joiner restarts with the room).
        let _b = reg.join("r", "call-b", 16000).await.expect("fresh room");
        assert_eq!(reg.live_rooms(), 1);
    }

    #[tokio::test]
    async fn stale_rooms_do_not_count_against_max_rooms() {
        let reg = ConferenceRegistry::new(enabled(1, 8));
        let a = reg.join("r1", "call-a", 8000).await.expect("a");
        drop(a);
        timeout(WAIT, async {
            while reg.live_rooms() != 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("r1 dies");

        // r1's stale entry must not block creating r2 at cap 1.
        let _b = reg.join("r2", "call-b", 8000).await.expect("b");
        assert_eq!(reg.live_rooms(), 1);
    }

    #[tokio::test]
    async fn leave_is_idempotent_and_unknown_room_is_a_no_op() {
        let reg = ConferenceRegistry::new(enabled(4, 8));
        reg.leave("never-created", "call-a");
        let _a = reg.join("r", "call-a", 8000).await.expect("a");
        reg.leave("r", "call-a");
        reg.leave("r", "call-a");
    }
}
