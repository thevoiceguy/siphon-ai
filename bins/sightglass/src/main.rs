//! sightglass — terminal operator console for siphon-ai fleets.
//!
//! A sight glass is the fitting on a pipe that lets you watch the
//! fluid moving through it. This is that for one or more running
//! siphon-ai nodes: a read-and-act client of each node's `[admin]`
//! HTTP listener, and nothing more — no SIP, no RTP, no daemon
//! coupling beyond the admin wire types. See DESIGN_SIGHTGLASS.md.

mod client;
mod config;
mod keys;
mod model;
mod poller;
mod ui;

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use clap::Parser;
use tokio::sync::{mpsc, watch};

use client::{AdminClient, ApiError};
use model::{Action, App, Msg, NodeId};
use ui::Theme;

#[derive(Debug, Parser)]
#[command(
    name = "sightglass",
    version,
    about = "Terminal operator console for siphon-ai nodes"
)]
pub struct Cli {
    /// Fleet config file (default: ~/.config/sightglass/config.toml).
    #[arg(long, conflicts_with = "target")]
    config: Option<std::path::PathBuf>,

    /// Ad-hoc single node: admin listener base URL
    /// (e.g. https://prod-1.example.com:9090). Token via --token-file
    /// or $SIGHTGLASS_TOKEN.
    #[arg(long)]
    target: Option<String>,

    /// Bearer-token file for --target. (Never a raw token argument —
    /// argv is visible in `ps`.)
    #[arg(long, requires = "target")]
    token_file: Option<std::path::PathBuf>,

    /// PEM CA bundle for --target when the admin TLS cert is
    /// privately signed.
    #[arg(long, requires = "target")]
    ca: Option<std::path::PathBuf>,

    /// Disable every mutating action client-side, regardless of token
    /// role — for NOC wall screens.
    #[arg(long)]
    read_only: bool,

    /// ASCII-only status glyphs (no Unicode dots).
    #[arg(long)]
    ascii: bool,
}

/// Wall-clock redraw cadence when nothing else arrives, and the
/// sampling cadence for the fleet-history sparkline.
const TICK: Duration = Duration::from_secs(1);

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    // Resolve config (and its errors) before raw mode: a bad fleet
    // file should print like a normal CLI failure, not corrupt a
    // half-initialized terminal.
    let fleet = config::load(&cli)?;
    let clients: Vec<Arc<AdminClient>> = fleet
        .nodes
        .iter()
        .map(|n| AdminClient::new(n).map(Arc::new))
        .collect::<Result<_>>()?;

    let (tx, rx) = mpsc::channel::<Msg>(64);
    let (focus_tx, focus_rx) = watch::channel::<Option<(NodeId, String)>>(None);
    for (id, client) in clients.iter().enumerate() {
        poller::spawn(
            id,
            clients.len(),
            client.clone(),
            fleet.poll_interval,
            tx.clone(),
            focus_rx.clone(),
        );
    }
    spawn_input_thread(tx.clone());

    // Learn each node's token role up front so action keys grey out
    // instead of 403ing (§5). Skipped in --read-only: no actions will
    // ever fire, so don't spend the probe (or its audit-log entries).
    if !cli.read_only {
        for (id, client) in clients.iter().enumerate() {
            let tx = tx.clone();
            let client = client.clone();
            tokio::spawn(async move {
                if let Some(role) = client.probe_role().await {
                    let _ = tx.send(Msg::RoleLearned { node: id, role }).await;
                }
            });
        }
    }

    let app = App::new(
        fleet.nodes.iter().map(|n| n.name.clone()).collect(),
        cli.read_only,
    );
    let theme = Theme::new(cli.ascii);

    let mut terminal = ratatui::init();
    let result = run(&mut terminal, app, &theme, rx, focus_tx, &clients, tx).await;
    ratatui::restore();
    result
}

