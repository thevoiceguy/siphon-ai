//! End-to-end grammar tests: TOML in, route picked out.
//!
//! These exercise the public API surface (`load_from_toml`,
//! `RouteSet::find_match`, `Headers`) the way the daemon will use it.
//! Module-level tests in `compile.rs` and `compiled.rs` cover the
//! internal pieces.

use siphon_ai_routes::{load_from_toml, CallInfo, Headers, RouteSet};

fn empty_headers() -> Headers {
    Headers::new()
}

fn info<'a>(request_uri_user: &'a str, headers: &'a Headers) -> CallInfo<'a> {
    CallInfo {
        request_uri_user,
        request_uri_host: "siphon.example.com",
        to_user: request_uri_user,
        to_host: "siphon.example.com",
        from_user: "+13125551234",
        from_host: "carrier.example.net",
        register_source: "trunk",
        peer_cert_names: &[],
        headers,
    }
}

fn matched_name<'a>(set: &'a RouteSet, info: &CallInfo<'_>) -> Option<&'a str> {
    set.find_match(info).map(|r| r.name.as_str())
}

#[test]
fn first_match_wins_top_to_bottom() {
    let toml = r#"
        [[route]]
        name = "specific"
        [route.match]
        request_uri_user = "5000"

        [[route]]
        name = "fallback"
        [route.match]
        any = true
    "#;
    let set = load_from_toml(toml).unwrap();
    let h = empty_headers();
    assert_eq!(matched_name(&set, &info("5000", &h)), Some("specific"));
    assert_eq!(matched_name(&set, &info("9999", &h)), Some("fallback"));
    assert!(set.has_default());
}

#[test]
fn literal_match_is_case_insensitive() {
    let toml = r#"
        [[route]]
        name = "alice"
        [route.match]
        to_user = "Alice"
    "#;
    let set = load_from_toml(toml).unwrap();
    let h = empty_headers();
    let mut i = info("anything", &h);
    i.to_user = "ALICE";
    assert_eq!(matched_name(&set, &i), Some("alice"));
    i.to_user = "alice";
    assert_eq!(matched_name(&set, &i), Some("alice"));
    i.to_user = "alic";
    assert_eq!(matched_name(&set, &i), None);
}

#[test]
fn regex_flag_applies_to_every_string_key_in_the_route() {
    let toml = r#"
        [[route]]
        name = "sales"
        [route.match]
        regex = true
        request_uri_user = "^sales-[0-9]+$"
        from_host = "carrier\\."
    "#;
    let set = load_from_toml(toml).unwrap();
    let h = empty_headers();

    let mut i = info("sales-42", &h);
    i.from_host = "carrier.example.net";
    assert_eq!(matched_name(&set, &i), Some("sales"));

    // Both must hold (AND). Same user, wrong from_host → no match.
    let mut i = info("sales-42", &h);
    i.from_host = "spam.example.com";
    assert_eq!(matched_name(&set, &i), None);

    // Regex on user side fails.
    let mut i = info("sales-abc", &h);
    i.from_host = "carrier.example.net";
    assert_eq!(matched_name(&set, &i), None);
}

#[test]
fn keys_within_a_route_are_anded() {
    let toml = r#"
        [[route]]
        name = "vip-from-cucm"
        [route.match]
        from_user = "+13125551234"
        register_source = "cucm-main"
    "#;
    let set = load_from_toml(toml).unwrap();
    let h = empty_headers();

    let mut i = info("any", &h);
    i.from_user = "+13125551234";
    i.register_source = "cucm-main";
    assert_eq!(matched_name(&set, &i), Some("vip-from-cucm"));

    // Either side missing → no match.
    let mut i = info("any", &h);
    i.from_user = "+13125551234";
    i.register_source = "trunk";
    assert_eq!(matched_name(&set, &i), None);

    let mut i = info("any", &h);
    i.from_user = "+19995551234";
    i.register_source = "cucm-main";
    assert_eq!(matched_name(&set, &i), None);
}

#[test]
fn header_match_is_case_insensitive_on_name() {
    let toml = r#"
        [[route]]
        name = "by-customer"
        [route.match]
        regex = true
        [route.match.header]
        X-Customer-Id = "^cust-.*$"
    "#;
    let set = load_from_toml(toml).unwrap();

    let mut h = Headers::new();
    h.insert("x-customer-id", "cust-42");
    assert_eq!(matched_name(&set, &info("any", &h)), Some("by-customer"));

    let mut h = Headers::new();
    h.insert("X-Customer-Id", "internal");
    assert_eq!(matched_name(&set, &info("any", &h)), None);

    // Header missing → predicate sees "" → no match.
    let h = Headers::new();
    assert_eq!(matched_name(&set, &info("any", &h)), None);
}

#[test]
fn no_match_returns_none_and_has_default_is_false() {
    let toml = r#"
        [[route]]
        name = "specific"
        [route.match]
        request_uri_user = "5000"
    "#;
    let set = load_from_toml(toml).unwrap();
    let h = empty_headers();
    assert_eq!(matched_name(&set, &info("5001", &h)), None);
    assert!(!set.has_default());
}

