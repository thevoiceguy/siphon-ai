//! Bidirectional audio tap on top of forge-engine's `MediaBridgeManager`.
//!
//! This is the integration point identified by the Week-1 spike
//! (`docs/SPIKE_MEDIA_TAP.md`): one tap per call, attached to forge's
//! `MediaBridgeManager`, exposing PCM16-LE byte channels the
//! `CallController` wires straight into the bridge crate.
//!
//! ## What flows where
//!
//! ```text
//!  caller's voice                                        WS server
//!       │                                                    ▲
//!       ▼                                                    │
//!   forge RTP recv ─► InboundMediaFrame ─► Reframer (20 ms) ─┘
//!                                          ─► pack_pcm16_le ─► caller_audio_tx
//!
//!  WS server                                          caller's ear
//!       │                                                    ▲
//!       ▼                                                    │
//!  playout_audio_rx ─► unpack_pcm16_le ─► OutboundMediaFrame ─► forge RTP send
//!                                         (target = MediaTarget::A)
//! ```
//!
//! ## Single-leg model
//!
//! Per the spike (§ "Single-leg vs two-leg"), forge models calls as
//! 2-legged. SiphonAI is single-leg — there's the SIP caller and a WS
//! server with no second SIP participant. We always inject playout to
//! `MediaTarget::A` (the SIP caller); a synthetic-or-quiet `B` stays
//! silent and never produces inbound frames.
//!
//! ## Hot path
//!
//! Per CLAUDE.md §4.3, the steady-state pump:
//! - Holds no `std::sync::Mutex` on the audio path.
//! - Does no blocking I/O — `select!` yields to the reactor.
//! - Allocates one `Vec<i16>` and one `Vec<u8>` per outbound frame
//!   (the `Reframer` pop and the PCM16 pack); see `siphon-ai-bridge`'s
//!   `audio` module for the budget breakdown.
//! - Validates that forge's `sample_rate` matches what the controller
//!   committed to in `start.audio.sample_rate`. A mid-call codec
//!   switch that changes the rate yields a fatal `SampleRateMismatch`.
//!
//! ## Bus-derived events (DTMF, speech)
//!
//! The tap subscribes to the daemon-wide [`EventBus`] forge publishes
//! to. Events for *this* call's `call_id` get mapped to
//! [`OutgoingEvent`] variants and forwarded through the supplied
//! sender; the bridge crate stamps `call_id` + `seq` and writes the
//! JSON text frame on the WS. v1 maps:
//!
//! - [`ForgeEvent::DtmfDigitDetected`] (only on `End`, so each press
//!   maps to one event carrying the final `duration_ms`).
//! - [`ForgeEvent::SpeechStarted`] / [`ForgeEvent::SpeechStopped`]
//!   from forge-vad. Each transition publishes once — hysteresis
//!   inside the detector filters per-frame jitter.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::idle::{IdleDetector, IdleEvent};
use crate::room::{RoomEvent, RoomMembership, RoomSender};
use crate::rtp_stats::RtpStatsTracker;

use forge_core::{CallId, DtmfDetectionMethod, DtmfEventKind, EventBus, ForgeError, ForgeEvent};
use forge_dtmf::DtmfDigit;
use forge_engine::{
    MediaBridgeHandle, MediaBridgeManager, MediaTarget, OutboundDtmfRequest, OutboundMediaFrame,
    PlayoutMode,
};
use siphon_ai_bridge::{
    pack_pcm16_le, unpack_pcm16_le, AudioError, ConferenceLeftReason, DtmfMethod, OutgoingEvent,
    Reframer,
};
use siphon_ai_recording::RecFrame;
use thiserror::Error;
use tokio::sync::{broadcast, mpsc};
use tracing::{debug, info, instrument, warn};

/// Per-frame WS playout duration. The bridge protocol fixes outbound
/// audio at 20 ms regardless of sample rate (CLAUDE.md §4.2 / docs/
/// PROTOCOL.md §3) — the inbound tap reframes whatever forge gives us
/// down to 20 ms before sending it on the WS, and the outbound side
/// receives 20 ms frames from the WS server. Estimated play-out time
/// of frame K is therefore `first_audio_pushed_at + K * 20ms`.
const PLAYOUT_FRAME_MS: u64 = 20;

#[derive(Debug, Error)]
pub enum MediaTapError {
    /// `MediaBridgeManager::attach_call` rejected (already attached).
    #[error("forge attach failed: {0}")]
    AttachFailed(String),

    /// forge's `send_audio` returned an error (bridge channel closed,
    /// resource limit, etc.).
    #[error("forge playout failed: {0}")]
    PlayoutFailed(String),

    /// forge handed us a frame whose sample rate doesn't match what the
    /// tap was configured for.
    #[error("sample rate mismatch: expected {expected} Hz, got {got} Hz")]
    SampleRateMismatch { expected: u32, got: u32 },

    /// The controller-supplied `playout_audio_rx` carried bytes that
    /// don't form a valid PCM16-LE buffer.
    #[error(transparent)]
    Audio(#[from] AudioError),
}

/// Out-of-band commands the controller can drive the tap with —
/// distinct from the audio path because they're rare control-plane
/// events, not per-frame data.
///
/// Each variant maps to a forge `MediaBridgeHandle` action. The tap
/// translates the WS server's intent (`BridgeIn::SendDtmf`, future
/// `Clear`, future `Mark`) into the right forge call.
///
/// New variants are additive — older controllers / tests building
/// only `SendDtmf` keep working without any match-arm churn here.
/// (Not `Clone` since 0.7.0: `JoinRoom` carries the room sink
/// receivers, which are single-owner by nature. No call site cloned
/// commands anyway — they're built in place and `try_send`-ed.)
#[derive(Debug)]
#[non_exhaustive]
pub enum TapCommand {
    /// Server-driven outbound DTMF press. The tap turns it into an
    /// [`OutboundDtmfRequest`] targeting [`MediaTarget::A`] (the SIP
    /// caller); forge's encoder writes the RFC 2833 packets onto the
    /// session's RTP socket.
    ///
    /// Invalid `digit` chars (anything outside `0-9 * # A-D`) are
    /// dropped with a warning rather than tearing the call down — a
    /// misbehaving WS server shouldn't kill a call.
    SendDtmf { digit: char, duration_ms: u32 },

    /// Drop every byte of pending outbound playout — both the bytes
    /// queued in the controller→tap audio channel that haven't yet
    /// reached forge, and forge's own encoder queue. Drives the WS
    /// protocol's `Clear` message: the server's typical use is
    /// "stop talking immediately" in response to caller barge-in,
    /// so any tail audio left in the pipe must not reach the
    /// caller.
    Clear,

    /// Insert a named marker into the playout stream. The tap
    /// estimates when the audio queued so far will have played out
    /// (frames pushed to forge × 20 ms, anchored at the wallclock
    /// of the first push) and fires a single
    /// [`OutgoingEvent::Mark`] back to the WS server at that
    /// moment. If no audio is queued, the mark fires immediately.
    ///
    /// Servers use marks to know when their TTS prompt has finished
    /// playing — e.g., "after the 'Please wait' clip is done, start
    /// listening for an answer." See `docs/PROTOCOL.md` §3 for the
    /// wire-side contract.
    ///
    /// v1 caveat: the estimate doesn't account for forge's internal
    /// playout jitter buffer (≈ 60 ms per the dev plan), so the
    /// fired mark precedes the real audio reaching the caller's ear
    /// by that buffer's depth. Acceptable for prompt-completion
    /// signalling; tighter sync needs a forge upstream PR exposing a
    /// per-frame playout-completion hook.
    Mark { name: String },

