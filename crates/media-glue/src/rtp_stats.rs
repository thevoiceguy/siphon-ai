//! `RtpStatsTracker` — caches the most-recent RTP-quality assessment
//! reported by forge and exposes it as a snapshot for periodic
//! emission of `rtp_stats` WS events.
//!
//! ## Wiring
//!
//! forge-engine emits a [`forge_core::ForgeEvent::RtcpReportReceived`]
//! event on every incoming RTCP RR block, carrying `jitter_ms`,
//! `packet_loss_ratio`, and (when forge is also originating its own
//! SRs) `rtt_ms`. The tap subscribes, caches the last-known values
//! via [`RtpStatsTracker::note_rtcp_report`], and emits periodic
//! snapshots at `[bridge].rtp_stats_interval_ms` cadence using the
//! cache.
//!
//! The legacy [`forge_core::ForgeEvent::QualityDegraded`] /
//! `QualityRestored` events were the 0.2.0 plan but never had a
//! producer in forge — see siphon-ai DEV_PLAN_0.3.0.md §9 decision
//! 8 for the resolution. The threshold-based `QualityDegraded` arm
//! is kept for forward-compatibility but is currently a no-op in
//! production because nothing emits it.
//!
//! For a healthy call that never received an RR (e.g., short call),
//! all snapshot fields are `None` (no data yet). Once at least one
//! RR has arrived, they are `Some(value)`. `rtt_ms` may stay `None`
//! even when jitter/loss are populated — RTT requires forge to
//! originate its own SRs (deferred to 0.3.1 per §9 decision 10).

use std::time::Duration;

/// What [`RtpStatsTracker::snapshot`] returns. Used by the tap's
/// periodic-emit arm to populate the wire event.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RtpStatsSnapshot {
    pub jitter_ms: Option<f32>,
    pub packet_loss_ratio: Option<f32>,
    /// Mean round-trip time over the reporting window in milliseconds.
    /// `None` until forge-engine originates its own RTCP SRs (deferred
    /// to 0.3.1 per DEV_PLAN_0.3.0.md §9 decision 10) — distinct from
    /// `Some(0.0)`, which is degenerate.
    pub rtt_ms: Option<f32>,
}

/// Per-call RTP-stats state. Owned by the tap; updated from forge
/// event arms; polled by the periodic-emit arm.
#[derive(Debug, Clone, Copy)]
pub struct RtpStatsTracker {
    interval: Option<Duration>,
    last_jitter_ms: Option<f32>,
    last_packet_loss_ratio: Option<f32>,
    last_rtt_ms: Option<f32>,
}

impl RtpStatsTracker {
    /// `interval = None` disables periodic emission entirely.
    pub fn new(interval: Option<Duration>) -> Self {
        Self {
            interval,
            last_jitter_ms: None,
            last_packet_loss_ratio: None,
            last_rtt_ms: None,
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

    /// forge reported a `RtcpReportReceived` — cache the three RR-derived
    /// fields. `rtt_ms = None` is preserved as-is (we don't want the
    /// snapshot's `rtt_ms` to flip to `Some(0.0)` when forge can't yet
    /// compute RTT).
    pub fn note_rtcp_report(
        &mut self,
        jitter_ms: f32,
        packet_loss_ratio: f32,
        rtt_ms: Option<f32>,
    ) {
        self.last_jitter_ms = Some(jitter_ms);
        self.last_packet_loss_ratio = Some(packet_loss_ratio);
        if rtt_ms.is_some() {
            self.last_rtt_ms = rtt_ms;
        }
    }

    /// forge reported a `QualityDegraded` — cache the values.
    /// `packet_loss_percent` is in [0.0, 100.0] (forge's convention);
    /// we convert to ratio [0.0, 1.0] for the wire event.
    ///
    /// Kept for backwards compatibility with the threshold-based event.
    /// `RtcpReportReceived` is the per-RR cadence event the
    /// 0.3.0 producer emits; this method handles the (currently unused)
    /// threshold path.
    pub fn note_quality_degraded(&mut self, packet_loss_percent: f32, jitter_ms: f32) {
        self.last_jitter_ms = Some(jitter_ms);
        self.last_packet_loss_ratio = Some(packet_loss_percent / 100.0);
    }

    /// forge reported a `QualityRestored` — mark jitter and loss as
    /// "explicitly healthy" (`Some(0.0)`), not `None`. `rtt_ms` is
    /// left untouched: it's an absolute measurement, not a threshold,
    /// so "healthy" doesn't imply zero RTT.
    pub fn note_quality_restored(&mut self) {
        self.last_jitter_ms = Some(0.0);
        self.last_packet_loss_ratio = Some(0.0);
    }

    /// Current snapshot for the periodic-emit arm.
    pub fn snapshot(&self) -> RtpStatsSnapshot {
        RtpStatsSnapshot {
            jitter_ms: self.last_jitter_ms,
            packet_loss_ratio: self.last_packet_loss_ratio,
            rtt_ms: self.last_rtt_ms,
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
        assert!(snap.rtt_ms.is_none());
    }

    #[test]
    fn rtcp_report_populates_all_three_fields() {
        let mut t = RtpStatsTracker::new(Some(Duration::from_secs(5)));
        t.note_rtcp_report(
            17.2,  /* jitter ms */
            0.025, /* 2.5% loss */
            Some(42.0),
        );
        let snap = t.snapshot();
        assert_eq!(snap.jitter_ms, Some(17.2));
        assert!((snap.packet_loss_ratio.unwrap() - 0.025).abs() < 1e-6);
        assert_eq!(snap.rtt_ms, Some(42.0));
    }

    #[test]
    fn rtcp_report_with_none_rtt_keeps_prior_rtt() {
        // Once a real RTT measurement lands, later RRs with rtt=None
        // (e.g., a window with no matching SR) shouldn't wipe it.
        // This matches the §A.7 reality: RTT is sparse.
        let mut t = RtpStatsTracker::new(Some(Duration::from_secs(5)));
        t.note_rtcp_report(10.0, 0.01, Some(35.0));
        t.note_rtcp_report(11.0, 0.02, None);
        let snap = t.snapshot();
        assert_eq!(snap.rtt_ms, Some(35.0));
        // …but jitter and loss DO update on every RR.
        assert_eq!(snap.jitter_ms, Some(11.0));
    }

    #[test]
    fn restored_does_not_clear_rtt() {
        // QualityRestored is a threshold event for jitter/loss only.
        // rtt_ms is an absolute measurement and shouldn't be reset.
        let mut t = RtpStatsTracker::new(Some(Duration::from_secs(5)));
        t.note_rtcp_report(50.0, 0.1, Some(80.0));
        t.note_quality_restored();
        let snap = t.snapshot();
        assert_eq!(snap.jitter_ms, Some(0.0));
        assert_eq!(snap.packet_loss_ratio, Some(0.0));
        assert_eq!(snap.rtt_ms, Some(80.0));
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
