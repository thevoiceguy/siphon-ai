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
use siphon_ai_core::{
    CallController, CallControllerConfig, CallTermination, ConferenceLimits, ConferenceRegistry,
    ParkContext, ParkRegistry, ParkSettings, ParkTimeoutAction,
};
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
        retrieved: false,
        reconnected: false,
        trace_context: None,
        barge_in_mode: None,
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

/// Like [`one_shot_server`] but accepts **two** connections on the same
/// port in sequence — the original session (`h1`) and the reconnect
/// redial (`h2`). Used by the WS-reconnect tests (0.7.3).
async fn two_shot_server<F1, Fut1, F2, Fut2>(h1: F1, h2: F2) -> u16
where
    F1: FnOnce(tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>) -> Fut1 + Send + 'static,
    Fut1: std::future::Future<Output = ()> + Send,
    F2: FnOnce(tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>) -> Fut2 + Send + 'static,
    Fut2: std::future::Future<Output = ()> + Send,
{
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let (s1, _) = listener.accept().await.expect("accept 1");
        let ws1 = tokio_tungstenite::accept_hdr_async(s1, echo_subprotocol)
            .await
            .expect("handshake 1");
        h1(ws1).await;

        let (s2, _) = listener.accept().await.expect("accept 2 (redial)");
        let ws2 = tokio_tungstenite::accept_hdr_async(s2, echo_subprotocol)
            .await
            .expect("handshake 2");
        h2(ws2).await;
    });
    port
}

/// Controller with WS reconnect enabled and a caller-chosen window.
fn make_controller_reconnect(
    port: u16,
    call_id: &str,
    window: Duration,
) -> (CallController, siphon_ai_core::CallHandle) {
    let manager = Arc::new(MediaBridgeManager::new());
    let tap = MediaTap::attach(
        &manager,
        &::std::sync::Arc::new(forge_core::EventBus::new()),
        ForgeCallId::new(call_id),
        8000,
    )
    .expect("attach tap")
    .with_survive_ws_drop(true);
    Box::leak(Box::new(manager));
    let cfg = CallControllerConfig {
        call_id: CallId::new(call_id),
        bridge: BridgeConfig {
            ws_url: format!("ws://127.0.0.1:{port}/"),
            auth_header: None,
            connect_timeout: Duration::from_secs(2),
            tls: None,
            ..Default::default()
        },
        start: start_msg(call_id),
        media_tap: tap,
        transfer: None,
        recording: None,
        conference: None,
        park: None,
        hold: None,
        ws_reconnect_enabled: true,
        ws_reconnect_max: window,
        ws_reconnect_moh_file: None,
        ws_failure_prompt: None,
    };
    CallController::new(cfg)
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
    make_controller_full(port, call_id, None, None)
}

fn make_controller_with_conference(
    port: u16,
    call_id: &str,
    conference: Option<ConferenceRegistry>,
) -> (CallController, siphon_ai_core::CallHandle) {
    make_controller_full(port, call_id, conference, None)
}

fn make_controller_with_park(
    port: u16,
    call_id: &str,
    park: ParkContext,
) -> (CallController, siphon_ai_core::CallHandle) {
    make_controller_full(port, call_id, None, Some(park))
}

fn make_controller_full(
    port: u16,
    call_id: &str,
    conference: Option<ConferenceRegistry>,
    park: Option<ParkContext>,
) -> (CallController, siphon_ai_core::CallHandle) {
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
            ..Default::default()
        },
        start: start_msg(call_id),
        media_tap: tap,
        transfer: None,
        recording: None,
        conference,
        park,
        hold: None,
        ws_reconnect_enabled: false,
        ws_reconnect_max: std::time::Duration::from_secs(30),
        ws_reconnect_moh_file: None,
        ws_failure_prompt: None,
    };
    CallController::new(cfg)
}

