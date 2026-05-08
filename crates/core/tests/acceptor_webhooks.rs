//! End-to-end lifecycle webhook emission: drive a call from
//! `prepare_call` → `run_call` against a real WS server, assert
//! the spawned task fires exactly one `call_start` and one
//! `call_end` event, in order, with the shape downstream
//! consumers see.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use forge_engine::{MediaBridgeManager, SessionManager, SessionManagerConfig};
use forge_rtp::PortPoolConfig;
use futures::{SinkExt, StreamExt};
use parking_lot::Mutex;
use serde_json::Value;
use sip_core::{Headers as SipHeaders, Method, Request, RequestLine, SipUri};
use siphon_ai_bridge::CallId as BridgeCallId;
use siphon_ai_core::{BridgeDefaults, BridgingAcceptor, CallRegistry};
use siphon_ai_media_glue::MediaSetup;
use siphon_ai_routes::{load_from_toml, RouteSet};
use siphon_ai_sip_glue::InviteFacts;
use siphon_ai_webhooks::{WebhookEvent, WebhookSink};
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
m=audio 7078 RTP/AVP 0\r\n\
a=rtpmap:0 PCMU/8000\r\n\
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
    h.push("Via", "SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-w")
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
        name = "webhook_route"
        [route.match]
        any = true
        [route.bridge]
        ws_url = "{ws_url}"
        "#,
    ))
    .expect("compile routes")
}

#[derive(Default)]
struct Recorder {
    events: Mutex<Vec<WebhookEvent>>,
}

#[async_trait]
impl WebhookSink for Recorder {
    async fn emit(&self, event: WebhookEvent) {
        self.events.lock().push(event);
    }
}

#[tokio::test]
async fn run_call_emits_call_start_then_call_end() {
    let (port_tx, port_rx) = tokio::sync::oneshot::channel();
    tokio::spawn(server_acks_start_then_idles(port_tx));
    let port = port_rx.await.unwrap();
    let ws_url = format!("ws://127.0.0.1:{port}/");

    let session_mgr = SessionManager::new(
        SessionManagerConfig {
            port_pool_config: PortPoolConfig::new(60700, 60800).unwrap(),
            ..Default::default()
        },
        None,
    );
    let bridge_mgr = Arc::new(MediaBridgeManager::new());
    let media = Arc::new(MediaSetup::new(
        Arc::clone(&session_mgr),
        bridge_mgr,
        "192.168.1.10",
    ));
    let registry = CallRegistry::new();
    let recorder = Arc::new(Recorder::default());

    let acceptor = BridgingAcceptor::new(media, BridgeDefaults::default(), registry.clone())
        .with_call_id_factory(Arc::new(|| BridgeCallId::new("siphon-webhook-test")))
        .with_webhook_sink(Arc::clone(&recorder) as Arc<dyn WebhookSink>);

    let routes = one_route(&ws_url);
    let route = routes.iter().next().unwrap();
    let req = invite(
        LINPHONE_PCMU_OFFER,
        "sip:5000@siphon.example.com",
        "abc-webhook@pbx",
    );
    let facts = InviteFacts::extract(&req);

    let prepared = acceptor
        .prepare_call(&req, route, &facts)
        .await
        .expect("prepare");
    let run_handle = acceptor.run_call(prepared, "webhook_route");

    tokio::time::sleep(Duration::from_millis(150)).await;
    let h = registry.lookup("abc-webhook@pbx").expect("registered");
    h.shutdown();

    tokio::time::timeout(Duration::from_secs(3), run_handle)
        .await
        .expect("run_call completes")
        .expect("task does not panic");

    // The call_start emit is on its own spawn — give it a beat
    // even after run_call returns.
    tokio::time::sleep(Duration::from_millis(50)).await;

    let events = recorder.events.lock();
    assert_eq!(events.len(), 2, "expected start+end, got {events:?}");

    // Order matters: receivers expect start before end.
    match &events[0] {
        WebhookEvent::CallStart(s) => {
            assert_eq!(s.version, 1);
            assert_eq!(s.call_id, "siphon-webhook-test");
            assert_eq!(s.sip_call_id, "abc-webhook@pbx");
            assert_eq!(s.from, "+13125551234");
            assert_eq!(s.to, "5000");
            assert_eq!(s.route, "webhook_route");
            assert_eq!(s.ws_url, ws_url);
        }
        other => panic!("expected CallStart first, got {other:?}"),
    }
    match &events[1] {
        WebhookEvent::CallEnd(e) => {
            assert_eq!(e.version, 1);
            assert_eq!(e.call_id, "siphon-webhook-test");
            assert_eq!(e.termination_cause, "local_shutdown");
            assert!(e.duration_ms < 5_000, "duration {} too long", e.duration_ms);
        }
        other => panic!("expected CallEnd second, got {other:?}"),
    }
}

#[tokio::test]
async fn null_webhook_sink_is_the_default() {
    // No with_webhook_sink call. Confirms the spawned task doesn't
    // panic when the default NullSink is in place.
    let (port_tx, port_rx) = tokio::sync::oneshot::channel();
    tokio::spawn(server_acks_start_then_idles(port_tx));
    let port = port_rx.await.unwrap();
    let ws_url = format!("ws://127.0.0.1:{port}/");

    let session_mgr = SessionManager::new(
        SessionManagerConfig {
            port_pool_config: PortPoolConfig::new(60900, 61000).unwrap(),
            ..Default::default()
        },
        None,
    );
    let bridge_mgr = Arc::new(MediaBridgeManager::new());
    let media = Arc::new(MediaSetup::new(
        Arc::clone(&session_mgr),
        bridge_mgr,
        "192.168.1.10",
    ));
    let registry = CallRegistry::new();
    let acceptor = BridgingAcceptor::new(media, BridgeDefaults::default(), registry.clone())
        .with_call_id_factory(Arc::new(|| BridgeCallId::new("siphon-null-webhook")));

    let routes = one_route(&ws_url);
    let route = routes.iter().next().unwrap();
    let req = invite(
        LINPHONE_PCMU_OFFER,
        "sip:5000@siphon.example.com",
        "abc-null@pbx",
    );
    let facts = InviteFacts::extract(&req);
    let prepared = acceptor
        .prepare_call(&req, route, &facts)
        .await
        .expect("prepare");

    let run_handle = acceptor.run_call(prepared, "webhook_route");
    tokio::time::sleep(Duration::from_millis(150)).await;
    let h = registry.lookup("abc-null@pbx").expect("registered");
    h.shutdown();
    tokio::time::timeout(Duration::from_secs(3), run_handle)
        .await
        .expect("run_call completes")
        .expect("task does not panic");
}
