//! Application state and the update function.
//!
//! Elm-shaped on purpose: pollers and the input thread produce
//! [`Msg`]s, [`App::update`] folds them into state, `ui::draw`
//! renders it. Update is pure (no I/O, no clocks), which is what the
//! multi-node reducer tests below lean on.
//!
//! The fleet invariant (DESIGN_SIGHTGLASS.md §2): every record is
//! keyed by `(node, id)`. A `NodeId` is the node's index into
//! [`App::nodes`] — stable for the process lifetime because the fleet
//! is fixed at startup.

use std::collections::VecDeque;

use siphon_ai_admin_api_types::{AdminCallRow, DrainStatus, RegistrationRow};

/// Index into [`App::nodes`]. Display name lives on the node itself.
pub type NodeId = usize;

/// Samples kept for sparklines (~2 minutes at the 1 s default poll).
const HISTORY_LEN: usize = 120;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Overview,
    Trunks,
    Calls,
}

impl Tab {
    pub const ALL: [Tab; 3] = [Tab::Overview, Tab::Trunks, Tab::Calls];

    pub fn title(self) -> &'static str {
        match self {
            Tab::Overview => "overview",
            Tab::Trunks => "trunks",
            Tab::Calls => "calls",
        }
    }

    fn index(self) -> usize {
        Self::ALL
            .iter()
            .position(|t| *t == self)
            .expect("tab is in ALL")
    }

    pub fn next(self) -> Tab {
        Self::ALL[(self.index() + 1) % Self::ALL.len()]
    }

    pub fn prev(self) -> Tab {
        Self::ALL[(self.index() + Self::ALL.len() - 1) % Self::ALL.len()]
    }
}

/// Reachability of one node's admin listener. A down node keeps its
/// last-seen data (rendered dimmed) — §2's "degrade, never break".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeHealth {
    /// No poll has completed yet.
    Connecting,
    Up,
    Down {
        error: String,
    },
}

/// Everything sightglass knows about one node.
#[derive(Debug)]
pub struct NodeState {
    pub name: String,
    pub health: NodeHealth,
    pub calls: Vec<AdminCallRow>,
    pub registrations: Vec<RegistrationRow>,
    pub drain: Option<DrainStatus>,
}

impl NodeState {
    fn new(name: String) -> Self {
        Self {
            name,
            health: NodeHealth::Connecting,
            calls: Vec::new(),
            registrations: Vec::new(),
            drain: None,
        }
    }
}

/// One successful poll round for a node.
#[derive(Debug)]
pub struct NodeSnapshot {
    pub calls: Vec<AdminCallRow>,
    pub registrations: Vec<RegistrationRow>,
    pub drain: DrainStatus,
}

/// Live-stats pane for the focused call: latest snapshot plus
/// client-side history rings (§2 — the daemon stores no history).
#[derive(Debug, Default)]
pub struct StatsPane {
    /// `(node, call_id)` the pane is showing. Cleared on focus change
    /// so a new selection never renders the previous call's numbers.
    pub key: Option<(NodeId, String)>,
    pub latest: Option<serde_json::Value>,
    /// MOS ×100 (Sparkline wants u64), newest last.
    pub mos_hist: VecDeque<u64>,
    /// Average jitter in ms, newest last.
    pub jitter_hist: VecDeque<u64>,
}

#[derive(Debug)]
pub enum Msg {
    /// One poll round finished for a node.
    Snapshot {
        node: NodeId,
        result: Result<NodeSnapshot, String>,
    },
    /// Stats arrived for the focused call.
    Stats {
        node: NodeId,
        call_id: String,
        stats: serde_json::Value,
    },
    /// 1 s cadence: sample fleet history for the overview sparkline.
    Tick,
    /// A terminal event from the input thread. Routed to `keys::handle`
    /// by the main loop before `update` ever sees it.
    Input(ratatui::crossterm::event::Event),
}

pub struct App {
    pub nodes: Vec<NodeState>,
    pub tab: Tab,
    /// `None` = all nodes; `Some(id)` scopes every tab to one node.
    pub node_filter: Option<NodeId>,
    /// Selection index into [`App::visible_calls`].
    pub selected_call: usize,
    pub stats: StatsPane,
    /// Fleet-wide active-call totals, one sample per tick, newest last.
    pub fleet_history: VecDeque<u64>,
    pub read_only: bool,
    pub should_quit: bool,
}

