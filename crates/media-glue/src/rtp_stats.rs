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
//!
//! ## RX side (0.30.0)
//!
//! The RR-derived fields above are **remote-reported** — how the far
//! end receives the stream we send. forge additionally publishes a
//! periodic [`forge_core::ForgeEvent::MediaStatsSnapshot`] with
//! **locally-measured** receive-side counters (loss, reorder,
//! duplicates, RFC 3550 interarrival jitter on the stream we receive);
//! [`RtpStatsTracker::note_media_stats`] caches the latest one. The
//! two viewpoints ride the same `rtp_stats` wire event so consumers
//! can spot asymmetric paths. A transport-only MOS-CQE estimate is
//! derived from the RX side + RTCP RTT (see [`mos_estimate`]).

use std::time::Duration;

/// Locally-measured receive-side counters, cached verbatim from the
/// latest `MediaStatsSnapshot`. Cumulative since call start.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RxStats {
    pub jitter_ms: f32,
    pub packets_received: u64,
    pub packets_lost: u64,
    pub packets_out_of_order: u64,
    pub packets_duplicate: u64,
}

impl RxStats {
    /// Loss as a percentage of packets expected (received + lost) —
    /// the E-model's input unit.
    fn loss_percent(&self) -> f32 {
        let expected = self.packets_received + self.packets_lost;
        if expected == 0 {
            return 0.0;
        }
        (self.packets_lost as f32 / expected as f32) * 100.0
    }
}

/// Transport-only MOS-CQE estimate in `[1.0, 5.0]` via the simplified
/// E-model (ITU-T G.107 reduced to jitter/loss/RTT) — the same math
/// heplify-server applies to our HEP QoS chunks, so Homer-side and
/// WS-side numbers agree. Reflects transport impairment only, not
/// codec or content quality.
///
/// `rtt_ms = 0.0` (RTT unknown) yields a slightly optimistic score;
/// callers decide whether to compute at all (we require an RX
/// snapshot, see [`RtpStatsTracker::snapshot`]).
pub fn mos_estimate(jitter_ms: f32, loss_percent: f32, rtt_ms: f32) -> f32 {
    // Effective one-way latency: RTT plus double-jitter headroom plus
    // 10 ms codec/processing allowance (Cole–Rosenbluth convention).
    let effective_latency = rtt_ms + jitter_ms * 2.0 + 10.0;
    let mut r = if effective_latency < 160.0 {
        93.2 - effective_latency / 40.0
    } else {
        93.2 - (effective_latency - 120.0) / 10.0
    };
    r -= loss_percent * 2.5;
    r = r.clamp(0.0, 100.0);
    let mos = 1.0 + 0.035 * r + 0.000_007 * r * (r - 52.0) * (100.0 - r);
    mos.clamp(1.0, 5.0)
}

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
    /// Latest locally-measured RX counters; `None` until the first
    /// `MediaStatsSnapshot` arrives (0.30.0).
    pub rx: Option<RxStats>,
    /// Transport-only MOS-CQE estimate; populated whenever `rx` is
    /// (RTT contributes when known, else 0 — see [`mos_estimate`]).
    pub mos_estimate: Option<f32>,
}

/// Whole-call quality aggregates for the CDR `quality` block (0.30.0).
/// Produced by [`RtpStatsTracker::quality_summary`]; every field is
/// `None` until its signal produced at least one sample.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct QualitySummary {
    /// Mean / max RR-reported jitter across the call's RTCP reports.
    pub avg_jitter_ms: Option<f32>,
    pub max_jitter_ms: Option<f32>,
    /// Mean / max RR-reported cumulative-loss ratio.
    pub avg_packet_loss_ratio: Option<f32>,
    pub max_packet_loss_ratio: Option<f32>,
    /// Mean RTCP RTT across reports that carried one.
    pub avg_rtcp_rtt_ms: Option<f32>,
    /// Latest (cumulative) local receive-side counters.
    pub rx: Option<RxStats>,
    /// Worst / mean transport-only MOS estimate, sampled once per
    /// local media-stats snapshot.
    pub mos_min: Option<f32>,
    pub mos_avg: Option<f32>,
}

impl QualitySummary {
    /// True when no signal ever produced a sample — the CDR omits the
    /// whole block then.
    pub fn is_empty(&self) -> bool {
        self.avg_jitter_ms.is_none() && self.rx.is_none() && self.avg_rtcp_rtt_ms.is_none()
    }
}

