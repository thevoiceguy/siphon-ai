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
    RawBridge, RawCdr, RawConference, RawConfig, RawGateway, RawHep, RawMedia, RawNode,
    RawObservability, RawOutbound, RawPark, RawRecording, RawRegister, RawSecurity, RawSip,
    RawSipTls, RawWebhooks,
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
    /// `[recording.storage]` (0.25.0) — S3-compatible upload of finalized
    /// recordings. `None` = local-disk only (the default).
    pub recording_storage: Option<siphon_ai_http::upload::UploadSettings>,
    pub outbound: OutboundConfig,
    pub conference: ConferenceConfig,
    pub park: ParkConfig,
    pub cdr: CdrConfig,
    pub observability: ObservabilityConfig,
    pub webhooks: WebhooksConfig,
    /// `[audit]` — signed audit-event stream (0.20.0).
    pub audit: AuditConfig,
    pub hep: HepConfig,
    /// `[admin]` — the authenticated admin API. `None` when no `[admin]`
    /// block is configured, in which case `/admin/*` is not served.
    pub admin: Option<AdminConfig>,
    /// `[shutdown]` — graceful drain on a shutdown signal (0.17.0).
    pub shutdown: ShutdownConfig,
}

/// Compiled `[shutdown]` — graceful connection draining on SIGTERM/SIGINT.
/// See `docs/design/DESIGN_GRACEFUL_SHUTDOWN.md`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShutdownConfig {
    /// How long to let active calls finish before forcing teardown.
    /// `None` = no drain (immediate exit, today's behaviour, opt-out via
    /// `drain_timeout_secs = 0`). `Some(d)` = wait up to `d` for the
    /// call registry to empty, then tear down.
    pub drain_timeout: Option<Duration>,
}

impl Default for ShutdownConfig {
    fn default() -> Self {
        // 30 s middle ground that fits common k8s grace periods.
        Self {
            drain_timeout: Some(Duration::from_secs(30)),
        }
    }
}

/// Compiled `[admin]` — the admin API listener address plus the
/// bearer-token auth table (tokens stored hashed). `docs/design/DESIGN_ADMIN_AUTH.md`.
#[derive(Debug, Clone)]
pub struct AdminConfig {
    pub listen_addr: SocketAddr,
    pub auth: siphon_ai_telemetry::AdminAuth,
    /// `Some` when `[admin.tls]` is configured — the listener serves
    /// HTTPS instead of plain HTTP. Loaded (and hot-reloaded on SIGHUP)
    /// by the daemon, like `[sip.tls]`.
    pub tls: Option<AdminTlsConfig>,
}

/// Compiled `[admin.tls]` — PEM cert/key paths for the admin listener.
#[derive(Debug, Clone)]
pub struct AdminTlsConfig {
    pub cert_path: PathBuf,
    pub key_path: PathBuf,
}

/// Compiled `[conference]` — conference rooms (0.7.0). Fail-closed:
/// `enabled = false` (the default) refuses every join, so a 0.6.x
/// deployment upgrades with zero behaviour change. The daemon maps
/// this 1:1 onto `siphon-ai-core`'s `ConferenceLimits`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConferenceConfig {
    pub enabled: bool,
    /// Live rooms across the daemon.
    pub max_rooms: usize,
    /// Member *calls* per room (each contributes 2 mixer
    /// participants: its SIP leg and its WS session). Kept small on
    /// purpose — per-sink mixing is O(N²) in this cap (see
    /// DEV_PLAN_0.7.0.md §6).
    pub max_participants_per_room: usize,
    /// Chime on join/leave.
    pub join_tones: bool,
}

impl Default for ConferenceConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_rooms: 16,
            max_participants_per_room: 8,
            join_tones: false,
        }
    }
}

/// What happens when a parked call hits `[park].timeout_secs`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParkTimeoutAction {
    /// Tear the call down (the default).
    Hangup,
    /// Leave it parked; the operator must retrieve or hang up.
    Keep,
}

/// Compiled `[park]` — media-only call park (0.7.0). Fail-closed:
/// `enabled = false` (the default) refuses every park, so a 0.6.x
/// deployment upgrades with zero behaviour change. The daemon maps this
/// 1:1 onto `siphon-ai-core`'s `ParkLimits`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParkConfig {
    pub enabled: bool,
    /// Validated hold-music path (exists + decodes at load). `None` →
    /// comfort noise. The native rate is resolved per-park; a call at a
    /// different rate falls back to comfort noise (no resampling in v1).
    pub moh_file: Option<PathBuf>,
    /// Seconds before `timeout_action` fires. `None` = no timeout.
    pub timeout: Option<Duration>,
    pub timeout_action: ParkTimeoutAction,
    /// Max simultaneously-parked calls across the daemon.
    pub max_parked: usize,
}

impl Default for ParkConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            moh_file: None,
            timeout: Some(Duration::from_secs(300)),
            timeout_action: ParkTimeoutAction::Hangup,
            max_parked: 32,
        }
    }
}

/// Compiled `[outbound]` + `[[gateway]]` — outbound call origination
/// (0.6.0). `max_concurrent == 0` means outbound is disabled (fail-closed).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct OutboundConfig {
    pub max_concurrent: usize,
    pub rate_limit_per_sec: Option<u32>,
    pub gateways: Vec<Gateway>,
}

impl OutboundConfig {
    /// Outbound origination is enabled only when a positive concurrency cap
    /// is set (fail-closed — see `docs/design/DEV_PLAN_0.6.0.md` §9.5/§9.6).
    pub fn enabled(&self) -> bool {
        self.max_concurrent > 0
    }

    /// Look a gateway up by name.
    pub fn gateway(&self, name: &str) -> Option<&Gateway> {
        self.gateways.iter().find(|g| g.name == name)
    }
}

/// One compiled outbound gateway (trunk/provider). `proxy_host` is left
/// unresolved — siphon-rs does RFC 3263 resolution at INVITE time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Gateway {
    pub name: String,
    pub proxy_host: String,
    pub proxy_port: u16,
    /// Transport for calls placed through this trunk. Non-UDP adds a
    /// `;transport=` URI parameter so the UAC's RFC 3263 resolution
    /// selects the right transport (and, for TLS, the right SNI).
    pub transport: SipTransport,
    /// Default caller-ID `sip:` URI.
    pub from: String,
    pub credentials: Option<GatewayCredentials>,
    /// SRTP policy for outbound media on this trunk (0.7.x). `Off` (the
    /// default) offers plaintext RTP; `Preferred`/`Required` offer SDES
    /// SRTP. Maps onto `siphon-ai-media-glue::OutboundSrtp` at the
    /// originate path.
    pub srtp: siphon_ai_core::SrtpMode,
}

impl Gateway {
    /// The Request-URI for dialing `destination` through this gateway —
    /// `sip:<destination>@<proxy_host>:<proxy_port>[;transport=…]`.
    pub fn request_uri(&self, destination: &str) -> String {
        format!(
            "sip:{}@{}:{}{}",
            destination,
            self.proxy_host,
            self.proxy_port,
            self.transport.uri_param()
        )
    }
}

/// Digest credentials for a gateway's UAC (the `CredentialProvider` source).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GatewayCredentials {
    pub username: String,
    pub password: String,
    pub realm: Option<String>,
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
    /// Require `[sip.auth]` digest on INVITEs from this trunk (AND the
    /// allowlist match). Default `false` ⇒ allowlist-only.
    pub auth_required: bool,
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
    /// HMAC-SHA256 signing secret. `None` ⇒ unsigned deliveries.
    pub secret: Option<String>,
    /// Durable spool directory. `None` ⇒ best-effort delivery.
    pub spool_dir: Option<String>,
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
    /// `Some` when `[observability.otlp].enabled` — OTLP/gRPC trace export
    /// (0.22.0). Independent of `enabled`/`http_listen` (traces without
    /// metrics scraping is a valid setup). The daemon maps this to
    /// `siphon_ai_telemetry::OtelConfig`.
    pub otlp: Option<OtlpConfig>,
}

/// Resolved OTLP trace-export plan (`[observability.otlp]`, 0.22.0).
#[derive(Debug, Clone)]
pub struct OtlpConfig {
    /// OTLP/gRPC endpoint (default `http://localhost:4317`).
    pub endpoint: String,
    /// Head sampling ratio in `[0.0, 1.0]`.
    pub sample_ratio: f64,
    /// Per-export gRPC timeout.
    pub timeout: Duration,
    /// `service.name` resource attribute.
    pub service_name: String,
    /// Extra resource attributes (`key`, `value`).
    pub attributes: Vec<(String, String)>,
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
    /// HMAC-SHA256 signing secret. `None` ⇒ unsigned deliveries.
    pub secret: Option<String>,
    /// Durable spool directory. `None` ⇒ best-effort delivery.
    pub spool_dir: Option<String>,
    pub retry_max: u32,
    pub timeout: Duration,
}

