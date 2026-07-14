//! `CallController` — the per-call lifecycle owner.
//!
//! One controller = one task = one call. There is no global mutable
//! call state and no calls registry (CLAUDE.md §4.4). The
//! controller is constructed by whoever did the SDP/SIP-200-OK
//! dance (sip-glue's `CallAcceptor` impl, once that lands), and
//! handed everything it needs to run a call to completion: a
//! pre-attached [`MediaTap`], a [`StartMsg`] reflecting the
//! negotiated audio format, and a [`BridgeConfig`] pointing at the
//! WS server the matched route resolved to.
//!
//! ## What the controller owns
//!
//! ```text
//!     ┌─────────────────────── CallController task ───────────────────────┐
//!     │                                                                    │
//!     │   ┌───────────┐  caller audio (20 ms PCM16)  ┌─────────────────┐  │
//!     │   │ MediaTap  ├──────────────────────────────► bridge::run task│  │
//!     │   │   task    │                              │  (WS sink)     │  │
//!     │   │           │  playout audio               │                │  │
//!     │   │           ◄──────────────────────────────┤                │  │
//!     │   └───────────┘                              └────────────────┘  │
//!     │         ▲                                            ▲          │
//!     │         │                                            │          │
//!     │         └─── ctrl: Hangup, Clear, … ◄────────────────┘          │
//!     │                                                                  │
//!     │                          await termination                        │
//!     └──────────────────────────────────────────────────────────────────┘
//! ```
//!
//! ## State
//!
//! ```text
//!  Initializing ─► Connecting ─► Active ─► Terminating ─► Done
//! ```
//!
//! Every transition logs at `info` and is observable through the
//! [`CallOutcome`] returned by [`CallController::run`].
//!
//! ## Hot path
//!
//! Audio flows tap → bridge and bridge → tap through bounded mpsc
//! channels (capacity 10 frames = 200 ms). The controller does NOT
//! sit on the audio path; it only handles control-plane messages
//! (`Hangup`, `Clear`, `Mark`, `SendDtmf`, `Transfer`) and
//! sub-task termination. CLAUDE.md §4.3 ("audio hot path is
//! sacred") is preserved by routing audio directly between tap and
//! bridge tasks.
//!
//! ## What this PR does NOT do
//!
//! - No SDP negotiation (forge-side answer construction).
//! - No SIP 200 OK (the layer that built this controller already did it).
//! - No actual handling of `Clear`, `Mark`, `Transfer`, `SendDtmf`
//!   yet — they're logged. Hangup terminates the call.
//! - No CDR / webhook emission.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use siphon_ai_bridge::{
    connect_and_run, connect_and_run_with_ready, BridgeChannels, BridgeConfig, BridgeError,
    BridgeIn, CallId, DisconnectReason, ErrorCode, OutgoingEvent, StartMsg, StopReason,
};
use siphon_ai_media_glue::{
    AnnounceSource, MediaTap, MediaTapError, MohSource, QualityReport, QualitySummary,
    RoomMembership, TapCommand, TapDisconnect,
};

use crate::conference::{ConferenceError, ConferenceRegistry};
use crate::hold::HoldContext;
use crate::park::{ParkContext, ParkTimeoutAction};
use chrono::Utc;
use parking_lot::RwLock;
use siphon_ai_recording::{
    RecControl, RecEvent, RecFrame, RecordingError, RecordingSetup, RecordingStats, RecordingWriter,
};
use siphon_ai_webhooks::{
    CallParkedEvent, CallRetrievedEvent, ParkTimeoutEvent, WebhookEvent, WebhookSinkHandle,
    WEBHOOK_VERSION,
};
use thiserror::Error;
use tokio::sync::{mpsc, oneshot, Notify};
use tokio::task::JoinHandle;
use tracing::{debug, info, instrument, warn, Instrument};

use crate::transfer::{plan_refer, ReferPlan, TransferContext, TransferOutcome};
use siphon_ai_telemetry::{
    HOLDS_TOTAL, PARKED_CALLS_ACTIVE, PARKS_TOTAL, RETRIEVES_TOTAL, TRANSFERS_TOTAL,
    WS_RECONNECTS_TOTAL,
};

/// Bounded channel capacity for audio frames. 10 × 20 ms = 200 ms
/// of audio; per CLAUDE.md §6.2 audio channels are bounded for
/// roughly that span.
const AUDIO_CHANNEL_CAPACITY: usize = 10;

/// Tap-side channels staged during a reconnect redial (0.7.3) and handed
/// to the tap via [`TapCommand::Unpark`] once the new socket signals
/// ready: `(caller_audio_tx, playout_audio_rx, events_tx)`.
type PendingUnpark = (
    mpsc::Sender<Vec<u8>>,
    mpsc::Receiver<Vec<u8>>,
    mpsc::Sender<OutgoingEvent>,
);

/// Bounded capacity for the recording fork — generous (≈2 s) slack so the
/// writer keeps up; a full channel drops frames best-effort rather than
/// stalling the audio path (CLAUDE.md §4.3).
const RECORDING_CHANNEL_CAPACITY: usize = 100;

/// Bounded channel capacity for control-plane messages. Per
/// CLAUDE.md §6.2, control channels get small bounded buffers.
const CONTROL_CHANNEL_CAPACITY: usize = 8;

/// Inputs to one call. Construct via [`CallControllerConfig::new`].
pub struct CallControllerConfig {
    /// Bridge protocol's per-call id. Distinct from the SIP
    /// Call-ID — that one lives inside [`StartMsg::sip`].
    pub call_id: CallId,

    /// WS server selected by the matched route. The controller
    /// hands this to `bridge::connect_and_run` verbatim.
    pub bridge: BridgeConfig,

    /// First message on the WS. The controller forces `seq = 0`
    /// inside the bridge task per PROTOCOL.md §3, so callers don't
    /// need to set it.
    pub start: StartMsg,

    /// Pre-attached forge tap. The controller drives it.
    pub media_tap: MediaTap,

    /// Optional REFER handle. `None` in unit tests that don't
    /// exercise transfer; the daemon's [`BridgingAcceptor`] populates
    /// it for every accepted call when the daemon-wide
    /// `IntegratedUAC` is installed.
    pub transfer: Option<TransferContext>,

    /// Optional recording. `Some` when `[recording]` selects this call
    /// (e.g. `mode = "always"`); the controller spawns a per-call writer
    /// task, forks both audio legs to it via the tap, and finalizes the WAV
    /// at teardown. `None` → no recording.
    pub recording: Option<RecordingSetup>,

    /// Conference registry (0.7.0). `Some` when `[conference].enabled`;
    /// the controller routes `BridgeIn::ConferenceJoin` through it. `None`
    /// → conferencing off, and a join request is answered with
    /// `error { code: "conference_failed" }`. Cheap to clone (Arc inside).
    pub conference: Option<ConferenceRegistry>,

    /// Park context (0.7.0). `Some` when `[park].enabled`; the
    /// controller honours `BridgeIn::Park` and admin park/retrieve.
    /// `None` → park off, a park request answered with
    /// `error { code: "park_failed" }`. Carries the MOH/timeout
    /// settings + the daemon-wide `ParkRegistry`.
    pub park: Option<ParkContext>,

    /// Hold context (0.7.2). `Some` for inbound legs (the bot can hold
    /// its own caller via `BridgeIn::Hold` / `Resume`). `None` → a hold
    /// request is answered with `error { code: "hold_failed" }`. Carries
    /// the precomputed hold/resume re-INVITE offers + MOH file.
    pub hold: Option<HoldContext>,

    /// WS reconnect (0.7.3). When `true`, an unexpected WS drop keeps the
    /// caller on hold music and re-dials the same `ws_url` instead of
    /// tearing the call down (PROTOCOL.md §5.7). Resolved from
    /// `[bridge].ws_reconnect_enabled` + the route override. `false` =
    /// the v1 teardown.
    pub ws_reconnect_enabled: bool,
    /// Total reconnect window (`[bridge].ws_reconnect_max_secs`): how long
    /// the caller hears hold music before reconnect gives up and §5.7
    /// teardown runs. Only consulted when `ws_reconnect_enabled`.
    pub ws_reconnect_max: Duration,
    /// MOH file for the reconnect gap — the same shared `[media].moh_file`
    /// hold/park use. `None` → generated comfort silence.
    pub ws_reconnect_moh_file: Option<std::path::PathBuf>,
}

/// Where in its life a call is. State transitions are logged at
/// `info`; per CLAUDE.md §4.5, every transition is observable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallState {
    /// Sub-tasks not yet spawned.
    Initializing,
    /// Bridge task spawned; WS handshake in progress (or just
    /// completed — we transition synchronously through this state
    /// once both sub-tasks are spawned).
    Connecting,
    /// Both bridge and tap running; audio is flowing.
    Active,
    /// One sub-task ended or shutdown was requested; cleaning up.
    Terminating,
    /// Done; [`CallController::run`] has returned.
    Done,
}

/// What ended the call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallTermination {
    /// Server sent [`BridgeIn::Hangup`].
    ServerHangup,
    /// External signal via [`CallHandle::shutdown`].
    LocalShutdown,
    /// Force-terminated at the graceful-shutdown drain deadline
    /// (0.17.0) — a [`CallHandle::shutdown`] preceded by
    /// [`CallHandle::mark_drain_forced`]. Behaves like `LocalShutdown`
    /// (clean BYE + WS hangup) but is attributed distinctly on the CDR
    /// and `siphon_ai_calls_total`.
    DrainForced,
    /// The bridge sub-task ended first (clean WS close, server
    /// disconnect, or a bridge-side error).
    BridgeEnded,
    /// The media tap sub-task ended first (call media stopped, tap
    /// detached).
    TapEnded,
}

/// Summary of one completed call.
///
/// Fields preserve the underlying sub-task results so observers
/// (CDR, telemetry) can produce richer summaries from the same
/// outcome. A controller exit by external shutdown will leave the
/// bridge/tap results as `Some(Ok(...))` from the cooperative
/// teardown.
#[derive(Debug)]
pub struct CallOutcome {
    pub call_id: CallId,
    pub termination: CallTermination,
    pub bridge: Option<Result<DisconnectReason, BridgeError>>,
    pub tap: Option<Result<TapDisconnect, MediaTapError>>,
    /// Recording outcome, `Some` when the call was recorded (or recording was
    /// attempted). `None` when recording was off, or on-demand and never
    /// started. Feeds the CDR `recording_path` and the recordings metric.
    pub recording: Option<RecordingSummary>,
    /// Park outcome, `Some` when the call was parked at least once.
    /// Feeds the CDR `park { count, total_ms }`. `None` for a call that
    /// was never parked (the field is omitted from the CDR then).
    pub park: Option<ParkSummary>,
    /// Hold outcome (0.7.2), `Some` when the bot held its own caller at
    /// least once. Feeds the CDR `hold { count, total_ms }`. `None` for
    /// a call that was never bot-held (the field is omitted then).
    pub hold: Option<HoldSummary>,
    /// Reconnect outcome (0.7.3), `Some` when the WS dropped and
    /// reconnect ran at least once. Feeds the CDR
    /// `reconnect { count, total_gap_ms }`. `None` when the call never
    /// reconnected (the field is omitted then).
    pub reconnect: Option<ReconnectSummary>,
    /// Recording-consent audit trail (0.26.0), `Some` when an
    /// announcement played or the server reported consent. Feeds the CDR
    /// `consent { announced, announcement_ms, server }`.
    pub consent: Option<ConsentSummary>,
    /// Per-call quality summary (0.30.0), `Some` when the call produced
    /// any quality signal. Feeds the CDR `quality` block. `None` for
    /// calls that never went active (the field is omitted then).
    pub quality: Option<QualityOutcome>,
}

/// Per-call quality outcome surfaced on [`CallOutcome`] → CDR (0.30.0).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct QualityOutcome {
    /// Milliseconds from "WS `start` on the wire" to the first server
    /// audio frame reaching playout. `None` when the server never sent
    /// audio (or the bridge never connected).
    pub first_audio_out_ms: Option<u64>,
    /// Playout clears (`auto_clear` + server `clear`).
    pub barge_in_count: u32,
    /// Jitter / loss / RTT / RX / MOS aggregates from the tap's tracker.
    pub stats: QualitySummary,
}

impl QualityOutcome {
    /// Map the tap's live report + the bridge-connected epoch onto the
    /// outcome shape. `None` when the call never measured anything —
    /// the CDR omits the block and the record task emits nothing.
    /// Shared by the CDR build and the quality record task (0.31.0) so
    /// both feeds carry identical numbers.
    pub(crate) fn from_report(
        report: QualityReport,
        connected_at: Option<Instant>,
    ) -> Option<Self> {
        let measured = !report.stats.is_empty()
            || report.barge_in_count > 0
            || report.first_audio_at.is_some();
        measured.then(|| QualityOutcome {
            first_audio_out_ms: match (report.first_audio_at, connected_at) {
                (Some(first), Some(connected)) => {
                    Some(first.saturating_duration_since(connected).as_millis() as u64)
                }
                _ => None,
            },
            barge_in_count: report.barge_in_count,
            stats: report.stats,
        })
    }
}