impl App {
    pub fn new(node_names: Vec<String>, read_only: bool) -> Self {
        Self {
            nodes: node_names.into_iter().map(NodeState::new).collect(),
            tab: Tab::Overview,
            node_filter: None,
            selected_call: 0,
            stats: StatsPane::default(),
            fleet_history: VecDeque::new(),
            read_only,
            should_quit: false,
        }
    }

    pub fn update(&mut self, msg: Msg) {
        match msg {
            Msg::Snapshot { node, result } => {
                let Some(n) = self.nodes.get_mut(node) else {
                    return;
                };
                match result {
                    Ok(snap) => {
                        n.health = NodeHealth::Up;
                        n.calls = snap.calls;
                        n.registrations = snap.registrations;
                        n.drain = Some(snap.drain);
                    }
                    // Keep last-seen data; only the health flips.
                    Err(error) => n.health = NodeHealth::Down { error },
                }
                self.clamp_selection();
                self.sync_stats_focus();
            }
            Msg::Stats {
                node,
                call_id,
                stats,
            } => {
                // A stale in-flight fetch can land after the selection
                // moved — drop anything that isn't the current focus.
                if self.stats.key.as_ref() != Some(&(node, call_id.clone())) {
                    return;
                }
                push_capped(
                    &mut self.stats.mos_hist,
                    extract_scaled(&stats, "mos_estimate_avg", 100.0),
                );
                push_capped(
                    &mut self.stats.jitter_hist,
                    extract_scaled(&stats, "avg_jitter_ms", 1.0),
                );
                self.stats.latest = Some(stats);
            }
            Msg::Tick => {
                let total: u64 = self.nodes.iter().map(|n| n.calls.len() as u64).sum();
                push_capped(&mut self.fleet_history, Some(total));
            }
            // Input is dispatched to `keys::handle` by the main loop.
            Msg::Input(_) => {}
        }
    }

    /// Calls visible under the current node filter, in stable
    /// (node, listing) order. The selection index points into this.
    pub fn visible_calls(&self) -> Vec<(NodeId, &AdminCallRow)> {
        self.nodes
            .iter()
            .enumerate()
            .filter(|(id, _)| self.node_filter.is_none_or(|f| f == *id))
            .flat_map(|(id, n)| n.calls.iter().map(move |c| (id, c)))
            .collect()
    }

    /// The `(node, call_id)` the stats poller should be fetching.
    pub fn focus(&self) -> Option<(NodeId, String)> {
        let calls = self.visible_calls();
        calls
            .get(self.selected_call)
            .map(|(id, c)| (*id, c.call_id.clone()))
    }

    pub fn select_next(&mut self) {
        self.selected_call = self.selected_call.saturating_add(1);
        self.clamp_selection();
        self.sync_stats_focus();
    }

    pub fn select_prev(&mut self) {
        self.selected_call = self.selected_call.saturating_sub(1);
        self.sync_stats_focus();
    }

    pub fn select_first(&mut self) {
        self.selected_call = 0;
        self.sync_stats_focus();
    }

    pub fn select_last(&mut self) {
        self.selected_call = self.visible_calls().len().saturating_sub(1);
        self.sync_stats_focus();
    }

    /// Cycle the node filter: all → node 0 → node 1 → … → all.
    pub fn cycle_node_filter(&mut self) {
        self.node_filter = match self.node_filter {
            None => Some(0),
            Some(i) if i + 1 < self.nodes.len() => Some(i + 1),
            Some(_) => None,
        };
        self.clamp_selection();
        self.sync_stats_focus();
    }

    fn clamp_selection(&mut self) {
        let len = self.visible_calls().len();
        if len == 0 {
            self.selected_call = 0;
        } else if self.selected_call >= len {
            self.selected_call = len - 1;
        }
    }

    /// Reset the stats pane whenever the focused `(node, call_id)`
    /// changed, so histories never mix calls.
    fn sync_stats_focus(&mut self) {
        let focus = self.focus();
        if self.stats.key != focus {
            self.stats = StatsPane {
                key: focus,
                ..StatsPane::default()
            };
        }
    }

    // ─── Fleet rollups for the header/overview ─────────────────────

    pub fn nodes_up(&self) -> usize {
        self.nodes
            .iter()
            .filter(|n| n.health == NodeHealth::Up)
            .count()
    }

    pub fn total_calls(&self) -> usize {
        self.nodes.iter().map(|n| n.calls.len()).sum()
    }
}

fn push_capped(hist: &mut VecDeque<u64>, sample: Option<u64>) {
    let Some(sample) = sample else { return };
    if hist.len() == HISTORY_LEN {
        hist.pop_front();
    }
    hist.push_back(sample);
}