/// Resolved `[audit]` plan (0.20.0). The daemon translates this into
/// real `siphon-ai-audit` sinks at runtime (config doesn't depend on
/// the audit crate, to keep the dep graph minimal — same as CDR).
#[derive(Debug, Clone, Default)]
pub struct AuditConfig {
    /// `[audit].enabled`. Even when true, file and webhook are
    /// individually off until their `enabled = true` is set.
    pub enabled: bool,
    /// Event-type allowlist. Empty = record everything.
    pub events: Vec<String>,
    pub file: Option<AuditFileConfig>,
    pub webhook: Option<AuditWebhookConfig>,
}

#[derive(Debug, Clone)]
pub struct AuditFileConfig {
    pub path: PathBuf,
}

#[derive(Debug, Clone)]
pub struct AuditWebhookConfig {
    pub url: String,
    pub auth_header: Option<String>,
    /// HMAC-SHA256 signing secret. `None` ⇒ unsigned deliveries.
    pub secret: Option<String>,
    /// Durable spool directory. `None` ⇒ best-effort delivery.
    pub spool_dir: Option<String>,
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
    /// `[sip.tls_client].extra_ca` — optional PEM bundle appended to
    /// the webpki roots when verifying OUTGOING TLS connections
    /// (gateways / registrations with `transport = "tls"`).
    pub tls_client_extra_ca: Option<PathBuf>,
    /// RFC 4028 Min-SE for the UAS (`[sip].min_session_expires_secs`).
    /// Defaults to 90 (RFC minimum).
    pub min_session_expires: Duration,
    /// Optional cap on Session-Expires when negotiating
    /// (`[sip].preferred_session_expires_secs`). `None` = honour
    /// the peer's value uncapped.
    pub preferred_session_expires: Option<Duration>,
    /// Accept offerless inbound INVITEs (RFC 3264 delayed offer):
    /// offer in the 200 OK, read the answer from the ACK. `false`
    /// rejects an offerless INVITE with 488. Default `true`.
    pub allow_delayed_offer: bool,
    /// `Some` when `[sip.auth].enabled` — RFC 3261 §22 inbound digest
    /// authentication. `None` ⇒ off.
    pub auth: Option<SipAuthConfig>,
    /// `Some` when `[sip.admission]` enables a per-source rate limit
    /// and/or a global concurrency cap. `None` ⇒ off.
    pub admission: Option<SipAdmissionConfig>,
    /// Idle timeout (seconds) for an established inbound SIP-over-TCP/TLS
    /// connection (`[sip].tcp_idle_timeout_secs`). Default 1800; `0`
    /// disables the idle close. Wired to `sip_transport::set_established_idle_timeout`
    /// at startup. UDP is unaffected.
    pub tcp_idle_timeout_secs: u64,
}

/// Compiled `[sip.admission]` — inbound INVITE admission control.
#[derive(Debug, Clone)]
pub struct SipAdmissionConfig {
    /// Per-source token-bucket rate (tokens/sec). `0` ⇒ no per-source
    /// limit (only the global cap, if set, applies).
    pub max_per_sec: u32,
    /// Per-source bucket capacity (burst). Always ≥ `max_per_sec`.
    pub burst: u32,
    /// Consecutive per-source rejects → silent drop instead of `503`.
    pub drop_after: u32,
    /// Global concurrent-call cap. `0` ⇒ no cap.
    pub max_concurrent: u32,
    /// Cap on tracked source IPs.
    pub max_sources: u32,
}

/// Compiled `[sip.auth]` — inbound digest authentication policy +
/// credential set. `algorithm`/`qop` are stored as the canonical
/// RFC strings (validated at load) so the SIP-glue layer can hand
/// them straight to the upstream digest parser.
#[derive(Debug, Clone)]
pub struct SipAuthConfig {
    pub realm: String,
    /// Canonical algorithm token: `MD5` | `SHA-256` | `SHA-512`.
    pub algorithm: String,
    /// Canonical qop token: `auth` | `auth-int`.
    pub qop: String,
    pub users: Vec<SipAuthUser>,
}

/// One inbound digest credential. The password is cleartext (the
/// upstream verifier computes HA1 from it on each challenge), as with
/// `[[gateway]]`/`[[register]]`.
#[derive(Clone)]
pub struct SipAuthUser {
    pub username: String,
    pub password: String,
}

impl std::fmt::Debug for SipAuthUser {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print the cleartext password. A non-reversible
        // fingerprint stands in so the SIGHUP restart-required check
        // (which hashes the Debug form) still notices a password change.
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        self.password.hash(&mut h);
        f.debug_struct("SipAuthUser")
            .field("username", &self.username)
            .field("password_fp", &format!("{:016x}", h.finish()))
            .finish()
    }
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