/// Controller whose tap arms the RTP inactivity watchdog with a short
/// window — used to drive the `rtp_timeout` path (0.13.x). No real RTP
/// peer feeds the tap, so the watchdog fires after `timeout`.
fn make_controller_inactivity(
    port: u16,
    call_id: &str,
    timeout: Duration,
) -> (CallController, siphon_ai_core::CallHandle) {
    let manager = Arc::new(MediaBridgeManager::new());
    let tap = MediaTap::attach(
        &manager,
        &::std::sync::Arc::new(forge_core::EventBus::new()),
        ForgeCallId::new(call_id),
        8000,
    )
    .expect("attach tap")
    .with_inactivity_timeout(Some(timeout));
    Box::leak(Box::new(manager));
    let cfg = CallControllerConfig {
        call_id: CallId::new(call_id),
        bridge: BridgeConfig {
            ws_url: format!("ws://127.0.0.1:{port}/"),
            auth_header: None,
            connect_timeout: Duration::from_secs(2),
            tls: None,
            ..Default::default()
        },
        start: start_msg(call_id),
        media_tap: tap,
        transfer: None,
        recording: None,
        conference: None,
        park: None,
        hold: None,
        ws_reconnect_enabled: false,
        ws_reconnect_max: Duration::from_secs(30),
        ws_reconnect_moh_file: None,
        ws_failure_prompt: None,
    };
    CallController::new(cfg)
}

/// A park context for tests: an enabled registry, no MOH file (comfort
/// noise — no fixture needed), no webhook sink, and a caller-supplied
/// timeout policy.
fn test_park_ctx(timeout: Option<Duration>, action: ParkTimeoutAction) -> ParkContext {
    ParkContext {
        settings: ParkSettings {
            moh_file: None,
            timeout,
            timeout_action: action,
        },
        registry: ParkRegistry::new(8),
        webhooks: None,
    }
}

