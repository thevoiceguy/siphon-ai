//! `RtpStatsTracker` — caches the most-recent RTP-quality assessment
//! reported by forge and exposes it as a snapshot for periodic
//! emission of `rtp_stats` WS events.
//!
//! ## Why a tracker, not a poller
//!
//! forge-engine does not (in 0.2.0) expose a session-level
//! `rtp_stats_snapshot()` API. What it does emit is the
//! [`forge_core::ForgeEvent::QualityDegraded`] / `QualityRestored`
//! events whenever its quality assessment changes. We subscribe to
//! those, cache the last-known values, and emit periodic snapshots
//! at `[bridge].rtp_stats_interval_ms` cadence using the cache.
//!
//! This means the emitted values reflect forge's most-recent state,
//! not the literal current measurement. For a healthy call that
//! never degraded, both fields are `None` (no data yet). After a
//! `QualityDegraded` arrives, both are `Some(value)`. After a
//! `QualityRestored`, both are `Some(0.0)` — explicitly "back to
//! healthy" rather than ambiguous `None`. A future forge upstream
//! PR exposing a snapshot accessor would let us replace the tracker
//! with a poller and get true periodic measurements; the WS event
//! shape stays the same either way.

use std::time::Duration;

/// What [`RtpStatsTracker::snapshot`] returns. Used by the tap's
/// periodic-emit arm to populate the wire event.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RtpStatsSnapshot {
    pub jitter_ms: Option<f32>,
    pub packet_loss_ratio: Option<f32>,
}

/// Per-call RTP-stats state. Owned by the tap; updated from forge
/// event arms; polled by the periodic-emit arm.
#[derive(Debug, Clone, Copy)]
pub struct RtpStatsTracker {
    interval: Option<Duration>,
    last_jitter_ms: Option<f32>,
    last_packet_loss_ratio: Option<f32>,
}

impl RtpStatsTracker {
    /// `interval = None` disables periodic emission entirely.
    pub fn new(interval: Option<Duration>) -> Self {
        Self {
            interval,
            last_jitter_ms: None,
            last_packet_loss_ratio: None,
        }
    }

    /// True when periodic emission is enabled.
    pub fn is_active(&self) -> bool {
        self.interval.is_some()
    }

    /// The configured emission interval, or `None` if disabled.
    pub fn interval(&self) -> Option<Duration> {
        self.interval
    }

    /// forge reported a `QualityDegraded` — cache the values.
    /// `packet_loss_percent` is in [0.0, 100.0] (forge's convention);
    /// we convert to ratio [0.0, 1.0] for the wire event.
    pub fn note_quality_degraded(&mut self, packet_loss_percent: f32, jitter_ms: f32) {
        self.last_jitter_ms = Some(jitter_ms);
        self.last_packet_loss_ratio = Some(packet_loss_percent / 100.0);
    }

    /// forge reported a `QualityRestored` — mark both fields as
    /// "explicitly healthy" (`Some(0.0)`), not `None`. Consumers
    /// distinguish "no data yet" (`None`) from "healthy" (`0.0`).
    pub fn note_quality_restored(&mut self) {
        self.last_jitter_ms = Some(0.0);
        self.last_packet_loss_ratio = Some(0.0);
    }

    /// Current snapshot for the periodic-emit arm.
    pub fn snapshot(&self) -> RtpStatsSnapshot {
        RtpStatsSnapshot {
            jitter_ms: self.last_jitter_ms,
            packet_loss_ratio: self.last_packet_loss_ratio,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_tracker_has_no_data() {
        let t = RtpStatsTracker::new(Some(Duration::from_secs(5)));
        let snap = t.snapshot();
        assert!(snap.jitter_ms.is_none());
        assert!(snap.packet_loss_ratio.is_none());
    }

    #[test]
    fn disabled_when_interval_is_none() {
        let t = RtpStatsTracker::new(None);
        assert!(!t.is_active());
        assert!(t.interval().is_none());
    }

    #[test]
    fn active_when_interval_is_set() {
        let t = RtpStatsTracker::new(Some(Duration::from_millis(5000)));
        assert!(t.is_active());
        assert_eq!(t.interval(), Some(Duration::from_millis(5000)));
    }

    #[test]
    fn degraded_converts_percent_to_ratio() {
        let mut t = RtpStatsTracker::new(Some(Duration::from_secs(5)));
        // forge emits packet_loss_percent in [0, 100]; the wire
        // event expects packet_loss_ratio in [0.0, 1.0].
        t.note_quality_degraded(2.5 /* 2.5% loss */, 17.2 /* ms */);
        let snap = t.snapshot();
        assert_eq!(snap.jitter_ms, Some(17.2));
        assert!((snap.packet_loss_ratio.unwrap() - 0.025).abs() < 1e-6);
    }

    #[test]
    fn restored_sets_explicit_zero() {
        // Distinct from `None`: "we know quality is good" vs "we
        // have no data yet". The WS server can show "—" vs "0%".
        let mut t = RtpStatsTracker::new(Some(Duration::from_secs(5)));
        t.note_quality_restored();
        let snap = t.snapshot();
        assert_eq!(snap.jitter_ms, Some(0.0));
        assert_eq!(snap.packet_loss_ratio, Some(0.0));
    }

    #[test]
    fn restored_after_degraded_overwrites() {
        let mut t = RtpStatsTracker::new(Some(Duration::from_secs(5)));
        t.note_quality_degraded(10.0, 50.0);
        t.note_quality_restored();
        let snap = t.snapshot();
        assert_eq!(snap.jitter_ms, Some(0.0));
        assert_eq!(snap.packet_loss_ratio, Some(0.0));
    }

    #[test]
    fn snapshot_is_pure_view() {
        // Calling snapshot() multiple times doesn't mutate state.
        let mut t = RtpStatsTracker::new(Some(Duration::from_secs(5)));
        t.note_quality_degraded(1.0, 10.0);
        let a = t.snapshot();
        let b = t.snapshot();
        assert_eq!(a, b);
    }
}
