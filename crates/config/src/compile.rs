//! Compile a `RawConfig` (post env-expansion, post TOML parse) into
//! a [`Config`] the daemon can hand to its sub-crates verbatim.
//!
//! Validation rules per `docs/DEV_PLAN.md` §6.5 and CLAUDE.md §4.6:
//!
//! - `[sip].listen` parses as a `SocketAddr`.
//! - At least one transport is enabled, and every name is one of
//!   `udp` / `tcp` / `tls`.
//! - Every codec name parses via [`Codec::from_encoding_name`].
//! - `[bridge].codecs` parse to known encodings via
//!   `Codec::from_encoding_name`. Opus is rejected here — see
//!   [`CompileError::CodecRequiresResampling`].
//! - Every regex in the dialplan compiles (delegated to the routes
//!   compiler).
//! - A trailing default route (`any = true`) is recommended but not
//!   required — we emit a `tracing::warn` instead, since reload
//!   workflows ("temporarily route everything to X") legitimately
//!   want a non-default trailing route.
//!
//! `[node].public_address` falls back to the bind host of
//! `[sip].listen` when unset. This is the host that goes onto every
//! answer's `c=` line; getting it wrong silently causes RTP to flow
//! to the wrong address, so we'd rather pick a sensible default
//! than fail loud.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use siphon_ai_bridge::normalize_auth_header;
use siphon_ai_core::BridgeDefaults;
use siphon_ai_media_glue::Codec;
use siphon_ai_routes::{compile as compile_routes, RawRouteFile, RouteSet};
use thiserror::Error;
use tracing::warn;

use crate::raw::{
    RawBridge, RawCdr, RawConfig, RawHep, RawMedia, RawNode, RawObservability, RawRecording,
    RawRegister, RawSecurity, RawSip, RawSipTls, RawWebhooks,
};

/// Compiled, ready-to-pass daemon config.
///
/// `bridge_defaults` is what `BridgingAcceptor::new` wants. `routes`
/// goes straight into `RoutingHandler::new`. `sip.listen_addr` is
/// what the SIP transport binds on. `local_ip` is what `MediaSetup`
/// stamps into answer SDP `c=` / `o=` lines. `cdr` is the resolved
/// CDR sinks plan (file + webhook); the daemon binary builds the
/// concrete sinks from it.
#[derive(Debug, Clone)]
pub struct Config {
    pub node: NodeConfig,
    pub sip: SipConfig,
    pub media: MediaConfig,
    pub bridge_defaults: BridgeDefaults,
    pub routes: RouteSet,
    pub registrations: Vec<RegisterConfig>,
    /// `[[trunk]]` allowlist. Empty when no trunk blocks were
    /// declared (daemon accepts INVITEs from any source —
    /// "legacy" posture, documented as dev / behind-firewall
    /// only). Non-empty enables strict-allowlist mode: every
    /// inbound INVITE must match a trunk or it's 403'd.
    pub trunks: Vec<TrunkConfig>,
    pub security: SecurityConfig,
    pub recording: siphon_ai_recording::RecordingConfig,
    pub cdr: CdrConfig,
    pub observability: ObservabilityConfig,
    pub webhooks: WebhooksConfig,
    pub hep: HepConfig,
}

/// Compiled `[security]` — the call-authentication policy (STIR/SHAKEN).
/// Default is fully inert: no minimum attestation and verification
/// disabled, so a 0.3.x config upgrades with zero behaviour change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecurityConfig {
    /// Global minimum-attestation gate. `None` admits every call.
    pub min_attestation: siphon_ai_security::MinAttestation,
    /// SIP status to return when the gate rejects a call (403 / 488 / 606).
    pub min_attestation_response: u16,
    /// Compiled STIR/SHAKEN verification settings.
    pub stir_shaken: siphon_ai_security::StirShakenConfig,
}

impl Default for SecurityConfig {
    fn default() -> Self {
        Self {
            min_attestation: siphon_ai_security::MinAttestation::None,
            min_attestation_response: 403,
            stir_shaken: siphon_ai_security::StirShakenConfig::default(),
        }
    }
}

/// One compiled `[[register]]` block. The daemon's
/// `RegistrationManager` consumes these; the registration `name`
/// also surfaces as a `register_source` route key for matched
/// inbound calls.
#[derive(Debug, Clone)]
pub struct RegisterConfig {
    pub name: String,
    /// Resolved registrar address. The daemon may still re-resolve
    /// at runtime via the SIP DNS resolver, but a literal
    /// `host:port` is the fast path and the only one v1 supports.
    pub server_addr: SocketAddr,
    /// Original `host` from config — used as the `From` URI host
    /// in REGISTER requests.
    pub server_host: String,
    pub transport: SipTransport,
    pub username: String,
    /// Defaults to `username` when not set.
    pub auth_username: String,
    pub password: String,
    pub realm: Option<String>,
    pub expires: Duration,
    pub register_on_startup: bool,
}

/// Compiled `[[trunk]]` allowlist entry. The daemon's sip-glue
/// `TrunkMatcher` walks these in order on each inbound INVITE;
/// when both `peer_addrs` and `from_hosts` are populated, both
/// must match (defense in depth).
#[derive(Debug, Clone)]
pub struct TrunkConfig {
    pub name: String,
    /// Allowed source addresses as parsed CIDR ranges. An exact
    /// IP is stored as a `/32` (IPv4) or `/128` (IPv6).
    /// Empty when `peer_addrs` was unset in the raw config — the
    /// matcher then skips the IP check for this trunk.
    pub peer_addrs: Vec<TrunkCidr>,
    /// Allowed From-URI hostnames, lowercased. Empty = skip
    /// From-host check.
    pub from_hosts: Vec<String>,
}

/// CIDR range matcher used by [`TrunkConfig::peer_addrs`]. Stored
/// pre-parsed at config-load time so the per-INVITE match is a
/// few bit-and / compare ops rather than re-parsing strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrunkCidr {
    pub network: std::net::IpAddr,
    /// Prefix length in bits. 32 for an exact IPv4, 128 for IPv6.
    pub prefix_len: u8,
}

impl TrunkCidr {
    /// Parse `"a.b.c.d"` or `"a.b.c.d/n"`. Hosts-only strings get
    /// an implicit `/32` (IPv4) or `/128` (IPv6).
    pub fn parse(s: &str) -> Result<Self, TrunkCidrParseError> {
        let (host, prefix) = match s.find('/') {
            Some(slash) => {
                let prefix: u8 = s[slash + 1..]
                    .parse()
                    .map_err(|_| TrunkCidrParseError::BadPrefix(s.to_string()))?;
                (&s[..slash], Some(prefix))
            }
            None => (s, None),
        };
        let ip: std::net::IpAddr = host
            .parse()
            .map_err(|_| TrunkCidrParseError::BadAddress(s.to_string()))?;
        let prefix_len = match (ip, prefix) {
            (std::net::IpAddr::V4(_), Some(p)) if p > 32 => {
                return Err(TrunkCidrParseError::PrefixOutOfRange(s.to_string()));
            }
            (std::net::IpAddr::V6(_), Some(p)) if p > 128 => {
                return Err(TrunkCidrParseError::PrefixOutOfRange(s.to_string()));
            }
            (std::net::IpAddr::V4(_), Some(p)) => p,
            (std::net::IpAddr::V6(_), Some(p)) => p,
            (std::net::IpAddr::V4(_), None) => 32,
            (std::net::IpAddr::V6(_), None) => 128,
        };
        Ok(TrunkCidr {
            network: ip,
            prefix_len,
        })
    }

    /// True iff `candidate` falls inside this CIDR.
    pub fn contains(&self, candidate: std::net::IpAddr) -> bool {
        match (self.network, candidate) {
            (std::net::IpAddr::V4(net), std::net::IpAddr::V4(c)) => {
                let net_bits = u32::from(net);
                let c_bits = u32::from(c);
                let shift = 32u32.saturating_sub(self.prefix_len as u32);
                if shift == 32 {
                    // /0 — matches everything.
                    return true;
                }
                let mask = u32::MAX.checked_shl(shift).unwrap_or(0);
                (net_bits & mask) == (c_bits & mask)
            }
            (std::net::IpAddr::V6(net), std::net::IpAddr::V6(c)) => {
                let net_bits = u128::from(net);
                let c_bits = u128::from(c);
                let shift = 128u128.saturating_sub(self.prefix_len as u128);
                if shift == 128 {
                    return true;
                }
                let mask = u128::MAX.checked_shl(shift as u32).unwrap_or(0);
                (net_bits & mask) == (c_bits & mask)
            }
            _ => false, // v4 ↔ v6 never match
        }
    }
}

/// Errors `TrunkCidr::parse` can surface. Wrapped by
/// `CompileError::BadTrunkPeerAddr` at the config layer.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum TrunkCidrParseError {
    #[error("not a valid IP address: {0:?}")]
    BadAddress(String),
    #[error("not a valid prefix length: {0:?}")]
    BadPrefix(String),
    #[error("prefix length out of range: {0:?}")]
    PrefixOutOfRange(String),
}

