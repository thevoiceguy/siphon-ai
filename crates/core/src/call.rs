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

use std::time::Duration;

use siphon_ai_bridge::{
    connect_and_run, BridgeChannels, BridgeConfig, BridgeError, BridgeIn, CallId, DisconnectReason,
    OutgoingEvent, StartMsg, StopReason,
};
use siphon_ai_media_glue::{MediaTap, MediaTapError, TapDisconnect};
use thiserror::Error;
use tokio::sync::{mpsc, Notify};
use tokio::task::JoinHandle;
use tracing::{debug, info, instrument, warn};

/// Bounded channel capacity for audio frames. 10 × 20 ms = 200 ms
/// of audio; per CLAUDE.md §6.2 audio channels are bounded for
/// roughly that span.
const AUDIO_CHANNEL_CAPACITY: usize = 10;

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
}

impl CallHandle {
    fn new(call_id: CallId) -> Self {
        Self {
            notify: std::sync::Arc::new(Notify::new()),
            call_id,
        }
    }

    /// Ask the controller to shut down cleanly. The controller
    /// will drain audio briefly, send `stop` over the bridge, and
    /// return.
    pub fn shutdown(&self) {
        self.notify.notify_one();
    }

    pub fn call_id(&self) -> &CallId {
        &self.call_id
    }
}

/// The controller itself.
pub struct CallController {
    cfg: CallControllerConfig,
    handle: CallHandle,
}

impl CallController {
    /// Construct a controller. Returns it together with a
    /// [`CallHandle`] the spawner can use to signal shutdown.
    pub fn new(cfg: CallControllerConfig) -> (Self, CallHandle) {
        let handle = CallHandle::new(cfg.call_id.clone());
        (
            Self {
                cfg,
                handle: handle.clone(),
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
        let CallController { cfg, handle } = self;
        let CallControllerConfig {
            call_id,
            bridge,
            start,
            media_tap,
        } = cfg;

        log_state(&call_id, CallState::Initializing);

        // ─── Channels ────────────────────────────────────────────
        // tap → bridge: 20 ms PCM16 frames from caller
        let (caller_audio_tx, caller_audio_rx) = mpsc::channel::<Vec<u8>>(AUDIO_CHANNEL_CAPACITY);
        // bridge → tap: 20 ms PCM16 frames from server (playout)
        let (playout_audio_tx, playout_audio_rx) = mpsc::channel::<Vec<u8>>(AUDIO_CHANNEL_CAPACITY);
        // controller → bridge: outgoing events (Stop, Mark, …)
        let (control_out_tx, control_out_rx) =
            mpsc::channel::<OutgoingEvent>(CONTROL_CHANNEL_CAPACITY);
        // bridge → controller: BridgeIn from server
        let (control_in_tx, mut control_in_rx) =
            mpsc::channel::<BridgeIn>(CONTROL_CHANNEL_CAPACITY);

        let channels = BridgeChannels {
            audio_out_rx: caller_audio_rx,
            control_out_rx,
            audio_in_tx: playout_audio_tx,
            control_in_tx,
        };

        // ─── Spawn sub-tasks ─────────────────────────────────────
        log_state(&call_id, CallState::Connecting);

        let mut bridge_task: JoinHandle<Result<DisconnectReason, BridgeError>> =
            tokio::spawn(connect_and_run(bridge, start, channels));
        // The tap forwards out-of-band events (currently DTMF) onto
        // the same control stream the bridge reads. Cloning the
        // sender means tap and controller are independent producers
        // — bridge teardown closes the receiver, both producers see
        // the same EOF.
        let mut tap_task: JoinHandle<Result<TapDisconnect, MediaTapError>> = tokio::spawn(
            media_tap.run(caller_audio_tx, playout_audio_rx, control_out_tx.clone()),
        );

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

                // External cancel — drive cooperative teardown.
                _ = shutdown.notified() => {
                    info!(call_id = %call_id, "controller shutdown requested");
                    termination = CallTermination::LocalShutdown;
                    let _ = control_out_tx
                        .send(OutgoingEvent::Stop { reason: StopReason::ServerHangup })
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
                        Some(other) => {
                            // Clear / Mark / Transfer / SendDtmf:
                            // log for now; full handling lands with
                            // their respective glue layers.
                            debug!(?other, "control message not yet handled");
                        }
                        None => {
                            // Bridge dropped the sender — let the
                            // bridge_task arm fire next.
                            debug!("control_in_rx closed; awaiting bridge task to settle");
                            control_open = false;
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
