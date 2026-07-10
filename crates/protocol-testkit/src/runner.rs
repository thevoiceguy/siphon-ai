//! Executes one scenario against a candidate server, playing the daemon.

use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde_json::Value;
use siphon_ai_bridge::{
    AudioEncoding, AudioFormat, BridgeIn, BridgeOut, CallId, Direction, SipMeta, StartMsg,
    PROTOCOL_VERSION,
};
use tokio::time::timeout;

use crate::client::{Incoming, WsClient};
use crate::report::ScenarioResult;
use crate::scenario::{Scenario, Step};
use crate::validate::MessageValidator;

pub const FRAME_MS: u64 = 20;

/// Hard cap per scenario so a wedged server can't hang a CI job.
const SCENARIO_DEADLINE: Duration = Duration::from_secs(60);

pub async fn run_scenario(
    url: &str,
    scenario: &Scenario,
    validator: &MessageValidator,
) -> ScenarioResult {
    let started = Instant::now();
    let mut result = ScenarioResult::new(&scenario.name);
    let outcome = timeout(
        SCENARIO_DEADLINE,
        drive(url, scenario, validator, &mut result),
    )
    .await;
    match outcome {
        Ok(Ok(())) => {}
        Ok(Err(e)) => result.failures.push(format!("{e:#}")),
        Err(_) => result.failures.push(format!(
            "scenario exceeded the {}s deadline (server wedged?)",
            SCENARIO_DEADLINE.as_secs()
        )),
    }
    result.duration_ms = started.elapsed().as_millis() as u64;
    result
}

struct SessionState {
    call_id: String,
    seq: u64,
    frame_bytes: usize,
    /// Reference points for the pacing bound.
    opened_at: Instant,
    frames_sent: u64,
    frames_received: u64,
    /// Server asked to hang up — the daemon honors it; so do we.
    hangup: bool,
}

impl SessionState {
    fn next_seq(&mut self) -> u64 {
        let s = self.seq;
        self.seq += 1;
        s
    }
}

