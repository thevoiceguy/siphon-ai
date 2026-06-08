//! Compiled, ready-to-evaluate route set.
//!
//! `RouteSet` is the matcher's runtime input. It's produced by
//! `crate::compile` from a `RawRouteFile`: regexes are compiled,
//! `any = true` is canonicalized, header names are lowercased,
//! and ordering is preserved verbatim from the file.
//!
//! Once compiled, a `RouteSet` is `Send + Sync` and cheap to share
//! across the per-call tasks via `Arc`.

use regex::Regex;

use crate::raw::{BridgeOverride, MediaOverride, RecordingOverride, SecurityOverride};

/// One pre-compiled match predicate (string or regex). Stored per
/// match key so a route can mix string keys (most common) with
/// header matching.
#[derive(Debug, Clone)]
pub(crate) enum Matcher {
    /// Case-insensitive exact match. The stored value is already
    /// lowercased; the matcher lowercases the candidate before
    /// comparing.
    Literal(String),

    /// Compiled Rust regex. Anchoring is the user's responsibility
    /// — `"^foo$"` for whole-string match, `"foo"` for substring.
    /// This mirrors what users expect from regex tools.
    Regex(Regex),
}

impl Matcher {
    /// True iff `candidate` matches this predicate.
    ///
    /// Empty candidate strings never match a literal or regex
    /// (a regex like `^$` would, but that's the user's choice).
    pub(crate) fn matches(&self, candidate: &str) -> bool {
        match self {
            Matcher::Literal(want) => {
                // Both sides are case-insensitive. `want` is already
                // lowercase; lowercase the candidate cheaply by
                // comparing byte-by-byte without allocating when
                // possible.
                eq_ignore_ascii_case(want, candidate)
            }
            Matcher::Regex(re) => re.is_match(candidate),
        }
    }
}

fn eq_ignore_ascii_case(lowercase_want: &str, candidate: &str) -> bool {
    if lowercase_want.len() != candidate.len() {
        return false;
    }
    lowercase_want
        .bytes()
        .zip(candidate.bytes())
        .all(|(w, c)| w == c.to_ascii_lowercase())
}

/// Compiled match block for one route.
#[derive(Debug, Clone)]
pub(crate) struct CompiledMatch {
    pub(crate) any: bool,

    pub(crate) request_uri_user: Option<Matcher>,
    pub(crate) request_uri_host: Option<Matcher>,

    pub(crate) to_user: Option<Matcher>,
    pub(crate) to_host: Option<Matcher>,

    pub(crate) from_user: Option<Matcher>,
    pub(crate) from_host: Option<Matcher>,

    pub(crate) register_source: Option<Matcher>,

    /// `(lowercased_header_name, predicate)` pairs.
    pub(crate) headers: Vec<(String, Matcher)>,
}

/// One compiled route — the unit `RouteSet::find_match` returns.
#[derive(Debug, Clone)]
pub struct CompiledRoute {
    pub name: String,
    pub(crate) match_: CompiledMatch,
    pub bridge: BridgeOverride,
    pub media: MediaOverride,
    pub security: SecurityOverride,
    pub recording: RecordingOverride,
}

impl CompiledRoute {
    /// True iff this route's `[match]` block predicates all hold
    /// against `info`. `any = true` short-circuits to `true`.
    pub fn matches(&self, info: &crate::CallInfo<'_>) -> bool {
        let m = &self.match_;
        if m.any {
            return true;
        }
        if let Some(p) = &m.request_uri_user {
            if !p.matches(info.request_uri_user) {
                return false;
            }
        }
        if let Some(p) = &m.request_uri_host {
            if !p.matches(info.request_uri_host) {
                return false;
            }
        }
        if let Some(p) = &m.to_user {
            if !p.matches(info.to_user) {
                return false;
            }
        }
        if let Some(p) = &m.to_host {
            if !p.matches(info.to_host) {
                return false;
            }
        }
        if let Some(p) = &m.from_user {
            if !p.matches(info.from_user) {
                return false;
            }
        }
        if let Some(p) = &m.from_host {
            if !p.matches(info.from_host) {
                return false;
            }
        }
        if let Some(p) = &m.register_source {
            if !p.matches(info.register_source) {
                return false;
            }
        }
        for (name, pred) in &m.headers {
            // Header names are stored lowercase; CallInfo::headers
            // is also lowercase-keyed, so we get an O(1) lookup.
            let candidate = info.headers.get(name).unwrap_or("");
            if !pred.matches(candidate) {
                return false;
            }
        }
        true
    }
}

/// An ordered set of compiled routes. Evaluation is top-down,
/// first-match-wins per CLAUDE.md §4.6.
#[derive(Debug, Clone, Default)]
pub struct RouteSet {
    routes: Vec<CompiledRoute>,
}

impl RouteSet {
    pub(crate) fn from_routes(routes: Vec<CompiledRoute>) -> Self {
        Self { routes }
    }

    /// Walk the route table in order and return the first route
    /// whose match block holds against `info`. Returns `None` if
    /// nothing matches — the caller is responsible for emitting SIP
    /// 404 in that case (see `docs/DEV_PLAN.md` §6.3).
    pub fn find_match(&self, info: &crate::CallInfo<'_>) -> Option<&CompiledRoute> {
        self.routes.iter().find(|r| r.matches(info))
    }

    /// True iff the *last* route is an unconditional match. The
    /// daemon logs a startup warning when this is false in a
    /// production config (CLAUDE.md §4.6).
    pub fn has_default(&self) -> bool {
        self.routes.last().map(|r| r.match_.any).unwrap_or(false)
    }

    pub fn len(&self) -> usize {
        self.routes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.routes.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &CompiledRoute> {
        self.routes.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_matcher_is_case_insensitive() {
        let m = Matcher::Literal("alice".to_string());
        assert!(m.matches("ALICE"));
        assert!(m.matches("Alice"));
        assert!(m.matches("alice"));
        assert!(!m.matches("alic"));
        assert!(!m.matches("alicee"));
        assert!(!m.matches(""));
    }

    #[test]
    fn regex_matcher_unanchored_is_substring() {
        let m = Matcher::Regex(Regex::new("sales").unwrap());
        assert!(m.matches("sales-42"));
        assert!(m.matches("global-sales"));
        assert!(!m.matches("billing"));
    }
}