/// An enabled registry for the conference round-trip tests.
fn enabled_conference() -> ConferenceRegistry {
    ConferenceRegistry::new(ConferenceLimits {
        enabled: true,
        max_rooms: 8,
        max_participants_per_room: 8,
        join_tones: false,
    })
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
async fn ws_drop_reconnects_and_resumes() {
    // 0.7.3: an unexpected drop with reconnect enabled re-dials the same
    // ws_url. Conn 1 reads `start` then drops the socket; conn 2 (the
    // redial) MUST carry `start.reconnected = true` (seq 0), then ends
    // the call with a `hangup`.
    let port = two_shot_server(
        |mut ws| async move {
            let _ = ws.next().await; // original start
            drop(ws); // unexpected close — no stop/hangup
        },
        |mut ws| async move {
            let text = match ws.next().await.expect("recv start").expect("ws ok") {
                Message::Text(t) => t,
                other => panic!("expected text, got {other:?}"),
            };
            let v: Value = serde_json::from_str(&text).expect("start is JSON");
            assert_eq!(v["type"], "start");
            assert_eq!(v["call_id"], "recon-1");
            assert_eq!(v["seq"], 0);
            assert_eq!(v["reconnected"], true, "redial start must flag reconnected");
            ws.send(Message::Text(
                serde_json::json!({"type":"hangup","call_id":"recon-1","cause":"normal"})
                    .to_string(),
            ))
            .await
            .unwrap();
            while let Some(Ok(m)) = ws.next().await {
                if matches!(m, Message::Close(_)) {
                    break;
                }
            }
            ws.close(None).await.ok();
        },
    )
    .await;

    let (controller, _handle) = make_controller_reconnect(port, "recon-1", Duration::from_secs(10));
    let outcome = controller.run().await.expect("run");
    // The call survived the drop and ended via the resumed session's hangup.
    assert_eq!(outcome.termination, CallTermination::ServerHangup);
}

#[tokio::test]
async fn ws_reconnect_exhausts_and_tears_down() {
    // Conn 1 reads `start` then drops; the server never accepts again, so
    // every redial is refused. With a short window the controller gives
    // up and tears the call down (→ §5.7 ws_disconnect).
    let port = one_shot_server(|mut ws| async move {
        let _ = ws.next().await;
        drop(ws);
    })
    .await;

    let (controller, _handle) =
        make_controller_reconnect(port, "recon-2", Duration::from_millis(500));
    let outcome = controller.run().await.expect("run");
    assert_eq!(outcome.termination, CallTermination::BridgeEnded);
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

#[tokio::test]
async fn conference_join_without_registry_emits_error() {
    // Conferencing disabled (conference = None): a `conference_join`
    // must surface as `error { code: conference_failed }`, not a
    // silent drop. The call continues.
    let port = one_shot_server(|mut ws| async move {
        let _ = ws.next().await; // drain start
        let join = serde_json::json!({
            "type": "conference_join",
            "call_id": "conf-1",
            "room_id": "support-7"
        });
        ws.send(Message::Text(join.to_string())).await.unwrap();

        let saw_error = loop {
            match ws.next().await {
                Some(Ok(Message::Text(t))) => {
                    let v: Value = serde_json::from_str(&t).expect("json");
                    if v["type"] == "error" {
                        assert_eq!(v["code"], "conference_failed");
                        break true;
                    }
                }
                Some(Ok(Message::Binary(_))) => {}
                _ => break false,
            }
        };
        assert!(saw_error, "did not observe conference_failed error");
        ws.close(None).await.ok();
    })
    .await;

    let (controller, _handle) = make_controller_with_conference(port, "conf-1", None);
    let outcome = controller.run().await.expect("run");
    assert_eq!(outcome.termination, CallTermination::BridgeEnded);
}

#[tokio::test]
async fn conference_join_then_leave_round_trip() {
    // With an enabled registry: `conference_join` → `conference_joined`
    // (participants = 1, this call alone in a fresh room), then
    // `conference_leave` → `conference_left { reason: "left" }`.
    let port = one_shot_server(|mut ws| async move {
        let _ = ws.next().await; // drain start

        ws.send(Message::Text(
            serde_json::json!({
                "type": "conference_join",
                "call_id": "conf-2",
                "room_id": "support-7"
            })
            .to_string(),
        ))
        .await
        .unwrap();

        // Wait for conference_joined.
        let joined = loop {
            match ws.next().await {
                Some(Ok(Message::Text(t))) => {
                    let v: Value = serde_json::from_str(&t).expect("json");
                    if v["type"] == "conference_joined" {
                        break v;
                    }
                    assert_ne!(v["type"], "error", "join failed: {v}");
                }
                Some(Ok(Message::Binary(_))) => {}
                other => panic!("ws closed before conference_joined: {other:?}"),
            }
        };
        assert_eq!(joined["room_id"], "support-7");
        assert_eq!(joined["participants"], 1);

        // Now leave.
        ws.send(Message::Text(
            serde_json::json!({ "type": "conference_leave", "call_id": "conf-2" }).to_string(),
        ))
        .await
        .unwrap();

        let left = loop {
            match ws.next().await {
                Some(Ok(Message::Text(t))) => {
                    let v: Value = serde_json::from_str(&t).expect("json");
                    if v["type"] == "conference_left" {
                        break v;
                    }
                }
                Some(Ok(Message::Binary(_))) => {}
                other => panic!("ws closed before conference_left: {other:?}"),
            }
        };
        assert_eq!(left["room_id"], "support-7");
        assert_eq!(left["reason"], "left");

        ws.close(None).await.ok();
    })
    .await;

    let (controller, _handle) =
        make_controller_with_conference(port, "conf-2", Some(enabled_conference()));
    let outcome = controller.run().await.expect("run");
    assert_eq!(outcome.termination, CallTermination::BridgeEnded);
}

// ─── Park / retrieve (0.7.0) ─────────────────────────────────────────

#[tokio::test]
async fn park_then_retrieve_round_trip() {
    // Park detaches the WS (the bridge sends `stop{park}` and closes)
    // while the controller stays alive on MOH; retrieve opens a *fresh*
    // WS that receives `start{retrieved:true}`. Two one-shot servers
    // model the pre-park and post-retrieve sessions; the call ends
    // normally when the retrieved session closes.
    let (a_done_tx, a_done_rx) = tokio::sync::oneshot::channel::<bool>();
    let port_a = one_shot_server(move |mut ws| async move {
        let _ = ws.next().await; // drain start
                                 // Read until the bridge closes the WS for the park; record
                                 // whether we saw the `stop{park}` first.
        let mut saw_park_stop = false;
        while let Some(msg) = ws.next().await {
            if let Ok(Message::Text(t)) = msg {
                let v: Value = serde_json::from_str(&t).expect("json");
                if v["type"] == "stop" && v["reason"] == "park" {
                    saw_park_stop = true;
                }
            }
        }
        let _ = a_done_tx.send(saw_park_stop);
    })
    .await;

    let (b_seen_tx, b_seen_rx) = tokio::sync::oneshot::channel::<bool>();
    let port_b = one_shot_server(move |mut ws| async move {
        // The retrieved session's `start` must carry `retrieved: true`.
        let retrieved = match ws.next().await {
            Some(Ok(Message::Text(t))) => {
                let v: Value = serde_json::from_str(&t).expect("json");
                assert_eq!(v["type"], "start");
                v["retrieved"] == serde_json::json!(true)
            }
            other => panic!("retrieved session sent no start: {other:?}"),
        };
        let _ = b_seen_tx.send(retrieved);
        ws.close(None).await.ok();
    })
    .await;

    let (controller, handle) = make_controller_with_park(
        port_a,
        "park-1",
        test_park_ctx(None, ParkTimeoutAction::Hangup),
    );
    let run = tokio::spawn(controller.run());

    // Park, then wait until server A has actually seen the park stop +
    // close before retrieving — deterministic ordering, no sleeps.
    handle.request_park(Some("lot-1".into()));
    assert!(
        a_done_rx.await.expect("server A signalled"),
        "pre-park session should have observed stop{{park}}"
    );

    handle.request_retrieve(Some(format!("ws://127.0.0.1:{port_b}/")));
    assert!(
        b_seen_rx.await.expect("server B signalled"),
        "retrieved start must set retrieved=true"
    );

    let outcome = run.await.expect("join").expect("run");
    assert_eq!(outcome.termination, CallTermination::BridgeEnded);
    let park = outcome.park.expect("park summary present");
    assert_eq!(park.count, 1, "one park episode");
}

#[tokio::test]
async fn park_timeout_hangup_tears_down() {
    // A parked call with `timeout_action = hangup` tears down when the
    // deadline fires (no retrieve, no caller BYE) → `LocalShutdown`.
    //
    // Timing note: this harness has no real RTP feeding forge, so the
    // tap's inbound stream ends on its own (`CallEnded`) ~tens of ms
    // into a sustained park. We use a near-immediate deadline so the
    // park-timeout arm — which precedes the tap arm in the controller's
    // `biased` select — fires first, deterministically. A production
    // call has continuous RTP, so the tap never ends mid-park and any
    // real `timeout_secs` applies.
    let port = one_shot_server(|mut ws| async move {
        let _ = ws.next().await; // drain start
        while ws.next().await.is_some() {} // until park closes the WS
    })
    .await;

    let (controller, handle) = make_controller_with_park(
        port,
        "park-timeout",
        test_park_ctx(Some(Duration::from_millis(1)), ParkTimeoutAction::Hangup),
    );
    let run = tokio::spawn(controller.run());

    handle.request_park(None);

    let outcome = run.await.expect("join").expect("run");
    assert_eq!(outcome.termination, CallTermination::LocalShutdown);
    assert_eq!(outcome.park.expect("park summary").count, 1);
}

#[tokio::test]
async fn caller_bye_while_parked_tears_down() {
    // A caller BYE (modelled as handle.shutdown()) while the call is
    // parked tears it down cleanly, with the park episode still
    // accounted on the outcome.
    let (a_done_tx, a_done_rx) = tokio::sync::oneshot::channel::<()>();
    let port = one_shot_server(move |mut ws| async move {
        let _ = ws.next().await; // drain start
        while ws.next().await.is_some() {} // until park closes the WS
        let _ = a_done_tx.send(());
    })
    .await;

    let (controller, handle) = make_controller_with_park(
        port,
        "park-bye",
        test_park_ctx(None, ParkTimeoutAction::Hangup),
    );
    let run = tokio::spawn(controller.run());

    handle.request_park(None);
    a_done_rx.await.expect("park detached the WS");
    // Now the SIP side hangs up while parked.
    handle.shutdown();

    let outcome = run.await.expect("join").expect("run");
    assert_eq!(outcome.termination, CallTermination::LocalShutdown);
    assert_eq!(outcome.park.expect("park summary").count, 1);
}

#[tokio::test]
async fn rtp_inactivity_emits_rtp_timeout_then_stop() {
    // Server reads frames and forwards text to a channel; it never sends
    // hangup or closes itself, so the ONLY thing that can end the call is
    // the tap's RTP inactivity watchdog → `rtp_timeout`.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let port = one_shot_server(move |mut ws| async move {
        while let Some(Ok(msg)) = ws.next().await {
            if let Message::Text(t) = msg {
                let _ = tx.send(t);
            }
        }
    })
    .await;

    let (controller, _handle) =
        make_controller_inactivity(port, "rtp-1", Duration::from_millis(200));
    let outcome = controller.run().await.expect("run");
    assert_eq!(outcome.termination, CallTermination::TapEnded);

    // Before the close, the server must have been told why: error{rtp_timeout}
    // followed by stop (§3.10 fatal invariant).
    let mut saw_rtp_timeout = false;
    let mut saw_stop = false;
    while let Ok(Some(text)) = tokio::time::timeout(Duration::from_millis(500), rx.recv()).await {
        if text.contains("\"rtp_timeout\"") {
            saw_rtp_timeout = true;
        }
        if text.contains("\"type\":\"stop\"") {
            saw_stop = true;
        }
    }
    assert!(
        saw_rtp_timeout,
        "server should receive error{{rtp_timeout}}"
    );
    assert!(saw_stop, "a fatal error must be followed by stop");
}

