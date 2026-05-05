//! End-to-end: synthesize an INVITE, route it.
//!
//! Builds `sip_core::Request` directly from `RequestLine` +
//! `Headers` + body (no parsing) so tests don't depend on any
//! specific serialized SIP wire format. The matcher cares only
//! about the parsed fields, which is exactly what these
//! constructors expose.

use bytes::Bytes;
use sip_core::{Headers as SipHeaders, Method, Request, RequestLine, SipUri};
use siphon_ai_routes::load_from_toml;
use siphon_ai_sip_glue::{route_invite, InviteFacts, RouteDecision};

/// Test fixture: build a synthetic INVITE with the given URIs and
/// optional X-* headers.
struct InviteBuilder {
    request_uri: String,
    from: String,
    to: String,
    extra: Vec<(String, String)>,
}

impl InviteBuilder {
    fn new(req_uri: &str, from: &str, to: &str) -> Self {
        Self {
            request_uri: req_uri.into(),
            from: from.into(),
            to: to.into(),
            extra: Vec::new(),
        }
    }

    fn header(mut self, name: &str, value: &str) -> Self {
        self.extra.push((name.into(), value.into()));
        self
    }

    fn build(self) -> Request {
        let uri = SipUri::parse(&self.request_uri).expect("request uri");
        let line = RequestLine::new(Method::Invite, uri);

        let mut headers = SipHeaders::new();
        headers.push("Via", "SIP/2.0/UDP 10.0.0.1:5060").unwrap();
        headers.push("From", self.from.as_str()).unwrap();
        headers.push("To", self.to.as_str()).unwrap();
        headers.push("Call-ID", "call-1@example.net").unwrap();
        headers.push("CSeq", "1 INVITE").unwrap();
        for (n, v) in self.extra {
            headers.push(n.as_str(), v.as_str()).unwrap();
        }
        headers.push("Content-Length", "0").unwrap();

        Request::new(line, headers, Bytes::new()).expect("valid request")
    }
}

#[test]
fn extracts_request_uri_from_to() {
    let req = InviteBuilder::new(
        "sip:5000@siphon.example.com",
        "<sip:+13125551234@carrier.example.net>;tag=abc123",
        "<sip:5000@siphon.example.com>",
    )
    .build();

    let facts = InviteFacts::extract(&req);
    assert_eq!(facts.request_uri_user, "5000");
    assert_eq!(facts.request_uri_host, "siphon.example.com");
    assert_eq!(facts.from_user, "+13125551234");
    assert_eq!(facts.from_host, "carrier.example.net");
    assert_eq!(facts.to_user, "5000");
    assert_eq!(facts.to_host, "siphon.example.com");
}

#[test]
fn extracts_custom_headers_lowercase() {
    let req = InviteBuilder::new(
        "sip:5000@siphon.example.com",
        "<sip:caller@example.net>",
        "<sip:5000@siphon.example.com>",
    )
    .header("X-Customer-Id", "cust-42")
    .header("X-Tenant", "acme")
    .build();

    let facts = InviteFacts::extract(&req);
    assert_eq!(facts.headers.get("x-customer-id"), Some("cust-42"));
    assert_eq!(facts.headers.get("X-Customer-Id"), Some("cust-42"));
    assert_eq!(facts.headers.get("x-tenant"), Some("acme"));
}

#[test]
fn missing_to_yields_empty_strings_not_panic() {
    let uri = SipUri::parse("sip:5000@siphon.example.com").unwrap();
    let line = RequestLine::new(Method::Invite, uri);
    let mut headers = SipHeaders::new();
    headers.push("Via", "SIP/2.0/UDP 10.0.0.1:5060").unwrap();
    headers.push("From", "<sip:caller@x.example>").unwrap();
    // No To header.
    headers.push("Call-ID", "c@x").unwrap();
    headers.push("CSeq", "1 INVITE").unwrap();
    headers.push("Content-Length", "0").unwrap();
    let req = Request::new(line, headers, Bytes::new()).unwrap();

    let facts = InviteFacts::extract(&req);
    assert_eq!(facts.to_user, "");
    assert_eq!(facts.to_host, "");
    assert_eq!(facts.from_user, "caller");
}

