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

    #[serde(default, rename = "register")]
    pub registrations: Vec<RawRegister>,

    /// `[[trunk]]` — peer-trunk allowlist. Identifies inbound SIP
    /// peers by source IP and/or From-URI host. When zero blocks
    /// are declared, the daemon accepts INVITEs from any source
    /// (legacy / dev posture). When one or more are declared,
    /// every inbound INVITE must match a trunk or it's rejected
    /// 403. See `docs/CONFIG.md` for the full grammar and threat
    /// model.
    #[serde(default, rename = "trunk")]
    pub trunks: Vec<RawTrunk>,

    #[serde(default)]
    pub security: RawSecurity,

    #[serde(default)]
    pub recording: RawRecording,

    /// `[[gateway]]` — outbound SIP trunks/providers SiphonAI dials
    /// *through* for originated calls (0.6.0).
    #[serde(default, rename = "gateway")]
    pub gateways: Vec<RawGateway>,

    #[serde(default)]
    pub outbound: RawOutbound,

    /// `[conference]` — multi-party rooms (0.7.0). Off by default.
    #[serde(default)]
    pub conference: RawConference,

    /// `[park]` — media-only call park (0.7.0). Off by default.
    #[serde(default)]
    pub park: RawPark,

    #[serde(default)]
    pub cdr: RawCdr,

    #[serde(default)]
    pub observability: RawObservability,

    #[serde(default)]
    pub webhooks: RawWebhooks,

    /// `[audit]` — signed audit-event stream for SIEM ingestion
    /// (0.20.0). Off by default.
    #[serde(default)]
    pub audit: RawAudit,

    #[serde(default)]
    pub hep: RawHep,

    /// `[admin]` — the authenticated admin API listener. Absent → the
    /// `/admin/*` surface is not served at all (secure default).
    #[serde(default)]
    pub admin: Option<RawAdmin>,

    /// `[shutdown]` — graceful drain on SIGTERM/SIGINT (0.17.0). Off by
    /// default in the sense of "today's behaviour" only when
    /// `drain_timeout_secs = 0`; absent → the 30 s default drain.
    #[serde(default)]
    pub shutdown: RawShutdown,
}

/// `[shutdown]` — graceful connection draining on a shutdown signal.
/// See `docs/design/DESIGN_GRACEFUL_SHUTDOWN.md`.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawShutdown {
    /// Max seconds to let active calls finish on SIGTERM/SIGINT before
    /// forcing teardown. `None` (unset) → 30 s default. `0` → no drain
    /// (immediate exit, today's behaviour). Must be ≤ the deployment's
    /// `terminationGracePeriodSeconds` or the orchestrator SIGKILLs
    /// mid-drain.
    #[serde(default)]
    pub drain_timeout_secs: Option<u64>,
}

/// `[admin]` — bearer-token-authenticated admin API on its own listener
/// (separate from `[observability]`'s open metrics/health). See
/// `docs/design/DESIGN_ADMIN_AUTH.md`.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawAdmin {
    /// `host:port` the admin listener binds. Required.
    pub listen: String,
    /// One or more bearer tokens, each with a role. At least one is
    /// required (an admin listener with no tokens authenticates nobody).
    #[serde(default, rename = "token")]
    pub tokens: Vec<RawAdminToken>,
    /// Optional `[admin.tls]` — serve the admin API over HTTPS so the
    /// bearer token isn't sent in the clear on a routable bind.
    #[serde(default)]
    pub tls: Option<RawAdminTls>,
}

/// `[admin.tls]` — server-side TLS for the admin listener.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawAdminTls {
    /// PEM-encoded certificate chain (path on disk). Required when
    /// `[admin.tls]` is present.
    #[serde(default)]
    pub cert: Option<String>,
    /// PEM-encoded private key (path on disk). Required when
    /// `[admin.tls]` is present.
    #[serde(default)]
    pub key: Option<String>,
}

