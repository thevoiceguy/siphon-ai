//! Route matching engine: TOML dialplan → matched bridge config.
//!
//! The shape of the dialplan and its evaluation rules are documented
//! in `docs/DIALPLAN.md` and `docs/DEV_PLAN.md` §6.3. The high-level
//! flow:
//!
//! ```text
//!   TOML file ─► serde ─► RawRouteFile
//!                                │
//!                                ▼
//!                       compile() ─► RouteSet
//!                                       │
//!                                       ▼
//!     incoming INVITE ─► CallInfo ─► RouteSet::find_match ─► &CompiledRoute
//! ```
//!
//! ## Cardinal rules (CLAUDE.md §4.6)
//!
//! - **Order matters.** Routes are evaluated top-down; the first
//!   route whose predicates all hold wins. We never reorder routes
//!   for "efficiency" — the file's order *is* the priority.
//! - **AND across keys within a route.** OR is expressed as
//!   multiple routes.
//! - **`regex = true` is per-route**, not per-key. Mixed regex/literal
//!   inside one match block isn't a feature.
//! - **`any = true` is exclusive** of all other keys (compile-time
//!   error otherwise).
//! - **Validation is at compile time, not first-use.** Invalid
//!   regex, conflicting keys, duplicate names → `RouteError`.

mod call_info;
mod compile;
mod compiled;
mod raw;

pub use call_info::{CallInfo, Headers};
pub use compile::{compile, RouteError};
pub use compiled::{CompiledRoute, RouteSet};
pub use raw::{
    BargeInOverride, BridgeOverride, MediaOverride, RawRoute, RawRouteFile, RawRouteMatch,
    SecurityOverride,
};

/// Convenience: parse a TOML string into a [`RawRouteFile`] and
/// compile it in one step. The TOML must contain `[[route]]`
/// arrays-of-tables; anything else is ignored.
///
/// This is for tests and standalone tools — the real daemon parses
/// the full siphon-ai TOML file (in the config crate) and forwards
/// just the routes section here.
pub fn load_from_toml(input: &str) -> Result<RouteSet, LoadError> {
    let raw: RawRouteFile = toml::from_str(input).map_err(LoadError::Toml)?;
    compile(raw).map_err(LoadError::Compile)
}

#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    #[error("invalid TOML: {0}")]
    Toml(toml::de::Error),

    #[error(transparent)]
    Compile(#[from] RouteError),
}
