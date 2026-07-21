//! Bidirectional audio tap on top of forge-engine's `MediaBridgeManager`.
//!
//! This is the integration point identified by the Week-1 spike
//! (`docs/design/SPIKE_MEDIA_TAP.md`): one tap per call, attached to forge's
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

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::idle::{IdleDetector, IdleEvent};
use crate::room::{RoomEvent, RoomMembership, RoomSender};
use crate::rtp_stats::{QualityReport, RtpStatsTracker, RxStats, TxStats};

use forge_core::{CallId, DtmfDetectionMethod, DtmfEventKind, EventBus, ForgeError, ForgeEvent};
use forge_dtmf::DtmfDigit;
use forge_engine::{
    MediaBridgeHandle, MediaBridgeManager, MediaTarget, OutboundDtmfRequest, OutboundMediaFrame,
    PlayoutMode,
};
use siphon_ai_bridge::{
    pack_pcm16_le, unpack_pcm16_le, AudioError, BargeInOutcome, ConferenceLeftReason, DtmfMethod,
    OutgoingEvent, Reframer,
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

/// Grace margin on the barge-in playout clock (§`bot_is_playing`). The
/// caller's acoustic tail (handset/speaker + far-end jitter buffer)
/// outlasts our local playout cursor by a few frames, and VAD/scheduling
/// add jitter; this margin absorbs both. Biased toward *more* gating —
/// fewer false barge-ins — which is the safe direction for an echo/noise
/// gate.
const BARGE_IN_PLAYOUT_GRACE: Duration = Duration::from_millis(60);

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

    /// Server verdict on a pause-mode barge-in arbitration
    /// ([`BargeInAction::Pause`]): the speech was a real interruption
    /// — drop the retained playout tail and stay quiet. Audio the
    /// server streamed since the pause plays immediately (it barged
    /// over itself — its choice). A no-op when no arbitration is
    /// pending: verdicts race with the deadline and with preempting
    /// commands by nature, so a late one must be harmless.
    BargeInConfirm,

    /// Server verdict: false positive (cough / backchannel / noise) —
    /// re-queue the retained tail and resume playout where it
    /// stopped, followed by any audio streamed since the pause. Same
    /// no-op semantics as [`BargeInConfirm`](Self::BargeInConfirm).
    BargeInReject,

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

    /// Park the call (0.7.0 §2.4): stop using the WS-facing channels
    /// and play `moh` into the caller leg on a 20 ms tick; drop inbound
    /// caller frames (there's no WS to forward to). The forge media
    /// session + RTP stay up. Pairs with [`TapCommand::Unpark`]. A
    /// `Park` while in a room leaves the room first.
    Park { moh: Box<crate::moh::MohSource> },

    /// Retrieve a parked call: stop the MOH tick and swap the tap's
    /// WS-facing endpoints to the fresh ones from the retrieve's new
    /// bridge, resuming the direct caller↔WS pair. A no-op if not
    /// parked.
    Unpark {
        caller_audio_tx: mpsc::Sender<Vec<u8>>,
        playout_audio_rx: mpsc::Receiver<Vec<u8>>,
        events_tx: mpsc::Sender<OutgoingEvent>,
    },

    /// Bot-initiated hold (0.7.2): play `moh` into the caller leg on a
    /// 20 ms tick and pause the caller↔WS bridge, while **keeping the WS
    /// session attached** (unlike [`Park`](Self::Park), which detaches
    /// it). Reuses park's MOH machinery; the difference is that the WS
    /// stays open, so the chatty bot keeps streaming playout — the tap
    /// drains-and-drops it (the caller hears MOH, not the bot) rather
    /// than letting it back-pressure the bridge. Caller audio is dropped
    /// (the caller is on hold), but still recorded. Pairs with
    /// [`Unhold`](Self::Unhold).
    Hold { moh: Box<crate::moh::MohSource> },

    /// Resume a held call (0.7.2): stop the MOH tick and restore the
    /// direct caller↔WS pair on the **existing** channels (no swap, the
    /// WS never went away). A no-op if not held.
    Unhold,

    /// Play a one-shot announcement to the caller (0.26.0 — the "this
    /// call may be recorded" compliance prompt). While it plays: caller
    /// frames are dropped (not forwarded to the WS, **not** forked to
    /// the recording — capture starts after the prompt) and WS playout
    /// is drained-and-dropped (the bot can't talk over it). At EOF the
    /// elapsed milliseconds are reported on `done` and the direct
    /// caller↔WS pair resumes. A `Park`/`Hold` during the announcement
    /// ends it early (reported as done).
    Announce {
        source: Box<crate::moh::AnnounceSource>,
        done: tokio::sync::oneshot::Sender<u64>,
    },
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
    /// Reversible barge-in (`docs/design/DESIGN_REVERSIBLE_BARGE_IN.md`):
    /// flush playout immediately — the same one-frame reaction as
    /// `AutoClear` — but retain the unplayed tail so the WS server
    /// (the only layer with STT) can rule on intent.
    /// [`TapCommand::BargeInConfirm`] drops the tail;
    /// [`TapCommand::BargeInReject`] re-queues it and playout resumes
    /// where it stopped. No verdict within `decision` applies
    /// `on_timeout`. Arbitration only arms while the bot is playing,
    /// and degrades to `Notify` inside a conference room (design
    /// note §7.2).
    Pause {
        /// Server verdict deadline (`[bridge.barge_in].decision_ms`).
        decision: Duration,
        /// Fallback verdict at the deadline.
        on_timeout: TimeoutVerdict,
        /// Cap on retained audio (`[bridge.barge_in].resume_max_secs`).
        /// A single utterance longer than this loses its oldest
        /// unplayed frames — warned once per call.
        resume_max: Duration,
    },
}