// ─── WS-failure prompt (0.34.0, DESIGN_WS_FAILURE_PROMPT.md) ─────────

/// Minimal mono PCM16 WAV of constant-valued samples, so prompt frames
/// are identifiable on the forge side.
fn write_const_wav(path: &std::path::Path, rate: u32, n_samples: usize, value: i16) {
    let data_len = (n_samples * 2) as u32;
    let mut b = Vec::new();
    b.extend_from_slice(b"RIFF");
    b.extend_from_slice(&(36 + data_len).to_le_bytes());
    b.extend_from_slice(b"WAVEfmt ");
    b.extend_from_slice(&16u32.to_le_bytes());
    b.extend_from_slice(&1u16.to_le_bytes());
    b.extend_from_slice(&1u16.to_le_bytes());
    b.extend_from_slice(&rate.to_le_bytes());
    b.extend_from_slice(&(rate * 2).to_le_bytes());
    b.extend_from_slice(&2u16.to_le_bytes());
    b.extend_from_slice(&16u16.to_le_bytes());
    b.extend_from_slice(b"data");
    b.extend_from_slice(&data_len.to_le_bytes());
    for _ in 0..n_samples {
        b.extend_from_slice(&value.to_le_bytes());
    }
    std::fs::write(path, b).unwrap();
}

