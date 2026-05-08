//! End-to-end tests for `BridgingAcceptor::prepare_call`.
//!
//! These exercise everything from "matched route" up to "controller
//! ready to run": real SDP negotiation against a real forge
//! `SessionManager`, real `MediaTap` attached on a real
//! `MediaBridgeManager`. The fully async `on_matched` path can't be
//! exercised in isolation because `ServerTransactionHandle` has no
//! public test constructor (see `sip-glue/tests/handler_dispatch.rs`).

use std::sync::Arc;

use bytes::Bytes;
use forge_engine::{MediaBridgeManager, SessionManager, SessionManagerConfig};
use forge_rtp::PortPoolConfig;
use forge_sdp::{MediaType, SessionDescription, SessionDescriptionExt};
use sip_core::{Headers as SipHeaders, Method, Request, RequestLine, SipUri};
use siphon_ai_bridge::{AudioEncoding, CallId as BridgeCallId, Direction, PROTOCOL_VERSION};
use siphon_ai_core::{AcceptError, BridgeDefaults, BridgingAcceptor, CallRegistry};
use siphon_ai_media_glue::MediaSetup;
use siphon_ai_routes::{load_from_toml, RouteSet};
use siphon_ai_sip_glue::InviteFacts;

const LINPHONE_PCMU_OFFER: &str = "v=0\r\n\
o=alice 1234 5678 IN IP4 10.0.0.5\r\n\
s=Talk\r\n\
c=IN IP4 10.0.0.5\r\n\
t=0 0\r\n\
m=audio 7078 RTP/AVP 0 8 101\r\n\
a=rtpmap:0 PCMU/8000\r\n\
a=rtpmap:8 PCMA/8000\r\n\
a=rtpmap:101 telephone-event/8000\r\n\
a=fmtp:101 0-15\r\n\
a=sendrecv\r\n";

const G729_ONLY_OFFER: &str = "v=0\r\n\
o=- 1 1 IN IP4 10.0.0.5\r\n\
s=Talk\r\n\
c=IN IP4 10.0.0.5\r\n\
t=0 0\r\n\
m=audio 7000 RTP/AVP 18\r\n\
a=rtpmap:18 G729/8000\r\n\
a=sendrecv\r\n";

fn invite(content_type: Option<&str>, body: &str, request_uri: &str) -> Request {
    let uri = SipUri::parse(request_uri).expect("uri");
    let line = RequestLine::new(Method::Invite, uri);
    let mut h = SipHeaders::new();
    h.push("Via", "SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-1")
        .unwrap();
    h.push("From", "<sip:+13125551234@carrier.example.net>;tag=abc")
        .unwrap();
    h.push("To", "<sip:5000@siphon.example.com>").unwrap();
    h.push("Call-ID", "abc-123@pbx.example.com").unwrap();
    h.push("CSeq", "1 INVITE").unwrap();
    h.push("User-Agent", "Cisco-CP8841").unwrap();
    if let Some(ct) = content_type {
        h.push("Content-Type", ct).unwrap();
    }
    h.push("Content-Length", body.len().to_string()).unwrap();
    Request::new(line, h, Bytes::from(body.as_bytes().to_vec())).unwrap()
}

fn one_route_routes() -> RouteSet {
    load_from_toml(
        r#"
        [[route]]
        name = "default"
        [route.match]
        any = true
        [route.bridge]
        ws_url = "wss://route.example/sip-bridge"
        "#,
    )
    .expect("compile routes")
}

fn build_acceptor(
    min_port: u16,
    max_port: u16,
) -> (
    BridgingAcceptor,
    Arc<MediaBridgeManager>,
    Arc<SessionManager>,
    CallRegistry,
) {
    let session_mgr = SessionManager::new(
        SessionManagerConfig {
            port_pool_config: PortPoolConfig::new(min_port, max_port).unwrap(),
            ..Default::default()
        },
        None,
    );
    let bridge_mgr = Arc::new(MediaBridgeManager::new());
    let media = Arc::new(MediaSetup::new(
        Arc::clone(&session_mgr),
        Arc::clone(&bridge_mgr),
        "192.168.1.10",
    ));
    let defaults = BridgeDefaults {
        forward_headers: vec!["User-Agent".into()],
        ..BridgeDefaults::default()
    };
    let registry = CallRegistry::new();
    (
        BridgingAcceptor::new(media, defaults, registry.clone())
            .with_call_id_factory(Arc::new(|| BridgeCallId::new("siphon-test-fixed"))),
        bridge_mgr,
        session_mgr,
        registry,
    )
}

