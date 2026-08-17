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

/// Admin token role ladder, mirroring the daemon's `auth::Role`
/// (`readonly` < `operator` < `admin`). Learned per node by the
/// startup probe (`AdminClient::probe_role`) and downgraded lazily if
/// an action ever answers 403.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Role {
    ReadOnly,
    Operator,
    Admin,
}

/// An operator action against exactly one node (DESIGN_SIGHTGLASS.md
/// §5). Node-scoped by construction: the composite `(node, id)` key
/// travels with the action, so a dispatch can never act on the wrong
/// box.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Hangup {
        node: NodeId,
        call_id: String,
    },
    Park {
        node: NodeId,
        call_id: String,
    },
    Retrieve {
        node: NodeId,
        call_id: String,
        ws_url: Option<String>,
    },
    AddToConference {
        node: NodeId,
        call_id: String,
        room_id: String,
    },
    Originate {
        node: NodeId,
        to: String,
        gateway: String,
        ws_url: Option<String>,
    },
}

impl Action {
    pub fn node(&self) -> NodeId {
        match self {
            Action::Hangup { node, .. }
            | Action::Park { node, .. }
            | Action::Retrieve { node, .. }
            | Action::AddToConference { node, .. }
            | Action::Originate { node, .. } => *node,
        }
    }

    /// Minimum role per the daemon's endpoint→role table
    /// (`telemetry::auth::min_role`): call control is `operator`,
    /// origination is billable and needs `admin`.
    pub fn required_role(&self) -> Role {
        match self {
            Action::Originate { .. } => Role::Admin,
            _ => Role::Operator,
        }
    }

    /// Verb for toasts/labels.
    pub fn verb(&self) -> &'static str {
        match self {
            Action::Hangup { .. } => "hangup",
            Action::Park { .. } => "park",
            Action::Retrieve { .. } => "retrieve",
            Action::AddToConference { .. } => "conference-add",
            Action::Originate { .. } => "originate",
        }
    }
}

/// A modal blocking normal input: a yes/no confirm or a small form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Modal {
    Confirm {
        action: Action,
        /// Built at open time; always names the node ("… on prod-2?").
        prompt: String,
    },
    Input(InputModal),
}

/// Which action an input form builds on submit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputKind {
    Retrieve,
    AddToConference,
    Originate,
}

/// One text field of an input modal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Field {
    pub label: &'static str,
    pub value: String,
    pub required: bool,
}

impl Field {
    pub fn required(label: &'static str) -> Self {
        Self {
            label,
            value: String::new(),
            required: true,
        }
    }

    pub fn optional(label: &'static str) -> Self {
        Self {
            label,
            value: String::new(),
            required: false,
        }
    }
}

/// A small form modal (retrieve / conference-add / originate).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputModal {
    pub kind: InputKind,
    pub node: NodeId,
    /// The focused call the form acts on (absent for originate).
    pub call_id: Option<String>,
    pub fields: Vec<Field>,
    pub active: usize,
}

impl InputModal {
    pub fn title(&self, node_name: &str) -> String {
        match self.kind {
            InputKind::Retrieve => format!(" retrieve on {node_name} "),
            InputKind::AddToConference => format!(" add to conference on {node_name} "),
            InputKind::Originate => format!(" originate on {node_name} "),
        }
    }

    /// Build the action, or an error naming the first missing field.
    pub fn submit(&self) -> Result<Action, String> {
        if let Some(missing) = self
            .fields
            .iter()
            .find(|f| f.required && f.value.trim().is_empty())
        {
            return Err(format!("{} is required", missing.label));
        }
        let val = |i: usize| self.fields[i].value.trim().to_string();
        let opt = |i: usize| {
            let v = val(i);
            (!v.is_empty()).then_some(v)
        };
        Ok(match self.kind {
            InputKind::Retrieve => Action::Retrieve {
                node: self.node,
                call_id: self.call_id.clone().unwrap_or_default(),
                ws_url: opt(0),
            },
            InputKind::AddToConference => Action::AddToConference {
                node: self.node,
                call_id: self.call_id.clone().unwrap_or_default(),
                room_id: val(0),
            },
            InputKind::Originate => Action::Originate {
                node: self.node,
                to: val(0),
                gateway: val(1),
                ws_url: opt(2),
            },
        })
    }
}

/// Toast lifetime in ticks (1 s cadence).
const TOAST_TTL: u8 = 5;
/// At most this many toasts on screen; older ones are dropped first.
const TOAST_MAX: usize = 4;

