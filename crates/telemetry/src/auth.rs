//! Admin API authentication + role-based authorization.
//!
//! The `/admin/*` surface can hang up calls, originate **billable**
//! outbound calls, manage conferences, retrieve parked calls, and change
//! log filters. This module is the daemon-native gate for it
//! (`docs/design/DESIGN_ADMIN_AUTH.md`): a bearer token in
//! `Authorization: Bearer <token>` is hashed (SHA-256) and compared in
//! **constant time** against the configured tokens; each token carries
//! one [`Role`], and every endpoint declares a minimum role.
//!
//! This chunk is the auth *core* — the types, the token compare, the
//! endpoint→role table, and [`AdminAuth::authorize`]. Standing up the
//! separate admin listener and calling `authorize` from it is the next
//! chunk; the logic here is unit-tested in isolation.

use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

/// Admin authorization role. Nested: `ReadOnly` < `Operator` < `Admin`
/// (the derived `Ord` follows declaration order), so an endpoint's
/// minimum role is satisfied by any role `>=` it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Role {
    /// Read-only: `GET` / list endpoints. Cannot change call or daemon
    /// state.
    ReadOnly,
    /// Live-call control: hangup, park, retrieve, conference CRUD +
    /// participant management. Everything `ReadOnly` can do, plus
    /// operating on in-flight calls.
    Operator,
    /// Everything `Operator` can do, plus the dangerous / billable /
    /// observability-blinding actions: originate outbound calls, change
    /// the runtime log filter, emit HEP probes.
    Admin,
}

impl Role {
    /// Canonical lowercase wire string (config value + audit label).
    pub fn as_str(self) -> &'static str {
        match self {
            Role::ReadOnly => "readonly",
            Role::Operator => "operator",
            Role::Admin => "admin",
        }
    }

    /// Parse a config `role` string. Case-sensitive (the modes are
    /// documented lowercase); unknown strings are rejected so config
    /// load can fail loud (CLAUDE.md §4.6).
    pub fn parse(s: &str) -> Option<Role> {
        match s {
            "readonly" => Some(Role::ReadOnly),
            "operator" => Some(Role::Operator),
            "admin" => Some(Role::Admin),
            _ => None,
        }
    }
}

/// One configured admin token: a label (for audit logs — never the
/// secret), the SHA-256 of the secret, and the role it grants. The
/// plaintext token is hashed at construction and never retained.
#[derive(Debug, Clone)]
pub struct AdminToken {
    pub name: String,
    token_sha256: [u8; 32],
    pub role: Role,
}

impl AdminToken {
    /// Hash `token` and build the entry. The plaintext is consumed and
    /// not stored.
    pub fn new(name: impl Into<String>, token: &str, role: Role) -> Self {
        Self {
            name: name.into(),
            token_sha256: sha256(token),
            role,
        }
    }
}

/// The daemon's admin token table. Built from `[admin]` config and held
/// for the process lifetime; consulted on every `/admin/*` request.
#[derive(Debug, Clone, Default)]
pub struct AdminAuth {
    tokens: Vec<AdminToken>,
}

impl AdminAuth {
    pub fn new(tokens: Vec<AdminToken>) -> Self {
        Self { tokens }
    }

    pub fn is_empty(&self) -> bool {
        self.tokens.is_empty()
    }

    /// Number of configured tokens.
    pub fn len(&self) -> usize {
        self.tokens.len()
    }

    /// Iterate the configured tokens (name + role; the hashed secret is
    /// never exposed). Used by `siphon-ai print-config` to list the
    /// admin tokens without revealing any secret material.
    pub fn iter(&self) -> impl Iterator<Item = &AdminToken> {
        self.tokens.iter()
    }

    /// Match a presented bearer token against the table in constant time.
    /// Returns the matching entry (with its role), or `None` for no
    /// match. We hash the candidate once and `ct_eq` every stored hash
    /// (no early return on the first mismatch) to avoid leaking which —
    /// or how many — tokens exist via timing.
    pub fn authenticate(&self, presented: &str) -> Option<&AdminToken> {
        let candidate = sha256(presented);
        let mut matched: Option<&AdminToken> = None;
        for token in &self.tokens {
            // ct_eq returns a Choice; fold without branching on it.
            if bool::from(candidate.ct_eq(&token.token_sha256)) {
                matched = Some(token);
            }
        }
        matched
    }