/// Resolved lifecycle-webhook plan. The daemon binary turns this
/// into a real `siphon-ai-webhooks::HttpSink` (optionally wrapped
/// in `FilteredSink`) at runtime.
#[derive(Debug, Clone, Default)]
pub struct WebhooksConfig {
    pub enabled: bool,
    pub url: Option<String>,
    pub auth_header: Option<String>,
    /// Empty = deliver everything; non-empty = allowlist filter.
    pub events: Vec<String>,
    pub retry_max: u32,
    pub timeout: Duration,
}

/// Resolved observability plan. The daemon binary turns this into
/// a real `siphon-ai-telemetry::ObservabilityServer` (or skips it
/// when disabled).
#[derive(Debug, Clone, Default)]
pub struct ObservabilityConfig {
    pub enabled: bool,
    pub http_listen: Option<SocketAddr>,
}

/// Resolved HEP3 (Homer) plan. The daemon binary builds a real
/// `hep_rs::UdpHepSink` from this, installs `sip_hep::SipHepEmitter`
/// and `forge_hep::ForgeHepEmitter` against it, and uses the same
/// sink for its own log/CDR HEP chunks.
///
/// Always present (default `enabled = false`). Other fields are
/// `Option`s — set by `compile_hep` when `enabled = true`.
#[derive(Debug, Clone, Default)]
pub struct HepConfig {
    pub enabled: bool,
    /// `host:port` of the collector. Always `Some` when `enabled`.
    pub collector: Option<SocketAddr>,
    /// Homer agent ID. Always `Some` when `enabled`.
    pub capture_id: Option<u32>,
    /// HEPlify-Server shared password.
    pub capture_password: Option<String>,
    /// Bounded queue capacity between producer and worker. Default
    /// `256` (matches `hep_rs::DEFAULT_QUEUE_CAPACITY`).
    pub queue_capacity: usize,
}

/// Resolved CDR plan. The daemon translates this into actual
/// `siphon-ai-cdr` sinks at runtime (config doesn't depend on the
/// CDR crate to keep the dep graph minimal).
#[derive(Debug, Clone, Default)]
pub struct CdrConfig {
    /// `[cdr].enabled`. Even when true, file and webhook are
    /// individually off until their `enabled = true` is set.
    pub enabled: bool,
    pub file: Option<CdrFileConfig>,
    pub webhook: Option<CdrWebhookConfig>,
}

#[derive(Debug, Clone)]
pub struct CdrFileConfig {
    pub path: PathBuf,
}

#[derive(Debug, Clone)]
pub struct CdrWebhookConfig {
    pub url: String,
    pub auth_header: Option<String>,
    pub retry_max: u32,
    pub timeout: Duration,
}

#[derive(Debug, Clone)]
pub struct NodeConfig {
    pub id: String,
    /// Address used for SDP `c=` / `o=`. Always non-empty after
    /// compile (defaults to `[sip].listen`'s bind host).
    pub public_address: String,
}

#[derive(Debug, Clone)]
pub struct SipConfig {
    pub listen_addr: SocketAddr,
    pub transports: Vec<SipTransport>,
    pub user_agent: Option<String>,
    pub contact: Option<String>,
    /// What — if any — provisional response the UAS layers on top of
    /// `100 Trying` before the 2xx. From `[sip.call_progress].mode`;
    /// default is `InstantAnswer` (v0.1.0 behaviour).
    pub call_progress: siphon_ai_core::CallProgressMode,
    /// `Some` when `[sip.tls]` is supplied AND `tls` is in the
    /// transports list. `None` when TLS isn't enabled. Daemon
    /// loads cert/key from these paths at startup.
    pub tls: Option<SipTlsConfig>,
    /// RFC 4028 Min-SE for the UAS (`[sip].min_session_expires_secs`).
    /// Defaults to 90 (RFC minimum).
    pub min_session_expires: Duration,
    /// Optional cap on Session-Expires when negotiating
    /// (`[sip].preferred_session_expires_secs`). `None` = honour
    /// the peer's value uncapped.
    pub preferred_session_expires: Option<Duration>,
}

#[derive(Debug, Clone)]
pub struct SipTlsConfig {
    pub listen_addr: SocketAddr,
    pub cert_path: PathBuf,
    pub key_path: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SipTransport {
    Udp,
    Tcp,
    Tls,
}

#[derive(Debug, Clone)]
pub struct MediaConfig {
    /// `[media].rtp_port_range`, when set; the daemon hands this to
    /// forge's `PortPool`. `None` = use forge's default.
    pub rtp_port_range: Option<(u16, u16)>,
    /// `[media].srtp` resolved to its enum form. Default is
    /// [`SrtpMode::Off`] — plaintext-RTP only, matching v0.2.0.
    /// Wire behaviour is wired up in Sprint 1 Week 2 / 3; the
    /// field exists in W1 so per-route override merging and the
    /// `start.srtp` event field have a stable type to bind to.
    pub srtp: siphon_ai_core::SrtpMode,
}

#[derive(Debug, Error)]
pub enum CompileError {
    #[error("[sip].listen {0:?} is not a valid socket address: {1}")]
    BadSipListen(String, std::net::AddrParseError),

    #[error("[sip].transports must be non-empty")]
    NoTransports,

    #[error("[sip].transports has unknown entry {0:?}; expected udp / tcp / tls")]
    UnknownTransport(String),

    #[error("[sip.tls].cert is required when transports includes \"tls\"")]
    SipTlsCertRequired,

    #[error("[sip.tls].key is required when transports includes \"tls\"")]
    SipTlsKeyRequired,

    #[error("[sip.tls].listen {0:?} is not a valid socket address: {1}")]
    BadSipTlsListen(String, std::net::AddrParseError),

    #[error("[sip.tls] is configured but transports does not include \"tls\"")]
    SipTlsConfiguredButNotEnabled,

    #[error("[media].codecs has unknown codec {0:?}")]
    UnknownCodec(String),