async fn drive(
    url: &str,
    scenario: &Scenario,
    validator: &MessageValidator,
    result: &mut ScenarioResult,
) -> Result<()> {
    let mut client = WsClient::connect(url).await?;
    let mut state = open_session(&mut client, scenario, /*reconnected=*/ false).await?;

    for (idx, step) in scenario.steps.iter().enumerate() {
        if state.hangup {
            // PROTOCOL §4.2: after the server's `hangup` the daemon sends
            // `stop` and closes. We did; the rest of the script is moot.
            result.notes.push(format!(
                "server sent `hangup` — honored it; skipped remaining steps from #{}",
                idx + 1
            ));
            return Ok(());
        }
        let label = format!("step #{} {:?}", idx + 1, step_name(step));
        match step {
            Step::SendAudio { frames } => {
                send_audio(
                    &mut client,
                    &mut state,
                    scenario,
                    validator,
                    result,
                    *frames,
                )
                .await
                .context(label)?;
            }
            Step::ExpectAudio {
                min_frames,
                within_ms,
            } => {
                let got = pump_until(
                    &mut client,
                    &mut state,
                    scenario,
                    validator,
                    result,
                    Duration::from_millis(*within_ms),
                    PumpGoal::AudioFrames(*min_frames),
                )
                .await
                .context(label.clone())?;
                if let PumpOutcome::TimedOut { audio_seen } = got {
                    result.failures.push(format!(
                        "{label}: expected a session total of ≥{min_frames} audio frames \
                         within {within_ms} ms, session has {audio_seen}"
                    ));
                }
            }
            Step::SendEvent { json } => {
                let text = build_event(&mut state, json, /*typed=*/ true).context(label)?;
                client.send_text(text).await?;
            }
            Step::SendRaw { json } => {
                let text = build_event(&mut state, json, /*typed=*/ false).context(label)?;
                client.send_text(text).await?;
            }
            Step::ExpectCommand {
                command,
                within_ms,
                optional,
            } => {
                let got = pump_until(
                    &mut client,
                    &mut state,
                    scenario,
                    validator,
                    result,
                    Duration::from_millis(*within_ms),
                    PumpGoal::Command(command.clone()),
                )
                .await
                .context(label.clone())?;
                if matches!(got, PumpOutcome::TimedOut { .. }) && !optional {
                    result.failures.push(format!(
                        "{label}: server did not send `{command}` within {within_ms} ms"
                    ));
                }
            }
            Step::ExpectSilence { ms } => {
                let before = (state.frames_received, result.commands_received);
                pump_until(
                    &mut client,
                    &mut state,
                    scenario,
                    validator,
                    result,
                    Duration::from_millis(*ms),
                    PumpGoal::Nothing,
                )
                .await
                .context(label.clone())?;
                let after = (state.frames_received, result.commands_received);
                if after != before {
                    result.failures.push(format!(
                        "{label}: expected no traffic for {ms} ms, saw {} audio frames \
                         and {} commands",
                        after.0 - before.0,
                        after.1 - before.1
                    ));
                }
            }
            Step::Ping { within_ms } => {
                client.send_ping().await?;
                let got = pump_until(
                    &mut client,
                    &mut state,
                    scenario,
                    validator,
                    result,
                    Duration::from_millis(*within_ms),
                    PumpGoal::Pong,
                )
                .await
                .context(label.clone())?;
                if matches!(got, PumpOutcome::TimedOut { .. }) {
                    result.failures.push(format!(
                        "{label}: no pong within {within_ms} ms — the daemon's §5.6 keepalive \
                         would declare this connection half-open and abandon it"
                    ));
                }
            }
            Step::Wait { ms } => {
                pump_until(
                    &mut client,
                    &mut state,
                    scenario,
                    validator,
                    result,
                    Duration::from_millis(*ms),
                    PumpGoal::Nothing,
                )
                .await
                .context(label)?;
            }
            Step::SendStop { reason } => {
                let seq = state.next_seq();
                let msg = serde_json::json!({
                    "type": "stop",
                    "call_id": state.call_id,
                    "seq": seq,
                    "reason": reason,
                });
                // Round-trip through the real type so scenario typos fail.
                let typed: BridgeOut = serde_json::from_value(msg)
                    .with_context(|| format!("{label}: invalid stop reason `{reason}`"))?;
                client.send_text(serde_json::to_string(&typed)?).await?;
            }
            Step::Close => {
                client.close().await?;
            }
            Step::Reconnect => {
                // Abrupt drop, fresh socket, `start { reconnected: true }`,
                // seq restarting at 0 (PROTOCOL §5.7).
                let call_id = state.call_id.clone();
                client.abort();
                tokio::time::sleep(Duration::from_millis(100)).await;
                client = WsClient::connect(url).await.context("reconnect dial")?;
                state = open_session(&mut client, scenario, true).await?;
                state.call_id = call_id;
                result.notes.push("reconnected with a fresh session".into());
            }
        }
    }
    Ok(())
}

async fn open_session(
    client: &mut WsClient,
    scenario: &Scenario,
    reconnected: bool,
) -> Result<SessionState> {
    let call_id = format!("testkit-{}", scenario.name);
    let start = BridgeOut::Start(StartMsg {
        version: PROTOCOL_VERSION.to_string(),
        call_id: CallId::new(call_id.clone()),
        seq: 0,
        from: scenario.session.from.clone(),
        to: scenario.session.to.clone(),
        direction: Direction::Inbound,
        audio: AudioFormat {
            encoding: AudioEncoding::Pcm16le,
            sample_rate: scenario.session.sample_rate,
            channels: 1,
            frame_ms: FRAME_MS as u32,
        },
        sip: SipMeta {
            call_id: format!("{call_id}@testkit.invalid"),
            headers: Default::default(),
        },
        srtp: None,
        verstat: None,
        retrieved: false,
        reconnected,
        trace_context: None,
    });
    client.send_text(serde_json::to_string(&start)?).await?;
    Ok(SessionState {
        call_id,
        seq: 1,
        frame_bytes: (scenario.session.sample_rate as usize / 1000) * FRAME_MS as usize * 2,
        opened_at: Instant::now(),
        frames_sent: 0,
        frames_received: 0,
        hangup: false,
    })
}