/// Pull a numeric field out of the loose stats payload, scaled to an
/// integer for sparkline use. Absent/unmeasured fields yield `None`.
fn extract_scaled(stats: &serde_json::Value, key: &str, scale: f64) -> Option<u64> {
    let v = stats.get(key)?.as_f64()?;
    if !v.is_finite() || v < 0.0 {
        return None;
    }
    Some((v * scale).round() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(id: &str) -> AdminCallRow {
        AdminCallRow {
            call_id: id.to_string(),
            sip_call_id: format!("{id}@host"),
            direction: "inbound".to_string(),
        }
    }

    fn snapshot(calls: Vec<AdminCallRow>) -> NodeSnapshot {
        NodeSnapshot {
            calls,
            registrations: vec![],
            drain: DrainStatus {
                draining: false,
                active_calls: 0,
                drain_timeout_secs: 30,
                remaining_secs: None,
            },
        }
    }

    fn two_node_app() -> App {
        let mut app = App::new(vec!["prod-1".into(), "prod-2".into()], false);
        app.update(Msg::Snapshot {
            node: 0,
            result: Ok(snapshot(vec![call("a"), call("b")])),
        });
        app.update(Msg::Snapshot {
            node: 1,
            result: Ok(snapshot(vec![call("a")])), // same call_id as node 0 on purpose
        });
        app
    }

    #[test]
    fn same_call_id_on_two_nodes_stays_two_rows() {
        let app = two_node_app();
        let visible = app.visible_calls();
        assert_eq!(visible.len(), 3);
        // The composite key differs even though call_id collides.
        let keys: Vec<(NodeId, &str)> = visible
            .iter()
            .map(|(n, c)| (*n, c.call_id.as_str()))
            .collect();
        assert_eq!(keys, vec![(0, "a"), (0, "b"), (1, "a")]);
    }

    #[test]
    fn node_filter_scopes_calls_and_cycles_back_to_all() {
        let mut app = two_node_app();
        app.cycle_node_filter(); // -> node 0
        assert_eq!(app.visible_calls().len(), 2);
        app.cycle_node_filter(); // -> node 1
        assert_eq!(app.visible_calls().len(), 1);
        assert_eq!(app.focus(), Some((1, "a".to_string())));
        app.cycle_node_filter(); // -> all
        assert_eq!(app.node_filter, None);
        assert_eq!(app.visible_calls().len(), 3);
    }

    #[test]
    fn node_down_keeps_last_seen_data_and_flips_health() {
        let mut app = two_node_app();
        app.update(Msg::Snapshot {
            node: 0,
            result: Err("connection refused".into()),
        });
        assert_eq!(
            app.nodes[0].health,
            NodeHealth::Down {
                error: "connection refused".into()
            }
        );
        // Last-seen calls are still there (rendered dimmed, not gone).
        assert_eq!(app.nodes[0].calls.len(), 2);
        assert_eq!(app.nodes_up(), 1);
    }

    #[test]
    fn selection_clamps_when_calls_shrink() {
        let mut app = two_node_app();
        app.select_last();
        assert_eq!(app.selected_call, 2);
        // Node 0's calls end; only node 1's remains.
        app.update(Msg::Snapshot {
            node: 0,
            result: Ok(snapshot(vec![])),
        });
        assert_eq!(app.selected_call, 0);
        assert_eq!(app.focus(), Some((1, "a".to_string())));
    }

    #[test]
    fn focus_change_resets_stats_pane() {
        let mut app = two_node_app();
        app.select_first();
        let stats = serde_json::json!({ "mos_estimate_avg": 4.2, "avg_jitter_ms": 11.5 });
        app.update(Msg::Stats {
            node: 0,
            call_id: "a".into(),
            stats,
        });
        assert_eq!(app.stats.mos_hist.back(), Some(&420));
        assert_eq!(app.stats.jitter_hist.back(), Some(&12));
        assert!(app.stats.latest.is_some());

        app.select_next(); // focus moved -> pane must reset
        assert!(app.stats.latest.is_none());
        assert!(app.stats.mos_hist.is_empty());

        // A stale stats message for the old focus is dropped.
        app.update(Msg::Stats {
            node: 0,
            call_id: "a".into(),
            stats: serde_json::json!({ "mos_estimate_avg": 1.0 }),
        });
        assert!(app.stats.latest.is_none());
    }

    #[test]
    fn tick_samples_fleet_total() {
        let mut app = two_node_app();
        app.update(Msg::Tick);
        assert_eq!(app.fleet_history.back(), Some(&3));
    }
}