/// One `[[admin.token]]` entry.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawAdminToken {
    /// Label for audit logs — NOT a secret. Must be unique.
    pub name: String,
    /// The bearer secret (env-expanded). Stored only as a SHA-256 hash
    /// after load.
    pub token: String,
    /// `"readonly"` | `"operator"` | `"admin"`.
    pub role: String,
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
    /// `host:port` to bind UDP / TCP on. Required.
    pub listen: String,
    /// Transports to enable on `listen`. Default: `["udp"]`. Valid
    /// entries: `udp`, `tcp`, `tls`. `tls` requires `[sip.tls]` to
    /// be configured (cert/key); compile-time validation enforces
    /// that.
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
    /// TLS sub-block. Even when `transports = ["tls"]` is set,
    /// `[sip.tls]` must supply cert/key paths. Defaults are all
    /// "off" so an `[sip]` block without `[sip.tls]` keeps working
    /// for UDP-only deployments.
    #[serde(default)]
    pub tls: RawSipTls,
    /// Client-side TLS sub-block — verification roots for OUTGOING
    /// TLS connections (gateways / registrations with
    /// `transport = "tls"`). Independent of `[sip.tls]`, which is
    /// the server side. Unset = the bundled webpki roots only.
    #[serde(default)]
    pub tls_client: RawSipTlsClient,
    /// Call-progress sub-block — how the UAS responds to inbound
    /// INVITEs before the 2xx. Unset = `mode = "instant_answer"`
    /// (v0.1.0 behaviour).
    #[serde(default)]
    pub call_progress: RawCallProgress,
    /// RFC 4028 Min-SE we'll enforce on inbound INVITEs. Defaults
    /// to 90 (RFC minimum). Smaller values are rejected with 422.
    #[serde(default)]
    pub min_session_expires_secs: Option<u32>,
    /// Optional UAS preference for Session-Expires. When the peer's
    /// request exceeds this value the negotiated timer is capped
    /// here. Unset = honour the peer's value uncapped.
    #[serde(default)]
    pub preferred_session_expires_secs: Option<u32>,
    /// Accept an inbound INVITE with no SDP (RFC 3264 delayed offer):
    /// SiphonAI offers in the 200 OK and reads the peer's answer from
    /// the ACK. Needed for CUCM trunks/phones without a forced MTP.
    /// Default `true`. Set `false` to force strict early offer — an
    /// offerless INVITE is then rejected (488).
    #[serde(default = "default_true")]
    pub allow_delayed_offer: bool,
    /// `[sip.auth]` — RFC 3261 §22 inbound digest authentication.
    /// Unset/`enabled = false` ⇒ off (today's behaviour).
    #[serde(default)]
    pub auth: Option<RawSipAuth>,
    /// `[sip.admission]` — per-source INVITE rate limiting + a global
    /// concurrency cap. Unset / all-zero ⇒ off.
    #[serde(default)]
    pub admission: Option<RawSipAdmission>,
    /// Idle timeout (seconds) for an **established** inbound SIP-over-TCP/TLS
    /// connection — one that has completed at least one SIP message. A SIP
    /// trunk (e.g. CUCM) holds this connection open for a call's whole life
    /// while sending no SIP (RTP is out-of-band), so this must exceed your
    /// longest SIP-quiet period. Default 1800 (matches typical session
    /// timers). `0` disables the idle close entirely. Does NOT shorten the
    /// separate Slowloris window for connections that never complete a
    /// request. UDP is connectionless and unaffected.
    #[serde(default)]
    pub tcp_idle_timeout_secs: Option<u64>,
}

/// `[sip.admission]` — inbound INVITE admission control. A per-source
/// token bucket sheds floods cheaply (before trunk/auth/route work), and
/// an optional global cap bounds concurrent calls.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawSipAdmission {
    /// Per-source new-INVITE rate (tokens/sec), keyed on the source IP.
    /// `0`/unset ⇒ no per-source limit.
    #[serde(default)]
    pub max_per_sec: Option<u32>,
    /// Per-source bucket capacity (burst). Default = `max_per_sec`.
    #[serde(default)]
    pub burst: Option<u32>,
    /// Consecutive per-source rejections after which further INVITEs from
    /// that source are **silently dropped** instead of answered `503`
    /// (don't spend a response on an obvious flood). Default `10`.
    #[serde(default)]
    pub drop_after: Option<u32>,
    /// Global cap on concurrent active calls. A new INVITE past this is
    /// answered `503`. `0`/unset ⇒ no cap.
    #[serde(default)]
    pub max_concurrent: Option<u32>,
    /// Cap on the number of distinct source IPs tracked (bounded memory;
    /// idle/oldest entries are evicted past this). Default `10000`.
    #[serde(default)]
    pub max_sources: Option<u32>,
}

fn default_true() -> bool {
    true
}

/// `[sip.auth]` — challenge inbound INVITEs with RFC 3261 §22 digest
/// authentication, verified against a configured credential set.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawSipAuth {
    /// Master switch. `false` (default) ⇒ no challenge is ever issued.
    #[serde(default)]
    pub enabled: bool,
    /// The digest `realm` advertised in the challenge and folded into
    /// HA1. Required when `enabled`. Typically the SIP domain.
    #[serde(default)]
    pub realm: Option<String>,
    /// Digest algorithm: `MD5`, `SHA-256` (default), or `SHA-512`
    /// (case-insensitive). MD5 is accepted for legacy peers but is
    /// cryptographically weak (RFC 7616 §3).
    #[serde(default)]
    pub algorithm: Option<String>,
    /// Quality of protection: `auth` (default) or `auth-int`.
    #[serde(default)]
    pub qop: Option<String>,
    /// The credential set. At least one `[[sip.auth.user]]` is
    /// required when `enabled`. The password is held in memory to
    /// compute HA1 on verify (the upstream digest verifier needs the
    /// cleartext, like `[[gateway]]`/`[[register]]`); use
    /// `${file:…}`/`${cred:…}` to keep it out of the config file.
    #[serde(default, rename = "user")]
    pub users: Vec<RawSipAuthUser>,
}

