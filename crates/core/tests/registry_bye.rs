//! End-to-end: BYE → registry lookup → CallHandle::shutdown wakes
//! a running CallController.
//!
//! Pulls together the wires the BYE plumbing layer adds:
//! - `CallRegistry` impls `DialogTerminator`
//! - `dispatch_bye` calls `terminate(call_id)`
//! - `terminate` looks up the handle and calls `shutdown()`
//! - `shutdown()` wakes the controller's main `select!`
//! - the controller exits with [`CallTermination::LocalShutdown`]
//!
//! Without this test, each piece works in isolation but nothing
//! verifies the chain.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use forge_core::CallId as ForgeCallId;
use forge_engine::MediaBridgeManager;
use futures::{SinkExt, StreamExt};
use serde_json::Value;
use sip_core::{Headers, Method, Request, RequestLine, SipUri};
use siphon_ai_bridge::{
    AudioEncoding, AudioFormat, BridgeConfig, CallId as BridgeCallId, Direction, SipMeta, StartMsg,
};
use siphon_ai_core::{CallController, CallControllerConfig, CallRegistry, CallTermination};
use siphon_ai_media_glue::MediaTap;
use siphon_ai_sip_glue::{dispatch_bye, DialogAction};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

mod common;
use common::{echo_subprotocol, server_acks_start_then_idles};

/// Same as [`server_acks_start_then_idles`] but captures the `reason`
/// field of the controller's `stop` event and reports it back. Used
/// to verify PROTOCOL.md §6: BYE → `caller_hangup`, WS hangup →
/// `server_hangup`.
async fn server_capture_stop_reason(
    port_tx: tokio::sync::oneshot::Sender<u16>,
    reason_tx: tokio::sync::oneshot::Sender<String>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().unwrap().port();
    let _ = port_tx.send(port);
    let (stream, _) = listener.accept().await.expect("accept");
    let mut ws = tokio_tungstenite::accept_hdr_async(stream, echo_subprotocol)
        .await
        .expect("ws accept");

    let mut reason_tx = Some(reason_tx);
    while let Some(msg) = ws.next().await {
        match msg {
            Ok(Message::Text(t)) => {
                let v: Value = serde_json::from_str(&t).unwrap();
                if v["type"] == "stop" {
                    if let Some(tx) = reason_tx.take() {
                        let r = v["reason"].as_str().unwrap_or("").to_string();
                        let _ = tx.send(r);
                    }
                    let _ = ws.send(Message::Close(None)).await;
                    break;
                }
            }
            Ok(Message::Close(_)) | Err(_) => break,
            _ => {}
        }
    }
}

fn bye_request(call_id: &str) -> Request {
    let uri = SipUri::parse("sip:5000@siphon.example.com").unwrap();
    let mut h = Headers::new();
    h.push("Via", "SIP/2.0/UDP h:5060;branch=z9hG4bK-bye")
        .unwrap();
    h.push("From", "<sip:caller@example.net>;tag=t").unwrap();
    h.push("To", "<sip:5000@siphon.example.com>;tag=u").unwrap();
    h.push("Call-ID", call_id).unwrap();
    h.push("CSeq", "2 BYE").unwrap();
    h.push("Content-Length", "0").unwrap();
    Request::new(RequestLine::new(Method::Bye, uri), h, Bytes::new()).unwrap()
}

#[tokio::test]
async fn dispatch_bye_wakes_running_controller_via_registry() {
    // 1. WS server that ACKs the start and waits for stop.
    let (port_tx, port_rx) = tokio::sync::oneshot::channel();
    tokio::spawn(server_acks_start_then_idles(port_tx));
    let port = port_rx.await.expect("server announces port");

    // 2. Build a controller around a real MediaTap + WS bridge.
    let manager = Arc::new(MediaBridgeManager::new());
    let forge_call_id = ForgeCallId::new("siphon-bye-test");
    let tap = MediaTap::attach(
        &manager,
        &::std::sync::Arc::new(forge_core::EventBus::new()),
        forge_call_id,
        8000,
    )
    .expect("attach");
    let cfg = CallControllerConfig {
        call_id: BridgeCallId::new("siphon-bye-test"),
        bridge: BridgeConfig {
            ws_url: format!("ws://127.0.0.1:{port}/"),
            connect_timeout: Duration::from_secs(2),
            ..Default::default()
        },
        start: StartMsg {
            version: "1".into(),
            call_id: BridgeCallId::new("siphon-bye-test"),
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
                call_id: "abc-123@pbx.example.com".into(),
                headers: HashMap::new(),
            },
            srtp: None,
            verstat: None,
            retrieved: false,
            reconnected: false,
            trace_context: None,
        },
        media_tap: tap,
        transfer: None,
        recording: None,
        conference: None,
        park: None,
        hold: None,
        ws_reconnect_enabled: false,
        ws_reconnect_max: std::time::Duration::from_secs(30),
        ws_reconnect_moh_file: None,
    };
    let (controller, handle) = CallController::new(cfg);

    // 3. Register and start the controller.
    let registry = CallRegistry::new();
    registry.insert(
        "abc-123@pbx.example.com",
        siphon_ai_core::registry::CallEntry::new(handle, None),
    );

    let run = tokio::spawn(async move { controller.run().await });

    // Give the WS bridge a moment to handshake and send `start`.
    tokio::time::sleep(Duration::from_millis(100)).await;

    // 4. Synthesize a BYE and dispatch it through the same path
    //    `RoutingHandler::on_bye` would: registry-as-DialogTerminator.
    let bye = bye_request("abc-123@pbx.example.com");
    let action = dispatch_bye(&registry, &bye);
    match action {
        DialogAction::SendFinal(resp) => {
            assert_eq!(resp.code(), 200);
            assert_eq!(resp.reason(), "OK");
        }
    }

    // 5. The controller must wake and end with LocalShutdown.
    let outcome = tokio::time::timeout(Duration::from_secs(3), run)
        .await
        .expect("controller exits after BYE")
        .expect("task didn't panic")
        .expect("controller returns Ok");
    assert_eq!(outcome.termination, CallTermination::LocalShutdown);
}

