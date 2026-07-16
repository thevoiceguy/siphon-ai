//! Read-only config inspection for the `print-config` and `route-test`
//! subcommands (config CLI chunk 2).
//!
//! Both walk an already-compiled [`Config`] and render text — no
//! sockets, no runtime. `print-config` shows the *effective* config
//! (post-`${VAR}`, post-merge) with secrets redacted by default;
//! `route-test` runs the dialplan against synthetic call attributes
//! and reports the winning route + its effective bridge config.
//!
//! The renderer is a bespoke text walker rather than a serde dump: the
//! compiled graph holds non-serde types (compiled `Regex`, hashed admin
//! tokens, `SocketAddr`) and we want secret redaction, not a faithful
//! round-trip.

use std::fmt::Write as _;

use serde_json::{json, Value};
use siphon_ai_config::{Config, RecordingMode, SipTransport};
use siphon_ai_routes::{CallInfo, Headers};

/// Inputs to `route-test`, collected from CLI flags.
pub struct RouteTestInput {
    pub ruri_user: String,
    pub ruri_host: String,
    pub to_user: String,
    pub to_host: String,
    pub from_user: String,
    pub from_host: String,
    pub register_source: String,
    /// `("X-Header", "value")` pairs.
    pub headers: Vec<(String, String)>,
}

/// `<unset>` / `<redacted>` / the value, depending on `show`.
fn secret(opt: &Option<String>, show: bool) -> String {
    match opt {
        None => "<unset>".into(),
        Some(_) if !show => "<redacted>".into(),
        Some(v) => v.clone(),
    }
}

/// Present a plain optional string (no redaction).
fn opt(o: &Option<String>) -> String {
    o.clone().unwrap_or_else(|| "<unset>".into())
}

/// JSON flavour of [`secret`]: `null` when unset, `"<redacted>"` when
/// set but hidden, the value when `show`.
fn secret_json(opt: &Option<String>, show: bool) -> Value {
    match opt {
        None => Value::Null,
        Some(_) if !show => json!("<redacted>"),
        Some(v) => json!(v),
    }
}

/// A required (non-optional) secret string, e.g. a register/gateway
/// password.
fn secret_value(v: &str, show: bool) -> Value {
    if show {
        json!(v)
    } else {
        json!("<redacted>")
    }
}

fn transport_str(t: &SipTransport) -> &'static str {
    match t {
        SipTransport::Udp => "udp",
        SipTransport::Tcp => "tcp",
        SipTransport::Tls => "tls",
    }
}

