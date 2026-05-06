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
fn cdr_disabled_by_default() {
    let env = MapEnv::new([]);
    let toml = r#"
[sip]
listen = "0.0.0.0:5060"

[bridge]
ws_url = "wss://x/y"

[[route]]
name = "d"
[route.match]
any = true
"#;
    let cfg = load_from_str_with_env(toml, &env).unwrap();
    assert!(!cfg.cdr.enabled);
    assert!(cfg.cdr.file.is_none());
    assert!(cfg.cdr.webhook.is_none());
}

#[test]
fn cdr_file_sink_compiles_with_path() {
    let env = MapEnv::new([]);
    let toml = r#"
[sip]
listen = "0.0.0.0:5060"

[bridge]
ws_url = "wss://x/y"

[cdr]
enabled = true
[cdr.file]
enabled = true
path = "/var/log/siphon-ai/cdr.jsonl"

[[route]]
name = "d"
[route.match]
any = true
"#;
    let cfg = load_from_str_with_env(toml, &env).unwrap();
    assert!(cfg.cdr.enabled);
    let file = cfg.cdr.file.expect("file sink configured");
    assert_eq!(file.path.to_str(), Some("/var/log/siphon-ai/cdr.jsonl"));
    assert!(cfg.cdr.webhook.is_none());
}

#[test]
fn cdr_webhook_sink_compiles_with_url_and_auth() {
    let env = MapEnv::new([("CDR_TOKEN", "tok-123")]);
    let toml = r#"
[sip]
listen = "0.0.0.0:5060"

[bridge]
ws_url = "wss://x/y"

[cdr]
enabled = true
[cdr.webhook]
enabled = true
url = "https://billing.example.com/cdr"
auth_header = "Bearer ${CDR_TOKEN}"
retry_max = 5
timeout_ms = 7500

[[route]]
name = "d"
[route.match]
any = true
"#;
    let cfg = load_from_str_with_env(toml, &env).unwrap();
    let w = cfg.cdr.webhook.expect("webhook configured");
    assert_eq!(w.url, "https://billing.example.com/cdr");
    assert_eq!(w.auth_header.as_deref(), Some("Bearer tok-123"));
    assert_eq!(w.retry_max, 5);
    assert_eq!(w.timeout, std::time::Duration::from_millis(7500));
}

#[test]
fn cdr_file_enabled_without_path_errors() {
    let env = MapEnv::new([]);
    let toml = r#"
[sip]
listen = "0.0.0.0:5060"

[bridge]
ws_url = "wss://x/y"

[cdr]
enabled = true
[cdr.file]
enabled = true

[[route]]
name = "d"
[route.match]
any = true
"#;
    let err = load_from_str_with_env(toml, &env).unwrap_err();
    assert!(err.to_string().contains("path is required"));
}

#[test]
fn cdr_webhook_enabled_without_url_errors() {
    let env = MapEnv::new([]);
    let toml = r#"
[sip]
listen = "0.0.0.0:5060"

[bridge]
ws_url = "wss://x/y"

[cdr]
enabled = true
[cdr.webhook]
enabled = true

[[route]]
name = "d"
[route.match]
any = true
"#;
    let err = load_from_str_with_env(toml, &env).unwrap_err();
    assert!(err.to_string().contains("url is required"));
}

#[test]
fn cdr_disabled_overrides_sub_block_misconfig() {
    // Master switch off → sub-block misconfig is tolerated. This
    // is intentional so operators can flip [cdr].enabled = false to
    // silence a flaky CDR pipeline mid-investigation without
    // editing all the sub-blocks.
    let env = MapEnv::new([]);
    let toml = r#"
[sip]
listen = "0.0.0.0:5060"

[bridge]
ws_url = "wss://x/y"

[cdr]
enabled = false
[cdr.file]
enabled = true
# path missing — would error if [cdr].enabled = true

[[route]]
name = "d"
[route.match]
any = true
"#;
    let cfg = load_from_str_with_env(toml, &env).unwrap();
    assert!(!cfg.cdr.enabled);
    assert!(cfg.cdr.file.is_none());
}

#[test]
fn sip_tls_disabled_by_default() {
    let env = MapEnv::new([]);
    let toml = r#"
[sip]
listen = "0.0.0.0:5060"

[bridge]
ws_url = "wss://x/y"

[[route]]
name = "d"
[route.match]
any = true
"#;
    let cfg = load_from_str_with_env(toml, &env).unwrap();
    assert!(cfg.sip.tls.is_none());
}

