//! Compile a `RawConfig` (post env-expansion, post TOML parse) into
//! a [`Config`] the daemon can hand to its sub-crates verbatim.
//!
//! Validation rules per `docs/DEV_PLAN.md` §6.5 and CLAUDE.md §4.6:
//!
//! - `[sip].listen` parses as a `SocketAddr`.
//! - At least one transport is enabled, and every name is one of
//!   `udp` / `tcp` / `tls`.
//! - Every codec name parses via [`Codec::from_encoding_name`].
//! - `[bridge].audio_sample_rate`, when set, is `8000` or `16000`.
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

use siphon_ai_core::BridgeDefaults;
use siphon_ai_media_glue::Codec;
use siphon_ai_routes::{compile as compile_routes, RawRouteFile, RouteSet};
use thiserror::Error;
use tracing::warn;

use crate::raw::{RawBridge, RawCdr, RawConfig, RawMedia, RawNode, RawSip};

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
    pub cdr: CdrConfig,
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
}

#[derive(Debug, Error)]
pub enum CompileError {
    #[error("[sip].listen {0:?} is not a valid socket address: {1}")]
    BadSipListen(String, std::net::AddrParseError),

    #[error("[sip].transports must be non-empty")]
    NoTransports,

    #[error("[sip].transports has unknown entry {0:?}; expected udp / tcp / tls")]
    UnknownTransport(String),

    #[error("[media].codecs has unknown codec {0:?}")]
    UnknownCodec(String),

    #[error("[media].dtmf is {0:?}; expected \"rfc2833\" or \"off\"")]
    UnknownDtmfMode(String),

    #[error("[media].rtp_port_range {min}-{max} is invalid (min must be < max and even)")]
    BadRtpPortRange { min: u16, max: u16 },

    #[error("[bridge].audio_sample_rate {0} not supported (8000 or 16000)")]
    BadSampleRate(u32),

    #[error("[cdr.file].path is required when [cdr.file].enabled = true")]
    CdrFilePathRequired,

    #[error("[cdr.webhook].url is required when [cdr.webhook].enabled = true")]
    CdrWebhookUrlRequired,

    #[error(transparent)]
    Routes(#[from] siphon_ai_routes::RouteError),
}

/// Compile a raw config into the consumer-ready form.
pub fn compile(raw: RawConfig) -> Result<Config, CompileError> {
    let sip = compile_sip(raw.sip)?;
    let node = compile_node(raw.node, &sip);
    let media = compile_media(&raw.media)?;
    let bridge_defaults = compile_bridge(raw.bridge, &raw.media)?;
    let routes = compile_dialplan(raw.routes)?;
    let cdr = compile_cdr(raw.cdr)?;

    if !routes.has_default() {
        warn!(
            route_count = routes.len(),
            "no default `any = true` route configured — non-matching INVITEs will be 404'd"
        );
    }
    Ok(Config {
        node,
        sip,
        media,
        bridge_defaults,
        routes,
        cdr,
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
    Ok(SipConfig {
        listen_addr,
        transports,
        user_agent: raw.user_agent,
        contact: raw.contact,
    })
}

fn compile_node(raw: RawNode, sip: &SipConfig) -> NodeConfig {
    let id = raw.id.unwrap_or_else(|| "siphon-ai".to_string());
    let public_address = raw
        .public_address
        .unwrap_or_else(|| sip.listen_addr.ip().to_string());
    NodeConfig { id, public_address }
}

fn compile_media(raw: &RawMedia) -> Result<MediaConfig, CompileError> {
    if let Some((min, max)) = raw.rtp_port_range {
        if min >= max || min % 2 != 0 {
            return Err(CompileError::BadRtpPortRange { min, max });
        }
    }
    Ok(MediaConfig {
        rtp_port_range: raw.rtp_port_range,
    })
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
    if let Some(rate) = raw.audio_sample_rate {
        if rate != 8000 && rate != 16000 {
            return Err(CompileError::BadSampleRate(rate));
        }
    }
    let connect_timeout = raw
        .ws_connect_timeout_ms
        .map(Duration::from_millis)
        .unwrap_or_else(|| Duration::from_secs(5));
    let auth_bearer = raw
        .ws_auth_header
        .as_deref()
        .map(strip_bearer_prefix)
        .filter(|s| !s.is_empty());

    Ok(BridgeDefaults {
        ws_url: raw.ws_url.filter(|s| !s.is_empty()),
        auth_bearer,
        connect_timeout,
        codecs,
        dtmf_payload_type,
        forward_headers: raw.forward_headers.unwrap_or_default(),
    })
}

fn parse_codecs(names: &[String]) -> Result<Vec<Codec>, CompileError> {
    let mut out = Vec::with_capacity(names.len());
    for name in names {
        let codec = Codec::from_encoding_name(name)
            .ok_or_else(|| CompileError::UnknownCodec(name.clone()))?;
        if !out.contains(&codec) {
            out.push(codec);
        }
    }
    Ok(out)
}

fn default_codecs() -> Vec<Codec> {
    vec![Codec::Pcmu, Codec::Pcma]
}

fn strip_bearer_prefix(value: &str) -> String {
    const PREFIX: &str = "Bearer ";
    let trimmed = value.trim();
    if trimmed.len() >= PREFIX.len() && trimmed[..PREFIX.len()].eq_ignore_ascii_case(PREFIX) {
        trimmed[PREFIX.len()..].trim().to_string()
    } else {
        trimmed.to_string()
    }
}

fn compile_dialplan(routes: Vec<siphon_ai_routes::RawRoute>) -> Result<RouteSet, CompileError> {
    let raw_file = RawRouteFile { routes };
    Ok(compile_routes(raw_file)?)
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