/// What the tap publishes on its quality watch channel (0.30.0) — the
/// live whole-call quality state, refreshed on every quality-relevant
/// event. The controller reads the latest value at teardown to build
/// the CDR `quality` block.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct QualityReport {
    pub stats: QualitySummary,
    /// Playout clears: `auto_clear` firings + server `clear` commands.
    pub barge_in_count: u32,
    /// When the first WS-server audio frame reached playout toward the
    /// caller. Sticky — set once, never reset (unlike the tap's
    /// Mark-estimation clock). The controller subtracts its
    /// bridge-connected instant to get `first_audio_out_ms`.
    pub first_audio_at: Option<std::time::Instant>,
}

/// Running sums/extremes behind [`QualitySummary`]. Sums in f64 so a
/// multi-hour call doesn't lose precision accumulating f32 samples.
#[derive(Debug, Clone, Copy, Default)]
struct QualityAggregates {
    jitter_sum: f64,
    jitter_max: f32,
    loss_sum: f64,
    loss_max: f32,
    rr_n: u32,
    rtt_sum: f64,
    rtt_n: u32,
    mos_sum: f64,
    mos_min: f32,
    mos_n: u32,
}

/// Per-call RTP-stats state. Owned by the tap; updated from forge
/// event arms; polled by the periodic-emit arm.
#[derive(Debug, Clone, Copy)]
pub struct RtpStatsTracker {
    interval: Option<Duration>,
    last_jitter_ms: Option<f32>,
    last_packet_loss_ratio: Option<f32>,
    last_rtt_ms: Option<f32>,
    last_rx: Option<RxStats>,
    agg: QualityAggregates,
}