    #[error(
        "[media].codecs lists {0:?}, which is not supported on the WS audio path \
         (samples at 48 kHz; the bridge ships PCM16 at 8 kHz or 16 kHz). Resampling \
         is post-v1 work — drop the codec from the list for now."
    )]
    CodecRequiresResampling(String),

    #[error(
        "route {route:?} sets [route.bridge].on_ws_failure = {value:?}; expected \
         \"hangup\" (play_prompt is post-v1)"
    )]
    UnknownOnWsFailure { route: String, value: String },

    #[error("[media].dtmf is {0:?}; expected \"rfc2833\" or \"off\"")]
    UnknownDtmfMode(String),

    #[error(
        "[sip.call_progress].mode is {0:?}; expected \"instant_answer\", \"ringing\", \
         or \"session_progress\""
    )]
    UnknownCallProgressMode(String),

    #[error("[bridge.barge_in].mode is {0:?}; expected \"auto_clear\" or \"notify_only\"")]
    UnknownBargeInMode(String),

    #[error("[media].srtp is {0:?}; expected \"off\", \"preferred\", or \"required\"")]
    UnknownSrtpMode(String),

    #[error("[bridge.tls] is malformed: {0}")]
    BadBridgeTls(#[from] siphon_ai_bridge::tls::TlsConfigError),

    #[error("[security].min_attestation is {0:?}; expected \"none\", \"A\", \"B\", or \"C\"")]
    UnknownMinAttestation(String),

    #[error("[security].min_attestation_response is {0}; expected 403, 488, or 606")]
    InvalidMinAttestationResponse(u16),

    #[error(
        "[security].min_attestation is {0:?} but [security.stir_shaken].enabled is false — \
         without verification no call can produce an attestation, so every call would be \
         rejected. Enable stir_shaken or set min_attestation = \"none\"."
    )]
    MinAttestationWithoutStirShaken(String),

    #[error(
        "route {route:?} sets [route.security].min_attestation = {value:?}; \
         expected \"none\", \"A\", \"B\", or \"C\""
    )]
    UnknownRouteMinAttestation { route: String, value: String },

    #[error(
        "route {route:?} sets [route.security].min_attestation but \
         [security.stir_shaken].enabled is false — without verification the gate would \
         reject every call this route matches. Enable stir_shaken or drop the override."
    )]
    RouteMinAttestationWithoutStirShaken { route: String },

    #[error("[security.stir_shaken].trust_anchors is required when enabled = true")]
    StirShakenTrustAnchorsRequired,

    #[error("[security.stir_shaken].trust_anchors {path:?} is invalid: {err}")]
    StirShakenTrustAnchorsInvalid { path: String, err: String },

    #[error("[security.stir_shaken].x5u_tls_extra_ca {path:?} is invalid: {err}")]
    StirShakenExtraCaInvalid { path: String, err: String },

    #[error("[recording].mode is {0:?}; expected \"off\" or \"always\"")]
    UnknownRecordingMode(String),

    #[error("[recording].dir is required when mode is not \"off\"")]
    RecordingDirRequired,

    #[error("[recording].dir {path:?} could not be created: {err}")]
    RecordingDirInvalid { path: String, err: String },

    #[error("[route.recording].mode on route {route:?} is {value:?}; expected \"off\", \"always\", or \"on_demand\"")]
    RouteRecordingModeInvalid { route: String, value: String },

    #[error("route {route:?} enables recording but [recording].dir is not set")]
    RouteRecordingWithoutDir { route: String },

    #[error("[media].rtp_port_range {min}-{max} is invalid (min must be < max and even)")]
    BadRtpPortRange { min: u16, max: u16 },

    #[error("[cdr.file].path is required when [cdr.file].enabled = true")]
    CdrFilePathRequired,

    #[error("[cdr.webhook].url is required when [cdr.webhook].enabled = true")]
    CdrWebhookUrlRequired,

    #[error("[observability].http_listen {0:?} is not a valid socket address: {1}")]
    BadObservabilityListen(String, std::net::AddrParseError),

    #[error("[observability].http_listen is required when [observability].enabled = true")]
    ObservabilityListenRequired,

    #[error("[webhooks].url is required when [webhooks].enabled = true")]
    WebhooksUrlRequired,

    #[error("[hep].collector is required when [hep].enabled = true")]
    HepCollectorRequired,

    #[error("[hep].collector {0:?} is not a valid host:port socket address: {1}")]
    BadHepCollector(String, std::net::AddrParseError),

    #[error("[hep].collector {0:?} failed DNS resolution: {1}")]
    BadHepCollectorResolve(String, std::io::Error),

    #[error("[hep].collector {0:?} resolved to no addresses")]
    HepCollectorResolveEmpty(String),

    #[error(
        "[node].public_address must be set when [sip].listen binds an unspecified address \
         (got {0}); the SDP answer's c= line cannot use 0.0.0.0 / ::"
    )]
    PublicAddressRequiredForWildcardListen(std::net::IpAddr),

    #[error("[hep].capture_id is required when [hep].enabled = true")]
    HepCaptureIdRequired,

    #[error(
        "[sip].min_session_expires_secs = {0} is below the RFC 4028 floor of 90 \
         seconds; raise it or omit the field to use the 90 s default"
    )]
    SessionTimerMinSeTooSmall(u32),

    #[error("[[trunk]] block at index {index} has empty name")]
    TrunkEmptyName { index: usize },

    #[error("two [[trunk]] blocks share name {name:?} (#{first} and #{second})")]
    TrunkDuplicateName {
        name: String,
        first: usize,
        second: usize,
    },

    #[error(
        "[[trunk]] {name:?} declares neither peer_addrs nor from_hosts — a trunk \
         must identify its peer by IP, From-URI host, or both"
    )]
    TrunkNoMatchCriteria { name: String },

    #[error("[[trunk]] {name:?} peer_addrs entry {value:?} is invalid: {err}")]
    TrunkBadPeerAddr {
        name: String,
        value: String,
        err: TrunkCidrParseError,
    },

    #[error("[[trunk]] {name:?} from_hosts entry {value:?} is empty / whitespace-only")]
    TrunkEmptyFromHost { name: String, value: String },

    #[error("[[register]] block at index {index} has empty name")]
    RegisterEmptyName { index: usize },

    #[error("two [[register]] blocks share name {name:?} (#{first} and #{second})")]
    RegisterDuplicateName {
        name: String,
        first: usize,
        second: usize,
    },

    #[error("[[register]] {name:?} server {server:?} is not a valid host or host:port: {err}")]
    RegisterBadServer {
        name: String,
        server: String,
        err: String,
    },

    #[error("[[register]] {name:?} unknown transport {transport:?}; expected udp / tcp / tls")]
    RegisterUnknownTransport { name: String, transport: String },

    #[error(transparent)]
    Routes(#[from] siphon_ai_routes::RouteError),
}

/// Compile a raw config into the consumer-ready form.
pub fn compile(raw: RawConfig) -> Result<Config, CompileError> {
    let sip = compile_sip(raw.sip)?;
    let node = compile_node(raw.node, &sip)?;
    let media = compile_media(&raw.media)?;
    let bridge_defaults = compile_bridge(raw.bridge, &raw.media)?;
    let routes = compile_dialplan(raw.routes)?;
    let registrations = compile_registrations(raw.registrations)?;
    let trunks = compile_trunks(raw.trunks)?;
    let security = compile_security(raw.security)?;
    let recording = compile_recording(raw.recording)?;
    let cdr = compile_cdr(raw.cdr)?;
    let observability = compile_observability(raw.observability)?;
    let webhooks = compile_webhooks(raw.webhooks)?;
    let hep = compile_hep(raw.hep)?;

    if !routes.has_default() {
        warn!(
            route_count = routes.len(),
            "no default `any = true` route configured — non-matching INVITEs will be 404'd"
        );
    }

    // A per-route attestation gate, like the global one, is meaningless
    // without verification: with stir_shaken off no call produces a trusted
    // attestation, so the route would reject everything it matches. Fail
    // loud rather than black-hole that route's traffic. (`min_attestation =
    // "none"` is a no-op override and allowed either way.)
    if !security.stir_shaken.enabled {
        for route in routes.iter() {
            let overrides_gate = route
                .security
                .min_attestation
                .as_deref()
                .and_then(siphon_ai_security::MinAttestation::parse)
                .is_some_and(|m| m != siphon_ai_security::MinAttestation::None);
            if overrides_gate {
                return Err(CompileError::RouteMinAttestationWithoutStirShaken {
                    route: route.name.clone(),
                });
            }
        }
    }

    // A per-route recording override that enables recording needs an output
    // directory. The dir lives on the global `[recording]` block; require it
    // (created at load by `compile_recording`) when any route turns recording
    // on, even if the global mode is `off`.
    if recording.dir.as_os_str().is_empty() {
        for route in routes.iter() {
            let enables = matches!(
                route.recording.mode.as_deref(),
                Some("always") | Some("on_demand")
            );
            if enables {
                return Err(CompileError::RouteRecordingWithoutDir {
                    route: route.name.clone(),
                });
            }
        }
    }

    // SRTP-without-SIP/TLS footgun warning (DEV_PLAN_0.3.0.md §11).
    // SDES exchanges the master key over the signalling plane; if
    // SIP is plaintext UDP, the key is in the clear and SRTP gives
    // no actual confidentiality. Warn at load (not at every call)
    // so the operator notices once when they boot the misconfigured
    // daemon. Per-route SRTP overrides matter too — a default-Off /
    // route-Preferred config has the same hazard.
    let any_route_uses_srtp = routes.iter().any(|r| {
        r.media
            .srtp
            .as_deref()
            .is_some_and(|m| m == "preferred" || m == "required")
    });
    let srtp_active = media.srtp != siphon_ai_core::SrtpMode::Off || any_route_uses_srtp;
    let tls_listener_bound = sip
        .transports
        .iter()
        .any(|t| matches!(t, SipTransport::Tls));
    if srtp_active && !tls_listener_bound {
        warn!(
            target: "siphon_ai_config",
            "[media].srtp (or a route override) is not \"off\" but no SIP/TLS listener \
             is configured ([sip].transports does not include \"tls\"). \
             SDES keys would travel in plaintext on the SIP signalling plane — \
             SRTP gives no confidentiality in this configuration. Pair [media].srtp \
             with [sip.tls] for end-to-end protection."
        );
    }
    Ok(Config {
        node,
        sip,
        media,
        bridge_defaults,
        routes,
        registrations,
        trunks,
        security,
        recording,
        cdr,
        observability,
        webhooks,
        hep,
    })
}

fn compile_sip(raw: RawSip) -> Result<SipConfig, CompileError> {
    let listen_addr: SocketAddr = raw
        .listen
        .parse()
        .map_err(|e| CompileError::BadSipListen(raw.listen.clone(), e))?;
    if raw.transports.is_empty() {
        return Err(CompileError::NoTransports);
    }
    let mut transports = Vec::with_capacity(raw.transports.len());
    for name in &raw.transports {
        let t = match name.to_ascii_lowercase().as_str() {
            "udp" => SipTransport::Udp,
            "tcp" => SipTransport::Tcp,
            "tls" => SipTransport::Tls,
            _ => return Err(CompileError::UnknownTransport(name.clone())),
        };
        if !transports.contains(&t) {
            transports.push(t);
        }
    }

    let tls_enabled = transports.contains(&SipTransport::Tls);
    let tls = compile_sip_tls(raw.tls, tls_enabled, &listen_addr)?;

    let call_progress = compile_call_progress(raw.call_progress)?;

    // RFC 4028: 90 s is the floor (Min-SE). An operator setting
    // something smaller is asking for trouble; reject at load.
    let min_session_expires = match raw.min_session_expires_secs {
        None => Duration::from_secs(90),
        Some(n) if n < 90 => return Err(CompileError::SessionTimerMinSeTooSmall(n)),
        Some(n) => Duration::from_secs(n as u64),
    };
    let preferred_session_expires = raw
        .preferred_session_expires_secs
        .map(|n| Duration::from_secs(n as u64));

    Ok(SipConfig {
        listen_addr,
        transports,
        user_agent: raw.user_agent,
        contact: raw.contact,
        tls,
        call_progress,
        min_session_expires,
        preferred_session_expires,
    })
}