    /// Suspend AI-side playout. Backed by a sustained `muted` flag
    /// on the tap: incoming bytes from the controller→tap channel
    /// are drained and dropped, and forge's queue is flushed so the
    /// caller hears silence immediately rather than after the
    /// already-queued tail. Pairs with [`TapCommand::Unmute`].
    Mute,

    /// Resume AI-side playout after [`TapCommand::Mute`]. A no-op
    /// if the tap is not currently muted.
    Unmute,

    /// Re-plumb this call into a conference room (0.7.0 §2.1). The
    /// membership comes from `RoomHandle::join` (driven by core's
    /// `ConferenceRegistry`). While joined:
    /// - caller frames go to the room (as the `sip` participant)
    ///   instead of straight to the WS;
    /// - WS playout goes to the room (as the `ws` participant)
    ///   instead of straight to forge/RTP;
    /// - RTP out is fed the room's mix-minus-`sip` sink;
    /// - the WS hears the room's mix-minus-`ws` sink.
    ///
    /// If the tap is already in a room it leaves it first (the old
    /// membership drops, which signals the old room). Leaving — or
    /// the room dying — always restores the direct caller↔WS pair.
    JoinRoom { membership: RoomMembership },

    /// Leave the current room and restore the direct caller↔WS
    /// pair. A no-op when not in a room.
    LeaveRoom,
}

/// How the tap reacts to forge-vad `SpeechStarted` events. Mirrors
/// `siphon-ai-core`'s public `BargeInMode` shape but lives in this
/// crate so media-glue doesn't have to take a backwards dep on
/// core. The acceptor translates `BargeInConfig` (`enabled` +
/// `mode`) into one of these at call-setup time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BargeInAction {
    /// Just forward the WS event. The server decides whether to
    /// send `clear`.
    Notify,
    /// Drop pending outbound playout (drain the tap-side audio
    /// channel + ask forge to flush leg A) AND forward the event.
    AutoClear,
}

/// Why the tap pump exited cleanly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TapDisconnect {
    /// forge's inbound stream returned `None` — the SIP-side media
    /// session ended (call hung up).
    CallEnded,
    /// The controller dropped at least one of the audio channels.
    /// Equivalent to "the bridge is gone, so there's no point reading
    /// or writing audio anymore".
    ControllerHungUp,
    /// No inbound RTP for the configured
    /// `[media].inactivity_timeout_secs` window. RFC-3550 says
    /// nothing about how long a stalled stream is "stalled enough,"
    /// so this is operator policy: a peer that's gone away on the
    /// network shouldn't keep a forge session alive forever.
    InactivityTimeout,
}

/// Bidirectional audio tap attached to one call's `MediaBridgeHandle`.
///
/// Construct via [`MediaTap::attach`], then drive with [`MediaTap::run`].
pub struct MediaTap {
    handle: MediaBridgeHandle,
    sample_rate: u32,
    call_id: CallId,
    /// Subscription to the daemon-wide forge [`EventBus`]. The tap
    /// filters for events matching its own `call_id`. Subscribing at
    /// `attach` time (rather than inside `run`) means events emitted
    /// between attach and run are still buffered for us — the
    /// broadcast channel's per-receiver queue holds up to the bus's
    /// configured capacity.
    events_rx: broadcast::Receiver<ForgeEvent>,
    /// Held alive across calls to [`Self::run`] so that, if the
    /// daemon-wide bus's `Sender` is dropped (test fixtures that don't
    /// keep the `Arc<EventBus>` alive, or a future shutdown sequence),
    /// the swapped-in fallback receiver doesn't immediately re-close.
    /// Without this anchor the `events_rx` `select!` arm would fire
    /// `Closed` every poll under `biased;`, starving the other arms.
    /// `None` while the original bus is still feeding us; `Some`
    /// after we've swapped in a fallback channel.
    events_keepalive: Option<broadcast::Sender<ForgeEvent>>,
    /// What this call does when forge-vad reports speech started.
    /// Resolved from `[bridge].barge_in` + `[route.bridge].barge_in`
    /// by the acceptor; passed in at `attach` time. Defaults to
    /// `Notify` on the bare `attach()` entry point so test fixtures
    /// that don't care about barge-in don't have to thread it
    /// through.
    barge_in_action: BargeInAction,
    /// RTP watchdog window. `None` disables the watchdog entirely;
    /// `Some(d)` means "if no inbound frame arrives within `d`,
    /// return `TapDisconnect::InactivityTimeout`." Settable on the
    /// fully-built tap via [`Self::with_inactivity_timeout`] so the
    /// 4-arg `attach()` form in tests stays terse.
    inactivity_timeout: Option<Duration>,
    /// AI-side playout gate. Set by [`TapCommand::Mute`], cleared by
    /// [`TapCommand::Unmute`]. While `true`, the playout arm drains
    /// the controller→tap audio channel but drops the bytes instead
    /// of pushing them into forge — the caller hears silence and
    /// the WS server isn't back-pressured.
    muted: bool,
    /// Timer-based derivation of `silence_detected` / `dead_air_detected`
    /// events. Polled on a 500 ms tick in `run()` when at least one
    /// threshold is configured; receives `note_speech_started` on
    /// every forge-vad speech-started, and `note_ws_audio` on every
    /// playout byte arriving from the WS server. Initialized with
    /// both thresholds `None` (disabled); the acceptor calls
    /// [`Self::with_idle_thresholds`] before `run()` to install the
    /// resolved per-call values.
    idle_detector: IdleDetector,
    /// Cached most-recent RTP-quality assessment from forge,
    /// emitted as periodic `rtp_stats` WS events. Disabled by
    /// default; the acceptor calls
    /// [`Self::with_rtp_stats_interval`] before `run()`. See
    /// `crates/media-glue/src/rtp_stats.rs` for the rationale.
    rtp_stats: RtpStatsTracker,
    /// Recording fork. `None` (default) → no recording. When set, the tap
    /// `try_send`s a copy of each leg's 20 ms frame to the sender —
    /// best-effort and non-blocking, so a backed-up recording writer never
    /// stalls the audio path (CLAUDE.md §4.3). A `Full` channel increments
    /// the shared drop counter (→ the recording is flagged `degraded`); a
    /// `Closed` channel (writer already stopped) is ignored. Set by the
    /// `CallController` before `run()`.
    recording: Option<(mpsc::Sender<RecFrame>, Arc<AtomicU64>)>,
}

impl std::fmt::Debug for MediaTap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // forge's `MediaBridgeHandle` doesn't impl Debug; redact it.
        f.debug_struct("MediaTap")
            .field("call_id", &self.call_id)
            .field("sample_rate", &self.sample_rate)
            .finish_non_exhaustive()
    }
}