impl SipTransport {
    /// The `;transport=` URI parameter for a Request-URI using this
    /// transport. Empty for UDP — the RFC 3263 default; emitting it
    /// explicitly would only churn byte-identical configs.
    pub fn uri_param(&self) -> &'static str {
        match self {
            SipTransport::Udp => "",
            SipTransport::Tcp => ";transport=tcp",
            SipTransport::Tls => ";transport=tls",
        }
    }
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
    /// `[media].moh_file` — hold music for bot-initiated hold (0.7.2).
    /// `None` → generated comfort silence. Validated to exist at load
    /// time (same check as `[park].moh_file`).
    pub moh_file: Option<PathBuf>,
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

    #[error(
        "ws_reconnect_max_secs must be > 0 when ws_reconnect_enabled = true \
         (a zero window gives up before the first reconnect attempt)"
    )]
    BadWsReconnectWindow,

    #[error(
        "route {route:?} sets [route.bridge].ws_reconnect_enabled = true with \
         ws_reconnect_max_secs = 0; the window must be > 0"
    )]
    RouteBadWsReconnectWindow { route: String },

    #[error("[media].srtp is {0:?}; expected \"off\", \"preferred\", or \"required\"")]
    UnknownSrtpMode(String),

    #[error("[media].srtp_offer is {0:?}; expected \"sdes\" or \"dtls\"")]
    UnknownSrtpOffer(String),

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

    #[error(
        "[recording.encryption].kek is required when enabled — 64 hex chars, \
         via ${{file:...}} or ${{cred:...}}"
    )]
    RecordingKekRequired,

    #[error("[recording.encryption].kek is invalid: {0} (expected 64 hex chars = 32 bytes)")]
    RecordingKekInvalid(String),

    #[error("[recording.encryption].key_id is required when enabled (1–255 bytes)")]
    RecordingKeyIdRequired,

    #[error("[recording.encryption].key_id must be 1–255 bytes, got {0}")]
    RecordingKeyIdInvalid(usize),

    #[error("[recording.storage].{0} is required when enabled")]
    RecordingStorageField(&'static str),

    #[error("[recording.encryption]: set exactly one of `kek` or `[recording.encryption.kms]`")]
    RecordingKekXorKms,

    #[error("[recording.encryption.kms].{0} is required")]
    RecordingKmsField(&'static str),

    #[error("[recording.storage].endpoint {0:?} must start with http:// or https://")]
    RecordingStorageEndpointInvalid(String),

    #[error(
        "[recording.storage].key_template {0:?} is invalid: {1} \
         (placeholders: {{call_id}} {{date}} {{route}} {{direction}}; \
         {{call_id}} is required)"
    )]
    RecordingStorageKeyTemplateInvalid(String, &'static str),

    #[error("[recording.storage].spool_dir {path:?} could not be created: {err}")]
    RecordingStorageSpoolInvalid { path: String, err: String },

    #[error("[route.recording].mode on route {route:?} is {value:?}; expected \"off\", \"always\", or \"on_demand\"")]
    RouteRecordingModeInvalid { route: String, value: String },

    #[error("route {route:?} enables recording but [recording].dir is not set")]
    RouteRecordingWithoutDir { route: String },

    #[error("[[gateway]] #{index} has an empty name")]
    GatewayEmptyName { index: usize },

    #[error("duplicate [[gateway]] name {name:?}")]
    GatewayDuplicateName { name: String },

    #[error("[[gateway]] {name:?} needs either `proxy` or `register`")]
    GatewayNeedsProxyOrRegister { name: String },

    #[error("[[gateway]] {gateway:?} references unknown [[register]] {register:?}")]
    GatewayUnknownRegister { gateway: String, register: String },

    #[error("[[gateway]] {name:?} proxy {proxy:?} is invalid: {err}")]
    GatewayBadProxy {
        name: String,
        proxy: String,
        err: String,
    },

    #[error("[[gateway]] {name:?} needs a `from` caller-ID URI")]
    GatewayFromRequired { name: String },

    #[error("[[gateway]] {name:?} `from` {from:?} must be a sip:/sips: URI")]
    GatewayBadFrom { name: String, from: String },

    #[error("[[gateway]] {name:?} has `auth_username` without `auth_password` (or vice versa)")]
    GatewayIncompleteAuth { name: String },

    #[error("[[gateway]] {name:?} transport {transport:?} is not one of udp, tcp, tls")]
    GatewayUnknownTransport { name: String, transport: String },

    #[error("[sip.tls_client].extra_ca {0:?} does not exist or is not a file")]
    TlsClientExtraCaMissing(String),

    #[error(
        "[[gateway]] {name:?} sets both `register` and `transport`; \
         the transport is inherited from the register block"
    )]
    GatewayTransportWithRegister { name: String },

    #[error("[conference].{field} = {value} is invalid ({reason})")]
    ConferenceBadLimit {
        field: &'static str,
        value: u32,
        reason: &'static str,
    },

    #[error("[park].max_parked must be >= 1")]
    ParkBadMaxParked,

    #[error("[park].timeout_action {0:?} is not one of \"hangup\", \"keep\"")]
    ParkBadTimeoutAction(String),

    #[error("[park].moh_file {0:?} does not exist or is not a file")]
    ParkMohMissing(String),

    #[error("[media].moh_file {0:?} does not exist or is not a file")]
    MediaMohMissing(String),

    #[error("[media].rtp_port_range {min}-{max} is invalid (min must be < max and even)")]
    BadRtpPortRange { min: u16, max: u16 },

    #[error("[cdr.file].path is required when [cdr.file].enabled = true")]
    CdrFilePathRequired,

    #[error("[cdr.webhook].url is required when [cdr.webhook].enabled = true")]
    CdrWebhookUrlRequired,

    #[error("[audit.file].path is required when [audit.file].enabled = true")]
    AuditFilePathRequired,

    #[error("[audit.webhook].url is required when [audit.webhook].enabled = true")]
    AuditWebhookUrlRequired,

    #[error("[audit].enabled = true but neither [audit.file] nor [audit.webhook] is enabled; enable at least one sink or set [audit].enabled = false")]
    AuditNoSink,

    #[error("[observability].http_listen {0:?} is not a valid socket address: {1}")]
    BadObservabilityListen(String, std::net::AddrParseError),

    #[error("[observability].http_listen is required when [observability].enabled = true")]
    ObservabilityListenRequired,

    #[error("[observability.otlp].sample_ratio {0} is out of range; must be between 0.0 and 1.0")]
    OtlpBadSampleRatio(f64),

    #[error("[admin].listen {0:?} is not a valid socket address: {1}")]
    BadAdminListen(String, std::net::AddrParseError),

    #[error("[admin] is configured but has no [[admin.token]] entries; an admin listener with no tokens can authenticate no one")]
    AdminNoTokens,

    #[error("[[admin.token]] has an empty name")]
    AdminTokenEmptyName,

    #[error("[[admin.token]] name {0:?} is used more than once")]
    AdminDuplicateTokenName(String),

    #[error("[[admin.token]] {0:?} has an empty token")]
    AdminTokenEmptySecret(String),

    #[error(
        "[[admin.token]] {0:?} role is {1:?}; expected \"readonly\", \"operator\", or \"admin\""
    )]
    AdminUnknownRole(String, String),

    #[error("[admin.tls] is present but [admin.tls].cert is missing or empty")]
    AdminTlsCertRequired,

    #[error("[admin.tls] is present but [admin.tls].key is missing or empty")]
    AdminTlsKeyRequired,

    #[error("[sip.auth].enabled = true but [sip.auth].realm is missing or empty")]
    SipAuthRealmRequired,

    #[error(
        "[sip.auth].algorithm {0:?} is unknown; expected \"MD5\", \"SHA-256\", or \"SHA-512\""
    )]
    SipAuthUnknownAlgorithm(String),

    #[error("[sip.auth].qop {0:?} is unknown; expected \"auth\" or \"auth-int\"")]
    SipAuthUnknownQop(String),

    #[error(
        "[sip.auth].enabled = true but has no [[sip.auth.user]] entries to authenticate against"
    )]
    SipAuthNoUsers,

    #[error("[[sip.auth.user]] has an empty username")]
    SipAuthUserEmptyName,

    #[error("[[sip.auth.user]] {0:?} has an empty password")]
    SipAuthUserEmptySecret(String),

    #[error("[[sip.auth.user]] username {0:?} is used more than once")]
    SipAuthDuplicateUser(String),

    #[error("[sip.admission].burst ({burst}) is smaller than max_per_sec ({max_per_sec}); burst must be ≥ the steady rate")]
    SipAdmissionBurstTooSmall { burst: u32, max_per_sec: u32 },

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
    let mut raw_recording = raw.recording;
    let raw_recording_storage = raw_recording.storage.take();
    let recording = compile_recording(raw_recording)?;
    let recording_storage = compile_recording_storage(raw_recording_storage, &recording)?;
    let outbound = compile_outbound(raw.outbound, raw.gateways, &registrations)?;
    let conference = compile_conference(raw.conference)?;
    let park = compile_park(raw.park)?;
    let cdr = compile_cdr(raw.cdr)?;
    let observability = compile_observability(raw.observability)?;
    let webhooks = compile_webhooks(raw.webhooks)?;
    let audit = compile_audit(raw.audit)?;
    let hep = compile_hep(raw.hep)?;
    let admin = compile_admin(raw.admin)?;
    let shutdown = compile_shutdown(raw.shutdown);

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
        recording_storage,
        node,
        sip,
        media,
        bridge_defaults,
        routes,
        registrations,
        trunks,
        security,
        recording,
        outbound,
        conference,
        park,
        cdr,
        observability,
        webhooks,
        audit,
        hep,
        admin,
        shutdown,
    })
}

/// Compile `[shutdown]`. Infallible: the only field is a non-negative
/// duration. Unset → 30 s default; `0` → no drain (immediate exit).
fn compile_shutdown(raw: crate::raw::RawShutdown) -> ShutdownConfig {
    match raw.drain_timeout_secs {
        None => ShutdownConfig::default(),
        Some(0) => ShutdownConfig {
            drain_timeout: None,
        },
        Some(secs) => ShutdownConfig {
            drain_timeout: Some(Duration::from_secs(secs)),
        },
    }
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

    // Client-side roots are independent of the TLS *listener* — a
    // UDP-only daemon can still dial a TLS trunk. Validate the path
    // at load per CLAUDE.md §4.6 (fail loud at startup).
    let tls_client_extra_ca = match raw.tls_client.extra_ca {
        None => None,
        Some(p) => {
            let path = PathBuf::from(&p);
            if !path.is_file() {
                return Err(CompileError::TlsClientExtraCaMissing(p));
            }
            Some(path)
        }
    };

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

    let auth = compile_sip_auth(raw.auth)?;
    let admission = compile_sip_admission(raw.admission)?;

    Ok(SipConfig {
        listen_addr,
        transports,
        user_agent: raw.user_agent,
        contact: raw.contact,
        tls,
        tls_client_extra_ca,
        call_progress,
        min_session_expires,
        preferred_session_expires,
        allow_delayed_offer: raw.allow_delayed_offer,
        auth,
        admission,
        tcp_idle_timeout_secs: raw.tcp_idle_timeout_secs.unwrap_or(1800),
    })
}

/// Compile `[sip.admission]`. `None`, or every knob zero/unset, ⇒ off.
/// `burst` defaults to (and is floored at) `max_per_sec`; `drop_after`
/// defaults to 10; `max_sources` to 10000.
fn compile_sip_admission(
    raw: Option<crate::raw::RawSipAdmission>,
) -> Result<Option<SipAdmissionConfig>, CompileError> {
    let Some(raw) = raw else { return Ok(None) };
    let max_per_sec = raw.max_per_sec.unwrap_or(0);
    let max_concurrent = raw.max_concurrent.unwrap_or(0);
    // Nothing to enforce → treat as off (an empty `[sip.admission]` is a
    // no-op, not an error).
    if max_per_sec == 0 && max_concurrent == 0 {
        return Ok(None);
    }
    // burst defaults to the rate and can't be below it (a burst smaller
    // than the steady rate would reject legitimate steady traffic).
    let burst = match raw.burst {
        None | Some(0) => max_per_sec,
        Some(b) if b < max_per_sec => {
            return Err(CompileError::SipAdmissionBurstTooSmall {
                burst: b,
                max_per_sec,
            })
        }
        Some(b) => b,
    };
    let drop_after = raw.drop_after.unwrap_or(10);
    let max_sources = match raw.max_sources {
        None | Some(0) => 10_000,
        Some(n) => n,
    };
    Ok(Some(SipAdmissionConfig {
        max_per_sec,
        burst,
        drop_after,
        max_concurrent,
        max_sources,
    }))
}