/// A transient action-result notice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Toast {
    pub text: String,
    pub ok: bool,
    pub ttl: u8,
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
    /// This node's token role, from the startup probe. `None` until
    /// learned — actions stay enabled while unknown (a wrong guess
    /// surfaces as a 403 toast, which also teaches the ceiling).
    pub role: Option<Role>,
}

impl NodeState {
    fn new(name: String) -> Self {
        Self {
            name,
            health: NodeHealth::Connecting,
            calls: Vec::new(),
            registrations: Vec::new(),
            drain: None,
            role: None,
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
    /// The startup probe resolved a node's token role.
    RoleLearned { node: NodeId, role: Role },
    /// A dispatched action finished (2xx or not).
    ActionOutcome {
        node: NodeId,
        ok: bool,
        /// The action answered 403 — the token sits below
        /// `required`; teaches the role ceiling.
        forbidden: bool,
        required: Role,
        text: String,
    },
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
    /// Blocking modal, if any. While set, all keys route to it.
    pub modal: Option<Modal>,
    /// Transient action-result notices, oldest first.
    pub toasts: Vec<Toast>,
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
            modal: None,
            toasts: Vec::new(),
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
                // Age out toasts.
                for t in &mut self.toasts {
                    t.ttl = t.ttl.saturating_sub(1);
                }
                self.toasts.retain(|t| t.ttl > 0);
            }
            // Input is dispatched to `keys::handle` by the main loop.
            Msg::Input(_) => {}
            Msg::RoleLearned { node, role } => {
                if let Some(n) = self.nodes.get_mut(node) {
                    n.role = Some(role);
                }
            }
            Msg::ActionOutcome {
                node,
                ok,
                forbidden,
                required,
                text,
            } => {
                self.push_toast(text, ok);
                // A 403 teaches the ceiling: the token is below
                // `required`, so it is at most the role beneath it.
                if forbidden {
                    if let Some(n) = self.nodes.get_mut(node) {
                        let ceiling = match required {
                            Role::Admin => Role::Operator,
                            _ => Role::ReadOnly,
                        };
                        n.role = Some(match n.role {
                            Some(current) => current.min(ceiling),
                            None => ceiling,
                        });
                    }
                }
            }
        }
    }

    pub fn push_toast(&mut self, text: String, ok: bool) {
        if self.toasts.len() == TOAST_MAX {
            self.toasts.remove(0);
        }
        self.toasts.push(Toast {
            text,
            ok,
            ttl: TOAST_TTL,
        });
    }

    /// Whether an action needing `required` on `node` is currently
    /// permitted: `--read-only` beats everything; an unknown role is
    /// permissive (the 403 toast will teach it).
    pub fn can(&self, node: NodeId, required: Role) -> bool {
        if self.read_only {
            return false;
        }
        match self.nodes.get(node).and_then(|n| n.role) {
            Some(role) => role >= required,
            None => true,
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

    #[test]
    fn toasts_expire_after_ttl_ticks() {
        let mut app = two_node_app();
        app.push_toast("hello".into(), true);
        for _ in 0..TOAST_TTL - 1 {
            app.update(Msg::Tick);
        }
        assert_eq!(app.toasts.len(), 1);
        app.update(Msg::Tick);
        assert!(app.toasts.is_empty());
    }

    #[test]
    fn action_outcome_toasts_and_403_teaches_role_ceiling() {
        let mut app = two_node_app();
        app.update(Msg::RoleLearned {
            node: 0,
            role: Role::Admin,
        });
        assert!(app.can(0, Role::Admin));

        // A forbidden operator-level action drops the ceiling to
        // readonly and leaves an error toast.
        app.update(Msg::ActionOutcome {
            node: 0,
            ok: false,
            forbidden: true,
            required: Role::Operator,
            text: "hangup on prod-1: forbidden".into(),
        });
        assert_eq!(app.nodes[0].role, Some(Role::ReadOnly));
        assert!(!app.can(0, Role::Operator));
        assert!(app.toasts.iter().any(|t| !t.ok));

        // A forbidden admin-level action on a fresh node caps it at
        // operator.
        app.update(Msg::ActionOutcome {
            node: 1,
            ok: false,
            forbidden: true,
            required: Role::Admin,
            text: "originate on prod-2: forbidden".into(),
        });
        assert_eq!(app.nodes[1].role, Some(Role::Operator));
        assert!(app.can(1, Role::Operator));
    }

    #[test]
    fn read_only_beats_any_role() {
        let mut app = App::new(vec!["a".into()], true);
        app.update(Msg::RoleLearned {
            node: 0,
            role: Role::Admin,
        });
        assert!(!app.can(0, Role::Operator));
    }

    #[test]
    fn unknown_role_is_permissive() {
        let app = two_node_app();
        assert!(app.can(0, Role::Admin));
    }
}
