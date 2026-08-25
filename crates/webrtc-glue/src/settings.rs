//! `[webrtc]` config → forge-webrtc's `PeerConfig`.
//!
//! Keeps the mapping in one place so the daemon's config layer never
//! imports forge-webrtc types directly, and so the codec-preference
//! decision has a single documented home.

use std::time::Duration;

use forge_core::AudioCodec;
use forge_webrtc::{PeerConfig, TurnServer};
use thiserror::Error;

/// A TURN server as the operator writes it in TOML.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnCredentials {
    /// `turn:host:port` (or `turns:`).
    pub uri: String,
    pub username: String,
    pub password: String,
}

/// The compiled `[webrtc]` block.
#[derive(Debug, Clone)]
pub struct WebRtcSettings {
    /// STUN URIs (`stun:host:port`). Empty is fine when the daemon's
    /// own address is already reachable by the browser — host
    /// candidates then suffice, which is the common single-node case.
    pub stun_servers: Vec<String>,
    /// TURN relays for browsers behind NATs that cannot be punched.
    /// The operator's coturn; siphon-ai only plumbs credentials.
    pub turn_servers: Vec<TurnCredentials>,
    /// Answer→DTLS-complete budget (`setup_timeout_secs`). A browser
    /// that signals but never finishes media must not hold a slot.
    pub setup_timeout: Duration,
    /// Prefer G.711 over Opus on the browser leg when the peer offers
    /// it. Browsers must implement G.711 (RFC 7874 §3), so this is
    /// always available; choosing it makes the bridge's transcode
    /// collapse into the same path a classic 8 kHz leg already uses
    /// (forge-media #130). Off by default — Opus at 48 kHz is the
    /// better input to an ASR pipeline, which is the plan's §1
    /// decision.
    pub prefer_g711: bool,
}

impl Default for WebRtcSettings {
    fn default() -> Self {
        Self {
            stun_servers: Vec::new(),
            turn_servers: Vec::new(),
            setup_timeout: Duration::from_secs(15),
            prefer_g711: false,
        }
    }
}

#[derive(Debug, Error)]
pub enum SettingsError {
    #[error("[webrtc].turn_servers[{index}].uri {uri:?} must start with turn: or turns:")]
    BadTurnUri { index: usize, uri: String },
    #[error("[webrtc].turn_servers[{index}] is missing a username or password")]
    IncompleteTurnCredentials { index: usize },
    #[error("[webrtc].stun_servers[{index}] {uri:?} must start with stun: or stuns:")]
    BadStunUri { index: usize, uri: String },
}

impl WebRtcSettings {
    /// Validate what only this crate can judge — URI schemes and
    /// credential completeness — so a typo fails at config load rather
    /// than at the first browser call (CLAUDE.md §4.6).
    pub fn validate(&self) -> Result<(), SettingsError> {
        for (index, uri) in self.stun_servers.iter().enumerate() {
            if !(uri.starts_with("stun:") || uri.starts_with("stuns:")) {
                return Err(SettingsError::BadStunUri {
                    index,
                    uri: uri.clone(),
                });
            }
        }
        for (index, turn) in self.turn_servers.iter().enumerate() {
            if !(turn.uri.starts_with("turn:") || turn.uri.starts_with("turns:")) {
                return Err(SettingsError::BadTurnUri {
                    index,
                    uri: turn.uri.clone(),
                });
            }
            if turn.username.is_empty() || turn.password.is_empty() {
                return Err(SettingsError::IncompleteTurnCredentials { index });
            }
        }
        Ok(())
    }

    /// Build the forge-webrtc configuration for one peer connection.
    ///
    /// Codec order is the whole of the transcode decision: the first
    /// preference the browser also offered is what gets negotiated
    /// (forge-media #130). Opus first by default (§1); `prefer_g711`
    /// flips it for a deployment whose SIP side is 8 kHz anyway.
    pub fn peer_config(&self) -> PeerConfig {
        let codecs = if self.prefer_g711 {
            vec![
                (AudioCodec::PCMU, 0),
                (AudioCodec::PCMA, 8),
                (AudioCodec::Opus, 111),
            ]
        } else {
            vec![
                (AudioCodec::Opus, 111),
                (AudioCodec::PCMU, 0),
                (AudioCodec::PCMA, 8),
            ]
        };
        PeerConfig {
            stun_servers: self.stun_servers.clone(),
            codecs,
            ..PeerConfig::default()
        }
    }

    /// The TURN servers in forge-ice's shape.
    pub fn turn(&self) -> Vec<TurnServer> {
        self.turn_servers
            .iter()
            .map(|t| TurnServer {
                uri: t.uri.clone(),
                username: t.username.clone(),
                password: t.password.clone(),
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_prefers_opus_then_g711() {
        let cfg = WebRtcSettings::default().peer_config();
        assert_eq!(cfg.codecs[0], (AudioCodec::Opus, 111));
        assert_eq!(cfg.codecs[1], (AudioCodec::PCMU, 0));
    }

    #[test]
    fn prefer_g711_puts_pcmu_first() {
        let s = WebRtcSettings {
            prefer_g711: true,
            ..Default::default()
        };
        let cfg = s.peer_config();
        assert_eq!(cfg.codecs[0], (AudioCodec::PCMU, 0));
        assert!(cfg.codecs.iter().any(|(c, _)| *c == AudioCodec::Opus));
    }

    #[test]
    fn stun_and_turn_uris_are_validated() {
        let bad_stun = WebRtcSettings {
            stun_servers: vec!["stun.l.google.com:19302".into()],
            ..Default::default()
        };
        assert!(matches!(
            bad_stun.validate(),
            Err(SettingsError::BadStunUri { index: 0, .. })
        ));

        let bad_turn = WebRtcSettings {
            turn_servers: vec![TurnCredentials {
                uri: "http://relay.example".into(),
                username: "u".into(),
                password: "p".into(),
            }],
            ..Default::default()
        };
        assert!(matches!(
            bad_turn.validate(),
            Err(SettingsError::BadTurnUri { index: 0, .. })
        ));

        let no_creds = WebRtcSettings {
            turn_servers: vec![TurnCredentials {
                uri: "turn:relay.example:3478".into(),
                username: String::new(),
                password: "p".into(),
            }],
            ..Default::default()
        };
        assert!(matches!(
            no_creds.validate(),
            Err(SettingsError::IncompleteTurnCredentials { index: 0 })
        ));
    }

    #[test]
    fn valid_settings_pass_and_map_through() {
        let s = WebRtcSettings {
            stun_servers: vec!["stun:stun.l.google.com:19302".into()],
            turn_servers: vec![TurnCredentials {
                uri: "turn:relay.example:3478".into(),
                username: "u".into(),
                password: "p".into(),
            }],
            ..Default::default()
        };
        s.validate().expect("valid");
        assert_eq!(s.peer_config().stun_servers.len(), 1);
        assert_eq!(s.turn().len(), 1);
        assert_eq!(s.setup_timeout, Duration::from_secs(15));
    }
}
