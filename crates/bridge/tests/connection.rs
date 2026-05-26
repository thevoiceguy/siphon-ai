//! Integration tests for `BridgeConn::connect_and_run` against an
//! in-process WebSocket server.
//!
//! The test server is a thin tokio-tungstenite accept loop. It captures
//! the upgrade request's headers for assertion, runs a configurable
//! script (echo, send-then-echo, etc.), and exposes channels so the
//! test can drive server-side behavior.

use std::sync::Arc;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use parking_lot::Mutex;
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_tungstenite::accept_hdr_async;
use tokio_tungstenite::tungstenite::handshake::server::{
    ErrorResponse, Request as HsRequest, Response as HsResponse,
};
use tokio_tungstenite::tungstenite::http::{HeaderValue, StatusCode};
use tokio_tungstenite::tungstenite::Message as WsMessage;

use siphon_ai_bridge::protocol::{
    AudioEncoding, AudioFormat, BridgeIn, CallId, Direction, DtmfMethod, HangupCause, SipMeta,
    StartMsg, StopReason, WS_SUBPROTOCOL,
};
use siphon_ai_bridge::{
    connect_and_run, BridgeChannels, BridgeConfig, BridgeError, DisconnectReason, OutgoingEvent,
};

// ─── Test server ────────────────────────────────────────────────────────────

#[derive(Debug, Default, Clone)]
struct CapturedRequest {
    subprotocol: Option<String>,
    authorization: Option<String>,
    user_agent: Option<String>,
    siphon_call_id: Option<String>,
}

#[derive(Debug, Default)]
struct ServerOpts {
    echo_subprotocol: bool,
    require_auth: Option<String>,
}

impl ServerOpts {
    fn echoing() -> Self {
        Self {
            echo_subprotocol: true,
            require_auth: None,
        }
    }
}

struct ServerHandle {
    addr: std::net::SocketAddr,
    captured: Arc<Mutex<CapturedRequest>>,
    /// Test → server: messages the test wants the server to send to the client.
    server_send_tx: mpsc::UnboundedSender<WsMessage>,
    /// Server → test: text frames the server received from the client.
    client_text_rx: mpsc::UnboundedReceiver<String>,
    /// Holds the spawned task; dropped at the end of the test.
    _task: tokio::task::JoinHandle<()>,
}

impl ServerHandle {
    fn ws_url(&self) -> String {
        format!("ws://{}", self.addr)
    }
}

async fn spawn_server(opts: ServerOpts) -> ServerHandle {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local_addr");
    let captured = Arc::new(Mutex::new(CapturedRequest::default()));

    let (server_send_tx, mut server_send_rx) = mpsc::unbounded_channel::<WsMessage>();
    let (client_text_tx, client_text_rx) = mpsc::unbounded_channel::<String>();

    let captured_clone = Arc::clone(&captured);
    let task = tokio::spawn(async move {
        let (stream, _) = match listener.accept().await {
            Ok(p) => p,
            Err(_) => return,
        };

        let echo_subprotocol = opts.echo_subprotocol;
        let require_auth = opts.require_auth.clone();
        let captured_for_callback = Arc::clone(&captured_clone);

        // `ErrorResponse` (the tungstenite handshake-rejection type) is
        // ~136 bytes; rust-1.95 clippy's `result_large_err` flags
        // closures returning it. The shape is dictated by the
        // tungstenite handshake-callback signature, so just allow
        // here (same pattern as crates/core/tests/common/mod.rs).
        #[allow(clippy::result_large_err)]
        let callback =
            move |req: &HsRequest, mut resp: HsResponse| -> Result<HsResponse, ErrorResponse> {
                // Capture interesting headers for assertion.
                let mut c = captured_for_callback.lock();
                c.subprotocol = req
                    .headers()
                    .get("sec-websocket-protocol")
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_string);
                c.authorization = req
                    .headers()
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_string);
                c.user_agent = req
                    .headers()
                    .get("user-agent")
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_string);
                c.siphon_call_id = req
                    .headers()
                    .get("x-siphon-call-id")
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_string);
                drop(c);

                // Auth gate.
                if let Some(expected) = &require_auth {
                    let actual = req
                        .headers()
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("");
                    if actual != format!("Bearer {expected}") {
                        let body = HsResponse::builder()
                            .status(StatusCode::UNAUTHORIZED)
                            .body(Some("Unauthorized".to_string()))
                            .unwrap();
                        return Err(body);
                    }
                }

                if echo_subprotocol {
                    resp.headers_mut().insert(
                        "Sec-WebSocket-Protocol",
                        HeaderValue::from_static(WS_SUBPROTOCOL),
                    );
                }
                Ok(resp)
            };

        let ws = match accept_hdr_async(stream, callback).await {
            Ok(w) => w,
            Err(_) => return, // upgrade rejected; nothing more to do
        };

        let (mut sink, mut stream) = ws.split();

        loop {
            tokio::select! {
                outbound = server_send_rx.recv() => {
                    match outbound {
                        Some(msg) => {
                            if sink.send(msg).await.is_err() {
                                break;
                            }
                        }
                        None => break,
                    }
                }
                incoming = stream.next() => {
                    match incoming {
                        Some(Ok(WsMessage::Text(t))) => {
                            let _ = client_text_tx.send(t);
                        }
                        Some(Ok(WsMessage::Binary(b))) => {
                            if sink.send(WsMessage::Binary(b)).await.is_err() {
                                break;
                            }
                        }
                        Some(Ok(WsMessage::Ping(p))) => {
                            let _ = sink.send(WsMessage::Pong(p)).await;
                        }
                        Some(Ok(WsMessage::Close(frame))) => {
                            let _ = sink.send(WsMessage::Close(frame)).await;
                            break;
                        }
                        Some(Ok(_)) => {}
                        Some(Err(_)) | None => break,
                    }
                }
            }
        }
    });

    ServerHandle {
        addr,
        captured,
        server_send_tx,
        client_text_rx,
        _task: task,
    }
}

