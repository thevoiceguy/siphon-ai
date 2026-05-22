//! Shared fixtures for the `siphon-ai-core` integration tests.
//!
//! Rust compiles every file in `tests/` as its own binary, so these
//! helpers were historically copy-pasted across `acceptor_metrics`,
//! `acceptor_webhooks`, `acceptor_cdr`, `hold_resume`, and
//! `registry_bye`. This module centralizes the ones that genuinely
//! repeated: the WS subprotocol handshake callback, the "ack start
//! then idle" WS server, the INVITE builder, the single-route
//! dialplan, and the SDP offer fixtures.
//!
//! Pulled in via `mod common;` per test file. `dead_code` is allowed
//! because no single test binary exercises every helper.
#![allow(dead_code)]

use bytes::Bytes;
use futures::{SinkExt, StreamExt};
use serde_json::Value;
use sip_core::{Headers as SipHeaders, Method, Request, RequestLine, SipUri};
use siphon_ai_routes::{load_from_toml, RouteSet};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::handshake::server::{
    ErrorResponse as HsErrorResponse, Request as HsRequest, Response as HsResponse,
};
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::Message;

/// A Linphone-style PCMU/PCMA offer with `telephone-event` — the
/// canonical inbound SDP for the acceptor integration tests.
pub const LINPHONE_PCMU_OFFER: &str = "v=0\r\n\
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

/// A G.729-only offer — no codec SiphonAI supports, used to exercise
/// the 488 reject path.
pub const G729_ONLY_OFFER: &str = "v=0\r\n\
o=- 1 1 IN IP4 10.0.0.5\r\n\
s=Talk\r\n\
c=IN IP4 10.0.0.5\r\n\
t=0 0\r\n\
m=audio 7000 RTP/AVP 18\r\n\
a=rtpmap:18 G729/8000\r\n\
a=sendrecv\r\n";

/// Handshake callback that echoes the `siphon-ai.v1` subprotocol so
/// tungstenite's stricter-than-spec client accepts the upgrade.
#[allow(clippy::result_large_err)]
pub fn echo_subprotocol(
    req: &HsRequest,
    mut resp: HsResponse,
) -> Result<HsResponse, HsErrorResponse> {
    if let Some(offered) = req.headers().get("Sec-WebSocket-Protocol") {
        resp.headers_mut().insert(
            "Sec-WebSocket-Protocol",
            HeaderValue::from_bytes(offered.as_bytes()).unwrap(),
        );
    }
    Ok(resp)
}

/// WS server that accepts one connection, ACKs the controller's
/// `start` by idling, and closes politely when it sees the `stop`.
/// Reports the bound port back over `port_tx`.
pub async fn server_acks_start_then_idles(port_tx: tokio::sync::oneshot::Sender<u16>) {
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

/// Build an inbound INVITE carrying `body` as an `application/sdp`
/// offer, addressed to `request_uri`, with the given `Call-ID`.
pub fn invite(body: &str, request_uri: &str, call_id: &str) -> Request {
    let uri = SipUri::parse(request_uri).expect("uri");
    let line = RequestLine::new(Method::Invite, uri);
    let mut h = SipHeaders::new();
    h.push("Via", "SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-test")
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

/// A single-route dialplan matching everything (`any = true`) and
/// bridging to `ws_url`. `name` is the route name the test asserts on.
pub fn one_route(name: &str, ws_url: &str) -> RouteSet {
    load_from_toml(&format!(
        r#"
        [[route]]
        name = "{name}"
        [route.match]
        any = true
        [route.bridge]
        ws_url = "{ws_url}"
        "#,
    ))
    .expect("compile routes")
}