#[tokio::test]
async fn prepare_happy_path_produces_runnable_call() {
    let (acceptor, bridge_mgr, session_mgr, _registry) = build_acceptor(50100, 50200);
    let routes = one_route_routes();
    let req = invite(
        Some("application/sdp"),
        LINPHONE_PCMU_OFFER,
        "sip:5000@siphon.example.com",
    );
    let facts = InviteFacts::extract(&req);
    let route = routes.iter().next().expect("route exists");

    let prepared = acceptor
        .prepare_call(&req, route, &facts)
        .await
        .expect("prepare succeeds");

    // (a) Bridge call id from the test-fixed factory.
    assert_eq!(prepared.bridge_call_id.as_str(), "siphon-test-fixed");
    assert_eq!(prepared.forge_call_id.0, "siphon-test-fixed");

    // (b) Bridge config picked up the route override.
    assert_eq!(
        prepared.bridge_config.ws_url,
        "wss://route.example/sip-bridge"
    );

    // (c) StartMsg reflects the negotiated audio + facts.
    assert_eq!(prepared.start.version, PROTOCOL_VERSION);
    assert_eq!(prepared.start.from, "+13125551234");
    assert_eq!(prepared.start.to, "5000");
    assert_eq!(prepared.start.direction, Direction::Inbound);
    assert_eq!(prepared.start.audio.encoding, AudioEncoding::Pcm16le);
    assert_eq!(prepared.start.audio.sample_rate, 8000); // PCMU
    assert_eq!(prepared.start.sip.call_id, "abc-123@pbx.example.com");
    assert_eq!(
        prepared
            .start
            .sip
            .headers
            .get("User-Agent")
            .map(String::as_str),
        Some("Cisco-CP8841"),
    );

    // (d) Answer is an actual SDP that points at our local port.
    let parsed = SessionDescription::from_str(&prepared.answer.answer_text).expect("answer parses");
    let audio = parsed.find_media(MediaType::Audio).expect("audio");
    assert!(prepared
        .answer
        .answer_text
        .contains("c=IN IP4 192.168.1.10"));
    assert_eq!(audio.port, prepared.answer.answer.media[0].port);

    // (e) The forge session and tap are alive.
    let forge_call_id = prepared.forge_call_id.clone();
    assert!(session_mgr.get_session(&forge_call_id).is_some());
    assert!(bridge_mgr.has_bridge(&forge_call_id));
}

#[tokio::test]
async fn unsupported_media_type_maps_to_415() {
    let (acceptor, _, _, _) = build_acceptor(50300, 50400);
    let routes = one_route_routes();
    let route = routes.iter().next().unwrap();
    let req = invite(Some("text/plain"), "hi", "sip:5000@siphon.example.com");
    let facts = InviteFacts::extract(&req);

    let err = acceptor
        .prepare_call(&req, route, &facts)
        .await
        .unwrap_err();
    assert_eq!(err.sip_status(), (415, "Unsupported Media Type"));
}

#[tokio::test]
async fn no_common_codec_maps_to_488() {
    let (acceptor, bridge_mgr, session_mgr, _registry) = build_acceptor(50500, 50600);
    let routes = one_route_routes();
    let route = routes.iter().next().unwrap();
    let req = invite(
        Some("application/sdp"),
        G729_ONLY_OFFER,
        "sip:5000@siphon.example.com",
    );
    let facts = InviteFacts::extract(&req);

    let err = acceptor
        .prepare_call(&req, route, &facts)
        .await
        .unwrap_err();
    let (code, _) = err.sip_status();
    assert_eq!(code, 488);
    assert!(matches!(err, AcceptError::Setup(_)));

    // The half-built session must roll back so ports return to the
    // pool. (The rollback is via `tokio::spawn` inside MediaSetup —
    // give it a beat to land.)
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert_eq!(session_mgr.session_count(), 0);
    assert!(!bridge_mgr.has_bridge(&forge_core::CallId::new("siphon-test-fixed")));
}