// ─── Helpers ────────────────────────────────────────────────────────────────

fn fixture_start(call_id: &str) -> StartMsg {
    StartMsg {
        version: "1".into(),
        call_id: CallId::new(call_id),
        seq: 0,
        from: "+1".into(),
        to: "5000".into(),
        direction: Direction::Inbound,
        audio: AudioFormat {
            encoding: AudioEncoding::Pcm16le,
            sample_rate: 8000,
            channels: 1,
            frame_ms: 20,
        },
        sip: SipMeta {
            call_id: "x@y".into(),
            headers: Default::default(),
        },
        srtp: None,
    }
}

#[allow(clippy::type_complexity)]
fn fixture_channels() -> (
    BridgeChannels,
    mpsc::Sender<Vec<u8>>,
    mpsc::Sender<OutgoingEvent>,
    mpsc::Receiver<Vec<u8>>,
    mpsc::Receiver<BridgeIn>,
) {
    let (audio_out_tx, audio_out_rx) = mpsc::channel(10);
    let (control_out_tx, control_out_rx) = mpsc::channel(10);
    let (audio_in_tx, audio_in_rx) = mpsc::channel(10);
    let (control_in_tx, control_in_rx) = mpsc::channel(10);
    let chans = BridgeChannels {
        audio_out_rx,
        control_out_rx,
        audio_in_tx,
        control_in_tx,
    };
    (
        chans,
        audio_out_tx,
        control_out_tx,
        audio_in_rx,
        control_in_rx,
    )
}

fn fixture_config(url: String) -> BridgeConfig {
    BridgeConfig {
        ws_url: url,
        auth_header: None,
        connect_timeout: Duration::from_secs(2),
        tls: None,
    }
}

// ─── Tests ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn upgrade_carries_subprotocol_user_agent_and_call_id() {
    let server = spawn_server(ServerOpts::echoing()).await;
    let (chans, _audio_out, control_out, _audio_in, _control_in) = fixture_channels();

    let conn = tokio::spawn(connect_and_run(
        fixture_config(server.ws_url()),
        fixture_start("siphon-test"),
        chans,
    ));

    // Give the handshake time to complete + start to be sent.
    tokio::time::sleep(Duration::from_millis(50)).await;

    let captured = server.captured.lock().clone();
    assert_eq!(captured.subprotocol.as_deref(), Some(WS_SUBPROTOCOL));
    assert_eq!(captured.siphon_call_id.as_deref(), Some("siphon-test"));
    assert!(
        captured
            .user_agent
            .as_deref()
            .unwrap_or("")
            .starts_with("siphon-ai/"),
        "User-Agent should start with siphon-ai/, got {:?}",
        captured.user_agent,
    );
    assert!(
        captured.authorization.is_none(),
        "no token configured → no Authorization"
    );

    // Tear down cleanly so the conn task returns.
    control_out
        .send(OutgoingEvent::Stop {
            reason: StopReason::CallerHangup,
        })
        .await
        .unwrap();
    let result = conn.await.unwrap().unwrap();
    assert_eq!(result, DisconnectReason::StopSent);
}