/// Per-call quality history sampler (0.31.0). Emits one record per
/// `interval` while the call measures anything, plus a final record
/// when the tap winds down (its `quality_tx` drops → the watch
/// closes). Runs entirely off the audio path: it only reads two watch
/// channels at record cadence.
async fn quality_record_task(
    call_id: CallId,
    mut quality_rx: tokio::sync::watch::Receiver<QualityReport>,
    epoch_rx: tokio::sync::watch::Receiver<Option<Instant>>,
    interval: Duration,
) {
    let mut seq: u64 = 0;
    let mut tick = tokio::time::interval(interval);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    tick.tick().await; // consume the immediate first tick

    let emit = |kind: siphon_ai_quality::RecordKind,
                seq: u64,
                report: QualityReport,
                connected_at: Option<Instant>|
     -> bool {
        // Skip records with nothing measured (e.g. early ticks before
        // the first media-stats snapshot) — an empty record tells an
        // operator nothing a CDR's absence doesn't.
        let Some(outcome) = QualityOutcome::from_report(report, connected_at) else {
            return false;
        };
        siphon_ai_quality::emit(siphon_ai_quality::QualityRecord {
            version: siphon_ai_quality::QUALITY_RECORD_VERSION,
            kind,
            call_id: call_id.as_str().to_string(),
            ts: Utc::now(),
            seq,
            quality: crate::acceptor::quality_info(outcome),
        });
        true
    };

    loop {
        tokio::select! {
            _ = tick.tick() => {
                if emit(
                    siphon_ai_quality::RecordKind::Interval,
                    seq,
                    *quality_rx.borrow(),
                    *epoch_rx.borrow(),
                ) {
                    seq += 1;
                }
            }
            res = quality_rx.changed() => {
                match res {
                    // A fresh report — just mark it seen; the tick arm
                    // samples on cadence.
                    Ok(()) => {}
                    // Tap dropped its sender → call is winding down.
                    Err(_) => {
                        emit(
                            siphon_ai_quality::RecordKind::Final,
                            seq,
                            *quality_rx.borrow(),
                            *epoch_rx.borrow(),
                        );
                        break;
                    }
                }
            }
        }
    }
}

/// Recording-consent audit trail surfaced on [`CallOutcome`] → CDR
/// (0.26.0). `announced`/`announcement_ms` come from the daemon-played
/// announcement; `server` is whatever the WS server reported via
/// `set_recording_consent`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ConsentSummary {
    pub announced: bool,
    pub announcement_ms: u64,
    pub server: Option<String>,
}

/// Per-call park outcome surfaced on [`CallOutcome`] → CDR.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParkSummary {
    /// How many times this call was parked over its lifetime.
    pub count: u32,
    /// Cumulative wall-time the call spent parked, in milliseconds.
    pub total_ms: u64,
}

/// Per-call bot-hold outcome surfaced on [`CallOutcome`] → CDR (0.7.2).
/// Counts only bot-initiated holds; a far-end hold isn't tallied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HoldSummary {
    /// How many times the bot held this call over its lifetime.
    pub count: u32,
    /// Cumulative wall-time the call spent bot-held, in milliseconds.
    pub total_ms: u64,
}

/// Per-call WS-reconnect outcome surfaced on [`CallOutcome`] → CDR
/// (0.7.3). An episode is one unexpected drop that entered the reconnect
/// path (whether it recovered or was exhausted).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReconnectSummary {
    /// How many reconnect episodes occurred over the call's lifetime.
    pub count: u32,
    /// Cumulative wall-time the call spent on reconnect hold music, in
    /// milliseconds.
    pub total_gap_ms: u64,
}

/// Per-call recording outcome surfaced on [`CallOutcome`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordingSummary {
    pub path: std::path::PathBuf,
    pub result: RecordingResult,
    /// Sealed at rest under `[recording.encryption]` (0.24.0).
    pub encrypted: bool,
}

/// How a recording finished — the `result` label on `siphon_ai_recordings_total`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordingResult {
    /// Written cleanly.
    Ok,
    /// Some 20 ms frames were dropped under writer back-pressure — the file
    /// is short, not corrupt.
    Degraded,
    /// An I/O error; the recording is incomplete or absent.
    Failed,
}

impl RecordingResult {
    pub fn as_str(self) -> &'static str {
        match self {
            RecordingResult::Ok => "ok",
            RecordingResult::Degraded => "degraded",
            RecordingResult::Failed => "failed",
        }
    }
}

#[derive(Debug, Error)]
pub enum CallError {
    /// Couldn't even spawn the bridge — channel allocation
    /// failure, etc. (Rare; mostly defensive.)
    #[error("controller setup failed: {0}")]
    Setup(String),

    /// A sub-task panicked. Surfaced as a [`tokio::task::JoinError`].
    #[error("sub-task crashed: {0}")]
    TaskJoin(#[from] tokio::task::JoinError),
}

/// External handle to a running [`CallController`].
///
/// Cheap to clone (Arc-of-Notify under the hood) so multiple
/// controllers (the SIP-side BYE handler, the admin force-hangup
/// endpoint, signal handlers) can all request the same shutdown.
#[derive(Debug, Clone)]
pub struct CallHandle {
    notify: std::sync::Arc<Notify>,
    call_id: CallId,
    /// Set when the peer drove teardown by sending a BYE on the
    /// confirmed dialog. The acceptor's post-controller cleanup
    /// consults this to decide whether to send an outbound BYE —
    /// without it, a WS-side `hangup` would leave the SIP leg up
    /// because the controller only stops the WS/tap path.
    remote_bye: std::sync::Arc<AtomicBool>,
    /// External-event push channel. The acceptor's `on_reinvite`
    /// uses this to surface mid-dialog state changes (currently
    /// `Hold` / `Resume`) to the WS bridge without going through
    /// the controller's main select loop.
    ///
    /// Behind a `Mutex` so a **retrieve** (0.7.0 park) can swap it to
    /// the fresh bridge's sender — the original bridge is gone after a
    /// park, and `push_bridge_event` must reach whichever bridge is
    /// current. This is control-plane only (re-INVITE events, rare),
    /// never the per-frame audio path, so the lock doesn't violate
    /// §4.3. `parking_lot::Mutex` (a core dep already) rather than a
    /// new arc-swap dep.
    bridge_events_tx: std::sync::Arc<RwLock<mpsc::Sender<OutgoingEvent>>>,
    /// Cross-call conference control (0.7.0). The admin API
    /// (`/admin/v1/conferences/:id/participants`) signals a call to
    /// join/leave a room *on its behalf* by pushing here — the
    /// controller runs the exact same join/leave path as a WS-driven
    /// `conference_join`, so §4.4 holds (we signal the call; the
    /// controller mutates its own state). Bounded + `try_send`.
    conf_cmd_tx: mpsc::Sender<ConferenceCommand>,
    /// Admin park/retrieve control (0.7.0). Same §4.4 stance as
    /// `conf_cmd_tx`: the admin API signals; the controller acts on
    /// its own state.
    park_cmd_tx: mpsc::Sender<ParkCommand>,
    /// Set while the **far end** has us on hold (0.7.2). The acceptor's
    /// `on_reinvite` flips it on the peer's sendonly/inactive →
    /// sendrecv transitions; the controller reads it to reject a
    /// *bot*-initiated hold while peer-held (first-cut policy — no hold
    /// stacking). Atomic so the SIP-dispatch side and the controller's
    /// own loop can touch it without a lock (§4.4: per-call state on the
    /// handle, not a global registry).
    peer_held: std::sync::Arc<AtomicBool>,
    /// Set just before the graceful-shutdown drain force-terminates
    /// this call (0.17.0). The controller reads it when its `shutdown`
    /// notify fires to attribute the termination as `DrainForced`
    /// rather than the generic `LocalShutdown`, so the CDR / metrics
    /// distinguish "ended by a deploy's drain deadline" from an admin
    /// force-hangup. Atomic, same §4.4 rationale as `remote_bye`: a
    /// fire-and-forget flag on the handle, not shared mutable state.
    drain_forced: std::sync::Arc<AtomicBool>,
}

/// A cross-call conference request pushed onto a [`CallHandle`] by the
/// admin API. Mirrors the self-scoped WS `conference_join` /
/// `conference_leave`, but initiated by an operator against *another*
/// call.
#[derive(Debug, Clone)]
pub enum ConferenceCommand {
    /// Join (or move to) the named room.
    Join { room_id: String },
    /// Leave whatever room the call is in.
    Leave,
}

/// An admin park/retrieve request pushed onto a [`CallHandle`] (0.7.0).
/// WS-initiated park comes via `BridgeIn::Park` instead; retrieve is
/// operator-only, so it has no WS variant.
#[derive(Debug, Clone)]
pub enum ParkCommand {
    /// Park this call (optionally tagging a hold lot).
    Park { slot: Option<String> },
    /// Retrieve this call onto a fresh WS session (optionally
    /// redirecting to a different `ws_url`).
    Retrieve { ws_url: Option<String> },
}

impl CallHandle {
    fn new(
        call_id: CallId,
        bridge_events_tx: mpsc::Sender<OutgoingEvent>,
        conf_cmd_tx: mpsc::Sender<ConferenceCommand>,
        park_cmd_tx: mpsc::Sender<ParkCommand>,
    ) -> Self {
        Self {
            notify: std::sync::Arc::new(Notify::new()),
            call_id,
            remote_bye: std::sync::Arc::new(AtomicBool::new(false)),
            bridge_events_tx: std::sync::Arc::new(RwLock::new(bridge_events_tx)),
            conf_cmd_tx,
            park_cmd_tx,
            peer_held: std::sync::Arc::new(AtomicBool::new(false)),
            drain_forced: std::sync::Arc::new(AtomicBool::new(false)),
        }
    }

    /// Record whether the far end currently has us on hold (0.7.2).
    /// Called by the acceptor's `on_reinvite` on peer hold/resume
    /// transitions.
    pub fn set_peer_held(&self, held: bool) {
        self.peer_held.store(held, Ordering::Release);
    }

    /// Whether the far end currently has us on hold.
    pub fn peer_held(&self) -> bool {
        self.peer_held.load(Ordering::Acquire)
    }

    /// Ask the controller to shut down cleanly. The controller
    /// will drain audio briefly, send `stop` over the bridge, and
    /// return.
    pub fn shutdown(&self) {
        self.notify.notify_one();
    }

    /// Record that the SIP peer has sent (or implicitly already
    /// completed) the BYE leg of teardown. Callers that initiate
    /// teardown locally (e.g., WS server `hangup`, admin force-
    /// hangup) leave this `false` so the acceptor knows it still
    /// owes the peer an outbound BYE.
    pub fn mark_remote_bye(&self) {
        self.remote_bye.store(true, Ordering::Release);
    }

    pub fn remote_bye_received(&self) -> bool {
        self.remote_bye.load(Ordering::Acquire)
    }

    /// Flag that this call is being force-terminated by the
    /// graceful-shutdown drain (0.17.0). Call this *before*
    /// [`Self::shutdown`] so the controller's teardown attributes the
    /// cause as `DrainForced`. Daemon-initiated, so `remote_bye` stays
    /// `false` and the acceptor still sends the outbound BYE.
    pub fn mark_drain_forced(&self) {
        self.drain_forced.store(true, Ordering::Release);
    }

    pub fn drain_forced(&self) -> bool {
        self.drain_forced.load(Ordering::Acquire)
    }

    pub fn call_id(&self) -> &CallId {
        &self.call_id
    }

    /// Push an event to the WS bridge from outside the controller's
    /// own select loop. Used by the acceptor's `on_reinvite` to
    /// emit `Hold` / `Resume` events when peer direction changes
    /// without having to route through the controller. `try_send`
    /// rather than `send` so a backed-up control channel (which
    /// means the bridge is in trouble) doesn't block the SIP
    /// dispatch thread. Drop with a warn — re-INVITE events are
    /// informational and a missed Hold/Resume isn't fatal.
    pub fn push_bridge_event(&self, event: OutgoingEvent) {
        if let Err(e) = self.bridge_events_tx.read().try_send(event) {
            tracing::warn!(
                call_id = %self.call_id,
                error = %e,
                "bridge_events_tx full or closed; dropping external event"
            );
        }
    }

    /// Swap the external-event sender to a retrieved call's fresh
    /// bridge (0.7.0). Called by the controller during retrieve so
    /// `push_bridge_event` reaches the new WS session.
    fn swap_bridge_sender(&self, sender: mpsc::Sender<OutgoingEvent>) {
        *self.bridge_events_tx.write() = sender;
    }

    /// A clone of the current external-event sender — the controller's
    /// own send path, kept in sync with the swappable bridge.
    fn bridge_sender(&self) -> mpsc::Sender<OutgoingEvent> {
        self.bridge_events_tx.read().clone()
    }