/// Build an injected event: parse the scenario's JSON, add `call_id`/`seq`
/// (not overriding an explicit one), and — for typed events — round-trip
/// through the daemon's `BridgeOut` so scenarios can't emit garbage
/// accidentally. Raw events skip the type check by design.
fn build_event(state: &mut SessionState, json: &str, typed: bool) -> Result<String> {
    let mut value: Value = serde_json::from_str(json).context("step `json` is not valid JSON")?;
    let obj = value
        .as_object_mut()
        .context("step `json` must be a JSON object")?;
    obj.entry("call_id")
        .or_insert_with(|| Value::String(state.call_id.clone()));
    if !obj.contains_key("seq") {
        obj.insert("seq".into(), Value::from(state.next_seq()));
    }
    if typed {
        let typed_msg: BridgeOut = serde_json::from_value(value)
            .context("send_event json does not round-trip through BridgeOut — typo?")?;
        Ok(serde_json::to_string(&typed_msg)?)
    } else {
        Ok(serde_json::to_string(&value)?)
    }
}

/// Send `frames` × 20 ms tone frames, paced, validating anything that
/// arrives while we stream.
async fn send_audio(
    client: &mut WsClient,
    state: &mut SessionState,
    scenario: &Scenario,
    validator: &MessageValidator,
    result: &mut ScenarioResult,
    frames: u64,
) -> Result<()> {
    let frame = tone_frame(state.frame_bytes, scenario.session.sample_rate);
    let mut ticker = tokio::time::interval(Duration::from_millis(FRAME_MS));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let target = state.frames_sent + frames;
    while state.frames_sent < target {
        tokio::select! {
            _ = ticker.tick() => {
                client.send_binary(frame.clone()).await?;
                state.frames_sent += 1;
            }
            incoming = client.recv() => {
                handle_incoming(incoming?, state, scenario, validator, result)?;
                if state.hangup {
                    finish_after_hangup(client, state).await?;
                    return Ok(());
                }
            }
        }
    }
    Ok(())
}

enum PumpGoal {
    AudioFrames(u64),
    Command(String),
    Pong,
    Nothing,
}

enum PumpOutcome {
    Reached,
    TimedOut { audio_seen: u64 },
}

/// Receive + validate until the goal is met or the window elapses.
/// Returning `Ok` never means "no violations" — those accumulate in
/// `result.failures` as they're seen.
async fn pump_until(
    client: &mut WsClient,
    state: &mut SessionState,
    scenario: &Scenario,
    validator: &MessageValidator,
    result: &mut ScenarioResult,
    window: Duration,
    goal: PumpGoal,
) -> Result<PumpOutcome> {
    let deadline = Instant::now() + window;
    // Audio totals are cumulative for the session — echo arrives
    // concurrently while `send_audio` streams, so the goal may already
    // be met before this step starts.
    if let PumpGoal::AudioFrames(n) = &goal {
        if state.frames_received >= *n {
            return Ok(PumpOutcome::Reached);
        }
    }
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(PumpOutcome::TimedOut {
                audio_seen: state.frames_received,
            });
        }
        let incoming = match timeout(remaining, client.recv()).await {
            Err(_) => continue, // window elapsed; loop exits above
            Ok(incoming) => incoming?,
        };
        let is_pong = matches!(incoming, Incoming::Pong);
        let command = handle_incoming(incoming, state, scenario, validator, result)?;
        if state.hangup {
            finish_after_hangup(client, state).await?;
            return Ok(PumpOutcome::Reached);
        }
        let reached = match &goal {
            PumpGoal::AudioFrames(n) => state.frames_received >= *n,
            PumpGoal::Command(want) => command.as_deref() == Some(want.as_str()),
            PumpGoal::Pong => is_pong,
            PumpGoal::Nothing => false,
        };
        if reached {
            return Ok(PumpOutcome::Reached);
        }
    }
}

