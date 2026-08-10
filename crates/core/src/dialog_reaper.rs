//! Deferred removal of finished dialogs from the shared
//! [`DialogManager`] (siphon-ai #458).
//!
//! # Why this exists
//!
//! `sip-uas` inserts one confirmed dialog per accepted INVITE and
//! siphon-ai never removed it, so the store grew for the life of the
//! process — roughly 1 KB of live heap per call, and worse than a leak
//! at the far end: `sip-dialog` caps the store at
//! `MAX_CONFIRMED_DIALOGS` (10,000) and `sip-uas` discards the
//! resulting `insert` error, so past that point dialogs silently stop
//! being tracked and in-dialog requests no longer match.
//!
//! # Why it defers instead of removing at teardown
//!
//! The dialog store is what in-dialog requests are matched against —
//! BYE, the post-REFER NOTIFY, and the transfer UAC all resolve through
//! it, which is why it is shared at all (#377). Removing a dialog the
//! moment its call ends would race the very BYE that ends the call: a
//! peer retransmitting BYE after our `200 OK` must still find the
//! dialog, or it gets "unknown dialog" instead. The acceptor's teardown
//! path already sequences itself around exactly this hazard.
//!
//! So removal is decoupled: teardown *retires* a `DialogId`, and a
//! sweeper removes it once [`DEFAULT_GRACE`] has passed. Within the
//! grace window the dialog behaves exactly as it does today.
//!
//! This is the "mark, then sweep" shape `DialogManager` is built for —
//! `cleanup_terminated()` retains `state != Terminated` — but that
//! path is unusable from here: `DialogManager::get` hands back a
//! *clone*, so `Dialog::terminate()` would mark a copy and leave the
//! stored dialog untouched. Retiring an id and calling `remove()` on it
//! achieves the same effect without needing an upstream in-place state
//! update.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use sip_dialog::{DialogId, DialogManager};
use siphon_ai_telemetry::metrics::DIALOGS_ACTIVE;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{debug, warn};

/// How long a finished dialog stays in the store before the sweeper
/// removes it.
///
/// 32 s is SIP Timer H / J — the window in which a peer may still be
/// retransmitting the BYE that ended the call. Removing sooner risks
/// answering a legitimate retransmission with "unknown dialog"; the
/// only cost of removing later is that the dialog occupies the store
/// for one more window.
pub const DEFAULT_GRACE: Duration = Duration::from_secs(32);

/// How often the sweeper runs. The grace window is the thing that
/// matters for correctness; this only bounds how far past it a dialog
/// can linger, and how often the gauge is refreshed.
pub const DEFAULT_SWEEP_INTERVAL: Duration = Duration::from_secs(5);

/// Bound on retirements queued between sweeps. At 5 s sweeps this is
/// far above any real call rate; a full queue means the sweeper is
/// wedged, in which case dropping a retirement is strictly better than
/// blocking a call's teardown (CLAUDE.md §4.3 — teardown never blocks
/// on housekeeping).
const RETIRE_QUEUE: usize = 4096;

/// Handle for retiring finished dialogs. Cheap to clone.
#[derive(Clone, Debug)]
pub struct DialogReaper {
    tx: mpsc::Sender<DialogId>,
}

impl DialogReaper {
    /// Queue `id` for removal once the grace window has elapsed.
    ///
    /// Non-blocking and infallible by design: a failure here must never
    /// hold up a call's teardown. A dropped retirement leaks one dialog
    /// (the pre-#458 behaviour for that one call), which is why the
    /// drop is warned about rather than ignored.
    pub fn retire(&self, id: DialogId) {
        if self.tx.try_send(id).is_err() {
            warn!(
                queue = RETIRE_QUEUE,
                "dialog retire queue full or sweeper gone; dialog will not be reclaimed"
            );
        }
    }

    /// Spawn the sweeper over `manager` and return the handle plus its
    /// `JoinHandle`. The task ends when every [`DialogReaper`] clone is
    /// dropped.
    pub fn spawn(manager: Arc<DialogManager>) -> (Self, JoinHandle<()>) {
        Self::spawn_with(manager, DEFAULT_GRACE, DEFAULT_SWEEP_INTERVAL)
    }

    /// [`spawn`](Self::spawn) with explicit timings — for tests, which
    /// cannot wait 32 s.
    pub fn spawn_with(
        manager: Arc<DialogManager>,
        grace: Duration,
        interval: Duration,
    ) -> (Self, JoinHandle<()>) {
        let (tx, rx) = mpsc::channel(RETIRE_QUEUE);
        let handle = tokio::spawn(sweep(manager, rx, grace, interval));
        (Self { tx }, handle)
    }
}

