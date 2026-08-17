//! Live per-call quality snapshots for `GET /admin/v1/calls/{id}/stats`
//! (0.31.0) — the "what is this call doing *right now*" probe.
//!
//! Each `CallController` registers its quality + connect-epoch watch
//! receivers here for the call's lifetime (RAII guard, so teardown can
//! never leak an entry). An admin request resolves the bridge
//! `call_id`, borrows the latest [`QualityReport`] from the watch, and
//! serializes it in the same shape as the CDR `quality` block — one
//! mapping (`acceptor::quality_info`) feeds the CDR, the history
//! records, and this endpoint, so all three always agree.
//!
//! This is an admin-read registry in the spirit of
//! [`crate::registry::CallRegistry`]: nothing on a call's audio or
//! control path ever reads another call's entry (CLAUDE.md §4.4), and
//! the lock is touched once per call setup/teardown plus per admin
//! request — never per frame.

use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::Instant;

use chrono::{DateTime, Utc};
use parking_lot::RwLock;
use serde::Serialize;
use siphon_ai_media_glue::QualityReport;
use tokio::sync::watch;

use crate::call::QualityOutcome;

struct LiveEntry {
    meta: LiveCallMeta,
    quality: watch::Receiver<QualityReport>,
    epoch: watch::Receiver<Option<Instant>>,
}

/// Static call facts registered alongside the quality feed (0.49.0,
/// DESIGN_SIGHTGLASS.md §6.4) and serialized additively into the
/// stats response — everything here is known at bridge setup and
/// never changes for the call's lifetime.
#[derive(Debug, Clone, Serialize)]
pub struct LiveCallMeta {
    /// `"inbound"` | `"outbound"`.
    pub direction: String,
    pub from: String,
    pub to: String,
    pub sip_call_id: String,
    /// Bridge-side sample rate (8000/16000).
    pub sample_rate: u32,
    /// Negotiated SRTP suite; `None` = plaintext RTP.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub srtp_profile: Option<String>,
    /// STIR/SHAKEN attestation (`"A"`/`"B"`/`"C"`), when verification
    /// ran.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verstat_attest: Option<String>,
}

fn live() -> &'static RwLock<HashMap<String, LiveEntry>> {
    static LIVE: OnceLock<RwLock<HashMap<String, LiveEntry>>> = OnceLock::new();
    LIVE.get_or_init(|| RwLock::new(HashMap::new()))
}

/// RAII registration: created by the `CallController` at setup,
/// deregisters on drop (i.e. on any teardown path, including panics
/// unwinding the controller task).
pub struct LiveQualityGuard {
    call_id: String,
}

impl LiveQualityGuard {
    pub fn register(
        call_id: &str,
        meta: LiveCallMeta,
        quality: watch::Receiver<QualityReport>,
        epoch: watch::Receiver<Option<Instant>>,
    ) -> Self {
        live().write().insert(
            call_id.to_string(),
            LiveEntry {
                meta,
                quality,
                epoch,
            },
        );
        Self {
            call_id: call_id.to_string(),
        }
    }
}

impl Drop for LiveQualityGuard {
    fn drop(&mut self) {
        live().write().remove(&self.call_id);
    }
}

/// What the admin endpoint serves: the CDR `quality` shape plus
/// probe framing. `quality` fields are individually omitted when
/// unmeasured — a young call legitimately answers `{}`-ish.
#[derive(Debug, Clone, Serialize)]
pub struct LiveQualityStats {
    pub call_id: String,
    /// When this snapshot was taken (i.e. now — it's a live probe).
    pub sampled_at: DateTime<Utc>,
    /// Static call facts (0.49.0) — additive fields, flattened.
    #[serde(flatten)]
    pub meta: LiveCallMeta,
    #[serde(flatten)]
    pub quality: siphon_ai_cdr::QualityInfo,
}

/// Snapshot one active call's current quality state. `None` when no
/// active call has that bridge `call_id` (ended calls answer through
/// the CDR / history records instead).
pub fn snapshot(call_id: &str) -> Option<LiveQualityStats> {
    let (report, connected_at, meta) = {
        let map = live().read();
        let entry = map.get(call_id)?;
        let report = *entry.quality.borrow();
        let connected_at = *entry.epoch.borrow();
        (report, connected_at, entry.meta.clone())
    };
    // Unlike the CDR (which omits an unmeasured block entirely), an
    // existing call always answers — with whatever is known so far.
    let outcome = QualityOutcome::from_report(report, connected_at).unwrap_or(QualityOutcome {
        first_audio_out_ms: None,
        barge_in_count: 0,
        stats: Default::default(),
    });
    Some(LiveQualityStats {
        call_id: call_id.to_string(),
        sampled_at: Utc::now(),
        meta,
        quality: crate::acceptor::quality_info(outcome),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn unknown_call_returns_none() {
        assert!(snapshot("siphon-nope").is_none());
    }

    #[tokio::test]
    async fn registered_call_snapshots_and_guard_cleans_up() {
        let (qtx, qrx) = watch::channel(QualityReport::default());
        let (_etx, erx) = watch::channel(None);
        let guard = LiveQualityGuard::register(
            "siphon-live-1",
            LiveCallMeta {
                direction: "inbound".into(),
                from: "sipp".into(),
                to: "1000".into(),
                sip_call_id: "abc@host".into(),
                sample_rate: 8000,
                srtp_profile: None,
                verstat_attest: Some("A".into()),
            },
            qrx,
            erx,
        );

        // Young call: exists, empty quality fields, static meta set.
        let row = snapshot("siphon-live-1").expect("registered");
        assert_eq!(row.call_id, "siphon-live-1");
        assert_eq!(row.quality.barge_in_count, 0);
        assert_eq!(row.meta.direction, "inbound");
        // Additive wire shape: meta flattens beside the quality
        // fields; srtp_profile is omitted-not-null when plaintext.
        let wire = serde_json::to_value(&row).unwrap();
        assert_eq!(wire["from"], "sipp");
        assert_eq!(wire["sample_rate"], 8000);
        assert_eq!(wire["verstat_attest"], "A");
        assert!(wire.get("srtp_profile").is_none());

        // A tap update shows up on the next probe.
        qtx.send_replace(QualityReport {
            barge_in_count: 2,
            ..Default::default()
        });
        let row = snapshot("siphon-live-1").expect("registered");
        assert_eq!(row.quality.barge_in_count, 2);

        drop(guard);
        assert!(snapshot("siphon-live-1").is_none(), "guard deregisters");
    }
}