/// `BridgeConfig.auth_header` is sent verbatim, including the
/// scheme. Bare-token normalisation happens upstream in
/// `core::acceptor` and `config::compile`, NOT here.
#[tokio::test]
async fn auth_header_forwarded_verbatim_bearer() {
    let server = spawn_server(ServerOpts::echoing()).await;
    let (chans, _audio_out, control_out, _audio_in, _control_in) = fixture_channels();
    let mut cfg = fixture_config(server.ws_url());
    cfg.auth_header = Some("Bearer s3cret".into());

    let conn = tokio::spawn(connect_and_run(cfg, fixture_start("c"), chans));

    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        server.captured.lock().authorization.as_deref(),
        Some("Bearer s3cret"),
    );

    control_out
        .send(OutgoingEvent::Stop {
            reason: StopReason::ServerHangup,
        })
        .await
        .unwrap();
    let _ = conn.await.unwrap();
}

/// Regression for the auth-scheme bug: non-Bearer schemes were
/// previously double-prefixed (`Authorization: Bearer Basic …`).
/// Now the bridge sends the configured value untouched.
#[tokio::test]
async fn auth_header_forwarded_verbatim_basic() {
    let server = spawn_server(ServerOpts::echoing()).await;
    let (chans, _audio_out, control_out, _audio_in, _control_in) = fixture_channels();
    let mut cfg = fixture_config(server.ws_url());
    cfg.auth_header = Some("Basic dXNlcjpwYXNz".into());

    let conn = tokio::spawn(connect_and_run(cfg, fixture_start("c"), chans));

    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        server.captured.lock().authorization.as_deref(),
        Some("Basic dXNlcjpwYXNz"),
    );

    control_out
        .send(OutgoingEvent::Stop {
            reason: StopReason::ServerHangup,
        })
        .await
        .unwrap();
    let _ = conn.await.unwrap();
}

#[tokio::test]
async fn start_is_first_message_with_seq_zero() {
    let mut server = spawn_server(ServerOpts::echoing()).await;
    let (chans, _audio_out, control_out, _audio_in, _control_in) = fixture_channels();
    let conn = tokio::spawn(connect_and_run(
        fixture_config(server.ws_url()),
        fixture_start("c"),
        chans,
    ));

    let first_text = tokio::time::timeout(Duration::from_millis(200), server.client_text_rx.recv())
        .await
        .expect("server should receive a text frame")
        .expect("channel open");
    let parsed: serde_json::Value = serde_json::from_str(&first_text).unwrap();
    assert_eq!(parsed["type"], "start");
    assert_eq!(parsed["version"], "1");
    assert_eq!(parsed["call_id"], "c");
    assert_eq!(parsed["seq"], 0);

    control_out
        .send(OutgoingEvent::Stop {
            reason: StopReason::CallerHangup,
        })
        .await
        .unwrap();
    let _ = conn.await.unwrap();
}

#[tokio::test]
async fn audio_frames_round_trip_to_server_and_back() {
    let server = spawn_server(ServerOpts::echoing()).await;
    let (chans, audio_out, control_out, mut audio_in, _control_in) = fixture_channels();
    let conn = tokio::spawn(connect_and_run(
        fixture_config(server.ws_url()),
        fixture_start("c"),
        chans,
    ));

    // Wait for `start` so the server is in steady state.
    tokio::time::sleep(Duration::from_millis(50)).await;

    let frame = vec![0xAB, 0xCD, 0xEF, 0x01];
    audio_out.send(frame.clone()).await.unwrap();

    let echoed = tokio::time::timeout(Duration::from_millis(500), audio_in.recv())
        .await
        .expect("echo should arrive within 500ms")
        .expect("audio_in_rx open");
    assert_eq!(echoed, frame);

    control_out
        .send(OutgoingEvent::Stop {
            reason: StopReason::CallerHangup,
        })
        .await
        .unwrap();
    let _ = conn.await.unwrap();
}

