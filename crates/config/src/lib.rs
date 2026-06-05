//! TOML configuration loading, validation, and compilation.
//!
//! See `docs/CONFIG.md` for the schema and `docs/DEV_PLAN.md` §6 for
//! design notes. This v1 cut covers the fields the layers we've
//! built so far actually consume — `[node]`, `[sip]`, `[media]`,
//! `[bridge]`, and `[[route]]`. Out-of-scope sections
//! (`[[register]]`, `[hep]`, `[cdr]`, `[webhooks]`,
//! `[observability]`, `[security]`) are accepted-and-ignored on
//! load so a real-world TOML file stays valid as follow-up PRs land
//! the rest.
//!
//! ## Pipeline
//!
//! ```text
//!   TOML on disk ─► env expansion (${VAR}) ─► serde ─► RawConfig
//!                                                          │
//!                                                          ▼
//!                                                       compile()
//!                                                          │
//!                                                          ▼
//!                                                        Config
//!                                  (BridgeDefaults + RouteSet + …)
//! ```
//!
//! Validation runs at [`compile`] time, not first-use (CLAUDE.md
//! §4.6). Reload (`SIGHUP`) is post-v1.

pub mod compile;
pub mod env;
pub mod raw;

use std::path::Path;

use thiserror::Error;

pub use compile::{
    compile, CdrConfig, CdrFileConfig, CdrWebhookConfig, CompileError, Config, HepConfig,
    MediaConfig, NodeConfig, ObservabilityConfig, RegisterConfig, SecurityConfig, SipConfig,
    SipTlsConfig, SipTransport, TrunkCidr, TrunkCidrParseError, TrunkConfig, WebhooksConfig,
};
pub use env::{expand, expand_cow, EnvError, EnvSource, ProcessEnv};
pub use raw::{
    RawBridge, RawCdr, RawCdrFile, RawCdrWebhook, RawConfig, RawHep, RawMedia, RawNode,
    RawObservability, RawRegister, RawSip, RawSipTls, RawTrunk, RawWebhooks,
};

/// Top-level error type. Loaders surface this; consumers match on
/// the underlying variants when they need to discriminate.
#[derive(Debug, Error)]
pub enum LoadError {
    #[error("failed to read config file {path:?}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },

    #[error(transparent)]
    Env(#[from] EnvError),

    #[error("invalid TOML: {0}")]
    Toml(toml::de::Error),

    #[error(transparent)]
    Compile(#[from] CompileError),
}

/// Load and compile a config file from disk.
///
/// Env var expansion uses [`ProcessEnv`]; tests should prefer
/// [`load_from_str_with_env`] to keep the global environment
/// untouched.
pub fn load_from_path(path: impl AsRef<Path>) -> Result<Config, LoadError> {
    let p = path.as_ref();
    let bytes = std::fs::read_to_string(p).map_err(|source| LoadError::Io {
        path: p.display().to_string(),
        source,
    })?;
    load_from_str(&bytes)
}

/// Load and compile a config from an in-memory TOML string. Uses
/// the process environment for `${VAR}` lookups.
pub fn load_from_str(input: &str) -> Result<Config, LoadError> {
    load_from_str_with_env(input, &ProcessEnv)
}

/// Load and compile, with a pluggable env source. Used by tests
/// that need deterministic env lookups.
pub fn load_from_str_with_env<E: EnvSource>(input: &str, env: &E) -> Result<Config, LoadError> {
    let expanded = expand(input, env)?;
    let raw: RawConfig = toml::from_str(&expanded).map_err(LoadError::Toml)?;
    Ok(compile(raw)?)
}
