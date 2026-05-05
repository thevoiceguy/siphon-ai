//! End-to-end load + compile tests for representative TOML.
//!
//! These exercise the full `load_from_str_with_env` pipeline:
//! env expansion → TOML parse → compile → final `Config`. Unit
//! tests for each stage live in the source modules; this is the
//! "did the contract survive a real-world fixture" check.

use std::collections::HashMap;
use std::time::Duration;

use siphon_ai_config::{load_from_str_with_env, EnvSource, LoadError, SipTransport};
use siphon_ai_media_glue::Codec;

/// Test env source. The crate's `EnvSource` trait has a default
/// `ProcessEnv` impl; we use a map here so tests stay deterministic.
struct MapEnv(HashMap<String, String>);
impl MapEnv {
    fn new<I: IntoIterator<Item = (&'static str, &'static str)>>(items: I) -> Self {
        Self(
            items
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        )
    }
}
impl EnvSource for MapEnv {
    fn lookup(&self, name: &str) -> Option<String> {
        self.0.get(name).cloned()
    }
}

const REPRESENTATIVE_TOML: &str = r#"
[node]
id = "siphon-ai-test"
public_address = "203.0.113.10"

[sip]
listen = "0.0.0.0:5060"
transports = ["udp", "tcp"]
user_agent = "SiphonAI/0.1.0-test"

[media]
codecs = ["pcmu", "pcma", "opus"]
dtmf = "rfc2833"
rtp_port_range = [16384, 32768]

[bridge]
ws_url = "wss://default.example.com/sip-bridge"
ws_auth_header = "Bearer ${BRIDGE_TOKEN}"
ws_connect_timeout_ms = 3000
audio_sample_rate = 16000
forward_headers = ["User-Agent", "X-Tenant-Id"]

[[route]]
name = "main_reception"
[route.match]
request_uri_user = "5000"
[route.bridge]
ws_url = "wss://reception.example.com/sip-bridge"

[[route]]
name = "default"
[route.match]
any = true
"#;

#[test]
fn representative_config_compiles_into_consumable_pieces() {
    let env = MapEnv::new([("BRIDGE_TOKEN", "abc-secret-123")]);
    let cfg = load_from_str_with_env(REPRESENTATIVE_TOML, &env).expect("config compiles");

    // [node]
    assert_eq!(cfg.node.id, "siphon-ai-test");
    assert_eq!(cfg.node.public_address, "203.0.113.10");

    // [sip]
    assert_eq!(cfg.sip.listen_addr.port(), 5060);
    assert_eq!(
        cfg.sip.transports,
        vec![SipTransport::Udp, SipTransport::Tcp]
    );
    assert_eq!(cfg.sip.user_agent.as_deref(), Some("SiphonAI/0.1.0-test"));

    // [media]
    assert_eq!(cfg.media.rtp_port_range, Some((16384, 32768)));

    // [bridge] → BridgeDefaults
    assert_eq!(
        cfg.bridge_defaults.ws_url.as_deref(),
        Some("wss://default.example.com/sip-bridge"),
    );
    // ws_auth_header arrived as "Bearer abc-secret-123" after env
    // expansion; the Bearer prefix is stripped on the way in.
    assert_eq!(
        cfg.bridge_defaults.auth_bearer.as_deref(),
        Some("abc-secret-123")
    );
    assert_eq!(
        cfg.bridge_defaults.connect_timeout,
        Duration::from_millis(3000)
    );
    assert_eq!(
        cfg.bridge_defaults.codecs,
        vec![Codec::Pcmu, Codec::Pcma, Codec::Opus],
    );
    assert_eq!(cfg.bridge_defaults.dtmf_payload_type, Some(101));
    assert_eq!(
        cfg.bridge_defaults.forward_headers,
        vec!["User-Agent".to_string(), "X-Tenant-Id".to_string()],
    );

    // [[route]] — handed off to siphon-ai-routes; just confirm both
    // routes survived and the trailing default is present.
    assert_eq!(cfg.routes.len(), 2);
    assert!(cfg.routes.has_default());
}

#[test]
fn missing_env_var_without_default_fails_load() {
    let env = MapEnv::new([]); // no BRIDGE_TOKEN
    let err = load_from_str_with_env(REPRESENTATIVE_TOML, &env).unwrap_err();
    assert!(matches!(err, LoadError::Env(_)));
}

#[test]
fn env_default_used_when_var_absent() {
    let env = MapEnv::new([]);
    let toml = r#"
[sip]
listen = "0.0.0.0:${SIP_PORT:-5070}"

[bridge]
ws_url = "wss://${HOST:-fallback.example.com}/ws"
"#;
    let cfg = load_from_str_with_env(toml, &env).expect("config compiles with defaults");
    assert_eq!(cfg.sip.listen_addr.port(), 5070);
    assert_eq!(
        cfg.bridge_defaults.ws_url.as_deref(),
        Some("wss://fallback.example.com/ws"),
    );
}

#[test]
fn public_address_falls_back_to_listen_ip_when_unset() {
    let env = MapEnv::new([]);
    let toml = r#"
[sip]
listen = "127.0.0.1:5060"

[bridge]
ws_url = "wss://x/y"

[[route]]
name = "default"
[route.match]
any = true
"#;
    let cfg = load_from_str_with_env(toml, &env).unwrap();
    assert_eq!(cfg.node.public_address, "127.0.0.1");
}

#[test]
fn unknown_codec_is_a_compile_error() {
    let env = MapEnv::new([]);
    let toml = r#"
[sip]
listen = "0.0.0.0:5060"

[media]
codecs = ["pcmu", "g729"]

[bridge]
ws_url = "wss://x/y"

[[route]]
name = "default"
[route.match]
any = true
"#;
    let err = load_from_str_with_env(toml, &env).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("g729"), "got: {msg}");
}