#[tokio::test]
async fn route_without_ws_url_when_no_default_yields_503() {
    // Build an acceptor with no global default ws_url, and a route
    // override that also doesn't set one. The acceptor should return
    // a 503 mapping rather than booking a half-built session.
    let session_mgr = SessionManager::new(
        SessionManagerConfig {
            port_pool_config: PortPoolConfig::new(50700, 50800).unwrap(),
            ..Default::default()
        },
        None,
    );
    let bridge_mgr = Arc::new(MediaBridgeManager::new());
    let media = Arc::new(MediaSetup::new(
        Arc::clone(&session_mgr),
        Arc::clone(&bridge_mgr),
        "192.168.1.10",
    ));
    let acceptor =
        BridgingAcceptor::new(media, BridgeDefaults::default(), CallRegistry::new());

    let routes = load_from_toml(
        r#"
        [[route]]
        name = "default"
        [route.match]
        any = true
        "#,
    )
    .unwrap();
    let route = routes.iter().next().unwrap();
    let req = invite(
        Some("application/sdp"),
        LINPHONE_PCMU_OFFER,
        "sip:5000@siphon.example.com",
    );
    let facts = InviteFacts::extract(&req);

    let err = acceptor
        .prepare_call(&req, route, &facts)
        .await
        .unwrap_err();
    assert_eq!(err.sip_status(), (503, "Service Unavailable"));

    // No session allocated.
    assert_eq!(session_mgr.session_count(), 0);
}

#[tokio::test]
async fn prepare_exposes_handle_and_sip_call_id_for_registry_use() {
    // The whole point of surfacing `handle` and `sip_call_id` from
    // PreparedCall is so on_matched (and any tests that mirror it)
    // can register the handle keyed by Call-ID.
    let (acceptor, _, _, registry) = build_acceptor(51100, 51200);
    let routes = one_route_routes();
    let req = invite(
        Some("application/sdp"),
        LINPHONE_PCMU_OFFER,
        "sip:5000@siphon.example.com",
    );
    let facts = InviteFacts::extract(&req);
    let route = routes.iter().next().unwrap();

    let prepared = acceptor.prepare_call(&req, route, &facts).await.unwrap();
    assert_eq!(prepared.sip_call_id, "abc-123@pbx.example.com");
    assert_eq!(prepared.handle.call_id().as_str(), "siphon-test-fixed");

    // Mirror the on_matched register step.
    registry.insert(prepared.sip_call_id.clone(), prepared.handle);
    let looked_up = registry.lookup("abc-123@pbx.example.com").expect("present");
    assert_eq!(looked_up.call_id().as_str(), "siphon-test-fixed");
    assert_eq!(registry.len(), 1);

    // Mirror the spawned-task remove step.
    registry.remove("abc-123@pbx.example.com");
    assert!(registry.is_empty());
}

#[tokio::test]
async fn second_call_gets_a_fresh_forge_session() {
    // Default factory generates unique ids. Call prepare twice with
    // the default factory, confirm both sessions live concurrently.
    let session_mgr = SessionManager::new(
        SessionManagerConfig {
            port_pool_config: PortPoolConfig::new(50900, 51000).unwrap(),
            ..Default::default()
        },
        None,
    );
    let bridge_mgr = Arc::new(MediaBridgeManager::new());
    let media = Arc::new(MediaSetup::new(
        Arc::clone(&session_mgr),
        Arc::clone(&bridge_mgr),
        "192.168.1.10",
    ));
    let acceptor =
        BridgingAcceptor::new(media, BridgeDefaults::default(), CallRegistry::new());

    // We need a default ws_url so prepare_call accepts the offer.
    let routes = load_from_toml(
        r#"
        [[route]]
        name = "default"
        [route.match]
        any = true
        [route.bridge]
        ws_url = "wss://x/y"
        "#,
    )
    .unwrap();
    let route = routes.iter().next().unwrap();
    let req = invite(
        Some("application/sdp"),
        LINPHONE_PCMU_OFFER,
        "sip:5000@siphon.example.com",
    );
    let facts = InviteFacts::extract(&req);

    let p1 = acceptor.prepare_call(&req, route, &facts).await.unwrap();
    let p2 = acceptor.prepare_call(&req, route, &facts).await.unwrap();

    assert_ne!(p1.bridge_call_id, p2.bridge_call_id);
    assert_eq!(session_mgr.session_count(), 2);
}