/// Validate one incoming frame; returns the command discriminator for
/// text frames. Violations are recorded, not returned as `Err` — an `Err`
/// here means the transport itself is unusable.
fn handle_incoming(
    incoming: Incoming,
    state: &mut SessionState,
    scenario: &Scenario,
    validator: &MessageValidator,
    result: &mut ScenarioResult,
) -> Result<Option<String>> {
    match incoming {
        Incoming::Pong => Ok(None),
        Incoming::Closed(frame) => {
            bail!(
                "server closed the connection mid-scenario ({}) — a server ends a call \
                 with `hangup`, not a bare close (PROTOCOL.md §5.7)",
                match frame {
                    Some((code, reason)) if !reason.is_empty() =>
                        format!("code {code}, reason `{reason}`"),
                    Some((code, _)) => format!("code {code}"),
                    None => "no close frame".to_string(),
                }
            );
        }
        Incoming::Binary(frame) => {
            state.frames_received += 1;
            result.audio_frames_received += 1;
            if frame.len() != state.frame_bytes {
                record_once(
                    &mut result.failures,
                    format!(
                        "audio frame of {} bytes — every binary frame must be exactly one \
                         20 ms chunk ({} bytes at {} Hz)",
                        frame.len(),
                        state.frame_bytes,
                        scenario.session.sample_rate
                    ),
                );
            }
            // Pacing: a server may echo at our cadence or generate at real
            // time, but must never flood. Bound = the larger of (frames we
            // sent) and (real-time since open), plus configured slack.
            let elapsed_frames = state.opened_at.elapsed().as_millis() as u64 / FRAME_MS;
            let budget =
                state.frames_sent.max(elapsed_frames) + scenario.session.pacing_slack_frames;
            if state.frames_received > budget {
                record_once(
                    &mut result.failures,
                    format!(
                        "audio arriving faster than real time: {} frames received vs a budget \
                         of {} (sent {}, elapsed ≈{} frames, slack {}) — outbound audio must \
                         be paced (PROTOCOL.md §2.2)",
                        state.frames_received,
                        budget,
                        state.frames_sent,
                        elapsed_frames,
                        scenario.session.pacing_slack_frames
                    ),
                );
            }
            Ok(None)
        }
        Incoming::Text(text) => {
            result.commands_received += 1;
            match validator.check(&text) {
                Err(violation) => {
                    result.failures.push(violation);
                    Ok(None)
                }
                Ok(command) => {
                    let name = command_name(&command);
                    if let BridgeIn::Hangup { .. } = command {
                        state.hangup = true;
                    }
                    Ok(Some(name.to_string()))
                }
            }
        }
    }
}

/// The daemon's reaction to a server `hangup`: `stop { server_hangup }`,
/// then a clean close.
async fn finish_after_hangup(client: &mut WsClient, state: &mut SessionState) -> Result<()> {
    let seq = state.next_seq();
    let stop = serde_json::json!({
        "type": "stop",
        "call_id": state.call_id,
        "seq": seq,
        "reason": "server_hangup",
    });
    client.send_text(stop.to_string()).await?;
    client.close().await
}

/// Repeating a per-frame violation 200 times drowns the report; once per
/// distinct message is enough.
fn record_once(failures: &mut Vec<String>, message: String) {
    if !failures.contains(&message) {
        failures.push(message);
    }
}

