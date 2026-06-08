//! End-to-end controller tests against an ephemeral WS server +
//! a real forge `MediaBridgeManager`.
//!
//! Each test stands up a tokio-tungstenite WS server on a random
//! port, plays a scripted role (close immediately, send Hangup,
//! …), and asserts the controller exits with the expected
//! [`CallTermination`] and sub-task results.
//!
//! These are integration tests — slower than unit tests but they
//! exercise the full plumbing: bridge handshake, JSON `start`,
//! channel wiring, tap pump cleanup, drain budget.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use forge_core::CallId as ForgeCallId;
use forge_engine::MediaBridgeManager;
use futures::{SinkExt, StreamExt};
use serde_json::Value;
use siphon_ai_bridge::{
    AudioEncoding, AudioFormat, BridgeConfig, CallId, Direction, DisconnectReason, SipMeta,
    StartMsg,
};
use siphon_ai_core::{CallController, CallControllerConfig, CallTermination};
use siphon_ai_media_glue::MediaTap;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::handshake::server::{
    ErrorResponse as HsErrorResponse, Request as HsRequest, Response as HsResponse,
};
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::Message;

/// Build a minimal valid `StartMsg` for the controller. The bridge
/// task overwrites `seq` to 0 regardless of input.
fn start_msg(call_id: &str) -> StartMsg {
    StartMsg {
        version: "1".into(),
        call_id: CallId::new(call_id),
        seq: 0,
        from: "+13125551234".into(),
        to: "5000".into(),
        direction: Direction::Inbound,
        audio: AudioFormat {
            encoding: AudioEncoding::Pcm16le,
            sample_rate: 8000,
            channels: 1,
            frame_ms: 20,
        },
        sip: SipMeta {
            call_id: "abc@pbx.example.com".into(),
            headers: HashMap::new(),
        },
        srtp: None,
        verstat: None,
    }
}

/// Start a one-shot WS server on an ephemeral port. The handler
/// is given the `WebSocketStream` for one accepted connection.
async fn one_shot_server<F, Fut>(handler: F) -> u16
where
    F: FnOnce(tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send,
{
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept");
        let ws = tokio_tungstenite::accept_hdr_async(stream, echo_subprotocol)
            .await
            .expect("accept_hdr_async");
        handler(ws).await;
    });
    port
}

/// Handshake callback: echo the `siphon-ai.v1` subprotocol back so
/// tungstenite's client doesn't reject the upgrade with
/// `NoSubProtocol`. The bridge code tolerates a missing echo
/// itself but tungstenite enforces stricter than the spec.
#[allow(clippy::result_large_err)] // tungstenite's callback signature
fn echo_subprotocol(
    _req: &HsRequest,
    mut response: HsResponse,
) -> Result<HsResponse, HsErrorResponse> {
    response.headers_mut().insert(
        "sec-websocket-protocol",
        HeaderValue::from_static("siphon-ai.v1"),
    );
    Ok(response)
}

fn make_controller(port: u16, call_id: &str) -> (CallController, siphon_ai_core::CallHandle) {
    let manager = Arc::new(MediaBridgeManager::new());
    let tap = MediaTap::attach(
        &manager,
        &::std::sync::Arc::new(forge_core::EventBus::new()),
        ForgeCallId::new(call_id),
        8000,
    )
    .expect("attach tap");
    // Keep the manager alive for the duration of the call by
    // leaking it intentionally — in production the daemon owns the
    // manager. Tests deliberately don't tear it down between calls.
    Box::leak(Box::new(manager));

    let cfg = CallControllerConfig {
        call_id: CallId::new(call_id),
        bridge: BridgeConfig {
            ws_url: format!("ws://127.0.0.1:{port}/"),
            auth_header: None,
            connect_timeout: Duration::from_secs(2),
            tls: None,
        },
        start: start_msg(call_id),
        media_tap: tap,
        transfer: None,
        recording: None,
    };
    CallController::new(cfg)
}