#[test]
fn sip_tls_compiles_with_cert_key_and_default_listen() {
    // `transports = ["udp", "tls"]` + `[sip.tls]` → tls config
    // with default :5061 (SIPS standard) bound to the same host.
    let env = MapEnv::new([]);
    let toml = r#"
[sip]
listen = "10.0.0.5:5060"
transports = ["udp", "tls"]

[sip.tls]
cert = "/etc/siphon-ai/tls/cert.pem"
key  = "/etc/siphon-ai/tls/key.pem"

[bridge]
ws_url = "wss://x/y"

[[route]]
name = "d"
[route.match]
any = true
"#;
    let cfg = load_from_str_with_env(toml, &env).unwrap();
    let tls = cfg.sip.tls.as_ref().expect("tls configured");
    assert_eq!(tls.listen_addr.ip().to_string(), "10.0.0.5");
    assert_eq!(tls.listen_addr.port(), 5061);
    assert_eq!(tls.cert_path.to_str(), Some("/etc/siphon-ai/tls/cert.pem"));
    assert_eq!(tls.key_path.to_str(), Some("/etc/siphon-ai/tls/key.pem"));
}

#[test]
fn sip_tls_explicit_listen_overrides_default() {
    let env = MapEnv::new([]);
    let toml = r#"
[sip]
listen = "0.0.0.0:5060"
transports = ["tls"]

[sip.tls]
listen = "0.0.0.0:5443"
cert   = "/c"
key    = "/k"

[bridge]
ws_url = "wss://x/y"

[[route]]
name = "d"
[route.match]
any = true
"#;
    let cfg = load_from_str_with_env(toml, &env).unwrap();
    let tls = cfg.sip.tls.as_ref().expect("tls configured");
    assert_eq!(tls.listen_addr.port(), 5443);
}

#[test]
fn sip_tls_missing_cert_errors() {
    let env = MapEnv::new([]);
    let toml = r#"
[sip]
listen = "0.0.0.0:5060"
transports = ["tls"]

[sip.tls]
key = "/k"

[bridge]
ws_url = "wss://x/y"

[[route]]
name = "d"
[route.match]
any = true
"#;
    let err = load_from_str_with_env(toml, &env).unwrap_err();
    assert!(err.to_string().contains("cert is required"));
}

#[test]
fn sip_tls_missing_key_errors() {
    let env = MapEnv::new([]);
    let toml = r#"
[sip]
listen = "0.0.0.0:5060"
transports = ["tls"]

[sip.tls]
cert = "/c"

[bridge]
ws_url = "wss://x/y"

[[route]]
name = "d"
[route.match]
any = true
"#;
    let err = load_from_str_with_env(toml, &env).unwrap_err();
    assert!(err.to_string().contains("key is required"));
}

#[test]
fn sip_tls_block_without_tls_transport_errors() {
    // Loud failure on the "I configured cert/key but forgot to put
    // tls in transports" footgun.
    let env = MapEnv::new([]);
    let toml = r#"
[sip]
listen = "0.0.0.0:5060"
transports = ["udp"]

[sip.tls]
cert = "/c"
key = "/k"

[bridge]
ws_url = "wss://x/y"

[[route]]
name = "d"
[route.match]
any = true
"#;
    let err = load_from_str_with_env(toml, &env).unwrap_err();
    assert!(err.to_string().contains("transports does not include"));
}

#[test]
fn sip_tls_bad_listen_addr_errors() {
    let env = MapEnv::new([]);
    let toml = r#"
[sip]
listen = "0.0.0.0:5060"
transports = ["tls"]

[sip.tls]
listen = "not-a-socket"
cert   = "/c"
key    = "/k"

[bridge]
ws_url = "wss://x/y"

[[route]]
name = "d"
[route.match]
any = true
"#;
    let err = load_from_str_with_env(toml, &env).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("not-a-socket") || msg.contains("invalid"),
        "got: {msg}"
    );
}

#[test]
fn webhooks_disabled_by_default() {
    let env = MapEnv::new([]);
    let toml = r#"
[sip]
listen = "0.0.0.0:5060"

[bridge]
ws_url = "wss://x/y"

[[route]]
name = "d"
[route.match]
any = true
"#;
    let cfg = load_from_str_with_env(toml, &env).unwrap();
    assert!(!cfg.webhooks.enabled);
    assert!(cfg.webhooks.url.is_none());
    assert!(cfg.webhooks.events.is_empty());
}