#[test]
fn matches_route_by_request_uri_user() {
    let routes = load_from_toml(
        r#"
        [[route]]
        name = "reception"
        [route.match]
        request_uri_user = "5000"

        [[route]]
        name = "default"
        [route.match]
        any = true
    "#,
    )
    .unwrap();

    let req = InviteBuilder::new(
        "sip:5000@siphon.example.com",
        "<sip:caller@example.net>",
        "<sip:5000@siphon.example.com>",
    )
    .build();

    match route_invite(&req, "trunk", &routes) {
        RouteDecision::Matched { route, .. } => assert_eq!(route.name, "reception"),
        RouteDecision::NoMatch { .. } => panic!("expected reception match"),
    }
}

#[test]
fn falls_through_to_default() {
    let routes = load_from_toml(
        r#"
        [[route]]
        name = "reception"
        [route.match]
        request_uri_user = "5000"

        [[route]]
        name = "default"
        [route.match]
        any = true
    "#,
    )
    .unwrap();

    let req = InviteBuilder::new(
        "sip:9999@siphon.example.com",
        "<sip:caller@example.net>",
        "<sip:9999@siphon.example.com>",
    )
    .build();

    match route_invite(&req, "trunk", &routes) {
        RouteDecision::Matched { route, .. } => assert_eq!(route.name, "default"),
        RouteDecision::NoMatch { .. } => panic!("expected default match"),
    }
}

#[test]
fn no_match_returns_nomatch_when_no_default() {
    let routes = load_from_toml(
        r#"
        [[route]]
        name = "reception"
        [route.match]
        request_uri_user = "5000"
    "#,
    )
    .unwrap();

    let req = InviteBuilder::new(
        "sip:9999@siphon.example.com",
        "<sip:caller@example.net>",
        "<sip:9999@siphon.example.com>",
    )
    .build();

    assert!(matches!(
        route_invite(&req, "trunk", &routes),
        RouteDecision::NoMatch { .. }
    ));
}

#[test]
fn register_source_distinguishes_routes() {
    let routes = load_from_toml(
        r#"
        [[route]]
        name = "from-cucm"
        [route.match]
        register_source = "cucm-main"

        [[route]]
        name = "from-trunk"
        [route.match]
        register_source = "trunk"
    "#,
    )
    .unwrap();

    let req = InviteBuilder::new(
        "sip:5000@siphon.example.com",
        "<sip:caller@example.net>",
        "<sip:5000@siphon.example.com>",
    )
    .build();

    match route_invite(&req, "cucm-main", &routes) {
        RouteDecision::Matched { route, .. } => assert_eq!(route.name, "from-cucm"),
        _ => panic!("expected from-cucm match"),
    }
    match route_invite(&req, "trunk", &routes) {
        RouteDecision::Matched { route, .. } => assert_eq!(route.name, "from-trunk"),
        _ => panic!("expected from-trunk match"),
    }
}

#[test]
fn header_match_with_regex() {
    let routes = load_from_toml(
        r#"
        [[route]]
        name = "tenant-acme"
        [route.match]
        regex = true
        [route.match.header]
        X-Tenant-Id = "^acme$"

        [[route]]
        name = "default"
        [route.match]
        any = true
    "#,
    )
    .unwrap();

    let req = InviteBuilder::new(
        "sip:5000@siphon.example.com",
        "<sip:caller@example.net>",
        "<sip:5000@siphon.example.com>",
    )
    .header("X-Tenant-Id", "acme")
    .build();

    match route_invite(&req, "trunk", &routes) {
        RouteDecision::Matched { route, .. } => assert_eq!(route.name, "tenant-acme"),
        _ => panic!("expected tenant-acme match"),
    }

    let req_other = InviteBuilder::new(
        "sip:5000@siphon.example.com",
        "<sip:caller@example.net>",
        "<sip:5000@siphon.example.com>",
    )
    .header("X-Tenant-Id", "globex")
    .build();

    match route_invite(&req_other, "trunk", &routes) {
        RouteDecision::Matched { route, .. } => assert_eq!(route.name, "default"),
        _ => panic!("expected default match"),
    }
}

#[test]
fn from_user_match_with_regex_anchors() {
    let routes = load_from_toml(
        r#"
        [[route]]
        name = "vip"
        [route.match]
        from_user = "+13125551234"

        [[route]]
        name = "default"
        [route.match]
        any = true
    "#,
    )
    .unwrap();

    let req = InviteBuilder::new(
        "sip:5000@siphon.example.com",
        "<sip:+13125551234@carrier.example.net>;tag=zzz",
        "<sip:5000@siphon.example.com>",
    )
    .build();

    match route_invite(&req, "trunk", &routes) {
        RouteDecision::Matched { route, .. } => assert_eq!(route.name, "vip"),
        _ => panic!("expected vip match"),
    }
}