#[test]
fn empty_file_loads_to_empty_set() {
    let set = load_from_toml("").unwrap();
    assert!(set.is_empty());
    assert!(!set.has_default());
}

#[test]
fn bridge_overrides_round_trip_through_compile() {
    let toml = r#"
        [[route]]
        name = "main"
        [route.match]
        request_uri_user = "5000"
        [route.bridge]
        ws_url = "wss://reception.example.com/sip-bridge"

        [route.bridge.barge_in]
        enabled = true
        debounce_ms = 120
    "#;
    let set = load_from_toml(toml).unwrap();
    let route = set.iter().next().unwrap();
    assert_eq!(
        route.bridge.ws_url.as_deref(),
        Some("wss://reception.example.com/sip-bridge")
    );
    assert_eq!(route.bridge.barge_in.enabled, Some(true));
    assert_eq!(route.bridge.barge_in.debounce_ms, Some(120));
}

#[test]
fn duplicate_route_names_fail_to_load() {
    let toml = r#"
        [[route]]
        name = "main"
        [route.match]
        request_uri_user = "5000"

        [[route]]
        name = "main"
        [route.match]
        request_uri_user = "6000"
    "#;
    assert!(load_from_toml(toml).is_err());
}

#[test]
fn invalid_regex_fails_to_load() {
    let toml = r#"
        [[route]]
        name = "bad"
        [route.match]
        regex = true
        request_uri_user = "[unterminated"
    "#;
    assert!(load_from_toml(toml).is_err());
}

#[test]
fn any_combined_with_other_keys_fails_to_load() {
    let toml = r#"
        [[route]]
        name = "ambiguous"
        [route.match]
        any = true
        request_uri_user = "5000"
    "#;
    assert!(load_from_toml(toml).is_err());
}

/// `peer_cert_san` (siphon-rs #129 / mutual TLS): matches when any
/// name the verified client certificate asserts satisfies the
/// predicate, and never for a call that presented no certificate.
#[test]
fn peer_cert_san_matches_any_asserted_name() {
    let toml = r#"
        [[route]]
        name = "node-1"
        [route.match]
        peer_cert_san = "sip:node-1@example.com"

        [[route]]
        name = "fallback"
        [route.match]
        any = true
    "#;
    let set = load_from_toml(toml).unwrap();
    let h = empty_headers();

    // URI SAN first, DNS SAN second, CN last — order the transport uses.
    let names = vec![
        "sip:node-1@example.com".to_string(),
        "node-1.example.com".to_string(),
        "node-1".to_string(),
    ];
    let mut i = info("any", &h);
    i.peer_cert_names = &names;
    assert_eq!(matched_name(&set, &i), Some("node-1"));

    // Literal matching is case-insensitive like every other key.
    let upper = vec!["SIP:NODE-1@EXAMPLE.COM".to_string()];
    let mut i = info("any", &h);
    i.peer_cert_names = &upper;
    assert_eq!(matched_name(&set, &i), Some("node-1"));

    // A different node falls through to the default.
    let other = vec!["sip:node-2@example.com".to_string()];
    let mut i = info("any", &h);
    i.peer_cert_names = &other;
    assert_eq!(matched_name(&set, &i), Some("fallback"));
}

#[test]
fn peer_cert_san_never_matches_a_call_without_a_verified_certificate() {
    let toml = r#"
        [[route]]
        name = "authenticated-trunk"
        [route.match]
        regex = true
        peer_cert_san = ".*"
    "#;
    let set = load_from_toml(toml).unwrap();
    let h = empty_headers();
    // Even a match-everything regex cannot admit a call with no
    // names: there is no candidate string to run it against.
    let i = info("any", &h);
    assert_eq!(matched_name(&set, &i), None);
    assert!(set.iter().next().unwrap().uses_peer_cert_san());
}

#[test]
fn peer_cert_san_regex_and_ands_with_other_keys() {
    let toml = r#"
        [[route]]
        name = "cascade"
        [route.match]
        regex = true
        peer_cert_san = "^sip:node-[0-9]+@example\\.com$"
        request_uri_user = "^conf-"
    "#;
    let set = load_from_toml(toml).unwrap();
    let h = empty_headers();
    let names = vec!["sip:node-7@example.com".to_string()];

    let mut i = info("conf-1234", &h);
    i.peer_cert_names = &names;
    assert_eq!(matched_name(&set, &i), Some("cascade"));

    // Right certificate, wrong target → no match.
    let mut i = info("5000", &h);
    i.peer_cert_names = &names;
    assert_eq!(matched_name(&set, &i), None);

    // Right target, certificate from outside the pattern → no match.
    let rogue = vec!["sip:attacker@example.org".to_string()];
    let mut i = info("conf-1234", &h);
    i.peer_cert_names = &rogue;
    assert_eq!(matched_name(&set, &i), None);
}
