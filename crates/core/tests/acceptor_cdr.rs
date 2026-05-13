//! End-to-end CDR emission: drive a call from `prepare_call` →
//! `run_call` against a real WS server and a recording CDR sink.
//! Confirms the spawned task emits exactly one CDR record with the
//! shape consumers will see.

use std::collections::HashMap;
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
use siphon_ai_cdr::{CdrRecord, CdrSink, Direction as CdrDirection, TerminationCause};
use siphon_ai_core::{BridgeDefaults, BridgingAcceptor, CallRegistry};
use siphon_ai_media_glue::MediaSetup;
use siphon_ai_routes::{load_from_toml, RouteSet};
use siphon_ai_sip_glue::InviteFacts;
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

/// WS server that ACKs the controller's `start` then politely
/// closes when it receives the `stop`. Returns the bound port via
/// the oneshot.
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

fn invite(body: &str, request_uri: &str) -> Request {
    let uri = SipUri::parse(request_uri).expect("uri");
    let line = RequestLine::new(Method::Invite, uri);
    let mut h = SipHeaders::new();
    h.push("Via", "SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-cdr")
        .unwrap();
    h.push("From", "<sip:+13125551234@carrier.example.net>;tag=abc")
        .unwrap();
    h.push("To", "<sip:5000@siphon.example.com>").unwrap();
    h.push("Call-ID", "abc-cdr@pbx.example.com").unwrap();
    h.push("CSeq", "1 INVITE").unwrap();
    h.push("User-Agent", "Cisco-CP8841").unwrap();
    h.push("Content-Type", "application/sdp").unwrap();
    h.push("Content-Length", body.len().to_string()).unwrap();
    Request::new(line, h, Bytes::from(body.as_bytes().to_vec())).unwrap()
}

fn one_route(ws_url: String) -> RouteSet {
    load_from_toml(&format!(
        r#"
        [[route]]
        name = "main_reception"
        [route.match]
        any = true
        [route.bridge]
        ws_url = "{ws_url}"
        "#,
    ))
    .expect("compile routes")
}

/// Recording sink that holds onto every emitted record.
#[derive(Default)]
struct Recorder {
    records: Mutex<Vec<CdrRecord>>,
}

#[async_trait]
impl CdrSink for Recorder {
    async fn emit(&self, record: CdrRecord) {
        self.records.lock().push(record);
    }
}