/// Render the effective compiled config as human-readable text.
/// Secrets are redacted unless `show_secrets`.
pub fn render_config(config: &Config, show_secrets: bool) -> String {
    let mut o = String::new();
    let s = &mut o;

    let _ = writeln!(s, "# Effective configuration (compiled, post-${{VAR}}).");
    if !show_secrets {
        let _ = writeln!(s, "# Secrets redacted — pass --show-secrets to reveal.");
    }
    let _ = writeln!(s);

    // [node]
    let _ = writeln!(s, "[node]");
    let _ = writeln!(s, "  id             = {}", config.node.id);
    let _ = writeln!(s, "  public_address = {}", config.node.public_address);

    // [sip]
    let transports = config
        .sip
        .transports
        .iter()
        .map(transport_str)
        .collect::<Vec<_>>()
        .join(", ");
    let _ = writeln!(s, "[sip]");
    let _ = writeln!(s, "  listen             = {}", config.sip.listen_addr);
    let _ = writeln!(s, "  transports         = [{transports}]");
    let _ = writeln!(
        s,
        "  tls                = {}",
        if config.sip.tls.is_some() {
            "on"
        } else {
            "off"
        }
    );
    let _ = writeln!(
        s,
        "  allow_delayed_offer = {}",
        config.sip.allow_delayed_offer
    );

    // [media]
    let _ = writeln!(s, "[media]");
    let _ = writeln!(s, "  srtp           = {:?}", config.media.srtp);
    let _ = writeln!(
        s,
        "  rtp_port_range = {}",
        match config.media.rtp_port_range {
            Some((lo, hi)) => format!("{lo}-{hi}"),
            None => "<forge default>".into(),
        }
    );
    let _ = writeln!(
        s,
        "  moh_file       = {}",
        config
            .media
            .moh_file
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "<comfort noise>".into())
    );

    // [bridge] defaults
    let b = &config.bridge_defaults;
    let _ = writeln!(s, "[bridge] (defaults)");
    let _ = writeln!(s, "  ws_url         = {}", opt(&b.ws_url));
    let _ = writeln!(
        s,
        "  auth_header    = {}",
        secret(&b.auth_header, show_secrets)
    );
    let _ = writeln!(s, "  codecs         = {:?}", b.codecs);
    let _ = writeln!(s, "  barge_in       = {:?}", b.barge_in.mode);
    let _ = writeln!(
        s,
        "  forward_headers = {}",
        if b.forward_headers.is_empty() {
            "[]".into()
        } else {
            format!("{:?}", b.forward_headers)
        }
    );

    // [[route]]
    let _ = writeln!(
        s,
        "routes ({}, default route: {})",
        config.routes.len(),
        if config.routes.has_default() {
            "yes"
        } else {
            "NO"
        }
    );
    for (i, r) in config.routes.iter().enumerate() {
        let _ = writeln!(s, "  [{i}] {}", r.name);
        // Per-route overrides that are actually set (None = inherits).
        if let Some(u) = &r.bridge.ws_url {
            let _ = writeln!(s, "        ws_url       -> {u}");
        }
        if r.bridge.ws_auth_header.is_some() {
            let _ = writeln!(
                s,
                "        ws_auth_header -> {}",
                secret(&r.bridge.ws_auth_header, show_secrets)
            );
        }
        if let Some(c) = &r.media.codecs {
            let _ = writeln!(s, "        codecs       -> {c:?}");
        }
        if let Some(m) = &r.media.srtp {
            let _ = writeln!(s, "        srtp         -> {m}");
        }
        if let Some(m) = &r.recording.mode {
            let _ = writeln!(s, "        recording    -> {m}");
        }
        if let Some(a) = &r.security.min_attestation {
            let _ = writeln!(s, "        min_attestation -> {a}");
        }
        if r.bridge_tls.is_some() {
            let _ = writeln!(s, "        bridge.tls   -> set (overrides global)");
        }
    }

    // [[register]]
    if !config.registrations.is_empty() {
        let _ = writeln!(s, "registrations ({})", config.registrations.len());
        for reg in &config.registrations {
            let _ = writeln!(
                s,
                "  {} -> {} [{}] user={} password={} expires={}s on_startup={}",
                reg.name,
                reg.server_host,
                transport_str(&reg.transport),
                reg.username,
                if show_secrets {
                    &reg.password
                } else {
                    "<redacted>"
                },
                reg.expires.as_secs(),
                reg.register_on_startup,
            );
        }
    }

    // [[trunk]]
    if !config.trunks.is_empty() {
        let _ = writeln!(s, "trunks ({})", config.trunks.len());
        for t in &config.trunks {
            let _ = writeln!(
                s,
                "  {} peer_addrs={} from_hosts={:?}",
                t.name,
                t.peer_addrs.len(),
                t.from_hosts
            );
        }
    } else {
        let _ = writeln!(s, "trunks: none (accepts INVITEs from any source)");
    }

    // [security]
    let _ = writeln!(s, "[security]");
    let _ = writeln!(
        s,
        "  stir_shaken    = {}",
        if config.security.stir_shaken.enabled {
            "on"
        } else {
            "off"
        }
    );
    let _ = writeln!(
        s,
        "  min_attestation = {:?}",
        config.security.min_attestation
    );

    // [recording]
    let _ = writeln!(s, "[recording]");
    let _ = writeln!(s, "  mode           = {:?}", config.recording.mode);
    if !matches!(config.recording.mode, RecordingMode::Off) {
        let _ = writeln!(s, "  dir            = {}", config.recording.dir.display());
    }

    // [outbound] + [[gateway]]
    let _ = writeln!(s, "[outbound]");
    let _ = writeln!(s, "  max_concurrent = {}", config.outbound.max_concurrent);
    let _ = writeln!(
        s,
        "  rate_limit/s   = {}",
        config
            .outbound
            .rate_limit_per_sec
            .map(|n| n.to_string())
            .unwrap_or_else(|| "<none>".into())
    );
    for g in &config.outbound.gateways {
        let creds = match &g.credentials {
            None => "none".to_string(),
            Some(c) => format!(
                "user={} password={}",
                c.username,
                if show_secrets {
                    &c.password
                } else {
                    "<redacted>"
                }
            ),
        };
        let _ = writeln!(
            s,
            "  gateway {} -> {}:{} [{}] from={} srtp={:?} creds=({creds})",
            g.name,
            g.proxy_host,
            g.proxy_port,
            transport_str(&g.transport),
            g.from,
            g.srtp,
        );
    }

    // [conference] / [park]
    let _ = writeln!(
        s,
        "[conference] enabled={} max_rooms={} max_participants_per_room={}",
        config.conference.enabled,
        config.conference.max_rooms,
        config.conference.max_participants_per_room
    );
    let _ = writeln!(
        s,
        "[park] enabled={} max_parked={} timeout={} action={:?}",
        config.park.enabled,
        config.park.max_parked,
        config
            .park
            .timeout
            .map(|d| format!("{}s", d.as_secs()))
            .unwrap_or_else(|| "<none>".into()),
        config.park.timeout_action,
    );

    // [cdr]
    let _ = writeln!(s, "[cdr] enabled={}", config.cdr.enabled);
    if let Some(f) = &config.cdr.file {
        let _ = writeln!(s, "  file    -> {}", f.path.display());
    }
    if let Some(w) = &config.cdr.webhook {
        let _ = writeln!(
            s,
            "  webhook -> {} auth_header={} secret={} spool_dir={}",
            w.url,
            secret(&w.auth_header, show_secrets),
            secret(&w.secret, show_secrets),
            opt(&w.spool_dir),
        );
    }

    // [webhooks]
    let _ = writeln!(s, "[webhooks] enabled={}", config.webhooks.enabled);
    if config.webhooks.enabled {
        let _ = writeln!(
            s,
            "  url={} auth_header={} secret={} spool_dir={} events={:?}",
            opt(&config.webhooks.url),
            secret(&config.webhooks.auth_header, show_secrets),
            secret(&config.webhooks.secret, show_secrets),
            opt(&config.webhooks.spool_dir),
            config.webhooks.events,
        );
    }

    // [audit]
    let _ = writeln!(
        s,
        "[audit] enabled={} events={:?}",
        config.audit.enabled, config.audit.events
    );
    if let Some(f) = &config.audit.file {
        let _ = writeln!(s, "  file    -> {}", f.path.display());
    }
    if let Some(w) = &config.audit.webhook {
        let _ = writeln!(
            s,
            "  webhook -> {} auth_header={} secret={} spool_dir={}",
            w.url,
            secret(&w.auth_header, show_secrets),
            secret(&w.secret, show_secrets),
            opt(&w.spool_dir),
        );
    }

    // [observability]
    let _ = writeln!(
        s,
        "[observability] enabled={} http_listen={}",
        config.observability.enabled,
        config
            .observability
            .http_listen
            .map(|a| a.to_string())
            .unwrap_or_else(|| "<unset>".into()),
    );

    // [admin]
    match &config.admin {
        None => {
            let _ = writeln!(s, "[admin] not configured (/admin/* not served)");
        }
        Some(a) => {
            let _ = writeln!(
                s,
                "[admin] listen={} tokens={}",
                a.listen_addr,
                a.auth.len()
            );
            for t in a.auth.iter() {
                let _ = writeln!(s, "  token {} role={}", t.name, t.role.as_str());
            }
        }
    }

    // [hep]
    let _ = writeln!(s, "[hep] enabled={}", config.hep.enabled);
    if config.hep.enabled {
        let _ = writeln!(
            s,
            "  collector={} capture_id={} capture_password={}",
            config
                .hep
                .collector
                .map(|a| a.to_string())
                .unwrap_or_else(|| "<unset>".into()),
            config
                .hep
                .capture_id
                .map(|i| i.to_string())
                .unwrap_or_else(|| "<unset>".into()),
            secret(&config.hep.capture_password, show_secrets),
        );
    }

    o
}