    /// Ask this call to park (admin). The controller runs the same
    /// path as a WS `park`. Best-effort `try_send`.
    pub fn request_park(&self, slot: Option<String>) {
        if let Err(e) = self.park_cmd_tx.try_send(ParkCommand::Park { slot }) {
            tracing::warn!(
                call_id = %self.call_id,
                error = %e,
                "park_cmd_tx full or closed; dropping park request"
            );
        }
    }

    /// Ask this call to retrieve onto a fresh WS session (admin),
    /// optionally redirecting to `ws_url`. No-op at the controller if
    /// the call isn't parked.
    pub fn request_retrieve(&self, ws_url: Option<String>) {
        if let Err(e) = self.park_cmd_tx.try_send(ParkCommand::Retrieve { ws_url }) {
            tracing::warn!(
                call_id = %self.call_id,
                error = %e,
                "park_cmd_tx full or closed; dropping retrieve request"
            );
        }
    }

    /// Ask this call to join `room_id` (admin cross-call add). The
    /// controller runs the same path as a WS `conference_join`; the
    /// result surfaces on this call's WS (`conference_joined` or
    /// `error{conference_failed}`). Best-effort `try_send`.
    pub fn request_conference_join(&self, room_id: impl Into<String>) {
        let room_id = room_id.into();
        if let Err(e) = self
            .conf_cmd_tx
            .try_send(ConferenceCommand::Join { room_id })
        {
            tracing::warn!(
                call_id = %self.call_id,
                error = %e,
                "conf_cmd_tx full or closed; dropping conference join request"
            );
        }
    }

    /// Ask this call to leave its conference room (admin cross-call
    /// remove). No-op at the controller if the call isn't in a room.
    pub fn request_conference_leave(&self) {
        if let Err(e) = self.conf_cmd_tx.try_send(ConferenceCommand::Leave) {
            tracing::warn!(
                call_id = %self.call_id,
                error = %e,
                "conf_cmd_tx full or closed; dropping conference leave request"
            );
        }
    }
}

/// The controller itself.
pub struct CallController {
    cfg: CallControllerConfig,
    handle: CallHandle,
    /// Receiver side of the OutgoingEvent channel whose sender
    /// is on the handle. Lives here until `run()` hands it to the
    /// bridge task.
    control_out_rx: mpsc::Receiver<OutgoingEvent>,
    /// Receiver for admin cross-call conference commands. Sender is on
    /// the handle. Consumed by `run()`.
    conf_cmd_rx: mpsc::Receiver<ConferenceCommand>,
    /// Receiver for admin park/retrieve commands. Sender on the handle.
    park_cmd_rx: mpsc::Receiver<ParkCommand>,
}

impl CallController {
    /// Construct a controller. Returns it together with a
    /// [`CallHandle`] the spawner can use to signal shutdown.
    pub fn new(cfg: CallControllerConfig) -> (Self, CallHandle) {
        let (control_out_tx, control_out_rx) =
            mpsc::channel::<OutgoingEvent>(CONTROL_CHANNEL_CAPACITY);
        let (conf_cmd_tx, conf_cmd_rx) =
            mpsc::channel::<ConferenceCommand>(CONTROL_CHANNEL_CAPACITY);
        let (park_cmd_tx, park_cmd_rx) = mpsc::channel::<ParkCommand>(CONTROL_CHANNEL_CAPACITY);
        let handle = CallHandle::new(
            cfg.call_id.clone(),
            control_out_tx,
            conf_cmd_tx,
            park_cmd_tx,
        );
        (
            Self {
                cfg,
                handle: handle.clone(),
                control_out_rx,
                conf_cmd_rx,
                park_cmd_rx,
            },
            handle,
        )
    }

    /// Attach the inbound connection this call's dialog rides on to the
    /// transfer **and** hold contexts, so an in-dialog REFER (#159) or a
    /// bot-initiated hold/resume re-INVITE (0.7.2) reuses it instead of
    /// dialing the peer's Contact (TCP/TLS dialogs). Called by `run_call`
    /// after accept, when the flow is known — the contexts themselves are
    /// built earlier, in `prepare_call`. No-op when neither is installed
    /// or `flow` is `None`.
    pub fn attach_transfer_flow(&mut self, flow: Option<crate::acceptor::DialogFlow>) {
        if let Some(transfer) = self.cfg.transfer.as_mut() {
            transfer.control.flow = flow.clone();
        }
        if let Some(hold) = self.cfg.hold.as_mut() {
            hold.control.flow = flow;
        }
    }

    pub fn handle(&self) -> &CallHandle {
        &self.handle
    }

