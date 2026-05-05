//! Pull route-matchable facts out of a sip-core `Request`.
//!
//! The route engine in `siphon-ai-routes` works on a borrowed
//! `CallInfo<'_>` whose fields are all `&str`. The cheapest way to
//! produce that from a parsed SIP message — without inviting
//! lifetime tangles between the caller's owned `Request` and the
//! short-lived `CallInfo` we hand to `RouteSet::find_match` — is to
//! materialize an [`InviteFacts`] that owns every string, then
//! borrow it for the match.
//!
//! Per CLAUDE.md §4.8, we never re-parse SIP here. Header parsing
//! goes through `sip-parse`'s `parse_from_header` /
//! `parse_to_header`; URI parsing rides on `sip-core::Uri::as_sip`.
//! If a field is missing or malformed, it surfaces as the empty
//! string — the matcher treats `""` as a non-match for every
//! literal/regex predicate, which is the right answer (the call
//! falls through to the default route, or 404s if none).

use sip_core::Request;
use sip_parse::{parse_from_header, parse_to_header};
use siphon_ai_routes::{CallInfo, Headers};

/// Owned, route-matchable view of one inbound INVITE.
///
/// Construct with [`InviteFacts::extract`]; consume with
/// [`InviteFacts::as_call_info`] when you need the borrowed shape
/// the matcher takes.
#[derive(Debug, Clone)]
pub struct InviteFacts {
    pub request_uri_user: String,
    pub request_uri_host: String,

    pub to_user: String,
    pub to_host: String,

    pub from_user: String,
    pub from_host: String,

    /// Every header on the INVITE, lowercased on the name. The
    /// matcher does case-insensitive name lookup so we don't have
    /// to filter to a known set up-front; whatever the route
    /// references will be there.
    pub headers: Headers,
}

impl InviteFacts {
    /// Pull every field a route's `[match]` block can predicate on
    /// out of `request`. Missing or malformed pieces become empty
    /// strings; we don't fail the call here.
    pub fn extract(request: &Request) -> Self {
        let r_uri = request.uri().as_sip();
        let request_uri_user = r_uri.and_then(|u| u.user()).unwrap_or_default().to_string();
        let request_uri_host = r_uri.map(|u| u.host()).unwrap_or_default().to_string();

        let (from_user, from_host) = parse_user_host(request, "From", AddrKind::From);
        let (to_user, to_host) = parse_user_host(request, "To", AddrKind::To);

        let mut headers = Headers::new();
        for header in request.headers().iter() {
            // `Headers::insert` lowercases the name and stores the
            // last write — sip-core preserves order, so a header
            // that legitimately appears multiple times (e.g.,
            // `Via`) collapses to its last value. The matcher only
            // ever predicates on application-level headers
            // (`X-*`); SIP routing headers don't make sense as
            // dialplan keys.
            headers.insert(header.name(), header.value());
        }

        Self {
            request_uri_user,
            request_uri_host,
            to_user,
            to_host,
            from_user,
            from_host,
            headers,
        }
    }

    /// Borrow self as a `CallInfo<'_>` ready for
    /// [`siphon_ai_routes::RouteSet::find_match`].
    ///
    /// `register_source` is the `name` of the `[[register]]` block
    /// the call arrived through, or `"trunk"` for unregistered
    /// inbound. The caller (sip-glue's UAS handler, ultimately)
    /// knows which it was.
    pub fn as_call_info<'a>(&'a self, register_source: &'a str) -> CallInfo<'a> {
        CallInfo {
            request_uri_user: &self.request_uri_user,
            request_uri_host: &self.request_uri_host,
            to_user: &self.to_user,
            to_host: &self.to_host,
            from_user: &self.from_user,
            from_host: &self.from_host,
            register_source,
            headers: &self.headers,
        }
    }
}

#[derive(Copy, Clone)]
enum AddrKind {
    From,
    To,
}

fn parse_user_host(request: &Request, name: &str, kind: AddrKind) -> (String, String) {
    let Some(value) = request.headers().get_smol(name) else {
        return (String::new(), String::new());
    };
    let parsed = match kind {
        AddrKind::From => parse_from_header(value).map(|h| h.inner().clone()),
        AddrKind::To => parse_to_header(value).map(|h| h.inner().clone()),
    };
    let Some(addr) = parsed else {
        return (String::new(), String::new());
    };
    let Some(uri) = addr.sip_uri() else {
        return (String::new(), String::new());
    };
    (
        uri.user().unwrap_or_default().to_string(),
        uri.host().to_string(),
    )
}