#[tokio::test]
async fn server_closes_after_start_yields_bridge_ended() {
    let port = one_shot_server(|mut ws| async move {
        // Read the start message, verify it's well-formed, then close.
        let msg = ws.next().await.expect("recv start").expect("ws ok");
        let text = match msg {
            Message::Text(t) => t,
            other => panic!("expected text, got {other:?}"),
        };
        let v: Value = serde_json::from_str(&text).expect("start is JSON");
        assert_eq!(v["type"], "start");
        assert_eq!(v["call_id"], "test-1");
        assert_eq!(v["seq"], 0);

        ws.close(None).await.ok();
    })
    .await;

    let (controller, _handle) = make_controller(port, "test-1");
    let outcome = controller.run().await.expect("run");
    assert_eq!(outcome.termination, CallTermination::BridgeEnded);
    let bridge = outcome.bridge.expect("bridge result");
    assert!(
        matches!(bridge, Ok(DisconnectReason::ServerClosed)),
        "expected ServerClosed, got {bridge:?}"
    );
}

#[tokio::test]
async fn server_hangup_yields_server_hangup_termination() {
    let port = one_shot_server(|mut ws| async move {
        // Read start.
        let _ = ws.next().await;

        // Send a Hangup with the right call_id.
        let hangup = serde_json::json!({
            "type": "hangup",
            "call_id": "test-2",
            "cause": "normal"
        });
        ws.send(Message::Text(hangup.to_string())).await.unwrap();

        // Expect SiphonAI to reply with a `stop` message and close.
        // We just drain whatever comes until close.
        while let Some(Ok(msg)) = ws.next().await {
            if matches!(msg, Message::Close(_)) {
                break;
            }
        }
        ws.close(None).await.ok();
    })
    .await;

    let (controller, _handle) = make_controller(port, "test-2");
    let outcome = controller.run().await.expect("run");
    assert_eq!(outcome.termination, CallTermination::ServerHangup);
}

#[tokio::test]
async fn external_shutdown_yields_local_shutdown() {
    let port = one_shot_server(|mut ws| async move {
        // Accept start, then sit until close.
        let _ = ws.next().await;
        while let Some(msg) = ws.next().await {
            if let Ok(Message::Close(_)) = msg {
                break;
            }
        }
        ws.close(None).await.ok();
    })
    .await;

    let (controller, handle) = make_controller(port, "test-3");
    let run_task = tokio::spawn(controller.run());

    // Give the bridge a moment to handshake + send start.
    tokio::time::sleep(Duration::from_millis(50)).await;
    handle.shutdown();

    let outcome = run_task.await.expect("join").expect("run");
    assert_eq!(outcome.termination, CallTermination::LocalShutdown);
}

#[tokio::test]
async fn unreachable_server_yields_bridge_error() {
    // Bind a port and immediately drop the listener — anything
    // connecting there gets a fresh connection refused.
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().unwrap().port();
    drop(listener);

    let (controller, _handle) = make_controller(port, "test-4");
    let outcome = controller.run().await.expect("run");
    assert_eq!(outcome.termination, CallTermination::BridgeEnded);
    assert!(matches!(outcome.bridge, Some(Err(_))));
}

#[tokio::test]
async fn transfer_without_uac_emits_error() {
    // No IntegratedUAC installed (transfer = None on the config).
    // BridgeIn::Transfer must surface as a BridgeOut::Error with
    // code = transfer_failed instead of silently dropping.
    let port = one_shot_server(|mut ws| async move {
        // Drain start.
        let _ = ws.next().await;
        // Ask for a transfer.
        let xfer = serde_json::json!({
            "type": "transfer",
            "call_id": "test-5",
            "target": "sip:bob@example.com"
        });
        ws.send(Message::Text(xfer.to_string())).await.unwrap();

        // We expect an `error` message back, then keep the
        // connection open so the controller doesn't tear down.
        let saw_error = loop {
            match ws.next().await {
                Some(Ok(Message::Text(t))) => {
                    let v: Value = serde_json::from_str(&t).expect("json");
                    if v["type"] == "error" {
                        assert_eq!(v["code"], "transfer_failed");
                        break true;
                    }
                }
                Some(Ok(Message::Binary(_))) => {} // ignore audio
                _ => break false,
            }
        };
        assert!(saw_error, "did not observe transfer_failed error");

        // Politely close so the controller exits.
        ws.close(None).await.ok();
    })
    .await;

    let (controller, _handle) = make_controller(port, "test-5");
    let outcome = controller.run().await.expect("run");
    // Server closed the WS after the error; controller's bridge
    // task is the first to exit.
    assert_eq!(outcome.termination, CallTermination::BridgeEnded);
}