    /// Run the call to completion.
    ///
    /// Returns when all sub-tasks have terminated. The returned
    /// [`CallOutcome`] is the source of truth for what happened.
    #[instrument(skip(self), fields(
        call_id = %self.cfg.call_id,
        sip_call_id = %self.cfg.start.sip.call_id,
        direction = ?self.cfg.start.direction,
        from = %self.cfg.start.from,
        to = %self.cfg.start.to,
    ))]
    pub async fn run(self) -> Result<CallOutcome, CallError> {
        let CallController {
            cfg,
            handle,
            control_out_rx,
            mut conf_cmd_rx,
            mut park_cmd_rx,
        } = self;
        let CallControllerConfig {
            call_id,
            bridge,
            mut start,
            media_tap,
            transfer,
            recording,
            conference,
            park,
            hold,
            ws_reconnect_enabled,
            ws_reconnect_max,
            ws_reconnect_moh_file,
        } = cfg;

        // W3C trace propagation (0.23.0): render this `run` span — the
        // call's trace root — as `traceparent`/`tracestate` and stamp it
        // onto `start`; the bridge sends it as upgrade headers and the
        // additive `start.trace_context` field. `None` (field absent, no
        // headers) when `[observability.otlp]` is disabled — the default.
        start.trace_context = siphon_ai_telemetry::otel::current_trace_context().map(|h| {
            siphon_ai_bridge::TraceContext {
                traceparent: h.traceparent,
                tracestate: h.tracestate,
            }
        });

        // Park needs to rebuild the WS bridge on retrieve from the
        // original call facts + bridge config; clone them before the
        // first bridge consumes the originals — `start_template` cloned
        // *after* the trace-context stamp so retrieve/reconnect sessions
        // stay in the same trace.
        let start_template = start.clone();
        let bridge_template = bridge.clone();

        // Captured before `media_tap` moves into its task — a room
        // locks to its first joiner's negotiated rate, and a join is
        // rejected at any other rate (no resampling in 0.7.0).
        let call_sample_rate = media_tap.sample_rate();

        log_state(&call_id, CallState::Initializing);

        // ─── Channels ────────────────────────────────────────────
        // tap → bridge: 20 ms PCM16 frames from caller
        let (caller_audio_tx, caller_audio_rx) = mpsc::channel::<Vec<u8>>(AUDIO_CHANNEL_CAPACITY);
        // bridge → tap: 20 ms PCM16 frames from server (playout)
        let (playout_audio_tx, playout_audio_rx) = mpsc::channel::<Vec<u8>>(AUDIO_CHANNEL_CAPACITY);
        // controller → bridge: outgoing events (Stop, Mark, …). The
        // sender lives on the CallHandle so external callers
        // (acceptor's on_reinvite) can push too. `mut` because a
        // retrieve (park) swaps it to the fresh bridge's sender.
        let mut control_out_tx = handle.bridge_sender();
        // bridge → controller: BridgeIn from server. `mut` so a
        // retrieve can swap in the fresh bridge's receiver.
        let (control_in_tx, mut control_in_rx) =
            mpsc::channel::<BridgeIn>(CONTROL_CHANNEL_CAPACITY);
        // controller → tap: out-of-band commands the controller routes
        // from BridgeIn (currently SendDtmf; future Clear / Mark).
        let (tap_cmd_tx, tap_cmd_rx) = mpsc::channel::<TapCommand>(CONTROL_CHANNEL_CAPACITY);
        // transfer task → controller: REFER outcome. Single-shot per
        // call (`Transfer` ends the call on success); cap = 1 keeps
        // back-pressure simple if a second BridgeIn::Transfer arrives
        // while the first is still in flight.
        let (transfer_result_tx, mut transfer_result_rx) = mpsc::channel::<TransferOutcome>(1);
        // conference join task → controller: the outcome of an async
        // `ConferenceRegistry::join`. Spawned (like REFER) so the
        // round-trip to the room task never blocks the control loop.
        // Cap a few deep so back-to-back joins (e.g. room-switch)
        // don't drop a result.
        let (conf_join_tx, mut conf_join_rx) =
            mpsc::channel::<Result<RoomMembership, ConferenceError>>(4);

        let channels = BridgeChannels {
            audio_out_rx: caller_audio_rx,
            control_out_rx,
            audio_in_tx: playout_audio_tx,
            control_in_tx,
        };

        // ─── Spawn sub-tasks ─────────────────────────────────────
        log_state(&call_id, CallState::Connecting);

        // Recording (optional). Spawn the per-call writer task and fork both
        // legs to it via the tap. The writer does its file I/O off the audio
        // path; the tap only `try_send`s copies (§4.3). `media_tap` is
        // rebound with the fork sender installed before it's spawned. The
        // controller drives the writer's state machine over `rec_ctrl_tx`
        // (from `BridgeIn` recording messages) and ferries the writer's
        // lifecycle events back out via `rec_evt_rx`.
        let mut recording_task: Option<JoinHandle<Result<Option<RecordingStats>, RecordingError>>> =
            None;
        let mut rec_ctrl_tx: Option<mpsc::Sender<RecControl>> = None;
        let mut rec_evt_rx: Option<mpsc::Receiver<RecEvent>> = None;
        // Dropped-frame counter shared with the tap: incremented when the
        // recording channel is full (back-pressure) → classifies the
        // recording `degraded`. `None` when recording is off for the call.
        let mut rec_drops: Option<Arc<AtomicU64>> = None;
        let mut rec_path: Option<std::path::PathBuf> = None;
        // Announcement gating (0.26.0): the prompt source (sent to the
        // tap once it's spawned), its completion channel, whether
        // capture should start when it finishes (mode=always, or a
        // server start_recording that arrived mid-prompt), and the
        // played duration for the CDR consent stamp. `rec_blocked`
        // fail-closes recording when the prompt can't play.
        let mut announce_source: Option<Box<AnnounceSource>> = None;
        let mut announce_done_rx: Option<tokio::sync::oneshot::Receiver<u64>> = None;
        let mut announce_start_after = false;
        let mut rec_blocked = false;
        let mut announced_ms: Option<u64> = None;
        // Whether this call's recording is sealed at rest (0.24.0) — feeds
        // the CDR `recording_encrypted` flag.
        let mut rec_encrypted = false;
        // One recording per call in this revision → recording_id = call_id.
        let recording_id = call_id.as_str().to_string();
        // Quality feed for the CDR `quality` block (0.30.0): the tap
        // publishes the latest whole-call state; we read it once at
        // outcome build. Watch semantics — no backpressure, no loss
        // that matters.
        let (quality_tx, quality_rx) = tokio::sync::watch::channel(QualityReport::default());
        let media_tap = media_tap.with_quality_watch(quality_tx);
        let media_tap = if let Some(setup) = recording {
            let (rec_tx, rec_rx) = mpsc::channel::<RecFrame>(RECORDING_CHANNEL_CAPACITY);
            let (ctrl_tx, ctrl_rx) = mpsc::channel::<RecControl>(CONTROL_CHANNEL_CAPACITY);
            let (evt_tx, evt_rx) = mpsc::channel::<RecEvent>(CONTROL_CHANNEL_CAPACITY);
            let drops = Arc::new(AtomicU64::new(0));
            rec_path = Some(setup.path.clone());
            rec_encrypted = setup.encryption.is_some();
            // Announcement gating (0.26.0): with a prompt configured the
            // writer starts idle regardless of mode — capture begins only
            // when the tap reports the prompt finished. An unusable
            // prompt file FAIL-CLOSES recording for this call (capturing
            // without the compliance announcement is worse than not
            // capturing; the CDR consent stamp shows announced=false).
            let mut auto_start = setup.auto_start;
            if let Some(file) = &setup.announcement {
                match AnnounceSource::new(file, media_tap.sample_rate()) {
                    Ok(src) => {
                        announce_source = Some(Box::new(src));
                        announce_start_after = auto_start;
                        auto_start = false;
                    }
                    Err(err) => {
                        warn!(
                            call_id = %call_id,
                            file = %file.display(),
                            error = %err,
                            "recording announcement unusable; recording fail-closed for this call"
                        );
                        auto_start = false;
                        rec_blocked = true;
                    }
                }
            }
            let writer = RecordingWriter::new(setup.path, media_tap.sample_rate(), auto_start)
                .with_encryption(setup.encryption)
                .with_format(setup.format);
            recording_task = Some(tokio::spawn(
                writer
                    .run(rec_rx, ctrl_rx, evt_tx)
                    .instrument(tracing::Span::current()),
            ));
            rec_ctrl_tx = Some(ctrl_tx);
            rec_evt_rx = Some(evt_rx);
            rec_drops = Some(Arc::clone(&drops));
            media_tap.with_recording(Some((rec_tx, drops)))
        } else {
            media_tap
        };

        // Spawned tasks don't inherit the current span, so a call would
        // otherwise fragment into sibling traces. Instrument each with the
        // controller's `run` span so the WS-bridge, media-tap, and recording
        // spans nest under it — one trace per call in OTLP (0.22.0).
        // Readiness carries the "start on the wire" Instant — the epoch
        // for `first_audio_out_ms` (0.30.0). A tiny forwarder fans the
        // oneshot into a watch so both the CDR build (below) and the
        // quality record task (0.31.0) can read it without racing over
        // a single consumer.
        let (bridge_ready_tx, bridge_ready_rx) = oneshot::channel();
        let (epoch_tx, epoch_rx) = tokio::sync::watch::channel(None::<Instant>);
        tokio::spawn(async move {
            if let Ok(connected_at) = bridge_ready_rx.await {
                let _ = epoch_tx.send(Some(connected_at));
            }
            // Bridge died before ready → epoch stays None; forwarder ends.
        });
        // Live-stats registration (0.31.0): expose this call's quality
        // feed to `GET /admin/v1/calls/{id}/stats` for the call's
        // lifetime. RAII — dropping the guard on any exit path from
        // `run` deregisters.
        let _quality_live_guard = crate::quality_live::LiveQualityGuard::register(
            call_id.as_str(),
            quality_rx.clone(),
            epoch_rx.clone(),
        );
        // Quality history records (0.31.0): when `[quality]` is
        // configured, sample the tap's quality feed on the configured
        // cadence + emit a final record at teardown. The task ends
        // itself when the tap drops its `quality_tx` (watch closes), so
        // its lifetime is bounded by the call — no JoinHandle needed.
        if let Some(record_interval) = siphon_ai_quality::record_interval() {
            tokio::spawn(
                quality_record_task(
                    call_id.clone(),
                    quality_rx.clone(),
                    epoch_rx.clone(),
                    record_interval,
                )
                .instrument(tracing::Span::current()),
            );
        }
        let mut bridge_task: JoinHandle<Result<DisconnectReason, BridgeError>> = tokio::spawn(
            connect_and_run_with_ready(bridge, start, channels, Some(bridge_ready_tx))
                .instrument(tracing::Span::current()),
        );
        // The tap forwards out-of-band events (currently DTMF) onto
        // the same control stream the bridge reads. Cloning the
        // sender means tap and controller are independent producers
        // — bridge teardown closes the receiver, both producers see
        // the same EOF.
        let mut tap_task: JoinHandle<Result<TapDisconnect, MediaTapError>> = tokio::spawn(
            media_tap
                .run(
                    caller_audio_tx,
                    playout_audio_rx,
                    control_out_tx.clone(),
                    tap_cmd_rx,
                )
                .instrument(tracing::Span::current()),
        );

        // We don't wait for the bridge handshake to declare
        // Active; the moment both tasks are spawned, audio plumbing
        // is in place. A late handshake error surfaces as the
        // bridge task ending early, which the select! picks up.
        log_state(&call_id, CallState::Active);

        // Kick off the recording announcement (0.26.0). The prompt plays
        // to the caller while the WS session comes up in parallel
        // (announce-then-bridge); the tap reports completion on the
        // oneshot, which gates the recording Start below.
        if let Some(source) = announce_source.take() {
            let (done_tx, done_rx) = tokio::sync::oneshot::channel();
            match tap_cmd_tx.try_send(TapCommand::Announce {
                source,
                done: done_tx,
            }) {
                Ok(()) => announce_done_rx = Some(done_rx),
                Err(e) => {
                    warn!(call_id = %call_id, error = %e,
                          "could not start announcement; recording fail-closed");
                    rec_blocked = true;
                }
            }
        }

        // ─── Main loop ───────────────────────────────────────────
        let termination: CallTermination;
        let mut bridge_result: Option<Result<DisconnectReason, BridgeError>> = None;
        let mut tap_result: Option<Result<TapDisconnect, MediaTapError>> = None;

        // Once the bridge task drops `control_in_tx` (e.g., on WS
        // close or connect error), `control_in_rx.recv()` returns
        // None immediately and keeps doing so. Without this guard,
        // a `biased` select! would spin on that arm and starve the
        // bridge_task arm. The flag flips to false on first None
        // and the arm becomes unselectable until we exit the loop.
        let mut control_open = true;

        // ─── Park state (0.7.0 §2.4) ─────────────────────────────
        // `parked` true between a park and its retrieve/teardown. While
        // parked the tap plays MOH and the WS bridge is gone (its
        // `bridge_task` completed) — so the bridge-end arm is guarded by
        // `bridge_alive` and made non-terminal. Retrieve spawns a fresh
        // bridge and flips both back.
        let mut parked = false;
        let mut bridge_alive = true;
        // CDR `park { count, total_ms }` accounting.
        let mut park_count: u32 = 0;
        let mut park_total = Duration::ZERO;
        let mut parked_since: Option<Instant> = None;
        // Park timeout: a pinned far-future sleep, reset on park, only
        // selected while `parked` AND a deadline is set (same pattern as
        // the tap's RTP watchdog).
        let park_timeout_sleep = tokio::time::sleep(Duration::from_secs(86_400));
        tokio::pin!(park_timeout_sleep);
        let mut park_deadline_armed = false;

        // ─── Hold state (0.7.2) ──────────────────────────────────
        // `held` true between a bot-initiated `Hold` and its `Resume`.
        // While held the tap plays MOH to the caller and drops the
        // caller↔WS bridge in both directions, but the WS session stays
        // attached (no durable-task rework — that's park's job). No hold
        // timeout: an abandoned hold is handled by the WS disconnecting.
        let mut held = false;
        // CDR `hold { count, total_ms }` accounting (mirrors park).
        let mut hold_count: u32 = 0;
        let mut hold_total = Duration::ZERO;
        // Recording-consent audit trail (0.26.0): the server-reported
        // note; the daemon-announcement half is stamped where the
        // announcement completes.
        let mut consent_server: Option<String> = None;
        let mut held_since: Option<Instant> = None;
        // Re-INVITE glare backoff (RFC 3261 §14.1): if the peer sends
        // its own re-INVITE at the same moment, our offer draws a 491 —
        // we wait this long, then retry once.
        const HOLD_GLARE_BACKOFF: Duration = Duration::from_millis(250);

        // ─── Reconnect state (0.7.3 §6) ──────────────────────────
        // `reconnecting` true between an eligible WS drop and a
        // successful redial (or the deadline). While reconnecting the
        // tap plays MOH (reusing `TapCommand::Park`) and `bridge_alive`
        // is false until the backoff timer spawns a fresh redial. The
        // redial's readiness signal (`reconnect_ready_rx`) drives the
        // `Unpark`, so MOH only drops once the new socket is live; the
        // deadline ends the whole effort and falls through to §5.7.
        let mut reconnecting = false;
        let mut reconnect_attempt: u32 = 0;
        let mut reconnect_since: Option<Instant> = None;
        // CDR `reconnect { count, total_gap_ms }` accounting (mirrors park).
        // `count` = reconnect episodes (one per eligible drop that entered
        // the reconnect path); `total` = cumulative time on reconnect MOH.
        let mut reconnect_count: u32 = 0;
        let mut reconnect_total = Duration::ZERO;
        // Tap-side channels of the in-flight redial, handed to the tap
        // on `ready` (not at dial) so a redial that fails fast doesn't
        // flap the caller's audio.
        let mut pending_unpark: Option<PendingUnpark> = None;
        let mut reconnect_ready_rx: Option<oneshot::Receiver<std::time::Instant>> = None;
        let reconnect_backoff_sleep = tokio::time::sleep(Duration::from_secs(86_400));
        tokio::pin!(reconnect_backoff_sleep);
        let mut reconnect_backoff_armed = false;
        let reconnect_deadline_sleep = tokio::time::sleep(Duration::from_secs(86_400));
        tokio::pin!(reconnect_deadline_sleep);
        let mut reconnect_deadline_armed = false;

        let shutdown = handle.notify.clone();

        loop {
            tokio::select! {
                biased;

                // External cancel — drive cooperative teardown. The
                // stop reason on the WS reflects who actually drove
                // teardown: a remote-side BYE flips `remote_bye` on
                // the handle (via `CallRegistry::terminate_from_bye`
                // from PR #19), so the controller can distinguish
                // `caller_hangup` from `server_hangup` per
                // PROTOCOL.md §6. Anything else that wakes
                // `shutdown` (admin force-hangup, RFC 4028 session
                // expiry) is daemon-initiated and maps to
                // `server_hangup` from the WS server's point of view.
                _ = shutdown.notified() => {
                    let reason = if handle.remote_bye_received() {
                        StopReason::CallerHangup
                    } else {
                        StopReason::ServerHangup
                    };
                    info!(call_id = %call_id, ?reason, "controller shutdown requested");
                    // A drain-forced shutdown (deploy deadline) is
                    // attributed distinctly from a generic local
                    // shutdown (admin force-hangup, session expiry);
                    // both send the same clean BYE + WS stop.
                    termination = if handle.drain_forced() {
                        CallTermination::DrainForced
                    } else {
                        CallTermination::LocalShutdown
                    };
                    let _ = control_out_tx
                        .send(OutgoingEvent::Stop { reason })
                        .await;
                    break;
                }

                // Server-initiated control message.
                msg = control_in_rx.recv(), if control_open => {
                    match msg {
                        Some(BridgeIn::Hangup { call_id: cid, cause }) => {
                            debug!(?cause, ws_call_id = %cid, "server requested hangup");
                            termination = CallTermination::ServerHangup;
                            let _ = control_out_tx
                                .send(OutgoingEvent::Stop { reason: StopReason::ServerHangup })
                                .await;
                            break;
                        }
                        Some(BridgeIn::SendDtmf {
                            call_id: cid,
                            digit,
                            duration_ms,
                        }) => {
                            debug!(
                                ws_call_id = %cid,
                                digit = %digit,
                                duration_ms,
                                "forwarding SendDtmf to tap",
                            );
                            // tap_cmd_tx capacity is small (8); a
                            // backed-up tap means forge or the WS
                            // side is misbehaving. Drop with a warn
                            // rather than blocking the control loop.
                            if let Err(e) = tap_cmd_tx
                                .try_send(TapCommand::SendDtmf { digit, duration_ms })
                            {
                                warn!(error = %e, "tap command channel full or closed; dropping SendDtmf");
                            }
                        }
                        Some(BridgeIn::Clear { call_id: cid }) => {
                            debug!(ws_call_id = %cid, "forwarding Clear to tap");
                            // Same drop-rather-than-block policy as
                            // SendDtmf — Clear is barge-in-driven and
                            // the WS server typically follows up
                            // with a fresh playback, so a missed
                            // command is recoverable.
                            if let Err(e) = tap_cmd_tx.try_send(TapCommand::Clear) {
                                warn!(error = %e, "tap command channel full or closed; dropping Clear");
                            }
                        }
                        Some(BridgeIn::Mute { call_id: cid }) => {
                            debug!(ws_call_id = %cid, "forwarding Mute to tap");
                            // Same try_send policy: a dropped Mute is
                            // recoverable — the WS server can re-send.
                            // The alternative (await on a full channel)
                            // would back-pressure the WS receive loop
                            // and stall every other control message.
                            if let Err(e) = tap_cmd_tx.try_send(TapCommand::Mute) {
                                warn!(error = %e, "tap command channel full or closed; dropping Mute");
                            }
                        }
                        Some(BridgeIn::Unmute { call_id: cid }) => {
                            debug!(ws_call_id = %cid, "forwarding Unmute to tap");
                            if let Err(e) = tap_cmd_tx.try_send(TapCommand::Unmute) {
                                warn!(error = %e, "tap command channel full or closed; dropping Unmute");
                            }
                        }
                        Some(BridgeIn::BargeInConfirm { call_id: cid }) => {
                            debug!(ws_call_id = %cid, "forwarding BargeInConfirm to tap");
                            // A dropped verdict degrades to the tap's own
                            // decision deadline (`on_timeout`), so the
                            // try_send drop policy is safe here too.
                            if let Err(e) = tap_cmd_tx.try_send(TapCommand::BargeInConfirm) {
                                warn!(error = %e, "tap command channel full or closed; dropping BargeInConfirm");
                            }
                        }
                        Some(BridgeIn::BargeInReject { call_id: cid }) => {
                            debug!(ws_call_id = %cid, "forwarding BargeInReject to tap");
                            if let Err(e) = tap_cmd_tx.try_send(TapCommand::BargeInReject) {
                                warn!(error = %e, "tap command channel full or closed; dropping BargeInReject");
                            }
                        }
                        Some(BridgeIn::StartRecording { call_id: cid }) => {
                            if rec_blocked {
                                warn!(call_id = %cid,
                                      "start_recording ignored: announcement failed (recording fail-closed)");
                            } else if announce_done_rx.is_some() {
                                // Capture starts only after the prompt —
                                // remember the request and act on
                                // announcement completion.
                                debug!(call_id = %cid,
                                       "start_recording deferred until the announcement completes");
                                announce_start_after = true;
                            } else {
                                route_rec_control(&rec_ctrl_tx, RecControl::Start, "StartRecording", &cid);
                            }
                        }
                        Some(BridgeIn::StopRecording { call_id: cid }) => {
                            route_rec_control(&rec_ctrl_tx, RecControl::Stop, "StopRecording", &cid);
                        }
                        Some(BridgeIn::PauseRecording { call_id: cid }) => {
                            route_rec_control(&rec_ctrl_tx, RecControl::Pause, "PauseRecording", &cid);
                        }
                        Some(BridgeIn::ResumeRecording { call_id: cid }) => {
                            route_rec_control(&rec_ctrl_tx, RecControl::Resume, "ResumeRecording", &cid);
                        }
                        Some(BridgeIn::SetRecordingConsent { call_id: cid, note }) => {
                            // Audit stamp only — never gates recording.
                            let note = note
                                .map(|n| n.chars().take(256).collect::<String>())
                                .unwrap_or_else(|| "unspecified".to_string());
                            debug!(call_id = %cid, note = %note, "recording consent reported by server");
                            consent_server = Some(note);
                        }
                        Some(BridgeIn::ConferenceJoin { call_id: cid, room_id }) => {
                            debug!(ws_call_id = %cid, %room_id, "conference join requested by WS");
                            // Async join (round-trips to the room task)
                            // is spawned so the control loop never
                            // blocks — same reasoning as REFER. The
                            // task reports back on `conf_join_rx`, which
                            // forwards the membership to the tap (or
                            // emits `conference_failed`).
                            if !spawn_conference_join(
                                conference.as_ref(),
                                &call_id,
                                call_sample_rate,
                                &conf_join_tx,
                                room_id,
                            ) {
                                let _ = control_out_tx
                                    .send(OutgoingEvent::Error {
                                        code: ErrorCode::ConferenceFailed,
                                        message: "conferencing is disabled on this daemon"
                                            .to_string(),
                                    })
                                    .await;
                            }
                        }
                        Some(BridgeIn::ConferenceLeave { call_id: cid }) => {
                            // The tap drops its RoomSender (→ the room
                            // removes this call) and emits
                            // `conference_left`. No registry round-trip
                            // needed: leave is drop-driven (chunk 1).
                            debug!(ws_call_id = %cid, "forwarding ConferenceLeave to tap");
                            if let Err(e) = tap_cmd_tx.try_send(TapCommand::LeaveRoom) {
                                warn!(error = %e, "tap command channel full or closed; dropping ConferenceLeave");
                            }
                        }
                        Some(BridgeIn::Park { call_id: cid, slot }) => {
                            // Funnel WS park through the same channel the
                            // admin uses, so there's exactly one park
                            // implementation (the park_cmd_rx arm).
                            debug!(ws_call_id = %cid, ?slot, "WS park requested");
                            handle.request_park(slot);
                        }
                        Some(BridgeIn::Hold { call_id: cid }) => {
                            // Bot-initiated hold: SiphonAI becomes the
                            // re-INVITE offerer (a=sendonly) and switches
                            // the caller to MOH. A failed hold never drops
                            // the call (PROTOCOL.md §4.10) — it just stays
                            // in its prior media state.
                            debug!(ws_call_id = %cid, "WS hold requested");
                            match hold.as_ref() {
                                _ if held => {
                                    // Idempotent: already held → re-ack.
                                    debug!(call_id = %call_id, "hold requested but already held; re-acking");
                                    let _ = control_out_tx.send(OutgoingEvent::Held).await;
                                }
                                _ if handle.peer_held() => {
                                    // No stacking (0.7.2 first cut): the far
                                    // end already holds us.
                                    warn!(call_id = %call_id, "hold rejected: call is already held by the far end");
                                    metrics::counter!(HOLDS_TOTAL, "result" => "failed").increment(1);
                                    let _ = control_out_tx.send(OutgoingEvent::Error {
                                        code: ErrorCode::HoldFailed,
                                        message: "call is already held by the far end".to_string(),
                                    }).await;
                                }
                                Some(hctx) => {
                                    // Switch the caller to MOH now (the bot
                                    // stops hearing the caller, the caller
                                    // hears hold music), then confirm at the
                                    // SIP layer with a sendonly re-INVITE.
                                    let moh = MohSource::new(
                                        hctx.moh_file.as_deref(),
                                        call_sample_rate,
                                    );
                                    if let Err(e) = tap_cmd_tx
                                        .try_send(TapCommand::Hold { moh: Box::new(moh) })
                                    {
                                        warn!(call_id = %call_id, error = %e,
                                            "tap command channel full/closed; hold aborted");
                                        metrics::counter!(HOLDS_TOTAL, "result" => "failed").increment(1);
                                        let _ = control_out_tx.send(OutgoingEvent::Error {
                                            code: ErrorCode::HoldFailed,
                                            message: "tap unavailable for hold".to_string(),
                                        }).await;
                                    } else {
                                        match drive_hold_reinvite(
                                            hctx,
                                            &hctx.hold_offer_sdp,
                                            HOLD_GLARE_BACKOFF,
                                        )
                                        .await
                                        {
                                            Ok(()) => {
                                                held = true;
                                                hold_count += 1;
                                                held_since = Some(Instant::now());
                                                metrics::counter!(HOLDS_TOTAL, "result" => "ok").increment(1);
                                                info!(call_id = %call_id, "call held (bot-initiated)");
                                                let _ = control_out_tx.send(OutgoingEvent::Held).await;
                                            }
                                            Err(e) => {
                                                // Revert the optimistic MOH switch.
                                                let _ = tap_cmd_tx.try_send(TapCommand::Unhold);
                                                warn!(call_id = %call_id, error = %e, "hold re-INVITE failed");
                                                metrics::counter!(HOLDS_TOTAL, "result" => "failed").increment(1);
                                                let _ = control_out_tx.send(OutgoingEvent::Error {
                                                    code: ErrorCode::HoldFailed,
                                                    message: e,
                                                }).await;
                                            }
                                        }
                                    }
                                }
                                None => {
                                    warn!(call_id = %call_id, "hold requested but hold is unavailable on this leg");
                                    metrics::counter!(HOLDS_TOTAL, "result" => "failed").increment(1);
                                    let _ = control_out_tx.send(OutgoingEvent::Error {
                                        code: ErrorCode::HoldFailed,
                                        message: "hold is not available on this call".to_string(),
                                    }).await;
                                }
                            }
                        }
                        Some(BridgeIn::Resume { call_id: cid }) => {
                            debug!(ws_call_id = %cid, "WS resume requested");
                            if !held {
                                // Not held → no-op success (PROTOCOL.md §4.10).
                                debug!(call_id = %call_id, "resume on a call that isn't held; acking no-op");
                                let _ = control_out_tx.send(OutgoingEvent::Resumed).await;
                            } else if let Some(hctx) = hold.as_ref() {
                                match drive_hold_reinvite(
                                    hctx,
                                    &hctx.resume_offer_sdp,
                                    HOLD_GLARE_BACKOFF,
                                )
                                .await
                                {
                                    Ok(()) => {
                                        held = false;
                                        // Close the books on this hold episode.
                                        if let Some(since) = held_since.take() {
                                            hold_total += since.elapsed();
                                        }
                                        // Restore the direct caller↔WS bridge
                                        // on the existing WS session.
                                        if let Err(e) = tap_cmd_tx.try_send(TapCommand::Unhold) {
                                            warn!(call_id = %call_id, error = %e,
                                                "tap command channel full/closed; resume audio may not restore");
                                        }
                                        metrics::counter!(HOLDS_TOTAL, "result" => "ok").increment(1);
                                        info!(call_id = %call_id, "call resumed (bot-initiated)");
                                        let _ = control_out_tx.send(OutgoingEvent::Resumed).await;
                                    }
                                    Err(e) => {
                                        // Stay held; the WS server can retry.
                                        warn!(call_id = %call_id, error = %e, "resume re-INVITE failed");
                                        metrics::counter!(HOLDS_TOTAL, "result" => "failed").increment(1);
                                        let _ = control_out_tx.send(OutgoingEvent::Error {
                                            code: ErrorCode::HoldFailed,
                                            message: e,
                                        }).await;
                                    }
                                }
                            } else {
                                // Held but no hold ctx — shouldn't happen.
                                metrics::counter!(HOLDS_TOTAL, "result" => "failed").increment(1);
                                let _ = control_out_tx.send(OutgoingEvent::Error {
                                    code: ErrorCode::HoldFailed,
                                    message: "resume is not available on this call".to_string(),
                                }).await;
                            }
                        }
                        Some(BridgeIn::Mark { call_id: cid, name }) => {
                            debug!(ws_call_id = %cid, %name, "forwarding Mark to tap");
                            // Mark is a notification request — the
                            // WS server is asking us to fire `mark`
                            // back when audio up to this point has
                            // played. Drop on full as elsewhere; the
                            // server can re-issue if it really
                            // needs the signal.
                            if let Err(e) =
                                tap_cmd_tx.try_send(TapCommand::Mark { name: name.clone() })
                            {
                                warn!(error = %e, %name, "tap command channel full or closed; dropping Mark");
                            }
                        }
                        Some(BridgeIn::Transfer { call_id: cid, target, replaces_call_id }) => {
                            // REFER — blind (RFC 3515) or, when
                            // `replaces_call_id` names a consult call,
                            // attended (RFC 5589). Spawn a task so
                            // the multi-RTT REFER never blocks the
                            // control loop (CLAUDE.md §4.3: nothing
                            // adjacent to audio may block). The task
                            // reports back via `transfer_result_rx`,
                            // which has its own select arm below.
                            match &transfer {
                                Some(ctx) => {
                                    let ctx = ctx.clone();
                                    let tx = transfer_result_tx.clone();
                                    let target_owned = target.clone();
                                    let replaces_owned = replaces_call_id.clone();
                                    debug!(
                                        ws_call_id = %cid,
                                        target = ?target_owned,
                                        replaces_call_id = ?replaces_owned,
                                        "spawning REFER task"
                                    );
                                    tokio::spawn(async move {
                                        let outcome = run_transfer(
                                            &ctx,
                                            target_owned.as_deref(),
                                            replaces_owned.as_deref(),
                                        )
                                        .await;
                                        // Receiver is bounded(1); the
                                        // call ends on first Accepted
                                        // so try_send is safe — a
                                        // queue-full only happens if
                                        // a second Transfer landed
                                        // back-to-back, in which case
                                        // dropping the result is fine.
                                        let _ = tx.try_send(outcome);
                                    });
                                }
                                None => {
                                    warn!(
                                        ws_call_id = %cid,
                                        "BridgeIn::Transfer received but no IntegratedUAC \
                                         is installed; rejecting"
                                    );
                                    let _ = control_out_tx
                                        .send(OutgoingEvent::Error {
                                            code: ErrorCode::TransferFailed,
                                            message: "transfer not configured on daemon"
                                                .to_string(),
                                        })
                                        .await;
                                }
                            }
                        }
                        None => {
                            // Bridge dropped the sender — let the
                            // bridge_task arm fire next.
                            debug!("control_in_rx closed; awaiting bridge task to settle");
                            control_open = false;
                        }
                    }
                }

                // Outcome of an in-flight REFER. Open for the lifetime
                // of the controller; cap = 1 so this fires at most
                // once per Transfer round.
                Some(outcome) = transfer_result_rx.recv() => {
                    match outcome {
                        TransferOutcome::Accepted => {
                            info!(call_id = %call_id, "REFER accepted; tearing down call");
                            // On inbound legs the transfer task has
                            // already sent the post-REFER BYE ("REFER
                            // + BYE"); flag the handle so the
                            // acceptor's cleanup doesn't owe the peer
                            // a second one (it would go out with a
                            // fresh CSeq space, which strict peers
                            // reject). Outbound legs stay unmarked —
                            // their run_call teardown still sends the
                            // BYE. If the task's BYE failed we skip
                            // the cleanup BYE anyway: it would reuse
                            // the same UAC and fail the same way, and
                            // the task already warned that the dialog
                            // may linger.
                            if transfer.as_ref().is_some_and(|t| t.control.source.bye_after_refer()) {
                                handle.mark_remote_bye();
                            }
                            termination = CallTermination::LocalShutdown;
                            let _ = control_out_tx
                                .send(OutgoingEvent::Stop { reason: StopReason::Transfer })
                                .await;
                            break;
                        }
                        TransferOutcome::LocalError(msg) => {
                            warn!(call_id = %call_id, error = %msg, "REFER failed locally");
                            let _ = control_out_tx
                                .send(OutgoingEvent::Error {
                                    code: ErrorCode::TransferFailed,
                                    message: msg,
                                })
                                .await;
                        }
                        TransferOutcome::RemoteRejected { status, reason } => {
                            warn!(
                                call_id = %call_id,
                                status,
                                reason = %reason,
                                "PBX rejected REFER"
                            );
                            let _ = control_out_tx
                                .send(OutgoingEvent::Error {
                                    code: ErrorCode::TransferFailed,
                                    message: format!("{status} {reason}"),
                                })
                                .await;
                        }
                    }
                }

                // Outcome of an async conference join. On success the
                // membership is handed to the tap (which re-plumbs the
                // audio and emits `conference_joined`); on failure the
                // server gets `error { code: "conference_failed" }`
                // and the call continues on its direct pair.
                Some(outcome) = conf_join_rx.recv() => {
                    match outcome {
                        Ok(membership) => {
                            let room_id = membership.room_id().to_string();
                            if let Err(e) =
                                tap_cmd_tx.try_send(TapCommand::JoinRoom { membership })
                            {
                                // The tap command channel is full or
                                // gone — we can't re-plumb, and the
                                // membership drops here (its RoomSender
                                // Drop leaves the room cleanly). Tell
                                // the server the join didn't take.
                                warn!(
                                    call_id = %call_id,
                                    %room_id,
                                    error = %e,
                                    "tap command channel full or closed; conference join aborted"
                                );
                                let _ = control_out_tx
                                    .send(OutgoingEvent::Error {
                                        code: ErrorCode::ConferenceFailed,
                                        message: "tap unavailable for conference join".to_string(),
                                    })
                                    .await;
                            } else {
                                info!(call_id = %call_id, %room_id, "joined conference room");
                            }
                        }
                        Err(e) => {
                            warn!(call_id = %call_id, error = %e, "conference join rejected");
                            let _ = control_out_tx
                                .send(OutgoingEvent::Error {
                                    code: ErrorCode::ConferenceFailed,
                                    message: e.to_string(),
                                })
                                .await;
                        }
                    }
                }

                // Admin cross-call conference command (operator added
                // or removed this call via the admin API). Runs the
                // same path as the WS-driven join/leave — §4.4: the
                // admin signals the call; the controller acts on its
                // own state.
                Some(cmd) = conf_cmd_rx.recv() => {
                    match cmd {
                        ConferenceCommand::Join { room_id } => {
                            info!(call_id = %call_id, %room_id, "conference join requested by admin");
                            if !spawn_conference_join(
                                conference.as_ref(),
                                &call_id,
                                call_sample_rate,
                                &conf_join_tx,
                                room_id,
                            ) {
                                let _ = control_out_tx
                                    .send(OutgoingEvent::Error {
                                        code: ErrorCode::ConferenceFailed,
                                        message: "conferencing is disabled on this daemon"
                                            .to_string(),
                                    })
                                    .await;
                            }
                        }
                        ConferenceCommand::Leave => {
                            info!(call_id = %call_id, "conference leave requested by admin");
                            if let Err(e) = tap_cmd_tx.try_send(TapCommand::LeaveRoom) {
                                warn!(error = %e, "tap command channel full or closed; dropping admin ConferenceLeave");
                            }
                        }
                    }
                }

                // Park / retrieve (admin, and WS park forwarded here).
                Some(cmd) = park_cmd_rx.recv() => {
                    match cmd {
                        ParkCommand::Park { slot } => {
                            if parked {
                                debug!(call_id = %call_id, "park requested but already parked; ignoring");
                            } else if let Some(park_ctx) = park.as_ref() {
                                match park_ctx.registry.try_park(call_id.as_str(), slot.clone()) {
                                    Ok(()) => {
                                        let moh = MohSource::new(
                                            park_ctx.settings.moh_file.as_deref(),
                                            call_sample_rate,
                                        );
                                        if let Err(e) = tap_cmd_tx
                                            .try_send(TapCommand::Park { moh: Box::new(moh) })
                                        {
                                            warn!(call_id = %call_id, error = %e,
                                                "tap command channel full/closed; park aborted");
                                            park_ctx.registry.remove(call_id.as_str());
                                            let _ = control_out_tx.send(OutgoingEvent::Error {
                                                code: ErrorCode::ParkFailed,
                                                message: "tap unavailable for park".to_string(),
                                            }).await;
                                        } else {
                                            // Detach the WS: the bridge sends
                                            // `stop{park}` and closes. The
                                            // bridge-end arm sees `parked` and
                                            // stays alive.
                                            let _ = control_out_tx
                                                .send(OutgoingEvent::Stop { reason: StopReason::Park })
                                                .await;
                                            parked = true;
                                            park_count += 1;
                                            parked_since = Some(Instant::now());
                                            if let Some(d) = park_ctx.settings.timeout {
                                                park_timeout_sleep
                                                    .as_mut()
                                                    .reset(tokio::time::Instant::now() + d);
                                                park_deadline_armed = true;
                                            }
                                            metrics::gauge!(PARKED_CALLS_ACTIVE).increment(1.0);
                                            metrics::counter!(PARKS_TOTAL, "result" => "ok").increment(1);
                                            info!(call_id = %call_id, ?slot, "call parked");
                                            spawn_webhook(
                                                &park_ctx.webhooks,
                                                WebhookEvent::CallParked(CallParkedEvent {
                                                    version: WEBHOOK_VERSION,
                                                    call_id: call_id.as_str().to_string(),
                                                    timestamp: Utc::now(),
                                                    slot,
                                                }),
                                            );
                                        }
                                    }
                                    Err(e) => {
                                        warn!(call_id = %call_id, error = %e, "park rejected");
                                        metrics::counter!(PARKS_TOTAL, "result" => "rejected").increment(1);
                                        let _ = control_out_tx.send(OutgoingEvent::Error {
                                            code: ErrorCode::ParkFailed,
                                            message: e.to_string(),
                                        }).await;
                                    }
                                }
                            } else {
                                warn!(call_id = %call_id, "park requested but park is disabled");
                                let _ = control_out_tx.send(OutgoingEvent::Error {
                                    code: ErrorCode::ParkFailed,
                                    message: "park is disabled on this daemon".to_string(),
                                }).await;
                            }
                        }
                        ParkCommand::Retrieve { ws_url } => {
                            if !parked {
                                debug!(call_id = %call_id, "retrieve on a non-parked call; ignoring");
                                metrics::counter!(RETRIEVES_TOTAL, "result" => "not_parked").increment(1);
                            } else {
                                // Account the parked span before flipping back.
                                if let Some(since) = parked_since.take() {
                                    park_total += since.elapsed();
                                }
                                // Fresh channels for the new bridge.
                                let (n_caller_tx, n_caller_rx) =
                                    mpsc::channel::<Vec<u8>>(AUDIO_CHANNEL_CAPACITY);
                                let (n_playout_tx, n_playout_rx) =
                                    mpsc::channel::<Vec<u8>>(AUDIO_CHANNEL_CAPACITY);
                                let (n_ci_tx, n_ci_rx) =
                                    mpsc::channel::<BridgeIn>(CONTROL_CHANNEL_CAPACITY);
                                let (n_co_tx, n_co_rx) =
                                    mpsc::channel::<OutgoingEvent>(CONTROL_CHANNEL_CAPACITY);
                                // Fresh `start` from the preserved facts.
                                let mut start2 = start_template.clone();
                                start2.seq = 0;
                                start2.retrieved = true;
                                let ws = ws_url
                                    .clone()
                                    .filter(|s| !s.is_empty())
                                    .unwrap_or_else(|| bridge_template.ws_url.clone());
                                let bridge_cfg2 = BridgeConfig {
                                    ws_url: ws.clone(),
                                    ..bridge_template.clone()
                                };
                                bridge_task = tokio::spawn(connect_and_run(
                                    bridge_cfg2,
                                    start2,
                                    BridgeChannels {
                                        audio_out_rx: n_caller_rx,
                                        control_out_rx: n_co_rx,
                                        audio_in_tx: n_playout_tx,
                                        control_in_tx: n_ci_tx,
                                    },
                                ));
                                bridge_alive = true;
                                control_in_rx = n_ci_rx;
                                control_open = true;
                                control_out_tx = n_co_tx.clone();
                                handle.swap_bridge_sender(n_co_tx.clone());
                                if let Err(e) = tap_cmd_tx.try_send(TapCommand::Unpark {
                                    caller_audio_tx: n_caller_tx,
                                    playout_audio_rx: n_playout_rx,
                                    events_tx: n_co_tx,
                                }) {
                                    warn!(call_id = %call_id, error = %e,
                                        "tap command channel full/closed; retrieve audio may not resume");
                                }
                                parked = false;
                                park_deadline_armed = false;
                                if let Some(pc) = park.as_ref() {
                                    pc.registry.remove(call_id.as_str());
                                }
                                metrics::gauge!(PARKED_CALLS_ACTIVE).decrement(1.0);
                                metrics::counter!(RETRIEVES_TOTAL, "result" => "ok").increment(1);
                                info!(call_id = %call_id, ws_url = %ws, "call retrieved onto fresh WS session");
                                if let Some(pc) = park.as_ref() {
                                    spawn_webhook(
                                        &pc.webhooks,
                                        WebhookEvent::CallRetrieved(CallRetrievedEvent {
                                            version: WEBHOOK_VERSION,
                                            call_id: call_id.as_str().to_string(),
                                            timestamp: Utc::now(),
                                            ws_url: ws,
                                        }),
                                    );
                                }
                            }
                        }
                    }
                }

                // Park timeout. Armed on park; fires once. Selected only
                // while parked with a live deadline.
                _ = &mut park_timeout_sleep, if parked && park_deadline_armed => {
                    park_deadline_armed = false;
                    let action = park
                        .as_ref()
                        .map(|p| p.settings.timeout_action)
                        .unwrap_or(ParkTimeoutAction::Hangup);
                    info!(call_id = %call_id, ?action, "park timeout fired");
                    if let Some(pc) = park.as_ref() {
                        spawn_webhook(
                            &pc.webhooks,
                            WebhookEvent::ParkTimeout(ParkTimeoutEvent {
                                version: WEBHOOK_VERSION,
                                call_id: call_id.as_str().to_string(),
                                timestamp: Utc::now(),
                                action: match action {
                                    ParkTimeoutAction::Hangup => "hangup".to_string(),
                                    ParkTimeoutAction::Keep => "keep".to_string(),
                                },
                            }),
                        );
                    }
                    match action {
                        ParkTimeoutAction::Hangup => {
                            termination = CallTermination::LocalShutdown;
                            break;
                        }
                        ParkTimeoutAction::Keep => {
                            // Stay parked; operator must retrieve or hang up.
                        }
                    }
                }

                // ─── Reconnect: backoff elapsed → (re)dial (0.7.3 §6) ───
                // Build fresh channels + a readiness signal and spawn a
                // redial to the same ws_url with `start.reconnected = true`.
                // We swap the control channels in now (sends queue on the
                // bounded channel until the socket is live) but DON'T drop
                // MOH yet — that waits for `ready`.
                _ = &mut reconnect_backoff_sleep, if reconnecting && reconnect_backoff_armed => {
                    reconnect_backoff_armed = false;
                    let (n_caller_tx, n_caller_rx) =
                        mpsc::channel::<Vec<u8>>(AUDIO_CHANNEL_CAPACITY);
                    let (n_playout_tx, n_playout_rx) =
                        mpsc::channel::<Vec<u8>>(AUDIO_CHANNEL_CAPACITY);
                    let (n_ci_tx, n_ci_rx) = mpsc::channel::<BridgeIn>(CONTROL_CHANNEL_CAPACITY);
                    let (n_co_tx, n_co_rx) =
                        mpsc::channel::<OutgoingEvent>(CONTROL_CHANNEL_CAPACITY);
                    let (ready_tx, ready_rx) = oneshot::channel();
                    // Fresh `start` from the preserved facts, flagged as a
                    // reconnect resume (seq restarts at 0, no replay).
                    let mut start2 = start_template.clone();
                    start2.seq = 0;
                    start2.reconnected = true;
                    bridge_task = tokio::spawn(connect_and_run_with_ready(
                        bridge_template.clone(),
                        start2,
                        BridgeChannels {
                            audio_out_rx: n_caller_rx,
                            control_out_rx: n_co_rx,
                            audio_in_tx: n_playout_tx,
                            control_in_tx: n_ci_tx,
                        },
                        Some(ready_tx),
                    ));
                    bridge_alive = true;
                    control_in_rx = n_ci_rx;
                    control_open = true;
                    control_out_tx = n_co_tx.clone();
                    handle.swap_bridge_sender(n_co_tx.clone());
                    pending_unpark = Some((n_caller_tx, n_playout_rx, n_co_tx));
                    reconnect_ready_rx = Some(ready_rx);
                    debug!(call_id = %call_id, attempt = reconnect_attempt, "redialing ws bridge");
                }

                // ─── Reconnect: redial connected (`ready` fired) ────────
                // The new socket is live and `start` is on the wire — drop
                // MOH and restore the direct caller↔WS pair on the fresh
                // channels. An `Err` means the bridge died before ready;
                // the bridge-end arm handles that (backoff/give-up).
                ready = recv_ready(&mut reconnect_ready_rx),
                    if reconnecting && reconnect_ready_rx.is_some() =>
                {
                    reconnect_ready_rx = None;
                    if ready.is_ok() {
                        if let Some((caller_tx, playout_rx, events_tx)) = pending_unpark.take() {
                            if let Err(e) = tap_cmd_tx.try_send(TapCommand::Unpark {
                                caller_audio_tx: caller_tx,
                                playout_audio_rx: playout_rx,
                                events_tx,
                            }) {
                                warn!(call_id = %call_id, error = %e,
                                    "tap command channel full/closed; reconnect audio may not resume");
                            }
                        }
                        reconnecting = false;
                        reconnect_deadline_armed = false;
                        reconnect_backoff_armed = false;
                        let gap = reconnect_since.take().map(|s| s.elapsed()).unwrap_or_default();
                        reconnect_total += gap;
                        metrics::counter!(WS_RECONNECTS_TOTAL, "result" => "recovered").increment(1);
                        info!(call_id = %call_id, gap_ms = gap.as_millis() as u64,
                            "ws reconnected; call resumed on a fresh session");
                    }
                }

                // ─── Reconnect: window elapsed → give up (0.7.3 §6) ─────
                _ = &mut reconnect_deadline_sleep, if reconnecting && reconnect_deadline_armed => {
                    if let Some(since) = reconnect_since.take() {
                        reconnect_total += since.elapsed();
                    }
                    metrics::counter!(WS_RECONNECTS_TOTAL, "result" => "exhausted").increment(1);
                    warn!(call_id = %call_id, window_secs = ws_reconnect_max.as_secs(),
                        "ws reconnect window elapsed; tearing down (ws_disconnect)");
                    termination = CallTermination::BridgeEnded;
                    break;
                }

                // Bridge sub-task ended. Guarded by `bridge_alive` so a
                // completed handle is never re-polled (it's reassigned on
                // retrieve). A bridge ending *while parked* is the park's
                // own `stop{park}` close — non-terminal; the call lives on
                // MOH until retrieve / timeout / caller-BYE.
                res = &mut bridge_task, if bridge_alive => {
                    bridge_alive = false;
                    let inner = match res {
                        Ok(inner) => inner,
                        Err(join_err) => {
                            warn!(?join_err, "bridge task panicked");
                            return Err(CallError::TaskJoin(join_err));
                        }
                    };
                    // Remember the most recent disconnect reason for the
                    // CDR (a give-up reconnect surfaces as ws_disconnect).
                    bridge_result = Some(inner);
                    let inner_ref = bridge_result.as_ref().expect("just set");
                    if parked {
                        debug!(call_id = %call_id, "WS bridge closed for park; call remains parked");
                    } else if reconnecting {
                        // A staged redial ended before signalling `ready`
                        // → it failed (a success would have flipped
                        // `reconnecting` off in the ready arm). Drop the
                        // stale staging and either back off again or, if
                        // the window has closed, give up.
                        reconnect_ready_rx = None;
                        pending_unpark = None;
                        if reconnect_deadline_armed {
                            reconnect_attempt += 1;
                            let backoff = reconnect_backoff(reconnect_attempt);
                            reconnect_backoff_sleep
                                .as_mut()
                                .reset(tokio::time::Instant::now() + backoff);
                            reconnect_backoff_armed = true;
                            debug!(call_id = %call_id, attempt = reconnect_attempt,
                                backoff_ms = backoff.as_millis() as u64,
                                "ws redial failed; backing off");
                        } else {
                            warn!(call_id = %call_id, "ws reconnect window closed; tearing down");
                            termination = CallTermination::BridgeEnded;
                            break;
                        }
                    } else if ws_reconnect_enabled && reconnect_eligible(inner_ref) {
                        // First eligible unexpected drop → hold the caller
                        // on MOH and start redialing the same ws_url
                        // (0.7.3 §6). A failed hold (tap gone) means we
                        // can't reconnect cleanly → tear down.
                        let moh = MohSource::new(ws_reconnect_moh_file.as_deref(), call_sample_rate);
                        if tap_cmd_tx
                            .try_send(TapCommand::Park { moh: Box::new(moh) })
                            .is_err()
                        {
                            warn!(call_id = %call_id,
                                "tap unavailable; cannot hold for reconnect; tearing down");
                            termination = CallTermination::BridgeEnded;
                            break;
                        }
                        reconnecting = true;
                        reconnect_attempt = 0;
                        reconnect_count += 1;
                        reconnect_since = Some(Instant::now());
                        reconnect_deadline_sleep
                            .as_mut()
                            .reset(tokio::time::Instant::now() + ws_reconnect_max);
                        reconnect_deadline_armed = true;
                        let backoff = reconnect_backoff(0);
                        reconnect_backoff_sleep
                            .as_mut()
                            .reset(tokio::time::Instant::now() + backoff);
                        reconnect_backoff_armed = true;
                        warn!(call_id = %call_id, window_secs = ws_reconnect_max.as_secs(),
                            "ws bridge dropped unexpectedly; reconnecting");
                    } else {
                        termination = CallTermination::BridgeEnded;
                        break;
                    }
                }

                // Tap sub-task ended.
                res = &mut tap_task => {
                    match res {
                        Ok(inner) => tap_result = Some(inner),
                        Err(join_err) => {
                            warn!(?join_err, "tap task panicked");
                            return Err(CallError::TaskJoin(join_err));
                        }
                    }
                    // PROTOCOL.md §3.10 `rtp_timeout`: the media inactivity
                    // watchdog fired (no inbound RTP for
                    // `[media].inactivity_timeout_secs`). Tell the WS server
                    // *why* before we close — queue `error{rtp_timeout}` +
                    // `stop`; the conn drains both (biased recv) and emits
                    // them before teardown drops `control_out_tx`.
                    // Best-effort: the bridge may already be gone. The CDR
                    // termination cause stays `TapEnded` (the rtp timeout is
                    // the cause; the stop is just the mechanism).
                    if matches!(tap_result, Some(Ok(TapDisconnect::InactivityTimeout))) {
                        let _ = control_out_tx
                            .send(OutgoingEvent::Error {
                                code: ErrorCode::RtpTimeout,
                                message: "no inbound RTP within the inactivity timeout".into(),
                            })
                            .await;
                        let _ = control_out_tx
                            .send(OutgoingEvent::Stop {
                                reason: StopReason::Error,
                            })
                            .await;
                    }
                    termination = CallTermination::TapEnded;
                    break;
                }

                // Recording writer lifecycle event → relay to the WS server.
                done = recv_announce_done(&mut announce_done_rx), if announce_done_rx.is_some() => {
                    announce_done_rx = None;
                    match done {
                        Ok(ms) => {
                            info!(call_id = %call_id, announcement_ms = ms, "announcement complete");
                            announced_ms = Some(ms);
                            if announce_start_after {
                                route_rec_control(
                                    &rec_ctrl_tx,
                                    RecControl::Start,
                                    "announcement-complete",
                                    &call_id,
                                );
                            }
                        }
                        Err(_) => {
                            // Tap dropped the channel (teardown race) —
                            // fail-closed: no capture starts.
                            warn!(call_id = %call_id,
                                  "announcement completion channel dropped; recording stays off");
                            rec_blocked = true;
                        }
                    }
                }

                maybe_evt = recv_rec_evt(&mut rec_evt_rx) => {
                    match maybe_evt {
                        Some(evt) => {
                            let out = match evt {
                                RecEvent::Started => {
                                    debug!(call_id = %call_id, recording_id = %recording_id, "recording started");
                                    OutgoingEvent::RecordingStarted { recording_id: recording_id.clone() }
                                }
                                RecEvent::Stopped { data_bytes, frames } => {
                                    debug!(call_id = %call_id, data_bytes, frames, "recording stopped");
                                    OutgoingEvent::RecordingStopped { recording_id: recording_id.clone() }
                                }
                                RecEvent::Failed { reason } => {
                                    warn!(call_id = %call_id, %reason, "recording failed");
                                    OutgoingEvent::RecordingFailed { recording_id: recording_id.clone(), reason }
                                }
                            };
                            let _ = control_out_tx.send(out).await;
                        }
                        // Writer task ended → stop polling this arm.
                        None => rec_evt_rx = None,
                    }
                }
            }
        }

        log_state(&call_id, CallState::Terminating);

        // ─── Park teardown ───────────────────────────────────────
        // If we broke out while still parked (timeout-hangup, or a
        // caller BYE during park), close the books on the parked span
        // and free the registry slot + gauge.
        if parked {
            if let Some(since) = parked_since.take() {
                park_total += since.elapsed();
            }
            if let Some(pc) = park.as_ref() {
                pc.registry.remove(call_id.as_str());
            }
            metrics::gauge!(PARKED_CALLS_ACTIVE).decrement(1.0);
        }
        let park_summary = (park_count > 0).then_some(ParkSummary {
            count: park_count,
            total_ms: park_total.as_millis() as u64,
        });

        // ─── Hold teardown ───────────────────────────────────────
        // If the call ended while still held (caller BYE / WS close
        // mid-hold), close the books on the open hold span.
        if held {
            if let Some(since) = held_since.take() {
                hold_total += since.elapsed();
            }
        }
        let hold_summary = (hold_count > 0).then_some(HoldSummary {
            count: hold_count,
            total_ms: hold_total.as_millis() as u64,
        });

        // ─── Reconnect teardown ──────────────────────────────────
        // If the call ended mid-reconnect (a caller BYE / tap end during
        // the gap, not the deadline), close the books on the open gap and
        // count the episode as exhausted (it never recovered).
        if reconnecting {
            if let Some(since) = reconnect_since.take() {
                reconnect_total += since.elapsed();
            }
            metrics::counter!(WS_RECONNECTS_TOTAL, "result" => "exhausted").increment(1);
        }
        let reconnect_summary = (reconnect_count > 0).then_some(ReconnectSummary {
            count: reconnect_count,
            total_gap_ms: reconnect_total.as_millis() as u64,
        });

        // ─── Drain remaining sub-tasks with a budget ─────────────
        // The bridge needs to flush its `stop` send + WS close;
        // 250 ms is plenty for a healthy connection. We don't want
        // to block forever if the server is unreachable.
        if !bridge_task.is_finished() && termination != CallTermination::BridgeEnded {
            // Drop the channel halves we still own to let the
            // bridge see EOF on its inputs and exit if it was
            // waiting on us.
            drop(control_out_tx);
            match tokio::time::timeout(Duration::from_millis(250), &mut bridge_task).await {
                Ok(Ok(inner)) => bridge_result = Some(inner),
                Ok(Err(join_err)) => return Err(CallError::TaskJoin(join_err)),
                Err(_) => {
                    warn!(call_id = %call_id, "bridge task did not exit within 250 ms; aborting");
                    bridge_task.abort();
                    let _ = (&mut bridge_task).await;
                }
            }
        } else if !bridge_task.is_finished() {
            // termination == BridgeEnded but we got here without
            // unwrapping the result — re-await to capture it.
            if let Ok(inner) = (&mut bridge_task).await {
                bridge_result = Some(inner);
            }
        }

        if !tap_task.is_finished() && termination != CallTermination::TapEnded {
            // Tap exits when its caller_audio_tx receiver drops
            // (i.e., bridge task is gone) or when its
            // playout_audio_rx sender drops. Both have happened by
            // now, but give it a beat.
            match tokio::time::timeout(Duration::from_millis(250), &mut tap_task).await {
                Ok(Ok(inner)) => tap_result = Some(inner),
                Ok(Err(join_err)) => return Err(CallError::TaskJoin(join_err)),
                Err(_) => {
                    warn!(call_id = %call_id, "tap task did not exit within 250 ms; aborting");
                    tap_task.abort();
                    let _ = (&mut tap_task).await;
                }
            }
        } else if !tap_task.is_finished() {
            if let Ok(inner) = (&mut tap_task).await {
                tap_result = Some(inner);
            }
        }

        // Finalize the recording. The tap has now exited, dropping its fork
        // sender → the writer sees EOF, flushes, and patches the WAV header.
        // Give it a budget so a slow final flush can't wedge teardown. The
        // tap is done, so the drop counter is final: any drops → `degraded`.
        let recording_summary = if let Some(mut rec_task) = recording_task.take() {
            let dropped = rec_drops
                .as_ref()
                .map(|d| d.load(Ordering::Relaxed))
                .unwrap_or(0);
            let failed_path = || rec_path.clone().unwrap_or_default();
            match tokio::time::timeout(Duration::from_millis(500), &mut rec_task).await {
                Ok(Ok(Ok(Some(stats)))) => {
                    let result = if dropped > 0 {
                        warn!(call_id = %call_id, dropped, path = %stats.path.display(),
                            "recording degraded: frames dropped under writer back-pressure");
                        RecordingResult::Degraded
                    } else {
                        debug!(call_id = %call_id, path = %stats.path.display(),
                            frames = stats.frames, "recording written");
                        RecordingResult::Ok
                    };
                    Some(RecordingSummary {
                        path: stats.path,
                        result,
                        encrypted: rec_encrypted,
                    })
                }
                // on-demand recording that was never started.
                Ok(Ok(Ok(None))) => {
                    debug!(call_id = %call_id, "no recording for this call");
                    None
                }
                Ok(Ok(Err(e))) => {
                    warn!(call_id = %call_id, error = %e, "recording failed");
                    Some(RecordingSummary {
                        path: failed_path(),
                        result: RecordingResult::Failed,
                        encrypted: rec_encrypted,
                    })
                }
                Ok(Err(join_err)) => {
                    warn!(call_id = %call_id, error = %join_err, "recording task panicked");
                    Some(RecordingSummary {
                        path: failed_path(),
                        result: RecordingResult::Failed,
                        encrypted: rec_encrypted,
                    })
                }
                Err(_) => {
                    warn!(call_id = %call_id, "recording did not finalize within 500 ms; aborting");
                    rec_task.abort();
                    let _ = (&mut rec_task).await;
                    Some(RecordingSummary {
                        path: failed_path(),
                        result: RecordingResult::Failed,
                        encrypted: rec_encrypted,
                    })
                }
            }
        } else {
            None
        };

        log_state(&call_id, CallState::Done);

        let consent_summary =
            (announced_ms.is_some() || consent_server.is_some()).then(|| ConsentSummary {
                announced: announced_ms.is_some(),
                announcement_ms: announced_ms.unwrap_or(0),
                server: consent_server,
            });

        // Latest quality state from the tap + the first session's
        // connect stamp. Omit the whole block when nothing measured —
        // a call that never went active has no quality story.
        let quality_summary = QualityOutcome::from_report(*quality_rx.borrow(), *epoch_rx.borrow());

        Ok(CallOutcome {
            call_id,
            termination,
            bridge: bridge_result,
            tap: tap_result,
            recording: recording_summary,
            park: park_summary,
            hold: hold_summary,
            reconnect: reconnect_summary,
            consent: consent_summary,
            quality: quality_summary,
        })
    }
}

