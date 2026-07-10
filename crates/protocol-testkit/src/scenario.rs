//! Scenario files: a scripted call, declaratively.
//!
//! Bundled scenarios live in `crates/protocol-testkit/scenarios/*.toml`
//! and are embedded in the binary; `--scenario-dir` loads additional
//! ones from disk. See `docs/CONFORMANCE.md` for the file format.

use anyhow::{bail, Context, Result};
use serde::Deserialize;

/// The five bundled scenarios, embedded so the shipped binary is
/// self-contained (DESIGN_PROTOCOL_SDKS §4).
pub const BUNDLED: &[(&str, &str)] = &[
    ("basic-echo", include_str!("../scenarios/basic-echo.toml")),
    ("dtmf", include_str!("../scenarios/dtmf.toml")),
    (
        "recording-controls",
        include_str!("../scenarios/recording-controls.toml"),
    ),
    (
        "hangup-semantics",
        include_str!("../scenarios/hangup-semantics.toml"),
    ),
    ("keepalive", include_str!("../scenarios/keepalive.toml")),
];

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scenario {
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub session: Session,
    pub steps: Vec<Step>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Session {
    /// `8000` or `16000` — sets the exact binary frame size asserted on
    /// every audio frame the server sends.
    #[serde(default = "default_sample_rate")]
    pub sample_rate: u32,
    #[serde(default = "default_from")]
    pub from: String,
    #[serde(default = "default_to")]
    pub to: String,
    /// Extra frames of audio the server may send beyond real-time pacing
    /// before it's a violation (500 ms of slack by default). Servers must
    /// pace generated audio (PROTOCOL.md §2.2); echo servers mirror our
    /// own cadence and never come near the bound.
    #[serde(default = "default_pacing_slack")]
    pub pacing_slack_frames: u64,
}

impl Default for Session {
    fn default() -> Self {
        Self {
            sample_rate: default_sample_rate(),
            from: default_from(),
            to: default_to(),
            pacing_slack_frames: default_pacing_slack(),
        }
    }
}

fn default_sample_rate() -> u32 {
    8000
}
fn default_from() -> String {
    "+13125551212".into()
}
fn default_to() -> String {
    "5000".into()
}
fn default_pacing_slack() -> u64 {
    25
}

#[derive(Debug, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum Step {
    /// Stream `frames` × 20 ms of paced audio (1 kHz tone), like the
    /// daemon's RTP→WS path.
    SendAudio { frames: u64 },
    /// Expect the session's CUMULATIVE audio-frame total to reach
    /// `min_frames` within `within_ms`. Cumulative because echo arrives
    /// concurrently while `send_audio` streams; the total resets on
    /// `reconnect` (new session).
    ExpectAudio { min_frames: u64, within_ms: u64 },
    /// Inject a daemon event. `json` is the message body WITHOUT
    /// `call_id`/`seq` (the runner owns those) and must round-trip
    /// through the daemon's real `BridgeOut` type — a typo'd scenario
    /// fails loudly, not silently.
    SendEvent { json: String },
    /// Send a text frame verbatim — for unknown-message-tolerance probes.
    /// `call_id`/`seq` are still injected if absent.
    SendRaw { json: String },
    /// Expect the server to send a specific command within `within_ms`.
    /// With `optional = true`, absence is not a failure (presence is
    /// still validated).
    ExpectCommand {
        #[serde(rename = "type")]
        command: String,
        within_ms: u64,
        #[serde(default)]
        optional: bool,
    },
    /// Expect NO text/binary traffic from the server for `ms`
    /// (pongs excluded).
    ExpectSilence { ms: u64 },
    /// WS ping; expect a pong within `within_ms` (PROTOCOL.md §5.6:
    /// "Servers MAY ping SiphonAI; SiphonAI always pongs" — and the
    /// reverse must hold for the daemon's keepalive to work).
    Ping { within_ms: u64 },
    /// Idle for `ms`, still receiving + validating whatever arrives.
    Wait { ms: u64 },
    /// Send `stop { reason }` — the daemon's last message on a session.
    SendStop { reason: String },
    /// Clean close (1000) and wait for the server's close reply.
    Close,
    /// Drop the socket abruptly (no close handshake), then open a fresh
    /// connection and send `start` with `reconnected: true` and `seq`
    /// restarting at 0 — exactly what a daemon-side WS reconnect looks
    /// like (PROTOCOL.md §5.7). The server must accept the session.
    Reconnect,
}

pub fn parse(name_hint: &str, text: &str) -> Result<Scenario> {
    let scenario: Scenario =
        toml::from_str(text).with_context(|| format!("scenario `{name_hint}` failed to parse"))?;
    if scenario.steps.is_empty() {
        bail!("scenario `{}` has no steps", scenario.name);
    }
    if !matches!(scenario.session.sample_rate, 8000 | 16000) {
        bail!(
            "scenario `{}`: sample_rate must be 8000 or 16000",
            scenario.name
        );
    }
    Ok(scenario)
}

/// All bundled scenarios, parsed (a broken bundled file is a build bug —
/// covered by the unit test below).
pub fn bundled() -> Result<Vec<Scenario>> {
    BUNDLED
        .iter()
        .map(|(name, text)| parse(name, text))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_bundled_scenarios_parse() {
        let scenarios = bundled().expect("bundled scenarios parse");
        assert_eq!(scenarios.len(), BUNDLED.len());
        for ((file_name, _), scenario) in BUNDLED.iter().zip(&scenarios) {
            assert_eq!(
                &scenario.name, file_name,
                "scenario `name` must match its file name"
            );
        }
    }

    #[test]
    fn unknown_step_action_rejected() {
        let err = parse(
            "t",
            r#"
name = "t"
description = "x"
[[steps]]
action = "explode"
"#,
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("failed to parse"));
    }

    #[test]
    fn bad_sample_rate_rejected() {
        assert!(parse(
            "t",
            r#"
name = "t"
description = "x"
[session]
sample_rate = 44100
[[steps]]
action = "close"
"#,
        )
        .is_err());
    }
}