/// Compile `[sip.auth]`. `None`/`enabled = false` ⇒ off. When enabled,
/// require a realm, ≥1 user with non-empty username+password, and a
/// recognised algorithm/qop (canonicalised to the RFC token).
fn compile_sip_auth(
    raw: Option<crate::raw::RawSipAuth>,
) -> Result<Option<SipAuthConfig>, CompileError> {
    let Some(raw) = raw else { return Ok(None) };
    if !raw.enabled {
        return Ok(None);
    }

    let realm = raw.realm.unwrap_or_default();
    if realm.trim().is_empty() {
        return Err(CompileError::SipAuthRealmRequired);
    }

    // Canonicalise algorithm/qop to the exact RFC token the upstream
    // digest parser expects (case-insensitive in, canonical out).
    let algorithm = match raw.algorithm.as_deref() {
        None => "SHA-256".to_string(),
        Some(a) => match a.to_ascii_uppercase().as_str() {
            "MD5" => "MD5".to_string(),
            "SHA-256" => "SHA-256".to_string(),
            "SHA-512" => "SHA-512".to_string(),
            _ => return Err(CompileError::SipAuthUnknownAlgorithm(a.to_string())),
        },
    };
    let qop = match raw.qop.as_deref() {
        None => "auth".to_string(),
        Some(q) => match q.to_ascii_lowercase().as_str() {
            "auth" => "auth".to_string(),
            "auth-int" => "auth-int".to_string(),
            _ => return Err(CompileError::SipAuthUnknownQop(q.to_string())),
        },
    };

    if raw.users.is_empty() {
        return Err(CompileError::SipAuthNoUsers);
    }
    let mut users = Vec::with_capacity(raw.users.len());
    let mut seen = std::collections::HashSet::new();
    for u in raw.users {
        if u.username.trim().is_empty() {
            return Err(CompileError::SipAuthUserEmptyName);
        }
        if u.password.is_empty() {
            return Err(CompileError::SipAuthUserEmptySecret(u.username));
        }
        if !seen.insert(u.username.clone()) {
            return Err(CompileError::SipAuthDuplicateUser(u.username));
        }
        users.push(SipAuthUser {
            username: u.username,
            password: u.password,
        });
    }

    Ok(Some(SipAuthConfig {
        realm,
        algorithm,
        qop,
        users,
    }))
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
    // Hold music: validate existence at load time (fail loud, §4.6).
    // Unlike `[park].moh_file`, hold has no enable flag — it's always
    // available on inbound legs — so an empty/absent value just means
    // comfort silence, but a *set* path that doesn't exist is an error.
    let moh_file = match raw.moh_file.as_deref().filter(|s| !s.is_empty()) {
        None => None,
        Some(p) => {
            let path = PathBuf::from(p);
            if !path.is_file() {
                return Err(CompileError::MediaMohMissing(p.to_string()));
            }
            Some(path)
        }
    };
    Ok(MediaConfig {
        rtp_port_range: raw.rtp_port_range,
        srtp,
        moh_file,
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

    // `[recording.encryption]` (0.24.0). Fail-loud at load (§4.6): a bad
    // or missing KEK/key_id must never surface as per-call recording
    // failures. The `kek` value arrives already resolved (`${file:}` /
    // `${cred:}` expand before parse), so it must be hex — raw key bytes
    // wouldn't survive the TOML splice.
    let encryption = match raw.encryption {
        Some(enc) if enc.enabled.unwrap_or(false) => {
            let key_id = enc.key_id.ok_or(CompileError::RecordingKeyIdRequired)?;
            if key_id.is_empty() || key_id.len() > 255 {
                return Err(CompileError::RecordingKeyIdInvalid(key_id.len()));
            }
            let kek = match (enc.kek, enc.kms) {
                (Some(kek_hex), None) => siphon_ai_recording::Kek::from_hex(&kek_hex, key_id)
                    .map_err(|e| CompileError::RecordingKekInvalid(e.to_string()))?,
                (None, Some(kms)) => {
                    let required = |field: &'static str, v: Option<String>| {
                        v.filter(|s| !s.trim().is_empty())
                            .ok_or(CompileError::RecordingKmsField(field))
                    };
                    let key_arn = required("key_arn", kms.key_arn)?;
                    let region = required("region", kms.region)?;
                    let credentials = siphon_ai_http::sigv4::SigV4Credentials {
                        access_key: required("access_key", kms.access_key)?,
                        secret_key: required("secret_key", kms.secret_key)?,
                    };
                    let client =
                        siphon_ai_http::kms::KmsClient::new(region, credentials, kms.endpoint);
                    siphon_ai_recording::Kek::AwsKms {
                        client,
                        key_arn,
                        key_id,
                    }
                }
                (None, None) => return Err(CompileError::RecordingKekRequired),
                (Some(_), Some(_)) => return Err(CompileError::RecordingKekXorKms),
            };
            Some(kek)
        }
        _ => None,
    };

    Ok(RecordingConfig {
        mode,
        dir,
        encryption,
    })
}

/// Compile and validate `[recording.storage]` (0.25.0). Fail-loud at
/// load (§4.6): every field checked here so upload failures at runtime
/// can only be transport, never config. The spool dir is created at load.
fn compile_recording_storage(
    raw: Option<crate::raw::RawRecordingStorage>,
    recording: &siphon_ai_recording::RecordingConfig,
) -> Result<Option<siphon_ai_http::upload::UploadSettings>, CompileError> {
    use siphon_ai_http::s3::S3Target;
    use siphon_ai_http::sigv4::SigV4Credentials;
    use siphon_ai_http::upload::UploadSettings;

    let Some(raw) = raw else { return Ok(None) };
    if !raw.enabled.unwrap_or(false) {
        return Ok(None);
    }

    let required = |field: &'static str, v: Option<String>| {
        v.filter(|s| !s.trim().is_empty())
            .ok_or(CompileError::RecordingStorageField(field))
    };
    let endpoint = required("endpoint", raw.endpoint)?;
    if !endpoint.starts_with("http://") && !endpoint.starts_with("https://") {
        return Err(CompileError::RecordingStorageEndpointInvalid(endpoint));
    }
    let endpoint = endpoint.trim_end_matches('/').to_string();
    let bucket = required("bucket", raw.bucket)?;
    let region = required("region", raw.region)?;
    let access_key = required("access_key", raw.access_key)?;
    let secret_key = required("secret_key", raw.secret_key)?;

    let key_template = raw
        .key_template
        .unwrap_or_else(|| "{date}/{call_id}".to_string());
    if !key_template.contains("{call_id}") {
        return Err(CompileError::RecordingStorageKeyTemplateInvalid(
            key_template,
            "missing {call_id}",
        ));
    }
    // Reject unknown placeholders now, not as literal braces in keys later.
    let mut rest = key_template.as_str();
    while let Some(open) = rest.find('{') {
        let Some(close) = rest[open..].find('}') else {
            return Err(CompileError::RecordingStorageKeyTemplateInvalid(
                key_template.clone(),
                "unbalanced braces",
            ));
        };
        let name = &rest[open + 1..open + close];
        if !matches!(name, "call_id" | "date" | "route" | "direction") {
            return Err(CompileError::RecordingStorageKeyTemplateInvalid(
                key_template.clone(),
                "unknown placeholder",
            ));
        }
        rest = &rest[open + close + 1..];
    }

    let spool_dir = PathBuf::from(required("spool_dir", raw.spool_dir)?);
    std::fs::create_dir_all(&spool_dir).map_err(|err| {
        CompileError::RecordingStorageSpoolInvalid {
            path: spool_dir.display().to_string(),
            err: err.to_string(),
        }
    })?;

    if recording.mode == siphon_ai_recording::RecordingMode::Off
        && recording.dir.as_os_str().is_empty()
    {
        warn!(
            "[recording.storage] is enabled but recording is fully off — \
             nothing will ever be uploaded"
        );
    }

    Ok(Some(UploadSettings {
        target: S3Target {
            endpoint,
            bucket,
            region,
            credentials: SigV4Credentials {
                access_key,
                secret_key,
            },
        },
        key_template,
        delete_local_after_upload: raw.delete_local_after_upload.unwrap_or(false),
        spool_dir,
    }))
}