fn log_state(call_id: &CallId, state: CallState) {
    info!(call_id = %call_id, ?state, "call state");
}

/// Whether a finished bridge's outcome is an **unexpected** drop that WS
/// reconnect (0.7.3) should try to recover from. A clean `stop` we sent
/// (`StopSent`) or our own teardown (`ControllerHungUp`) is the call
/// ending — never reconnect. A server-side close before `stop`
/// (`ServerClosed`, incl. a bare socket close without `hangup`) or any
/// connect/IO/keepalive error is a drop → reconnect.
fn reconnect_eligible(outcome: &Result<DisconnectReason, BridgeError>) -> bool {
    match outcome {
        Ok(DisconnectReason::ServerClosed) => true,
        Ok(DisconnectReason::StopSent | DisconnectReason::ControllerHungUp) => false,
        // `server_too_slow` is a healthy connection with a slow server —
        // redialing the same endpoint wouldn't help. Definitive teardown.
        Ok(DisconnectReason::ServerTooSlow) => false,
        // `protocol_error` is a buggy server sending invalid frames —
        // redialing just repeats the violation. Definitive teardown.
        Ok(DisconnectReason::ProtocolError) => false,
        Err(_) => true,
    }
}

/// Exponential reconnect backoff: 250 ms × 2^attempt, capped at 5 s
/// (0.7.3 §4). No jitter in the first cut — attempts are few and bounded
/// by the reconnect window; a thundering herd across calls isn't a
/// concern at SiphonAI's per-node scale.
fn reconnect_backoff(attempt: u32) -> Duration {
    let shift = attempt.min(5);
    let ms = 250u64.saturating_mul(1u64 << shift);
    Duration::from_millis(ms.min(5_000))
}