    /// Authorize an admin request: authenticate the bearer token, then
    /// check its role against the endpoint's minimum. `auth_header` is
    /// the raw `Authorization` header value (`None` if absent).
    pub fn authorize(
        &self,
        method: &hyper::Method,
        path: &str,
        auth_header: Option<&str>,
    ) -> Result<&AdminToken, AuthReject> {
        let presented = auth_header
            .and_then(bearer_token)
            .ok_or(AuthReject::Unauthenticated)?;
        let token = self
            .authenticate(presented)
            .ok_or(AuthReject::Unauthenticated)?;
        // An unknown admin path has no min role; let dispatch 404 it —
        // but only after the caller authenticated (don't leak the route
        // map to anonymous callers). We treat "unknown path" as needing
        // at least ReadOnly so a valid token reaches dispatch's 404.
        let required = min_role(method, path).unwrap_or(Role::ReadOnly);
        if token.role >= required {
            Ok(token)
        } else {
            Err(AuthReject::Forbidden {
                required,
                have: token.role,
            })
        }
    }
}

/// Why an admin request was rejected. `Unauthenticated` → `401`,
/// `Forbidden` → `403` (the listener maps these in the next chunk).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthReject {
    /// No bearer token, or it matched no configured token.
    Unauthenticated,
    /// Authenticated, but the token's role is below the endpoint's
    /// minimum.
    Forbidden { required: Role, have: Role },
}

/// Extract the token from an `Authorization: Bearer <token>` header
/// value. Case-insensitive on the `Bearer` scheme; `None` for any other
/// shape (Basic, missing scheme, empty token).
fn bearer_token(header: &str) -> Option<&str> {
    let (scheme, token) = header.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let token = token.trim();
    (!token.is_empty()).then_some(token)
}

/// SHA-256 of a string, as a fixed 32-byte array.
fn sha256(s: &str) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(s.as_bytes());
    hasher.finalize().into()
}

/// The minimum [`Role`] required for a known admin endpoint, or `None`
/// if `(method, path)` is not a recognised admin route. **Must mirror
/// the routes in `admin::dispatch`.**
///
/// - `ReadOnly`: every `GET` / list.
/// - `Operator`: live-call control (hangup, park, retrieve, conference
///   create/end + participant add/remove).
/// - `Admin`: originate (billable), log-filter change, HEP probe.
pub fn min_role(method: &hyper::Method, path: &str) -> Option<Role> {
    use hyper::Method;
    match (method, path) {
        // ── ReadOnly ──
        (&Method::GET, "/admin/calls")
        | (&Method::GET, "/admin/registrations")
        | (&Method::GET, "/admin/log")
        | (&Method::GET, "/admin/v1/conferences")
        | (&Method::GET, "/admin/v1/parked") => Some(Role::ReadOnly),

        // ── Admin (billable / config / observability-blinding) ──
        (&Method::PUT, "/admin/log")
        | (&Method::POST, "/admin/hep/test")
        | (&Method::POST, "/admin/v1/calls") => Some(Role::Admin),

        // ── Operator (live-call control) ──
        (&Method::POST, "/admin/v1/conferences") => Some(Role::Operator),
        // ── ReadOnly (parameterised) ──
        // /admin/v1/calls/:id/stats — live quality probe (0.31.0).
        (m, p)
            if *m == Method::GET && p.starts_with("/admin/v1/calls/") && p.ends_with("/stats") =>
        {
            Some(Role::ReadOnly)
        }
        (m, p) => operator_pattern(m, p).then_some(Role::Operator),
    }
}

