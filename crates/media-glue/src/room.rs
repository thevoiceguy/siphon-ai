//! Conference room: one task owning one `forge_mixer::AudioMixer`
//! and a 20 ms playout tick (DEV_PLAN_0.7.0.md §2.1).
//!
//! ## Model (§9.1: every leg keeps its WS session)
//!
//! A room mixes **2N** participants for N joined calls: each call
//! contributes its SIP leg (caller audio in) and its WS session (bot
//! audio in) as two independent mixer participants. Each side also
//! gets a per-sink output carrying the room mix **minus its own
//! input** — the caller never hears their own voice back, the bot
//! never hears its own playout, but the caller does hear their own
//! bot and the bot does hear its own caller (STT keeps working).
//!
//! ## Ownership and channels (CLAUDE.md §4.3 / §4.4)
//!
//! The room task is the only owner of the mixer. Joined calls hand
//! it frames over one shared bounded mpsc (samples tagged with the
//! participant id) and receive sink frames over per-sink bounded
//! mpscs. Sink delivery is `try_send` — a slow consumer loses frames
//! (counted) but can never stall the room or the other members. No
//! lock is shared with any tap's audio path, and the room never
//! reaches into a `CallController`: membership is an explicit
//! rendezvous a call opts into, same §4.4 stance as `ConsultRegistry`.
//!
//! ## Mixing: drain-once, subtract-self
//!
//! Upstream's `mix_excluding(id)` *drains* every contributor's
//! buffer, so calling it once per sink in the same tick would consume
//! a fresh (different!) frame per call and shear the room apart.
//! Instead each tick drains every ready participant exactly once
//! (`get_all_participant_audio`, gain applied) and computes each
//! sink's output as `sum - own`, applying the same `1/√n` auto-gain
//! and clamp the upstream mixer uses. The arithmetic is ~20 lines;
//! the mixer still owns buffering, gain, participant state and VAD.
//! (A `mix_all_excluding` upstream API would replace this — noted in
//! DEV_PLAN_0.7.0.md §6, ask the user before PRing.)
//!
//! ## Lifecycle
//!
//! A room is spawned on first join (`ConferenceRegistry` in
//! siphon-ai-core) and exits when its last member leaves. Leaving is
//! triple-redundant by design, because "the room died" must always
//! restore the direct caller↔WS pair (§2.1): an explicit
//! [`RoomCtrl::Leave`] (sent by [`RoomSender`]'s `Drop`), plus a
//! per-tick reap of members whose sink receivers were dropped, plus
//! sink `try_send` observing a closed channel mid-tick.

use std::collections::HashMap;
use std::time::Duration;

use forge_core::{AudioCodec, AudioFormat};
use forge_injection::{AudioSource, ToneGenerator};
use forge_mixer::AudioMixer;
use thiserror::Error;
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, info, warn};

/// One mixed frame is 20 ms, like everything else on the bridge
/// (CLAUDE.md §4.2).
const FRAME_MS: u64 = 20;

/// Per-sink output buffer: ~200 ms of audio, the standard audio
/// channel bound (CLAUDE.md §6.2).
const SINK_CHANNEL_FRAMES: usize = 10;

/// The shared input channel. Sized for a burst from every producer
/// at once (2 producers/call × 8 calls × a few frames each).
const INPUT_CHANNEL_FRAMES: usize = 64;

/// Control channel: small and bounded like every control channel.
const CTRL_CHANNEL_CAPACITY: usize = 8;

/// How much audio a participant may buffer inside the mixer before
/// the oldest samples are dropped. WS bots stream TTS faster than
/// realtime; 30 s absorbs a long prompt queued at once. (8 calls ×
/// 2 participants × 30 s × 16 kHz × 2 B ≈ 15 MB worst case — fine
/// for a daemon-level facility capped at `max_rooms`.)
const MAX_BUFFER_SECONDS: usize = 30;

/// Join/leave chime parameters (active when `[conference].join_tones`
/// is on): a short, quiet single tone; rising for join, falling for
/// leave.
const TONE_MS: u32 = 150;
const TONE_AMPLITUDE: f32 = 0.2;
const JOIN_TONE_HZ: f32 = 660.0;
const LEAVE_TONE_HZ: f32 = 440.0;