/// Await an optional reconnect readiness signal. `None` parks forever
/// (the select arm's guard keeps it unselectable then) — same pattern as
/// [`recv_rec_evt`]. `&mut Receiver` is itself a `Future` (oneshot's
/// receiver is `Unpin`), so this doesn't consume the option.
async fn recv_ready(
    rx: &mut Option<oneshot::Receiver<std::time::Instant>>,
) -> Result<std::time::Instant, oneshot::error::RecvError> {
    match rx.as_mut() {
        Some(r) => r.await,
        None => std::future::pending().await,
    }
}

/// Fire a lifecycle webhook off the control loop (best-effort, like the
/// acceptor's call_start/end). `None` sink → no-op.
fn spawn_webhook(sink: &Option<WebhookSinkHandle>, event: WebhookEvent) {
    if let Some(sink) = sink {
        let sink = sink.clone();
        tokio::spawn(async move {
            sink.emit(event).await;
        });
    }
}

/// Spawn the async conference join (round-trips to the room task) off
/// the control loop, reporting the outcome on `conf_join_tx`. Shared by
/// the WS `conference_join` path and the admin cross-call command path.
/// Returns `false` (nothing spawned) when conferencing is disabled — the
/// caller then emits `error{conference_failed}`.
fn spawn_conference_join(
    conference: Option<&ConferenceRegistry>,
    call_id: &CallId,
    sample_rate: u32,
    conf_join_tx: &mpsc::Sender<Result<RoomMembership, ConferenceError>>,
    room_id: String,
) -> bool {
    let Some(registry) = conference else {
        warn!(call_id = %call_id, %room_id, "conference join requested but conferencing is disabled");
        return false;
    };
    let registry = registry.clone();
    let tx = conf_join_tx.clone();
    let join_call_id = call_id.as_str().to_string();
    tokio::spawn(async move {
        let outcome = registry.join(&room_id, &join_call_id, sample_rate).await;
        let _ = tx.try_send(outcome);
    });
    true
}