/// Render the effective compiled config as pretty-printed JSON — the
/// same sections and redaction semantics as [`render_config`], shaped
/// for tooling (`jq`, deploy diffing) instead of eyeballs.
///
/// This is an *inspection* view, not a config round-trip: enum-ish
/// fields are rendered as strings, unset optionals are `null`, and
/// redacted secrets are the literal string `"<redacted>"`. Feeding the
/// output back into the daemon is not supported.
pub fn render_config_json(config: &Config, show_secrets: bool) -> String {
    let routes: Vec<Value> = config
        .routes
        .iter()
        .map(|r| {
            // Same rule as the text renderer: only overrides that are
            // actually set appear (absent key = inherits the global).
            let mut route = serde_json::Map::new();
            route.insert("name".into(), json!(r.name));
            if let Some(u) = &r.bridge.ws_url {
                route.insert("ws_url".into(), json!(u));
            }
            if r.bridge.ws_auth_header.is_some() {
                route.insert(
                    "ws_auth_header".into(),
                    secret_json(&r.bridge.ws_auth_header, show_secrets),
                );
            }
            if let Some(c) = &r.media.codecs {
                route.insert(
                    "codecs".into(),
                    json!(c.iter().map(|c| format!("{c:?}")).collect::<Vec<_>>()),
                );
            }
            if let Some(m) = &r.media.srtp {
                route.insert("srtp".into(), json!(m.to_string()));
            }
            if let Some(m) = &r.recording.mode {
                route.insert("recording".into(), json!(m.to_string()));
            }
            if let Some(a) = &r.security.min_attestation {
                route.insert("min_attestation".into(), json!(a.to_string()));
            }
            if r.bridge_tls.is_some() {
                route.insert("bridge_tls".into(), json!("set (overrides global)"));
            }
            Value::Object(route)
        })
        .collect();

    let registrations: Vec<Value> = config
        .registrations
        .iter()
        .map(|reg| {
            json!({
                "name": reg.name,
                "server": reg.server_host,
                "transport": transport_str(&reg.transport),
                "username": reg.username,
                "password": secret_value(&reg.password, show_secrets),
                "expires_secs": reg.expires.as_secs(),
                "register_on_startup": reg.register_on_startup,
            })
        })
        .collect();

    let trunks: Vec<Value> = config
        .trunks
        .iter()
        .map(|t| {
            json!({
                "name": t.name,
                "peer_addrs": t.peer_addrs.len(),
                "from_hosts": t.from_hosts,
            })
        })
        .collect();

    let gateways: Vec<Value> = config
        .outbound
        .gateways
        .iter()
        .map(|g| {
            json!({
                "name": g.name,
                "proxy": format!("{}:{}", g.proxy_host, g.proxy_port),
                "transport": transport_str(&g.transport),
                "from": g.from,
                "srtp": format!("{:?}", g.srtp),
                "credentials": g.credentials.as_ref().map(|c| {
                    json!({
                        "username": c.username,
                        "password": secret_value(&c.password, show_secrets),
                    })
                }),
            })
        })
        .collect();

    let b = &config.bridge_defaults;
    let root = json!({
        "node": {
            "id": config.node.id,
            "public_address": config.node.public_address.to_string(),
        },
        "sip": {
            "listen": config.sip.listen_addr.to_string(),
            "transports": config.sip.transports.iter().map(transport_str).collect::<Vec<_>>(),
            "tls": config.sip.tls.is_some(),
            "allow_delayed_offer": config.sip.allow_delayed_offer,
        },
        "media": {
            "srtp": format!("{:?}", config.media.srtp),
            "rtp_port_range": config.media.rtp_port_range.map(|(lo, hi)| format!("{lo}-{hi}")),
            "moh_file": config.media.moh_file.as_ref().map(|p| p.display().to_string()),
        },
        "bridge": {
            "ws_url": b.ws_url,
            "auth_header": secret_json(&b.auth_header, show_secrets),
            "codecs": b.codecs.iter().map(|c| format!("{c:?}")).collect::<Vec<_>>(),
            "barge_in": format!("{:?}", b.barge_in.mode),
            "forward_headers": b.forward_headers,
        },
        "routes": {
            "count": config.routes.len(),
            "has_default": config.routes.has_default(),
            "list": routes,
        },
        "registrations": registrations,
        "trunks": trunks,
        "security": {
            "stir_shaken": config.security.stir_shaken.enabled,
            "min_attestation": format!("{:?}", config.security.min_attestation),
        },
        "recording": {
            "mode": format!("{:?}", config.recording.mode),
            "dir": if matches!(config.recording.mode, RecordingMode::Off) {
                Value::Null
            } else {
                json!(config.recording.dir.display().to_string())
            },
        },
        "outbound": {
            "max_concurrent": config.outbound.max_concurrent,
            "rate_limit_per_sec": config.outbound.rate_limit_per_sec,
            "gateways": gateways,
        },
        "conference": {
            "enabled": config.conference.enabled,
            "max_rooms": config.conference.max_rooms,
            "max_participants_per_room": config.conference.max_participants_per_room,
        },
        "park": {
            "enabled": config.park.enabled,
            "max_parked": config.park.max_parked,
            "timeout_secs": config.park.timeout.map(|d| d.as_secs()),
            "timeout_action": format!("{:?}", config.park.timeout_action),
        },
        "cdr": {
            "enabled": config.cdr.enabled,
            "file": config.cdr.file.as_ref().map(|f| json!({
                "path": f.path.display().to_string(),
            })),
            "webhook": config.cdr.webhook.as_ref().map(|w| json!({
                "url": w.url,
                "auth_header": secret_json(&w.auth_header, show_secrets),
                "secret": secret_json(&w.secret, show_secrets),
                "spool_dir": w.spool_dir,
            })),
        },
        "webhooks": {
            "enabled": config.webhooks.enabled,
            "url": config.webhooks.url,
            "auth_header": secret_json(&config.webhooks.auth_header, show_secrets),
            "secret": secret_json(&config.webhooks.secret, show_secrets),
            "spool_dir": config.webhooks.spool_dir,
            "events": config.webhooks.events,
        },
        "audit": {
            "enabled": config.audit.enabled,
            "events": config.audit.events,
            "file": config.audit.file.as_ref().map(|f| json!({
                "path": f.path.display().to_string(),
            })),
            "webhook": config.audit.webhook.as_ref().map(|w| json!({
                "url": w.url,
                "auth_header": secret_json(&w.auth_header, show_secrets),
                "secret": secret_json(&w.secret, show_secrets),
                "spool_dir": w.spool_dir,
            })),
        },
        "observability": {
            "enabled": config.observability.enabled,
            "http_listen": config.observability.http_listen.map(|a| a.to_string()),
        },
        "admin": config.admin.as_ref().map(|a| json!({
            "listen": a.listen_addr.to_string(),
            "tokens": a.auth.iter().map(|t| json!({
                "name": t.name,
                "role": t.role.as_str(),
            })).collect::<Vec<_>>(),
        })),
        "hep": {
            "enabled": config.hep.enabled,
            "collector": config.hep.collector.map(|a| a.to_string()),
            "capture_id": config.hep.capture_id,
            "capture_password": secret_json(&config.hep.capture_password, show_secrets),
        },
    });

    // `to_string_pretty` can only fail on non-string map keys / broken
    // `Serialize` impls — `Value` has neither.
    let mut s = serde_json::to_string_pretty(&root).expect("Value serializes");
    s.push('\n');
    s
}

