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
    quality: watch::Receiver<QualityReport>,
    epoch: watch::Receiver<Option<Instant>>,
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
        quality: watch::Receiver<QualityReport>,
        epoch: watch::Receiver<Option<Instant>>,
    ) -> Self {
        live()
            .write()
            .insert(call_id.to_string(), LiveEntry { quality, epoch });
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
    #[serde(flatten)]
    pub quality: siphon_ai_cdr::QualityInfo,
}

/// Snapshot one active call's current quality state. `None` when no
/// active call has that bridge `call_id` (ended calls answer through
/// the CDR / history records instead).
pub fn snapshot(call_id: &str) -> Option<LiveQualityStats> {
    let (report, connected_at) = {
        let map = live().read();
        let entry = map.get(call_id)?;
        let report = *entry.quality.borrow();
        let connected_at = *entry.epoch.borrow();
        (report, connected_at)
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
        let guard = LiveQualityGuard::register("siphon-live-1", qrx, erx);

        // Young call: exists, empty quality fields.
        let row = snapshot("siphon-live-1").expect("registered");
        assert_eq!(row.call_id, "siphon-live-1");
        assert_eq!(row.quality.barge_in_count, 0);

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