fn command_name(command: &BridgeIn) -> &'static str {
    match command {
        BridgeIn::Clear { .. } => "clear",
        BridgeIn::Mark { .. } => "mark",
        BridgeIn::Hangup { .. } => "hangup",
        BridgeIn::Transfer { .. } => "transfer",
        BridgeIn::SendDtmf { .. } => "send_dtmf",
        BridgeIn::Mute { .. } => "mute",
        BridgeIn::Unmute { .. } => "unmute",
        BridgeIn::StartRecording { .. } => "start_recording",
        BridgeIn::StopRecording { .. } => "stop_recording",
        BridgeIn::PauseRecording { .. } => "pause_recording",
        BridgeIn::ResumeRecording { .. } => "resume_recording",
        BridgeIn::SetRecordingConsent { .. } => "set_recording_consent",
        BridgeIn::Park { .. } => "park",
        BridgeIn::ConferenceJoin { .. } => "conference_join",
        BridgeIn::ConferenceLeave { .. } => "conference_leave",
        BridgeIn::Hold { .. } => "hold",
        BridgeIn::Resume { .. } => "resume",
    }
}

fn step_name(step: &Step) -> &'static str {
    match step {
        Step::SendAudio { .. } => "send_audio",
        Step::ExpectAudio { .. } => "expect_audio",
        Step::SendEvent { .. } => "send_event",
        Step::SendRaw { .. } => "send_raw",
        Step::ExpectCommand { .. } => "expect_command",
        Step::ExpectSilence { .. } => "expect_silence",
        Step::Ping { .. } => "ping",
        Step::Wait { .. } => "wait",
        Step::SendStop { .. } => "send_stop",
        Step::Close => "close",
        Step::Reconnect => "reconnect",
    }
}

/// One 20 ms PCM16-LE frame of a 1 kHz tone at modest volume — audibly
/// non-silence if a human ever points a scenario at a real bot.
fn tone_frame(frame_bytes: usize, sample_rate: u32) -> Vec<u8> {
    let samples = frame_bytes / 2;
    let mut out = Vec::with_capacity(frame_bytes);
    for n in 0..samples {
        let t = n as f32 / sample_rate as f32;
        let s = (2.0 * std::f32::consts::PI * 1000.0 * t).sin();
        let v = (s * 8000.0) as i16;
        out.extend_from_slice(&v.to_le_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scenario::Session;

    fn state() -> SessionState {
        SessionState {
            call_id: "testkit-t".into(),
            seq: 1,
            frame_bytes: 320,
            opened_at: Instant::now(),
            frames_sent: 0,
            frames_received: 0,
            hangup: false,
        }
    }

    #[test]
    fn build_event_injects_envelope_and_type_checks() {
        let mut s = state();
        let text = build_event(
            &mut s,
            r#"{"type":"dtmf","digit":"5","duration_ms":160,"method":"rfc2833"}"#,
            true,
        )
        .unwrap();
        let v: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["call_id"], "testkit-t");
        assert_eq!(v["seq"], 1);
        assert_eq!(s.seq, 2);
    }

    #[test]
    fn build_event_rejects_typos_for_typed_sends() {
        let mut s = state();
        let err = build_event(&mut s, r#"{"type":"dtmpf","digit":"5"}"#, true).unwrap_err();
        assert!(format!("{err:#}").contains("BridgeOut"));
        // ...but raw sends are allowed to be anything (that's their job).
        build_event(&mut s, r#"{"type":"dtmpf","digit":"5"}"#, false).unwrap();
    }

    #[test]
    fn tone_frame_is_exactly_one_frame() {
        assert_eq!(tone_frame(320, 8000).len(), 320);
        assert_eq!(tone_frame(640, 16000).len(), 640);
    }

    #[test]
    fn violations_are_deduplicated() {
        let mut failures = Vec::new();
        record_once(&mut failures, "same".into());
        record_once(&mut failures, "same".into());
        record_once(&mut failures, "other".into());
        assert_eq!(failures.len(), 2);
    }

    #[test]
    fn session_defaults_are_sane() {
        let s = Session::default();
        assert_eq!(s.sample_rate, 8000);
        assert_eq!(s.pacing_slack_frames, 25);
    }
}
