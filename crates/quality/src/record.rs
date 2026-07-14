//! The on-the-wire quality record.
//!
//! Shape = the CDR `quality` block plus record framing
//! (`call_id` / `ts` / `seq` / `kind`), per the locked design (D3).
//! Reusing [`siphon_ai_cdr::QualityInfo`] as the flattened payload
//! guarantees the history feed and the CDR agree field-for-field.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use siphon_ai_cdr::QualityInfo;

/// Schema version of the quality record. Follows the CDR rules
/// (CLAUDE.md §7.7): additive optional fields don't bump; anything a
/// strict parser could choke on does.
pub const QUALITY_RECORD_VERSION: u32 = 1;

/// Whether this record is a mid-call sample or the end-of-call summary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordKind {
    /// Periodic sample at `[quality].interval_secs` cadence. Counters
    /// inside are cumulative-since-call-start, not per-interval deltas.
    Interval,
    /// Final summary, emitted once when the call's media tap winds
    /// down. Field-for-field the same numbers the CDR `quality` block
    /// carries for the call.
    Final,
}

impl RecordKind {
    /// Stable low-cardinality metric label.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Interval => "interval",
            Self::Final => "final",
        }
    }
}

/// One quality record: framing + the flattened CDR quality payload.
/// Serialised as a single JSON object on a single line for JSONL
/// sinks.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QualityRecord {
    /// Schema version ([`QUALITY_RECORD_VERSION`]).
    pub version: u32,
    /// Sample or end-of-call summary.
    pub kind: RecordKind,
    /// SiphonAI bridge call id — the same one on the WS `start`
    /// message and the CDR, so records join against both.
    pub call_id: String,
    /// When this record was sampled.
    pub ts: DateTime<Utc>,
    /// Per-call record counter, starting at 0. Monotonic within a
    /// call; the final record carries the next value in sequence.
    pub seq: u64,
    /// The quality numbers, identical in shape to the CDR `quality`
    /// block (unmeasured fields omitted, not zeroed).
    #[serde(flatten)]
    pub quality: QualityInfo,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_quality() -> QualityInfo {
        QualityInfo {
            first_audio_out_ms: Some(742),
            barge_in_count: 3,
            avg_jitter_ms: Some(11.5),
            max_jitter_ms: Some(30.0),
            avg_packet_loss_ratio: Some(0.004),
            max_packet_loss_ratio: Some(0.02),
            avg_rtcp_rtt_ms: None,
            rx_packets_received: Some(14_820),
            rx_packets_lost: Some(12),
            rx_packets_out_of_order: Some(3),
            rx_packets_duplicate: Some(0),
            mos_estimate_min: Some(3.9),
            mos_estimate_avg: Some(4.3),
        }
    }

    #[test]
    fn record_flattens_quality_fields_to_top_level() {
        let rec = QualityRecord {
            version: QUALITY_RECORD_VERSION,
            kind: RecordKind::Interval,
            call_id: "siphon-abc".into(),
            ts: Utc::now(),
            seq: 4,
            quality: sample_quality(),
        };
        let v = serde_json::to_value(&rec).unwrap();
        // Framing + payload live side by side — no nested "quality"
        // object (that's the CDR's shape; the record IS the block).
        assert_eq!(v["kind"], "interval");
        assert_eq!(v["call_id"], "siphon-abc");
        assert_eq!(v["seq"], 4);
        assert_eq!(v["rx_packets_received"], 14_820);
        assert_eq!(v["barge_in_count"], 3);
        assert!(v.get("quality").is_none());
        // Unmeasured fields omitted, not null.
        assert!(v.get("avg_rtcp_rtt_ms").is_none());
    }

    #[test]
    fn record_round_trips() {
        let rec = QualityRecord {
            version: QUALITY_RECORD_VERSION,
            kind: RecordKind::Final,
            call_id: "siphon-xyz".into(),
            ts: Utc::now(),
            seq: 9,
            quality: sample_quality(),
        };
        let json = serde_json::to_string(&rec).unwrap();
        let back: QualityRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(back, rec);
    }
}
