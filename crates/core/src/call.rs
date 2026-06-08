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

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use sip_core::SipUri;
use siphon_ai_bridge::{
    connect_and_run, BridgeChannels, BridgeConfig, BridgeError, BridgeIn, CallId, DisconnectReason,
    ErrorCode, OutgoingEvent, StartMsg, StopReason,
};
use siphon_ai_media_glue::{MediaTap, MediaTapError, TapCommand, TapDisconnect};
use siphon_ai_recording::{
    RecFrame, RecordingError, RecordingSetup, RecordingStats, RecordingWriter,
};
use thiserror::Error;
use tokio::sync::{mpsc, Notify};
use tokio::task::JoinHandle;
use tracing::{debug, info, instrument, warn};

use crate::transfer::{TransferContext, TransferOutcome};

/// Bounded channel capacity for audio frames. 10 × 20 ms = 200 ms
/// of audio; per CLAUDE.md §6.2 audio channels are bounded for
/// roughly that span.
const AUDIO_CHANNEL_CAPACITY: usize = 10;

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
    /// the controller's main select loop. Cloned from the same
    /// sender the controller and tap push onto, so all three
    /// producers feed one consumer (the bridge task).
    bridge_events_tx: mpsc::Sender<OutgoingEvent>,
}

impl CallHandle {
    fn new(call_id: CallId, bridge_events_tx: mpsc::Sender<OutgoingEvent>) -> Self {
        Self {
            notify: std::sync::Arc::new(Notify::new()),
            call_id,
            remote_bye: std::sync::Arc::new(AtomicBool::new(false)),
            bridge_events_tx,
        }
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
        if let Err(e) = self.bridge_events_tx.try_send(event) {
            tracing::warn!(
                call_id = %self.call_id,
                error = %e,
                "bridge_events_tx full or closed; dropping external event"
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
}

impl CallController {
    /// Construct a controller. Returns it together with a
    /// [`CallHandle`] the spawner can use to signal shutdown.
    pub fn new(cfg: CallControllerConfig) -> (Self, CallHandle) {
        let (control_out_tx, control_out_rx) =
            mpsc::channel::<OutgoingEvent>(CONTROL_CHANNEL_CAPACITY);
        let handle = CallHandle::new(cfg.call_id.clone(), control_out_tx);
        (
            Self {
                cfg,
                handle: handle.clone(),
                control_out_rx,
            },
            handle,
        )
    }

    pub fn handle(&self) -> &CallHandle {
        &self.handle
    }

    /// Run the call to completion.
    ///
    /// Returns when all sub-tasks have terminated. The returned
    /// [`CallOutcome`] is the source of truth for what happened.
    #[instrument(skip(self), fields(call_id = %self.cfg.call_id))]
    pub async fn run(self) -> Result<CallOutcome, CallError> {
        let CallController {
            cfg,
            handle,
            control_out_rx,
        } = self;
        let CallControllerConfig {
            call_id,
            bridge,
            start,
            media_tap,
            transfer,
            recording,
        } = cfg;

        log_state(&call_id, CallState::Initializing);

        // ─── Channels ────────────────────────────────────────────
        // tap → bridge: 20 ms PCM16 frames from caller
        let (caller_audio_tx, caller_audio_rx) = mpsc::channel::<Vec<u8>>(AUDIO_CHANNEL_CAPACITY);
        // bridge → tap: 20 ms PCM16 frames from server (playout)
        let (playout_audio_tx, playout_audio_rx) = mpsc::channel::<Vec<u8>>(AUDIO_CHANNEL_CAPACITY);
        // controller → bridge: outgoing events (Stop, Mark, …). The
        // sender lives on the CallHandle so external callers
        // (acceptor's on_reinvite) can push too. The controller
        // gets its own clone via the handle.
        let control_out_tx = handle.bridge_events_tx.clone();
        // bridge → controller: BridgeIn from server
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
        // rebound with the fork sender installed before it's spawned.
        let mut recording_task: Option<JoinHandle<Result<RecordingStats, RecordingError>>> = None;
        let media_tap = if let Some(setup) = recording {
            let (rec_tx, rec_rx) = mpsc::channel::<RecFrame>(RECORDING_CHANNEL_CAPACITY);
            let writer = RecordingWriter::new(setup.path, media_tap.sample_rate());
            recording_task = Some(tokio::spawn(writer.run(rec_rx)));
            media_tap.with_recording(Some(rec_tx))
        } else {
            media_tap
        };

        let mut bridge_task: JoinHandle<Result<DisconnectReason, BridgeError>> =
            tokio::spawn(connect_and_run(bridge, start, channels));
        // The tap forwards out-of-band events (currently DTMF) onto
        // the same control stream the bridge reads. Cloning the
        // sender means tap and controller are independent producers
        // — bridge teardown closes the receiver, both producers see
        // the same EOF.
        let mut tap_task: JoinHandle<Result<TapDisconnect, MediaTapError>> =
            tokio::spawn(media_tap.run(
                caller_audio_tx,
                playout_audio_rx,
                control_out_tx.clone(),
                tap_cmd_rx,
            ));

        // We don't wait for the bridge handshake to declare
        // Active; the moment both tasks are spawned, audio plumbing
        // is in place. A late handshake error surfaces as the
        // bridge task ending early, which the select! picks up.
        log_state(&call_id, CallState::Active);

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
                    termination = CallTermination::LocalShutdown;
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
                        Some(BridgeIn::Transfer { call_id: cid, target }) => {
                            // RFC 3515 blind transfer. Spawn a task so
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
                                    debug!(
                                        ws_call_id = %cid,
                                        target = %target_owned,
                                        "spawning REFER task"
                                    );
                                    tokio::spawn(async move {
                                        let outcome = run_transfer(&ctx, &target_owned).await;
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

                // Bridge sub-task ended.
                res = &mut bridge_task => {
                    match res {
                        Ok(inner) => bridge_result = Some(inner),
                        Err(join_err) => {
                            warn!(?join_err, "bridge task panicked");
                            return Err(CallError::TaskJoin(join_err));
                        }
                    }
                    termination = CallTermination::BridgeEnded;
                    break;
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
                    termination = CallTermination::TapEnded;
                    break;
                }
            }
        }

        log_state(&call_id, CallState::Terminating);

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
        // Give it a budget so a slow final flush can't wedge teardown.
        if let Some(mut rec_task) = recording_task.take() {
            match tokio::time::timeout(Duration::from_millis(500), &mut rec_task).await {
                Ok(Ok(Ok(stats))) => debug!(
                    call_id = %call_id,
                    path = %stats.path.display(),
                    frames = stats.frames,
                    "recording written"
                ),
                Ok(Ok(Err(e))) => warn!(call_id = %call_id, error = %e, "recording failed"),
                Ok(Err(join_err)) => {
                    warn!(call_id = %call_id, error = %join_err, "recording task panicked")
                }
                Err(_) => {
                    warn!(call_id = %call_id, "recording did not finalize within 500 ms; aborting");
                    rec_task.abort();
                    let _ = (&mut rec_task).await;
                }
            }
        }

        log_state(&call_id, CallState::Done);

        Ok(CallOutcome {
            call_id,
            termination,
            bridge: bridge_result,
            tap: tap_result,
        })
    }
}

fn log_state(call_id: &CallId, state: CallState) {
    info!(call_id = %call_id, ?state, "call state");
}

/// One-shot REFER round-trip. Lookup → URI parse → send_refer →
/// classify the response. Errors are returned as [`TransferOutcome`]
/// variants (never `Result::Err`) so the controller has a single
/// match arm to handle every outcome.
async fn run_transfer(ctx: &TransferContext, target: &str) -> TransferOutcome {
    let refer_to = match SipUri::parse(target) {
        Ok(uri) => uri,
        Err(e) => {
            return TransferOutcome::LocalError(format!("invalid transfer target {target:?}: {e}"));
        }
    };

    // The UAS owns the canonical dialog; we operate on a clone so the
    // local CSeq the REFER consumes doesn't race the next inbound
    // request on the same dialog. CLAUDE.md §4.4: per-call state is
    // not shared across tasks — this is the per-task copy.
    let Some(mut dialog) = ctx.dialog_manager.find_by_call_id(&ctx.sip_call_id) else {
        return TransferOutcome::LocalError(format!(
            "no dialog for sip_call_id {:?}",
            ctx.sip_call_id
        ));
    };

    match ctx.uac.send_refer(&mut dialog, &refer_to, None).await {
        Ok((response, _subscription)) => {
            let status = response.code();
            if (200..300).contains(&status) {
                // RFC 5589 allows either pattern (BYE-after-202 vs.
                // staying in-dialog to consume NOTIFYs); v1 ships the
                // simpler "REFER + BYE" so the SIP dialog actually
                // tears down on the wire. Without this the peer holds
                // the dialog open until its session-expires kicks in.
                if let Err(e) = ctx.uac.bye(&dialog).await {
                    warn!(error = %e, "post-REFER BYE failed (dialog may linger)");
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