async fn run(
    terminal: &mut ratatui::DefaultTerminal,
    mut app: App,
    theme: &Theme,
    mut rx: mpsc::Receiver<Msg>,
    focus_tx: watch::Sender<Option<(NodeId, String)>>,
    clients: &[Arc<AdminClient>],
    tx: mpsc::Sender<Msg>,
) -> Result<()> {
    let mut tick = tokio::time::interval(TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        terminal.draw(|f| ui::draw(f, &app, theme))?;
        tokio::select! {
            msg = rx.recv() => match msg {
                Some(Msg::Input(event)) => {
                    if let Some(action) = keys::handle(&mut app, &event) {
                        dispatch(action, &app, clients, &tx);
                    }
                }
                Some(msg) => app.update(msg),
                None => return Ok(()), // all producers gone
            },
            _ = tick.tick() => app.update(Msg::Tick),
        }
        // Tell the pollers which call's stats to fetch. send_if_modified
        // keeps the watch quiet when the focus didn't move.
        focus_tx.send_if_modified(|current| {
            let focus = app.focus();
            if *current == focus {
                false
            } else {
                *current = focus;
                true
            }
        });
        if app.should_quit {
            return Ok(());
        }
    }
}

/// Fire an action against its owning node's client on a background
/// task; the outcome comes back as a [`Msg::ActionOutcome`] toast.
/// The keymap already RBAC-gated the action — a 403 here means the
/// probe guessed high, and the outcome teaches the real ceiling.
fn dispatch(action: Action, app: &App, clients: &[Arc<AdminClient>], tx: &mpsc::Sender<Msg>) {
    let node = action.node();
    let Some(client) = clients.get(node).cloned() else {
        return;
    };
    let node_name = app.nodes[node].name.clone();
    let required = action.required_role();
    let verb = action.verb();
    let tx = tx.clone();
    tokio::spawn(async move {
        let (subject, result) = match &action {
            Action::Hangup { call_id, .. } => (call_id.clone(), client.hangup(call_id).await),
            Action::Park { call_id, .. } => (call_id.clone(), client.park(call_id).await),
            Action::Retrieve {
                call_id, ws_url, ..
            } => (
                call_id.clone(),
                client.retrieve(call_id, ws_url.as_deref()).await,
            ),
            Action::AddToConference {
                call_id, room_id, ..
            } => (
                format!("{call_id} → {room_id}"),
                client.add_to_conference(room_id, call_id).await,
            ),
            Action::Originate {
                to,
                gateway,
                ws_url,
                ..
            } => (
                format!("{to} via {gateway}"),
                client.originate(to, gateway, ws_url.as_deref()).await,
            ),
            Action::EndConference { room_id, .. } => {
                (room_id.clone(), client.end_conference(room_id).await)
            }
            Action::RemoveParticipant {
                room_id, call_id, ..
            } => (
                format!("{call_id} from {room_id}"),
                client.remove_participant(room_id, call_id).await,
            ),
            Action::SetLogFilter { directive, .. } => {
                (directive.clone(), client.set_log_filter(directive).await)
            }
            Action::HepProbe { .. } => ("probe".to_string(), client.hep_test().await),
            Action::StartDrain { .. } => ("graceful".to_string(), client.drain_start().await),
        };
        let msg = match result {
            Ok(detail) => Msg::ActionOutcome {
                node,
                ok: true,
                forbidden: false,
                required,
                text: format!("{verb} {subject} on {node_name}: {detail}"),
            },
            Err(ApiError::Forbidden) => Msg::ActionOutcome {
                node,
                ok: false,
                forbidden: true,
                required,
                text: format!("{verb} on {node_name}: forbidden (token role too low)"),
            },
            Err(ApiError::Other(detail)) => Msg::ActionOutcome {
                node,
                ok: false,
                forbidden: false,
                required,
                text: format!("{verb} {subject} on {node_name}: {detail}"),
            },
        };
        let _ = tx.send(msg).await;
    });
}

/// Terminal input on a plain thread: `crossterm::event::read` blocks,
/// which is fine here and keeps the async side purely channel-driven.
/// The thread ends with the process (the receiver hanging up just
/// makes it exit on the next event).
fn spawn_input_thread(tx: mpsc::Sender<Msg>) {
    std::thread::spawn(move || loop {
        match ratatui::crossterm::event::read() {
            Ok(event) => {
                if tx.blocking_send(Msg::Input(event)).is_err() {
                    return;
                }
            }
            Err(_) => return,
        }
    });
}