#[tokio::test]
async fn run_call_emits_cdr_when_controller_exits() {
    // Stand up a real WS server so the controller actually runs.
    let (port_tx, port_rx) = tokio::sync::oneshot::channel();
    tokio::spawn(server_acks_start_then_idles(port_tx));
    let port = port_rx.await.unwrap();
    let ws_url = format!("ws://127.0.0.1:{port}/");

    // Build the acceptor with a recording sink.
    let session_mgr = SessionManager::new(
        SessionManagerConfig {
            port_pool_config: PortPoolConfig::new(60100, 60200).unwrap(),
            ..Default::default()
        },
        None,
    );
    let bridge_mgr = Arc::new(MediaBridgeManager::new());
    let media = Arc::new(MediaSetup::new(
        Arc::clone(&session_mgr),
        bridge_mgr,
        Arc::new(forge_core::EventBus::new()),
        "192.168.1.10",
    ));
    let registry = CallRegistry::new();
    let recorder = Arc::new(Recorder::default());

    let acceptor = BridgingAcceptor::new(media, BridgeDefaults::default(), registry.clone())
        .with_call_id_factory(Arc::new(|| BridgeCallId::new("siphon-cdr-test")))
        .with_cdr_sink(Arc::clone(&recorder) as Arc<dyn CdrSink>);

    let routes = one_route(ws_url.clone());
    let route = routes.iter().next().unwrap();
    let req = invite(LINPHONE_PCMU_OFFER, "sip:5000@siphon.example.com");
    let facts = InviteFacts::extract(&req);

    let prepared = acceptor
        .prepare_call(&req, route, &facts)
        .await
        .expect("prepare");

    // Drive the call. Trigger the BYE-style shutdown a moment
    // later so the controller exits via LocalShutdown.
    let run_handle = acceptor.run_call(prepared, "main_reception", None);

    // Give the bridge time to handshake and send `start`.
    tokio::time::sleep(Duration::from_millis(150)).await;

    // Tell the controller to stop via the registry — same path BYE
    // uses.
    let h = registry
        .lookup("abc-cdr@pbx.example.com")
        .expect("registered");
    h.shutdown();

    // The spawned task ends after run_call returns; await it so we
    // know the CDR has been emitted before checking.
    tokio::time::timeout(Duration::from_secs(3), run_handle)
        .await
        .expect("run_call completes")
        .expect("task does not panic");

    let recs = recorder.records.lock();
    assert_eq!(recs.len(), 1, "expected exactly one CDR; got {recs:?}");
    let r = &recs[0];

    assert_eq!(r.version, 1);
    assert_eq!(r.call_id, "siphon-cdr-test");
    assert_eq!(r.sip_call_id, "abc-cdr@pbx.example.com");
    assert_eq!(r.from, "+13125551234");
    assert_eq!(r.to, "5000");
    assert_eq!(r.direction, CdrDirection::Inbound);
    assert_eq!(r.route, "main_reception");
    assert_eq!(r.ws_url, ws_url);
    assert_eq!(r.audio.codec, "PCMU");
    assert_eq!(r.audio.payload_type, 0);
    assert_eq!(r.audio.sample_rate, 8000);
    assert_eq!(r.termination.cause, TerminationCause::LocalShutdown);
    assert!(r.duration_ms < 5_000, "duration {} too long", r.duration_ms);
    assert!(r.ended_at >= r.started_at);

    // Registry was deregistered on the way out.
    assert!(registry.is_empty(), "registry should be empty after exit");
}

#[tokio::test]
async fn null_sink_is_the_default_when_no_sink_configured() {
    // Acceptor without with_cdr_sink should not panic when the
    // controller exits and an emit is attempted — the default
    // NullSink takes the record and drops it.
    let (port_tx, port_rx) = tokio::sync::oneshot::channel();
    tokio::spawn(server_acks_start_then_idles(port_tx));
    let port = port_rx.await.unwrap();
    let ws_url = format!("ws://127.0.0.1:{port}/");

    let session_mgr = SessionManager::new(
        SessionManagerConfig {
            port_pool_config: PortPoolConfig::new(60300, 60400).unwrap(),
            ..Default::default()
        },
        None,
    );
    let bridge_mgr = Arc::new(MediaBridgeManager::new());
    let media = Arc::new(MediaSetup::new(
        Arc::clone(&session_mgr),
        bridge_mgr,
        Arc::new(forge_core::EventBus::new()),
        "192.168.1.10",
    ));
    let registry = CallRegistry::new();
    let acceptor = BridgingAcceptor::new(media, BridgeDefaults::default(), registry.clone())
        .with_call_id_factory(Arc::new(|| BridgeCallId::new("siphon-null-cdr")));

    let routes = one_route(ws_url);
    let route = routes.iter().next().unwrap();
    let req = invite(LINPHONE_PCMU_OFFER, "sip:5000@siphon.example.com");
    let facts = InviteFacts::extract(&req);
    let prepared = acceptor
        .prepare_call(&req, route, &facts)
        .await
        .expect("prepare");

    let run_handle = acceptor.run_call(prepared, "main_reception", None);
    tokio::time::sleep(Duration::from_millis(150)).await;
    let h = registry
        .lookup("abc-cdr@pbx.example.com")
        .expect("registered");
    h.shutdown();
    tokio::time::timeout(Duration::from_secs(3), run_handle)
        .await
        .expect("run_call completes")
        .expect("task does not panic");
}

/// Helper: silence unused-imports for HashMap in environments where
/// the chain above doesn't already drag it in.
#[allow(dead_code)]
fn _hash_map_in_scope() -> HashMap<String, String> {
    HashMap::new()
}