/// Forward a recording control to the writer (best-effort, non-blocking).
/// `None` sender → recording isn't enabled for this call; log and drop.
fn route_rec_control(
    tx: &Option<mpsc::Sender<RecControl>>,
    ctrl: RecControl,
    label: &str,
    cid: &CallId,
) {
    match tx {
        Some(tx) => {
            debug!(ws_call_id = %cid, label, "forwarding recording control to writer");
            if let Err(e) = tx.try_send(ctrl) {
                warn!(error = %e, label, "recording control channel full or closed; dropping");
            }
        }
        None => debug!(
            ws_call_id = %cid,
            label,
            "recording control ignored; recording not enabled for this call"
        ),
    }
}

/// `select!`-friendly receive over the optional recording-event channel.
/// Pends forever when there's no channel (recording off, or the writer
/// already ended), so the arm never busy-loops.
async fn recv_announce_done(
    rx: &mut Option<tokio::sync::oneshot::Receiver<u64>>,
) -> Result<u64, tokio::sync::oneshot::error::RecvError> {
    match rx {
        Some(r) => r.await,
        None => std::future::pending().await,
    }
}

async fn recv_rec_evt(rx: &mut Option<mpsc::Receiver<RecEvent>>) -> Option<RecEvent> {
    match rx {
        Some(r) => r.recv().await,
        None => std::future::pending().await,
    }
}

