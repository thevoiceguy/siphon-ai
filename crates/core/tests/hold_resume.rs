//! Integration: hold/resume → WS `hold` and `resume` events.
//!
//! Covers the path that the unit tests on `prepare_reinvite_answer`
//! can't reach: the actual `OutgoingEvent` channel from
//! `CallHandle::push_bridge_event` through the bridge task and out
//! onto the WS as a JSON event. The acceptor's `on_reinvite` uses
//! `push_bridge_event` to emit `Hold` / `Resume` on direction
//! transitions; this test pins that wiring.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use forge_core::CallId as ForgeCallId;
use forge_engine::MediaBridgeManager;
use futures::{SinkExt, StreamExt};
use serde_json::Value;
use siphon_ai_bridge::{
    AudioEncoding, AudioFormat, BridgeConfig, CallId as BridgeCallId, Direction, OutgoingEvent,
    SipMeta, StartMsg,
};
use siphon_ai_core::{CallController, CallControllerConfig};
use siphon_ai_media_glue::MediaTap;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

mod common;
use common::echo_subprotocol;

/// WS server that captures every text frame's `type` + relevant
/// payload fields, reports them back over a channel, and closes
/// when a `stop` arrives. Used to assert event ordering and
/// payload shape on hold/resume.
async fn server_capture_events(
    port_tx: tokio::sync::oneshot::Sender<u16>,
    events_tx: tokio::sync::mpsc::UnboundedSender<Value>,
) {
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
                let v: Value = serde_json::from_str(&t).unwrap_or(Value::Null);
                let _ = events_tx.send(v.clone());
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

#[tokio::test]
async fn push_bridge_event_emits_hold_and_resume_on_ws() {
    // 1. WS server that captures every JSON event.
    let (port_tx, port_rx) = tokio::sync::oneshot::channel();
    let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(server_capture_events(port_tx, events_tx));
    let port = port_rx.await.expect("server announces port");

    // 2. Build a controller around a real MediaTap + WS bridge.
    let manager = Arc::new(MediaBridgeManager::new());
    let forge_call_id = ForgeCallId::new("siphon-hold-resume");
    let tap = MediaTap::attach(
        &manager,
        &Arc::new(forge_core::EventBus::new()),
        forge_call_id,
        8000,
    )
    .expect("attach");
    let cfg = CallControllerConfig {
        call_id: BridgeCallId::new("siphon-hold-resume"),
        bridge: BridgeConfig {
            ws_url: format!("ws://127.0.0.1:{port}/"),
            connect_timeout: Duration::from_secs(2),
            ..Default::default()
        },
        start: StartMsg {
            version: "1".into(),
            call_id: BridgeCallId::new("siphon-hold-resume"),
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
                call_id: "hold-resume@pbx".into(),
                headers: HashMap::new(),
            },
            srtp: None,
            verstat: None,
        },
        media_tap: tap,
        transfer: None,
        recording: None,
    };
    let (controller, handle) = CallController::new(cfg);
    let run = tokio::spawn(async move { controller.run().await });

    // 3. Wait for the `start` to land, then drive hold/resume from
    //    "outside" the controller exactly the way `on_reinvite` does.
    let start = tokio::time::timeout(Duration::from_secs(2), events_rx.recv())
        .await
        .expect("start event arrives")
        .expect("channel open");
    assert_eq!(start["type"], "start");

    handle.push_bridge_event(OutgoingEvent::Hold {
        direction: "sendonly".into(),
    });
    let hold = tokio::time::timeout(Duration::from_secs(2), events_rx.recv())
        .await
        .expect("hold event arrives")
        .expect("channel open");
    assert_eq!(hold["type"], "hold");
    assert_eq!(hold["direction"], "sendonly");

    handle.push_bridge_event(OutgoingEvent::Resume);
    let resume = tokio::time::timeout(Duration::from_secs(2), events_rx.recv())
        .await
        .expect("resume event arrives")
        .expect("channel open");
    assert_eq!(resume["type"], "resume");

    // 4. Drive cleanup so the controller exits.
    handle.shutdown();
    let _ = tokio::time::timeout(Duration::from_secs(3), run).await;
}