/// One `[[sip.auth.user]]` credential.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawSipAuthUser {
    /// SIP username presented in the `Authorization` header.
    pub username: String,
    /// Shared secret (env/secret-expanded). Non-empty.
    pub password: String,
}

/// `[sip.call_progress]` — what — if any — provisional response
/// `siphon-ai` layers on top of `IntegratedUAS`'s `100 Trying`
/// before the 2xx. See `docs/design/DEV_PLAN_0.2.0.md` §4.1.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawCallProgress {
    /// `"instant_answer"` (default) | `"ringing"` | `"session_progress"`.
    /// `instant_answer` matches v0.1.0 behaviour (skip extra
    /// provisional). `ringing` sends `180 Ringing`. `session_progress`
    /// sends `183 Session Progress` with the negotiated answer SDP
    /// (best-effort; peers requiring `100rel` fall back to
    /// `instant_answer` per the §9.1 decision).
    #[serde(default)]
    pub mode: Option<String>,
}

/// `[sip.tls]` — TLS server configuration. Required when
/// `[sip].transports` includes `"tls"`. v1 only does inbound
/// (server-side) TLS; outbound TLS for UAC mode is a follow-up.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawSipTls {
    /// `host:port` to bind the TLS listener on. If unset, the
    /// listener defaults to the same host as `[sip].listen` on
    /// port 5061 (the SIPS standard). Set explicitly for
    /// non-standard ports.
    #[serde(default)]
    pub listen: Option<String>,
    /// PEM-encoded certificate chain (path on disk). Required.
    #[serde(default)]
    pub cert: Option<String>,
    /// PEM-encoded private key (path on disk). Required.
    #[serde(default)]
    pub key: Option<String>,
}

/// `[sip.tls_client]` — verification roots for outgoing TLS.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawSipTlsClient {
    /// Path to a PEM bundle appended to the bundled webpki roots —
    /// for trunks fronted by a private CA, and for test rigs with
    /// self-signed certs.
    #[serde(default)]
    pub extra_ca: Option<String>,
}

fn default_transports() -> Vec<String> {
    vec!["udp".to_string()]
}

/// `[media]` — codecs + DTMF + RTP port range + inactivity watchdog.
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
    /// Tear the call down after this many seconds with no inbound RTP.
    /// `None` (unset) → defaults to 60 s at compile time. `Some(0)` →
    /// watchdog disabled. Per-route `[route.media].inactivity_timeout_secs`
    /// overrides this value.
    #[serde(default)]
    pub inactivity_timeout_secs: Option<u64>,
    /// SRTP negotiation mode — `"off"` | `"preferred"` | `"required"`.
    /// `None` (unset) → defaults to `"off"` at compile time, preserving
    /// v0.2.0 behaviour (plaintext-RTP only). Per-route
    /// `[route.media].srtp` overrides this value.
    ///
    /// Behaviour by mode:
    ///   * `"off"` — answer plaintext only. An offer with an `RTP/SAVP`
    ///     or `UDP/TLS/RTP/SAVPF` profile is rejected with 488 (no
    ///     silent downgrade to plaintext).
    ///   * `"preferred"` — answer SRTP when the offer carries it;
    ///     fall back to plaintext otherwise.
    ///   * `"required"` — refuse plaintext-RTP offers with 488.
    ///
    /// The mode names + semantics are enforced at config-load time
    /// via [`compile::compile_srtp_mode`]; unknown strings are a
    /// fail-loud error per CLAUDE.md §4.6.
    #[serde(default)]
    pub srtp: Option<String>,
    /// Which SRTP key-exchange to **offer** when SiphonAI is the offerer
    /// on a **delayed offer** (inbound offerless INVITE) and `srtp` is
    /// `"preferred"`/`"required"` — `"sdes"` (default) or `"dtls"`. Only
    /// the delayed-offer path offers SRTP; inbound early offer always
    /// *answers* the peer's choice, so this is ignored there. Unknown
    /// strings are a fail-loud error.
    #[serde(default)]
    pub srtp_offer: Option<String>,
    /// Hold-music file played to the caller during a bot-initiated hold
    /// (0.7.2) — a WAV whose native rate matches the call's negotiated
    /// rate (no resampling in v1; a mismatch falls back to generated
    /// comfort silence, same rule as `[park].moh_file`). `None` (unset)
    /// → comfort silence. Validated to exist at load time.
    #[serde(default)]
    pub moh_file: Option<String>,
}

/// `[security]` — call-authentication policy (STIR/SHAKEN, 0.4.0).
/// Entirely optional; the feature is inert unless
/// `[security.stir_shaken].enabled = true`. Compiled and validated via
/// [`compile::compile_security`].
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawSecurity {
    /// Minimum attestation a call must carry to be accepted:
    /// `"none"` (default) | `"A"` | `"B"` | `"C"`. Requires
    /// `[security.stir_shaken].enabled = true` to have any effect — a
    /// non-`none` value without verification rejects every call, which is
    /// a fail-loud config error.
    #[serde(default)]
    pub min_attestation: Option<String>,
    /// SIP status returned when the attestation gate rejects a call:
    /// `403` (default) | `488` | `606`.
    #[serde(default)]
    pub min_attestation_response: Option<u16>,
    /// `[security.stir_shaken]` verification sub-block.
    #[serde(default)]
    pub stir_shaken: RawStirShaken,
}