#[tokio::test]
async fn outgoing_control_events_get_seq_stamped_in_order() {
    let mut server = spawn_server(ServerOpts::echoing()).await;
    let (chans, _audio_out, control_out, _audio_in, _control_in) = fixture_channels();
    let conn = tokio::spawn(connect_and_run(
        fixture_config(server.ws_url()),
        fixture_start("c"),
        chans,
    ));

    // Drain the start message.
    let _ = tokio::time::timeout(Duration::from_millis(200), server.client_text_rx.recv()).await;

    control_out
        .send(OutgoingEvent::Dtmf {
            digit: '1',
            duration_ms: 100,
            method: DtmfMethod::Rfc2833,
        })
        .await
        .unwrap();
    control_out
        .send(OutgoingEvent::Mark {
            name: "ack-1".into(),
        })
        .await
        .unwrap();

    let collect = async {
        let mut seqs = vec![];
        for _ in 0..2 {
            let text = server.client_text_rx.recv().await.unwrap();
            let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
            seqs.push(parsed["seq"].as_u64().unwrap());
        }
        seqs
    };
    let seqs = tokio::time::timeout(Duration::from_millis(500), collect)
        .await
        .expect("server should see both events");
    // Start was seq=0; subsequent events use 1, 2, ...
    assert_eq!(seqs, vec![1, 2]);

    control_out
        .send(OutgoingEvent::Stop {
            reason: StopReason::CallerHangup,
        })
        .await
        .unwrap();
    let _ = conn.await.unwrap();
}

#[tokio::test]
async fn server_sent_bridge_in_messages_are_parsed_and_dispatched() {
    let server = spawn_server(ServerOpts::echoing()).await;
    let (chans, _audio_out, control_out, _audio_in, mut control_in) = fixture_channels();
    let conn = tokio::spawn(connect_and_run(
        fixture_config(server.ws_url()),
        fixture_start("c"),
        chans,
    ));

    // Wait for connection.
    tokio::time::sleep(Duration::from_millis(50)).await;

    server
        .server_send_tx
        .send(WsMessage::Text(
            serde_json::json!({
                "type": "hangup",
                "call_id": "c",
                "cause": "busy"
            })
            .to_string(),
        ))
        .unwrap();

    let received = tokio::time::timeout(Duration::from_millis(500), control_in.recv())
        .await
        .expect("inbound control should arrive")
        .expect("control_in_rx open");
    assert!(matches!(
        received,
        BridgeIn::Hangup { ref call_id, cause: HangupCause::Busy } if call_id.as_str() == "c"
    ));

    control_out
        .send(OutgoingEvent::Stop {
            reason: StopReason::ServerHangup,
        })
        .await
        .unwrap();
    let _ = conn.await.unwrap();
}

#[tokio::test]
async fn mismatched_call_id_yields_error() {
    let server = spawn_server(ServerOpts::echoing()).await;
    let (chans, _audio_out, _control_out, _audio_in, _control_in) = fixture_channels();
    let conn = tokio::spawn(connect_and_run(
        fixture_config(server.ws_url()),
        fixture_start("expected-id"),
        chans,
    ));

    tokio::time::sleep(Duration::from_millis(50)).await;
    server
        .server_send_tx
        .send(WsMessage::Text(
            serde_json::json!({ "type": "clear", "call_id": "WRONG" }).to_string(),
        ))
        .unwrap();

    let result = tokio::time::timeout(Duration::from_secs(1), conn)
        .await
        .expect("conn task should return")
        .unwrap();
    match result {
        Err(BridgeError::CallIdMismatch { expected, got }) => {
            assert_eq!(expected, "expected-id");
            assert_eq!(got, "WRONG");
        }
        other => panic!("expected CallIdMismatch, got {other:?}"),
    }
}