/// The mixer participant that carries join/leave tones. Never a
/// member; contributes only while tone samples remain buffered.
/// Bridge call ids are `siphon-…`, so this can't collide.
const TONE_PARTICIPANT: &str = "__room_tone__";

/// Metric names. Literals must match the consts in
/// `siphon-ai-telemetry::metrics` (same pattern as
/// `siphon_ai_rtp_rtt_ms`).
const METRIC_CONFERENCES_ACTIVE: &str = "siphon_ai_conferences_active";
const METRIC_PARTICIPANTS: &str = "siphon_ai_conference_participants";
const METRIC_TICK_LAG: &str = "siphon_ai_room_tick_lag_seconds";
const METRIC_FRAMES_DROPPED: &str = "siphon_ai_room_frames_dropped_total";

/// Why a join was refused. The WS surface (chunk 2) maps these onto
/// `error { code: "conference_failed" }` details.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum RoomJoinError {
    /// `[conference].max_participants_per_room` reached (counted in
    /// *calls*, not mixer entries).
    #[error("room is full ({max_calls} calls)")]
    RoomFull { max_calls: usize },

    /// The room locked to its first joiner's sample rate and this
    /// call negotiated a different one. No resampling in 0.7.0 —
    /// documented limitation (DEV_PLAN_0.7.0.md §2.1).
    #[error("room runs at {room_rate} Hz; call negotiated {call_rate} Hz")]
    SampleRateMismatch { room_rate: u32, call_rate: u32 },

    /// The call is already a member of this room.
    #[error("call is already a member of this room")]
    AlreadyJoined,

    /// The room task is gone (last member left between lookup and
    /// join). The registry retries once with a fresh room.
    #[error("room is gone")]
    RoomClosed,
}

/// Parameters a room is spawned with. The sample rate is the first
/// joiner's negotiated rate; the caps come from `[conference]`.
#[derive(Debug, Clone)]
pub struct RoomConfig {
    pub room_id: String,
    /// 8000 or 16000 — the rate the room is locked to.
    pub sample_rate: u32,
    /// Maximum member *calls* (each contributes 2 mixer participants).
    pub max_calls: usize,
    /// Play a short chime into the room on every join/leave.
    pub join_tones: bool,
}

/// Control-plane requests into the room task.
enum RoomCtrl {
    Join {
        call_id: String,
        sample_rate: u32,
        reply: oneshot::Sender<Result<RoomMembership, RoomJoinError>>,
    },
    Leave {
        call_id: String,
    },
    /// Drop everything a participant has buffered in the mixer —
    /// the room-mode analogue of the tap's `Clear`/`Mute` flush, so
    /// barge-in still silences a bot's queued tail immediately.
    ClearInput {
        participant: String,
    },
}

/// Cheap-to-clone handle to a running room. Held by the
/// `ConferenceRegistry`; `join` is the only way in.
#[derive(Debug, Clone)]
pub struct RoomHandle {
    room_id: String,
    sample_rate: u32,
    ctrl_tx: mpsc::Sender<RoomCtrl>,
}

impl RoomHandle {
    pub fn room_id(&self) -> &str {
        &self.room_id
    }

    /// The rate the room is locked to (its first joiner's).
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// True once the room task has exited (last member left). A
    /// closed handle never accepts another join — the registry
    /// replaces it with a fresh room.
    pub fn is_closed(&self) -> bool {
        self.ctrl_tx.is_closed()
    }

    /// Join `call_id` to the room. On success the returned
    /// [`RoomMembership`] carries everything the call's tap needs to
    /// re-plumb (see [`MediaTap`](crate::MediaTap) +
    /// [`TapCommand::JoinRoom`](crate::TapCommand)).
    pub async fn join(
        &self,
        call_id: &str,
        call_sample_rate: u32,
    ) -> Result<RoomMembership, RoomJoinError> {
        let (reply, reply_rx) = oneshot::channel();
        self.ctrl_tx
            .send(RoomCtrl::Join {
                call_id: call_id.to_string(),
                sample_rate: call_sample_rate,
                reply,
            })
            .await
            .map_err(|_| RoomJoinError::RoomClosed)?;
        reply_rx.await.map_err(|_| RoomJoinError::RoomClosed)?
    }