/// `[security.stir_shaken]` — STIR/SHAKEN verification settings.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawStirShaken {
    /// Master switch. `false` (default) → no Identity parsing/verification
    /// and no `verstat` surfaced (0.3.x behaviour preserved).
    #[serde(default)]
    pub enabled: Option<bool>,
    /// Path to the PEM bundle of STI-PA trust anchors (ship
    /// `contrib/sti-pa-roots.pem`). Required when `enabled = true`;
    /// validated at load time (must exist and hold ≥1 PEM certificate).
    #[serde(default)]
    pub trust_anchors: Option<String>,
    /// How long a fetched signing certificate is cached, in seconds.
    /// `None` → 3600 (1 hour). (Seconds, for consistency with the other
    /// duration fields in this config; the plan's `"1h"` string form is a
    /// possible later ergonomics pass.)
    #[serde(default)]
    pub cert_cache_ttl_secs: Option<u64>,
    /// Reject INVITEs with no `Identity` header (428 "Use Identity Header")
    /// instead of admitting them as unsigned. Default `false`.
    #[serde(default)]
    pub require_identity: Option<bool>,
    /// PASSporT `iat` freshness window, in seconds (replay protection,
    /// ATIS-1000074). `None` → 60. `0` disables the check.
    #[serde(default)]
    pub iat_freshness_secs: Option<u64>,
    /// Optional PEM bundle of extra CA cert(s) trusted for the `x5u` HTTPS
    /// fetch only (private/lab x5u hosting). `None` → public web PKI only.
    /// Validated at load when `enabled`.
    #[serde(default)]
    pub x5u_tls_extra_ca: Option<String>,
}

/// `[recording]` — per-call audio recording (0.5.0). Off by default.
#[derive(Debug, Default, Clone, Deserialize)]
pub struct RawRecording {
    /// `"off"` (default) / `"always"`. (`"on_demand"` is a later chunk.)
    #[serde(default)]
    pub mode: Option<String>,
    /// Directory recordings are written to. Required when `mode != "off"`.
    #[serde(default)]
    pub dir: Option<String>,
    /// `"wav"` (default) or `"opus"` (0.25.0).
    #[serde(default)]
    pub format: Option<String>,
    /// `[recording.encryption]` (0.24.0) — envelope encryption at rest.
    #[serde(default)]
    pub encryption: Option<RawRecordingEncryption>,
    /// `[recording.storage]` (0.25.0) — S3-compatible upload.
    #[serde(default)]
    pub storage: Option<RawRecordingStorage>,
}

/// `[recording.storage]` — upload finalized recordings to S3-compatible
/// object storage (0.25.0). Off by default.
#[derive(Debug, Default, Clone, Deserialize)]
pub struct RawRecordingStorage {
    #[serde(default)]
    pub enabled: Option<bool>,
    /// Scheme + host (+ port), e.g. `https://s3.us-east-1.amazonaws.com`
    /// or a MinIO/R2/B2 URL.
    #[serde(default)]
    pub endpoint: Option<String>,
    #[serde(default)]
    pub bucket: Option<String>,
    #[serde(default)]
    pub region: Option<String>,
    /// Use `${cred:}` / `${file:}` references, never inline secrets.
    #[serde(default)]
    pub access_key: Option<String>,
    #[serde(default)]
    pub secret_key: Option<String>,
    /// Object-key template; `{call_id}` / `{date}` / `{route}` /
    /// `{direction}`. Default `"{date}/{call_id}"`. Must contain
    /// `{call_id}` (key uniqueness).
    #[serde(default)]
    pub key_template: Option<String>,
    /// Delete the local file after a durable upload. Default false.
    #[serde(default)]
    pub delete_local_after_upload: Option<bool>,
    /// Durable job spool (survives restarts). Required when enabled.
    #[serde(default)]
    pub spool_dir: Option<String>,
}

/// `[recording.encryption]` — seal recordings into `.wava` envelopes
/// (0.24.0). Off by default (`enabled = false`), mirroring
/// `[observability.otlp]`.
#[derive(Debug, Default, Clone, Deserialize)]
pub struct RawRecordingEncryption {
    #[serde(default)]
    pub enabled: Option<bool>,
    /// The KEK as 64 hex chars — reference a secret with `${file:}` /
    /// `${cred:}`, never inline it.
    #[serde(default)]
    pub kek: Option<String>,
    /// Identifier stamped into recording headers; names which KEK wrapped
    /// each recording so keys can rotate.
    #[serde(default)]
    pub key_id: Option<String>,
    /// `[recording.encryption.kms]` (0.25.0) — wrap the per-recording
    /// data key via AWS KMS instead of a local `kek`. Exactly one of
    /// `kek` / `kms` when enabled.
    #[serde(default)]
    pub kms: Option<RawRecordingKms>,
}

