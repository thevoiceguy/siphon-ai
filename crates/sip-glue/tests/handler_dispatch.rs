//! Tests for `dispatch_invite` — the synchronous routing decision.
//!
//! End-to-end async tests that drive the `UasRequestHandler` impl
//! aren't here because `ServerTransactionHandle` has no public test
//! constructor (and standing up a real `TransactionManager` is more
//! plumbing than this layer warrants). The compile-time check that
//! `RoutingHandler` satisfies `UasRequestHandler` lives as an
//! internal `#[cfg(test)]` module in `handler.rs`.

use bytes::Bytes;
use sip_core::{Headers as SipHeaders, Method, Request, RequestLine, SipUri};
use siphon_ai_routes::load_from_toml;
use siphon_ai_sip_glue::{dispatch_invite, RouteAction};

fn invite(req_uri: &str, from: &str, to: &str) -> Request {
    let uri = SipUri::parse(req_uri).expect("request uri");
    let line = RequestLine::new(Method::Invite, uri);
    let mut headers = SipHeaders::new();
    headers
        .push("Via", "SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-1")
        .unwrap();
    headers.push("From", from).unwrap();
    headers.push("To", to).unwrap();
    headers.push("Call-ID", "call-1@example.net").unwrap();
    headers.push("CSeq", "1 INVITE").unwrap();
    headers.push("Content-Length", "0").unwrap();
    Request::new(line, headers, Bytes::new()).unwrap()
}

#[test]
fn matched_invite_yields_accept_with_route_and_facts() {
    let routes = load_from_toml(
        r#"
        [[route]]
        name = "reception"
        [route.match]
        request_uri_user = "5000"
        [route.bridge]
        ws_url = "wss://reception.example.com/sip-bridge"

        [[route]]
        name = "default"
        [route.match]
        any = true
    "#,
    )
    .unwrap();

    let req = invite(
        "sip:5000@siphon.example.com",
        "<sip:+13125551234@carrier.example.net>;tag=abc",
        "<sip:5000@siphon.example.com>",
    );

    match dispatch_invite(&routes, "trunk", &req) {
        RouteAction::Accept { facts, route } => {
            assert_eq!(route.name, "reception");
            assert_eq!(
                route.bridge.ws_url.as_deref(),
                Some("wss://reception.example.com/sip-bridge")
            );
            assert_eq!(facts.request_uri_user, "5000");
            assert_eq!(facts.from_user, "+13125551234");
        }
        RouteAction::SendFinal(_) => panic!("expected Accept"),
    }
}

#[test]
fn unmatched_invite_yields_404_with_request_correlation_headers() {
    // No default route — anything that doesn't match 5000 falls
    // through to 404.
    let routes = load_from_toml(
        r#"
        [[route]]
        name = "reception"
        [route.match]
        request_uri_user = "5000"
    "#,
    )
    .unwrap();

    let req = invite(
        "sip:9999@siphon.example.com",
        "<sip:caller@example.net>;tag=xyz",
        "<sip:9999@siphon.example.com>",
    );

    match dispatch_invite(&routes, "trunk", &req) {
        RouteAction::SendFinal(response) => {
            assert_eq!(response.code(), 404);
            assert_eq!(response.reason(), "Not Found");
            // RFC 3261 §8.2.6.1: response carries Via, From, To,
            // Call-ID, CSeq from the request.
            let h = response.headers();
            assert!(h.get("Via").is_some(), "Via must be copied");
            assert_eq!(h.get("Call-ID"), Some("call-1@example.net"));
            assert_eq!(h.get("CSeq"), Some("1 INVITE"));
        }
        RouteAction::Accept { .. } => panic!("expected SendFinal(404)"),
    }
}

#[test]
fn matched_route_with_default_fallback_picks_specific_first() {
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

    // Specific match.
    let req = invite(
        "sip:5000@siphon.example.com",
        "<sip:caller@example.net>",
        "<sip:5000@siphon.example.com>",
    );
    match dispatch_invite(&routes, "trunk", &req) {
        RouteAction::Accept { route, .. } => assert_eq!(route.name, "reception"),
        _ => panic!("expected Accept reception"),
    }

    // Falls through to default.
    let req = invite(
        "sip:1234@siphon.example.com",
        "<sip:caller@example.net>",
        "<sip:1234@siphon.example.com>",
    );
    match dispatch_invite(&routes, "trunk", &req) {
        RouteAction::Accept { route, .. } => assert_eq!(route.name, "default"),
        _ => panic!("expected Accept default"),
    }
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

    let req = invite(
        "sip:5000@siphon.example.com",
        "<sip:caller@example.net>",
        "<sip:5000@siphon.example.com>",
    );

    match dispatch_invite(&routes, "cucm-main", &req) {
        RouteAction::Accept { route, .. } => assert_eq!(route.name, "from-cucm"),
        _ => panic!("expected Accept from-cucm"),
    }
    match dispatch_invite(&routes, "trunk", &req) {
        RouteAction::Accept { route, .. } => assert_eq!(route.name, "from-trunk"),
        _ => panic!("expected Accept from-trunk"),
    }
}

#[test]
fn dispatch_returned_route_outlives_register_source_string() {
    // Compile-time verification: the matched route reference's
    // lifetime is tied to `routes`, not `register_source`.
    // A short-lived String for register_source must not constrain
    // how long we can hold the Accept'd route. Drop register_source
    // before reading route.name.
    let routes = load_from_toml(
        r#"
        [[route]]
        name = "any-call"
        [route.match]
        any = true
    "#,
    )
    .unwrap();
    let req = invite(
        "sip:any@host.example",
        "<sip:c@x.example>",
        "<sip:any@host.example>",
    );

    let action = {
        let rs = String::from("trunk");
        dispatch_invite(&routes, &rs, &req)
        // rs dropped here
    };

    match action {
        RouteAction::Accept { route, .. } => assert_eq!(route.name, "any-call"),
        _ => panic!("expected Accept"),
    }
}