    /// Remove `call_id` from the room. Best-effort and idempotent —
    /// the per-tick reap is the backstop if the control channel is
    /// full or the call already left.
    pub fn leave(&self, call_id: &str) {
        let _ = self.ctrl_tx.try_send(RoomCtrl::Leave {
            call_id: call_id.to_string(),
        });
    }
}

/// Everything a joined call holds: the send side (input channel +
/// participant ids + leave-on-drop) and the two per-sink receivers.
/// Handed to the call's tap via `TapCommand::JoinRoom`; split with
/// [`Self::split`] inside the tap.
#[derive(Debug)]
pub struct RoomMembership {
    pub(crate) sender: RoomSender,
    /// Mix-minus-`sip` — what the SIP caller hears (their bot + the
    /// other calls), played out to RTP.
    pub(crate) sip_out_rx: mpsc::Receiver<Vec<i16>>,
    /// Mix-minus-`ws` — what the bot hears (its own caller + the
    /// other calls), forwarded to the WS as caller audio.
    pub(crate) ws_out_rx: mpsc::Receiver<Vec<i16>>,
}

impl RoomMembership {
    pub fn room_id(&self) -> &str {
        &self.sender.room_id
    }

    /// Split into the send half and the two sink receivers (tap
    /// keeps them in separate locals so its `select!` arms can
    /// borrow them independently).
    pub(crate) fn split(
        self,
    ) -> (
        RoomSender,
        mpsc::Receiver<Vec<i16>>,
        mpsc::Receiver<Vec<i16>>,
    ) {
        (self.sender, self.sip_out_rx, self.ws_out_rx)
    }
}

/// The send half of a membership. Dropping it tells the room to
/// remove the call (so every tap exit path — clean or not — leaves
/// the room without explicit bookkeeping).
#[derive(Debug)]
pub struct RoomSender {
    room_id: String,
    call_id: String,
    sip_id: String,
    ws_id: String,
    input_tx: mpsc::Sender<(String, Vec<i16>)>,
    ctrl_tx: mpsc::Sender<RoomCtrl>,
}

impl RoomSender {
    pub fn room_id(&self) -> &str {
        &self.room_id
    }

    /// Forward one 20 ms frame of SIP-caller samples into the room.
    /// Non-blocking: a full room input drops the frame (counted) —
    /// the tap's audio loop must never stall on the room.
    pub fn send_sip(&self, samples: Vec<i16>) {
        self.send(&self.sip_id, "sip", samples);
    }

    /// Forward one frame of WS-bot playout samples into the room.
    pub fn send_ws(&self, samples: Vec<i16>) {
        self.send(&self.ws_id, "ws", samples);
    }

    fn send(&self, participant: &str, side: &'static str, samples: Vec<i16>) {
        if let Err(mpsc::error::TrySendError::Full(_)) =
            self.input_tx.try_send((participant.to_string(), samples))
        {
            metrics::counter!(METRIC_FRAMES_DROPPED, "stage" => "input", "side" => side)
                .increment(1);
        }
    }

    /// Drop whatever the bot has buffered in the room mixer — wired
    /// to the tap's `Clear`/`Mute` so barge-in silences the queued
    /// TTS tail inside the room too, not just forge's playout queue.
    pub fn clear_ws_input(&self) {
        let _ = self.ctrl_tx.try_send(RoomCtrl::ClearInput {
            participant: self.ws_id.clone(),
        });
    }
}

impl Drop for RoomSender {
    fn drop(&mut self) {
        // Best-effort prompt leave; the room's per-tick reap of
        // closed sinks is the guaranteed path.
        let _ = self.ctrl_tx.try_send(RoomCtrl::Leave {
            call_id: self.call_id.clone(),
        });
    }
}

/// Per-member bookkeeping inside the room task.
struct Member {
    sip_id: String,
    ws_id: String,
    sip_out_tx: mpsc::Sender<Vec<i16>>,
    ws_out_tx: mpsc::Sender<Vec<i16>>,
}

impl Member {
    /// Both sink receivers dropped → the tap is gone; reap.
    fn is_abandoned(&self) -> bool {
        self.sip_out_tx.is_closed() && self.ws_out_tx.is_closed()
    }
}

