//! Deserialize-only TOML representation of the dialplan.
//!
//! These types mirror the `[[route]]` schema documented in
//! `docs/DIALPLAN.md` and `docs/DEV_PLAN.md` §6.2. They preserve the
//! file's intent verbatim — no defaults that would silently override
//! globals, no normalization. Compilation (regex parsing,
//! cross-route validation) happens in `compile()`.
//!
//! Override blocks (`[route.bridge]`, `[route.media]`) live here so
//! the routes crate can carry the overrides through compilation.
//! The merge against global defaults is the config crate's job;
//! routes only stores the partial Optional fields that a route set.

use std::collections::BTreeMap;

use serde::Deserialize;

/// Top-level wrapper for a TOML file or fragment containing
/// `[[route]]` arrays. The full siphon-ai TOML embeds these via the
/// `route` key (TOML's array-of-tables syntax). Standalone test
/// fixtures use this struct directly.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct RawRouteFile {
    #[serde(default, rename = "route")]
    pub routes: Vec<RawRoute>,
}

/// One `[[route]]` entry.
#[derive(Debug, Clone, Deserialize)]
pub struct RawRoute {
    pub name: String,

    #[serde(rename = "match")]
    pub match_: RawRouteMatch,

    #[serde(default)]
    pub bridge: BridgeOverride,

    #[serde(default)]
    pub media: MediaOverride,

    #[serde(default)]
    pub security: SecurityOverride,

    #[serde(default)]
    pub recording: RecordingOverride,
}

/// `[route.match]` block — the matcher's input grammar.
///
/// All string fields are case-insensitive literals by default.
/// Setting `regex = true` reinterprets every string field in this
/// block as a Rust regex pattern. The flag is scoped per-route per
/// CLAUDE.md §4.6 ("`regex = true` is per-route, not per-match-key").
#[derive(Debug, Clone, Default, Deserialize)]
pub struct RawRouteMatch {
    /// Unconditional match. Mutually exclusive with all other keys.
    /// Required on the trailing default route.
    #[serde(default)]
    pub any: bool,

    /// Treat all string match values in this block as regex patterns.
    #[serde(default)]
    pub regex: bool,

    pub request_uri_user: Option<String>,
    pub request_uri_host: Option<String>,

    pub to_user: Option<String>,
    pub to_host: Option<String>,

    pub from_user: Option<String>,
    pub from_host: Option<String>,

    /// Which `[[register]]` block this call arrived through, or
    /// `"trunk"` for unregistered inbound (UAS-mode) calls.
    pub register_source: Option<String>,

    /// `header.<NAME> = "<value>"` — name is case-insensitive,
    /// matched against the inbound INVITE's headers.
    #[serde(default)]
    pub header: BTreeMap<String, String>,
}

/// `[route.bridge]` overrides. Every field is optional; unset fields
/// inherit from the global `[bridge]` block. The merge happens in
/// the config crate — routes carries the partial.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct BridgeOverride {
    pub ws_url: Option<String>,
    pub ws_auth_header: Option<String>,
    pub audio_direction: Option<String>,
    pub on_ws_failure: Option<String>,
    pub ws_connect_timeout_ms: Option<u64>,

    #[serde(default)]
    pub barge_in: BargeInOverride,

    /// Per-route override of `[bridge].silence_threshold_ms`. Same
    /// shape as the global: `None` = inherit, `Some(0)` = disable,
    /// `Some(n)` = `n` ms.
    pub silence_threshold_ms: Option<u64>,
    /// Per-route override of `[bridge].dead_air_threshold_ms`. Same
    /// shape as `silence_threshold_ms`.
    pub dead_air_threshold_ms: Option<u64>,
    /// Per-route override of `[bridge].rtp_stats_interval_ms`. Same
    /// shape: `None` = inherit, `Some(0)` = disable, `Some(n)` = ms.
    pub rtp_stats_interval_ms: Option<u64>,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct BargeInOverride {
    pub enabled: Option<bool>,
    pub mode: Option<String>,
    pub debounce_ms: Option<u64>,
}

/// `[route.media]` overrides. Same merge rules as `BridgeOverride`.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct MediaOverride {
    pub codecs: Option<Vec<String>>,
    pub dtmf: Option<String>,
    pub inactivity_timeout_secs: Option<u64>,
    pub rtp_port_range: Option<(u16, u16)>,
    /// `[route.media].srtp` — per-route override of the global
    /// `[media].srtp` mode. `None` means "inherit"; any of `"off"`,
    /// `"preferred"`, `"required"` overrides. Validated at config
    /// load via the same path as the global field.
    pub srtp: Option<String>,
}

/// `[route.security]` overrides. Same merge rules as the other override
/// blocks — `None` inherits the global `[security]` value. Kept as the
/// raw string so the routes crate stays free of a dependency on the
/// security-policy types; the config crate validates it and the accept
/// path parses it against the global default.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct SecurityOverride {
    /// `[route.security].min_attestation` — per-route override of the
    /// global `[security].min_attestation` gate. `None` inherits; any of
    /// `"none"`, `"A"`, `"B"`, `"C"` overrides (strict override, matching
    /// `[route.media].srtp` semantics — the route value fully replaces the
    /// global, even when more permissive).
    pub min_attestation: Option<String>,
}

/// `[route.recording]` overrides. Same merge rules — `None` inherits the
/// global `[recording].mode`. Kept as the raw string so the routes crate
/// stays free of a dependency on the recording types; the config crate
/// validates it and the accept path resolves it against the global.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct RecordingOverride {
    /// `[route.recording].mode` — per-route override of the global
    /// `[recording].mode`. `None` inherits; `"off"` / `"always"` /
    /// `"on_demand"` override (strict — fully replaces the global).
    pub mode: Option<String>,
}