#[tokio::test]
async fn malformed_json_from_server_is_a_protocol_error() {
    let server = spawn_server(ServerOpts::echoing()).await;
    let (chans, _audio_out, _control_out, _audio_in, _control_in) = fixture_channels();
    let conn = tokio::spawn(connect_and_run(
        fixture_config(server.ws_url()),
        fixture_start("c"),
        chans,
    ));

    tokio::time::sleep(Duration::from_millis(50)).await;
    server
        .server_send_tx
        .send(WsMessage::Text("{ this is not valid json".into()))
        .unwrap();

    let result = tokio::time::timeout(Duration::from_secs(1), conn)
        .await
        .expect("conn task should return")
        .unwrap();
    assert!(
        matches!(result, Err(BridgeError::BadJson(_))),
        "got {result:?}"
    );
}

#[tokio::test]
async fn stop_event_returns_stop_sent() {
    let server = spawn_server(ServerOpts::echoing()).await;
    let (chans, _audio_out, control_out, _audio_in, _control_in) = fixture_channels();
    let conn = tokio::spawn(connect_and_run(
        fixture_config(server.ws_url()),
        fixture_start("c"),
        chans,
    ));

    tokio::time::sleep(Duration::from_millis(50)).await;
    control_out
        .send(OutgoingEvent::Stop {
            reason: StopReason::CallerHangup,
        })
        .await
        .unwrap();

    let result = tokio::time::timeout(Duration::from_secs(1), conn)
        .await
        .expect("conn returns")
        .unwrap()
        .expect("clean disconnect");
    assert_eq!(result, DisconnectReason::StopSent);
}

#[tokio::test]
async fn dropping_control_channel_yields_controller_hung_up() {
    let server = spawn_server(ServerOpts::echoing()).await;
    let (chans, audio_out, control_out, _audio_in, _control_in) = fixture_channels();
    let conn = tokio::spawn(connect_and_run(
        fixture_config(server.ws_url()),
        fixture_start("c"),
        chans,
    ));

    tokio::time::sleep(Duration::from_millis(50)).await;
    drop(control_out);
    drop(audio_out); // drop both so the conn select returns from control branch

    let result = tokio::time::timeout(Duration::from_secs(1), conn)
        .await
        .expect("conn returns")
        .unwrap()
        .expect("clean disconnect");
    assert_eq!(result, DisconnectReason::ControllerHungUp);
}

#[tokio::test]
async fn unsupported_url_returns_invalid_config_or_ws_error() {
    let (chans, _audio_out, _control_out, _audio_in, _control_in) = fixture_channels();
    let cfg = BridgeConfig {
        ws_url: "not-a-url".into(),
        auth_header: None,
        connect_timeout: Duration::from_millis(500),
        tls: None,
    };
    let result = connect_and_run(cfg, fixture_start("c"), chans).await;
    assert!(
        matches!(
            result,
            Err(BridgeError::WebSocket(_)) | Err(BridgeError::InvalidConfig(_))
        ),
        "got {result:?}"
    );
}

#[tokio::test]
async fn auth_failure_at_upgrade_time_propagates_as_websocket_error() {
    let server = spawn_server(ServerOpts {
        echo_subprotocol: true,
        require_auth: Some("expected".into()),
    })
    .await;
    let (chans, _audio_out, _control_out, _audio_in, _control_in) = fixture_channels();
    let mut cfg = fixture_config(server.ws_url());
    cfg.auth_header = Some("wrong-token".into());

    let result = connect_and_run(cfg, fixture_start("c"), chans).await;
    assert!(
        matches!(result, Err(BridgeError::WebSocket(_))),
        "expected WebSocket error from rejected upgrade, got {result:?}",
    );
}

#[tokio::test]
async fn dropped_audio_in_rx_disconnects_with_controller_hung_up() {
    let server = spawn_server(ServerOpts::echoing()).await;
    let (chans, audio_out, _control_out, audio_in, _control_in) = fixture_channels();
    drop(audio_in); // close the receiver before any audio arrives

    let conn = tokio::spawn(connect_and_run(
        fixture_config(server.ws_url()),
        fixture_start("c"),
        chans,
    ));

    tokio::time::sleep(Duration::from_millis(50)).await;
    audio_out.send(vec![1, 2, 3, 4]).await.unwrap();

    let result = tokio::time::timeout(Duration::from_secs(1), conn)
        .await
        .expect("conn returns")
        .unwrap()
        .expect("clean disconnect");
    assert_eq!(result, DisconnectReason::ControllerHungUp);
}