/// `[recording.encryption.kms]` — AWS KMS as the KEK (0.25.0).
#[derive(Debug, Default, Clone, Deserialize)]
pub struct RawRecordingKms {
    /// KMS key to wrap new recordings' data keys with.
    #[serde(default)]
    pub key_arn: Option<String>,
    #[serde(default)]
    pub region: Option<String>,
    /// Use `${cred:}` / `${file:}` references, never inline secrets.
    #[serde(default)]
    pub access_key: Option<String>,
    #[serde(default)]
    pub secret_key: Option<String>,
    /// Endpoint override for KMS-compatible emulators (LocalStack).
    #[serde(default)]
    pub endpoint: Option<String>,
}

/// `[conference]` — conference rooms (0.7.0). Fail-closed like
/// `[outbound]`: with `enabled = false` (the default) every join is
/// refused and a 0.6.x deployment upgrades with zero behaviour
/// change.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawConference {
    #[serde(default)]
    pub enabled: bool,
    /// Live rooms across the daemon. Default 16.
    #[serde(default)]
    pub max_rooms: Option<u32>,
    /// Member *calls* per room (each contributes its SIP leg and its
    /// WS session to the mix). Default 8.
    #[serde(default)]
    pub max_participants_per_room: Option<u32>,
    /// Play a short chime into the room on join/leave. Default false.
    #[serde(default)]
    pub join_tones: bool,
}

/// `[park]` — media-only call park (0.7.0). Fail-closed like
/// `[conference]`: with `enabled = false` (the default) park is refused
/// and a 0.6.x deployment upgrades with zero behaviour change.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawPark {
    #[serde(default)]
    pub enabled: bool,
    /// Optional hold-music file (WAV/MP3/OGG/…). Looped while parked.
    /// Unset → comfort noise. Existence + decodability checked at load.
    #[serde(default)]
    pub moh_file: Option<String>,
    /// Seconds a call may stay parked before `timeout_action` fires.
    /// Default 300. `0` disables the timeout (park indefinitely).
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    /// What happens at timeout: `"hangup"` (default) or `"keep"`.
    #[serde(default)]
    pub timeout_action: Option<String>,
    /// Max simultaneously-parked calls across the daemon. Default 32.
    #[serde(default)]
    pub max_parked: Option<u32>,
}

/// `[[gateway]]` — one outbound trunk/provider (0.6.0). A gateway is the
/// SIP peer SiphonAI sends originated INVITEs *through*. Two forms:
///
/// - **Standalone trunk**: set `proxy` + `from` (+ optional digest auth).
/// - **Register reuse**: set `register = "<name>"` to dial through a
///   `[[register]]` entry, inheriting its server address, credentials, and
///   AOR (used as the default `from`).
#[derive(Debug, Default, Clone, Deserialize)]
pub struct RawGateway {
    pub name: String,
    /// `host` or `host:port` of the trunk. Required unless `register` is set.
    #[serde(default)]
    pub proxy: Option<String>,
    /// `udp` (default) | `tcp` | `tls` — transport for calls placed
    /// through this trunk. With `tls`, the default proxy port becomes
    /// 5061 and the daemon verifies the trunk's certificate against
    /// its client TLS roots (webpki + `[sip.tls_client].extra_ca`).
    /// Must be unset when `register` is set — the transport is
    /// inherited from the register block.
    #[serde(default)]
    pub transport: Option<String>,
    /// Default caller-ID — a full `sip:` URI. Required for standalone
    /// trunks; defaults to the register AOR when `register` is set.
    #[serde(default)]
    pub from: Option<String>,
    /// Name of a `[[register]]` to dial through (reuse its server + creds).
    #[serde(default)]
    pub register: Option<String>,
    /// Digest username for the trunk (standalone form). `${VAR}`-expandable.
    #[serde(default)]
    pub auth_username: Option<String>,
    /// Digest password for the trunk (standalone form).
    #[serde(default)]
    pub auth_password: Option<String>,
    /// Optional digest realm hint.
    #[serde(default)]
    pub realm: Option<String>,
    /// SRTP policy for media on calls placed through this trunk (0.7.x).
    /// `"off"` (default) | `"preferred"` | `"required"` — the outbound
    /// mirror of `[media].srtp`. `preferred` offers SDES SRTP but accepts a
    /// plaintext downgrade; `required` fails the call if the trunk won't do
    /// SRTP. Pair with `transport = "tls"` — SDES keys travel on the
    /// signalling plane, so plaintext SIP leaks them (warned at load).
    #[serde(default)]
    pub srtp: Option<String>,
}