/// One-shot REFER round-trip — blind, or attended when
/// `replaces_call_id` is set (DEV_PLAN_0.6.1 §2.2/§2.3). Plan →
/// dialog lookup → send_refer → classify the response. Errors are
/// returned as [`TransferOutcome`] variants (never `Result::Err`) so
/// the controller has a single match arm to handle every outcome.
/// Emits `siphon_ai_transfers_total{mode, result}` for every attempt.
async fn run_transfer(
    ctx: &TransferContext,
    target: Option<&str>,
    replaces_call_id: Option<&str>,
) -> TransferOutcome {
    let mode = if replaces_call_id.is_some() {
        "attended"
    } else {
        "blind"
    };
    let outcome = run_transfer_inner(ctx, target, replaces_call_id).await;
    let result = match &outcome {
        TransferOutcome::Accepted => "accepted",
        TransferOutcome::RemoteRejected { .. } => "rejected",
        TransferOutcome::LocalError(_) => "local_error",
    };
    metrics::counter!(TRANSFERS_TOTAL, "mode" => mode, "result" => result).increment(1);
    outcome
}

async fn run_transfer_inner(
    ctx: &TransferContext,
    target: Option<&str>,
    replaces_call_id: Option<&str>,
) -> TransferOutcome {
    let plan = match plan_refer(&ctx.consult_registry, target, replaces_call_id) {
        Ok(plan) => plan,
        Err(msg) => return TransferOutcome::LocalError(msg),
    };

    // The leg's owner keeps the canonical dialog; we operate on a
    // clone so the local CSeq the REFER consumes doesn't race other
    // requests on the same dialog. CLAUDE.md §4.4: per-call state is
    // not shared across tasks — this is the per-task copy.
    let Some(mut dialog) = ctx.control.source.resolve() else {
        return TransferOutcome::LocalError("dialog for this call is gone".to_string());
    };

    let (refer_to, consult) = match &plan {
        ReferPlan::Blind { refer_to } => (refer_to, None),
        ReferPlan::Attended { refer_to, consult } => (refer_to, Some(consult.as_ref())),
    };

    // TCP/TLS dialogs: the REFER must ride the inbound connection —
    // the dispatcher is inbound-only and the peer's Contact names an
    // ephemeral source port nothing listens on (issue #159, same
    // reasoning as the cleanup BYE in #157).
    let sent = match &ctx.control.flow {
        Some(flow) => {
            ctx.control
                .uac
                .send_refer_via_flow(&mut dialog, refer_to, consult, flow.to_uac_flow())
                .await
        }
        None => {
            ctx.control
                .uac
                .send_refer(&mut dialog, refer_to, consult)
                .await
        }
    };
    match sent {
        Ok((response, _subscription)) => {
            let status = response.code();
            debug!(
                status,
                reused_inbound_connection = ctx.control.flow.is_some(),
                "REFER sent"
            );
            if (200..300).contains(&status) {
                // RFC 5589 allows either pattern (BYE-after-202 vs.
                // staying in-dialog to consume NOTIFYs); v1 ships the
                // simpler "REFER + BYE" so the SIP dialog actually
                // tears down on the wire. Without this the peer holds
                // the dialog open until its session-expires kicks in.
                // Attended: the consult leg is NOT hung up here — the
                // transferee's INVITE-with-Replaces takes it over, and
                // its own teardown path runs when that leg ends.
                // Outbound legs skip the BYE too: their run_call
                // teardown sends it when the controller exits.
                if ctx.control.source.bye_after_refer() {
                    let bye_sent = match &ctx.control.flow {
                        Some(flow) => {
                            ctx.control
                                .uac
                                .bye_via_flow(&dialog, flow.to_uac_flow())
                                .await
                        }
                        None => ctx.control.uac.bye(&dialog).await,
                    };
                    if let Err(e) = bye_sent {
                        warn!(error = %e, "post-REFER BYE failed (dialog may linger)");
                    }
                }
                TransferOutcome::Accepted
            } else {
                TransferOutcome::RemoteRejected {
                    status,
                    reason: response.reason().to_string(),
                }
            }
        }
        Err(e) => TransferOutcome::LocalError(format!("send_refer: {e}")),
    }
}

/// Drive one bot-initiated hold/resume re-INVITE with `offer_sdp` (our
/// cached media with the direction flipped — `a=sendonly` to hold,
/// `a=sendrecv` to resume), reusing the inbound TCP/TLS connection when
/// the leg arrived over one. On 491 glare (RFC 3261 §14.1 — the peer
/// offered at the same instant) we back off once and retry on the same
/// dialog (its CSeq has already advanced). Returns `Ok(())` on a 2xx
/// (the stack auto-ACKs), `Err(reason)` on a non-2xx / network failure
/// / missing dialog — the caller maps that to `hold_failed` and leaves
/// the call in its prior media state (a failed hold never drops it).
async fn drive_hold_reinvite(
    ctx: &HoldContext,
    offer_sdp: &str,
    glare_backoff: Duration,
) -> Result<(), String> {
    let Some(mut dialog) = ctx.control.resolve() else {
        return Err("dialog for this call is gone".to_string());
    };
    let mut glare_retried = false;
    loop {
        match ctx.control.send_reinvite(&mut dialog, offer_sdp).await {
            Ok(response) => {
                let status = response.code();
                if (200..300).contains(&status) {
                    return Ok(());
                }
                if status == 491 && !glare_retried {
                    glare_retried = true;
                    debug!(
                        status,
                        "hold re-INVITE glare (491); backing off and retrying once"
                    );
                    tokio::time::sleep(glare_backoff).await;
                    continue;
                }
                return Err(format!(
                    "re-INVITE rejected: {} {}",
                    status,
                    response.reason()
                ));
            }
            Err(e) => return Err(format!("re-INVITE failed: {e}")),
        }
    }
}

#[cfg(test)]
mod handle_tests {
    use super::*;

    fn handle() -> CallHandle {
        let (be_tx, _be_rx) = mpsc::channel(1);
        let (conf_tx, _conf_rx) = mpsc::channel(1);
        let (park_tx, _park_rx) = mpsc::channel(1);
        CallHandle::new(CallId("test".into()), be_tx, conf_tx, park_tx)
    }

    #[test]
    fn drain_forced_defaults_false_and_flips() {
        let h = handle();
        assert!(!h.drain_forced());
        h.mark_drain_forced();
        assert!(h.drain_forced());
        // Drain-forced is daemon-initiated: it does NOT imply a remote
        // BYE, so the acceptor still owes the peer an outbound BYE.
        assert!(!h.remote_bye_received());
    }
}