fn compile_call_progress(
    raw: crate::raw::RawCallProgress,
) -> Result<siphon_ai_core::CallProgressMode, CompileError> {
    match raw.mode.as_deref() {
        None | Some("instant_answer") => Ok(siphon_ai_core::CallProgressMode::InstantAnswer),
        Some("ringing") => Ok(siphon_ai_core::CallProgressMode::Ringing),
        Some("session_progress") => Ok(siphon_ai_core::CallProgressMode::SessionProgress),
        Some(other) => Err(CompileError::UnknownCallProgressMode(other.to_string())),
    }
}

fn compile_sip_tls(
    raw: RawSipTls,
    tls_enabled: bool,
    sip_listen: &SocketAddr,
) -> Result<Option<SipTlsConfig>, CompileError> {
    let has_any_tls_field = raw.cert.is_some() || raw.key.is_some() || raw.listen.is_some();

    if !tls_enabled {
        if has_any_tls_field {
            // Operator set `[sip.tls]` but didn't enable `tls` in
            // the transports list — that's almost always a typo
            // (their "tls" listen will silently never receive
            // traffic). Fail loud instead of silently ignoring.
            return Err(CompileError::SipTlsConfiguredButNotEnabled);
        }
        return Ok(None);
    }

    let cert_path = raw.cert.ok_or(CompileError::SipTlsCertRequired)?;
    if cert_path.is_empty() {
        return Err(CompileError::SipTlsCertRequired);
    }
    let key_path = raw.key.ok_or(CompileError::SipTlsKeyRequired)?;
    if key_path.is_empty() {
        return Err(CompileError::SipTlsKeyRequired);
    }

    // Default TLS listen: same host as the UDP/TCP listen, port
    // 5061 (SIPS standard per RFC 3261 §26.2.1).
    let listen_addr = match raw.listen {
        Some(s) => s.parse().map_err(|e| CompileError::BadSipTlsListen(s, e))?,
        None => SocketAddr::new(sip_listen.ip(), 5061),
    };

    Ok(Some(SipTlsConfig {
        listen_addr,
        cert_path: PathBuf::from(cert_path),
        key_path: PathBuf::from(key_path),
    }))
}

fn compile_node(raw: RawNode, sip: &SipConfig) -> Result<NodeConfig, CompileError> {
    let id = raw.id.unwrap_or_else(|| "siphon-ai".to_string());

    // When the operator didn't supply `[node].public_address`, fall
    // back to the SIP bind host — but ONLY if it's a real
    // routable address. An unspecified bind (`0.0.0.0` / `::`)
    // would otherwise leak into the SDP answer's `c=` line and
    // RTP from the caller goes nowhere. Refuse loudly rather than
    // silently misconfigure.
    let public_address = match raw.public_address {
        Some(addr) => addr,
        None => {
            let ip = sip.listen_addr.ip();
            if ip.is_unspecified() {
                return Err(CompileError::PublicAddressRequiredForWildcardListen(ip));
            }
            ip.to_string()
        }
    };

    Ok(NodeConfig { id, public_address })
}

fn compile_media(raw: &RawMedia) -> Result<MediaConfig, CompileError> {
    if let Some((min, max)) = raw.rtp_port_range {
        if min >= max || min % 2 != 0 {
            return Err(CompileError::BadRtpPortRange { min, max });
        }
    }
    let srtp = compile_srtp_mode(raw.srtp.as_deref())?;
    Ok(MediaConfig {
        rtp_port_range: raw.rtp_port_range,
        srtp,
    })
}

/// Translate the raw `[media].srtp` string into a typed
/// [`SrtpMode`][siphon_ai_core::SrtpMode]. `None` (unset) → `Off`,
/// matching v0.2.0 behaviour. Unknown values fail loud per
/// CLAUDE.md §4.6 — no silent fallback that would surprise an
/// operator after a typo.
pub(crate) fn compile_srtp_mode(
    raw: Option<&str>,
) -> Result<siphon_ai_core::SrtpMode, CompileError> {
    match raw {
        None | Some("off") => Ok(siphon_ai_core::SrtpMode::Off),
        Some("preferred") => Ok(siphon_ai_core::SrtpMode::Preferred),
        Some("required") => Ok(siphon_ai_core::SrtpMode::Required),
        Some(other) => Err(CompileError::UnknownSrtpMode(other.to_string())),
    }
}

/// Compile and validate `[security]`. Fails loud on contradictory config
/// (unknown attestation level / response code, a `min_attestation` set with
/// verification disabled, or a missing/empty trust-anchor file when
/// verification is enabled).
fn compile_security(raw: RawSecurity) -> Result<SecurityConfig, CompileError> {
    use siphon_ai_security::{
        validate_trust_anchors, MinAttestation, StirShakenConfig, DEFAULT_CERT_CACHE_TTL,
        DEFAULT_IAT_FRESHNESS,
    };

    let min_attestation = match raw.min_attestation.as_deref() {
        None | Some("") => MinAttestation::None,
        Some(s) => MinAttestation::parse(s)
            .ok_or_else(|| CompileError::UnknownMinAttestation(s.to_string()))?,
    };

    let min_attestation_response = match raw.min_attestation_response {
        None => 403,
        Some(code @ (403 | 488 | 606)) => code,
        Some(other) => return Err(CompileError::InvalidMinAttestationResponse(other)),
    };

    let stir = raw.stir_shaken;
    let enabled = stir.enabled.unwrap_or(false);
    let trust_anchors = stir.trust_anchors.map(PathBuf::from).unwrap_or_default();
    let cert_cache_ttl = stir
        .cert_cache_ttl_secs
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_CERT_CACHE_TTL);
    let require_identity = stir.require_identity.unwrap_or(false);
    // `iat_freshness_secs = 0` is a deliberate "disable the check" value, so
    // map it straight through (Duration::ZERO); absent → the 60 s default.
    let iat_freshness = stir
        .iat_freshness_secs
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_IAT_FRESHNESS);
    let x5u_tls_extra_ca = stir.x5u_tls_extra_ca.map(PathBuf::from);

    // A minimum-attestation gate is meaningless without verification: with
    // stir_shaken off, no call yields a trusted attestation, so the gate
    // would 4xx every call. Fail loud rather than black-hole all traffic.
    if min_attestation != MinAttestation::None && !enabled {
        return Err(CompileError::MinAttestationWithoutStirShaken(
            min_attestation_label(min_attestation).to_string(),
        ));
    }

    // Validate the trust-anchor file at load time when verification is on,
    // so a missing/empty bundle is a startup failure, not a per-call one.
    if enabled {
        if trust_anchors.as_os_str().is_empty() {
            return Err(CompileError::StirShakenTrustAnchorsRequired);
        }
        validate_trust_anchors(&trust_anchors).map_err(|err| {
            CompileError::StirShakenTrustAnchorsInvalid {
                path: trust_anchors.display().to_string(),
                err: err.to_string(),
            }
        })?;

        // The supplemental x5u-fetch CA, when set, gets the same load-time
        // existence + ≥1-cert check so a typo fails at startup.
        if let Some(extra) = &x5u_tls_extra_ca {
            validate_trust_anchors(extra).map_err(|err| {
                CompileError::StirShakenExtraCaInvalid {
                    path: extra.display().to_string(),
                    err: err.to_string(),
                }
            })?;
        }
    }

    Ok(SecurityConfig {
        min_attestation,
        min_attestation_response,
        stir_shaken: StirShakenConfig {
            enabled,
            trust_anchors,
            cert_cache_ttl,
            require_identity,
            iat_freshness,
            x5u_tls_extra_ca,
        },
    })
}

