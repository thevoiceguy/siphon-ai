//! Per-node poll tasks.
//!
//! One task per node (DESIGN_SIGHTGLASS.md §2): fetch the three list
//! endpoints concurrently each interval, plus the focused call's
//! stats when the focus (a `watch` fed by the UI) points at this
//! node. Start times are staggered so N nodes don't burst-poll in
//! phase. A task ends when the UI side of the channel is gone.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, watch};

use crate::client::AdminClient;
use crate::model::{Msg, NodeId, NodeSnapshot};

pub fn spawn(
    node: NodeId,
    node_count: usize,
    client: Arc<AdminClient>,
    interval: Duration,
    tx: mpsc::Sender<Msg>,
    focus: watch::Receiver<Option<(NodeId, String)>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        // Spread node start offsets across one interval.
        tokio::time::sleep(interval * node as u32 / node_count.max(1) as u32).await;
        let mut tick = tokio::time::interval(interval);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tick.tick().await;
            let (calls, registrations, drain, errors, conferences, parked, status, recent, log) = tokio::join!(
                client.calls(),
                client.registrations(),
                client.drain(),
                client.errors(),
                client.conferences(),
                client.parked(),
                client.status(),
                client.recent_cdrs(),
                client.log_filter()
            );
            // Only the three core endpoints decide node health. The
            // errors ring and status are 0.49.0+ (older daemons 404 →
            // "endpoint unavailable", never node-down); conferences and
            // parked answer 501 when the feature is off → empty lists.
            let errors = errors.ok().map(|e| e.errors);
            let conferences = conferences.map(|c| c.conferences).unwrap_or_default();
            let parked = parked.map(|p| p.parked).unwrap_or_default();
            let status = status.ok();
            let recent_cdrs = recent.ok().map(|r| r.cdrs);
            let log_filter = log.ok().map(|l| l.filter);
            let result = calls
                .and_then(|c| registrations.map(|r| (c, r)))
                .and_then(|(c, r)| {
                    drain.map(|d| {
                        Box::new(NodeSnapshot {
                            calls: c.calls,
                            registrations: r.registrations,
                            drain: d,
                            conferences,
                            parked,
                            status,
                            log_filter,
                            recent_cdrs,
                            errors,
                        })
                    })
                });
            if tx.send(Msg::Snapshot { node, result }).await.is_err() {
                return; // UI is gone — shut down.
            }

            // Focused-call stats: only this node's poller fetches, and
            // only while the focus points here (§2 — never fanned out).
            let focused = focus.borrow().clone();
            if let Some((focus_node, call_id)) = focused {
                if focus_node == node {
                    if let Ok(stats) = client.call_stats(&call_id).await {
                        let send = tx
                            .send(Msg::Stats {
                                node,
                                call_id,
                                stats,
                            })
                            .await;
                        if send.is_err() {
                            return;
                        }
                    }
                    // A stats error is not node-down: the call likely
                    // just ended between the listing and this fetch.
                    // The next snapshot will drop the row.
                }
            }
        }
    })
}