/// Controller with `on_ws_failure = play_prompt` semantics: the tap in
/// survive-WS-drop mode + the prompt path on the config. Returns the
/// forge manager so the test can observe the prompt frames.
fn make_controller_prompt(
    port: u16,
    call_id: &str,
    prompt: std::path::PathBuf,
) -> (
    CallController,
    siphon_ai_core::CallHandle,
    Arc<MediaBridgeManager>,
) {
    let manager = Arc::new(MediaBridgeManager::new());
    let tap = MediaTap::attach(
        &manager,
        &::std::sync::Arc::new(forge_core::EventBus::new()),
        ForgeCallId::new(call_id),
        8000,
    )
    .expect("attach tap")
    .with_survive_ws_drop(true);
    let cfg = CallControllerConfig {
        call_id: CallId::new(call_id),
        bridge: BridgeConfig {
            ws_url: format!("ws://127.0.0.1:{port}/"),
            auth_header: None,
            connect_timeout: Duration::from_secs(2),
            tls: None,
            ..Default::default()
        },
        start: start_msg(call_id),
        media_tap: tap,
        transfer: None,
        recording: None,
        conference: None,
        park: None,
        hold: None,
        ws_reconnect_enabled: false,
        ws_reconnect_max: Duration::from_secs(30),
        ws_reconnect_moh_file: None,
        ws_failure_prompt: Some(prompt),
    };
    let (controller, handle) = CallController::new(cfg);
    (controller, handle, manager)
}