/// `[outbound]` — global outbound-origination controls (0.6.0). The native
/// guardrails for the originate path (which has no built-in auth — the
/// endpoint is fronted by a reverse proxy, see `docs/design/DEV_PLAN_0.6.0.md` §9.5).
#[derive(Debug, Default, Clone, Deserialize)]
pub struct RawOutbound {
    /// Max simultaneous outbound calls. `0` (the default) disables outbound
    /// origination entirely (fail-closed). Set a positive cap to enable it.
    #[serde(default)]
    pub max_concurrent: Option<usize>,
    /// Optional ceiling on new outbound calls per second (token bucket).
    /// `None` / `0` = no rate limit (the concurrency cap still applies).
    #[serde(default)]
    pub rate_limit_per_sec: Option<u32>,
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
    /// Optional HMAC-SHA256 signing secret. `${VAR}` env-expanded by
    /// the loader. When set, each record POST carries
    /// `X-SiphonAI-Signature`.
    #[serde(default)]
    pub secret: Option<String>,
    /// Optional durable spool directory. When set, a record that
    /// exhausts the in-memory retry budget is persisted here and
    /// retried by a background worker that survives restarts.
    #[serde(default)]
    pub spool_dir: Option<String>,
    #[serde(default)]
    pub retry_max: Option<u32>,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

/// `[[register]]` — a single outbound REGISTER endpoint. Zero or
/// more allowed; each becomes a `register_source` route key.
///
/// `name` is the dialplan handle (`[route.match].register_source =
/// "cucm-main"` matches a `[[register]]` block named `"cucm-main"`).
/// `server` is the registrar's host or `host:port`; if `port` is
/// supplied separately it overrides any port in `server`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawRegister {
    pub name: String,
    pub server: String,
    /// Defaults to 5060 for udp/tcp, 5061 for tls.
    #[serde(default)]
    pub port: Option<u16>,
    /// `udp` (default) | `tcp` | `tls`. v1 implements all three;
    /// when set to `tls`, the daemon uses its own client TLS roots
    /// (no per-registration TLS config in v1).
    #[serde(default)]
    pub transport: Option<String>,
    /// SIP `From` username and the AOR (`sip:<username>@<server>`).
    pub username: String,
    /// Username used in the digest challenge response. Defaults to
    /// `username` when unset.
    #[serde(default)]
    pub auth_username: Option<String>,
    /// Password for digest auth. `${VAR}` env-expanded by the
    /// upstream loader.
    pub password: String,
    /// Optional realm — most registrars supply it on the challenge
    /// so this is mostly a hint for tooling.
    #[serde(default)]
    pub realm: Option<String>,
    /// Registration lifetime in seconds. Default 3600. We refresh
    /// at `expires - 60s` so the daemon doesn't race the registrar.
    #[serde(default)]
    pub expires_secs: Option<u32>,
    /// `false` to leave the block configured-but-inactive (useful
    /// during outages). Default `true`.
    #[serde(default)]
    pub register_on_startup: Option<bool>,
}

/// `[[trunk]]` — peer-trunk allowlist entry. Identifies inbound
/// SIP peers by source IP (CIDR) and/or From-URI host. A trunk
/// MUST declare at least one of the two fields; if both are set,
/// BOTH must match (defense in depth). The matched trunk's `name`
/// becomes the call's `register_source`, so routes can scope per
/// trunk via the existing `[route.match].register_source = "..."`
/// matcher.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawTrunk {
    pub name: String,
    /// Allowed source addresses. Each entry is either an exact IP
    /// (`"203.0.113.10"`) or a CIDR (`"10.0.0.0/24"`, `"2001:db8::/32"`).
    /// Empty / unset means "don't constrain by IP" — but the trunk
    /// must then declare `from_hosts` instead.
    #[serde(default)]
    pub peer_addrs: Option<Vec<String>>,
    /// Allowed `From:` URI hostnames (case-insensitive). Useful for
    /// trunks whose egress IP rotates but the SIP From domain is
    /// stable (carrier federation). From-host matching is forgeable
    /// by an on-path attacker — pair with `peer_addrs` where
    /// possible. See `docs/CONFIG.md` for the threat model.
    #[serde(default)]
    pub from_hosts: Option<Vec<String>>,
    /// Require RFC 3261 digest authentication (`[sip.auth]`) for
    /// INVITEs from this trunk, in addition to the allowlist match.
    /// Default `false`: a static-IP carrier stays allowlist-only.
    /// Set `true` for trunks where the network identity is spoofable
    /// (no static egress IP). Requires `[sip.auth].enabled`.
    #[serde(default)]
    pub auth_required: Option<bool>,
}

/// `[webhooks]` — out-of-band lifecycle events (call_start /
/// call_end). Off by default. When enabled, requires `url`; the
/// optional `events` allowlist filters which event types are sent.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawWebhooks {
    #[serde(default)]
    pub enabled: bool,
    /// Required when `enabled = true`.
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub auth_header: Option<String>,
    /// Optional HMAC-SHA256 signing secret. `${VAR}` env-expanded by
    /// the loader. When set, each event POST carries
    /// `X-SiphonAI-Signature`.
    #[serde(default)]
    pub secret: Option<String>,
    /// Optional durable spool directory. When set, an event that
    /// exhausts the in-memory retry budget is persisted here and
    /// retried by a background worker that survives restarts.
    #[serde(default)]
    pub spool_dir: Option<String>,
    /// Allowlist of event types to deliver. Empty / unset = all.
    /// Valid values today: `"call_start"`, `"call_end"`. Unknown
    /// names are accepted but never match (no events from them).
    #[serde(default)]
    pub events: Option<Vec<String>>,
    #[serde(default)]
    pub retry_max: Option<u32>,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