#[test]
fn webhooks_enabled_compiles_with_url_auth_and_allowlist() {
    let env = MapEnv::new([("WEBHOOK_TOKEN", "wh-xyz")]);
    let toml = r#"
[sip]
listen = "0.0.0.0:5060"

[bridge]
ws_url = "wss://x/y"

[webhooks]
enabled = true
url = "https://ops.example.com/siphon-events"
auth_header = "Bearer ${WEBHOOK_TOKEN}"
events = ["call_start", "call_end"]
retry_max = 5
timeout_ms = 4000

[[route]]
name = "d"
[route.match]
any = true
"#;
    let cfg = load_from_str_with_env(toml, &env).unwrap();
    assert!(cfg.webhooks.enabled);
    assert_eq!(
        cfg.webhooks.url.as_deref(),
        Some("https://ops.example.com/siphon-events")
    );
    assert_eq!(cfg.webhooks.auth_header.as_deref(), Some("Bearer wh-xyz"));
    assert_eq!(
        cfg.webhooks.events,
        vec!["call_start".to_string(), "call_end".to_string()]
    );
    assert_eq!(cfg.webhooks.retry_max, 5);
    assert_eq!(cfg.webhooks.timeout, std::time::Duration::from_millis(4000));
}

#[test]
fn webhooks_enabled_without_url_errors() {
    let env = MapEnv::new([]);
    let toml = r#"
[sip]
listen = "0.0.0.0:5060"

[bridge]
ws_url = "wss://x/y"

[webhooks]
enabled = true

[[route]]
name = "d"
[route.match]
any = true
"#;
    let err = load_from_str_with_env(toml, &env).unwrap_err();
    assert!(err.to_string().contains("url is required"));
}

#[test]
fn webhooks_disabled_tolerates_missing_url() {
    // Master-switch-off pattern.
    let env = MapEnv::new([]);
    let toml = r#"
[sip]
listen = "0.0.0.0:5060"

[bridge]
ws_url = "wss://x/y"

[webhooks]
enabled = false

[[route]]
name = "d"
[route.match]
any = true
"#;
    let cfg = load_from_str_with_env(toml, &env).unwrap();
    assert!(!cfg.webhooks.enabled);
}

#[test]
fn observability_disabled_by_default() {
    let env = MapEnv::new([]);
    let toml = r#"
[sip]
listen = "0.0.0.0:5060"

[bridge]
ws_url = "wss://x/y"

[[route]]
name = "d"
[route.match]
any = true
"#;
    let cfg = load_from_str_with_env(toml, &env).unwrap();
    assert!(!cfg.observability.enabled);
    assert!(cfg.observability.http_listen.is_none());
}

#[test]
fn observability_enabled_compiles_with_listen_addr() {
    let env = MapEnv::new([]);
    let toml = r#"
[sip]
listen = "0.0.0.0:5060"

[bridge]
ws_url = "wss://x/y"

[observability]
enabled = true
http_listen = "0.0.0.0:9090"

[[route]]
name = "d"
[route.match]
any = true
"#;
    let cfg = load_from_str_with_env(toml, &env).unwrap();
    assert!(cfg.observability.enabled);
    assert_eq!(cfg.observability.http_listen.map(|a| a.port()), Some(9090));
}

#[test]
fn observability_enabled_without_listen_errors() {
    let env = MapEnv::new([]);
    let toml = r#"
[sip]
listen = "0.0.0.0:5060"

[bridge]
ws_url = "wss://x/y"

[observability]
enabled = true

[[route]]
name = "d"
[route.match]
any = true
"#;
    let err = load_from_str_with_env(toml, &env).unwrap_err();
    assert!(err.to_string().contains("http_listen is required"));
}

#[test]
fn observability_bad_listen_addr_errors() {
    let env = MapEnv::new([]);
    let toml = r#"
[sip]
listen = "0.0.0.0:5060"

[bridge]
ws_url = "wss://x/y"

[observability]
enabled = true
http_listen = "not-a-socket"

[[route]]
name = "d"
[route.match]
any = true
"#;
    let err = load_from_str_with_env(toml, &env).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("not-a-socket") || msg.contains("invalid"),
        "got: {msg}"
    );
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