/// Parameterised Operator routes that `min_role`'s literal arms can't
/// match (ids in the path). Mirrors the tail of `admin::dispatch`.
fn operator_pattern(method: &hyper::Method, path: &str) -> bool {
    use hyper::Method;
    // /admin/calls/:id/hangup
    (*method == Method::POST && path.starts_with("/admin/calls/") && path.ends_with("/hangup"))
        // /admin/v1/calls/:id/park
        || (*method == Method::POST
            && path.starts_with("/admin/v1/calls/")
            && path.ends_with("/park"))
        // /admin/v1/calls/:id/retrieve
        || (*method == Method::POST
            && path.starts_with("/admin/v1/calls/")
            && path.ends_with("/retrieve"))
        // /admin/v1/conferences/:id  (DELETE = force-end)
        // /admin/v1/conferences/:id/participants  (POST = add)
        // /admin/v1/conferences/:id/participants/:cid  (DELETE = remove)
        || ((*method == Method::DELETE || *method == Method::POST)
            && path.starts_with("/admin/v1/conferences/"))
}

/// A **bounded-cardinality** label for the admin-request metric: the
/// route template (ids collapsed to `:id`), `"unknown"` for an
/// unrecognised path. Never the raw path (which carries call/room ids).
pub fn route_label(method: &hyper::Method, path: &str) -> &'static str {
    use hyper::Method;
    match (method, path) {
        (&Method::GET, "/admin/calls") => "GET /admin/calls",
        (&Method::GET, "/admin/registrations") => "GET /admin/registrations",
        (&Method::GET, "/admin/log") => "GET /admin/log",
        (&Method::PUT, "/admin/log") => "PUT /admin/log",
        (&Method::POST, "/admin/hep/test") => "POST /admin/hep/test",
        (&Method::POST, "/admin/v1/calls") => "POST /admin/v1/calls",
        (&Method::GET, "/admin/v1/conferences") => "GET /admin/v1/conferences",
        (&Method::POST, "/admin/v1/conferences") => "POST /admin/v1/conferences",
        (&Method::GET, "/admin/v1/parked") => "GET /admin/v1/parked",
        (m, p)
            if *m == Method::POST && p.starts_with("/admin/calls/") && p.ends_with("/hangup") =>
        {
            "POST /admin/calls/:id/hangup"
        }
        (m, p)
            if *m == Method::GET && p.starts_with("/admin/v1/calls/") && p.ends_with("/stats") =>
        {
            "GET /admin/v1/calls/:id/stats"
        }
        (m, p)
            if *m == Method::POST && p.starts_with("/admin/v1/calls/") && p.ends_with("/park") =>
        {
            "POST /admin/v1/calls/:id/park"
        }
        (m, p)
            if *m == Method::POST
                && p.starts_with("/admin/v1/calls/")
                && p.ends_with("/retrieve") =>
        {
            "POST /admin/v1/calls/:id/retrieve"
        }
        (&Method::DELETE, p)
            if p.starts_with("/admin/v1/conferences/") && p.contains("/participants/") =>
        {
            "DELETE /admin/v1/conferences/:id/participants/:cid"
        }
        (&Method::POST, p)
            if p.starts_with("/admin/v1/conferences/") && p.ends_with("/participants") =>
        {
            "POST /admin/v1/conferences/:id/participants"
        }
        (&Method::DELETE, p) if p.starts_with("/admin/v1/conferences/") => {
            "DELETE /admin/v1/conferences/:id"
        }
        _ => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hyper::Method;

    #[test]
    fn roles_are_ordered_readonly_lt_operator_lt_admin() {
        assert!(Role::ReadOnly < Role::Operator);
        assert!(Role::Operator < Role::Admin);
        assert!(Role::Admin >= Role::Operator);
        assert!(Role::Admin >= Role::ReadOnly);
    }

    #[test]
    fn role_parse_round_trips_and_rejects_unknown() {
        for r in [Role::ReadOnly, Role::Operator, Role::Admin] {
            assert_eq!(Role::parse(r.as_str()), Some(r));
        }
        assert_eq!(Role::parse("Admin"), None); // case-sensitive
        assert_eq!(Role::parse("root"), None);
        assert_eq!(Role::parse(""), None);
    }

    #[test]
    fn bearer_extraction() {
        assert_eq!(bearer_token("Bearer abc123"), Some("abc123"));
        assert_eq!(bearer_token("bearer abc123"), Some("abc123")); // scheme ci
        assert_eq!(bearer_token("Bearer   spaced  "), Some("spaced"));
        assert_eq!(bearer_token("Basic abc123"), None);
        assert_eq!(bearer_token("abc123"), None);
        assert_eq!(bearer_token("Bearer "), None);
    }

    fn auth() -> AdminAuth {
        AdminAuth::new(vec![
            AdminToken::new("ro", "ro-secret", Role::ReadOnly),
            AdminToken::new("op", "op-secret", Role::Operator),
            AdminToken::new("ad", "ad-secret", Role::Admin),
        ])
    }

    #[test]
    fn authenticate_matches_only_the_right_token() {
        let a = auth();
        assert_eq!(
            a.authenticate("op-secret").map(|t| t.role),
            Some(Role::Operator)
        );
        assert_eq!(a.authenticate("ad-secret").map(|t| &*t.name), Some("ad"));
        assert!(a.authenticate("nope").is_none());
        assert!(a.authenticate("").is_none());
    }

    #[test]
    fn authorize_401_without_or_with_bad_token() {
        let a = auth();
        assert_eq!(
            a.authorize(&Method::GET, "/admin/calls", None).unwrap_err(),
            AuthReject::Unauthenticated
        );
        assert_eq!(
            a.authorize(&Method::GET, "/admin/calls", Some("Bearer wrong"))
                .unwrap_err(),
            AuthReject::Unauthenticated
        );
    }

    #[test]
    fn authorize_enforces_minimum_role() {
        let a = auth();
        // readonly token: GET ok, hangup forbidden, originate forbidden.
        assert!(a
            .authorize(&Method::GET, "/admin/calls", Some("Bearer ro-secret"))
            .is_ok());
        assert_eq!(
            a.authorize(
                &Method::POST,
                "/admin/calls/x/hangup",
                Some("Bearer ro-secret")
            )
            .unwrap_err(),
            AuthReject::Forbidden {
                required: Role::Operator,
                have: Role::ReadOnly
            }
        );
        assert_eq!(
            a.authorize(&Method::POST, "/admin/v1/calls", Some("Bearer ro-secret"))
                .unwrap_err(),
            AuthReject::Forbidden {
                required: Role::Admin,
                have: Role::ReadOnly
            }
        );
        // operator token: hangup ok, originate (Admin) forbidden.
        assert!(a
            .authorize(
                &Method::POST,
                "/admin/calls/x/hangup",
                Some("Bearer op-secret")
            )
            .is_ok());
        assert_eq!(
            a.authorize(&Method::POST, "/admin/v1/calls", Some("Bearer op-secret"))
                .unwrap_err(),
            AuthReject::Forbidden {
                required: Role::Admin,
                have: Role::Operator
            }
        );
        // admin token: originate + log change ok.
        assert!(a
            .authorize(&Method::POST, "/admin/v1/calls", Some("Bearer ad-secret"))
            .is_ok());
        assert!(a
            .authorize(&Method::PUT, "/admin/log", Some("Bearer ad-secret"))
            .is_ok());
    }

    #[test]
    fn min_role_table_matches_endpoint_groups() {
        assert_eq!(
            min_role(&Method::GET, "/admin/v1/parked"),
            Some(Role::ReadOnly)
        );
        assert_eq!(
            min_role(&Method::POST, "/admin/v1/calls"),
            Some(Role::Admin)
        );
        assert_eq!(min_role(&Method::PUT, "/admin/log"), Some(Role::Admin));
        assert_eq!(
            min_role(&Method::POST, "/admin/calls/abc/hangup"),
            Some(Role::Operator)
        );
        assert_eq!(
            min_role(&Method::POST, "/admin/v1/calls/abc/retrieve"),
            Some(Role::Operator)
        );
        assert_eq!(
            min_role(&Method::DELETE, "/admin/v1/conferences/room1"),
            Some(Role::Operator)
        );
        assert_eq!(
            min_role(
                &Method::DELETE,
                "/admin/v1/conferences/room1/participants/call9"
            ),
            Some(Role::Operator)
        );
        // Unknown admin path → no min role.
        assert_eq!(min_role(&Method::GET, "/admin/nope"), None);
    }
}