/// Compile and validate `[recording]`. Off by default. When recording is
/// on, the output directory is required and **created at load** so a bad
/// path fails loud at startup, not on the first recorded call.
fn compile_recording(
    raw: RawRecording,
) -> Result<siphon_ai_recording::RecordingConfig, CompileError> {
    use siphon_ai_recording::{RecordingConfig, RecordingMode};
    let mode = match raw.mode.as_deref() {
        None | Some("") | Some("off") => RecordingMode::Off,
        Some("always") => RecordingMode::Always,
        Some("on_demand") => RecordingMode::OnDemand,
        Some(other) => return Err(CompileError::UnknownRecordingMode(other.to_string())),
    };
    let dir = raw.dir.map(PathBuf::from).unwrap_or_default();
    if mode != RecordingMode::Off && dir.as_os_str().is_empty() {
        return Err(CompileError::RecordingDirRequired);
    }
    // Create the dir at load whenever one is configured — even with the
    // global mode `off`, a per-route `[route.recording]` may enable
    // recording, so the directory must already exist. A bad path fails loud
    // here, not on the first recorded call.
    if !dir.as_os_str().is_empty() {
        std::fs::create_dir_all(&dir).map_err(|err| CompileError::RecordingDirInvalid {
            path: dir.display().to_string(),
            err: err.to_string(),
        })?;
    }
    Ok(RecordingConfig { mode, dir })
}

fn min_attestation_label(m: siphon_ai_security::MinAttestation) -> &'static str {
    use siphon_ai_security::MinAttestation::*;
    match m {
        None => "none",
        A => "A",
        B => "B",
        C => "C",
    }
}

fn compile_bridge(raw: RawBridge, media: &RawMedia) -> Result<BridgeDefaults, CompileError> {
    let codecs = match media.codecs.as_ref() {
        None => default_codecs(),
        Some(names) => parse_codecs(names)?,
    };
    let dtmf_payload_type = match media.dtmf.as_deref() {
        None | Some("rfc2833") => Some(101),
        Some("off") => None,
        Some(other) => return Err(CompileError::UnknownDtmfMode(other.to_string())),
    };
    let connect_timeout = raw
        .ws_connect_timeout_ms
        .map(Duration::from_millis)
        .unwrap_or_else(|| Duration::from_secs(5));
    let auth_header = raw
        .ws_auth_header
        .as_deref()
        .map(normalize_auth_header)
        .filter(|s| !s.is_empty());

    let barge_in = compile_barge_in_default(&raw.barge_in)?;

    // `None` → 60 s default; `Some(0)` → watchdog off. The merge
    // step in `resolve_inactivity_timeout` handles per-route 0 →
    // disabled the same way.
    let inactivity_timeout = match media.inactivity_timeout_secs {
        None => Some(Duration::from_secs(60)),
        Some(0) => None,
        Some(n) => Some(Duration::from_secs(n)),
    };

    // Silence / dead-air primitives — §9.2 defaults. Same shape as
    // inactivity_timeout: `None` = use default, `Some(0)` = disable,
    // `Some(n)` = `n` ms.
    let silence_threshold = match raw.silence_threshold_ms {
        None => Some(Duration::from_millis(3000)),
        Some(0) => None,
        Some(ms) => Some(Duration::from_millis(ms)),
    };
    let dead_air_threshold = match raw.dead_air_threshold_ms {
        None => Some(Duration::from_millis(10000)),
        Some(0) => None,
        Some(ms) => Some(Duration::from_millis(ms)),
    };
    // §9.3 default 5000 ms mirrors RTCP §6.2 compound-report cadence.
    let rtp_stats_interval = match raw.rtp_stats_interval_ms {
        None => Some(Duration::from_millis(5000)),
        Some(0) => None,
        Some(ms) => Some(Duration::from_millis(ms)),
    };

    // `[media].srtp` resolved to the typed enum form via the same
    // strict-matching path the route-level override uses
    // (`compile_srtp_mode`). Default — and any unset value — is `Off`.
    let srtp_mode = compile_srtp_mode(media.srtp.as_deref())?;

    // `[bridge.tls]` — mTLS for the WS leg. Validation at compile
    // time so cert/key issues surface at daemon startup, not on the
    // first call that tries to use them.
    let bridge_tls = match raw.tls.as_ref() {
        None => None,
        Some(raw_tls) => Some(siphon_ai_bridge::tls::BridgeTlsConfig::from_paths(
            std::path::Path::new(&raw_tls.client_cert),
            std::path::Path::new(&raw_tls.client_key),
            raw_tls.pinned_sha256.as_deref(),
        )?),
    };

    Ok(BridgeDefaults {
        ws_url: raw.ws_url.filter(|s| !s.is_empty()),
        auth_header,
        connect_timeout,
        codecs,
        dtmf_payload_type,
        forward_headers: raw.forward_headers.unwrap_or_default(),
        barge_in,
        inactivity_timeout,
        silence_threshold,
        dead_air_threshold,
        rtp_stats_interval,
        srtp_mode,
        bridge_tls,
    })
}

/// Translate `[bridge.barge_in]` into a resolved
/// [`siphon_ai_core::BargeInConfig`]. Defaults follow `BargeInConfig`'s
/// own `Default` (`enabled = true`, `mode = AutoClear`); only fields
/// the operator set in TOML override.
fn compile_barge_in_default(
    raw: &crate::raw::RawBargeIn,
) -> Result<siphon_ai_core::BargeInConfig, CompileError> {
    let mut cfg = siphon_ai_core::BargeInConfig::default();
    if let Some(enabled) = raw.enabled {
        cfg.enabled = enabled;
    }
    if let Some(mode) = raw.mode.as_deref() {
        cfg.mode = parse_barge_in_mode(mode)?;
    }
    Ok(cfg)
}

fn parse_barge_in_mode(s: &str) -> Result<siphon_ai_core::BargeInMode, CompileError> {
    match s {
        "auto_clear" => Ok(siphon_ai_core::BargeInMode::AutoClear),
        "notify_only" => Ok(siphon_ai_core::BargeInMode::NotifyOnly),
        other => Err(CompileError::UnknownBargeInMode(other.to_string())),
    }
}

fn parse_codecs(names: &[String]) -> Result<Vec<Codec>, CompileError> {
    let mut out = Vec::with_capacity(names.len());
    for name in names {
        let codec = Codec::from_encoding_name(name)
            .ok_or_else(|| CompileError::UnknownCodec(name.clone()))?;
        // The bridge audio path only handles 8 kHz and 16 kHz PCM16
        // (CLAUDE.md §4.2 pins the WS protocol's audio shape). Opus
        // produces 48 kHz; without resampling in the media engine
        // the call would 488 at SDP-negotiate time or — worse — 200
        // OK then fail at tap attach. Refuse at load time so the
        // operator sees the limitation before a single INVITE.
        if !is_ws_compatible(codec) {
            return Err(CompileError::CodecRequiresResampling(name.clone()));
        }
        if !out.contains(&codec) {
            out.push(codec);
        }
    }
    Ok(out)
}

/// Codecs whose post-decode PCM rate the WS audio path supports.
/// G.722 is 16 kHz audio despite its 8 kHz rtpmap quirk; the rest of
/// our supported set is 8 kHz. Opus is the lone outlier today.
fn is_ws_compatible(codec: Codec) -> bool {
    matches!(codec.audio_sample_rate(), 8000 | 16000)
}

fn default_codecs() -> Vec<Codec> {
    vec![Codec::Pcmu, Codec::Pcma]
}

fn compile_dialplan(routes: Vec<siphon_ai_routes::RawRoute>) -> Result<RouteSet, CompileError> {
    let raw_file = RawRouteFile { routes };
    let set = compile_routes(raw_file)?;
    // Walk each route's `[route.bridge]` overrides for fields that
    // need an enum check beyond what the routes crate already does.
    // Today: `on_ws_failure` is "hangup" only — the play_prompt
    // path needs a forge-driven prompt player that isn't built.
    for route in set.iter() {
        if let Some(mode) = route.bridge.on_ws_failure.as_deref() {
            if !mode.eq_ignore_ascii_case("hangup") {
                return Err(CompileError::UnknownOnWsFailure {
                    route: route.name.clone(),
                    value: mode.to_string(),
                });
            }
        }
        // Validate the per-route attestation override at load time so a typo
        // is a startup failure, not a silent runtime fall-back. The
        // cross-check against stir_shaken.enabled happens in `compile`,
        // which has the global security config in scope.
        if let Some(value) = route.security.min_attestation.as_deref() {
            if siphon_ai_security::MinAttestation::parse(value).is_none() {
                return Err(CompileError::UnknownRouteMinAttestation {
                    route: route.name.clone(),
                    value: value.to_string(),
                });
            }
        }
        // Validate the per-route recording override the same way. The
        // dir-required cross-check happens in `compile` (global scope).
        if let Some(value) = route.recording.mode.as_deref() {
            if !matches!(value, "off" | "always" | "on_demand") {
                return Err(CompileError::RouteRecordingModeInvalid {
                    route: route.name.clone(),
                    value: value.to_string(),
                });
            }
        }
    }
    Ok(set)
}

