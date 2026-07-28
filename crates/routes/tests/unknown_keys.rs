//! A key the dialplan schema doesn't define is a typo, and a typo in a
//! route is never harmless: dropped silently it either widens the match
//! (routes are first-match-wins, so an over-broad route steals calls
//! meant for later ones) or leaves an override unset, which is
//! indistinguishable from "inherit the global". See issue #383.

use siphon_ai_routes::{load_from_toml, LoadError};

/// Every case here must be rejected at parse time, before compilation.
fn rejected_key(toml: &str) -> String {
    match load_from_toml(toml) {
        Err(LoadError::Toml(e)) => e.to_string(),
        Err(other) => panic!("expected a TOML parse error, got {other:?}"),
        Ok(_) => panic!("unknown key was accepted:\n{toml}"),
    }
}

#[test]
fn misspelled_match_key_is_rejected_not_dropped() {
    // `to` reads right — it is what the SIP header is called — but the
    // schema key is `to_user`. Dropped, this route would match on
    // `from_user` alone: broader than written.
    rejected_key(
        r#"
        [[route]]
        name = "narrow"
        [route.match]
        from_user = "+13125551234"
        to = "+13125559999"
        "#,
    );
}

#[test]
fn misspelled_bridge_override_is_rejected_not_silently_inherited() {
    // `ws_uri` vs `ws_url`. Dropped, the route bridges to the global
    // `[bridge].ws_url` with no warning — a production misroute.
    let err = rejected_key(
        r#"
        [[route]]
        name = "bot"
        [route.match]
        any = true
        [route.bridge]
        ws_uri = "ws://127.0.0.1:8081/"
        "#,
    );
    assert!(err.contains("ws_uri"), "error should name the key: {err}");
}

#[test]
fn unknown_key_on_the_route_itself_is_rejected() {
    // `register_source` is a `[route.match]` key; at route level it did
    // nothing. The shipped twilio-trunk example carried exactly this.
    rejected_key(
        r#"
        [[route]]
        name = "trunk_inbound"
        register_source = "trunk"
        [route.match]
        any = true
        "#,
    );
}

#[test]
fn unknown_keys_in_the_remaining_override_blocks_are_rejected() {
    let blocks = [
        ("route.media", "codecs_list = [\"pcmu\"]"),
        ("route.security", "min_attest = \"B\""),
        ("route.recording", "modes = \"always\""),
        ("route.bridge.barge_in", "debounce = 200"),
    ];
    for (block, key) in blocks {
        rejected_key(&format!(
            r#"
            [[route]]
            name = "r"
            [route.match]
            any = true
            [{block}]
            {key}
            "#
        ));
    }
}

#[test]
fn known_keys_still_parse() {
    // The guard against over-tightening: a route using a key from every
    // block must still load — including `[route.match.header]`, whose
    // *names* are map keys and must stay unrestricted.
    let toml = r#"
        [[route]]
        name = "full"
        [route.match]
        to_user = "5000"
        [route.match.header]
        X-Tenant = "acme"
        [route.bridge]
        ws_url = "ws://127.0.0.1:8081/"
        [route.bridge.barge_in]
        debounce_ms = 200
        [route.media]
        codecs = ["pcmu"]
        [route.security]
        min_attestation = "B"
        [route.recording]
        mode = "always"

        [[route]]
        name = "fallback"
        [route.match]
        any = true
    "#;
    let set = load_from_toml(toml).expect("known keys must still load");
    assert!(set.has_default());
}

#[test]
fn non_route_tables_are_still_ignored() {
    // `load_from_toml` is handed whole config files and picks the routes
    // out, so unknown *top-level* tables must stay tolerated — the
    // strictness is scoped to the route tables themselves.
    let toml = r#"
        [node]
        id = "siphon-ai-local"

        [sip]
        listen = "127.0.0.1:5070"

        [[route]]
        name = "fallback"
        [route.match]
        any = true
    "#;
    let set = load_from_toml(toml).expect("top-level tables stay ignorable");
    assert!(set.has_default());
}