/// Run the dialplan against synthetic call attributes and report the
/// winning route + its effective bridge config (route override merged
/// over the `[bridge]` default for the headline fields).
pub fn route_test(config: &Config, input: &RouteTestInput) -> String {
    let mut headers = Headers::new();
    for (k, v) in &input.headers {
        headers.insert(k, v.clone());
    }
    let info = CallInfo {
        request_uri_user: &input.ruri_user,
        request_uri_host: &input.ruri_host,
        to_user: &input.to_user,
        to_host: &input.to_host,
        from_user: &input.from_user,
        from_host: &input.from_host,
        register_source: &input.register_source,
        headers: &headers,
    };

    let mut o = String::new();
    let s = &mut o;
    let _ = writeln!(s, "route-test input:");
    let _ = writeln!(s, "  request-uri = {}@{}", input.ruri_user, input.ruri_host);
    let _ = writeln!(s, "  to          = {}@{}", input.to_user, input.to_host);
    let _ = writeln!(s, "  from        = {}@{}", input.from_user, input.from_host);
    let _ = writeln!(s, "  register_source = {}", input.register_source);
    if !input.headers.is_empty() {
        let _ = writeln!(s, "  headers     = {:?}", input.headers);
    }
    let _ = writeln!(s);

    match config.routes.find_match(&info) {
        None => {
            let _ = writeln!(s, "NO MATCH — the call would be rejected with SIP 404.");
            if !config.routes.has_default() {
                let _ = writeln!(
                    s,
                    "(no default `any = true` route is configured — add one to catch unmatched calls)"
                );
            }
        }
        Some(r) => {
            let _ = writeln!(s, "matched route: {}", r.name);
            // Effective bridge config for the headline fields: the route
            // override wins, else the global default.
            let eff_ws_url = r
                .bridge
                .ws_url
                .clone()
                .or_else(|| config.bridge_defaults.ws_url.clone());
            let _ = writeln!(
                s,
                "  effective ws_url = {}",
                match &eff_ws_url {
                    Some(u) => u.clone(),
                    None => "<unset> — call would be rejected 503 (no ws_url)".into(),
                }
            );
            let inherits = r.bridge.ws_url.is_none();
            let _ = writeln!(
                s,
                "    (from {})",
                if inherits {
                    "[bridge] default"
                } else {
                    "route override"
                }
            );
            if let Some(c) = &r.media.codecs {
                let _ = writeln!(s, "  effective codecs = {c:?} (route override)");
            } else {
                let _ = writeln!(
                    s,
                    "  effective codecs = {:?} ([bridge] default)",
                    config.bridge_defaults.codecs
                );
            }
            if let Some(m) = &r.recording.mode {
                let _ = writeln!(s, "  recording = {m} (route override)");
            }
            let _ = writeln!(
                s,
                "  bridge mTLS = {}",
                if r.bridge_tls.is_some() {
                    "on (route override)"
                } else if config.bridge_defaults.bridge_tls.is_some() {
                    "on ([bridge.tls] default)"
                } else {
                    "off"
                }
            );
        }
    }
    o
}
