//! Metrics emitted by `BridgingAcceptor` end-to-end.
//!
//! Uses `metrics::with_local_recorder` to install a per-test
//! Prometheus recorder, drives a real call through `prepare_call` →
//! `run_call`, asserts on the rendered `/metrics`-style text. The
//! local-recorder approach avoids touching the global `metrics`
//! state so tests run in parallel without leaking into each other.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use forge_engine::{MediaBridgeManager, SessionManager, SessionManagerConfig};
use forge_rtp::PortPoolConfig;
use futures::{SinkExt, StreamExt};
use serde_json::Value;
use sip_core::{Headers as SipHeaders, Method, Request, RequestLine, SipUri};
use siphon_ai_bridge::CallId as BridgeCallId;
use siphon_ai_core::{BridgeDefaults, BridgingAcceptor, CallRegistry};
use siphon_ai_media_glue::MediaSetup;
use siphon_ai_routes::{load_from_toml, RouteSet};
use siphon_ai_sip_glue::InviteFacts;
use siphon_ai_telemetry::{
    prometheus_builder, register_descriptions, CALLS_ACTIVE, CALLS_TOTAL, CALL_DURATION_SECONDS,
    INVITES_TOTAL, ROUTE_MATCH_TOTAL, SDP_NEGOTIATE_SECONDS,
};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::handshake::server::{
    ErrorResponse as HsErrorResponse, Request as HsRequest, Response as HsResponse,
};
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::Message;

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

#[allow(clippy::result_large_err)]
fn echo_subprotocol(req: &HsRequest, mut resp: HsResponse) -> Result<HsResponse, HsErrorResponse> {
    if let Some(offered) = req.headers().get("Sec-WebSocket-Protocol") {
        resp.headers_mut().insert(
            "Sec-WebSocket-Protocol",
            HeaderValue::from_bytes(offered.as_bytes()).unwrap(),
        );
    }
    Ok(resp)
}

async fn server_acks_start_then_idles(port_tx: tokio::sync::oneshot::Sender<u16>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().unwrap().port();
    let _ = port_tx.send(port);
    let (stream, _) = listener.accept().await.expect("accept");
    let mut ws = tokio_tungstenite::accept_hdr_async(stream, echo_subprotocol)
        .await
        .expect("ws accept");
    while let Some(msg) = ws.next().await {
        match msg {
            Ok(Message::Text(t)) => {
                let v: Value = serde_json::from_str(&t).unwrap();
                if v["type"] == "stop" {
                    let _ = ws.send(Message::Close(None)).await;
                    break;
                }
            }
            Ok(Message::Close(_)) | Err(_) => break,
            _ => {}
        }
    }
}

fn invite(body: &str, request_uri: &str, call_id: &str) -> Request {
    let uri = SipUri::parse(request_uri).expect("uri");
    let line = RequestLine::new(Method::Invite, uri);
    let mut h = SipHeaders::new();
    h.push("Via", "SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-m")
        .unwrap();
    h.push("From", "<sip:+13125551234@carrier.example.net>;tag=abc")
        .unwrap();
    h.push("To", "<sip:5000@siphon.example.com>").unwrap();
    h.push("Call-ID", call_id).unwrap();
    h.push("CSeq", "1 INVITE").unwrap();
    h.push("Content-Type", "application/sdp").unwrap();
    h.push("Content-Length", body.len().to_string()).unwrap();
    Request::new(line, h, Bytes::from(body.as_bytes().to_vec())).unwrap()
}

fn one_route(ws_url: &str) -> RouteSet {
    load_from_toml(&format!(
        r#"
        [[route]]
        name = "metrics_route"
        [route.match]
        any = true
        [route.bridge]
        ws_url = "{ws_url}"
        "#,
    ))
    .expect("compile routes")
}

