//! Deserialize-only TOML representation of the daemon config.
//!
//! Mirrors the schema in `docs/CONFIG.md` / `docs/DEV_PLAN.md` §6.2.
//! v1 ships a deliberately small slice — every field here has a
//! consumer in the layers we've already built. Out-of-scope fields
//! (`[[register]]`, `[hep]`, `[cdr]`, `[webhooks]`, `[observability]`,
//! `[security]`) get accepted-and-ignored on load so today's TOML
//! file doesn't become invalid the moment a follow-up PR adds them.
//!
//! `[[route]]` deserialization is delegated to the routes crate via
//! `RawRouteFile` — keeping the dialplan grammar in one place
//! (CLAUDE.md §4.6).

use serde::Deserialize;
use siphon_ai_routes::RawRoute;

/// Top-level parse target. `#[serde(deny_unknown_fields = false)]` is
/// the default; we tolerate unknown top-level tables so adding a new
/// section in a deployed config doesn't break daemons that don't
/// know about it yet. Unknown *fields within known sections* still
/// surface as parse errors, which is the right strictness — it
/// catches typos like `auido_sample_rate`.
#[derive(Debug, Deserialize)]
pub struct RawConfig {
    #[serde(default)]
    pub node: RawNode,

    pub sip: RawSip,

    #[serde(default)]
    pub media: RawMedia,

    #[serde(default)]
    pub bridge: RawBridge,

    #[serde(default, rename = "route")]
    pub routes: Vec<RawRoute>,

    #[serde(default)]
    pub cdr: RawCdr,

    #[serde(default)]
    pub observability: RawObservability,
}

/// `[node]` — identity for logs / metrics / SDP origin host.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawNode {
    /// Logging context. Optional in v1.
    #[serde(default)]
    pub id: Option<String>,
    /// Address that goes into the answer's `c=`/`o=` lines. If
    /// unset, the bind address from `[sip].listen` is used (good
    /// enough for L2 networks; deployments behind 1:1 NAT MUST set
    /// this).
    #[serde(default)]
    pub public_address: Option<String>,
}

/// `[sip]` — the SIP transport layer.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawSip {
    /// `host:port` to bind. Required.
    pub listen: String,
    /// Transports to enable on `listen`. Default: `["udp"]`.
    #[serde(default = "default_transports")]
    pub transports: Vec<String>,
    /// Value of the `User-Agent` header on outbound responses. The
    /// SIP stack has its own default; this overrides it.
    #[serde(default)]
    pub user_agent: Option<String>,
    /// SIP `Contact` URI — `sip:user@host[:port]`. Optional; if
    /// unset the daemon synthesizes one from `[node].public_address`
    /// and `listen`.
    #[serde(default)]
    pub contact: Option<String>,
}

fn default_transports() -> Vec<String> {
    vec!["udp".to_string()]
}

/// `[media]` — codecs + DTMF + RTP port range. v1 ignores
/// `inactivity_timeout_secs` (forge-engine has its own setting).
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawMedia {
    /// Priority-ordered codec list.
    #[serde(default)]
    pub codecs: Option<Vec<String>>,
    /// `"rfc2833" | "off"` — `"info"` / `"both"` post-v1.
    #[serde(default)]
    pub dtmf: Option<String>,
    /// `[min, max]` for forge's RTP port pool. Optional in v1; if
    /// unset, forge's default range is used.
    #[serde(default)]
    pub rtp_port_range: Option<(u16, u16)>,
}

/// `[cdr]` — call detail record sinks. v1 supports a JSONL file
/// sink and an HTTP webhook sink; both off by default.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawCdr {
    /// Master switch. When `false` the daemon installs a no-op
    /// sink regardless of the file/webhook sub-blocks.
    #[serde(default)]
    pub enabled: bool,

    #[serde(default)]
    pub file: RawCdrFile,

    #[serde(default)]
    pub webhook: RawCdrWebhook,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawCdrFile {
    #[serde(default)]
    pub enabled: bool,
    /// Required when `enabled = true`. Parent directory must exist
    /// at startup; the daemon does NOT mkdir.
    #[serde(default)]
    pub path: Option<String>,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawCdrWebhook {
    #[serde(default)]
    pub enabled: bool,
    /// Required when `enabled = true`.
    #[serde(default)]
    pub url: Option<String>,
    /// Optional `Authorization` header value, sent verbatim.
    #[serde(default)]
    pub auth_header: Option<String>,
    #[serde(default)]
    pub retry_max: Option<u32>,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

/// `[observability]` — Prometheus metrics + `/health` + `/ready`
/// HTTP endpoints. v1 supports a single `http_listen` address; the
/// daemon refuses to start if both `[observability].enabled = true`
/// and `http_listen` is missing.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawObservability {
    /// `false` (default) means the observability HTTP server is not
    /// spawned at all — the metrics facade still works (process-wide
    /// recorder is installed regardless), but nothing scrapes it.
    /// In production deployments you almost always want this true.
    #[serde(default)]
    pub enabled: bool,
    /// `host:port` to bind the observability HTTP listener on.
    /// Required when `enabled = true`.
    #[serde(default)]
    pub http_listen: Option<String>,
}

/// `[bridge]` — daemon-wide bridge defaults.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawBridge {
    /// Default WebSocket URL. May be unset if every route sets its
    /// own `ws_url`.
    #[serde(default)]
    pub ws_url: Option<String>,
    /// Default `Authorization` header value (e.g., `"Bearer xyz"`).
    #[serde(default)]
    pub ws_auth_header: Option<String>,
    /// WS handshake timeout. Default: 5000 ms.
    #[serde(default)]
    pub ws_connect_timeout_ms: Option<u64>,
    /// `8000` or `16000`. Default: 8000. (Per-route can still
    /// override via `[route.bridge].audio_sample_rate`, but the
    /// negotiated codec ultimately decides — this is a *preference*
    /// hint that becomes the answer's first-choice when our caps
    /// support multiple rates.)
    #[serde(default)]
    pub audio_sample_rate: Option<u32>,
    /// SIP headers to forward on the bridge `start.sip.headers`.
    /// Names are case-insensitive at lookup time.
    #[serde(default)]
    pub forward_headers: Option<Vec<String>>,
}