/// What a [`BargeInAction::Pause`] arbitration falls back to when the
/// server doesn't rule within the decision window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeoutVerdict {
    /// Treat the speech as a real barge-in: drop the retained tail and
    /// stay quiet. The safe default — never talk over the caller. A
    /// server that ignores arbitration entirely therefore degrades to
    /// "auto_clear delayed by the decision window".
    Confirm,
    /// Treat it as a false positive: resume the retained playout.
    Reject,
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
    /// Playout-gated barge-in debounce (echo/noise gate). When `Some(d)`,
    /// a VAD `SpeechStarted` that arrives **while the bot is playing out**
    /// is held for `d`; it only confirms a real barge-in (flush + forward)
    /// if speech is still active when `d` elapses. A `SpeechStopped` within
    /// the window cancels it — the common shape of the bot's own echo or a
    /// brief background-noise blip. `None` (default) disables the gate:
    /// every `SpeechStarted` flushes immediately, the pre-0.7.1 behaviour.
    /// Only affects `AutoClear`; while the bot is silent, barge-in is
    /// always immediate. Set via [`Self::with_barge_in_debounce`] from
    /// `[bridge.barge_in].debounce_ms`.
    barge_in_debounce: Option<Duration>,
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
    /// Live whole-call quality feed for the CDR `quality` block
    /// (0.30.0). When set, the tap `send_replace`s a fresh
    /// [`QualityReport`] on every quality-relevant moment (RTCP note,
    /// media-stats note, playout clear, first server audio). Watch
    /// semantics: the controller only ever wants the latest value, so
    /// missed intermediate states cost nothing. Set by the
    /// `CallController` via [`Self::with_quality_watch`] before `run()`.
    quality_tx: Option<tokio::sync::watch::Sender<QualityReport>>,
    /// Playout clears (`auto_clear` firings + server `clear` commands),
    /// for the CDR `barge_in_count`.
    barge_in_count: u32,
    /// First WS-server audio frame reaching playout — sticky, unlike
    /// the Mark-estimation clock which resets on Mute/Clear.
    first_audio_at: Option<Instant>,
    /// Recording fork. `None` (default) → no recording. When set, the tap
    /// `try_send`s a copy of each leg's 20 ms frame to the sender —
    /// best-effort and non-blocking, so a backed-up recording writer never
    /// stalls the audio path (CLAUDE.md §4.3). A `Full` channel increments
    /// the shared drop counter (→ the recording is flagged `degraded`); a
    /// `Closed` channel (writer already stopped) is ignored. Set by the
    /// `CallController` before `run()`.
    recording: Option<(mpsc::Sender<RecFrame>, Arc<AtomicU64>)>,
    /// WS reconnect survival (0.7.3). When `true`, the WS-facing channels
    /// closing (the bridge task ending on an unexpected drop) is **not**
    /// fatal — the tap waits on hold music for the controller to redial
    /// (`TapCommand::Park` then `Unpark`) instead of tearing down. With
    /// this set, the authoritative "controller is gone" teardown signal
    /// becomes `commands_rx` closing (the controller dropping its
    /// `tap_cmd_tx`), not the audio channels. `false` (default) = the v1
    /// behaviour where a closed WS channel ends the tap. Set by the
    /// acceptor from `[bridge].ws_reconnect_enabled` via
    /// [`Self::with_survive_ws_drop`].
    survive_ws_drop: bool,
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
            barge_in_debounce: None,
            inactivity_timeout: None,
            muted: false,
            idle_detector: IdleDetector::new(None, None, Instant::now()),
            rtp_stats: RtpStatsTracker::new(None),
            quality_tx: None,
            barge_in_count: 0,
            first_audio_at: None,
            recording: None,
            survive_ws_drop: false,
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

    /// Install the quality watch feed (0.30.0). The controller keeps
    /// the receiver and reads the latest [`QualityReport`] at teardown
    /// for the CDR `quality` block.
    pub fn with_quality_watch(mut self, tx: tokio::sync::watch::Sender<QualityReport>) -> Self {
        self.quality_tx = Some(tx);
        self
    }

    /// Push the current quality state to the watch feed. Control-rate
    /// only (RTCP cadence / clears / first audio) — never per-frame.
    fn publish_quality(&self) {
        if let Some(tx) = &self.quality_tx {
            tx.send_replace(QualityReport {
                stats: self.rtp_stats.quality_summary(),
                barge_in_count: self.barge_in_count,
                first_audio_at: self.first_audio_at,
            });
        }
    }

    /// Record the resolution of a pause-mode barge-in arbitration:
    /// the decision metrics always; the CDR barge-in count only when
    /// the resolution treats the speech as a real interruption
    /// (confirm-shaped outcomes — a rejected arbitration was, by the
    /// server's own ruling, not a barge-in).
    fn note_barge_in_decision(
        &mut self,
        armed_at: Instant,
        outcome: &'static str,
        is_barge_in: bool,
    ) {
        metrics::counter!("siphon_ai_barge_in_decisions_total", "outcome" => outcome).increment(1);
        metrics::histogram!("siphon_ai_barge_in_decision_seconds")
            .record(armed_at.elapsed().as_secs_f64());
        if is_barge_in {
            self.barge_in_count += 1;
            self.publish_quality();
        }
    }

    /// Resolve a pending pause arbitration as confirm without
    /// re-queuing anything — used when another feature takes over the
    /// caller's ear (mute / clear / hold / park / announce / room) or
    /// the WS drops. The retained tail is moot in every such case, and
    /// the interruption stands for CDR purposes. A `None` pending is a
    /// no-op, so call sites don't need their own guard.
    fn abandon_arbitration(
        &mut self,
        pending: &mut Option<PendingVerdict>,
        events_tx: &mpsc::Sender<OutgoingEvent>,
        site: &'static str,
    ) {
        if let Some(p) = pending.take() {
            self.note_barge_in_decision(p.armed_at, "confirmed", true);
            emit_resolved(events_tx, &self.call_id, BargeInOutcome::Confirmed);
            debug!(
                call_id = %self.call_id,
                site,
                "pending barge-in arbitration resolved as confirm",
            );
        }
    }

    /// Re-queue retained playout chunks into forge at arbitration
    /// resolution (design note §5.3). `fork_recording` is set for the
    /// post-pause `fresh` audio (first time it plays) and unset for
    /// the `resume` tail (already forked at its original push — the
    /// recording must not carry it twice). Re-pushed chunks re-enter
    /// the shadow ring so an immediate second barge-in stays
    /// reversible; the caller re-anchors the playout clock afterwards
    /// via [`restore_playout_clock`].
    async fn repush_chunks(
        &self,
        chunks: Vec<Vec<u8>>,
        fork_recording: bool,
        shadow: &mut VecDeque<Vec<u8>>,
        shadow_cap: usize,
    ) -> Result<u64, MediaTapError> {
        let mut pushed = 0u64;
        for bytes in chunks {
            let samples = unpack_pcm16_le(&bytes)?;
            if fork_recording {
                if let Some((rec, drops)) = &self.recording {
                    if let Err(mpsc::error::TrySendError::Full(_)) =
                        rec.try_send(RecFrame::Bot(bytes.clone()))
                    {
                        drops.fetch_add(1, Ordering::Relaxed);
                    }
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
            if shadow_cap > 0 {
                if shadow.len() == shadow_cap {
                    shadow.pop_front();
                }
                shadow.push_back(bytes);
            }
            pushed += 1;
        }
        Ok(pushed)
    }

    /// Install the playout-gated barge-in debounce from
    /// `[bridge.barge_in].debounce_ms` (and any per-route override).
    /// `None`/`Some(0)` disables it (immediate flush, pre-0.7.1 behaviour).
    /// Acceptor / outbound service call this before `run()`.
    pub fn with_barge_in_debounce(mut self, debounce: Option<Duration>) -> Self {
        self.barge_in_debounce = debounce.filter(|d| !d.is_zero());
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

    /// Enable WS-reconnect survival (0.7.3): a closed WS-facing channel
    /// becomes non-fatal (the tap waits for the controller to redial)
    /// and teardown routes through `commands_rx` closing instead. See
    /// [`Self::survive_ws_drop`]. The acceptor sets this from the call's
    /// resolved `[bridge].ws_reconnect_enabled`.
    pub fn with_survive_ws_drop(mut self, enabled: bool) -> Self {
        self.survive_ws_drop = enabled;
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
        // The WS-facing endpoints are `mut` locals so a park→retrieve
        // can swap them to a fresh bridge's channels (`TapCommand::Unpark`).
        let mut caller_audio_tx = caller_audio_tx;
        let mut events_tx = events_tx;
        // Park state (0.7.0 §2.4). `Some(moh)` = the WS session is
        // detached and the caller hears hold music on a 20 ms tick;
        // caller audio is dropped. Installed by `TapCommand::Park`,
        // cleared by `TapCommand::Unpark`.
        let mut parked: Option<crate::moh::MohSource> = None;
        // Bot-hold state (0.7.2). `Some(moh)` = the caller is on hold
        // (MOH on the 20 ms tick, caller audio dropped) but the WS
        // session is STILL attached — so bot playout is drained-and-
        // dropped rather than suppressed (it would back-pressure the
        // open bridge). Installed by `TapCommand::Hold`, cleared by
        // `TapCommand::Unhold`. Mutually exclusive with `parked` in
        // practice (a parked call has no WS to drive a hold), but the
        // arms below treat them independently for clarity.
        let mut held: Option<crate::moh::MohSource> = None;
        // One-shot announcement (0.26.0): the playing source, its
        // completion channel, and frames played so far. Cleared (and
        // `done` fired) at EOF or when park/hold preempts it.
        let mut announcing: Option<(
            Box<crate::moh::AnnounceSource>,
            tokio::sync::oneshot::Sender<u64>,
            u64,
        )> = None;
        // WS-reconnect survival (0.7.3). When `survive_ws_drop` is set and
        // the WS-facing channels close (bridge task ended on an unexpected
        // drop), we set `ws_dropped` to suppress the close-driven teardown
        // arms and wait for the controller to redial (`Park` then
        // `Unpark`); teardown then comes from `commands_rx` closing. Reset
        // on `Unpark` (the fresh channels are live again).
        let survive_ws_drop = self.survive_ws_drop;
        let mut ws_dropped = false;

        let mut reframer = Reframer::new(self.sample_rate)?;
        // Play-out estimation state for `TapCommand::Mark`.
        // Updated in the playout arm; read in the command arm.
        let mut frames_sent_to_forge: u64 = 0;
        let mut first_audio_pushed_at: Option<Instant> = None;
        // Playout clock for barge-in gating (§`bot_is_playing`) — distinct
        // from the Mark bookkeeping above. Estimated wallclock at which the
        // most recently queued playout frame finishes. Advances 20 ms per
        // pushed frame from `max(now, cursor)` so it re-anchors after a
        // silence gap between bot phrases instead of drifting behind `now`.
        // `None` = nothing queued / just flushed.
        let mut playout_until: Option<Instant> = None;

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

        // MOH playout cadence — 20 ms, monotonic (CLAUDE §4.3), active
        // only while parked. Same placeholder-interval pattern as the
        // other optional ticks; the arm guard suppresses it otherwise.
        let mut moh_tick = tokio::time::interval(Duration::from_millis(PLAYOUT_FRAME_MS));
        moh_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        moh_tick.tick().await;

        // Barge-in debounce (echo/noise gate, §barge_in_debounce). Pinned
        // far-future placeholder; reset to `now + debounce` when a barge-in
        // is held during playout. `pending_barge_in` holds the deferred
        // `speech_started` event — `Some` gates the timer arm, and the held
        // event is forwarded on confirm or dropped on cancel. Same
        // placeholder-sleep pattern as the watchdog.
        let barge_debounce = tokio::time::sleep(Duration::from_secs(86_400));
        tokio::pin!(barge_debounce);
        let mut pending_barge_in: Option<OutgoingEvent> = None;

        // Pause-mode barge-in arbitration (§5 of the design note).
        // `shadow` holds the *unplayed* playout tail — each pushed frame
        // is retained until the playout clock says it has played (plus
        // one frame of early-bias slop) — so a reject can re-queue what
        // the pause's flush dropped. Capacity 0 in the other modes
        // makes every push a no-op. `pending_verdict` gates the
        // deadline arm; same pinned-placeholder pattern as the
        // debounce timer above.
        let shadow_cap = match self.barge_in_action {
            BargeInAction::Pause { resume_max, .. } => {
                (resume_max.as_millis() as u64 / PLAYOUT_FRAME_MS).max(1) as usize
            }
            _ => 0,
        };
        let mut shadow: VecDeque<Vec<u8>> = VecDeque::with_capacity(shadow_cap.min(2048));
        let mut shadow_truncation_warned = false;
        let mut pending_verdict: Option<PendingVerdict> = None;
        let arb_deadline = tokio::time::sleep(Duration::from_secs(86_400));
        tokio::pin!(arb_deadline);

        loop {
            tokio::select! {
                biased;

                // Controller dropped the caller_audio_tx receiver →
                // tear down immediately, even if no inbound frame is
                // pending. Suppressed while parked: the WS bridge (and
                // thus this channel's receiver) is intentionally gone.
                _ = caller_audio_tx.closed(), if parked.is_none() && !ws_dropped => {
                    if survive_ws_drop {
                        // Reconnect-enabled: the bridge dropping its
                        // receiver is a WS drop, not a teardown. Stop
                        // polling this arm and wait for the controller to
                        // redial (Park → Unpark). Teardown comes from
                        // `commands_rx` closing.
                        debug!(call_id = %self.call_id,
                            "caller_audio_tx receiver dropped; holding for reconnect");
                        // The arbiter is gone — a pending pause verdict
                        // resolves as confirm (§7.2 of the design note);
                        // flushed-and-quiet is the right state alongside
                        // the reconnect hold music.
                        self.abandon_arbitration(&mut pending_verdict, &events_tx, "ws_drop");
                        ws_dropped = true;
                    } else {
                        debug!("caller_audio_tx receiver dropped; ending tap");
                        return Ok(TapDisconnect::ControllerHungUp);
                    }
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
                // Suppressed while parked — there's no WS, and the MOH
                // tick owns playout instead.
                playout = playout_audio_rx.recv(), if parked.is_none() && !ws_dropped => {
                    let Some(bytes) = playout else {
                        if survive_ws_drop {
                            // WS drop with reconnect on — hold, don't tear
                            // down (mirrors the caller_audio_tx arm).
                            debug!(call_id = %self.call_id,
                                "playout_audio_rx closed; holding for reconnect");
                            // Same as the caller_audio_tx arm: no arbiter,
                            // pending verdict resolves as confirm.
                            self.abandon_arbitration(&mut pending_verdict, &events_tx, "ws_drop");
                            ws_dropped = true;
                            continue;
                        }
                        debug!("playout_audio_rx closed; ending tap");
                        return Ok(TapDisconnect::ControllerHungUp);
                    };
                    // Idle-detector: any WS-side push counts as
                    // "audio activity" for dead-air detection,
                    // regardless of mute state — a muted call where
                    // the WS server keeps streaming is NOT dead air,
                    // it's intentional silence.
                    self.idle_detector.note_ws_audio(Instant::now());
                    // Bot-hold: the WS stays attached and a chatty bot
                    // keeps streaming, but the caller is hearing MOH (the
                    // moh_tick arm). Drain-and-drop the playout — routing
                    // it to forge would talk over the hold music, and
                    // leaving it un-recv'd would back-pressure the open
                    // bridge. A genuine WS disconnect still tears down via
                    // the `None` branch above.
                    if held.is_some() || announcing.is_some() {
                        continue;
                    }
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
                    // Pause-mode arbitration in flight: post-pause server
                    // audio queues behind the retained tail (§5.3 of the
                    // design note) instead of reaching forge mid-pause —
                    // it's re-queued (and recording-forked) at
                    // resolution. Bounded by the shadow cap; a server
                    // that floods faster than real time during the
                    // sub-second window loses the excess.
                    if let Some(p) = pending_verdict.as_mut() {
                        if p.fresh.len() < shadow_cap {
                            p.fresh.push(bytes);
                        } else if !shadow_truncation_warned {
                            shadow_truncation_warned = true;
                            warn!(
                                call_id = %self.call_id,
                                cap_frames = shadow_cap,
                                "barge-in pause buffer full; dropping incoming playout",
                            );
                        }
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
                    let push_now = Instant::now();
                    if frames_sent_to_forge == 0 {
                        first_audio_pushed_at = Some(push_now);
                    }
                    if self.first_audio_at.is_none() {
                        self.first_audio_at = Some(push_now);
                        self.publish_quality();
                    }
                    frames_sent_to_forge += 1;
                    // Advance the barge-in playout clock: from the later of
                    // `now` and the current cursor (re-anchors after a gap),
                    // plus one frame of playout.
                    playout_until = Some(
                        playout_until.map_or(push_now, |c| c.max(push_now))
                            + Duration::from_millis(PLAYOUT_FRAME_MS),
                    );
                    // Pause mode: shadow the pushed frame so a barge-in
                    // reject can re-queue it, then drop frames the
                    // playout clock says have already played — the ring
                    // only ever holds the unplayed tail plus one frame
                    // of early-bias slop (repeating ≤20 ms on resume
                    // beats skipping a syllable). Steady-state cost is
                    // moving the already-owned `bytes` Vec, no copy.
                    if shadow_cap > 0 {
                        if shadow.len() == shadow_cap {
                            shadow.pop_front();
                            if !shadow_truncation_warned {
                                shadow_truncation_warned = true;
                                warn!(
                                    call_id = %self.call_id,
                                    cap_frames = shadow_cap,
                                    "utterance exceeds barge-in resume_max; oldest unplayed audio won't survive a reject",
                                );
                            }
                        }
                        shadow.push_back(bytes);
                        let keep = unplayed_frames(playout_until, push_now).saturating_add(1);
                        while shadow.len() > keep {
                            shadow.pop_front();
                        }
                    }
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
                        // Announcing (0.26.0): drop the frame entirely —
                        // no WS forward AND no recording fork; capture
                        // starts only after the compliance prompt.
                        if announcing.is_some() {
                            continue;
                        }
                        // Parked or held: don't forward the caller to
                        // the WS (parked = no WS; held = caller is on
                        // hold). Keep recording the caller's own voice
                        // (left channel) but drop the frame otherwise —
                        // the caller hears MOH (the moh_tick arm), not
                        // themselves.
                        if parked.is_some() || held.is_some() {
                            if let Some((rec, drops)) = &self.recording {
                                if let Err(mpsc::error::TrySendError::Full(_)) =
                                    rec.try_send(RecFrame::Caller(pack_pcm16_le(&samples)))
                                {
                                    drops.fetch_add(1, Ordering::Relaxed);
                                }
                            }
                            continue;
                        }
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
                    let push_now = Instant::now();
                    if frames_sent_to_forge == 0 {
                        first_audio_pushed_at = Some(push_now);
                    }
                    if self.first_audio_at.is_none() {
                        self.first_audio_at = Some(push_now);
                        self.publish_quality();
                    }
                    frames_sent_to_forge += 1;
                    playout_until = Some(
                        playout_until.map_or(push_now, |c| c.max(push_now))
                            + Duration::from_millis(PLAYOUT_FRAME_MS),
                    );
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
                                    cumulative_lost,
                                    rtt_ms,
                                    ..
                                } if cid == &self.call_id => {
                                    self.rtp_stats.note_rtcp_report(
                                        *jitter_ms,
                                        *packet_loss_ratio,
                                        i64::from(*cumulative_lost),
                                        *rtt_ms,
                                    );
                                    self.publish_quality();
                                }
                                // Locally-measured RX- and TX-side
                                // counters (0.30.0 / 0.38.0).
                                //
                                // Filtered to leg A, the SIP peer: per
                                // the "Single-leg model" note above, B
                                // is synthetic and never carries RTP,
                                // so A is the only leg that describes
                                // this call. forge publishes per leg
                                // and its own emit guard widened in
                                // forge-media #93 (send-only legs now
                                // qualify), so pinning the leg here
                                // keeps a future two-leg forge from
                                // interleaving two cumulative series
                                // into one tracker.
                                ForgeEvent::MediaStatsSnapshot {
                                    call_id: cid,
                                    leg: forge_core::MediaLeg::A,
                                    rx_packets_received,
                                    rx_packets_lost,
                                    rx_packets_out_of_order,
                                    rx_packets_duplicate,
                                    rx_jitter_ms,
                                    tx_packets_sent,
                                    tx_octets_sent,
                                    ..
                                } if cid == &self.call_id => {
                                    self.rtp_stats.note_media_stats(
                                        RxStats {
                                            jitter_ms: *rx_jitter_ms,
                                            packets_received: *rx_packets_received,
                                            packets_lost: *rx_packets_lost,
                                            packets_out_of_order: *rx_packets_out_of_order,
                                            packets_duplicate: *rx_packets_duplicate,
                                        },
                                        TxStats {
                                            packets_sent: *tx_packets_sent,
                                            octets_sent: *tx_octets_sent,
                                        },
                                    );
                                    self.publish_quality();
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
                            if let Some(mut out) = derive_outgoing_event(&self.call_id, ev) {
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
                                // Barge-in reaction beyond forwarding the
                                // event. `auto_clear` drops pending
                                // outbound playout; `pause` does the same
                                // flush but retains the unplayed tail and
                                // arms the server arbitration (design note
                                // §5.2). Pause degrades to notify-only in
                                // a room (§7.2), while an arbitration is
                                // already pending (forge-vad won't re-fire
                                // without a SpeechStopped between, but the
                                // bus is best-effort), and while the bot
                                // is silent — nothing to pause.
                                let speech_now = Instant::now();
                                let reaction = if matches!(out, OutgoingEvent::SpeechStarted { .. })
                                {
                                    match self.barge_in_action {
                                        BargeInAction::Notify => None,
                                        BargeInAction::AutoClear => Some(self.barge_in_action),
                                        BargeInAction::Pause { .. }
                                            if room_send.is_some()
                                                || pending_verdict.is_some()
                                                || !bot_is_playing(playout_until, speech_now) =>
                                        {
                                            None
                                        }
                                        pause @ BargeInAction::Pause { .. } => Some(pause),
                                    }
                                } else {
                                    None
                                };
                                if let Some(action) = reaction {
                                    // Playout-gated debounce (echo/noise):
                                    // if the bot is currently talking and a
                                    // debounce is configured, the speech is
                                    // *provisional* — hold the event (no
                                    // flush, no forward) and let the
                                    // `barge_debounce` timer confirm it or
                                    // a `SpeechStopped` cancel it. While
                                    // the bot is silent, barge-in stays
                                    // immediate. Applies to both flushing
                                    // modes: the gate is the acoustic
                                    // filter, arbitration the semantic one
                                    // — they compose (§7.1).
                                    let gate = self
                                        .barge_in_debounce
                                        .filter(|_| bot_is_playing(playout_until, speech_now));
                                    if let Some(debounce) = gate {
                                        if pending_barge_in.is_none() {
                                            barge_debounce
                                                .as_mut()
                                                .reset(tokio::time::Instant::now() + debounce);
                                            pending_barge_in = Some(out);
                                            debug!(
                                                call_id = %self.call_id,
                                                debounce_ms = debounce.as_millis() as u64,
                                                "barge-in held during playout; awaiting confirm",
                                            );
                                        }
                                        continue; // timer or speech-stopped decides
                                    }
                                    match action {
                                        BargeInAction::AutoClear => {
                                            // Drop pending outbound playout
                                            // before forwarding the WS
                                            // event. The drain catches bytes
                                            // in the controller→tap audio
                                            // channel that haven't yet
                                            // reached forge; the flush dumps
                                            // forge's encoder queue. Reset
                                            // the Mark bookkeeping so the
                                            // next `Mark` doesn't wait on
                                            // now-dropped audio.
                                            self.barge_in_count += 1;
                                            self.publish_quality();
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
                                            playout_until = None;
                                        }
                                        BargeInAction::Pause { decision, .. } => {
                                            let resume = begin_pause_arbitration(
                                                &self.call_id,
                                                &self.handle,
                                                &mut playout_audio_rx,
                                                &mut shadow,
                                            )
                                            .await;
                                            frames_sent_to_forge = 0;
                                            first_audio_pushed_at = None;
                                            playout_until = None;
                                            arb_deadline
                                                .as_mut()
                                                .reset(tokio::time::Instant::now() + decision);
                                            pending_verdict = Some(PendingVerdict {
                                                resume,
                                                fresh: Vec::new(),
                                                armed_at: speech_now,
                                            });
                                            // The forwarded speech_started IS
                                            // the arbitration request — stamp
                                            // the wire fields (§2 of the
                                            // design note).
                                            stamp_decision_pending(&mut out, decision);
                                        }
                                        BargeInAction::Notify => {}
                                    }
                                }
                                // A speech-stopped within the debounce
                                // window cancels the held barge-in — it was
                                // the bot's own echo or a brief noise blip,
                                // not the caller. Drop both the held
                                // speech-started and this speech-stopped
                                // (the server never saw the pair).
                                if matches!(out, OutgoingEvent::SpeechStopped { .. })
                                    && pending_barge_in.take().is_some()
                                {
                                    debug!(
                                        call_id = %self.call_id,
                                        "barge-in cancelled (speech stopped within debounce)",
                                    );
                                    continue;
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
                        if survive_ws_drop {
                            // Reconnect-enabled calls treat the audio-close
                            // arms as non-fatal WS drops, so the controller
                            // dropping its `tap_cmd_tx` is the authoritative
                            // teardown signal (it happens when the
                            // controller exits its loop and drains).
                            debug!(call_id = %self.call_id,
                                "commands_rx closed; ending tap (reconnect mode)");
                            return Ok(TapDisconnect::ControllerHungUp);
                        }
                        // Default: the audio arms still drive the call;
                        // treat command closure as a non-fatal "no more
                        // commands" and stop polling this arm by recreating
                        // the receiver against a dropped sender.
                        debug!("commands_rx closed; tap continues without commands");
                        let (_drop_tx, replacement) = mpsc::channel::<TapCommand>(1);
                        commands_rx = replacement;
                        continue;
                    };
                    match cmd {
                        TapCommand::Mute => {
                            // Mute takes over the caller's ear — a
                            // pending pause arbitration resolves as
                            // confirm (§7.2); the retained tail would be
                            // flushed below anyway.
                            self.abandon_arbitration(&mut pending_verdict, &events_tx, "mute");
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
                            playout_until = None;
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
                        TapCommand::BargeInConfirm => {
                            if let Some(p) = pending_verdict.take() {
                                self.note_barge_in_decision(p.armed_at, "confirmed", true);
                                // The pause already flushed; only the
                                // post-pause server audio plays (§5.3).
                                let n = self
                                    .repush_chunks(p.fresh, true, &mut shadow, shadow_cap)
                                    .await?;
                                restore_playout_clock(
                                    n,
                                    &mut frames_sent_to_forge,
                                    &mut first_audio_pushed_at,
                                    &mut playout_until,
                                );
                                emit_resolved(&events_tx, &self.call_id, BargeInOutcome::Confirmed);
                                debug!(
                                    call_id = %self.call_id,
                                    fresh = n,
                                    "barge-in confirmed by server; retained tail dropped",
                                );
                            } else {
                                debug!(
                                    call_id = %self.call_id,
                                    "barge_in_confirm with no pending arbitration; ignoring",
                                );
                            }
                        }
                        TapCommand::BargeInReject => {
                            if let Some(p) = pending_verdict.take() {
                                self.note_barge_in_decision(p.armed_at, "rejected", false);
                                // Resume where playout stopped: the
                                // retained tail first (already recording-
                                // forked at its original push — §5.3 "no
                                // double-record"), then the post-pause
                                // server audio.
                                let resumed = self
                                    .repush_chunks(p.resume, false, &mut shadow, shadow_cap)
                                    .await?;
                                let fresh = self
                                    .repush_chunks(p.fresh, true, &mut shadow, shadow_cap)
                                    .await?;
                                restore_playout_clock(
                                    resumed + fresh,
                                    &mut frames_sent_to_forge,
                                    &mut first_audio_pushed_at,
                                    &mut playout_until,
                                );
                                emit_resolved(&events_tx, &self.call_id, BargeInOutcome::Rejected);
                                debug!(
                                    call_id = %self.call_id,
                                    resumed,
                                    fresh,
                                    "barge-in rejected by server; playout resumed",
                                );
                            } else {
                                debug!(
                                    call_id = %self.call_id,
                                    "barge_in_reject with no pending arbitration; ignoring",
                                );
                            }
                        }
                        TapCommand::Clear => {
                            // `clear` during a pending pause arbitration
                            // acts as confirm (design note §2) — the
                            // count is bumped once, via the arbitration
                            // path.
                            if pending_verdict.is_some() {
                                self.abandon_arbitration(&mut pending_verdict, &events_tx, "clear");
                            } else {
                                // CDR barge_in_count: a server-driven clear
                                // is the Notify-mode barge-in (the server
                                // heard speech_started and interrupted its
                                // playout).
                                self.barge_in_count += 1;
                                self.publish_quality();
                            }
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
                            // Playout dropped → the bot is no longer talking
                            // for barge-in gating. (Mark bookkeeping is left
                            // as-is, matching prior behaviour.)
                            playout_until = None;
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
                            // Arbitration is suspended in rooms (§7.2) —
                            // a pending one resolves as confirm before
                            // the re-plumb.
                            self.abandon_arbitration(&mut pending_verdict, &events_tx, "join_room");
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
                        TapCommand::Park { moh } => {
                            // Park takes over the caller's ear — a
                            // pending pause arbitration resolves as
                            // confirm (§7.2).
                            self.abandon_arbitration(&mut pending_verdict, &events_tx, "park");
                            // Parking while in a room first leaves it
                            // (drops the RoomSender → the room reaps us).
                            if let Some(send) = room_send.take() {
                                drop(send);
                                room_sip_out = None;
                                room_ws_out = None;
                                room_events = None;
                            }
                            info!(
                                call_id = %self.call_id,
                                "tap parked; playing hold music, WS detached"
                            );
                            if let Some((_, done, frames)) = announcing.take() {
                                debug!(call_id = %self.call_id,
                                       "announcement cut short by park");
                                let _ = done.send(frames * 20);
                            }
                            parked = Some(*moh);
                            // Align the MOH cadence to now so the first
                            // frame plays ~20 ms from here, not whenever
                            // the free-running interval next ticks.
                            moh_tick.reset();
                        }
                        TapCommand::Unpark {
                            caller_audio_tx: new_caller_tx,
                            playout_audio_rx: new_playout_rx,
                            events_tx: new_events_tx,
                        } => {
                            if parked.take().is_some() {
                                // Swap to the fresh bridge's channels.
                                caller_audio_tx = new_caller_tx;
                                playout_audio_rx = new_playout_rx;
                                events_tx = new_events_tx;
                                frames_sent_to_forge = 0;
                                first_audio_pushed_at = None;
                                playout_until = None;
                                // The fresh channels are live — clear the
                                // WS-drop hold so the audio arms poll again
                                // (0.7.3 reconnect resume / park retrieve).
                                ws_dropped = false;
                                info!(
                                    call_id = %self.call_id,
                                    "tap retrieved; direct bridge restored on fresh WS session"
                                );
                            }
                        }
                        TapCommand::Hold { moh } => {
                            // Same as Park: MOH takes over the caller's
                            // ear, pending arbitration resolves as confirm.
                            self.abandon_arbitration(&mut pending_verdict, &events_tx, "hold");
                            // Holding while in a room first leaves it
                            // (drops the RoomSender → the room reaps us),
                            // same as Park.
                            if let Some(send) = room_send.take() {
                                drop(send);
                                room_sip_out = None;
                                room_ws_out = None;
                                room_events = None;
                            }
                            info!(
                                call_id = %self.call_id,
                                "tap held; playing hold music, WS attached but paused"
                            );
                            if let Some((_, done, frames)) = announcing.take() {
                                debug!(call_id = %self.call_id,
                                       "announcement cut short by hold");
                                let _ = done.send(frames * 20);
                            }
                            held = Some(*moh);
                            // Align the MOH cadence to now (same as Park).
                            moh_tick.reset();
                        }
                        TapCommand::Announce { source, done } => {
                            // The prompt preempts playout — pending
                            // arbitration resolves as confirm (§7.2).
                            self.abandon_arbitration(&mut pending_verdict, &events_tx, "announce");
                            if held.is_some() {
                                // Hold owns the caller's ear; skip the
                                // prompt rather than queue it.
                                debug!(call_id = %self.call_id,
                                       "announce skipped: call is held");
                                let _ = done.send(0);
                            } else {
                                // Announce-over-park (0.34.0,
                                // DESIGN_WS_FAILURE_PROMPT.md §3.3): an
                                // announcement STARTED while parked plays
                                // — the tick prefers it over MOH until
                                // EOF (the WS-failure prompt after an
                                // exhausted reconnect window). The
                                // reverse order is unchanged: a Park
                                // arriving mid-announcement still cuts
                                // it short (consent semantics).
                                info!(
                                    call_id = %self.call_id,
                                    over_moh = parked.is_some(),
                                    "announcement started",
                                );
                                announcing = Some((source, done, 0));
                                moh_tick.reset();
                            }
                        }
                        TapCommand::Unhold => {
                            if held.take().is_some() {
                                // The WS channels never changed — just
                                // flush any MOH tail out of forge's leg-A
                                // queue so the resumed bot audio doesn't
                                // play behind leftover hold music, and
                                // reset the Mark bookkeeping.
                                if let Err(e) = self.handle.flush(Some(MediaTarget::A), None).await {
                                    warn!(
                                        call_id = %self.call_id,
                                        error = %e,
                                        "forge flush failed on unhold",
                                    );
                                }
                                frames_sent_to_forge = 0;
                                first_audio_pushed_at = None;
                                info!(
                                    call_id = %self.call_id,
                                    "tap resumed; direct bridge restored on existing WS session"
                                );
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
                    if let Some(rx) = &snap.rx {
                        metrics::histogram!("siphon_ai_rtp_rx_jitter_ms")
                            .record(rx.jitter_ms as f64);
                    }
                    if let Some(m) = snap.mos_estimate {
                        metrics::histogram!("siphon_ai_rtp_mos_estimate").record(m as f64);
                    }
                    let out = OutgoingEvent::RtpStats {
                        jitter_ms: snap.jitter_ms,
                        packet_loss_ratio: snap.packet_loss_ratio,
                        rtcp_rtt_ms: snap.rtt_ms,
                        rx_jitter_ms: snap.rx.map(|rx| rx.jitter_ms),
                        rx_packets_received: snap.rx.map(|rx| rx.packets_received),
                        rx_packets_lost: snap.rx.map(|rx| rx.packets_lost),
                        rx_packets_out_of_order: snap.rx.map(|rx| rx.packets_out_of_order),
                        rx_packets_duplicate: snap.rx.map(|rx| rx.packets_duplicate),
                        tx_packets_sent: snap.tx.map(|tx| tx.packets_sent),
                        tx_octets_sent: snap.tx.map(|tx| tx.octets_sent),
                        tx_packets_lost_reported: snap.tx_packets_lost_reported,
                        mos_estimate: snap.mos_estimate,
                    };
                    if let Err(e) = events_tx.try_send(out) {
                        warn!(
                            call_id = %self.call_id,
                            error = %e,
                            "events_tx full or closed; dropping rtp_stats event"
                        );
                    }
                }

                // MOH playout while parked or held: one hold-music frame
                // per 20 ms into the caller leg. Active in either state;
                // both share the same MohSource-driven tick.
                _ = moh_tick.tick(), if parked.is_some() || held.is_some() || announcing.is_some() => {
                    // A running announcement owns the tick over parked
                    // MOH (announce-over-park, 0.34.0 §3.3) — but never
                    // over hold, whose command site skips announcements
                    // outright. At announcement EOF a parked call falls
                    // back to MOH on the next tick.
                    if held.is_none() && (announcing.is_some() || parked.is_none()) {
                        if let Some((source, _, frames)) = announcing.as_mut() {
                            match source.next_frame() {
                                Some(samples) => {
                                    *frames += 1;
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
                                }
                                None => {
                                    if let Some((_, done, frames)) = announcing.take() {
                                        debug!(call_id = %self.call_id, ms = frames * 20,
                                               "announcement finished");
                                        let _ = done.send(frames * 20);
                                    }
                                }
                            }
                        }
                        continue;
                    }
                    if let Some(moh) = parked.as_mut().or(held.as_mut()) {
                        let samples = moh.next_frame();
                        // Recording right channel = what the caller
                        // hears = the hold music.
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
                    }
                }

                // Barge-in debounce elapsed with a candidate still held →
                // the speech sustained past the acoustic gate, so it's a
                // real barge-in for this layer. `auto_clear` flushes the
                // bot's playout; `pause` runs the same flush but retains
                // the tail and arms the semantic arbitration (§7.1: the
                // two filters compose). Either way the held
                // `speech_started` is forwarded. (A `SpeechStopped`
                // would have cleared `pending_barge_in` first, disabling
                // this arm.)
                _ = &mut barge_debounce, if pending_barge_in.is_some() => {
                    match self.barge_in_action {
                        BargeInAction::Pause { decision, .. } if pending_verdict.is_none() => {
                            let resume = begin_pause_arbitration(
                                &self.call_id,
                                &self.handle,
                                &mut playout_audio_rx,
                                &mut shadow,
                            )
                            .await;
                            frames_sent_to_forge = 0;
                            first_audio_pushed_at = None;
                            playout_until = None;
                            arb_deadline
                                .as_mut()
                                .reset(tokio::time::Instant::now() + decision);
                            pending_verdict = Some(PendingVerdict {
                                resume,
                                fresh: Vec::new(),
                                armed_at: Instant::now(),
                            });
                            // The held speech_started (forwarded below)
                            // is the arbitration request.
                            if let Some(out) = pending_barge_in.as_mut() {
                                stamp_decision_pending(out, decision);
                            }
                            debug!(
                                call_id = %self.call_id,
                                "barge-in sustained past debounce; pause arbitration armed",
                            );
                        }
                        BargeInAction::Pause { .. } => {
                            // An arbitration is somehow already pending
                            // (shouldn't happen — the gate only arms
                            // while playing, and the pause cleared the
                            // clock). Just forward the held event below.
                        }
                        _ => {
                            self.barge_in_count += 1;
                            self.publish_quality();
                            let mut drained = 0usize;
                            while let Ok(_bytes) = playout_audio_rx.try_recv() {
                                drained += 1;
                            }
                            if let Some(send) = &room_send {
                                send.clear_ws_input();
                            }
                            if let Err(e) = self.handle.flush(Some(MediaTarget::A), None).await {
                                warn!(
                                    call_id = %self.call_id,
                                    error = %e,
                                    "forge flush failed during confirmed barge-in",
                                );
                            } else {
                                debug!(
                                    call_id = %self.call_id,
                                    drained,
                                    "barge-in confirmed after debounce; dropped pending playout",
                                );
                            }
                            frames_sent_to_forge = 0;
                            first_audio_pushed_at = None;
                            playout_until = None;
                        }
                    }
                    if let Some(out) = pending_barge_in.take() {
                        if let Err(e) = events_tx.try_send(out) {
                            warn!(
                                call_id = %self.call_id,
                                error = %e,
                                "events_tx full or closed; dropping confirmed barge-in event"
                            );
                        }
                    }
                }

                // Pause-mode decision window elapsed without a server
                // verdict → apply the configured fallback (§1: the
                // default `confirm` fails toward not talking over the
                // caller, so a server that never arbitrates degrades to
                // "auto_clear delayed by the decision window").
                _ = &mut arb_deadline, if pending_verdict.is_some() => {
                    if let (Some(p), BargeInAction::Pause { on_timeout, .. }) =
                        (pending_verdict.take(), self.barge_in_action)
                    {
                        match on_timeout {
                            TimeoutVerdict::Confirm => {
                                self.note_barge_in_decision(p.armed_at, "timeout", true);
                                let fresh = self
                                    .repush_chunks(p.fresh, true, &mut shadow, shadow_cap)
                                    .await?;
                                restore_playout_clock(
                                    fresh,
                                    &mut frames_sent_to_forge,
                                    &mut first_audio_pushed_at,
                                    &mut playout_until,
                                );
                                emit_resolved(&events_tx, &self.call_id, BargeInOutcome::Timeout);
                                debug!(
                                    call_id = %self.call_id,
                                    fresh,
                                    "barge-in arbitration timed out; confirmed (tail dropped)",
                                );
                            }
                            TimeoutVerdict::Reject => {
                                self.note_barge_in_decision(p.armed_at, "timeout", false);
                                let resumed = self
                                    .repush_chunks(p.resume, false, &mut shadow, shadow_cap)
                                    .await?;
                                let fresh = self
                                    .repush_chunks(p.fresh, true, &mut shadow, shadow_cap)
                                    .await?;
                                restore_playout_clock(
                                    resumed + fresh,
                                    &mut frames_sent_to_forge,
                                    &mut first_audio_pushed_at,
                                    &mut playout_until,
                                );
                                emit_resolved(&events_tx, &self.call_id, BargeInOutcome::Timeout);
                                debug!(
                                    call_id = %self.call_id,
                                    resumed,
                                    fresh,
                                    "barge-in arbitration timed out; rejected (playout resumed)",
                                );
                            }
                        }
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
/// Whether the bot is (estimated to be) still playing audio out to the
/// caller — i.e. there is queued/playing playout. Used to gate barge-in:
/// a VAD speech-started while this is `true` is likely the bot's own echo
/// (or background noise) rather than the caller interrupting.
///
/// Reads the `playout_until` clock — the estimated wallclock at which the
/// most recently queued frame finishes — plus [`BARGE_IN_PLAYOUT_GRACE`].
/// The clock advances 20 ms per pushed frame from `max(now, cursor)`, so
/// it re-anchors after a silence gap between bot phrases rather than
/// drifting behind `now` (the bug in the original `first_push + frames ×
/// 20 ms` form, which used a cumulative frame count and so under-read once
/// any inter-phrase gap had elapsed). Resets to `None` on every flush, so
/// this reads `false` once the bot's utterance is cleared or has drained.
fn bot_is_playing(playout_until: Option<Instant>, now: Instant) -> bool {
    match playout_until {
        Some(end) => now < end + BARGE_IN_PLAYOUT_GRACE,
        None => false,
    }
}

/// How many queued 20 ms frames the playout clock says have NOT yet
/// played. The shadow ring is trimmed to this (plus one frame of
/// early-bias slop) on every push, so at pause time the ring IS the
/// resume tail. Rounds up — estimation slop must lean toward
/// repeating audio, never skipping it.
fn unplayed_frames(playout_until: Option<Instant>, now: Instant) -> usize {
    match playout_until {
        Some(end) if end > now => {
            ((end - now).as_millis() as u64).div_ceil(PLAYOUT_FRAME_MS) as usize
        }
        _ => 0,
    }
}

/// In-flight pause-mode barge-in arbitration (design note §5).
struct PendingVerdict {
    /// The unplayed tail retained at pause time (shadow ring +
    /// drained controller→tap channel bytes, in playout order).
    /// Re-queued on reject, dropped on confirm.
    resume: Vec<Vec<u8>>,
    /// Audio the WS server streamed *after* the pause (§5.3) — queued
    /// behind the tail on reject, plays immediately on confirm.
    fresh: Vec<Vec<u8>>,
    /// When the arbitration armed, for the decision-latency histogram.
    armed_at: Instant,
}

/// Arm a pause arbitration (design note §5.2): capture the resume
/// tail — the shadow ring (everything estimated unplayed) plus
/// whatever is still queued in the controller→tap channel — then
/// flush forge so the caller hears the bot stop within one frame,
/// exactly like `auto_clear`. The caller resets the Mark/playout
/// bookkeeping and arms the decision deadline.
async fn begin_pause_arbitration(
    call_id: &CallId,
    handle: &MediaBridgeHandle,
    playout_audio_rx: &mut mpsc::Receiver<Vec<u8>>,
    shadow: &mut VecDeque<Vec<u8>>,
) -> Vec<Vec<u8>> {
    let mut resume: Vec<Vec<u8>> = shadow.drain(..).collect();
    while let Ok(bytes) = playout_audio_rx.try_recv() {
        resume.push(bytes);
    }
    if let Err(e) = handle.flush(Some(MediaTarget::A), None).await {
        warn!(
            call_id = %call_id,
            error = %e,
            "forge flush failed while arming pause arbitration",
        );
    } else {
        debug!(
            call_id = %call_id,
            retained = resume.len(),
            "barge-in pause armed; playout flushed, tail retained",
        );
    }
    resume
}

/// Stamp the pause-mode arbitration fields onto a `speech_started`
/// about to be forwarded (design note §2): the event IS the
/// arbitration request, so the server knows a verdict is expected and
/// how long it has. A no-op on any other event shape.
fn stamp_decision_pending(out: &mut OutgoingEvent, decision: Duration) {
    if let OutgoingEvent::SpeechStarted {
        decision_pending,
        decision_deadline_ms,
        ..
    } = out
    {
        *decision_pending = true;
        *decision_deadline_ms = Some(decision.as_millis() as u64);
    }
}

/// Best-effort `barge_in_resolved` emission at arbitration resolution.
/// `debug!` (not `warn!`) on failure: the WS-drop abandonment path
/// resolves against an already-closed events channel by design.
fn emit_resolved(
    events_tx: &mpsc::Sender<OutgoingEvent>,
    call_id: &CallId,
    outcome: BargeInOutcome,
) {
    if let Err(e) = events_tx.try_send(OutgoingEvent::BargeInResolved { outcome }) {
        debug!(
            call_id = %call_id,
            error = %e,
            "events_tx full or closed; dropping barge_in_resolved",
        );
    }
}

/// Re-anchor the Mark / barge-in playout bookkeeping after re-queuing
/// `frames` chunks at arbitration resolution. Zero frames leaves
/// everything cleared — the pause's flush already reset it.
fn restore_playout_clock(
    frames: u64,
    frames_sent_to_forge: &mut u64,
    first_audio_pushed_at: &mut Option<Instant>,
    playout_until: &mut Option<Instant>,
) {
    if frames == 0 {
        return;
    }
    let now = Instant::now();
    *frames_sent_to_forge = frames;
    *first_audio_pushed_at = Some(now);
    *playout_until = Some(now + Duration::from_millis(frames * PLAYOUT_FRAME_MS));
}

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
            // Stamped by the pause-mode arm just before forwarding,
            // when this event arms an arbitration (0.32.0).
            decision_pending: false,
            decision_deadline_ms: None,
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
    fn bot_is_playing_tracks_queued_playout() {
        let t0 = Instant::now();
        // Nothing queued → not playing.
        assert!(!bot_is_playing(None, t0));
        // Cursor 200 ms out. Mid-playout → playing. Within the grace
        // margin past the cursor → still counted as playing (biased toward
        // gating). Well past cursor + grace → done.
        let until = Some(t0 + Duration::from_millis(200));
        assert!(bot_is_playing(until, t0 + Duration::from_millis(100)));
        assert!(bot_is_playing(until, t0 + Duration::from_millis(200)));
        assert!(bot_is_playing(
            until,
            t0 + Duration::from_millis(200) + BARGE_IN_PLAYOUT_GRACE - Duration::from_millis(1)
        ));
        assert!(!bot_is_playing(
            until,
            t0 + Duration::from_millis(200) + BARGE_IN_PLAYOUT_GRACE + Duration::from_millis(1)
        ));
    }

    #[test]
    fn unplayed_frames_rounds_up_and_clamps_at_zero() {
        let t0 = Instant::now();
        // Nothing queued / clock in the past → 0.
        assert_eq!(unplayed_frames(None, t0), 0);
        assert_eq!(unplayed_frames(Some(t0), t0 + Duration::from_millis(1)), 0);
        // Exact multiples and mid-frame remainders round UP — slop
        // must lean toward repeating audio, never skipping it.
        let until = Some(t0 + Duration::from_millis(100));
        assert_eq!(unplayed_frames(until, t0), 5);
        assert_eq!(unplayed_frames(until, t0 + Duration::from_millis(39)), 4);
        assert_eq!(unplayed_frames(until, t0 + Duration::from_millis(40)), 3);
        assert_eq!(unplayed_frames(until, t0 + Duration::from_millis(99)), 1);
    }

    #[test]
    fn with_barge_in_debounce_drops_zero() {
        let manager = Arc::new(MediaBridgeManager::new());
        let bus = Arc::new(EventBus::new());
        let tap = MediaTap::attach(&manager, &bus, CallId::new("c-deb"), 8000)
            .expect("attach")
            .with_barge_in_debounce(Some(Duration::ZERO));
        // Zero is normalized to None (no gate), matching "off".
        assert!(tap.barge_in_debounce.is_none());
        let tap = tap.with_barge_in_debounce(Some(Duration::from_millis(200)));
        assert_eq!(tap.barge_in_debounce, Some(Duration::from_millis(200)));
    }

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

        let room = crate::room::spawn_room(
            crate::room::RoomConfig {
                room_id: "r-tap".into(),
                sample_rate: 8000,
                max_calls: 8,
                join_tones: false,
            },
            None,
        );
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