/// `[audit]` — signed audit-event stream (0.20.0). A tamper-evident
/// trail of admin + security decisions for SIEM ingestion. Off by
/// default. Ships to an append-only JSONL file (`[audit.file]`) and/or
/// an HMAC-signed webhook (`[audit.webhook]`); enable either or both.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawAudit {
    /// Master switch. When `false` the daemon installs no audit sink
    /// and `audit::emit` is a no-op regardless of the sub-blocks.
    #[serde(default)]
    pub enabled: bool,

    /// Allowlist of event types to record. Empty / unset = all. Valid
    /// values: `"admin_request"`, `"sip_auth"`, `"invite_rejected"`,
    /// `"attestation_rejected"`, `"config_reload"`, `"cert_reload"`.
    /// Unknown names are accepted but never match.
    #[serde(default)]
    pub events: Option<Vec<String>>,

    #[serde(default)]
    pub file: RawAuditFile,

    #[serde(default)]
    pub webhook: RawAuditWebhook,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawAuditFile {
    #[serde(default)]
    pub enabled: bool,
    /// Required when `enabled = true`. Append-only JSONL; the parent
    /// directory must exist at startup (the daemon does NOT mkdir).
    #[serde(default)]
    pub path: Option<String>,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawAuditWebhook {
    #[serde(default)]
    pub enabled: bool,
    /// Required when `enabled = true`.
    #[serde(default)]
    pub url: Option<String>,
    /// Optional `Authorization` header value, sent verbatim.
    #[serde(default)]
    pub auth_header: Option<String>,
    /// HMAC-SHA256 signing secret. `${VAR}` / `${file:}` / `${cred:}`
    /// resolved by the loader. Strongly recommended for an audit
    /// stream — the signature is what makes it tamper-evident.
    #[serde(default)]
    pub secret: Option<String>,
    /// Optional durable spool directory. When set, an event that
    /// exhausts the in-memory retry budget is persisted here and
    /// retried by a background worker that survives restarts.
    #[serde(default)]
    pub spool_dir: Option<String>,
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
    /// `[observability.otlp]` — OpenTelemetry OTLP trace export (0.22.0).
    /// Independent of the metrics HTTP server above — you can export traces
    /// without scraping metrics, and vice versa. Off by default.
    #[serde(default)]
    pub otlp: RawObservabilityOtlp,
}

/// `[observability.otlp]` — export per-call spans over OTLP/gRPC to a
/// collector (Tempo / Jaeger / an OpenTelemetry Collector). Off by default;
/// best-effort (a slow/unreachable collector never blocks a call).
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawObservabilityOtlp {
    #[serde(default)]
    pub enabled: bool,
    /// OTLP/gRPC endpoint. Default `http://localhost:4317`.
    #[serde(default)]
    pub endpoint: Option<String>,
    /// Head sampling ratio in `[0.0, 1.0]`. Default `1.0` (sample all).
    /// Parent-based: a sampled parent keeps its children.
    #[serde(default)]
    pub sample_ratio: Option<f64>,
    /// Per-export gRPC timeout (ms). Default `5000`.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    /// `service.name` resource attribute. Default `siphon-ai`.
    #[serde(default)]
    pub service_name: Option<String>,
    /// Extra resource attributes (e.g. `deployment.environment = "prod"`),
    /// attached to every exported span.
    #[serde(default)]
    pub attributes: Option<std::collections::BTreeMap<String, String>>,
}

/// `[hep]` — HEP3 (Homer) shipping. Off by default; when
/// `enabled = true`, `collector` is required. The capture ID
/// disambiguates multiple SiphonAI agents reporting into the same
/// Homer; the password is the HEPlify-Server shared-secret chunk
/// (`0x000E`). Both siphon-rs's SIP-message capture and forge-media's
/// RTCP capture install their global emitters from this config, plus
/// SiphonAI's own log / CDR chunks.
///
/// v1 ships UDP only — TCP/TLS are deferred to the `hep-rs` follow-up.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawHep {
    /// Master switch. Defaults to `false` so a config without `[hep]`
    /// keeps doing nothing observability-wise.
    #[serde(default)]
    pub enabled: bool,
    /// `host:port` of the Homer / HEPlify-Server UDP collector.
    /// Required when `enabled = true`.
    #[serde(default)]
    pub collector: Option<String>,
    /// Homer agent ID — required when `enabled = true`. Operators
    /// usually pick a small integer per node (e.g., 2001).
    #[serde(default)]
    pub capture_id: Option<u32>,
    /// Optional HEPlify-Server shared password. `${VAR}` env-expanded
    /// upstream like other secret fields.
    #[serde(default)]
    pub capture_password: Option<String>,
    /// Sink queue capacity. Drops on full; tune up for high call
    /// volumes. Default `256` (per `hep-rs::DEFAULT_QUEUE_CAPACITY`).
    #[serde(default)]
    pub queue_capacity: Option<usize>,
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
    /// SIP headers to forward on the bridge `start.sip.headers`.
    /// Names are case-insensitive at lookup time.
    #[serde(default)]
    pub forward_headers: Option<Vec<String>>,
    /// `[bridge.barge_in]` block. Empty = inherit defaults
    /// (`enabled = true`, `mode = "auto_clear"`).
    #[serde(default)]
    pub barge_in: RawBargeIn,
    /// One-sided silence threshold: fire `silence_detected` when the
    /// caller has been silent (no forge-vad speech) for this many
    /// milliseconds. `None` (unset) = use the 3000 ms default; `0` =
    /// disable the event entirely.
    #[serde(default)]
    pub silence_threshold_ms: Option<u64>,
    /// Two-sided dead-air threshold: fire `dead_air_detected` when
    /// NEITHER side has produced audio (no caller speech AND no
    /// outbound playout from the WS server) for this many ms.
    /// `None` (unset) = use the 10000 ms default; `0` = disable.
    #[serde(default)]
    pub dead_air_threshold_ms: Option<u64>,
    /// Periodic emission cadence for `rtp_stats` events. `None`
    /// (unset) = use the 5000 ms default (mirrors RTCP §6.2); `0`
    /// = disable the event entirely.
    #[serde(default)]
    pub rtp_stats_interval_ms: Option<u64>,
    /// WS keepalive ping cadence in seconds (PROTOCOL.md §5.6). `None`
    /// (unset) = 15 s default; `0` = disable keepalive.
    #[serde(default)]
    pub ws_ping_interval_secs: Option<u64>,
    /// Pong deadline in seconds for the keepalive ping (PROTOCOL.md §5.6).
    /// `None` = 10 s default; `0` = disable keepalive.
    #[serde(default)]
    pub ws_pong_timeout_secs: Option<u64>,
    /// `server_too_slow` start-deadline in seconds (PROTOCOL.md §3.1): the
    /// WS server must send its first audio frame (or `hangup`) within this
    /// window of `start`. `None` = 5 s default; `0` = disable.
    #[serde(default)]
    pub server_start_deadline_secs: Option<u64>,
    /// `[bridge.tls]` — mTLS for the WS bridge connection (W4 Part A).
    /// Absent = use the existing plaintext / webpki path. Present =
    /// build a custom rustls ClientConfig carrying the client cert
    /// and optional SPKI pin.
    #[serde(default)]
    pub tls: Option<RawBridgeTls>,
    /// Opt-in automatic WS reconnect mid-call (0.7.3). When `true`, an
    /// **unexpected** WS drop (server closed without a `hangup`, IO/TLS
    /// error, keepalive timeout) doesn't tear the call down: SiphonAI
    /// keeps the caller on hold music and re-dials the same `ws_url`,
    /// resuming on a fresh session (`start.reconnected: true`). `None`
    /// (unset) / `false` = the v1 behaviour (PROTOCOL.md §5.7 teardown).
    /// Per-route override via `[route.bridge].ws_reconnect_enabled`.
    #[serde(default)]
    pub ws_reconnect_enabled: Option<bool>,
    /// Total wall-clock window (seconds) a call may spend reconnecting
    /// before falling back to §5.7 teardown — i.e. how long the caller
    /// hears hold music before we give up. `None` (unset) = 30 s default.
    /// Must be `> 0` when `ws_reconnect_enabled = true`. Per-route
    /// override via `[route.bridge].ws_reconnect_max_secs`.
    #[serde(default)]
    pub ws_reconnect_max_secs: Option<u64>,
}