/// Spawn a room task. Returns immediately; the room runs until its
/// last member leaves.
pub fn spawn_room(cfg: RoomConfig) -> RoomHandle {
    let (ctrl_tx, ctrl_rx) = mpsc::channel(CTRL_CHANNEL_CAPACITY);
    let (input_tx, input_rx) = mpsc::channel(INPUT_CHANNEL_FRAMES);
    let handle = RoomHandle {
        room_id: cfg.room_id.clone(),
        sample_rate: cfg.sample_rate,
        ctrl_tx: ctrl_tx.clone(),
    };
    tokio::spawn(run_room(cfg, ctrl_tx, ctrl_rx, input_tx, input_rx));
    handle
}

/// RAII gauge decrement so early exits can't leak gauge state.
struct GaugeGuard(&'static str, f64);
impl GaugeGuard {
    fn new(name: &'static str, amount: f64) -> Self {
        metrics::gauge!(name).increment(amount);
        Self(name, amount)
    }
}
impl Drop for GaugeGuard {
    fn drop(&mut self) {
        metrics::gauge!(self.0).decrement(self.1);
    }
}

async fn run_room(
    cfg: RoomConfig,
    ctrl_tx: mpsc::Sender<RoomCtrl>,
    mut ctrl_rx: mpsc::Receiver<RoomCtrl>,
    input_tx: mpsc::Sender<(String, Vec<i16>)>,
    mut input_rx: mpsc::Receiver<(String, Vec<i16>)>,
) {
    let frame_size = (cfg.sample_rate / 1000) as usize * FRAME_MS as usize;
    let mixer = match AudioMixer::with_options(
        AudioFormat::new(cfg.sample_rate, 1, AudioCodec::PCM),
        frame_size,
        forge_mixer::MixerOptions {
            max_buffer_frames: MAX_BUFFER_SECONDS * (1000 / FRAME_MS as usize),
            recording_base_dir: None,
            recording_root_jail: None,
        },
    ) {
        Ok(m) => m,
        Err(e) => {
            // Unreachable with our validated inputs (frame_size > 0),
            // but a room that can't mix must die loudly, not panic.
            warn!(room_id = %cfg.room_id, error = %e, "room mixer construction failed");
            return;
        }
    };
    if cfg.join_tones {
        if let Err(e) = mixer.add_participant(TONE_PARTICIPANT, Some(1.0)) {
            warn!(room_id = %cfg.room_id, error = %e, "tone participant rejected; tones off");
        }
    }

    let _active = GaugeGuard::new(METRIC_CONFERENCES_ACTIVE, 1.0);
    let mut participants_gauge: Option<GaugeGuard> = None;
    info!(room_id = %cfg.room_id, sample_rate = cfg.sample_rate, "conference room created");

    let mut members: HashMap<String, Member> = HashMap::new();
    let mut ever_joined = false;

    // 20 ms cadence from a monotonic interval (CLAUDE.md §4.3 — not
    // a self-correcting sleep). `Delay` keeps inter-tick spacing
    // stable after a stall; the mixer's per-participant buffers
    // absorb the backlog.
    let mut tick = tokio::time::interval(Duration::from_millis(FRAME_MS));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut last_tick: Option<tokio::time::Instant> = None;

    // Reused across ticks — no steady-state allocation for the sum.
    let mut sum_buf: Vec<i32> = vec![0; frame_size];

    loop {
        tokio::select! {
            ctrl = ctrl_rx.recv() => {
                // `ctrl_tx` is held by this task itself (cloned into
                // each membership's RoomSender), so recv() can't
                // return None while we run.
                let Some(ctrl) = ctrl else { continue };
                match ctrl {
                    RoomCtrl::Join { call_id, sample_rate, reply } => {
                        let result = handle_join(
                            &cfg, &mixer, &mut members,
                            &ctrl_tx, &input_tx,
                            call_id, sample_rate,
                        );
                        if result.is_ok() {
                            ever_joined = true;
                            bump_participants(&mut participants_gauge, members.len());
                            if cfg.join_tones {
                                play_tone(&mixer, JOIN_TONE_HZ, cfg.sample_rate);
                            }
                        }
                        // Receiver dropped = joiner gave up; the
                        // membership inside `result` drops here and
                        // its RoomSender fires Leave, cleaning up.
                        let _ = reply.send(result);
                    }
                    RoomCtrl::Leave { call_id } => {
                        if remove_member(&cfg, &mixer, &mut members, &call_id) {
                            bump_participants(&mut participants_gauge, members.len());
                            if cfg.join_tones {
                                play_tone(&mixer, LEAVE_TONE_HZ, cfg.sample_rate);
                            }
                        }
                    }
                    RoomCtrl::ClearInput { participant } => {
                        // Unknown id just means the member already
                        // left — a harmless race.
                        let _ = mixer.clear_buffer(&participant);
                    }
                }
            }

            frame = input_rx.recv() => {
                // Same keepalive argument as ctrl_rx: this task owns
                // a sender clone, so None is unreachable.
                let Some((participant, samples)) = frame else { continue };
                // A frame racing a leave targets an unknown id;
                // dropping it is correct.
                if let Err(e) = mixer.write_samples(&participant, &samples) {
                    debug!(room_id = %cfg.room_id, %participant, error = %e,
                           "dropping input frame for unknown participant");
                }
            }

            now = tick.tick() => {
                if let Some(prev) = last_tick {
                    // Lag = how far past the 20 ms cadence this tick
                    // fired. Healthy rooms sit near zero.
                    let lag = now.duration_since(prev).as_secs_f64() - 0.020;
                    metrics::histogram!(METRIC_TICK_LAG).record(lag.max(0.0));
                }
                last_tick = Some(now);

                mix_and_fan_out(&mixer, &members, frame_size, &mut sum_buf);

                // Reap members whose tap dropped both sink receivers
                // without an explicit leave (controller crash, tap
                // teardown mid-call).
                let abandoned: Vec<String> = members
                    .iter()
                    .filter(|(_, m)| m.is_abandoned())
                    .map(|(id, _)| id.clone())
                    .collect();
                for call_id in abandoned {
                    if remove_member(&cfg, &mixer, &mut members, &call_id) {
                        bump_participants(&mut participants_gauge, members.len());
                        if cfg.join_tones {
                            play_tone(&mixer, LEAVE_TONE_HZ, cfg.sample_rate);
                        }
                    }
                }

                if ever_joined && members.is_empty() {
                    info!(room_id = %cfg.room_id, "last member left; conference room ends");
                    return;
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_join(
    cfg: &RoomConfig,
    mixer: &AudioMixer,
    members: &mut HashMap<String, Member>,
    ctrl_tx: &mpsc::Sender<RoomCtrl>,
    input_tx: &mpsc::Sender<(String, Vec<i16>)>,
    call_id: String,
    sample_rate: u32,
) -> Result<RoomMembership, RoomJoinError> {
    if sample_rate != cfg.sample_rate {
        return Err(RoomJoinError::SampleRateMismatch {
            room_rate: cfg.sample_rate,
            call_rate: sample_rate,
        });
    }
    if members.contains_key(&call_id) {
        return Err(RoomJoinError::AlreadyJoined);
    }
    if members.len() >= cfg.max_calls {
        return Err(RoomJoinError::RoomFull {
            max_calls: cfg.max_calls,
        });
    }

    let sip_id = format!("{call_id}:sip");
    let ws_id = format!("{call_id}:ws");
    // `add_participant` only fails on a recording-path misconfig we
    // never set; treat failure as a full room rather than panicking.
    for id in [&sip_id, &ws_id] {
        if let Err(e) = mixer.add_participant(id.clone(), None) {
            warn!(room_id = %cfg.room_id, %call_id, error = %e, "mixer rejected participant");
            let _ = mixer.remove_participant(&sip_id);
            return Err(RoomJoinError::RoomFull {
                max_calls: cfg.max_calls,
            });
        }
    }

    let (sip_out_tx, sip_out_rx) = mpsc::channel(SINK_CHANNEL_FRAMES);
    let (ws_out_tx, ws_out_rx) = mpsc::channel(SINK_CHANNEL_FRAMES);
    members.insert(
        call_id.clone(),
        Member {
            sip_id: sip_id.clone(),
            ws_id: ws_id.clone(),
            sip_out_tx,
            ws_out_tx,
        },
    );
    info!(
        room_id = %cfg.room_id,
        %call_id,
        calls = members.len(),
        "call joined conference room"
    );

    Ok(RoomMembership {
        sender: RoomSender {
            room_id: cfg.room_id.clone(),
            call_id,
            sip_id,
            ws_id,
            input_tx: input_tx.clone(),
            ctrl_tx: ctrl_tx.clone(),
        },
        sip_out_rx,
        ws_out_rx,
    })
}

/// Remove a member's two mixer participants and sink senders.
/// Returns false when the call wasn't a member (double-leave race).
fn remove_member(
    cfg: &RoomConfig,
    mixer: &AudioMixer,
    members: &mut HashMap<String, Member>,
    call_id: &str,
) -> bool {
    let Some(member) = members.remove(call_id) else {
        return false;
    };
    let _ = mixer.remove_participant(&member.sip_id);
    let _ = mixer.remove_participant(&member.ws_id);
    info!(
        room_id = %cfg.room_id,
        %call_id,
        calls = members.len(),
        "call left conference room"
    );
    true
}

/// Keep the participants gauge equal to the live mixer-entry count
/// (2 per member call — the number the SIPp DoD asserts).
fn bump_participants(guard: &mut Option<GaugeGuard>, member_calls: usize) {
    *guard = None; // decrement the previous value first
    if member_calls > 0 {
        *guard = Some(GaugeGuard::new(
            METRIC_PARTICIPANTS,
            (member_calls * 2) as f64,
        ));
    }
}

/// One tick: drain every ready participant once, then hand each sink
/// `sum - own` with upstream's auto-gain (`1/√n` when n > 1) + clamp.
fn mix_and_fan_out(
    mixer: &AudioMixer,
    members: &HashMap<String, Member>,
    frame_size: usize,
    sum_buf: &mut [i32],
) {
    let frames = mixer.get_all_participant_audio(frame_size, None);
    let contributors = frames.len();
    if contributors == 0 {
        return;
    }

    sum_buf.fill(0);
    for samples in frames.values() {
        for (acc, &s) in sum_buf.iter_mut().zip(samples.iter()) {
            *acc += s as i32;
        }
    }

    for member in members.values() {
        for (participant, out_tx, side) in [
            (&member.sip_id, &member.sip_out_tx, "sip"),
            (&member.ws_id, &member.ws_out_tx, "ws"),
        ] {
            let own = frames.get(participant.as_str());
            let n = contributors - usize::from(own.is_some());
            if n == 0 {
                // Nothing but this sink's own audio this tick —
                // sending silence would only mask the pair-wise
                // direct path's behavior; skip.
                continue;
            }
            let gain = if n > 1 { 1.0 / (n as f32).sqrt() } else { 1.0 };
            // One Vec per sink per tick: the channel handoff needs
            // an owned buffer, same per-frame budget as the tap's
            // pack/unpack (see module docs + DEV_PLAN_0.7.0.md §6).
            let mut out = Vec::with_capacity(frame_size);
            match own {
                Some(own) => out.extend(
                    sum_buf
                        .iter()
                        .zip(own.iter())
                        .map(|(&sum, &o)| scale(sum - o as i32, gain)),
                ),
                None => out.extend(sum_buf.iter().map(|&sum| scale(sum, gain))),
            }
            if let Err(mpsc::error::TrySendError::Full(_)) = out_tx.try_send(out) {
                metrics::counter!(
                    METRIC_FRAMES_DROPPED, "stage" => "sink", "side" => side
                )
                .increment(1);
            }
            // Closed sinks are reaped by the caller's per-tick sweep.
        }
    }
}

/// Upstream's clamp-with-gain, bit-for-bit (`mixer.rs::mix*`).
fn scale(value: i32, gain: f32) -> i16 {
    ((value as f32 * gain).clamp(-32768.0, 32767.0)) as i16
}

/// Queue a short chime into the tone participant's buffer. The tone
/// participates in mixing only while these samples last.
fn play_tone(mixer: &AudioMixer, frequency: f32, sample_rate: u32) {
    let total = (sample_rate as usize * TONE_MS as usize) / 1000;
    let mut gen = ToneGenerator::single_tone(frequency, sample_rate)
        .with_duration(TONE_MS)
        .with_amplitude(TONE_AMPLITUDE);
    match gen.read_frame(total) {
        Ok(samples) => {
            if let Err(e) = mixer.write_samples(TONE_PARTICIPANT, &samples) {
                debug!(error = %e, "tone write skipped");
            }
        }
        Err(e) => debug!(error = %e, "tone generation failed"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::time::{timeout, Duration};

    const WAIT: Duration = Duration::from_secs(2);

    fn cfg(id: &str, rate: u32, max_calls: usize, tones: bool) -> RoomConfig {
        RoomConfig {
            room_id: id.to_string(),
            sample_rate: rate,
            max_calls,
            join_tones: tones,
        }
    }

    /// A 20 ms frame of constant samples at 8 kHz.
    fn const_frame(value: i16) -> Vec<i16> {
        vec![value; 160]
    }

    /// Pump `frames` constant-valued SIP frames for a member while
    /// awaiting the first sink frame on `rx` that contains non-zero
    /// audio. Returns that frame.
    async fn first_audible(rx: &mut mpsc::Receiver<Vec<i16>>) -> Vec<i16> {
        timeout(WAIT, async {
            loop {
                let frame = rx.recv().await.expect("sink open");
                if frame.iter().any(|&s| s != 0) {
                    return frame;
                }
            }
        })
        .await
        .expect("audible frame before timeout")
    }

    #[tokio::test]
    async fn two_calls_hear_each_other_minus_self() {
        let handle = spawn_room(cfg("r1", 8000, 8, false));
        let a = handle.join("call-a", 8000).await.expect("join a");
        let b = handle.join("call-b", 8000).await.expect("join b");
        let (a_send, mut a_sip_out, mut a_ws_out) = a.split();
        let (b_send, mut b_sip_out, _b_ws_out) = b.split();

        // Feed both SIP legs continuously so every tick has a full
        // frame from each (the WS sides stay silent — like real bots
        // that only speak occasionally).
        let pump = tokio::spawn(async move {
            loop {
                a_send.send_sip(const_frame(1000));
                b_send.send_sip(const_frame(2000));
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        });

        // Caller A's ear: mix minus A's own voice = B only (single
        // contributor → no auto-gain) = exactly 2000.
        let a_hears = first_audible(&mut a_sip_out).await;
        assert!(a_hears.iter().all(|&s| s == 2000), "a_sip_out = B only");

        // Caller B's ear: A only.
        let b_hears = first_audible(&mut b_sip_out).await;
        assert!(b_hears.iter().all(|&s| s == 1000), "b_sip_out = A only");

        // Bot A's ear: both SIP legs (its own caller included), two
        // contributors → auto-gain 1/√2: (1000 + 2000) * 0.7071 ≈ 2121.
        let bot_a_hears = first_audible(&mut a_ws_out).await;
        let expected = ((3000_f32) * (1.0 / 2_f32.sqrt())) as i16;
        assert!(
            bot_a_hears.iter().all(|&s| (s - expected).abs() <= 1),
            "a_ws_out should carry both callers with auto-gain, got {} want ~{}",
            bot_a_hears[0],
            expected
        );

        pump.abort();
    }

    #[tokio::test]
    async fn sample_rate_mismatch_is_rejected() {
        let handle = spawn_room(cfg("r2", 8000, 8, false));
        let _a = handle.join("call-a", 8000).await.expect("join a");
        let err = handle.join("call-b", 16000).await.unwrap_err();
        assert_eq!(
            err,
            RoomJoinError::SampleRateMismatch {
                room_rate: 8000,
                call_rate: 16000
            }
        );
    }

    #[tokio::test]
    async fn room_full_rejects_join_beyond_cap() {
        let handle = spawn_room(cfg("r3", 8000, 2, false));
        let _a = handle.join("call-a", 8000).await.expect("join a");
        let _b = handle.join("call-b", 8000).await.expect("join b");
        let err = handle.join("call-c", 8000).await.unwrap_err();
        assert_eq!(err, RoomJoinError::RoomFull { max_calls: 2 });
    }

    #[tokio::test]
    async fn duplicate_join_is_rejected() {
        let handle = spawn_room(cfg("r4", 8000, 8, false));
        let _a = handle.join("call-a", 8000).await.expect("join a");
        let err = handle.join("call-a", 8000).await.unwrap_err();
        assert_eq!(err, RoomJoinError::AlreadyJoined);
    }

    #[tokio::test]
    async fn explicit_leave_closes_sinks_and_last_leave_ends_room() {
        let handle = spawn_room(cfg("r5", 8000, 8, false));
        let a = handle.join("call-a", 8000).await.expect("join a");
        let (a_send, mut a_sip_out, _a_ws_out) = a.split();

        handle.leave("call-a");
        // Sink closes when the room removes the member.
        let closed = timeout(WAIT, async {
            loop {
                if a_sip_out.recv().await.is_none() {
                    return true;
                }
            }
        })
        .await
        .expect("sink closed");
        assert!(closed);

        // Last member gone → room task exits → handle reports closed
        // and a fresh join is refused.
        timeout(WAIT, async {
            while !handle.is_closed() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("room exits after last leave");
        let err = handle.join("call-b", 8000).await.unwrap_err();
        assert_eq!(err, RoomJoinError::RoomClosed);

        drop(a_send);
    }

    #[tokio::test]
    async fn dropped_membership_is_reaped_without_explicit_leave() {
        let handle = spawn_room(cfg("r6", 8000, 8, false));
        let a = handle.join("call-a", 8000).await.expect("join a");
        // Simulate a tap teardown that never sends LeaveRoom: drop
        // everything (RoomSender's Drop fires Leave; even without
        // it, the closed-sink reap catches this — exercised in
        // `abandoned_member_reaped_by_tick` below).
        drop(a);

        timeout(WAIT, async {
            while !handle.is_closed() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("room exits after membership dropped");
    }

    #[tokio::test]
    async fn abandoned_member_reaped_by_tick_even_if_leave_is_lost() {
        let handle = spawn_room(cfg("r7", 8000, 8, false));
        let a = handle.join("call-a", 8000).await.expect("join a");
        let (a_send, a_sip_out, a_ws_out) = a.split();
        // Drop only the receivers and *forget* the sender's Drop by
        // leaking it — the per-tick is_closed() reap must still
        // remove the member and end the room.
        drop(a_sip_out);
        drop(a_ws_out);
        std::mem::forget(a_send);

        timeout(WAIT, async {
            while !handle.is_closed() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("tick reap removes abandoned member");
    }

    #[tokio::test]
    async fn join_tone_is_audible_without_any_input() {
        let handle = spawn_room(cfg("r8", 8000, 8, true));
        let a = handle.join("call-a", 8000).await.expect("join a");
        let b = handle.join("call-b", 8000).await.expect("join b");
        let (_a_send, mut a_sip_out, _a_ws_out) = a.split();

        // B's join chime reaches A with no participant ever sending
        // audio (the tone participant is the only contributor).
        let frame = first_audible(&mut a_sip_out).await;
        assert!(frame.iter().any(|&s| s != 0));

        drop(b);
    }

    #[tokio::test]
    async fn clear_ws_input_drops_buffered_bot_audio() {
        let handle = spawn_room(cfg("r9", 8000, 8, false));
        let a = handle.join("call-a", 8000).await.expect("join a");
        let b = handle.join("call-b", 8000).await.expect("join b");
        let (a_send, _a_sip_out, _a_ws_out) = a.split();
        let (_b_send, mut b_sip_out, _b_ws_out) = b.split();

        // Queue ~2 s of bot-A audio, then clear it before (most of)
        // it can play out; B should go silent shortly after.
        for _ in 0..100 {
            a_send.send_ws(const_frame(3000));
            // Stay under the input channel bound.
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        let _ = first_audible(&mut b_sip_out).await; // bot-A audio flowing
        a_send.clear_ws_input();

        // After the clear, B's sink dries up (no contributors), so
        // recv() blocks — assert via timeout on a *fresh* drain.
        timeout(WAIT, async {
            loop {
                // Drain whatever was already in flight…
                match timeout(Duration::from_millis(300), b_sip_out.recv()).await {
                    Ok(Some(_)) => continue, // tail still draining
                    Ok(None) => panic!("sink closed unexpectedly"),
                    Err(_) => return, // 300 ms of silence = cleared
                }
            }
        })
        .await
        .expect("bot audio stops after clear_ws_input");
    }
}