fn build_acceptor() -> (BridgingAcceptor, Arc<MediaBridgeManager>, CallRegistry) {
    let session_mgr = SessionManager::new(
        SessionManagerConfig {
            port_pool_config: PortPoolConfig::new(60500, 60600).unwrap(),
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
    let registry = CallRegistry::new();
    let acceptor = BridgingAcceptor::new(media, BridgeDefaults::default(), registry.clone())
        .with_call_id_factory(Arc::new(|| BridgeCallId::new("siphon-metrics-test")));
    (acceptor, bridge_mgr, registry)
}

#[tokio::test(flavor = "current_thread")]
async fn full_call_lifecycle_emits_expected_metrics() {
    // Per-test recorder; the `LocalRecorderGuard` is thread-local
    // and the `current_thread` flavor pins all spawned tasks to the
    // same OS thread, so `metrics::counter!` calls inside the
    // acceptor's spawned task see this recorder.
    let recorder = prometheus_builder().unwrap().build_recorder();
    let handle = recorder.handle();
    let _guard = metrics::set_default_local_recorder(&recorder);
    register_descriptions();

    let (port_tx, port_rx) = tokio::sync::oneshot::channel();
    tokio::spawn(server_acks_start_then_idles(port_tx));
    let port = port_rx.await.unwrap();
    let ws_url = format!("ws://127.0.0.1:{port}/");

    let (acceptor, _bridge_mgr, registry) = build_acceptor();
    let routes = one_route(&ws_url);
    let route = routes.iter().next().unwrap();
    let req = invite(
        LINPHONE_PCMU_OFFER,
        "sip:5000@siphon.example.com",
        "abc-metrics@pbx",
    );
    let facts = InviteFacts::extract(&req);

    let prepared = acceptor.prepare_call(&req, route, &facts).await.unwrap();
    // Mirror what on_matched does on the accept path so the
    // accepted-counter increment is exercised here too.
    metrics::counter!(INVITES_TOTAL, "result" => "accepted").increment(1);
    let run_handle = acceptor.run_call(prepared, "metrics_route");

    tokio::time::sleep(Duration::from_millis(150)).await;
    let h = registry.lookup("abc-metrics@pbx").expect("registered");
    h.shutdown();

    tokio::time::timeout(Duration::from_secs(3), run_handle)
        .await
        .expect("run_call completes")
        .expect("task does not panic");

    let out = handle.render();

    // Counters
    assert!(
        out.contains(&format!("{INVITES_TOTAL}{{result=\"accepted\"}} 1")),
        "missing accepted counter:\n{out}"
    );
    assert!(
        out.contains(&format!("{ROUTE_MATCH_TOTAL}{{route=\"metrics_route\"}} 1")),
        "missing route counter:\n{out}"
    );
    assert!(
        out.contains(&format!("{CALLS_TOTAL}{{cause=\"local_shutdown\"}} 1")),
        "missing calls_total local_shutdown:\n{out}"
    );

    // Active gauge: should have been 1 during the call, decremented
    // back to 0 by exit.
    assert!(
        out.contains(&format!("{CALLS_ACTIVE} 0")),
        "active gauge should be 0 after exit:\n{out}"
    );

    // Histograms: at least one bucket touched, count > 0.
    assert!(
        out.contains(&format!("{SDP_NEGOTIATE_SECONDS}_count")),
        "missing sdp_negotiate histogram:\n{out}"
    );
    assert!(
        out.contains(&format!("{CALL_DURATION_SECONDS}_count")),
        "missing call_duration histogram:\n{out}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn rejected_invite_increments_rejected_counter() {
    let recorder = prometheus_builder().unwrap().build_recorder();
    let handle = recorder.handle();
    let _guard = metrics::set_default_local_recorder(&recorder);
    register_descriptions();

    let (acceptor, _, _) = build_acceptor();
    let routes = one_route("wss://x/y");
    let route = routes.iter().next().unwrap();
    let req = invite(
        G729_ONLY_OFFER,
        "sip:5000@siphon.example.com",
        "abc-rej@pbx",
    );
    let facts = InviteFacts::extract(&req);

    let err = acceptor
        .prepare_call(&req, route, &facts)
        .await
        .unwrap_err();
    // Mirror what `on_matched` does on the reject path so we hit
    // the same metric increments without fabricating a
    // ServerTransactionHandle.
    assert_eq!(err.sip_status().0, 488);
    metrics::counter!(INVITES_TOTAL, "result" => "rejected").increment(1);

    let out = handle.render();
    // SDP negotiate histogram should have an `error` result entry.
    assert!(
        out.contains(&format!(
            "{SDP_NEGOTIATE_SECONDS}_count{{result=\"error\"}}"
        )),
        "expected error-result histogram entry:\n{out}"
    );
    assert!(
        out.contains(&format!("{INVITES_TOTAL}{{result=\"rejected\"}} 1")),
        "missing rejected counter:\n{out}"
    );
}