/// `[bridge.tls]` — mTLS settings for the bridge WS leg.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawBridgeTls {
    /// PEM-encoded client certificate chain. Must contain at least
    /// the leaf cert; intermediates allowed.
    pub client_cert: String,
    /// PEM-encoded client private key. Must match the leaf in
    /// `client_cert`.
    pub client_key: String,
    /// Optional SHA-256 SPKI pin (64 hex chars, no separators).
    /// When set, replaces default CA chain verification with
    /// exact-match against this single pin.
    #[serde(default)]
    pub pinned_sha256: Option<String>,
}

/// `[bridge.barge_in]` — global default barge-in policy.
/// Mirrors the `[route.bridge.barge_in]` override grammar so the
/// merge in the compile step is purely "if route field is `Some`,
/// take it; else inherit the default."
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawBargeIn {
    /// Master switch. Defaults to `true` on the global side.
    #[serde(default)]
    pub enabled: Option<bool>,
    /// `"auto_clear"` (default) or `"notify_only"`.
    #[serde(default)]
    pub mode: Option<String>,
    /// Playout-gated barge-in debounce (0.7.x). While the bot is playing
    /// out, a VAD speech-started is held for this many ms and only flushes
    /// if speech sustains past it — an echo / brief-noise gate that does
    /// **not** delay barge-in while the bot is silent. `0` / unset = off
    /// (immediate flush, the original behaviour). Only affects `auto_clear`.
    #[serde(default)]
    pub debounce_ms: Option<u64>,
}