/// Drain retirements into a FIFO, remove those past `grace`, and
/// republish the store's size.
///
/// The queue is a `VecDeque` and retirements arrive in time order, so
/// expiry is a prefix scan — no per-entry timer, no sorting.
async fn sweep(
    manager: Arc<DialogManager>,
    mut rx: mpsc::Receiver<DialogId>,
    grace: Duration,
    interval: Duration,
) {
    let mut pending: VecDeque<(DialogId, tokio::time::Instant)> = VecDeque::new();
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    // Publish once before any call, so the series exists from startup
    // rather than appearing only after the first sweep.
    metrics::gauge!(DIALOGS_ACTIVE).set(manager.count() as f64);

    loop {
        tokio::select! {
            // Bias the receive arm: retirements are cheap to absorb and
            // buffering them here keeps the bounded channel empty.
            biased;
            maybe = rx.recv() => {
                match maybe {
                    Some(id) => pending.push_back((id, tokio::time::Instant::now())),
                    // Every handle dropped — the daemon is shutting
                    // down. The process is about to exit, so there is
                    // nothing to reclaim.
                    None => {
                        debug!("dialog reaper stopping; all handles dropped");
                        return;
                    }
                }
            }
            _ = ticker.tick() => {
                let now = tokio::time::Instant::now();
                let mut removed = 0usize;
                while let Some((_, retired_at)) = pending.front() {
                    if now.duration_since(*retired_at) < grace {
                        break; // FIFO: nothing behind this is older
                    }
                    let (id, _) = pending.pop_front().expect("front just checked");
                    if manager.remove(&id).is_some() {
                        removed += 1;
                    }
                }
                if removed > 0 {
                    debug!(removed, pending = pending.len(), "reaped finished dialogs");
                }
                metrics::gauge!(DIALOGS_ACTIVE).set(manager.count() as f64);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::test_support::consult_dialog;

    /// Short timings so the tests run in real time without a paused
    /// clock (tokio's `test-util` isn't enabled in this crate).
    const GRACE: Duration = Duration::from_millis(400);
    const SWEEP: Duration = Duration::from_millis(50);

    fn seed(mgr: &DialogManager, n: usize) -> DialogId {
        let d = consult_dialog(
            &format!("call-{n}@test"),
            &format!("l{n}"),
            &format!("r{n}"),
        );
        let id = d.id().clone();
        mgr.insert(d).expect("insert");
        id
    }

    /// The deferral is the whole point: a retransmitted BYE arriving
    /// just after teardown must still match the dialog.
    #[tokio::test]
    async fn retired_dialog_survives_the_grace_window_then_is_removed() {
        let mgr = Arc::new(DialogManager::new());
        let id = seed(&mgr, 1);
        let (reaper, _task) = DialogReaper::spawn_with(Arc::clone(&mgr), GRACE, SWEEP);
        reaper.retire(id.clone());

        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(mgr.count(), 1, "removed before the grace window elapsed");
        assert!(mgr.get(&id).is_some(), "dialog must still be matchable");

        tokio::time::sleep(Duration::from_millis(600)).await;
        assert_eq!(mgr.count(), 0, "not reclaimed after the grace window");
        assert!(mgr.get(&id).is_none());
    }

    /// #458 in miniature: many calls' worth of dialogs must not
    /// accumulate.
    #[tokio::test]
    async fn many_retirements_all_drain() {
        let mgr = Arc::new(DialogManager::new());
        let (reaper, _task) = DialogReaper::spawn_with(Arc::clone(&mgr), GRACE, SWEEP);
        for n in 0..500 {
            reaper.retire(seed(&mgr, n));
        }
        assert_eq!(mgr.count(), 500);

        tokio::time::sleep(Duration::from_millis(900)).await;
        assert_eq!(mgr.count(), 0, "store must return to empty");
    }

    /// The reaper must never collect a dialog nobody retired — that
    /// would be garbage-collecting a live call.
    #[tokio::test]
    async fn unretired_dialogs_are_left_alone() {
        let mgr = Arc::new(DialogManager::new());
        let live_id = seed(&mgr, 1);
        let done_id = seed(&mgr, 2);
        let (reaper, _task) = DialogReaper::spawn_with(Arc::clone(&mgr), GRACE, SWEEP);
        reaper.retire(done_id.clone());

        tokio::time::sleep(Duration::from_millis(900)).await;
        assert!(mgr.get(&live_id).is_some(), "live dialog must survive");
        assert!(mgr.get(&done_id).is_none(), "retired dialog must be gone");
    }
}
