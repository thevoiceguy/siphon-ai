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
            let (calls, registrations, drain, errors) = tokio::join!(
                client.calls(),
                client.registrations(),
                client.drain(),
                client.errors()
            );
            // The errors ring is 0.49.0+; a failure there (404 from an
            // older daemon) must not mark the node down — it degrades
            // to "endpoint unavailable" on the Errors tab.
            let errors = errors.ok().map(|e| e.errors);
            let result = calls
                .and_then(|c| registrations.map(|r| (c, r)))
                .and_then(|(c, r)| {
                    drain.map(|d| NodeSnapshot {
                        calls: c.calls,
                        registrations: r.registrations,
                        drain: d,
                        errors,
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