#[test]
fn bad_listen_addr_is_a_compile_error() {
    let env = MapEnv::new([]);
    let toml = r#"
[sip]
listen = "not-a-socket"

[bridge]
ws_url = "wss://x/y"
"#;
    let err = load_from_str_with_env(toml, &env).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("not-a-socket") || msg.contains("invalid"),
        "got: {msg}",
    );
}

#[test]
fn unknown_transport_is_a_compile_error() {
    let env = MapEnv::new([]);
    let toml = r#"
[sip]
listen = "0.0.0.0:5060"
transports = ["udp", "smoke"]

[bridge]
ws_url = "wss://x/y"
"#;
    let err = load_from_str_with_env(toml, &env).unwrap_err();
    assert!(err.to_string().contains("smoke"));
}

#[test]
fn audio_sample_rate_must_be_8k_or_16k() {
    let env = MapEnv::new([]);
    let toml = r#"
[sip]
listen = "0.0.0.0:5060"

[bridge]
ws_url = "wss://x/y"
audio_sample_rate = 44100
"#;
    let err = load_from_str_with_env(toml, &env).unwrap_err();
    assert!(err.to_string().contains("44100"));
}

#[test]
fn dtmf_off_disables_payload_type() {
    let env = MapEnv::new([]);
    let toml = r#"
[sip]
listen = "0.0.0.0:5060"

[media]
dtmf = "off"

[bridge]
ws_url = "wss://x/y"

[[route]]
name = "d"
[route.match]
any = true
"#;
    let cfg = load_from_str_with_env(toml, &env).unwrap();
    assert_eq!(cfg.bridge_defaults.dtmf_payload_type, None);
}

#[test]
fn unknown_dtmf_mode_errors() {
    let env = MapEnv::new([]);
    let toml = r#"
[sip]
listen = "0.0.0.0:5060"

[media]
dtmf = "morse"

[bridge]
ws_url = "wss://x/y"
"#;
    let err = load_from_str_with_env(toml, &env).unwrap_err();
    assert!(err.to_string().contains("morse"));
}

#[test]
fn unknown_top_level_section_is_tolerated() {
    // [hep], [cdr], [webhooks] etc. aren't implemented yet — config
    // accepts and ignores them so a real-world TOML file stays valid
    // as follow-ups land them.
    let env = MapEnv::new([]);
    let toml = r#"
[sip]
listen = "0.0.0.0:5060"

[bridge]
ws_url = "wss://x/y"

[hep]
enabled = false
collector = "homer.example.com:9060"

[cdr]
enabled = true

[[route]]
name = "d"
[route.match]
any = true
"#;
    let cfg = load_from_str_with_env(toml, &env).unwrap();
    assert!(cfg.routes.has_default());
}

#[test]
fn unknown_field_in_known_section_errors() {
    // We're strict on typos within known sections — `auido` should
    // reject, not silently miss.
    let env = MapEnv::new([]);
    let toml = r#"
[sip]
listen = "0.0.0.0:5060"

[bridge]
ws_url = "wss://x/y"
auido_sample_rate = 8000
"#;
    let err = load_from_str_with_env(toml, &env).unwrap_err();
    assert!(matches!(err, LoadError::Toml(_)), "got: {err:?}");
}

#[test]
fn no_default_route_loads_but_warns() {
    // "No default route" is recommended-against, not forbidden —
    // CLAUDE.md §4.6 says log a warning. We can't easily assert on
    // the warning content here, but loading must still succeed.
    let env = MapEnv::new([]);
    let toml = r#"
[sip]
listen = "0.0.0.0:5060"

[bridge]
ws_url = "wss://x/y"

[[route]]
name = "specific"
[route.match]
request_uri_user = "5000"
"#;
    let cfg = load_from_str_with_env(toml, &env).unwrap();
    assert_eq!(cfg.routes.len(), 1);
    assert!(!cfg.routes.has_default());
}

#[test]
fn duplicate_codecs_are_deduplicated() {
    // A user copy-pasting "pcmu, pcmu" shouldn't end up with
    // duplicate entries in the negotiated caps.
    let env = MapEnv::new([]);
    let toml = r#"
[sip]
listen = "0.0.0.0:5060"

[media]
codecs = ["pcmu", "pcma", "pcmu"]

[bridge]
ws_url = "wss://x/y"

[[route]]
name = "d"
[route.match]
any = true
"#;
    let cfg = load_from_str_with_env(toml, &env).unwrap();
    assert_eq!(cfg.bridge_defaults.codecs, vec![Codec::Pcmu, Codec::Pcma]);
}

#[test]
fn defaults_kick_in_when_optional_blocks_omitted() {
    // Smallest valid config: just sip + bridge. Everything else
    // gets a sensible default.
    let env = MapEnv::new([]);
    let toml = r#"
[sip]
listen = "0.0.0.0:5060"

[bridge]
ws_url = "wss://x/y"
"#;
    let cfg = load_from_str_with_env(toml, &env).unwrap();
    assert_eq!(cfg.node.id, "siphon-ai");
    assert_eq!(cfg.sip.transports, vec![SipTransport::Udp]);
    assert_eq!(cfg.bridge_defaults.codecs, vec![Codec::Pcmu, Codec::Pcma]);
    assert_eq!(cfg.bridge_defaults.dtmf_payload_type, Some(101));
    assert_eq!(cfg.bridge_defaults.connect_timeout, Duration::from_secs(5));
    assert!(cfg.bridge_defaults.forward_headers.is_empty());
}