#[tokio::test]
async fn bye_drives_wire_stop_with_caller_hangup_reason() {
    // PROTOCOL.md §6: `caller_hangup` means the SIP peer sent BYE.
    // The shutdown.notified() arm in call.rs reads
    // `handle.remote_bye_received()` to disambiguate; this end-to-
    // end check goes from BYE → registry → controller exit and
    // observes the `reason` field on the WS `stop` frame.
    let (port_tx, port_rx) = tokio::sync::oneshot::channel();
    let (reason_tx, reason_rx) = tokio::sync::oneshot::channel();
    tokio::spawn(server_capture_stop_reason(port_tx, reason_tx));
    let port = port_rx.await.expect("server announces port");

    let manager = Arc::new(MediaBridgeManager::new());
    let forge_call_id = ForgeCallId::new("siphon-bye-reason");
    let tap = MediaTap::attach(
        &manager,
        &::std::sync::Arc::new(forge_core::EventBus::new()),
        forge_call_id,
        8000,
    )
    .expect("attach");
    let cfg = CallControllerConfig {
        call_id: BridgeCallId::new("siphon-bye-reason"),
        bridge: BridgeConfig {
            ws_url: format!("ws://127.0.0.1:{port}/"),
            connect_timeout: Duration::from_secs(2),
            ..Default::default()
        },
        start: StartMsg {
            version: "1".into(),
            call_id: BridgeCallId::new("siphon-bye-reason"),
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
                call_id: "bye-reason@pbx".into(),
                headers: HashMap::new(),
            },
            srtp: None,
            verstat: None,
            retrieved: false,
            reconnected: false,
            trace_context: None,
        },
        media_tap: tap,
        transfer: None,
        recording: None,
        conference: None,
        park: None,
        hold: None,
        ws_reconnect_enabled: false,
        ws_reconnect_max: std::time::Duration::from_secs(30),
        ws_reconnect_moh_file: None,
    };
    let (controller, handle) = CallController::new(cfg);

    let registry = CallRegistry::new();
    registry.insert(
        "bye-reason@pbx",
        siphon_ai_core::registry::CallEntry::new(handle, None),
    );

    let run = tokio::spawn(async move { controller.run().await });
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Dispatch via the BYE path — `terminate_from_bye` flips
    // `remote_bye` on the handle before signalling shutdown.
    let bye = bye_request("bye-reason@pbx");
    let _ = dispatch_bye(&registry, &bye);

    let outcome = tokio::time::timeout(Duration::from_secs(3), run)
        .await
        .expect("controller exits after BYE")
        .expect("task didn't panic")
        .expect("controller returns Ok");
    assert_eq!(outcome.termination, CallTermination::LocalShutdown);

    let reason = tokio::time::timeout(Duration::from_secs(2), reason_rx)
        .await
        .expect("WS observed stop")
        .expect("server sent reason");
    assert_eq!(
        reason, "caller_hangup",
        "BYE-driven teardown must surface as caller_hangup on the WS"
    );
}

#[tokio::test]
async fn dispatch_bye_for_unknown_call_id_does_not_panic() {
    // The registry has no entry; dispatch_bye still produces a 200
    // OK and returns false from the terminator. (The 200 OK is
    // mandatory per RFC 3261 §15.1.2.)
    let registry = CallRegistry::new();
    let bye = bye_request("ghost@pbx");
    match dispatch_bye(&registry, &bye) {
        DialogAction::SendFinal(resp) => assert_eq!(resp.code(), 200),
    }
    assert!(registry.is_empty());
}