impl MediaTap {
    /// Attach a new tap for `call_id` against the process-wide
    /// `MediaBridgeManager` and subscribe to the supplied `EventBus`
    /// (the same bus forge's session manager publishes on).
    ///
    /// Fails with [`MediaTapError::AttachFailed`] if a tap is already
    /// attached for the same call (forge enforces 1 handle per call).
    /// Also fails on an unsupported `sample_rate` per the bridge
    /// audio module's rules (8 kHz or 16 kHz only in v1).
    pub fn attach(
        manager: &Arc<MediaBridgeManager>,
        event_bus: &Arc<EventBus>,
        call_id: CallId,
        sample_rate: u32,
    ) -> Result<Self, MediaTapError> {
        Self::attach_with_barge_in(
            manager,
            event_bus,
            call_id,
            sample_rate,
            BargeInAction::Notify,
        )
    }

    /// Same as [`Self::attach`] but with an explicit barge-in policy.
    /// The 4-arg form is kept so test fixtures that don't care
    /// about barge-in don't have to thread the policy through.
    pub fn attach_with_barge_in(
        manager: &Arc<MediaBridgeManager>,
        event_bus: &Arc<EventBus>,
        call_id: CallId,
        sample_rate: u32,
        barge_in_action: BargeInAction,
    ) -> Result<Self, MediaTapError> {
        // Validate sample rate up front so we don't attach a handle
        // we'd immediately have to detach.
        let _ = siphon_ai_bridge::samples_per_frame(sample_rate)?;
        let handle = manager
            .attach_call(call_id.clone())
            .map_err(forge_attach_err)?;
        let events_rx = event_bus.subscribe();
        Ok(Self {
            handle,
            sample_rate,
            call_id,
            events_rx,
            events_keepalive: None,
            barge_in_action,
            inactivity_timeout: None,
            muted: false,
            idle_detector: IdleDetector::new(None, None, Instant::now()),
            rtp_stats: RtpStatsTracker::new(None),
            recording: None,
        })
    }

    /// Override the inactivity watchdog window. The acceptor calls
    /// this after `attach_with_barge_in` so the route's resolved
    /// `inactivity_timeout_secs` lands on the tap before `run` starts.
    pub fn with_inactivity_timeout(mut self, timeout: Option<Duration>) -> Self {
        self.inactivity_timeout = timeout;
        self
    }

    /// Install the silence / dead-air thresholds resolved from
    /// `[bridge].silence_threshold_ms` + `[bridge].dead_air_threshold_ms`
    /// (and any per-route override). `None` disables that event for
    /// this call. Acceptor calls this after `attach_with_barge_in`
    /// before `run()`.
    pub fn with_idle_thresholds(
        mut self,
        silence: Option<Duration>,
        dead_air: Option<Duration>,
    ) -> Self {
        self.idle_detector = IdleDetector::new(silence, dead_air, Instant::now());
        self
    }

    /// Install the periodic `rtp_stats` emission cadence resolved
    /// from `[bridge].rtp_stats_interval_ms` (and any per-route
    /// override). `None` disables the event for this call.
    pub fn with_rtp_stats_interval(mut self, interval: Option<Duration>) -> Self {
        self.rtp_stats = RtpStatsTracker::new(interval);
        self
    }