#[tokio::test]
async fn ws_drop_with_play_prompt_plays_then_tears_down() {
    // Server reads `start` then drops the socket — an unexpected,
    // siphon-initiated-teardown failure. With play_prompt the caller
    // hears the prompt (observed as constant-sample frames reaching
    // forge) before the controller completes the normal teardown.
    let port = one_shot_server(|mut ws| async move {
        let _ = ws.next().await;
        drop(ws);
    })
    .await;

    let dir = std::env::temp_dir().join(format!("siphon_wsp_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let wav = dir.join("failure.wav");
    write_const_wav(&wav, 8000, 8000 / 2, 3000); // 0.5 s prompt

    let (controller, _handle, manager) = make_controller_prompt(port, "wsp-1", wav);
    let call = ForgeCallId::new("wsp-1");

    let watcher = {
        let manager = Arc::clone(&manager);
        tokio::spawn(async move {
            // True once a prompt frame (all samples == 3000) hits forge.
            loop {
                if let Some(forge_engine::OutboundMediaRequest::Audio(f)) =
                    manager.try_recv_outbound_request(&call).await
                {
                    if !f.samples.is_empty() && f.samples.iter().all(|&s| s == 3000) {
                        return true;
                    }
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
    };

    let outcome = controller.run().await.expect("run");
    assert_eq!(outcome.termination, CallTermination::BridgeEnded);
    let bridge = outcome.bridge.expect("bridge result");
    // An abrupt drop surfaces as ServerClosed or a raw IO/protocol
    // error depending on how the socket died — both are prompt-eligible
    // failures, and neither is rewritten by the prompt.
    assert!(
        matches!(bridge, Ok(DisconnectReason::ServerClosed) | Err(_)),
        "cause unchanged by the prompt, got {bridge:?}",
    );
    let saw_prompt = tokio::time::timeout(Duration::from_secs(1), watcher)
        .await
        .expect("prompt frames observed before teardown finished")
        .expect("watcher join");
    assert!(saw_prompt);

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn ws_drop_with_unusable_prompt_falls_open_to_hangup() {
    // The prompt file vanished after config load (or is garbage): the
    // call must fall open to today's immediate teardown — never wedge.
    let port = one_shot_server(|mut ws| async move {
        let _ = ws.next().await;
        drop(ws);
    })
    .await;

    let (controller, _handle, _manager) = make_controller_prompt(
        port,
        "wsp-2",
        std::path::PathBuf::from("/nonexistent/failure.wav"),
    );
    let started = std::time::Instant::now();
    let outcome = controller.run().await.expect("run");
    assert_eq!(outcome.termination, CallTermination::BridgeEnded);
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "fail-open must not wait on any prompt machinery",
    );
}
