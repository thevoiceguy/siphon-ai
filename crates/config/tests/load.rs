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
listen = "127.0.0.1:5060"
transports = ["udp", "tcp"]
user_agent = "SiphonAI/0.1.0-test"

[media]
codecs = ["pcmu", "pcma"]
dtmf = "rfc2833"
rtp_port_range = [16384, 32768]

[bridge]
ws_url = "wss://default.example.com/sip-bridge"
ws_auth_header = "Bearer ${BRIDGE_TOKEN}"
ws_connect_timeout_ms = 3000
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
    // expansion. Stored verbatim — the bridge emits the full
    // value as `Authorization:` on the WS upgrade. (Bare tokens
    // get normalised to `Bearer <token>` upstream; values that
    // already contain a scheme pass through.)
    assert_eq!(
        cfg.bridge_defaults.auth_header.as_deref(),
        Some("Bearer abc-secret-123")
    );
    assert_eq!(
        cfg.bridge_defaults.connect_timeout,
        Duration::from_millis(3000)
    );
    assert_eq!(cfg.bridge_defaults.codecs, vec![Codec::Pcmu, Codec::Pcma]);
    assert_eq!(cfg.bridge_defaults.dtmf_payload_type, Some(101));
    assert_eq!(
        cfg.bridge_defaults.forward_headers,
        vec!["User-Agent".to_string(), "X-Tenant-Id".to_string()],
    );
    // WS reconnect (0.7.3): off by default, 30 s window, when unset.
    assert!(!cfg.bridge_defaults.ws_reconnect_enabled);
    assert_eq!(
        cfg.bridge_defaults.ws_reconnect_max,
        Duration::from_secs(30)
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

const RECONNECT_TOML: &str = r#"
[node]
id = "siphon-ai-reconnect-test"
public_address = "203.0.113.10"
[sip]
listen = "127.0.0.1:5060"
[bridge]
ws_url = "wss://default.example.com/sip-bridge"
ws_reconnect_enabled = true
WS_RECONNECT_MAX
[[route]]
name = "default"
[route.match]
any = true
"#;

#[test]
fn ws_reconnect_enabled_with_default_window_compiles() {
    let toml = RECONNECT_TOML.replace("WS_RECONNECT_MAX\n", ""); // window unset → 30 s
    let cfg = load_from_str_with_env(&toml, &MapEnv::new([])).expect("compiles");
    assert!(cfg.bridge_defaults.ws_reconnect_enabled);
    assert_eq!(
        cfg.bridge_defaults.ws_reconnect_max,
        Duration::from_secs(30)
    );
}

#[test]
fn ws_reconnect_custom_window_compiles() {
    let toml = RECONNECT_TOML.replace("WS_RECONNECT_MAX", "ws_reconnect_max_secs = 45");
    let cfg = load_from_str_with_env(&toml, &MapEnv::new([])).expect("compiles");
    assert_eq!(
        cfg.bridge_defaults.ws_reconnect_max,
        Duration::from_secs(45)
    );
}

#[test]
fn ws_reconnect_enabled_with_zero_window_fails() {
    // Enabling reconnect with a zero window is a fail-loud config error.
    let toml = RECONNECT_TOML.replace("WS_RECONNECT_MAX", "ws_reconnect_max_secs = 0");
    let err = load_from_str_with_env(&toml, &MapEnv::new([])).unwrap_err();
    assert!(
        matches!(err, LoadError::Compile(_)),
        "expected a compile error, got {err:?}"
    );
}

#[test]
fn env_default_used_when_var_absent() {
    let env = MapEnv::new([]);
    let toml = r#"
[sip]
listen = "127.0.0.1:${SIP_PORT:-5070}"

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
fn tcp_idle_timeout_defaults_to_1800() {
    let toml = r#"
[sip]
listen = "127.0.0.1:5060"

[bridge]
ws_url = "wss://x/y"
"#;
    let cfg = load_from_str_with_env(toml, &MapEnv::new([])).expect("compiles");
    assert_eq!(cfg.sip.tcp_idle_timeout_secs, 1800);
}

#[test]
fn tcp_idle_timeout_custom_and_zero() {
    for (val, want) in [("300", 300u64), ("0", 0)] {
        let toml = format!(
            r#"
[sip]
listen = "127.0.0.1:5060"
tcp_idle_timeout_secs = {val}

[bridge]
ws_url = "wss://x/y"
"#
        );
        let cfg = load_from_str_with_env(&toml, &MapEnv::new([])).expect("compiles");
        assert_eq!(cfg.sip.tcp_idle_timeout_secs, want);
    }
}

#[test]
fn otlp_disabled_by_default_and_independent_of_metrics_listener() {
    // No [observability] block at all → otlp is None.
    let toml = "[sip]\nlisten = \"127.0.0.1:5060\"\n[bridge]\nws_url = \"wss://x/y\"\n";
    let cfg = load_from_str_with_env(toml, &MapEnv::new([])).expect("compiles");
    assert!(cfg.observability.otlp.is_none());

    // OTLP enabled WITHOUT the metrics HTTP server — a valid setup.
    let toml = r#"
[sip]
listen = "127.0.0.1:5060"
[bridge]
ws_url = "wss://x/y"
[observability.otlp]
enabled = true
"#;
    let cfg = load_from_str_with_env(toml, &MapEnv::new([])).expect("compiles");
    assert!(!cfg.observability.enabled, "metrics listener stays off");
    let otlp = cfg.observability.otlp.expect("otlp compiled");
    assert_eq!(otlp.endpoint, "http://localhost:4317");
    assert_eq!(otlp.sample_ratio, 1.0);
    assert_eq!(otlp.service_name, "siphon-ai");
}

#[test]
fn otlp_custom_fields_and_attributes() {
    let toml = r#"
[sip]
listen = "127.0.0.1:5060"
[bridge]
ws_url = "wss://x/y"
[observability.otlp]
enabled = true
endpoint = "http://collector:4317"
sample_ratio = 0.25
timeout_ms = 2000
service_name = "siphon-edge"
[observability.otlp.attributes]
"deployment.environment" = "prod"
region = "us-east"
"#;
    let cfg = load_from_str_with_env(toml, &MapEnv::new([])).expect("compiles");
    let otlp = cfg.observability.otlp.expect("otlp compiled");
    assert_eq!(otlp.endpoint, "http://collector:4317");
    assert_eq!(otlp.sample_ratio, 0.25);
    assert_eq!(otlp.timeout.as_millis(), 2000);
    assert_eq!(otlp.service_name, "siphon-edge");
    assert!(otlp
        .attributes
        .contains(&("deployment.environment".to_string(), "prod".to_string())));
}

#[test]
fn otlp_bad_sample_ratio_fails_load() {
    let toml = r#"
[sip]
listen = "127.0.0.1:5060"
[bridge]
ws_url = "wss://x/y"
[observability.otlp]
enabled = true
sample_ratio = 1.5
"#;
    let err = load_from_str_with_env(toml, &MapEnv::new([])).unwrap_err();
    assert!(
        format!("{err}").contains("sample_ratio"),
        "expected sample_ratio range error, got: {err}"
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
fn allow_delayed_offer_defaults_true_and_can_be_disabled() {
    let env = MapEnv::new([]);
    let base = r#"
[sip]
listen = "127.0.0.1:5060"
__DELAYED__

[bridge]
ws_url = "wss://x/y"

[[route]]
name = "default"
[route.match]
any = true
"#;
    // Default (field omitted) — delayed offer is accepted.
    let cfg = load_from_str_with_env(&base.replace("__DELAYED__", ""), &env)
        .expect("compiles with default");
    assert!(cfg.sip.allow_delayed_offer, "delayed offer on by default");

    // Explicit opt-out.
    let cfg = load_from_str_with_env(
        &base.replace("__DELAYED__", "allow_delayed_offer = false"),
        &env,
    )
    .expect("compiles with opt-out");
    assert!(
        !cfg.sip.allow_delayed_offer,
        "operator can force early offer"
    );
}

#[test]
fn srtp_offer_defaults_sdes_parses_dtls_and_rejects_unknown() {
    let env = MapEnv::new([]);
    let base = r#"
[sip]
listen = "127.0.0.1:5060"

[media]
codecs = ["pcmu"]
srtp = "required"
__OFFER__

[bridge]
ws_url = "wss://x/y"

[[route]]
name = "default"
[route.match]
any = true
"#;
    // Default (field omitted) → SDES (offer_dtls_srtp false).
    let cfg = load_from_str_with_env(&base.replace("__OFFER__\n", ""), &env)
        .expect("compiles with default");
    assert!(!cfg.bridge_defaults.offer_dtls_srtp, "defaults to SDES");

    // Explicit "dtls".
    let cfg = load_from_str_with_env(&base.replace("__OFFER__", r#"srtp_offer = "dtls""#), &env)
        .expect("compiles with dtls");
    assert!(cfg.bridge_defaults.offer_dtls_srtp, "dtls offer enabled");

    // "sdes" is explicit-default.
    let cfg = load_from_str_with_env(&base.replace("__OFFER__", r#"srtp_offer = "sdes""#), &env)
        .expect("compiles with sdes");
    assert!(!cfg.bridge_defaults.offer_dtls_srtp);

    // Unknown → fail loud (CLAUDE.md §4.6).
    let err = load_from_str_with_env(&base.replace("__OFFER__", r#"srtp_offer = "tls""#), &env)
        .unwrap_err();
    assert!(
        matches!(err, LoadError::Compile(_)),
        "unknown srtp_offer must be a compile error, got {err:?}"
    );
}

const ADMIN_BASE: &str = r#"
[sip]
listen = "127.0.0.1:5060"
[bridge]
ws_url = "wss://x/y"
[[route]]
name = "default"
[route.match]
any = true
__ADMIN__
"#;

#[test]
fn admin_absent_means_not_served() {
    let cfg = load_from_str_with_env(&ADMIN_BASE.replace("__ADMIN__", ""), &MapEnv::new([]))
        .expect("compiles");
    assert!(cfg.admin.is_none(), "no [admin] block → /admin not served");
}

#[test]
fn admin_with_tokens_compiles_and_gates_by_role() {
    let toml = ADMIN_BASE.replace(
        "__ADMIN__",
        r#"
[admin]
listen = "127.0.0.1:9092"
[[admin.token]]
name = "ops"
token = "${OPS_TOK}"
role = "operator"
[[admin.token]]
name = "billing"
token = "secret-admin"
role = "admin"
"#,
    );
    let cfg =
        load_from_str_with_env(&toml, &MapEnv::new([("OPS_TOK", "secret-ops")])).expect("compiles");
    let admin = cfg.admin.expect("admin configured");
    assert_eq!(admin.listen_addr.port(), 9092);
    // The env-expanded operator token authenticates as operator; the
    // admin token as admin; a wrong token does not authenticate.
    use siphon_ai_telemetry::Role;
    assert_eq!(
        admin.auth.authenticate("secret-ops").map(|t| t.role),
        Some(Role::Operator)
    );
    assert_eq!(
        admin.auth.authenticate("secret-admin").map(|t| t.role),
        Some(Role::Admin)
    );
    assert!(admin.auth.authenticate("nope").is_none());
}

#[test]
fn admin_without_tokens_is_an_error() {
    let toml = ADMIN_BASE.replace("__ADMIN__", "[admin]\nlisten = \"127.0.0.1:9092\"\n");
    let err = load_from_str_with_env(&toml, &MapEnv::new([])).unwrap_err();
    assert!(
        matches!(err, LoadError::Compile(_)),
        "an admin listener with no tokens must fail loud, got {err:?}"
    );
}

#[test]
fn admin_unknown_role_and_bad_listen_are_errors() {
    let bad_role = ADMIN_BASE.replace(
        "__ADMIN__",
        "[admin]\nlisten = \"127.0.0.1:9092\"\n[[admin.token]]\nname=\"x\"\ntoken=\"t\"\nrole=\"root\"\n",
    );
    assert!(matches!(
        load_from_str_with_env(&bad_role, &MapEnv::new([])).unwrap_err(),
        LoadError::Compile(_)
    ));

    let bad_listen = ADMIN_BASE.replace(
        "__ADMIN__",
        "[admin]\nlisten = \"not-an-addr\"\n[[admin.token]]\nname=\"x\"\ntoken=\"t\"\nrole=\"admin\"\n",
    );
    assert!(matches!(
        load_from_str_with_env(&bad_listen, &MapEnv::new([])).unwrap_err(),
        LoadError::Compile(_)
    ));
}

#[test]
fn admin_tls_compiles_with_cert_and_key() {
    let toml = ADMIN_BASE.replace(
        "__ADMIN__",
        r#"
[admin]
listen = "0.0.0.0:9092"
[[admin.token]]
name = "ops"
token = "t"
role = "operator"
[admin.tls]
cert = "/etc/siphon/admin.crt"
key = "/etc/siphon/admin.key"
"#,
    );
    let cfg = load_from_str_with_env(&toml, &MapEnv::new([])).expect("compiles");
    let tls = cfg
        .admin
        .expect("admin configured")
        .tls
        .expect("admin.tls configured");
    assert_eq!(tls.cert_path.to_str(), Some("/etc/siphon/admin.crt"));
    assert_eq!(tls.key_path.to_str(), Some("/etc/siphon/admin.key"));
}

#[test]
fn admin_tls_without_cert_or_key_is_an_error() {
    // cert present, key missing
    let no_key = ADMIN_BASE.replace(
        "__ADMIN__",
        "[admin]\nlisten=\"127.0.0.1:9092\"\n[[admin.token]]\nname=\"x\"\ntoken=\"t\"\nrole=\"admin\"\n[admin.tls]\ncert=\"/c.crt\"\n",
    );
    assert!(
        matches!(
            load_from_str_with_env(&no_key, &MapEnv::new([])).unwrap_err(),
            LoadError::Compile(_)
        ),
        "[admin.tls] with cert but no key must fail loud"
    );

    // empty cert string
    let empty_cert = ADMIN_BASE.replace(
        "__ADMIN__",
        "[admin]\nlisten=\"127.0.0.1:9092\"\n[[admin.token]]\nname=\"x\"\ntoken=\"t\"\nrole=\"admin\"\n[admin.tls]\ncert=\"\"\nkey=\"/k.key\"\n",
    );
    assert!(
        matches!(
            load_from_str_with_env(&empty_cert, &MapEnv::new([])).unwrap_err(),
            LoadError::Compile(_)
        ),
        "[admin.tls] with an empty cert path must fail loud"
    );
}

#[test]
fn admin_tls_paths_env_expand() {
    let toml = ADMIN_BASE.replace(
        "__ADMIN__",
        "[admin]\nlisten=\"127.0.0.1:9092\"\n[[admin.token]]\nname=\"x\"\ntoken=\"t\"\nrole=\"admin\"\n[admin.tls]\ncert=\"${ADMIN_CRT}\"\nkey=\"${ADMIN_KEY}\"\n",
    );
    let cfg = load_from_str_with_env(
        &toml,
        &MapEnv::new([("ADMIN_CRT", "/x/a.crt"), ("ADMIN_KEY", "/x/a.key")]),
    )
    .expect("compiles");
    let tls = cfg.admin.unwrap().tls.expect("admin.tls");
    assert_eq!(tls.cert_path.to_str(), Some("/x/a.crt"));
    assert_eq!(tls.key_path.to_str(), Some("/x/a.key"));
}

#[test]
fn admin_without_tls_has_none() {
    let toml = ADMIN_BASE.replace(
        "__ADMIN__",
        "[admin]\nlisten=\"127.0.0.1:9092\"\n[[admin.token]]\nname=\"x\"\ntoken=\"t\"\nrole=\"admin\"\n",
    );
    let cfg = load_from_str_with_env(&toml, &MapEnv::new([])).expect("compiles");
    assert!(cfg.admin.unwrap().tls.is_none());
}

#[test]
fn sip_auth_absent_means_off() {
    let cfg = load_from_str_with_env(&ADMIN_BASE.replace("__ADMIN__", ""), &MapEnv::new([]))
        .expect("compiles");
    assert!(cfg.sip.auth.is_none(), "no [sip.auth] → digest off");
}

#[test]
fn sip_auth_disabled_flag_means_off() {
    let toml = ADMIN_BASE.replace(
        "__ADMIN__",
        "[sip.auth]\nenabled = false\nrealm = \"x\"\n[[sip.auth.user]]\nusername=\"a\"\npassword=\"b\"\n",
    );
    let cfg = load_from_str_with_env(&toml, &MapEnv::new([])).expect("compiles");
    assert!(cfg.sip.auth.is_none(), "enabled=false → digest off");
}

#[test]
fn sip_auth_compiles_with_defaults() {
    let toml = ADMIN_BASE.replace(
        "__ADMIN__",
        r#"
[sip.auth]
enabled = true
realm = "siphon.example"
[[sip.auth.user]]
username = "alice"
password = "${ALICE_PW}"
"#,
    );
    let cfg =
        load_from_str_with_env(&toml, &MapEnv::new([("ALICE_PW", "s3cret")])).expect("compiles");
    let auth = cfg.sip.auth.expect("auth configured");
    assert_eq!(auth.realm, "siphon.example");
    assert_eq!(auth.algorithm, "SHA-256"); // default
    assert_eq!(auth.qop, "auth"); // default
    assert_eq!(auth.users.len(), 1);
    assert_eq!(auth.users[0].username, "alice");
    assert_eq!(auth.users[0].password, "s3cret"); // env-expanded
}

#[test]
fn sip_auth_canonicalises_algorithm_and_qop() {
    let toml = ADMIN_BASE.replace(
        "__ADMIN__",
        "[sip.auth]\nenabled=true\nrealm=\"x\"\nalgorithm=\"sha-512\"\nqop=\"auth-int\"\n[[sip.auth.user]]\nusername=\"a\"\npassword=\"b\"\n",
    );
    let auth = load_from_str_with_env(&toml, &MapEnv::new([]))
        .expect("compiles")
        .sip
        .auth
        .expect("auth");
    assert_eq!(auth.algorithm, "SHA-512");
    assert_eq!(auth.qop, "auth-int");
}

#[test]
fn sip_auth_validation_errors() {
    let cases = [
        // enabled, no realm
        "[sip.auth]\nenabled=true\n[[sip.auth.user]]\nusername=\"a\"\npassword=\"b\"\n",
        // enabled, no users
        "[sip.auth]\nenabled=true\nrealm=\"x\"\n",
        // unknown algorithm
        "[sip.auth]\nenabled=true\nrealm=\"x\"\nalgorithm=\"rot13\"\n[[sip.auth.user]]\nusername=\"a\"\npassword=\"b\"\n",
        // unknown qop
        "[sip.auth]\nenabled=true\nrealm=\"x\"\nqop=\"nope\"\n[[sip.auth.user]]\nusername=\"a\"\npassword=\"b\"\n",
        // empty password
        "[sip.auth]\nenabled=true\nrealm=\"x\"\n[[sip.auth.user]]\nusername=\"a\"\npassword=\"\"\n",
        // duplicate username
        "[sip.auth]\nenabled=true\nrealm=\"x\"\n[[sip.auth.user]]\nusername=\"a\"\npassword=\"b\"\n[[sip.auth.user]]\nusername=\"a\"\npassword=\"c\"\n",
    ];
    for (i, block) in cases.iter().enumerate() {
        let toml = ADMIN_BASE.replace("__ADMIN__", block);
        assert!(
            matches!(
                load_from_str_with_env(&toml, &MapEnv::new([])).unwrap_err(),
                LoadError::Compile(_)
            ),
            "case {i} should be a compile error: {block}"
        );
    }
}

#[test]
fn trunk_auth_required_defaults_false_and_parses_true() {
    let toml = ADMIN_BASE.replace(
        "__ADMIN__",
        r#"
[[trunk]]
name = "static-carrier"
peer_addrs = ["203.0.113.7"]
[[trunk]]
name = "roaming"
from_hosts = ["sip.roam.example"]
auth_required = true
"#,
    );
    let cfg = load_from_str_with_env(&toml, &MapEnv::new([])).expect("compiles");
    let by_name = |n: &str| cfg.trunks.iter().find(|t| t.name == n).unwrap();
    assert!(!by_name("static-carrier").auth_required, "default false");
    assert!(by_name("roaming").auth_required, "explicit true");
}

#[test]
fn sip_admission_absent_means_off() {
    let cfg = load_from_str_with_env(&ADMIN_BASE.replace("__ADMIN__", ""), &MapEnv::new([]))
        .expect("compiles");
    assert!(cfg.sip.admission.is_none());
}

#[test]
fn sip_admission_empty_block_is_off() {
    // A `[sip.admission]` with no knobs set is a no-op, not an error.
    let toml = ADMIN_BASE.replace("__ADMIN__", "[sip.admission]\n");
    let cfg = load_from_str_with_env(&toml, &MapEnv::new([])).expect("compiles");
    assert!(cfg.sip.admission.is_none());
}

#[test]
fn sip_admission_compiles_with_defaults() {
    let toml = ADMIN_BASE.replace("__ADMIN__", "[sip.admission]\nmax_per_sec = 10\n");
    let adm = load_from_str_with_env(&toml, &MapEnv::new([]))
        .expect("compiles")
        .sip
        .admission
        .expect("admission on");
    assert_eq!(adm.max_per_sec, 10);
    assert_eq!(adm.burst, 10); // defaults to max_per_sec
    assert_eq!(adm.drop_after, 10); // default
    assert_eq!(adm.max_concurrent, 0); // unset
    assert_eq!(adm.max_sources, 10_000); // default
}

#[test]
fn sip_admission_explicit_values() {
    let toml = ADMIN_BASE.replace(
        "__ADMIN__",
        "[sip.admission]\nmax_per_sec = 5\nburst = 20\ndrop_after = 3\nmax_concurrent = 200\nmax_sources = 50000\n",
    );
    let adm = load_from_str_with_env(&toml, &MapEnv::new([]))
        .expect("compiles")
        .sip
        .admission
        .expect("admission on");
    assert_eq!(adm.max_per_sec, 5);
    assert_eq!(adm.burst, 20);
    assert_eq!(adm.drop_after, 3);
    assert_eq!(adm.max_concurrent, 200);
    assert_eq!(adm.max_sources, 50_000);
}

#[test]
fn sip_admission_global_cap_only() {
    // max_concurrent alone (no per-source rate) still enables admission.
    let toml = ADMIN_BASE.replace("__ADMIN__", "[sip.admission]\nmax_concurrent = 100\n");
    let adm = load_from_str_with_env(&toml, &MapEnv::new([]))
        .expect("compiles")
        .sip
        .admission
        .expect("admission on");
    assert_eq!(adm.max_per_sec, 0);
    assert_eq!(adm.max_concurrent, 100);
}

#[test]
fn sip_admission_burst_below_rate_is_error() {
    let toml = ADMIN_BASE.replace(
        "__ADMIN__",
        "[sip.admission]\nmax_per_sec = 10\nburst = 3\n",
    );
    assert!(matches!(
        load_from_str_with_env(&toml, &MapEnv::new([])).unwrap_err(),
        LoadError::Compile(_)
    ));
}

#[test]
fn unknown_codec_is_a_compile_error() {
    let env = MapEnv::new([]);
    let toml = r#"
[sip]
listen = "127.0.0.1:5060"

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
listen = "127.0.0.1:5060"
transports = ["udp", "smoke"]

[bridge]
ws_url = "wss://x/y"
"#;
    let err = load_from_str_with_env(toml, &env).unwrap_err();
    assert!(err.to_string().contains("smoke"));
}

#[test]
fn invalid_per_route_min_attestation_rejected_at_load() {
    let env = MapEnv::new([]);
    let toml = r#"
[sip]
listen = "127.0.0.1:5060"

[bridge]
ws_url = "wss://x/y"

[[route]]
name = "vip"
[route.match]
any = true
[route.security]
min_attestation = "platinum"
"#;
    let err = load_from_str_with_env(toml, &env).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("platinum") && msg.contains("vip"),
        "got: {msg}"
    );
}

#[test]
fn per_route_min_attestation_without_stir_shaken_rejected() {
    // A valid level, but verification is off — the gate would 4xx every
    // call this route matches, so config load must fail loud.
    let env = MapEnv::new([]);
    let toml = r#"
[sip]
listen = "127.0.0.1:5060"

[bridge]
ws_url = "wss://x/y"

[[route]]
name = "vip"
[route.match]
any = true
[route.security]
min_attestation = "A"
"#;
    let err = load_from_str_with_env(toml, &env).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("vip") && msg.contains("stir_shaken"),
        "got: {msg}"
    );
}

#[test]
fn per_route_min_attestation_none_is_inert_without_stir_shaken() {
    // `min_attestation = "none"` is a no-op override — allowed even with
    // verification off (it can't reject anything).
    let env = MapEnv::new([]);
    let toml = r#"
[sip]
listen = "127.0.0.1:5060"

[bridge]
ws_url = "wss://x/y"

[[route]]
name = "open"
[route.match]
any = true
[route.security]
min_attestation = "none"
"#;
    let cfg = load_from_str_with_env(toml, &env).expect("none override compiles");
    let route = cfg.routes.iter().next().expect("one route");
    assert_eq!(route.security.min_attestation.as_deref(), Some("none"));
}

#[test]
fn session_timer_defaults_to_90s_floor() {
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
    assert_eq!(cfg.sip.min_session_expires, Duration::from_secs(90));
    assert_eq!(cfg.sip.preferred_session_expires, None);
}

#[test]
fn session_timer_below_90s_rejected_at_load() {
    let env = MapEnv::new([]);
    let toml = r#"
[sip]
listen = "127.0.0.1:5060"
min_session_expires_secs = 30

[bridge]
ws_url = "wss://x/y"
"#;
    let err = load_from_str_with_env(toml, &env).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("90") && msg.contains("30"),
        "expected RFC 4028 floor rejection, got: {msg}"
    );
}

#[test]
fn session_timer_explicit_values_compile() {
    let env = MapEnv::new([]);
    let toml = r#"
[sip]
listen = "127.0.0.1:5060"
min_session_expires_secs = 300
preferred_session_expires_secs = 1800

[bridge]
ws_url = "wss://x/y"

[[route]]
name = "default"
[route.match]
any = true
"#;
    let cfg = load_from_str_with_env(toml, &env).unwrap();
    assert_eq!(cfg.sip.min_session_expires, Duration::from_secs(300));
    assert_eq!(
        cfg.sip.preferred_session_expires,
        Some(Duration::from_secs(1800)),
    );
}

#[test]
fn on_ws_failure_play_prompt_requires_a_file() {
    // 0.34.0: `play_prompt` is now a real mode — but an effective
    // play_prompt with no effective prompt file anywhere is a config
    // mistake, rejected at load (fail loud, not on the first failing
    // call).
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
[route.bridge]
on_ws_failure = "play_prompt"
"#;
    let err = load_from_str_with_env(toml, &env).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("ws_failure_prompt_file"),
        "expected the file-required rejection, got: {msg}"
    );
}

#[test]
fn on_ws_failure_play_prompt_with_file_compiles_globally_and_inherits() {
    // Global play_prompt + file: compiles, and a route with no
    // override inherits it (the cross-check accepts the global file).
    let dir = std::env::temp_dir().join(format!("siphon_wsfp_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let wav = dir.join("prompt.wav");
    std::fs::write(&wav, b"RIFF").unwrap(); // existence is the load gate
    let env = MapEnv::new([]);
    let toml = format!(
        r#"
[sip]
listen = "127.0.0.1:5060"

[bridge]
ws_url = "wss://x/y"
on_ws_failure = "play_prompt"
ws_failure_prompt_file = "{}"

[[route]]
name = "default"
[route.match]
any = true
"#,
        wav.display()
    );
    let cfg = load_from_str_with_env(&toml, &env).expect("global play_prompt + file compiles");
    assert_eq!(
        cfg.bridge_defaults.ws_failure_action,
        siphon_ai_core::WsFailureAction::PlayPrompt
    );
    assert_eq!(
        cfg.bridge_defaults.ws_failure_prompt_file.as_deref(),
        Some(wav.as_path())
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn on_ws_failure_missing_file_and_unknown_value_fail() {
    let env = MapEnv::new([]);
    // File doesn't exist → load error.
    let toml = r#"
[sip]
listen = "127.0.0.1:5060"

[bridge]
ws_url = "wss://x/y"
on_ws_failure = "play_prompt"
ws_failure_prompt_file = "/nonexistent/prompt.wav"

[[route]]
name = "default"
[route.match]
any = true
"#;
    let msg = load_from_str_with_env(toml, &env).unwrap_err().to_string();
    assert!(
        msg.contains("does not exist"),
        "expected missing-file rejection, got: {msg}"
    );
    // Unknown value → load error (global and route grammar agree).
    let toml = r#"
[sip]
listen = "127.0.0.1:5060"

[bridge]
ws_url = "wss://x/y"
on_ws_failure = "play-prompt"

[[route]]
name = "default"
[route.match]
any = true
"#;
    let msg = load_from_str_with_env(toml, &env).unwrap_err().to_string();
    assert!(
        msg.contains("on_ws_failure"),
        "expected unknown-value rejection, got: {msg}"
    );
}

#[test]
fn route_hangup_override_opts_out_of_global_play_prompt() {
    // A route explicitly on "hangup" under a global play_prompt is
    // valid without any file of its own.
    let dir = std::env::temp_dir().join(format!("siphon_wsfp_opt_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let wav = dir.join("prompt.wav");
    std::fs::write(&wav, b"RIFF").unwrap();
    let env = MapEnv::new([]);
    let toml = format!(
        r#"
[sip]
listen = "127.0.0.1:5060"

[bridge]
ws_url = "wss://x/y"
on_ws_failure = "play_prompt"
ws_failure_prompt_file = "{}"

[[route]]
name = "quiet"
[route.match]
request_uri_user = "7000"
[route.bridge]
on_ws_failure = "hangup"

[[route]]
name = "default"
[route.match]
any = true
"#,
        wav.display()
    );
    load_from_str_with_env(&toml, &env).expect("route-level hangup opt-out compiles");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn on_ws_failure_hangup_compiles() {
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
[route.bridge]
on_ws_failure = "hangup"
"#;
    let cfg = load_from_str_with_env(toml, &env).expect("hangup is the v1 supported mode");
    assert!(cfg.routes.has_default());
}

#[test]
fn inactivity_timeout_defaults_to_60s_when_absent() {
    // The watchdog default in `BridgeDefaults::default()` is 60s so
    // an operator who never wrote the field still gets a sensible
    // safety net against zombie calls.
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
    assert_eq!(
        cfg.bridge_defaults.inactivity_timeout,
        Some(Duration::from_secs(60)),
    );
}

#[test]
fn inactivity_timeout_zero_disables_watchdog() {
    let env = MapEnv::new([]);
    let toml = r#"
[sip]
listen = "127.0.0.1:5060"

[media]
inactivity_timeout_secs = 0

[bridge]
ws_url = "wss://x/y"

[[route]]
name = "default"
[route.match]
any = true
"#;
    let cfg = load_from_str_with_env(toml, &env).unwrap();
    assert_eq!(cfg.bridge_defaults.inactivity_timeout, None);
}

#[test]
fn inactivity_timeout_explicit_value_compiles() {
    let env = MapEnv::new([]);
    let toml = r#"
[sip]
listen = "127.0.0.1:5060"

[media]
inactivity_timeout_secs = 45

[bridge]
ws_url = "wss://x/y"

[[route]]
name = "default"
[route.match]
any = true
"#;
    let cfg = load_from_str_with_env(toml, &env).unwrap();
    assert_eq!(
        cfg.bridge_defaults.inactivity_timeout,
        Some(Duration::from_secs(45)),
    );
}

#[test]
fn opus_in_codec_list_compiles() {
    // Opus is accepted as of 0.8.0 (forge runs the codec at a 16 kHz
    // bridge rate; libopus does the 48<->16 + stereo->mono). It now
    // satisfies the WS audio path's 8/16 kHz contract, so it parses into
    // the compiled codec set instead of being rejected.
    let env = MapEnv::new([]);
    let toml = r#"
[node]
id = "opus-test"
public_address = "203.0.113.10"
[sip]
listen = "127.0.0.1:5060"
[media]
codecs = ["pcmu", "opus"]
[bridge]
ws_url = "wss://x/y"
[[route]]
name = "default"
[route.match]
any = true
"#;
    let cfg = load_from_str_with_env(toml, &env).expect("opus config compiles");
    assert!(
        cfg.bridge_defaults.codecs.contains(&Codec::Opus),
        "Opus should be in the compiled codec set, got {:?}",
        cfg.bridge_defaults.codecs
    );
}

// ─── [[trunk]] allowlist ───────────────────────────────────────

#[test]
fn no_trunks_means_no_gate() {
    // Zero [[trunk]] blocks → daemon stays in legacy "accept any
    // source" mode. The compiled `trunks` list is empty.
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
    assert!(cfg.trunks.is_empty());
}

#[test]
fn trunk_with_ip_only_compiles() {
    let env = MapEnv::new([]);
    let toml = r#"
[sip]
listen = "127.0.0.1:5060"

[bridge]
ws_url = "wss://x/y"

[[trunk]]
name = "freeswitch-main"
peer_addrs = ["10.0.0.10", "10.0.1.0/24"]

[[route]]
name = "default"
[route.match]
any = true
"#;
    let cfg = load_from_str_with_env(toml, &env).unwrap();
    assert_eq!(cfg.trunks.len(), 1);
    let t = &cfg.trunks[0];
    assert_eq!(t.name, "freeswitch-main");
    assert_eq!(t.peer_addrs.len(), 2);
    // Exact IP normalises to /32; CIDR keeps its prefix.
    assert_eq!(t.peer_addrs[0].prefix_len, 32);
    assert_eq!(t.peer_addrs[1].prefix_len, 24);
    assert!(t.from_hosts.is_empty());
}

#[test]
fn trunk_with_from_hosts_only_compiles() {
    let env = MapEnv::new([]);
    let toml = r#"
[sip]
listen = "127.0.0.1:5060"

[bridge]
ws_url = "wss://x/y"

[[trunk]]
name = "carrier-a"
from_hosts = ["sip.carrier-a.example", "BACKUP.CARRIER-A.example"]

[[route]]
name = "default"
[route.match]
any = true
"#;
    let cfg = load_from_str_with_env(toml, &env).unwrap();
    let t = &cfg.trunks[0];
    assert!(t.peer_addrs.is_empty());
    // Lowercased + trim'd.
    assert_eq!(
        t.from_hosts,
        vec!["sip.carrier-a.example", "backup.carrier-a.example"]
    );
}

#[test]
fn trunk_without_match_criteria_rejected() {
    let env = MapEnv::new([]);
    let toml = r#"
[sip]
listen = "127.0.0.1:5060"

[bridge]
ws_url = "wss://x/y"

[[trunk]]
name = "anonymous"

[[route]]
name = "default"
[route.match]
any = true
"#;
    let err = load_from_str_with_env(toml, &env).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("anonymous") && msg.contains("neither peer_addrs nor from_hosts"),
        "got: {msg}"
    );
}

#[test]
fn trunk_with_bad_cidr_rejected() {
    let env = MapEnv::new([]);
    let toml = r#"
[sip]
listen = "127.0.0.1:5060"

[bridge]
ws_url = "wss://x/y"

[[trunk]]
name = "broken"
peer_addrs = ["not-an-ip"]

[[route]]
name = "default"
[route.match]
any = true
"#;
    let err = load_from_str_with_env(toml, &env).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("broken") && msg.contains("not-an-ip"),
        "got: {msg}"
    );
}

#[test]
fn duplicate_trunk_names_rejected() {
    let env = MapEnv::new([]);
    let toml = r#"
[sip]
listen = "127.0.0.1:5060"

[bridge]
ws_url = "wss://x/y"

[[trunk]]
name = "twice"
peer_addrs = ["10.0.0.1"]

[[trunk]]
name = "twice"
peer_addrs = ["10.0.0.2"]

[[route]]
name = "default"
[route.match]
any = true
"#;
    let err = load_from_str_with_env(toml, &env).unwrap_err();
    assert!(err.to_string().contains("twice"));
}

#[test]
fn trunk_cidr_contains_v4() {
    use siphon_ai_config::TrunkCidr;
    let cidr = TrunkCidr::parse("10.0.0.0/24").unwrap();
    assert!(cidr.contains("10.0.0.5".parse().unwrap()));
    assert!(cidr.contains("10.0.0.255".parse().unwrap()));
    assert!(!cidr.contains("10.0.1.0".parse().unwrap()));
    let exact = TrunkCidr::parse("203.0.113.10").unwrap();
    assert_eq!(exact.prefix_len, 32);
    assert!(exact.contains("203.0.113.10".parse().unwrap()));
    assert!(!exact.contains("203.0.113.11".parse().unwrap()));
}

#[test]
fn trunk_cidr_contains_v6() {
    use siphon_ai_config::TrunkCidr;
    let cidr = TrunkCidr::parse("2001:db8::/32").unwrap();
    assert!(cidr.contains("2001:db8:1::1".parse().unwrap()));
    assert!(!cidr.contains("2001:db9::1".parse().unwrap()));
    // v4 ↔ v6 never match.
    assert!(!cidr.contains("10.0.0.1".parse().unwrap()));
}

#[test]
fn dtmf_off_disables_payload_type() {
    let env = MapEnv::new([]);
    let toml = r#"
[sip]
listen = "127.0.0.1:5060"

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
listen = "127.0.0.1:5060"

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
listen = "127.0.0.1:5060"

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
listen = "127.0.0.1:5060"

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
listen = "127.0.0.1:5060"

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
listen = "127.0.0.1:5060"

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
listen = "127.0.0.1:5060"

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
listen = "127.0.0.1:5060"

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
    // No `format` key → JSONL, the pre-0.36.0 behaviour.
    assert_eq!(file.format, siphon_ai_config::CdrFileFormat::Jsonl);
    assert!(cfg.cdr.webhook.is_none());
}

#[test]
fn cdr_file_format_csv_compiles_and_bad_value_errors() {
    let env = MapEnv::new([]);
    let toml = r#"
[sip]
listen = "127.0.0.1:5060"

[bridge]
ws_url = "wss://x/y"

[cdr]
enabled = true
[cdr.file]
enabled = true
path = "/var/log/siphon-ai/cdr.csv"
format = "csv"

[[route]]
name = "d"
[route.match]
any = true
"#;
    let cfg = load_from_str_with_env(toml, &env).unwrap();
    let file = cfg.cdr.file.expect("file sink configured");
    assert_eq!(file.format, siphon_ai_config::CdrFileFormat::Csv);

    // Unknown format value fails at load time, naming the variants.
    let bad = toml.replace("format = \"csv\"", "format = \"xml\"");
    let err = load_from_str_with_env(&bad, &env).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("unknown variant") && msg.contains("csv"),
        "unhelpful error: {msg}"
    );
}

#[test]
fn cdr_webhook_sink_compiles_with_url_and_auth() {
    let env = MapEnv::new([("CDR_TOKEN", "tok-123"), ("CDR_SECRET", "whsec-cdr")]);
    let toml = r#"
[sip]
listen = "127.0.0.1:5060"

[bridge]
ws_url = "wss://x/y"

[cdr]
enabled = true
[cdr.webhook]
enabled = true
url = "https://billing.example.com/cdr"
auth_header = "Bearer ${CDR_TOKEN}"
secret = "${CDR_SECRET}"
spool_dir = "/var/lib/siphon-ai/spool/cdr"
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
    // Signing secret arrives env-expanded.
    assert_eq!(w.secret.as_deref(), Some("whsec-cdr"));
    assert_eq!(w.spool_dir.as_deref(), Some("/var/lib/siphon-ai/spool/cdr"));
    assert_eq!(w.retry_max, 5);
    assert_eq!(w.timeout, std::time::Duration::from_millis(7500));
}

#[test]
fn cdr_file_enabled_without_path_errors() {
    let env = MapEnv::new([]);
    let toml = r#"
[sip]
listen = "127.0.0.1:5060"

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
listen = "127.0.0.1:5060"

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
listen = "127.0.0.1:5060"

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
fn register_blocks_default_to_empty() {
    let env = MapEnv::new([]);
    let toml = r#"
[sip]
listen = "127.0.0.1:5060"

[bridge]
ws_url = "wss://x/y"

[[route]]
name = "d"
[route.match]
any = true
"#;
    let cfg = load_from_str_with_env(toml, &env).unwrap();
    assert!(cfg.registrations.is_empty());
}

#[test]
fn register_block_compiles_with_required_fields() {
    let env = MapEnv::new([("CUCM_PASS", "hunter2")]);
    let toml = r#"
[sip]
listen = "127.0.0.1:5060"

[bridge]
ws_url = "wss://x/y"

[[register]]
name = "cucm-main"
server = "10.20.30.40"
username = "ai-receptionist"
password = "${CUCM_PASS}"

[[route]]
name = "d"
[route.match]
any = true
"#;
    let cfg = load_from_str_with_env(toml, &env).unwrap();
    assert_eq!(cfg.registrations.len(), 1);
    let r = &cfg.registrations[0];
    assert_eq!(r.name, "cucm-main");
    assert_eq!(r.server_addr.ip().to_string(), "10.20.30.40");
    assert_eq!(r.server_addr.port(), 5060);
    assert_eq!(r.username, "ai-receptionist");
    assert_eq!(r.auth_username, "ai-receptionist");
    assert_eq!(r.password, "hunter2");
    assert_eq!(r.expires.as_secs(), 3600);
    assert!(r.register_on_startup);
}

#[test]
fn register_block_picks_default_port_per_transport() {
    let env = MapEnv::new([]);
    let toml = r#"
[sip]
listen = "127.0.0.1:5060"

[bridge]
ws_url = "wss://x/y"

[[register]]
name = "tls-trunk"
server = "10.0.0.5"
transport = "tls"
username = "u"
password = "p"

[[route]]
name = "d"
[route.match]
any = true
"#;
    let cfg = load_from_str_with_env(toml, &env).unwrap();
    assert_eq!(cfg.registrations[0].server_addr.port(), 5061);
}

#[test]
fn register_explicit_port_overrides_default() {
    let env = MapEnv::new([]);
    let toml = r#"
[sip]
listen = "127.0.0.1:5060"

[bridge]
ws_url = "wss://x/y"

[[register]]
name = "trunk"
server = "10.0.0.5"
port = 5072
username = "u"
password = "p"

[[route]]
name = "d"
[route.match]
any = true
"#;
    let cfg = load_from_str_with_env(toml, &env).unwrap();
    assert_eq!(cfg.registrations[0].server_addr.port(), 5072);
}

#[test]
fn register_duplicate_name_errors() {
    let env = MapEnv::new([]);
    let toml = r#"
[sip]
listen = "127.0.0.1:5060"

[bridge]
ws_url = "wss://x/y"

[[register]]
name = "trunk"
server = "10.0.0.5"
username = "u"
password = "p"

[[register]]
name = "trunk"
server = "10.0.0.6"
username = "u2"
password = "p2"

[[route]]
name = "d"
[route.match]
any = true
"#;
    let err = load_from_str_with_env(toml, &env).unwrap_err();
    assert!(err.to_string().contains("share name"));
}

#[test]
fn register_hostname_server_errors_in_v1() {
    // v1 only accepts literal IPs — DNS-resolved registrars are
    // a v1.1 feature.
    let env = MapEnv::new([]);
    let toml = r#"
[sip]
listen = "127.0.0.1:5060"

[bridge]
ws_url = "wss://x/y"

[[register]]
name = "trunk"
server = "cucm.example.com"
username = "u"
password = "p"

[[route]]
name = "d"
[route.match]
any = true
"#;
    let err = load_from_str_with_env(toml, &env).unwrap_err();
    assert!(err.to_string().contains("literal IP"));
}

#[test]
fn register_unknown_transport_errors() {
    let env = MapEnv::new([]);
    let toml = r#"
[sip]
listen = "127.0.0.1:5060"

[bridge]
ws_url = "wss://x/y"

[[register]]
name = "trunk"
server = "10.0.0.5"
transport = "smoke"
username = "u"
password = "p"

[[route]]
name = "d"
[route.match]
any = true
"#;
    let err = load_from_str_with_env(toml, &env).unwrap_err();
    assert!(err.to_string().contains("smoke"));
}

#[test]
fn register_auth_username_defaults_to_username() {
    let env = MapEnv::new([]);
    let toml = r#"
[sip]
listen = "127.0.0.1:5060"

[bridge]
ws_url = "wss://x/y"

[[register]]
name = "trunk"
server = "10.0.0.5"
username = "alice"
password = "p"

[[route]]
name = "d"
[route.match]
any = true
"#;
    let cfg = load_from_str_with_env(toml, &env).unwrap();
    assert_eq!(cfg.registrations[0].auth_username, "alice");
}

#[test]
fn sip_tls_disabled_by_default() {
    let env = MapEnv::new([]);
    let toml = r#"
[sip]
listen = "127.0.0.1:5060"

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
listen = "127.0.0.1:5060"
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
listen = "127.0.0.1:5060"
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
listen = "127.0.0.1:5060"
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
listen = "127.0.0.1:5060"
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
listen = "127.0.0.1:5060"
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
listen = "127.0.0.1:5060"

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
    let env = MapEnv::new([
        ("WEBHOOK_TOKEN", "wh-xyz"),
        ("WEBHOOK_SECRET", "whsec-life"),
    ]);
    let toml = r#"
[sip]
listen = "127.0.0.1:5060"

[bridge]
ws_url = "wss://x/y"

[webhooks]
enabled = true
url = "https://ops.example.com/siphon-events"
auth_header = "Bearer ${WEBHOOK_TOKEN}"
secret = "${WEBHOOK_SECRET}"
spool_dir = "/var/lib/siphon-ai/spool/webhooks"
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
    // Signing secret arrives env-expanded.
    assert_eq!(cfg.webhooks.secret.as_deref(), Some("whsec-life"));
    assert_eq!(
        cfg.webhooks.spool_dir.as_deref(),
        Some("/var/lib/siphon-ai/spool/webhooks")
    );
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
listen = "127.0.0.1:5060"

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
listen = "127.0.0.1:5060"

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
listen = "127.0.0.1:5060"

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
listen = "127.0.0.1:5060"

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
listen = "127.0.0.1:5060"

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
listen = "127.0.0.1:5060"

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
    // gets a sensible default. The bind has to be a real IP (not
    // 0.0.0.0) because the SDP answer needs a routable
    // [node].public_address — see
    // CompileError::PublicAddressRequiredForWildcardListen.
    let env = MapEnv::new([]);
    let toml = r#"
[sip]
listen = "127.0.0.1:5060"

[bridge]
ws_url = "wss://x/y"
"#;
    let cfg = load_from_str_with_env(toml, &env).unwrap();
    assert_eq!(cfg.node.id, "siphon-ai");
    assert_eq!(cfg.node.public_address, "127.0.0.1");
    assert_eq!(cfg.sip.transports, vec![SipTransport::Udp]);
    assert_eq!(cfg.bridge_defaults.codecs, vec![Codec::Pcmu, Codec::Pcma]);
    assert_eq!(cfg.bridge_defaults.dtmf_payload_type, Some(101));
    assert_eq!(cfg.bridge_defaults.connect_timeout, Duration::from_secs(5));
    assert!(cfg.bridge_defaults.forward_headers.is_empty());
}

#[test]
fn wildcard_listen_without_public_address_is_rejected() {
    // The bug this guards: binding 0.0.0.0 with no
    // [node].public_address would otherwise produce an SDP
    // answer with c=IN IP4 0.0.0.0, which RTP can't route to.
    // Refuse loudly at config compile rather than silently
    // shipping a misconfig to prod.
    let env = MapEnv::new([]);
    let toml = r#"
[sip]
listen = "0.0.0.0:5060"

[bridge]
ws_url = "wss://x/y"
"#;
    let err = load_from_str_with_env(toml, &env).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("public_address") && msg.contains("unspecified"),
        "expected the new wildcard-bind error, got: {msg}"
    );
}

#[test]
fn wildcard_listen_with_public_address_works() {
    // Same bind, but with public_address spelled out — fine.
    let env = MapEnv::new([]);
    let toml = r#"
[node]
public_address = "203.0.113.4"

[sip]
listen = "0.0.0.0:5060"

[bridge]
ws_url = "wss://x/y"
"#;
    let cfg = load_from_str_with_env(toml, &env).unwrap();
    assert_eq!(cfg.node.public_address, "203.0.113.4");
}

#[test]
fn ipv6_wildcard_listen_without_public_address_is_rejected() {
    // Mirror of the v4 case: `::` is also unspecified.
    let env = MapEnv::new([]);
    let toml = r#"
[sip]
listen = "[::]:5060"

[bridge]
ws_url = "wss://x/y"
"#;
    let err = load_from_str_with_env(toml, &env).unwrap_err();
    assert!(err.to_string().contains("public_address"));
}

// ─── [[gateway]] transport selection (outbound TLS, PR #163) ──────

#[test]
fn gateway_transport_tls_defaults_port_5061_and_adds_uri_param() {
    let env = MapEnv::new([]);
    let toml = r#"
[sip]
listen = "127.0.0.1:5060"

[bridge]
ws_url = "wss://x/y"

[outbound]
max_concurrent = 2

[[gateway]]
name = "twilio"
proxy = "siphon.pstn.twilio.com"
transport = "tls"
from = "sip:+15551230000@siphon.pstn.twilio.com"
"#;
    let cfg = load_from_str_with_env(toml, &env).unwrap();
    let gw = cfg.outbound.gateway("twilio").expect("gateway compiled");
    assert_eq!(gw.transport, SipTransport::Tls);
    // TLS flips the default proxy port to 5061 (SIPS standard).
    assert_eq!(gw.proxy_port, 5061);
    assert_eq!(
        gw.request_uri("+15183217034"),
        "sip:+15183217034@siphon.pstn.twilio.com:5061;transport=tls"
    );
}

#[test]
fn gateway_transport_defaults_to_udp_with_no_uri_param() {
    let env = MapEnv::new([]);
    let toml = r#"
[sip]
listen = "127.0.0.1:5060"

[bridge]
ws_url = "wss://x/y"

[outbound]
max_concurrent = 2

[[gateway]]
name = "pbx"
proxy = "10.0.0.5"
from = "sip:bot@10.0.0.5"
"#;
    let cfg = load_from_str_with_env(toml, &env).unwrap();
    let gw = cfg.outbound.gateway("pbx").expect("gateway compiled");
    assert_eq!(gw.transport, SipTransport::Udp);
    assert_eq!(gw.proxy_port, 5060);
    // UDP is the RFC 3263 default; no param churn on existing configs.
    assert_eq!(gw.request_uri("7001"), "sip:7001@10.0.0.5:5060");
}

#[test]
fn gateway_unknown_transport_is_rejected() {
    let env = MapEnv::new([]);
    let toml = r#"
[sip]
listen = "127.0.0.1:5060"

[bridge]
ws_url = "wss://x/y"

[[gateway]]
name = "bad"
proxy = "10.0.0.5"
transport = "sctp"
from = "sip:bot@10.0.0.5"
"#;
    let err = load_from_str_with_env(toml, &env).unwrap_err();
    assert!(err.to_string().contains("sctp"), "got: {err}");
}

#[test]
fn gateway_transport_conflicts_with_register_reuse() {
    // With `register` set the transport is inherited; an explicit
    // `transport` is a conflict, not an override.
    let env = MapEnv::new([]);
    let toml = r#"
[sip]
listen = "127.0.0.1:5060"

[bridge]
ws_url = "wss://x/y"

[[register]]
name = "pbx-main"
server = "10.0.0.5"
transport = "tls"
username = "bot"
password = "hunter2"

[[gateway]]
name = "via-pbx"
register = "pbx-main"
transport = "udp"
"#;
    let err = load_from_str_with_env(toml, &env).unwrap_err();
    assert!(
        err.to_string()
            .contains("inherited from the register block"),
        "got: {err}"
    );
}

#[test]
fn gateway_register_reuse_inherits_transport() {
    let env = MapEnv::new([]);
    let toml = r#"
[sip]
listen = "127.0.0.1:5060"

[bridge]
ws_url = "wss://x/y"

[[register]]
name = "pbx-main"
server = "10.0.0.5"
transport = "tls"
username = "bot"
password = "hunter2"

[[gateway]]
name = "via-pbx"
register = "pbx-main"
"#;
    let cfg = load_from_str_with_env(toml, &env).unwrap();
    let gw = cfg.outbound.gateway("via-pbx").expect("gateway compiled");
    assert_eq!(gw.transport, SipTransport::Tls);
    assert_eq!(gw.proxy_port, 5061);
    assert!(gw.request_uri("100").ends_with(";transport=tls"));
}

// ─── [sip.tls_client] — outgoing TLS verification roots ───────────

#[test]
fn tls_client_extra_ca_missing_file_is_rejected_at_load() {
    let env = MapEnv::new([]);
    let toml = r#"
[sip]
listen = "127.0.0.1:5060"

[sip.tls_client]
extra_ca = "/nonexistent/ca.pem"

[bridge]
ws_url = "wss://x/y"
"#;
    let err = load_from_str_with_env(toml, &env).unwrap_err();
    assert!(err.to_string().contains("extra_ca"), "got: {err}");
}

#[test]
fn tls_client_extra_ca_existing_file_compiles() {
    let env = MapEnv::new([]);
    let dir = std::env::temp_dir().join(format!("siphon-ai-ca-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let ca = dir.join("ca.pem");
    std::fs::write(&ca, "not validated at compile time, only at startup").unwrap();
    let toml = format!(
        r#"
[sip]
listen = "127.0.0.1:5060"

[sip.tls_client]
extra_ca = "{}"

[bridge]
ws_url = "wss://x/y"
"#,
        ca.display()
    );
    let cfg = load_from_str_with_env(&toml, &env).unwrap();
    assert_eq!(cfg.sip.tls_client_extra_ca.as_deref(), Some(ca.as_path()));
    let _ = std::fs::remove_dir_all(&dir);
}

// ─── [conference] — conference rooms (0.7.0) ──────────────────────

#[test]
fn conference_defaults_off_with_documented_caps() {
    // A config with no [conference] block at all (every 0.6.x
    // config) compiles to the fail-closed defaults.
    let env = MapEnv::new([]);
    let toml = r#"
[sip]
listen = "127.0.0.1:5060"

[bridge]
ws_url = "wss://x/y"
"#;
    let cfg = load_from_str_with_env(toml, &env).unwrap();
    assert!(!cfg.conference.enabled);
    assert_eq!(cfg.conference.max_rooms, 16);
    assert_eq!(cfg.conference.max_participants_per_room, 8);
    assert!(!cfg.conference.join_tones);
}

#[test]
fn conference_block_compiles_with_overrides() {
    let env = MapEnv::new([]);
    let toml = r#"
[sip]
listen = "127.0.0.1:5060"

[bridge]
ws_url = "wss://x/y"

[conference]
enabled = true
max_rooms = 4
max_participants_per_room = 3
join_tones = true
"#;
    let cfg = load_from_str_with_env(toml, &env).unwrap();
    assert!(cfg.conference.enabled);
    assert_eq!(cfg.conference.max_rooms, 4);
    assert_eq!(cfg.conference.max_participants_per_room, 3);
    assert!(cfg.conference.join_tones);
}

#[test]
fn conference_zero_max_rooms_is_rejected_at_load() {
    let env = MapEnv::new([]);
    let toml = r#"
[sip]
listen = "127.0.0.1:5060"

[bridge]
ws_url = "wss://x/y"

[conference]
enabled = true
max_rooms = 0
"#;
    let err = load_from_str_with_env(toml, &env).unwrap_err();
    assert!(
        err.to_string().contains("max_rooms"),
        "unexpected error: {err}"
    );
}

#[test]
fn conference_single_call_room_cap_is_rejected_at_load() {
    // max_participants_per_room = 1 can never conference; fail loud
    // at load (CLAUDE.md §4.6), not silently at first join.
    let env = MapEnv::new([]);
    let toml = r#"
[sip]
listen = "127.0.0.1:5060"

[bridge]
ws_url = "wss://x/y"

[conference]
max_participants_per_room = 1
"#;
    let err = load_from_str_with_env(toml, &env).unwrap_err();
    assert!(
        err.to_string().contains("max_participants_per_room"),
        "unexpected error: {err}"
    );
}

#[test]
fn conference_unknown_key_is_rejected() {
    // deny_unknown_fields: typos must not silently no-op a cap.
    let env = MapEnv::new([]);
    let toml = r#"
[sip]
listen = "127.0.0.1:5060"

[bridge]
ws_url = "wss://x/y"

[conference]
max_partcipants_per_room = 4
"#;
    assert!(load_from_str_with_env(toml, &env).is_err());
}

// ─── [park] — media-only call park (0.7.0) ────────────────────────

#[test]
fn park_defaults_off() {
    let env = MapEnv::new([]);
    let toml = r#"
[sip]
listen = "127.0.0.1:5060"

[bridge]
ws_url = "wss://x/y"
"#;
    let cfg = load_from_str_with_env(toml, &env).unwrap();
    assert!(!cfg.park.enabled);
    assert_eq!(cfg.park.max_parked, 32);
    assert_eq!(cfg.park.timeout, Some(Duration::from_secs(300)));
    assert!(cfg.park.moh_file.is_none());
}

#[test]
fn park_block_compiles_with_overrides() {
    let env = MapEnv::new([]);
    let toml = r#"
[sip]
listen = "127.0.0.1:5060"

[bridge]
ws_url = "wss://x/y"

[park]
enabled = true
timeout_secs = 0
timeout_action = "keep"
max_parked = 4
"#;
    let cfg = load_from_str_with_env(toml, &env).unwrap();
    assert!(cfg.park.enabled);
    assert_eq!(cfg.park.timeout, None);
    assert!(matches!(
        cfg.park.timeout_action,
        siphon_ai_config::ParkTimeoutAction::Keep
    ));
    assert_eq!(cfg.park.max_parked, 4);
}

#[test]
fn park_bad_timeout_action_is_rejected() {
    let env = MapEnv::new([]);
    let toml = r#"
[sip]
listen = "127.0.0.1:5060"

[bridge]
ws_url = "wss://x/y"

[park]
enabled = true
timeout_action = "explode"
"#;
    let err = load_from_str_with_env(toml, &env).unwrap_err();
    assert!(err.to_string().contains("timeout_action"), "got: {err}");
}

// ─── [shutdown] graceful drain (0.17.0) ──────────────────────────

#[test]
fn shutdown_defaults_to_30s_drain_when_block_omitted() {
    let env = MapEnv::new([]);
    let toml = r#"
[sip]
listen = "127.0.0.1:5060"

[bridge]
ws_url = "wss://x/y"
"#;
    let cfg = load_from_str_with_env(toml, &env).unwrap();
    assert_eq!(cfg.shutdown.drain_timeout, Some(Duration::from_secs(30)));
}

#[test]
fn shutdown_zero_means_no_drain() {
    let env = MapEnv::new([]);
    let toml = r#"
[sip]
listen = "127.0.0.1:5060"

[bridge]
ws_url = "wss://x/y"

[shutdown]
drain_timeout_secs = 0
"#;
    let cfg = load_from_str_with_env(toml, &env).unwrap();
    assert_eq!(cfg.shutdown.drain_timeout, None);
}

#[test]
fn shutdown_explicit_timeout_is_honored() {
    let env = MapEnv::new([]);
    let toml = r#"
[sip]
listen = "127.0.0.1:5060"

[bridge]
ws_url = "wss://x/y"

[shutdown]
drain_timeout_secs = 90
"#;
    let cfg = load_from_str_with_env(toml, &env).unwrap();
    assert_eq!(cfg.shutdown.drain_timeout, Some(Duration::from_secs(90)));
}

#[test]
fn shutdown_unknown_field_is_rejected() {
    let env = MapEnv::new([]);
    let toml = r#"
[sip]
listen = "127.0.0.1:5060"

[bridge]
ws_url = "wss://x/y"

[shutdown]
drain_timeout_secs = 30
drian_timeout_secs = 10
"#;
    let err = load_from_str_with_env(toml, &env).unwrap_err();
    assert!(err.to_string().contains("drian_timeout_secs"), "got: {err}");
}

#[test]
fn park_zero_max_parked_is_rejected() {
    let env = MapEnv::new([]);
    let toml = r#"
[sip]
listen = "127.0.0.1:5060"

[bridge]
ws_url = "wss://x/y"

[park]
enabled = true
max_parked = 0
"#;
    let err = load_from_str_with_env(toml, &env).unwrap_err();
    assert!(err.to_string().contains("max_parked"), "got: {err}");
}

#[test]
fn park_missing_moh_file_is_rejected_when_enabled() {
    let env = MapEnv::new([]);
    let toml = r#"
[sip]
listen = "127.0.0.1:5060"

[bridge]
ws_url = "wss://x/y"

[park]
enabled = true
moh_file = "/nonexistent/hold.wav"
"#;
    let err = load_from_str_with_env(toml, &env).unwrap_err();
    assert!(err.to_string().contains("moh_file"), "got: {err}");
}

#[test]
fn park_unknown_key_is_rejected() {
    let env = MapEnv::new([]);
    let toml = r#"
[sip]
listen = "127.0.0.1:5060"

[bridge]
ws_url = "wss://x/y"

[park]
timeut_secs = 60
"#;
    assert!(load_from_str_with_env(toml, &env).is_err());
}

#[test]
fn metrics_token_unset_set_and_empty() {
    // 0.35.0 (DESIGN_METRICS_AUTH.md): unset = open endpoint (the
    // default); set = gate configured; empty after expansion = load
    // error (an accidentally-open gate, not a policy).
    let env = MapEnv::new([("MT", "scrape-secret")]);
    let base = r#"
[sip]
listen = "127.0.0.1:5060"
[bridge]
ws_url = "wss://x/y"
[observability]
enabled = true
http_listen = "127.0.0.1:9090"
METRICS_TOKEN
[[route]]
name = "default"
[route.match]
any = true
"#;

    let cfg = load_from_str_with_env(&base.replace("METRICS_TOKEN\n", ""), &env).expect("open");
    assert_eq!(cfg.observability.metrics_token, None);

    let cfg = load_from_str_with_env(
        &base.replace("METRICS_TOKEN", r#"metrics_token = "${MT}""#),
        &env,
    )
    .expect("gated");
    assert_eq!(
        cfg.observability.metrics_token.as_deref(),
        Some("scrape-secret")
    );

    let msg = load_from_str_with_env(
        &base.replace("METRICS_TOKEN", r#"metrics_token = """#),
        &env,
    )
    .unwrap_err()
    .to_string();
    assert!(
        msg.contains("metrics_token"),
        "expected empty-token rejection, got: {msg}"
    );
}