fn compile_webhooks(raw: RawWebhooks) -> Result<WebhooksConfig, CompileError> {
    if !raw.enabled {
        // Master switch off — sub-block misconfig tolerated, same
        // pattern as [cdr] / [observability].
        return Ok(WebhooksConfig::default());
    }
    let url = raw.url.ok_or(CompileError::WebhooksUrlRequired)?;
    if url.is_empty() {
        return Err(CompileError::WebhooksUrlRequired);
    }
    Ok(WebhooksConfig {
        enabled: true,
        url: Some(url),
        auth_header: raw.auth_header.filter(|s| !s.is_empty()),
        events: raw.events.unwrap_or_default(),
        retry_max: raw.retry_max.unwrap_or(3),
        timeout: Duration::from_millis(raw.timeout_ms.unwrap_or(5000)),
    })
}

fn compile_trunks(raw: Vec<crate::raw::RawTrunk>) -> Result<Vec<TrunkConfig>, CompileError> {
    let mut compiled: Vec<TrunkConfig> = Vec::with_capacity(raw.len());
    for (i, t) in raw.into_iter().enumerate() {
        if t.name.trim().is_empty() {
            return Err(CompileError::TrunkEmptyName { index: i });
        }
        for (j, prior) in compiled.iter().enumerate() {
            if prior.name == t.name {
                return Err(CompileError::TrunkDuplicateName {
                    name: t.name.clone(),
                    first: j,
                    second: i,
                });
            }
        }
        let peer_addrs = match t.peer_addrs {
            None => Vec::new(),
            Some(values) => {
                let mut out = Vec::with_capacity(values.len());
                for v in values {
                    let cidr =
                        TrunkCidr::parse(&v).map_err(|err| CompileError::TrunkBadPeerAddr {
                            name: t.name.clone(),
                            value: v.clone(),
                            err,
                        })?;
                    out.push(cidr);
                }
                out
            }
        };
        let from_hosts = match t.from_hosts {
            None => Vec::new(),
            Some(values) => {
                let mut out = Vec::with_capacity(values.len());
                for v in values {
                    if v.trim().is_empty() {
                        return Err(CompileError::TrunkEmptyFromHost {
                            name: t.name.clone(),
                            value: v,
                        });
                    }
                    out.push(v.trim().to_ascii_lowercase());
                }
                out
            }
        };
        if peer_addrs.is_empty() && from_hosts.is_empty() {
            return Err(CompileError::TrunkNoMatchCriteria { name: t.name });
        }
        compiled.push(TrunkConfig {
            name: t.name,
            peer_addrs,
            from_hosts,
        });
    }
    Ok(compiled)
}

fn compile_registrations(raw: Vec<RawRegister>) -> Result<Vec<RegisterConfig>, CompileError> {
    let mut compiled = Vec::with_capacity(raw.len());
    for (i, r) in raw.into_iter().enumerate() {
        if r.name.trim().is_empty() {
            return Err(CompileError::RegisterEmptyName { index: i });
        }
        for (j, prior) in compiled.iter().enumerate() {
            let prior: &RegisterConfig = prior;
            if prior.name == r.name {
                return Err(CompileError::RegisterDuplicateName {
                    name: r.name.clone(),
                    first: j,
                    second: i,
                });
            }
        }

        let transport = match r
            .transport
            .as_deref()
            .unwrap_or("udp")
            .to_ascii_lowercase()
            .as_str()
        {
            "udp" => SipTransport::Udp,
            "tcp" => SipTransport::Tcp,
            "tls" => SipTransport::Tls,
            other => {
                return Err(CompileError::RegisterUnknownTransport {
                    name: r.name.clone(),
                    transport: other.to_string(),
                })
            }
        };

        let default_port = match transport {
            SipTransport::Tls => 5061,
            _ => 5060,
        };
        let (server_host, server_port) = parse_register_server(&r.server, r.port, default_port)
            .map_err(|err| CompileError::RegisterBadServer {
                name: r.name.clone(),
                server: r.server.clone(),
                err,
            })?;
        // We resolve a literal IP; DNS lookups happen at runtime.
        // For configs that supply a hostname, the daemon's UAC
        // resolver kicks in — but to keep `server_addr` typed, we
        // accept literal IPs here and surface a clear error for
        // hostnames the user can fix later. (DNS-resolved
        // registrars are a v1.1 feature.)
        let ip = server_host.parse().map_err(|e: std::net::AddrParseError| {
            CompileError::RegisterBadServer {
                name: r.name.clone(),
                server: r.server.clone(),
                err: format!(
                    "{e} (v1 only accepts literal IP addresses for [[register]].server; \
                     hostname resolution lands in v1.1)"
                ),
            }
        })?;
        let server_addr = SocketAddr::new(ip, server_port);

        compiled.push(RegisterConfig {
            name: r.name,
            server_addr,
            server_host,
            transport,
            auth_username: r.auth_username.unwrap_or_else(|| r.username.clone()),
            username: r.username,
            password: r.password,
            realm: r.realm,
            expires: Duration::from_secs(r.expires_secs.unwrap_or(3600) as u64),
            register_on_startup: r.register_on_startup.unwrap_or(true),
        });
    }
    Ok(compiled)
}

/// Split the configured `server` into `(host_str, port)`. The
/// `server` field accepts three shapes:
///
/// - `"host"` or `"1.2.3.4"` — bare host/IPv4, port from
///   `explicit_port` or `default_port`.
/// - `"host:5061"` or `"1.2.3.4:5061"` — host + port. Explicit
///   `port` (the `[[register]].port` TOML field) wins if also set.
/// - `"2001:db8::1"` — bare IPv6 literal (no port). The function
///   recognises this by parsing the whole string as an [`IpAddr`].
/// - `"[2001:db8::1]:5061"` — bracketed IPv6 + port, RFC 3986 §3.2.2.
///
/// Previously the function did `server.rsplit_once(':')` and
/// parsed the right half as a port unconditionally, which
/// misparsed `"2001:db8::1"` as host=`"2001:db8:"` + port=`":1"`
/// (the latter then failing as a number). IPv6 registrars were
/// effectively impossible to configure even though the documented
/// "v1 accepts only literal IPs" contract didn't exclude IPv6.
fn parse_register_server(
    server: &str,
    explicit_port: Option<u16>,
    default_port: u16,
) -> Result<(String, u16), String> {
    use std::net::IpAddr;

    let trimmed = server.trim();
    if trimmed.is_empty() {
        return Err("server must not be empty".into());
    }

    // Bracketed IPv6: `[2001:db8::1]` or `[2001:db8::1]:5061`.
    if let Some(rest) = trimmed.strip_prefix('[') {
        let (inside, after) = rest
            .split_once(']')
            .ok_or_else(|| format!("unterminated '[' in server {trimmed:?}"))?;
        let port_in_str = match after {
            "" => None,
            other => {
                let p = other.strip_prefix(':').ok_or_else(|| {
                    format!("expected ':<port>' after ']' in server {trimmed:?}, got {other:?}")
                })?;
                Some(
                    p.parse::<u16>()
                        .map_err(|e: std::num::ParseIntError| e.to_string())?,
                )
            }
        };
        let port = explicit_port.or(port_in_str).unwrap_or(default_port);
        return Ok((inside.to_string(), port));
    }

    // Bare IPv6 with no port (e.g. `2001:db8::1`). Detected by a
    // whole-string IPv6 parse — that's the only shape with `:`
    // characters that's also a valid IP literal.
    if trimmed.parse::<IpAddr>().is_ok() && trimmed.contains(':') {
        // Pure IPv6 literal. Port comes from explicit_port / default.
        return Ok((trimmed.to_string(), explicit_port.unwrap_or(default_port)));
    }

    // Host or IPv4, optionally with `:port`. Splitting on the
    // rightmost `:` is safe here because we've already excluded
    // IPv6 above.
    let (host, port_in_str) = match trimmed.rsplit_once(':') {
        Some((h, p)) => {
            let parsed: u16 = p
                .parse()
                .map_err(|e: std::num::ParseIntError| e.to_string())?;
            (h.to_string(), Some(parsed))
        }
        None => (trimmed.to_string(), None),
    };
    let port = explicit_port.or(port_in_str).unwrap_or(default_port);
    Ok((host, port))
}

fn compile_observability(raw: RawObservability) -> Result<ObservabilityConfig, CompileError> {
    if !raw.enabled {
        // Disabled means "don't spawn the HTTP server" — sub-block
        // misconfig is tolerated (same shape as [cdr] master switch
        // — operators can flip enabled = false to silence a flaky
        // listener without re-editing every field).
        return Ok(ObservabilityConfig::default());
    }
    let listen_str = raw
        .http_listen
        .ok_or(CompileError::ObservabilityListenRequired)?;
    if listen_str.is_empty() {
        return Err(CompileError::ObservabilityListenRequired);
    }
    let http_listen = listen_str
        .parse()
        .map_err(|e| CompileError::BadObservabilityListen(listen_str.clone(), e))?;
    Ok(ObservabilityConfig {
        enabled: true,
        http_listen: Some(http_listen),
    })
}