/// Compile `[outbound]` and `[[gateway]]`. Gateways resolve to a uniform
/// "where and how to dial" shape: either a standalone trunk (a `proxy` plus
/// a `from`, with optional digest auth) or a `[[register]]` reuse that
/// inherits the registrar's server, credentials, and AOR. Validated at load:
/// unique names, a proxy-or-register source, a `sip:` caller-ID, and
/// complete auth.
fn compile_outbound(
    raw: RawOutbound,
    gateways: Vec<RawGateway>,
    registrations: &[RegisterConfig],
) -> Result<OutboundConfig, CompileError> {
    let max_concurrent = raw.max_concurrent.unwrap_or(0);
    let rate_limit_per_sec = raw.rate_limit_per_sec.filter(|&r| r > 0);

    let mut compiled: Vec<Gateway> = Vec::with_capacity(gateways.len());
    for (i, g) in gateways.into_iter().enumerate() {
        if g.name.trim().is_empty() {
            return Err(CompileError::GatewayEmptyName { index: i });
        }
        if compiled.iter().any(|c| c.name == g.name) {
            return Err(CompileError::GatewayDuplicateName { name: g.name });
        }

        // SRTP policy is independent of register-reuse vs standalone;
        // resolve it once and stamp it into whichever branch builds the
        // Gateway. Unknown value fails loud (same as [media].srtp).
        let srtp = compile_srtp_mode(g.srtp.as_deref())?;

        let gw = if let Some(reg_name) = g.register.as_deref().filter(|s| !s.is_empty()) {
            // Register reuse — inherit server + credentials + AOR +
            // transport. An explicit `transport` here is a conflict,
            // not an override: fail loud per CLAUDE.md §4.6.
            if g.transport.is_some() {
                return Err(CompileError::GatewayTransportWithRegister {
                    name: g.name.clone(),
                });
            }
            let reg = registrations
                .iter()
                .find(|r| r.name == reg_name)
                .ok_or_else(|| CompileError::GatewayUnknownRegister {
                    gateway: g.name.clone(),
                    register: reg_name.to_string(),
                })?;
            let from = g
                .from
                .clone()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| format!("sip:{}@{}", reg.username, reg.server_host));
            validate_gateway_from(&from, &g.name)?;
            Gateway {
                name: g.name.clone(),
                proxy_host: reg.server_host.clone(),
                proxy_port: reg.server_addr.port(),
                transport: reg.transport,
                from,
                credentials: Some(GatewayCredentials {
                    username: reg.auth_username.clone(),
                    password: reg.password.clone(),
                    realm: reg.realm.clone(),
                }),
                srtp,
            }
        } else {
            // Standalone trunk.
            let proxy = g
                .proxy
                .as_deref()
                .filter(|s| !s.is_empty())
                .ok_or_else(|| CompileError::GatewayNeedsProxyOrRegister {
                    name: g.name.clone(),
                })?;
            let transport = match g
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
                    return Err(CompileError::GatewayUnknownTransport {
                        name: g.name.clone(),
                        transport: other.to_string(),
                    })
                }
            };
            let default_port = match transport {
                SipTransport::Tls => 5061,
                _ => 5060,
            };
            let (proxy_host, proxy_port) = parse_register_server(proxy, None, default_port)
                .map_err(|err| CompileError::GatewayBadProxy {
                    name: g.name.clone(),
                    proxy: proxy.to_string(),
                    err,
                })?;
            let from = g.from.clone().filter(|s| !s.is_empty()).ok_or_else(|| {
                CompileError::GatewayFromRequired {
                    name: g.name.clone(),
                }
            })?;
            validate_gateway_from(&from, &g.name)?;
            let user = g.auth_username.as_deref().filter(|s| !s.is_empty());
            let pass = g.auth_password.as_deref().filter(|s| !s.is_empty());
            let credentials = match (user, pass) {
                (Some(u), Some(p)) => Some(GatewayCredentials {
                    username: u.to_string(),
                    password: p.to_string(),
                    realm: g.realm.clone().filter(|s| !s.is_empty()),
                }),
                (None, None) => None,
                _ => {
                    return Err(CompileError::GatewayIncompleteAuth {
                        name: g.name.clone(),
                    })
                }
            };
            Gateway {
                name: g.name.clone(),
                proxy_host,
                proxy_port,
                transport,
                from,
                credentials,
                srtp,
            }
        };

        // SDES-key-in-the-clear footgun (per-gateway mirror of the inbound
        // `[media].srtp`-without-SIP/TLS warning): SRTP on a non-TLS trunk
        // exchanges the master key over plaintext signalling, so it gives
        // no real confidentiality. Warn once at load.
        if gw.srtp != siphon_ai_core::SrtpMode::Off && gw.transport != SipTransport::Tls {
            warn!(
                target: "siphon_ai_config",
                gateway = %gw.name,
                "[[gateway]].srtp is not \"off\" but transport is not \"tls\"; the SDES \
                 master key would travel in plaintext on the SIP signalling plane — \
                 SRTP gives no confidentiality. Set transport = \"tls\" on this gateway."
            );
        }
        compiled.push(gw);
    }

    Ok(OutboundConfig {
        max_concurrent,
        rate_limit_per_sec,
        gateways: compiled,
    })
}

fn validate_gateway_from(from: &str, gateway: &str) -> Result<(), CompileError> {
    if from.starts_with("sip:") || from.starts_with("sips:") {
        Ok(())
    } else {
        Err(CompileError::GatewayBadFrom {
            name: gateway.to_string(),
            from: from.to_string(),
        })
    }
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

    // WS reconnect (0.7.3). Off unless explicitly enabled; the window
    // defaults to 30 s. Enabling with a zero window is a config mistake
    // (it would give up before the first backoff) — fail loud per §4.6.
    let ws_reconnect_enabled = raw.ws_reconnect_enabled.unwrap_or(false);
    let ws_reconnect_secs = raw.ws_reconnect_max_secs.unwrap_or(30);
    if ws_reconnect_enabled && ws_reconnect_secs == 0 {
        return Err(CompileError::BadWsReconnectWindow);
    }
    let ws_reconnect_max = Duration::from_secs(ws_reconnect_secs);

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

    // WS liveness (PROTOCOL.md §5.6 keepalive, §3.1 start-deadline).
    // `None` = spec default; `Some(0)` = disable (resolves to
    // `Duration::ZERO`, which the bridge treats as off); `Some(n)` = n s.
    let ws_ping_interval = match raw.ws_ping_interval_secs {
        None => Duration::from_secs(15),
        Some(s) => Duration::from_secs(s),
    };
    let ws_pong_timeout = match raw.ws_pong_timeout_secs {
        None => Duration::from_secs(10),
        Some(s) => Duration::from_secs(s),
    };
    let server_start_deadline = match raw.server_start_deadline_secs {
        None => Duration::from_secs(5),
        Some(s) => Duration::from_secs(s),
    };

    // `[media].srtp` resolved to the typed enum form via the same
    // strict-matching path the route-level override uses
    // (`compile_srtp_mode`). Default — and any unset value — is `Off`.
    let srtp_mode = compile_srtp_mode(media.srtp.as_deref())?;

    // `[media].srtp_offer` — which key-exchange to OFFER when we're the
    // offerer on a delayed offer. `"sdes"` (default) or `"dtls"`; unknown
    // strings fail loud (CLAUDE.md §4.6).
    let offer_dtls_srtp = match media.srtp_offer.as_deref() {
        None | Some("sdes") => false,
        Some("dtls") => true,
        Some(other) => return Err(CompileError::UnknownSrtpOffer(other.to_string())),
    };

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
        ws_ping_interval,
        ws_pong_timeout,
        server_start_deadline,
        srtp_mode,
        offer_dtls_srtp,
        bridge_tls,
        ws_reconnect_enabled,
        ws_reconnect_max,
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
    if let Some(ms) = raw.debounce_ms {
        // 0 = explicitly off; any positive value arms the playout gate.
        cfg.debounce = (ms > 0).then(|| std::time::Duration::from_millis(ms));
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
        // A route enabling reconnect with a zero window is the same
        // mistake as the global (0.7.3). `Some(0)` only — `None`
        // inherits the (validated, non-zero) global.
        if route.bridge.ws_reconnect_enabled == Some(true)
            && route.bridge.ws_reconnect_max_secs == Some(0)
        {
            return Err(CompileError::RouteBadWsReconnectWindow {
                route: route.name.clone(),
            });
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
        secret: raw.secret.filter(|s| !s.is_empty()),
        spool_dir: raw.spool_dir.filter(|s| !s.is_empty()),
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
            auth_required: t.auth_required.unwrap_or(false),
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
    // OTLP trace export is independent of the metrics HTTP listener — like
    // HEP vs `[cdr]`, you can export traces without scraping metrics — so
    // compile it regardless of the `enabled` master switch.
    let otlp = compile_otlp(raw.otlp)?;

    if !raw.enabled {
        // Disabled means "don't spawn the HTTP server" — sub-block
        // misconfig is tolerated (same shape as [cdr] master switch
        // — operators can flip enabled = false to silence a flaky
        // listener without re-editing every field).
        return Ok(ObservabilityConfig {
            enabled: false,
            http_listen: None,
            otlp,
        });
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
        otlp,
    })
}

fn compile_otlp(raw: crate::raw::RawObservabilityOtlp) -> Result<Option<OtlpConfig>, CompileError> {
    if !raw.enabled {
        return Ok(None);
    }
    let endpoint = raw
        .endpoint
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "http://localhost:4317".to_string());
    let sample_ratio = raw.sample_ratio.unwrap_or(1.0);
    if !(0.0..=1.0).contains(&sample_ratio) {
        return Err(CompileError::OtlpBadSampleRatio(sample_ratio));
    }
    let service_name = raw
        .service_name
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "siphon-ai".to_string());
    Ok(Some(OtlpConfig {
        endpoint,
        sample_ratio,
        timeout: Duration::from_millis(raw.timeout_ms.unwrap_or(5000)),
        service_name,
        attributes: raw.attributes.unwrap_or_default().into_iter().collect(),
    }))
}