    /// Install the recording fork: a sender for the per-leg frame copies and
    /// a shared counter the tap bumps when the channel is full (drops →
    /// `degraded`). `None` (default) is no recording. The `CallController`
    /// sets this before `run()` when the call is being recorded.
    pub fn with_recording(
        mut self,
        recording: Option<(mpsc::Sender<RecFrame>, Arc<AtomicU64>)>,
    ) -> Self {
        self.recording = recording;
        self
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn call_id(&self) -> &CallId {
        &self.call_id
    }

    /// Drive the bidirectional pump until the call ends or the
    /// controller drops its channels.
    ///
    /// Channels:
    /// - `caller_audio_tx`: each completed 20 ms PCM16-LE frame from
    ///   the SIP caller is sent here (the bridge crate forwards into
    ///   the WS as a binary frame).
    /// - `playout_audio_rx`: bytes the bridge crate received from the
    ///   WS server arrive here; the tap unpacks and hands them to
    ///   forge for playout into the SIP leg.
    /// - `events_tx`: out-of-band [`OutgoingEvent`]s the tap derives
    ///   from forge's event bus (currently only DTMF). The bridge
    ///   crate stamps `call_id` + `seq` and writes the JSON text
    ///   frame on the WS.
    /// - `commands_rx`: control-plane requests from the controller
    ///   (currently [`TapCommand::SendDtmf`]). The tap translates
    ///   each into the right forge `MediaBridgeHandle` call.
    #[instrument(skip_all, fields(call_id = %self.call_id, sample_rate = self.sample_rate))]
    pub async fn run(
        mut self,
        caller_audio_tx: mpsc::Sender<Vec<u8>>,
        mut playout_audio_rx: mpsc::Receiver<Vec<u8>>,
        events_tx: mpsc::Sender<OutgoingEvent>,
        mut commands_rx: mpsc::Receiver<TapCommand>,
    ) -> Result<TapDisconnect, MediaTapError> {
        let mut reframer = Reframer::new(self.sample_rate)?;
        // Play-out estimation state for `TapCommand::Mark`.
        // Updated in the playout arm; read in the command arm.
        let mut frames_sent_to_forge: u64 = 0;
        let mut first_audio_pushed_at: Option<Instant> = None;

        // Conference-room routing (0.7.0 §2.1). `None` = the direct
        // caller↔WS pair. Three separate locals (not one struct) so
        // the `select!` arms below can borrow the two sink receivers
        // independently. Installed by `TapCommand::JoinRoom`; torn
        // down by `LeaveRoom` or by either sink closing (= the room
        // died), which always restores the direct pair.
        let mut room_send: Option<RoomSender> = None;
        let mut room_sip_out: Option<mpsc::Receiver<Vec<i16>>> = None;
        let mut room_ws_out: Option<mpsc::Receiver<Vec<i16>>> = None;
        // Membership-change fan-out from the room → `participant_joined`
        // / `participant_left` WS events.
        let mut room_events: Option<mpsc::Receiver<RoomEvent>> = None;

        // RTP watchdog. Pinned so we can reset the deadline in place
        // every time an inbound frame arrives. When the timeout is
        // `None` we still pin a far-future sleep so the select arm
        // is structurally identical — the arm just never fires.
        let inactivity = self.inactivity_timeout;
        let watchdog_initial = inactivity.unwrap_or(Duration::from_secs(86_400));
        let watchdog = tokio::time::sleep(watchdog_initial);
        tokio::pin!(watchdog);

        // Silence / dead-air poll tick. 500 ms cadence gives ±500 ms
        // detection accuracy against thresholds of 3 s / 10 s — fine
        // for the WS-server "are you still there?" use case. The
        // first tick fires after the period (not immediately) so we
        // don't spam events on a freshly-connected call.
        let mut idle_check = tokio::time::interval(Duration::from_millis(500));
        idle_check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // Consume the immediate first tick that `interval` emits, so
        // the arm doesn't fire at t=0 against a fresh detector.
        idle_check.tick().await;

        // Periodic `rtp_stats` emission. When the tracker is
        // disabled (`with_rtp_stats_interval(None)`) the arm guard
        // suppresses ticks; we still need a placeholder interval so
        // the future type unifies — pick 1h, which never fires.
        let rtp_stats_period = self
            .rtp_stats
            .interval()
            .unwrap_or(Duration::from_secs(3600));
        let mut rtp_stats_tick = tokio::time::interval(rtp_stats_period);
        rtp_stats_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        rtp_stats_tick.tick().await;

        loop {
            tokio::select! {
                biased;

                // Controller dropped the caller_audio_tx receiver →
                // tear down immediately, even if no inbound frame is
                // pending.
                _ = caller_audio_tx.closed() => {
                    debug!("caller_audio_tx receiver dropped; ending tap");
                    return Ok(TapDisconnect::ControllerHungUp);
                }

                // RTP watchdog. `if inactivity.is_some()` makes the
                // arm un-selectable when the operator turned the
                // watchdog off — `tokio::select!` skips the arm
                // entirely rather than registering its waker, so the
                // far-future sleep never costs anything.
                _ = &mut watchdog, if inactivity.is_some() => {
                    warn!(
                        call_id = %self.call_id,
                        timeout_ms = inactivity.unwrap().as_millis() as u64,
                        "no inbound RTP within inactivity window; tearing down",
                    );
                    return Ok(TapDisconnect::InactivityTimeout);
                }

                // Server → caller: bytes from the WS into forge's playout.
                playout = playout_audio_rx.recv() => {
                    let Some(bytes) = playout else {
                        debug!("playout_audio_rx closed; ending tap");
                        return Ok(TapDisconnect::ControllerHungUp);
                    };
                    // Idle-detector: any WS-side push counts as
                    // "audio activity" for dead-air detection,
                    // regardless of mute state — a muted call where
                    // the WS server keeps streaming is NOT dead air,
                    // it's intentional silence.
                    self.idle_detector.note_ws_audio(Instant::now());
                    // `TapCommand::Mute` gates AI-side playout. We
                    // still recv (draining the channel keeps WS
                    // backpressure away) but skip unpack + forge so
                    // the caller hears silence. Mark bookkeeping
                    // intentionally untouched — frames we drop never
                    // play, so they don't move the "audio queued so
                    // far" clock.
                    if self.muted {
                        continue;
                    }
                    // In a room, bot playout becomes the `ws`
                    // participant's input — the room mixes it for
                    // everyone (this caller included, via the
                    // mix-minus-`sip` sink). The Bot recording fork
                    // moves to the sip_out arm below: "what the
                    // caller actually heard" is the room mix there,
                    // not this leg's raw playout.
                    if let Some(send) = &room_send {
                        let samples = unpack_pcm16_le(&bytes)?;
                        send.send_ws(samples);
                        continue;
                    }
                    // Recording fork (right channel): only non-muted playout
                    // is recorded, so a muted span shows as silence on the
                    // bot side — what the caller actually heard.
                    if let Some((rec, drops)) = &self.recording {
                        if let Err(mpsc::error::TrySendError::Full(_)) =
                            rec.try_send(RecFrame::Bot(bytes.clone()))
                        {
                            drops.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    let samples = unpack_pcm16_le(&bytes)?;
                    let frame = OutboundMediaFrame {
                        target: MediaTarget::A,
                        sample_rate: self.sample_rate,
                        samples,
                        playback_id: None,
                        mode: PlayoutMode::Append,
                    };
                    self.handle
                        .send_audio(frame)
                        .await
                        .map_err(|e| MediaTapError::PlayoutFailed(e.to_string()))?;
                    // Bookkeeping for `TapCommand::Mark`: count
                    // frames pushed to forge and stamp the wallclock
                    // of the first push so we can estimate when the
                    // Nth queued frame finishes playing.
                    if frames_sent_to_forge == 0 {
                        first_audio_pushed_at = Some(Instant::now());
                    }
                    frames_sent_to_forge += 1;
                }

                // Caller → server: PCM16 samples from forge, reframed and packed.
                inbound = self.handle.recv_frame() => {
                    let Some(frame) = inbound else {
                        debug!("forge handle stream ended");
                        return Ok(TapDisconnect::CallEnded);
                    };
                    if frame.sample_rate != self.sample_rate {
                        warn!(
                            expected = self.sample_rate,
                            got = frame.sample_rate,
                            "sample rate mismatch on inbound frame; tearing down tap"
                        );
                        return Err(MediaTapError::SampleRateMismatch {
                            expected: self.sample_rate,
                            got: frame.sample_rate,
                        });
                    }
                    // Reset the inactivity deadline on every received
                    // frame. Cheap; `Sleep::reset` reuses the timer.
                    if let Some(d) = inactivity {
                        watchdog.as_mut().reset(tokio::time::Instant::now() + d);
                    }
                    reframer.push(&frame.samples);
                    while let Some(samples) = reframer.pop_frame() {
                        // In a room, caller frames feed the `sip`
                        // participant; the WS instead receives the
                        // room's mix-minus-`ws` sink (arm below).
                        // The Caller recording fork is unchanged —
                        // the left channel is always this caller's
                        // own voice.
                        if let Some(send) = &room_send {
                            if let Some((rec, drops)) = &self.recording {
                                if let Err(mpsc::error::TrySendError::Full(_)) =
                                    rec.try_send(RecFrame::Caller(pack_pcm16_le(&samples)))
                                {
                                    drops.fetch_add(1, Ordering::Relaxed);
                                }
                            }
                            send.send_sip(samples);
                            continue;
                        }
                        let bytes = pack_pcm16_le(&samples);
                        // Recording fork (left channel), best-effort.
                        if let Some((rec, drops)) = &self.recording {
                            if let Err(mpsc::error::TrySendError::Full(_)) =
                                rec.try_send(RecFrame::Caller(bytes.clone()))
                            {
                                drops.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                        if caller_audio_tx.send(bytes).await.is_err() {
                            debug!("caller_audio_tx closed; ending tap");
                            return Ok(TapDisconnect::ControllerHungUp);
                        }
                    }
                }

                // Room → caller: the mix-minus-`sip` sink, played out
                // to RTP. A closed sink means the room ended — revert
                // to the direct pair (§2.1: leave/room-death always
                // restores it).
                mixed = recv_opt(&mut room_sip_out), if room_sip_out.is_some() => {
                    let Some(samples) = mixed else {
                        revert_to_direct(
                            &events_tx, &self.call_id,
                            &mut room_send, &mut room_sip_out,
                            &mut room_ws_out, &mut room_events,
                        );
                        continue;
                    };
                    // Recording fork (right channel) in room mode:
                    // the room mix IS what the caller hears.
                    if let Some((rec, drops)) = &self.recording {
                        if let Err(mpsc::error::TrySendError::Full(_)) =
                            rec.try_send(RecFrame::Bot(pack_pcm16_le(&samples)))
                        {
                            drops.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    let frame = OutboundMediaFrame {
                        target: MediaTarget::A,
                        sample_rate: self.sample_rate,
                        samples,
                        playback_id: None,
                        mode: PlayoutMode::Append,
                    };
                    self.handle
                        .send_audio(frame)
                        .await
                        .map_err(|e| MediaTapError::PlayoutFailed(e.to_string()))?;
                    // Mark bookkeeping keeps counting forge pushes;
                    // in room mode frames arrive at playout cadence,
                    // so a Mark estimate degenerates to ≈ now — the
                    // honest answer while the room owns playout.
                    if frames_sent_to_forge == 0 {
                        first_audio_pushed_at = Some(Instant::now());
                    }
                    frames_sent_to_forge += 1;
                }

                // Room → server: the mix-minus-`ws` sink becomes the
                // "caller audio" the WS session hears.
                mixed = recv_opt(&mut room_ws_out), if room_ws_out.is_some() => {
                    let Some(samples) = mixed else {
                        revert_to_direct(
                            &events_tx, &self.call_id,
                            &mut room_send, &mut room_sip_out,
                            &mut room_ws_out, &mut room_events,
                        );
                        continue;
                    };
                    if caller_audio_tx.send(pack_pcm16_le(&samples)).await.is_err() {
                        debug!("caller_audio_tx closed; ending tap");
                        return Ok(TapDisconnect::ControllerHungUp);
                    }
                }

                // Room membership changes (another call joined/left
                // this room) → `participant_joined` / `participant_left`
                // WS events. Best-effort try_send like the other tap
                // events.
                room_evt = recv_opt(&mut room_events), if room_events.is_some() => {
                    match room_evt {
                        Some(RoomEvent::ParticipantJoined { call_id: who }) => {
                            emit_room_event(
                                &events_tx, &self.call_id, &room_send,
                                |room_id| OutgoingEvent::ParticipantJoined {
                                    room_id,
                                    participant_call_id: who,
                                },
                            );
                        }
                        Some(RoomEvent::ParticipantLeft { call_id: who }) => {
                            emit_room_event(
                                &events_tx, &self.call_id, &room_send,
                                |room_id| OutgoingEvent::ParticipantLeft {
                                    room_id,
                                    participant_call_id: who,
                                },
                            );
                        }
                        // Sender closed without the audio sinks closing
                        // (shouldn't happen — they share the room task's
                        // lifetime — but don't spin the arm if it does).
                        None => room_events = None,
                    }
                }

                // Forge events (currently only DTMF). Filter to this
                // call_id; map End-of-press to a single OutgoingEvent.
                event = self.events_rx.recv() => {
                    match event {
                        Ok(ev) => {
                            // Capture quality assessments into the
                            // rtp_stats tracker BEFORE the move into
                            // derive_outgoing_event (which doesn't
                            // forward Quality events to the WS — they
                            // surface via the periodic rtp_stats arm).
                            match &ev {
                                ForgeEvent::RtcpReportReceived {
                                    call_id: cid,
                                    jitter_ms,
                                    packet_loss_ratio,
                                    rtt_ms,
                                    ..
                                } if cid == &self.call_id => {
                                    self.rtp_stats.note_rtcp_report(
                                        *jitter_ms,
                                        *packet_loss_ratio,
                                        *rtt_ms,
                                    );
                                }
                                ForgeEvent::QualityDegraded {
                                    call_id: cid,
                                    packet_loss_percent,
                                    jitter_ms,
                                    ..
                                } if cid == &self.call_id => {
                                    self.rtp_stats.note_quality_degraded(
                                        *packet_loss_percent,
                                        *jitter_ms,
                                    );
                                }
                                ForgeEvent::QualityRestored {
                                    call_id: cid,
                                    ..
                                } if cid == &self.call_id => {
                                    self.rtp_stats.note_quality_restored();
                                }
                                _ => {}
                            }
                            if let Some(out) = derive_outgoing_event(&self.call_id, ev) {
                                // Hook the idle detector on every
                                // SpeechStarted — regardless of
                                // barge-in policy. Resets silence +
                                // dead-air timers and clears the
                                // silence-fired suppression flag so
                                // the next silence stretch can fire
                                // a fresh event.
                                if matches!(out, OutgoingEvent::SpeechStarted { .. }) {
                                    self.idle_detector.note_speech_started(Instant::now());
                                }
                                // Barge-in `auto_clear`: when forge-vad
                                // reports speech-started AND this call's
                                // policy is AutoClear, drop pending
                                // outbound playout before forwarding the
                                // WS event. The drain catches bytes in
                                // the controller→tap audio channel that
                                // haven't yet reached forge; the flush
                                // dumps forge's encoder queue. Reset the
                                // Mark bookkeeping so the next `Mark`
                                // doesn't wait on now-dropped audio.
                                if matches!(out, OutgoingEvent::SpeechStarted { .. })
                                    && self.barge_in_action == BargeInAction::AutoClear
                                {
                                    let mut drained = 0usize;
                                    while let Ok(_bytes) = playout_audio_rx.try_recv() {
                                        drained += 1;
                                    }
                                    // In a room, the queued tail also
                                    // lives in the room's ws buffer.
                                    if let Some(send) = &room_send {
                                        send.clear_ws_input();
                                    }
                                    if let Err(e) = self
                                        .handle
                                        .flush(Some(MediaTarget::A), None)
                                        .await
                                    {
                                        warn!(
                                            call_id = %self.call_id,
                                            error = %e,
                                            "forge flush failed during auto_clear",
                                        );
                                    } else {
                                        debug!(
                                            call_id = %self.call_id,
                                            drained,
                                            "auto_clear: dropped pending playout on speech",
                                        );
                                    }
                                    frames_sent_to_forge = 0;
                                    first_audio_pushed_at = None;
                                }
                                // Best-effort, mirroring
                                // `CallHandle::push_bridge_event`: a
                                // backed-up or closed bridge channel
                                // must NOT stall the audio arms of
                                // this `select!` loop, so `try_send`
                                // rather than `send().await`. Tap
                                // events (DTMF / speech / mark) are
                                // informational; a dropped one isn't
                                // fatal. Genuine controller teardown
                                // is still caught by the
                                // `caller_audio_tx` / `playout_audio_rx`
                                // arms above.
                                if let Err(e) = events_tx.try_send(out) {
                                    warn!(
                                        call_id = %self.call_id,
                                        error = %e,
                                        "events_tx full or closed; dropping tap event"
                                    );
                                }
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            // The broadcast queue overflowed for this
                            // subscriber. Per CLAUDE.md §4.5 we never
                            // panic on the audio path; just log and
                            // resume — events from the lag window are
                            // best-effort by definition.
                            warn!(skipped = n, "tap event subscriber lagged");
                        }
                        Err(broadcast::error::RecvError::Closed) => {
                            debug!("forge event bus closed; tap continues without events");
                            // Without holding a keepalive sender, the
                            // fresh receiver would also be Closed, the
                            // arm would fire on every poll, and under
                            // `biased;` the cmd / audio arms would be
                            // starved. Stash the sender on `self` so
                            // it lives as long as the tap.
                            let (dummy_tx, dummy_rx) = broadcast::channel(1);
                            self.events_keepalive = Some(dummy_tx);
                            self.events_rx = dummy_rx;
                        }
                    }
                }

                // Controller-driven commands (currently only SendDtmf).
                cmd = commands_rx.recv() => {
                    let Some(cmd) = cmd else {
                        // Controller dropped its sender. The audio
                        // arms still drive the call; treat command
                        // closure as a non-fatal "no more commands"
                        // and stop polling this arm by recreating the
                        // receiver against a dropped sender.
                        debug!("commands_rx closed; tap continues without commands");
                        let (_drop_tx, replacement) = mpsc::channel::<TapCommand>(1);
                        commands_rx = replacement;
                        continue;
                    };
                    match cmd {
                        TapCommand::Mute => {
                            // Sustained AI-side gate. Same drain +
                            // forge-flush combo as Clear so the
                            // caller hears silence immediately; the
                            // `muted` flag then drops all subsequent
                            // bytes in the playout arm until Unmute.
                            self.muted = true;
                            let mut drained = 0usize;
                            while let Ok(_bytes) = playout_audio_rx.try_recv() {
                                drained += 1;
                            }
                            if let Some(send) = &room_send {
                                send.clear_ws_input();
                            }
                            if let Err(e) = self
                                .handle
                                .flush(Some(MediaTarget::A), None)
                                .await
                            {
                                warn!(
                                    call_id = %self.call_id,
                                    error = %e,
                                    "forge flush failed during Mute",
                                );
                            } else {
                                debug!(
                                    call_id = %self.call_id,
                                    drained,
                                    "muted; flushed pending outbound playout",
                                );
                            }
                            // Mark bookkeeping reset — anything queued
                            // before the mute is gone and cannot
                            // satisfy a pending Mark estimate.
                            frames_sent_to_forge = 0;
                            first_audio_pushed_at = None;
                        }
                        TapCommand::Unmute => {
                            // Just lift the gate. No flush — there's
                            // nothing queued because the playout arm
                            // has been dropping bytes since Mute.
                            self.muted = false;
                            debug!(
                                call_id = %self.call_id,
                                "unmuted; AI playout resumes",
                            );
                        }
                        TapCommand::Clear => {
                            // Drain any bytes queued in the
                            // controller→tap audio channel before
                            // they can reach forge. The mpsc bound
                            // is small (10 frames = 200 ms) but
                            // letting that tail slip through a
                            // barge-in event would be perceptible.
                            let mut drained = 0usize;
                            while let Ok(_bytes) = playout_audio_rx.try_recv() {
                                drained += 1;
                            }
                            if let Some(send) = &room_send {
                                send.clear_ws_input();
                            }
                            if let Err(e) = self
                                .handle
                                .flush(Some(MediaTarget::A), None)
                                .await
                            {
                                warn!(
                                    call_id = %self.call_id,
                                    error = %e,
                                    "forge flush failed during Clear",
                                );
                            } else {
                                debug!(
                                    call_id = %self.call_id,
                                    drained,
                                    "cleared pending outbound playout",
                                );
                            }
                        }
                        TapCommand::SendDtmf { digit, duration_ms } => {
                            handle_send_dtmf(&self.call_id, &self.handle, digit, duration_ms).await;
                        }
                        TapCommand::Mark { name } => {
                            schedule_mark(
                                &self.call_id,
                                &events_tx,
                                name,
                                frames_sent_to_forge,
                                first_audio_pushed_at,
                            );
                        }
                        TapCommand::JoinRoom { membership } => {
                            if room_send.is_some() {
                                // Switching rooms: dropping the old
                                // RoomSender signals the old room to
                                // remove this call.
                                info!(
                                    call_id = %self.call_id,
                                    room_id = %membership.room_id(),
                                    "leaving current room to join another"
                                );
                            } else {
                                info!(
                                    call_id = %self.call_id,
                                    room_id = %membership.room_id(),
                                    "tap re-plumbed into conference room"
                                );
                            }
                            let room_id = membership.room_id().to_string();
                            let participants = membership.participants();
                            let (send, sip_out, ws_out, events) = membership.split();
                            room_send = Some(send);
                            room_sip_out = Some(sip_out);
                            room_ws_out = Some(ws_out);
                            room_events = Some(events);
                            // The command response the WS server is
                            // waiting on for its `conference_join`.
                            if let Err(e) = events_tx.try_send(OutgoingEvent::ConferenceJoined {
                                room_id,
                                participants,
                            }) {
                                warn!(
                                    call_id = %self.call_id,
                                    error = %e,
                                    "events_tx full or closed; dropping conference_joined"
                                );
                            }
                        }
                        TapCommand::LeaveRoom => {
                            if let Some(send) = room_send.take() {
                                let room_id = send.room_id().to_string();
                                // Drop the sender first so the room sees
                                // the leave before we announce it.
                                drop(send);
                                room_sip_out = None;
                                room_ws_out = None;
                                room_events = None;
                                info!(
                                    call_id = %self.call_id,
                                    %room_id,
                                    "left conference room; direct bridge restored"
                                );
                                if let Err(e) = events_tx.try_send(OutgoingEvent::ConferenceLeft {
                                    room_id,
                                    reason: ConferenceLeftReason::Left,
                                }) {
                                    warn!(
                                        call_id = %self.call_id,
                                        error = %e,
                                        "events_tx full or closed; dropping conference_left"
                                    );
                                }
                            }
                        }
                    }
                }

                // Idle-detector poll. Fires every 500 ms when at
                // least one threshold is configured; emits derived
                // `SilenceDetected` / `DeadAirDetected` events as
                // best-effort (same try_send policy as DTMF and
                // Mark).
                _ = idle_check.tick(), if self.idle_detector.is_active() => {
                    let now = Instant::now();
                    for ev in self.idle_detector.poll(now) {
                        let out = match ev {
                            IdleEvent::SilenceDetected { duration_ms } => {
                                metrics::counter!("siphon_ai_silence_events_total").increment(1);
                                OutgoingEvent::SilenceDetected { duration_ms }
                            }
                            IdleEvent::DeadAirDetected { duration_ms } => {
                                metrics::counter!("siphon_ai_dead_air_events_total").increment(1);
                                OutgoingEvent::DeadAirDetected { duration_ms }
                            }
                        };
                        if let Err(e) = events_tx.try_send(out) {
                            warn!(
                                call_id = %self.call_id,
                                error = %e,
                                "events_tx full or closed; dropping idle event"
                            );
                        }
                    }
                }

                // Periodic `rtp_stats` emission. Cadence comes from
                // `[bridge].rtp_stats_interval_ms` (default 5 s,
                // mirrors RTCP §6.2). Arm guard suppresses ticks
                // when the tracker is disabled (`interval = None`).
                // Snapshot values default to `null` until forge has
                // reported its first QualityDegraded event.
                _ = rtp_stats_tick.tick(), if self.rtp_stats.is_active() => {
                    let snap = self.rtp_stats.snapshot();
                    if let Some(j) = snap.jitter_ms {
                        metrics::histogram!("siphon_ai_rtp_jitter_ms").record(j as f64);
                    }
                    if let Some(l) = snap.packet_loss_ratio {
                        metrics::histogram!("siphon_ai_rtp_packet_loss_ratio").record(l as f64);
                    }
                    if let Some(r) = snap.rtt_ms {
                        metrics::histogram!("siphon_ai_rtp_rtt_ms").record(r as f64);
                    }
                    let out = OutgoingEvent::RtpStats {
                        jitter_ms: snap.jitter_ms,
                        packet_loss_ratio: snap.packet_loss_ratio,
                        rtcp_rtt_ms: snap.rtt_ms,
                    };
                    if let Err(e) = events_tx.try_send(out) {
                        warn!(
                            call_id = %self.call_id,
                            error = %e,
                            "events_tx full or closed; dropping rtp_stats event"
                        );
                    }
                }
            }
        }
    }
}

/// `recv` on an optional receiver. The `select!` arms that use this
/// carry an `is_some()` guard, so the `pending` branch is never
/// polled — it exists to keep the future total.
async fn recv_opt<T>(rx: &mut Option<mpsc::Receiver<T>>) -> Option<T> {
    match rx {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

/// Tear down room routing and restore the direct caller↔WS pair,
/// emitting `conference_left { room_closed }` first. Called when a
/// room sink closes — i.e. the room ended underneath us (last member
/// left, or an operator force-ended it). Idempotent on the already-
/// reverted state (no `room_send` → nothing emitted, all cleared).
fn revert_to_direct(
    events_tx: &mpsc::Sender<OutgoingEvent>,
    call_id: &CallId,
    room_send: &mut Option<RoomSender>,
    room_sip_out: &mut Option<mpsc::Receiver<Vec<i16>>>,
    room_ws_out: &mut Option<mpsc::Receiver<Vec<i16>>>,
    room_events: &mut Option<mpsc::Receiver<RoomEvent>>,
) {
    if let Some(send) = room_send.as_ref() {
        let room_id = send.room_id().to_string();
        info!(call_id = %call_id, %room_id, "room ended; reverting to direct bridge");
        if let Err(e) = events_tx.try_send(OutgoingEvent::ConferenceLeft {
            room_id,
            reason: ConferenceLeftReason::RoomClosed,
        }) {
            warn!(call_id = %call_id, error = %e, "events_tx full or closed; dropping conference_left");
        }
    }
    *room_send = None;
    *room_sip_out = None;
    *room_ws_out = None;
    *room_events = None;
}

/// Emit a participant-change WS event, tagging it with the current
/// room id. A `None` `room_send` (we left the room between the event
/// arriving and being handled) drops it silently.
fn emit_room_event(
    events_tx: &mpsc::Sender<OutgoingEvent>,
    call_id: &CallId,
    room_send: &Option<RoomSender>,
    build: impl FnOnce(String) -> OutgoingEvent,
) {
    let Some(send) = room_send.as_ref() else {
        return;
    };
    if let Err(e) = events_tx.try_send(build(send.room_id().to_string())) {
        warn!(call_id = %call_id, error = %e, "events_tx full or closed; dropping participant event");
    }
}

/// Schedule an `OutgoingEvent::Mark` to fire when the audio queued so
/// far is estimated to have played out.
///
/// "Estimated" because forge's streaming `send_audio` path doesn't
/// expose a per-frame playout-completion event — instead we anchor to
/// the wallclock when the first frame went into forge and assume one
/// frame per 20 ms. If `frames_sent` is 0 (no audio queued yet) the
/// mark fires immediately.
///
/// The fire happens on a detached tokio task so the tap's main
/// `select!` loop keeps draining the audio path during the wait. The
/// task holds a clone of the events sender; when the tap tears down
/// and drops the inner receiver, the cloned sender's send returns
/// `Err` and the task quietly exits.
fn schedule_mark(
    call_id: &CallId,
    events_tx: &mpsc::Sender<OutgoingEvent>,
    name: String,
    frames_sent: u64,
    first_pushed_at: Option<Instant>,
) {
    let target = match first_pushed_at {
        Some(start) if frames_sent > 0 => {
            start + Duration::from_millis(frames_sent.saturating_mul(PLAYOUT_FRAME_MS))
        }
        // Mark requested before any audio queued — fire now.
        _ => Instant::now(),
    };
    let events_tx = events_tx.clone();
    let call_id = call_id.clone();
    tokio::spawn(async move {
        // `sleep_until` accepts `tokio::time::Instant`. Convert.
        let target = tokio::time::Instant::from_std(target);
        tokio::time::sleep_until(target).await;
        if events_tx
            .send(OutgoingEvent::Mark { name: name.clone() })
            .await
            .is_err()
        {
            debug!(call_id = %call_id, name = %name, "events_tx closed; mark dropped");
        } else {
            debug!(call_id = %call_id, name = %name, "fired mark after estimated playout");
        }
    });
}

async fn handle_send_dtmf(
    call_id: &CallId,
    handle: &MediaBridgeHandle,
    digit: char,
    duration_ms: u32,
) {
    let parsed = match DtmfDigit::from_char(digit) {
        Ok(d) => d,
        Err(e) => {
            warn!(
                call_id = %call_id,
                digit = %digit, error = %e,
                "ignoring SendDtmf with unsupported digit char",
            );
            return;
        }
    };
    let req = OutboundDtmfRequest {
        target: MediaTarget::A,
        digit: parsed,
        duration_ms,
        playback_id: None,
        mode: PlayoutMode::Append,
    };
    if let Err(e) = handle.send_dtmf(req).await {
        warn!(call_id = %call_id, error = %e, "forge send_dtmf failed");
    } else {
        debug!(call_id = %call_id, digit = %digit, duration_ms, "queued outbound DTMF");
    }
}

/// Map a forge `ForgeEvent` to an `OutgoingEvent` the bridge can ship.
///
/// Returns `None` for events that aren't this call's, that aren't part
/// of the v1 WS protocol surface, or that don't carry the final
/// duration we need (DTMF `Start`/`Continue`).
fn derive_outgoing_event(call_id: &CallId, event: ForgeEvent) -> Option<OutgoingEvent> {
    match event {
        ForgeEvent::DtmfDigitDetected {
            call_id: ev_call,
            digit,
            duration_ms,
            method,
            event_type: DtmfEventKind::End,
            ..
        } if &ev_call == call_id => Some(OutgoingEvent::Dtmf {
            digit,
            // RFC 2833 detection always reports duration, but the
            // ForgeEvent type is Option to model SIP INFO. Fall back
            // to 0 — the bridge protocol's `duration_ms` is u32 and
            // a missing duration on End is forge giving up; we still
            // emit the press so the WS server isn't left guessing.
            duration_ms: duration_ms.unwrap_or(0),
            method: map_dtmf_method(method),
        }),
        ForgeEvent::SpeechStarted {
            call_id: ev_call,
            timestamp,
        } if &ev_call == call_id => Some(OutgoingEvent::SpeechStarted {
            // `ts_ms` is the wallclock Unix-epoch milliseconds of
            // the transition. forge's `timestamp_millis()` returns
            // i64; we coerce to u64 (negative pre-epoch values would
            // wrap, but that won't happen for live calls). The WS
            // server decides how to display it.
            ts_ms: timestamp.timestamp_millis().max(0) as u64,
        }),
        ForgeEvent::SpeechStopped {
            call_id: ev_call,
            timestamp,
            duration_ms,
        } if &ev_call == call_id => Some(OutgoingEvent::SpeechStopped {
            ts_ms: timestamp.timestamp_millis().max(0) as u64,
            duration_ms,
        }),
        _ => None,
    }
}

fn map_dtmf_method(method: DtmfDetectionMethod) -> DtmfMethod {
    match method {
        DtmfDetectionMethod::Rfc2833 => DtmfMethod::Rfc2833,
        DtmfDetectionMethod::Inband => DtmfMethod::Inband,
        // SIP INFO DTMF rides on the SIP layer (not RTP), so the
        // bridge's RTP-derived `method` enum has no slot for it. v1's
        // SIP layer doesn't surface INFO DTMF anyway; map it to
        // Inband so a future change here doesn't break wire shape if
        // forge ever publishes one.
        DtmfDetectionMethod::SipInfo => DtmfMethod::Inband,
    }
}

fn forge_attach_err(e: ForgeError) -> MediaTapError {
    MediaTapError::AttachFailed(e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attach_validates_sample_rate_first() {
        // We don't need a manager for the failure path: validation
        // happens before attach_call is invoked.
        let manager = Arc::new(MediaBridgeManager::new());
        let err = MediaTap::attach(
            &manager,
            &::std::sync::Arc::new(forge_core::EventBus::new()),
            CallId::new("c"),
            48000,
        )
        .unwrap_err();
        assert!(matches!(err, MediaTapError::Audio(_)));
    }

    #[test]
    fn attach_succeeds_at_supported_rate_and_drops_clean() {
        let manager = Arc::new(MediaBridgeManager::new());
        let call = CallId::new("c");
        {
            let tap = MediaTap::attach(
                &manager,
                &::std::sync::Arc::new(forge_core::EventBus::new()),
                call.clone(),
                8000,
            )
            .expect("attach");
            assert_eq!(tap.sample_rate(), 8000);
            assert_eq!(tap.call_id(), &call);
            assert!(manager.has_bridge(&call));
        }
        // Tap dropped → handle dropped → forge auto-detaches.
        assert!(!manager.has_bridge(&call));
    }

    #[test]
    fn attach_twice_for_same_call_fails() {
        let manager = Arc::new(MediaBridgeManager::new());
        let call = CallId::new("c");
        let _t1 = MediaTap::attach(
            &manager,
            &::std::sync::Arc::new(forge_core::EventBus::new()),
            call.clone(),
            8000,
        )
        .expect("first attach");
        let err = MediaTap::attach(
            &manager,
            &::std::sync::Arc::new(forge_core::EventBus::new()),
            call,
            8000,
        )
        .unwrap_err();
        assert!(matches!(err, MediaTapError::AttachFailed(_)));
    }

    #[tokio::test(start_paused = true)]
    async fn inactivity_watchdog_fires_when_no_inbound_frame() {
        // `start_paused = true` lets us advance virtual time
        // deterministically — no real wall-clock sleeps in the test.
        // Attach a tap, hand it inactivity = 100 ms, and watch the
        // watchdog arm fire when no frame arrives. forge never sends
        // a frame in this fixture (no session driver), which is
        // exactly the "RTP stopped" scenario.
        let manager = Arc::new(MediaBridgeManager::new());
        let call = CallId::new("c-watchdog");
        let tap = MediaTap::attach(
            &manager,
            &::std::sync::Arc::new(forge_core::EventBus::new()),
            call,
            8000,
        )
        .expect("attach")
        .with_inactivity_timeout(Some(Duration::from_millis(100)));

        let (caller_tx, mut caller_rx) = mpsc::channel(4);
        let (_playout_tx, playout_rx) = mpsc::channel(4);
        let (events_tx, mut events_rx) = mpsc::channel(4);
        let (_cmd_tx, cmd_rx) = mpsc::channel(4);

        let join = tokio::spawn(tap.run(caller_tx, playout_rx, events_tx, cmd_rx));

        // Advance virtual time past the deadline. The watchdog arm
        // fires and `run` returns `Ok(InactivityTimeout)`.
        tokio::time::advance(Duration::from_millis(200)).await;

        let outcome = join.await.expect("join").expect("ok");
        assert_eq!(outcome, TapDisconnect::InactivityTimeout);

        // No audio / events should have been emitted — we exited
        // straight from the watchdog arm.
        assert!(caller_rx.try_recv().is_err());
        assert!(events_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn join_room_replumbs_ws_playout_and_leave_restores_direct() {
        // End-to-end against a real room: WS playout pushed into a
        // joined tap must surface on ANOTHER member's sink (i.e. it
        // went through the room, not forge), and LeaveRoom must cut
        // that flow again. The SIP-inbound side needs live RTP and
        // is covered by the SIPp phase in a later chunk.
        let manager = Arc::new(MediaBridgeManager::new());
        let tap = MediaTap::attach(
            &manager,
            &::std::sync::Arc::new(forge_core::EventBus::new()),
            CallId::new("call-a"),
            8000,
        )
        .expect("attach");

        let (caller_tx, _caller_rx) = mpsc::channel(16);
        let (playout_tx, playout_rx) = mpsc::channel(16);
        let (events_tx, _events_rx) = mpsc::channel(16);
        let (cmd_tx, cmd_rx) = mpsc::channel(16);
        let _join = tokio::spawn(tap.run(caller_tx, playout_rx, events_tx, cmd_rx));

        let room = crate::room::spawn_room(crate::room::RoomConfig {
            room_id: "r-tap".into(),
            sample_rate: 8000,
            max_calls: 8,
            join_tones: false,
        });
        let membership_a = room.join("call-a", 8000).await.expect("join a");
        let membership_b = room.join("call-b", 8000).await.expect("join b");
        let (_b_send, mut b_sip_out, _b_ws_out, _b_events) = membership_b.split();

        cmd_tx
            .send(TapCommand::JoinRoom {
                membership: membership_a,
            })
            .await
            .expect("send join");

        // Stream bot-A playout into the tap; with A's `ws`
        // participant as the only contributor, member B's SIP sink
        // must carry it verbatim (single contributor → no auto-gain).
        let frame = pack_pcm16_le(&vec![1000i16; 160]);
        let pump_tx = playout_tx.clone();
        let pump_frame = frame.clone();
        let pump = tokio::spawn(async move {
            loop {
                if pump_tx.send(pump_frame.clone()).await.is_err() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        });
        let heard = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let mixed = b_sip_out.recv().await.expect("sink open");
                if mixed.iter().any(|&s| s != 0) {
                    return mixed;
                }
            }
        })
        .await
        .expect("bot-A playout reaches member B through the room");
        assert!(heard.iter().all(|&s| s == 1000));
        pump.abort();

        // Leave: playout reverts to the direct forge path, so B's
        // sink dries up (drain the in-flight tail, then expect
        // silence for 300 ms).
        cmd_tx
            .send(TapCommand::LeaveRoom)
            .await
            .expect("send leave");
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                // Keep pushing playout — it must NOT reach the room.
                let _ = playout_tx.send(frame.clone()).await;
                match tokio::time::timeout(Duration::from_millis(300), b_sip_out.recv()).await {
                    Ok(Some(_)) => continue, // tail still draining
                    Ok(None) => return,      // room reaped member A — also fine
                    Err(_) => return,        // 300 ms silence = direct mode
                }
            }
        })
        .await
        .expect("flow into the room stops after LeaveRoom");
    }

    #[tokio::test(start_paused = true)]
    async fn inactivity_watchdog_off_when_timeout_none() {
        // With `inactivity_timeout = None` the watchdog arm is
        // disabled — `tap.run` should NOT exit just because virtual
        // time advanced past any deadline. We tear it down by
        // dropping the caller_audio_tx receiver to confirm the loop
        // is still alive and responsive.
        let manager = Arc::new(MediaBridgeManager::new());
        let tap = MediaTap::attach(
            &manager,
            &::std::sync::Arc::new(forge_core::EventBus::new()),
            CallId::new("c-no-watchdog"),
            8000,
        )
        .expect("attach")
        .with_inactivity_timeout(None);

        let (caller_tx, caller_rx) = mpsc::channel(4);
        let (_playout_tx, playout_rx) = mpsc::channel(4);
        let (events_tx, _events_rx) = mpsc::channel(4);
        let (_cmd_tx, cmd_rx) = mpsc::channel(4);

        let join = tokio::spawn(tap.run(caller_tx, playout_rx, events_tx, cmd_rx));

        // Way past any plausible default — still no exit.
        tokio::time::advance(Duration::from_secs(3600)).await;
        // Drop the receiver to wake the `caller_audio_tx.closed()` arm.
        drop(caller_rx);
        // Allow the runtime to step.
        tokio::task::yield_now().await;

        let outcome = join.await.expect("join").expect("ok");
        assert_eq!(outcome, TapDisconnect::ControllerHungUp);
    }
}