fn compile_hep(raw: RawHep) -> Result<HepConfig, CompileError> {
    // Same "disabled = stop here" semantics as observability. Lets
    // operators flip the master switch off without re-validating the
    // sub-fields they may have left stale.
    if !raw.enabled {
        return Ok(HepConfig::default());
    }

    let collector_str = raw.collector.ok_or(CompileError::HepCollectorRequired)?;
    if collector_str.is_empty() {
        return Err(CompileError::HepCollectorRequired);
    }
    // Accept either a literal `host:port` socket address or a
    // `hostname:port` that resolves via the system resolver. The
    // hostname path is what makes service-discovery-style configs
    // work (`host.docker.internal:9060`, `homer.internal:9060`,
    // etc.) — without it operators have to bake the IP into
    // config, which is painful in container deployments.
    //
    // We resolve once at startup, not per packet — HEP is a long-
    // lived UDP socket. If the collector moves, restart the
    // daemon.
    let collector = match collector_str.parse::<std::net::SocketAddr>() {
        Ok(addr) => addr,
        Err(_) => std::net::ToSocketAddrs::to_socket_addrs(collector_str.as_str())
            .map_err(|e| CompileError::BadHepCollectorResolve(collector_str.clone(), e))?
            .next()
            .ok_or_else(|| CompileError::HepCollectorResolveEmpty(collector_str.clone()))?,
    };

    let capture_id = raw.capture_id.ok_or(CompileError::HepCaptureIdRequired)?;

    Ok(HepConfig {
        enabled: true,
        collector: Some(collector),
        capture_id: Some(capture_id),
        capture_password: raw.capture_password,
        // 256 matches hep_rs::DEFAULT_QUEUE_CAPACITY. We re-declare
        // it here rather than depending on hep-rs from this crate
        // (config has a deliberately minimal dep graph).
        queue_capacity: raw.queue_capacity.unwrap_or(256),
    })
}

fn compile_cdr(raw: RawCdr) -> Result<CdrConfig, CompileError> {
    if !raw.enabled {
        // Whole CDR pipeline off; sub-block config is parsed but
        // ignored. Validating disabled sub-blocks would surprise
        // operators who flip `enabled = false` to silence a
        // misconfig while they investigate.
        return Ok(CdrConfig::default());
    }
    let file = if raw.file.enabled {
        let path = raw.file.path.ok_or(CompileError::CdrFilePathRequired)?;
        if path.is_empty() {
            return Err(CompileError::CdrFilePathRequired);
        }
        Some(CdrFileConfig {
            path: PathBuf::from(path),
        })
    } else {
        None
    };
    let webhook = if raw.webhook.enabled {
        let url = raw.webhook.url.ok_or(CompileError::CdrWebhookUrlRequired)?;
        if url.is_empty() {
            return Err(CompileError::CdrWebhookUrlRequired);
        }
        Some(CdrWebhookConfig {
            url,
            auth_header: raw.webhook.auth_header.filter(|s| !s.is_empty()),
            retry_max: raw.webhook.retry_max.unwrap_or(3),
            timeout: Duration::from_millis(raw.webhook.timeout_ms.unwrap_or(5000)),
        })
    } else {
        None
    };
    Ok(CdrConfig {
        enabled: true,
        file,
        webhook,
    })
}

#[cfg(test)]
mod call_progress_tests {
    use super::{compile_call_progress, CompileError};
    use crate::raw::RawCallProgress;
    use siphon_ai_core::CallProgressMode;

    fn raw(mode: Option<&str>) -> RawCallProgress {
        RawCallProgress {
            mode: mode.map(str::to_string),
        }
    }

    #[test]
    fn missing_block_defaults_to_instant_answer() {
        assert_eq!(
            compile_call_progress(raw(None)).unwrap(),
            CallProgressMode::InstantAnswer
        );
    }

    #[test]
    fn explicit_modes_parse() {
        assert_eq!(
            compile_call_progress(raw(Some("instant_answer"))).unwrap(),
            CallProgressMode::InstantAnswer
        );
        assert_eq!(
            compile_call_progress(raw(Some("ringing"))).unwrap(),
            CallProgressMode::Ringing
        );
        assert_eq!(
            compile_call_progress(raw(Some("session_progress"))).unwrap(),
            CallProgressMode::SessionProgress
        );
    }

    #[test]
    fn unknown_mode_errors() {
        let err = compile_call_progress(raw(Some("instant-answer"))).unwrap_err();
        match err {
            CompileError::UnknownCallProgressMode(s) => assert_eq!(s, "instant-answer"),
            other => panic!("expected UnknownCallProgressMode, got {other:?}"),
        }
    }
}

#[cfg(test)]
mod srtp_mode_tests {
    use super::{compile_srtp_mode, CompileError};
    use siphon_ai_core::SrtpMode;

    #[test]
    fn missing_defaults_to_off() {
        // Critical for backwards-compat: a 0.2.0 config without
        // [media].srtp must keep producing plaintext-only behaviour
        // after upgrade.
        assert_eq!(compile_srtp_mode(None).unwrap(), SrtpMode::Off);
    }

    #[test]
    fn explicit_modes_parse() {
        assert_eq!(compile_srtp_mode(Some("off")).unwrap(), SrtpMode::Off);
        assert_eq!(
            compile_srtp_mode(Some("preferred")).unwrap(),
            SrtpMode::Preferred
        );
        assert_eq!(
            compile_srtp_mode(Some("required")).unwrap(),
            SrtpMode::Required
        );
    }

    #[test]
    fn unknown_mode_errors() {
        // Typos must fail loud at config load (CLAUDE.md §4.6) —
        // silently treating "ON" as "off" would surprise an
        // operator who thought they'd turned encryption on.
        let err = compile_srtp_mode(Some("ON")).unwrap_err();
        match err {
            CompileError::UnknownSrtpMode(s) => assert_eq!(s, "ON"),
            other => panic!("expected UnknownSrtpMode, got {other:?}"),
        }
    }

    #[test]
    fn case_is_significant() {
        // We document the modes as lowercase; "Off" / "OFF" / "PrEfErReD"
        // are typos, not aliases. Lock that in.
        assert!(matches!(
            compile_srtp_mode(Some("Off")),
            Err(CompileError::UnknownSrtpMode(_))
        ));
        assert!(matches!(
            compile_srtp_mode(Some("PREFERRED")),
            Err(CompileError::UnknownSrtpMode(_))
        ));
    }
}

#[cfg(test)]
mod recording_tests {
    use super::{compile_dialplan, compile_recording, CompileError, RawRecording};
    use siphon_ai_recording::RecordingMode;
    use siphon_ai_routes::{RawRoute, RawRouteMatch, RecordingOverride};

    fn raw(mode: Option<&str>, dir: Option<&str>) -> RawRecording {
        RawRecording {
            mode: mode.map(str::to_string),
            dir: dir.map(str::to_string),
        }
    }

    #[test]
    fn default_is_off() {
        let c = compile_recording(RawRecording::default()).unwrap();
        assert_eq!(c.mode, RecordingMode::Off);
    }

    #[test]
    fn modes_parse() {
        let dir = std::env::temp_dir().join("siphon_rec_cfg_test");
        let d = dir.to_str();
        assert_eq!(
            compile_recording(raw(Some("always"), d)).unwrap().mode,
            RecordingMode::Always
        );
        assert_eq!(
            compile_recording(raw(Some("on_demand"), d)).unwrap().mode,
            RecordingMode::OnDemand
        );
    }

    #[test]
    fn unknown_mode_fails_loud() {
        assert!(matches!(
            compile_recording(raw(Some("sometimes"), Some("/tmp/x"))),
            Err(CompileError::UnknownRecordingMode(s)) if s == "sometimes"
        ));
    }

    #[test]
    fn enabled_requires_dir() {
        assert!(matches!(
            compile_recording(raw(Some("always"), None)),
            Err(CompileError::RecordingDirRequired)
        ));
    }

    #[test]
    fn per_route_invalid_mode_fails_loud() {
        let route = RawRoute {
            name: "r".into(),
            match_: RawRouteMatch {
                any: true,
                ..Default::default()
            },
            bridge: Default::default(),
            media: Default::default(),
            security: Default::default(),
            recording: RecordingOverride {
                mode: Some("sometimes".into()),
            },
        };
        assert!(matches!(
            compile_dialplan(vec![route]),
            Err(CompileError::RouteRecordingModeInvalid { value, .. }) if value == "sometimes"
        ));
    }
}

#[cfg(test)]
mod security_tests {
    use super::{compile_security, CompileError, SecurityConfig};
    use crate::raw::{RawSecurity, RawStirShaken};
    use siphon_ai_security::{MinAttestation, DEFAULT_CERT_CACHE_TTL};

    #[test]
    fn default_is_inert() {
        let c = compile_security(RawSecurity::default()).unwrap();
        assert_eq!(c.min_attestation, MinAttestation::None);
        assert_eq!(c.min_attestation_response, 403);
        assert!(!c.stir_shaken.enabled);
        assert_eq!(c.stir_shaken.cert_cache_ttl, DEFAULT_CERT_CACHE_TTL);
        assert_eq!(c, SecurityConfig::default());
    }