fn compile_admin(raw: Option<crate::raw::RawAdmin>) -> Result<Option<AdminConfig>, CompileError> {
    // No [admin] block → /admin/* is not served (secure default).
    let Some(raw) = raw else {
        return Ok(None);
    };
    let listen_addr: SocketAddr = raw
        .listen
        .parse()
        .map_err(|e| CompileError::BadAdminListen(raw.listen.clone(), e))?;

    // An admin listener with no tokens can authenticate nobody — refuse
    // rather than silently lock everyone out (CLAUDE.md §4.6).
    if raw.tokens.is_empty() {
        return Err(CompileError::AdminNoTokens);
    }

    let mut seen_names = std::collections::HashSet::new();
    let mut tokens = Vec::with_capacity(raw.tokens.len());
    for t in raw.tokens {
        if t.name.is_empty() {
            return Err(CompileError::AdminTokenEmptyName);
        }
        if !seen_names.insert(t.name.clone()) {
            return Err(CompileError::AdminDuplicateTokenName(t.name));
        }
        if t.token.is_empty() {
            return Err(CompileError::AdminTokenEmptySecret(t.name));
        }
        let role = siphon_ai_telemetry::Role::parse(&t.role)
            .ok_or_else(|| CompileError::AdminUnknownRole(t.name.clone(), t.role.clone()))?;
        tokens.push(siphon_ai_telemetry::AdminToken::new(t.name, &t.token, role));
    }

    // [admin.tls] (optional). When present, both cert and key are
    // required and non-empty — fail loud rather than silently serving
    // plain HTTP.
    let tls = match raw.tls {
        None => None,
        Some(t) => {
            let cert = t.cert.filter(|s| !s.is_empty());
            let key = t.key.filter(|s| !s.is_empty());
            let cert_path = cert.ok_or(CompileError::AdminTlsCertRequired)?;
            let key_path = key.ok_or(CompileError::AdminTlsKeyRequired)?;
            Some(AdminTlsConfig {
                cert_path: PathBuf::from(cert_path),
                key_path: PathBuf::from(key_path),
            })
        }
    };

    Ok(Some(AdminConfig {
        listen_addr,
        auth: siphon_ai_telemetry::AdminAuth::new(tokens),
        tls,
    }))
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

/// Compile `[conference]` (0.7.0). Validation per CLAUDE.md §4.6 —
/// fail loud at load, not at first join:
/// - `max_rooms >= 1` (0 would make `enabled = true` a silent no-op);
/// - `max_participants_per_room >= 2` (a 1-call cap can never
///   conference — the first joiner waits alone for a peer that can
///   never be admitted).
fn compile_conference(raw: RawConference) -> Result<ConferenceConfig, CompileError> {
    let defaults = ConferenceConfig::default();
    let max_rooms = raw.max_rooms.unwrap_or(defaults.max_rooms as u32);
    if max_rooms == 0 {
        return Err(CompileError::ConferenceBadLimit {
            field: "max_rooms",
            value: 0,
            reason: "must be >= 1",
        });
    }
    let max_participants = raw
        .max_participants_per_room
        .unwrap_or(defaults.max_participants_per_room as u32);
    if max_participants < 2 {
        return Err(CompileError::ConferenceBadLimit {
            field: "max_participants_per_room",
            value: max_participants,
            reason: "must be >= 2 (a room needs at least two calls to conference)",
        });
    }
    Ok(ConferenceConfig {
        enabled: raw.enabled,
        max_rooms: max_rooms as usize,
        max_participants_per_room: max_participants as usize,
        join_tones: raw.join_tones,
    })
}

/// Compile `[park]` (0.7.0). Validation per CLAUDE.md §4.6 — fail loud
/// at load: `timeout_action` is a known value, `max_parked >= 1`, and
/// (when set + enabled) `moh_file` exists. Decodability is probed by the
/// daemon at startup (config deliberately doesn't dep on the media
/// stack — same split as `[sip.tls_client].extra_ca`).
fn compile_park(raw: RawPark) -> Result<ParkConfig, CompileError> {
    let defaults = ParkConfig::default();
    let max_parked = raw.max_parked.unwrap_or(defaults.max_parked as u32);
    if max_parked == 0 {
        return Err(CompileError::ParkBadMaxParked);
    }
    let timeout_action = match raw.timeout_action.as_deref() {
        None => defaults.timeout_action,
        Some("hangup") => ParkTimeoutAction::Hangup,
        Some("keep") => ParkTimeoutAction::Keep,
        Some(other) => return Err(CompileError::ParkBadTimeoutAction(other.to_string())),
    };
    let timeout = match raw.timeout_secs {
        None => defaults.timeout,
        Some(0) => None,
        Some(secs) => Some(Duration::from_secs(secs)),
    };
    // Existence check only when park is on — a disabled block shouldn't
    // fail a boot over a stale path. (A moh_file on a disabled block is
    // retained but unchecked; it's inert.)
    let moh_file = match raw.moh_file.filter(|s| !s.is_empty()) {
        None => None,
        Some(p) => {
            let path = PathBuf::from(&p);
            if raw.enabled && !path.is_file() {
                return Err(CompileError::ParkMohMissing(p));
            }
            Some(path)
        }
    };
    Ok(ParkConfig {
        enabled: raw.enabled,
        moh_file,
        timeout,
        timeout_action,
        max_parked: max_parked as usize,
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
            secret: raw.webhook.secret.filter(|s| !s.is_empty()),
            spool_dir: raw.webhook.spool_dir.filter(|s| !s.is_empty()),
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

fn compile_audit(raw: crate::raw::RawAudit) -> Result<AuditConfig, CompileError> {
    if !raw.enabled {
        // Master switch off; sub-block config parsed but ignored,
        // matching the [cdr] pattern (flip `enabled = false` to
        // silence a misconfig while investigating).
        return Ok(AuditConfig::default());
    }
    let file = if raw.file.enabled {
        let path = raw.file.path.ok_or(CompileError::AuditFilePathRequired)?;
        if path.is_empty() {
            return Err(CompileError::AuditFilePathRequired);
        }
        Some(AuditFileConfig {
            path: PathBuf::from(path),
        })
    } else {
        None
    };
    let webhook = if raw.webhook.enabled {
        let url = raw
            .webhook
            .url
            .ok_or(CompileError::AuditWebhookUrlRequired)?;
        if url.is_empty() {
            return Err(CompileError::AuditWebhookUrlRequired);
        }
        Some(AuditWebhookConfig {
            url,
            auth_header: raw.webhook.auth_header.filter(|s| !s.is_empty()),
            secret: raw.webhook.secret.filter(|s| !s.is_empty()),
            spool_dir: raw.webhook.spool_dir.filter(|s| !s.is_empty()),
            retry_max: raw.webhook.retry_max.unwrap_or(3),
            timeout: Duration::from_millis(raw.webhook.timeout_ms.unwrap_or(5000)),
        })
    } else {
        None
    };
    // `[audit].enabled = true` with no sink enabled is almost certainly
    // a mistake (the operator thinks they're auditing but nothing is
    // recorded). Fail loud rather than silently install a null sink.
    if file.is_none() && webhook.is_none() {
        return Err(CompileError::AuditNoSink);
    }
    Ok(AuditConfig {
        enabled: true,
        events: raw.events.unwrap_or_default(),
        file,
        webhook,
    })
}

#[cfg(test)]
mod audit_tests {
    use super::{compile_audit, CompileError};
    use crate::raw::{RawAudit, RawAuditFile, RawAuditWebhook};

    fn base() -> RawAudit {
        RawAudit {
            enabled: true,
            events: None,
            file: RawAuditFile::default(),
            webhook: RawAuditWebhook::default(),
        }
    }

    #[test]
    fn disabled_yields_default_no_sinks() {
        let cfg = compile_audit(RawAudit::default()).unwrap();
        assert!(!cfg.enabled);
        assert!(cfg.file.is_none());
        assert!(cfg.webhook.is_none());
    }

    #[test]
    fn enabled_with_no_sink_is_rejected() {
        let err = compile_audit(base()).unwrap_err();
        assert!(matches!(err, CompileError::AuditNoSink));
    }

    #[test]
    fn file_enabled_requires_path() {
        let mut raw = base();
        raw.file.enabled = true;
        let err = compile_audit(raw).unwrap_err();
        assert!(matches!(err, CompileError::AuditFilePathRequired));
    }

    #[test]
    fn webhook_enabled_requires_url() {
        let mut raw = base();
        raw.webhook.enabled = true;
        let err = compile_audit(raw).unwrap_err();
        assert!(matches!(err, CompileError::AuditWebhookUrlRequired));
    }

    #[test]
    fn file_and_webhook_compile_with_defaults() {
        let mut raw = base();
        raw.file.enabled = true;
        raw.file.path = Some("/var/log/siphon-ai/audit.jsonl".into());
        raw.webhook.enabled = true;
        raw.webhook.url = Some("https://siem.example/events".into());
        raw.webhook.secret = Some("s3cret".into());
        raw.events = Some(vec!["admin_request".into(), "sip_auth".into()]);

        let cfg = compile_audit(raw).unwrap();
        assert!(cfg.enabled);
        assert_eq!(
            cfg.file.unwrap().path.to_str().unwrap(),
            "/var/log/siphon-ai/audit.jsonl"
        );
        let wh = cfg.webhook.unwrap();
        assert_eq!(wh.url, "https://siem.example/events");
        assert_eq!(wh.secret.as_deref(), Some("s3cret"));
        assert_eq!(wh.retry_max, 3);
        assert_eq!(cfg.events, vec!["admin_request", "sip_auth"]);
    }

    #[test]
    fn empty_secret_and_spool_normalise_to_none() {
        let mut raw = base();
        raw.webhook.enabled = true;
        raw.webhook.url = Some("https://siem.example/events".into());
        raw.webhook.secret = Some(String::new());
        raw.webhook.spool_dir = Some(String::new());
        let wh = compile_audit(raw).unwrap().webhook.unwrap();
        assert!(wh.secret.is_none());
        assert!(wh.spool_dir.is_none());
    }
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
mod barge_in_tests {
    use super::compile_barge_in_default;
    use crate::raw::RawBargeIn;
    use std::time::Duration;

    #[test]
    fn debounce_unset_or_zero_is_none() {
        // Unset → no gate (immediate flush, unchanged behaviour).
        let cfg = compile_barge_in_default(&RawBargeIn::default()).unwrap();
        assert_eq!(cfg.debounce, None);
        // Explicit 0 → off.
        let raw = RawBargeIn {
            debounce_ms: Some(0),
            ..Default::default()
        };
        assert_eq!(compile_barge_in_default(&raw).unwrap().debounce, None);
    }

    #[test]
    fn debounce_positive_arms_the_gate() {
        let raw = RawBargeIn {
            debounce_ms: Some(200),
            ..Default::default()
        };
        assert_eq!(
            compile_barge_in_default(&raw).unwrap().debounce,
            Some(Duration::from_millis(200))
        );
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
            encryption: None,
            storage: None,
        }
    }

    fn raw_enc(
        enabled: bool,
        kek: Option<&str>,
        key_id: Option<&str>,
    ) -> crate::raw::RawRecordingEncryption {
        crate::raw::RawRecordingEncryption {
            enabled: Some(enabled),
            kek: kek.map(str::to_string),
            key_id: key_id.map(str::to_string),
            kms: None,
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
    fn encryption_compiles_and_flips_extension() {
        let dir = std::env::temp_dir().join("siphon_rec_cfg_enc_test");
        let mut r = raw(Some("always"), dir.to_str());
        r.encryption = Some(raw_enc(true, Some(&"ab".repeat(32)), Some("k-2026")));
        let c = compile_recording(r).unwrap();
        let kek = c.encryption.as_ref().expect("encryption compiled");
        assert_eq!(kek.key_id(), "k-2026");
        assert!(c
            .path_for("call1")
            .to_string_lossy()
            .ends_with("call1.wava"));
    }

    #[test]
    fn encryption_disabled_section_is_inert() {
        let dir = std::env::temp_dir().join("siphon_rec_cfg_enc_test");
        let mut r = raw(Some("always"), dir.to_str());
        // enabled = false → no KEK/key_id required, plaintext output.
        r.encryption = Some(raw_enc(false, None, None));
        let c = compile_recording(r).unwrap();
        assert!(c.encryption.is_none());
        assert!(c.path_for("call1").to_string_lossy().ends_with("call1.wav"));
    }

    #[test]
    fn encryption_validation_fails_loud() {
        let dir = std::env::temp_dir().join("siphon_rec_cfg_enc_test");
        let d = dir.to_str();

        let mut r = raw(Some("always"), d);
        r.encryption = Some(raw_enc(true, Some(&"ab".repeat(32)), None));
        assert!(matches!(
            compile_recording(r),
            Err(CompileError::RecordingKeyIdRequired)
        ));

        let mut r = raw(Some("always"), d);
        r.encryption = Some(raw_enc(true, None, Some("k")));
        assert!(matches!(
            compile_recording(r),
            Err(CompileError::RecordingKekRequired)
        ));

        let mut r = raw(Some("always"), d);
        r.encryption = Some(raw_enc(true, Some("too-short"), Some("k")));
        assert!(matches!(
            compile_recording(r),
            Err(CompileError::RecordingKekInvalid(_))
        ));

        let mut r = raw(Some("always"), d);
        r.encryption = Some(raw_enc(true, Some(&"zz".repeat(32)), Some("k")));
        assert!(matches!(
            compile_recording(r),
            Err(CompileError::RecordingKekInvalid(_))
        ));

        let mut r = raw(Some("always"), d);
        r.encryption = Some(raw_enc(
            true,
            Some(&"ab".repeat(32)),
            Some(&"x".repeat(256)),
        ));
        assert!(matches!(
            compile_recording(r),
            Err(CompileError::RecordingKeyIdInvalid(256))
        ));
    }

    fn raw_storage(overrides: &[(&str, &str)]) -> crate::raw::RawRecordingStorage {
        let mut r = crate::raw::RawRecordingStorage {
            enabled: Some(true),
            endpoint: Some("http://127.0.0.1:9000".into()),
            bucket: Some("recs".into()),
            region: Some("us-east-1".into()),
            access_key: Some("ak".into()),
            secret_key: Some("sk".into()),
            key_template: None,
            delete_local_after_upload: None,
            spool_dir: Some(
                std::env::temp_dir()
                    .join("siphon_cfg_storage_spool")
                    .to_string_lossy()
                    .into_owned(),
            ),
        };
        for (k, v) in overrides {
            let v = Some(v.to_string());
            match *k {
                "endpoint" => r.endpoint = v,
                "bucket" => r.bucket = v,
                "key_template" => r.key_template = v,
                other => panic!("unknown override {other}"),
            }
        }
        r
    }

    #[test]
    fn encryption_kms_compiles_and_is_xor_with_kek() {
        let dir = std::env::temp_dir().join("siphon_rec_cfg_kms_test");
        let kms = crate::raw::RawRecordingKms {
            key_arn: Some("arn:aws:kms:us-east-1:000000000000:key/abc".into()),
            region: Some("us-east-1".into()),
            access_key: Some("ak".into()),
            secret_key: Some("sk".into()),
            endpoint: Some("http://127.0.0.1:4566".into()),
        };

        // KMS alone compiles.
        let mut r = raw(Some("always"), dir.to_str());
        let mut enc = raw_enc(true, None, Some("kms-1"));
        enc.kms = Some(kms.clone());
        r.encryption = Some(enc);
        let c = compile_recording(r).unwrap();
        assert_eq!(c.encryption.as_ref().unwrap().key_id(), "kms-1");

        // kek + kms together fail loud.
        let mut r = raw(Some("always"), dir.to_str());
        let mut enc = raw_enc(true, Some(&"ab".repeat(32)), Some("kms-1"));
        enc.kms = Some(kms.clone());
        r.encryption = Some(enc);
        assert!(matches!(
            compile_recording(r),
            Err(CompileError::RecordingKekXorKms)
        ));

        // Missing kms field fails loud.
        let mut incomplete = kms;
        incomplete.region = None;
        let mut r = raw(Some("always"), dir.to_str());
        let mut enc = raw_enc(true, None, Some("kms-1"));
        enc.kms = Some(incomplete);
        r.encryption = Some(enc);
        assert!(matches!(
            compile_recording(r),
            Err(CompileError::RecordingKmsField("region"))
        ));
    }

    #[test]
    fn storage_compiles_with_defaults() {
        let rec = compile_recording(raw(
            Some("always"),
            std::env::temp_dir().join("siphon_cfg_storage_rec").to_str(),
        ))
        .unwrap();
        let up = super::compile_recording_storage(Some(raw_storage(&[])), &rec)
            .unwrap()
            .expect("enabled storage compiles");
        assert_eq!(up.key_template, "{date}/{call_id}");
        assert!(!up.delete_local_after_upload);
        assert_eq!(up.target.bucket, "recs");
    }

    #[test]
    fn storage_disabled_or_absent_is_none() {
        let rec = compile_recording(RawRecording::default()).unwrap();
        assert!(super::compile_recording_storage(None, &rec)
            .unwrap()
            .is_none());
        let mut r = raw_storage(&[]);
        r.enabled = Some(false);
        assert!(super::compile_recording_storage(Some(r), &rec)
            .unwrap()
            .is_none());
    }

    #[test]
    fn storage_validation_fails_loud() {
        let rec = compile_recording(RawRecording::default()).unwrap();

        let mut r = raw_storage(&[]);
        r.bucket = None;
        assert!(matches!(
            super::compile_recording_storage(Some(r), &rec),
            Err(CompileError::RecordingStorageField("bucket"))
        ));

        let r = raw_storage(&[("endpoint", "s3.amazonaws.com")]);
        assert!(matches!(
            super::compile_recording_storage(Some(r), &rec),
            Err(CompileError::RecordingStorageEndpointInvalid(_))
        ));

        let r = raw_storage(&[("key_template", "{date}/all-calls")]);
        assert!(matches!(
            super::compile_recording_storage(Some(r), &rec),
            Err(CompileError::RecordingStorageKeyTemplateInvalid(
                _,
                "missing {call_id}"
            ))
        ));

        let r = raw_storage(&[("key_template", "{tenant}/{call_id}")]);
        assert!(matches!(
            super::compile_recording_storage(Some(r), &rec),
            Err(CompileError::RecordingStorageKeyTemplateInvalid(
                _,
                "unknown placeholder"
            ))
        ));

        let r = raw_storage(&[("key_template", "{call_id}/{oops")]);
        assert!(matches!(
            super::compile_recording_storage(Some(r), &rec),
            Err(CompileError::RecordingStorageKeyTemplateInvalid(
                _,
                "unbalanced braces"
            ))
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
mod outbound_tests {
    use super::{compile_outbound, CompileError, OutboundConfig, RegisterConfig};
    use crate::raw::{RawGateway, RawOutbound};
    use crate::SipTransport;
    use std::net::SocketAddr;
    use std::time::Duration;

    fn gw(name: &str) -> RawGateway {
        RawGateway {
            name: name.into(),
            ..Default::default()
        }
    }

    fn register(name: &str) -> RegisterConfig {
        RegisterConfig {
            name: name.into(),
            server_addr: "10.0.0.1:5061".parse::<SocketAddr>().unwrap(),
            server_host: "pbx.example.com".into(),
            transport: SipTransport::Tls,
            username: "siphon".into(),
            auth_username: "siphon-auth".into(),
            password: "pw".into(),
            realm: Some("pbx".into()),
            expires: Duration::from_secs(3600),
            register_on_startup: true,
        }
    }

    #[test]
    fn standalone_trunk_compiles_with_auth() {
        let raw = RawGateway {
            proxy: Some("twilio.example:5060".into()),
            from: Some("sip:+13125551234@twilio.example".into()),
            auth_username: Some("acct".into()),
            auth_password: Some("token".into()),
            ..gw("twilio")
        };
        let out = compile_outbound(RawOutbound::default(), vec![raw], &[]).unwrap();
        let g = out.gateway("twilio").expect("gateway present");
        assert_eq!(g.proxy_host, "twilio.example");
        assert_eq!(g.proxy_port, 5060);
        assert_eq!(
            g.request_uri("+15551112222"),
            "sip:+15551112222@twilio.example:5060"
        );
        let creds = g.credentials.as_ref().expect("creds");
        assert_eq!(
            (creds.username.as_str(), creds.password.as_str()),
            ("acct", "token")
        );
        // SRTP defaults to off (unchanged 0.6.x behaviour).
        assert_eq!(g.srtp, siphon_ai_core::SrtpMode::Off);
    }

    #[test]
    fn gateway_srtp_required_compiles_over_tls() {
        let raw = RawGateway {
            proxy: Some("twilio.example".into()),
            from: Some("sip:+13125551234@twilio.example".into()),
            transport: Some("tls".into()),
            srtp: Some("required".into()),
            ..gw("twilio")
        };
        let out = compile_outbound(RawOutbound::default(), vec![raw], &[]).unwrap();
        let g = out.gateway("twilio").expect("gateway present");
        assert_eq!(g.srtp, siphon_ai_core::SrtpMode::Required);
        assert_eq!(g.transport, SipTransport::Tls);
    }

    #[test]
    fn gateway_srtp_preferred_without_tls_still_compiles() {
        // The SDES-key-in-the-clear case warns at load but is not an error
        // (operator may accept the risk on a private network).
        let raw = RawGateway {
            proxy: Some("trunk.example".into()),
            from: Some("sip:+13125551234@trunk.example".into()),
            srtp: Some("preferred".into()),
            ..gw("trunk")
        };
        let out = compile_outbound(RawOutbound::default(), vec![raw], &[]).unwrap();
        assert_eq!(
            out.gateway("trunk").unwrap().srtp,
            siphon_ai_core::SrtpMode::Preferred
        );
    }

    #[test]
    fn gateway_bad_srtp_value_is_rejected() {
        let raw = RawGateway {
            proxy: Some("trunk.example".into()),
            from: Some("sip:x@trunk.example".into()),
            srtp: Some("encrypted".into()),
            ..gw("trunk")
        };
        let err = compile_outbound(RawOutbound::default(), vec![raw], &[]).unwrap_err();
        assert!(matches!(err, CompileError::UnknownSrtpMode(v) if v == "encrypted"));
    }

    #[test]
    fn register_reuse_inherits_server_and_creds() {
        let raw = RawGateway {
            register: Some("pbx".into()),
            ..gw("pbx-out")
        };
        let out = compile_outbound(RawOutbound::default(), vec![raw], &[register("pbx")]).unwrap();
        let g = out.gateway("pbx-out").unwrap();
        assert_eq!(g.proxy_host, "pbx.example.com");
        assert_eq!(g.proxy_port, 5061);
        assert_eq!(g.from, "sip:siphon@pbx.example.com"); // default AOR
        let creds = g.credentials.as_ref().unwrap();
        assert_eq!(creds.username, "siphon-auth");
        assert_eq!(creds.realm.as_deref(), Some("pbx"));
    }

    #[test]
    fn max_concurrent_drives_enabled() {
        assert!(!OutboundConfig::default().enabled());
        let out = compile_outbound(
            RawOutbound {
                max_concurrent: Some(5),
                rate_limit_per_sec: Some(2),
            },
            vec![],
            &[],
        )
        .unwrap();
        assert!(out.enabled());
        assert_eq!(out.max_concurrent, 5);
        assert_eq!(out.rate_limit_per_sec, Some(2));
    }

    #[test]
    fn validation_failures_are_loud() {
        let only = |g: RawGateway, regs: &[RegisterConfig]| {
            compile_outbound(RawOutbound::default(), vec![g], regs).unwrap_err()
        };
        // No proxy and no register.
        assert!(matches!(
            only(gw("bare"), &[]),
            CompileError::GatewayNeedsProxyOrRegister { .. }
        ));
        // Unknown register reference.
        assert!(matches!(
            only(
                RawGateway {
                    register: Some("nope".into()),
                    ..gw("x")
                },
                &[]
            ),
            CompileError::GatewayUnknownRegister { .. }
        ));
        // from missing the sip: scheme.
        assert!(matches!(
            only(
                RawGateway {
                    proxy: Some("h:5060".into()),
                    from: Some("+1555@h".into()),
                    ..gw("x")
                },
                &[]
            ),
            CompileError::GatewayBadFrom { .. }
        ));
        // username without password.
        assert!(matches!(
            only(
                RawGateway {
                    proxy: Some("h:5060".into()),
                    from: Some("sip:a@h".into()),
                    auth_username: Some("u".into()),
                    ..gw("x")
                },
                &[]
            ),
            CompileError::GatewayIncompleteAuth { .. }
        ));
        // duplicate names.
        assert!(matches!(
            compile_outbound(
                RawOutbound::default(),
                vec![
                    RawGateway {
                        register: Some("pbx".into()),
                        ..gw("dup")
                    },
                    RawGateway {
                        register: Some("pbx".into()),
                        ..gw("dup")
                    },
                ],
                &[register("pbx")]
            )
            .unwrap_err(),
            CompileError::GatewayDuplicateName { .. }
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