impl RtpStatsTracker {
    /// `interval = None` disables periodic emission entirely.
    pub fn new(interval: Option<Duration>) -> Self {
        Self {
            interval,
            last_jitter_ms: None,
            last_packet_loss_ratio: None,
            last_rtt_ms: None,
            last_rx: None,
            agg: QualityAggregates::default(),
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
        // Whole-call aggregates for the CDR quality block.
        self.agg.jitter_sum += jitter_ms as f64;
        self.agg.jitter_max = self.agg.jitter_max.max(jitter_ms);
        self.agg.loss_sum += packet_loss_ratio as f64;
        self.agg.loss_max = self.agg.loss_max.max(packet_loss_ratio);
        self.agg.rr_n += 1;
        if let Some(rtt) = rtt_ms {
            self.agg.rtt_sum += rtt as f64;
            self.agg.rtt_n += 1;
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

    /// forge published a `MediaStatsSnapshot` — cache the locally-
    /// measured RX counters verbatim (they're cumulative; the newest
    /// snapshot supersedes the previous one), and take one MOS sample
    /// for the whole-call aggregates.
    pub fn note_media_stats(&mut self, rx: RxStats) {
        self.last_rx = Some(rx);
        let mos = mos_estimate(
            rx.jitter_ms,
            rx.loss_percent(),
            self.last_rtt_ms.unwrap_or(0.0),
        );
        self.agg.mos_sum += mos as f64;
        self.agg.mos_min = if self.agg.mos_n == 0 {
            mos
        } else {
            self.agg.mos_min.min(mos)
        };
        self.agg.mos_n += 1;
    }

    /// Whole-call aggregates for the CDR `quality` block.
    pub fn quality_summary(&self) -> QualitySummary {
        let rr = self.agg.rr_n;
        QualitySummary {
            avg_jitter_ms: (rr > 0).then(|| (self.agg.jitter_sum / rr as f64) as f32),
            max_jitter_ms: (rr > 0).then_some(self.agg.jitter_max),
            avg_packet_loss_ratio: (rr > 0).then(|| (self.agg.loss_sum / rr as f64) as f32),
            max_packet_loss_ratio: (rr > 0).then_some(self.agg.loss_max),
            avg_rtcp_rtt_ms: (self.agg.rtt_n > 0)
                .then(|| (self.agg.rtt_sum / self.agg.rtt_n as f64) as f32),
            rx: self.last_rx,
            mos_min: (self.agg.mos_n > 0).then_some(self.agg.mos_min),
            mos_avg: (self.agg.mos_n > 0)
                .then(|| (self.agg.mos_sum / self.agg.mos_n as f64) as f32),
        }
    }

    /// Current snapshot for the periodic-emit arm. `mos_estimate`
    /// requires at least one RX snapshot (jitter + loss are its load-
    /// bearing inputs); RTT sharpens it when RTCP has produced one.
    pub fn snapshot(&self) -> RtpStatsSnapshot {
        let mos = self.last_rx.map(|rx| {
            mos_estimate(
                rx.jitter_ms,
                rx.loss_percent(),
                self.last_rtt_ms.unwrap_or(0.0),
            )
        });
        RtpStatsSnapshot {
            jitter_ms: self.last_jitter_ms,
            packet_loss_ratio: self.last_packet_loss_ratio,
            rtt_ms: self.last_rtt_ms,
            rx: self.last_rx,
            mos_estimate: mos,
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

    fn rx(jitter_ms: f32, received: u64, lost: u64) -> RxStats {
        RxStats {
            jitter_ms,
            packets_received: received,
            packets_lost: lost,
            packets_out_of_order: 0,
            packets_duplicate: 0,
        }
    }

    #[test]
    fn fresh_tracker_has_no_rx_or_mos() {
        let t = RtpStatsTracker::new(Some(Duration::from_secs(5)));
        let snap = t.snapshot();
        assert!(snap.rx.is_none());
        assert!(snap.mos_estimate.is_none(), "no MOS without RX inputs");
    }

    #[test]
    fn media_stats_populates_rx_and_mos() {
        let mut t = RtpStatsTracker::new(Some(Duration::from_secs(5)));
        t.note_media_stats(rx(3.5, 1500, 6));
        let snap = t.snapshot();
        assert_eq!(snap.rx, Some(rx(3.5, 1500, 6)));
        let mos = snap.mos_estimate.expect("MOS once RX data exists");
        assert!((1.0..=5.0).contains(&mos), "mos = {mos}");
    }

    #[test]
    fn newer_media_stats_supersede_older() {
        // Counters are cumulative — latest snapshot wins outright.
        let mut t = RtpStatsTracker::new(Some(Duration::from_secs(5)));
        t.note_media_stats(rx(3.5, 500, 2));
        t.note_media_stats(rx(4.0, 1500, 6));
        assert_eq!(t.snapshot().rx, Some(rx(4.0, 1500, 6)));
    }

    #[test]
    fn rx_does_not_disturb_remote_reported_fields() {
        // The two viewpoints are independent halves of the same event.
        let mut t = RtpStatsTracker::new(Some(Duration::from_secs(5)));
        t.note_rtcp_report(17.2, 0.025, Some(42.0));
        t.note_media_stats(rx(3.5, 1500, 6));
        let snap = t.snapshot();
        assert_eq!(snap.jitter_ms, Some(17.2));
        assert_eq!(snap.rtt_ms, Some(42.0));
        assert_eq!(snap.rx, Some(rx(3.5, 1500, 6)));
    }

    #[test]
    fn mos_clean_call_scores_high() {
        // ~0 jitter, 0 loss, no RTT: textbook R ≈ 92.9 → MOS ≈ 4.4.
        let mos = mos_estimate(0.0, 0.0, 0.0);
        assert!(mos > 4.3, "clean call MOS = {mos}");
    }

    #[test]
    fn mos_degrades_monotonically_with_loss() {
        let clean = mos_estimate(2.0, 0.0, 40.0);
        let lossy = mos_estimate(2.0, 5.0, 40.0);
        let awful = mos_estimate(2.0, 20.0, 40.0);
        assert!(clean > lossy, "{clean} > {lossy}");
        assert!(lossy > awful, "{lossy} > {awful}");
        assert!(awful >= 1.0);
    }

    #[test]
    fn mos_high_latency_takes_steeper_slope() {
        // ≥160 ms effective latency switches to the steeper Id term:
        // R drops 1 point per 10 ms instead of per 40 ms. At 620 ms
        // effective (600 RTT + 2×5 jitter + 10) R = 43.2 → MOS ≈ 2.2,
        // versus R = 90.2 → MOS ≈ 4.4 at 120 ms effective.
        let low = mos_estimate(5.0, 0.0, 100.0); // eff = 120
        let high = mos_estimate(5.0, 0.0, 600.0); // eff = 620
        assert!(low - high > 1.5, "steep penalty: {low} vs {high}");
        assert!(high >= 1.0);
    }

    #[test]
    fn mos_stays_in_bounds_at_extremes() {
        let worst = mos_estimate(500.0, 100.0, 2000.0);
        assert_eq!(worst, 1.0);
        let best = mos_estimate(0.0, 0.0, 0.0);
        assert!(best <= 5.0);
    }

    #[test]
    fn rx_loss_percent_handles_zero_expected() {
        // Degenerate: snapshot before any packet counted. No panic,
        // 0% loss.
        let mut t = RtpStatsTracker::new(Some(Duration::from_secs(5)));
        t.note_media_stats(rx(0.0, 0, 0));
        let mos = t.snapshot().mos_estimate.expect("computable");
        assert!(mos > 4.0);
    }
}