    #[test]
    fn unknown_min_attestation_fails_loud() {
        let raw = RawSecurity {
            min_attestation: Some("strong".into()),
            ..Default::default()
        };
        assert!(matches!(
            compile_security(raw),
            Err(CompileError::UnknownMinAttestation(s)) if s == "strong"
        ));
    }

    #[test]
    fn invalid_response_code_rejected() {
        let raw = RawSecurity {
            min_attestation_response: Some(500),
            ..Default::default()
        };
        assert!(matches!(
            compile_security(raw),
            Err(CompileError::InvalidMinAttestationResponse(500))
        ));
    }

    #[test]
    fn min_attestation_without_stir_shaken_rejected() {
        // A gate with verification off would 4xx every call — fail loud.
        let raw = RawSecurity {
            min_attestation: Some("B".into()),
            ..Default::default()
        };
        assert!(matches!(
            compile_security(raw),
            Err(CompileError::MinAttestationWithoutStirShaken(s)) if s == "B"
        ));
    }

    #[test]
    fn enabled_requires_trust_anchors() {
        let raw = RawSecurity {
            stir_shaken: RawStirShaken {
                enabled: Some(true),
                trust_anchors: None,
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(matches!(
            compile_security(raw),
            Err(CompileError::StirShakenTrustAnchorsRequired)
        ));
    }

    #[test]
    fn enabled_with_missing_anchor_file_rejected() {
        let raw = RawSecurity {
            stir_shaken: RawStirShaken {
                enabled: Some(true),
                trust_anchors: Some("/nonexistent/sti-pa-roots.pem".into()),
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(matches!(
            compile_security(raw),
            Err(CompileError::StirShakenTrustAnchorsInvalid { .. })
        ));
    }

    #[test]
    fn valid_enabled_config_compiles() {
        // Write a throwaway PEM bundle so the load-time anchor check passes.
        let path = std::env::temp_dir().join("siphon_security_test_anchors.pem");
        std::fs::write(
            &path,
            "-----BEGIN CERTIFICATE-----\nMIIB...\n-----END CERTIFICATE-----\n",
        )
        .unwrap();

        let raw = RawSecurity {
            min_attestation: Some("B".into()),
            min_attestation_response: Some(606),
            stir_shaken: RawStirShaken {
                enabled: Some(true),
                trust_anchors: Some(path.to_string_lossy().into_owned()),
                cert_cache_ttl_secs: Some(1800),
                require_identity: Some(true),
                iat_freshness_secs: Some(30),
                x5u_tls_extra_ca: None,
            },
        };
        let c = compile_security(raw).unwrap();
        assert_eq!(c.min_attestation, MinAttestation::B);
        assert_eq!(c.min_attestation_response, 606);
        assert!(c.stir_shaken.enabled);
        assert!(c.stir_shaken.require_identity);
        assert_eq!(
            c.stir_shaken.cert_cache_ttl,
            std::time::Duration::from_secs(1800)
        );
        assert_eq!(
            c.stir_shaken.iat_freshness,
            std::time::Duration::from_secs(30)
        );
        assert_eq!(c.stir_shaken.trust_anchors, path);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn invalid_x5u_extra_ca_rejected_at_load() {
        // A valid trust-anchor bundle but a bogus x5u_tls_extra_ca path →
        // loud failure at config load (only checked when enabled).
        let anchors = std::env::temp_dir().join("siphon_x5u_ca_test_anchors.pem");
        std::fs::write(
            &anchors,
            "-----BEGIN CERTIFICATE-----\nMIIB...\n-----END CERTIFICATE-----\n",
        )
        .unwrap();
        let raw = RawSecurity {
            stir_shaken: RawStirShaken {
                enabled: Some(true),
                trust_anchors: Some(anchors.to_string_lossy().into_owned()),
                x5u_tls_extra_ca: Some("/nonexistent/lab-ca.pem".into()),
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(matches!(
            compile_security(raw),
            Err(CompileError::StirShakenExtraCaInvalid { .. })
        ));
        let _ = std::fs::remove_file(&anchors);
    }

    #[test]
    fn iat_freshness_defaults_and_zero_disables() {
        // Absent → 60 s default.
        let c = compile_security(RawSecurity::default()).unwrap();
        assert_eq!(
            c.stir_shaken.iat_freshness,
            std::time::Duration::from_secs(60)
        );
        // 0 maps straight through (disables the check), not the default.
        let raw = RawSecurity {
            stir_shaken: RawStirShaken {
                iat_freshness_secs: Some(0),
                ..Default::default()
            },
            ..Default::default()
        };
        let c = compile_security(raw).unwrap();
        assert_eq!(c.stir_shaken.iat_freshness, std::time::Duration::ZERO);
    }

    #[test]
    fn shipped_trust_anchor_template_ships_empty_and_fails_loud() {
        // The contrib bundle ships as a template with zero certificates so an
        // operator who enables verification before populating it gets a loud
        // startup failure — never silent. This also guards against anyone
        // accidentally vendoring a real (or malformed) root: the decision is
        // "operator supplies the anchor", and a change here forces it to be
        // conscious.
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../contrib/sti-pa-roots.pem"
        );
        let err = siphon_ai_security::validate_trust_anchors(std::path::Path::new(path))
            .expect_err("template must contain no certificates");
        assert!(matches!(
            err,
            siphon_ai_security::TrustAnchorError::NoCertificates { .. }
        ));
    }
}

#[cfg(test)]
mod parse_register_server_tests {
    use super::parse_register_server;

    // Tests for the IPv6 fix on `parse_register_server`. The pre-fix
    // implementation did `server.rsplit_once(':')` and parsed the
    // right half as a u16 port, which:
    //   * misparsed `"2001:db8::1"` as host=`"2001:db8:"` port=":1"
    //     (the latter failing as a number);
    //   * had no concept of the bracketed `[host]:port` form.

    const SIP_PORT: u16 = 5060;

    #[test]
    fn ipv4_host_only_uses_default_port() {
        let (h, p) = parse_register_server("10.0.0.10", None, SIP_PORT).unwrap();
        assert_eq!((h.as_str(), p), ("10.0.0.10", SIP_PORT));
    }

    #[test]
    fn ipv4_host_port_parses() {
        let (h, p) = parse_register_server("10.0.0.10:5061", None, SIP_PORT).unwrap();
        assert_eq!((h.as_str(), p), ("10.0.0.10", 5061));
    }

    #[test]
    fn explicit_port_wins_over_inline() {
        let (h, p) = parse_register_server("10.0.0.10:5061", Some(5070), SIP_PORT).unwrap();
        assert_eq!((h.as_str(), p), ("10.0.0.10", 5070));
    }

    #[test]
    fn hostname_only_uses_default_port() {
        let (h, p) = parse_register_server("sip.example.com", None, SIP_PORT).unwrap();
        assert_eq!((h.as_str(), p), ("sip.example.com", SIP_PORT));
    }

    #[test]
    fn ipv6_literal_uses_default_port() {
        let (h, p) = parse_register_server("2001:db8::1", None, SIP_PORT).unwrap();
        assert_eq!((h.as_str(), p), ("2001:db8::1", SIP_PORT));
    }

    #[test]
    fn ipv6_literal_with_explicit_port() {
        let (h, p) = parse_register_server("2001:db8::1", Some(5061), SIP_PORT).unwrap();
        assert_eq!((h.as_str(), p), ("2001:db8::1", 5061));
    }

    #[test]
    fn ipv6_bracketed_host_port_parses() {
        let (h, p) = parse_register_server("[2001:db8::1]:5061", None, SIP_PORT).unwrap();
        assert_eq!((h.as_str(), p), ("2001:db8::1", 5061));
    }

    #[test]
    fn ipv6_bracketed_no_port_uses_default() {
        let (h, p) = parse_register_server("[2001:db8::1]", None, SIP_PORT).unwrap();
        assert_eq!((h.as_str(), p), ("2001:db8::1", SIP_PORT));
    }

    #[test]
    fn ipv6_bracketed_missing_close_bracket_errors() {
        assert!(parse_register_server("[2001:db8::1", None, SIP_PORT).is_err());
    }

    #[test]
    fn ipv6_bracketed_garbage_after_close_bracket_errors() {
        assert!(parse_register_server("[2001:db8::1]junk", None, SIP_PORT).is_err());
    }

    #[test]
    fn empty_server_errors() {
        assert!(parse_register_server("", None, SIP_PORT).is_err());
        assert!(parse_register_server("   ", None, SIP_PORT).is_err());
    }

    #[test]
    fn unparseable_port_errors() {
        assert!(parse_register_server("10.0.0.10:nope", None, SIP_PORT).is_err());
    }
}
