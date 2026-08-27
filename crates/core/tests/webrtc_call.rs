//! A browser call end to end, in one process (`DEV_PLAN_WebRTC.md`
//! §5, item 1) — the acceptance test the plan's Playwright leg was
//! going to provide, minus the browser.
//!
//! An INVITE carrying a real forge-webrtc offer goes into
//! `prepare_call`; the answer goes back into the offering peer; the
//! controller runs against a real WS server. Audio then makes the full
//! round trip — browser → SRTP → Opus decode → the WS bridge → the
//! server's echo → Opus encode → SRTP → browser — and the port pair
//! comes back when the call ends.
//!
//! Only compiled with `--features webrtc`, which CI now builds.

#![cfg(feature = "webrtc")]

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use forge_engine::{MediaBridgeManager, SessionManager, SessionManagerConfig};
use forge_rtp::PortPoolConfig;
use forge_webrtc::{ConnectionState, PeerConnection, PeerEvent};
use futures::{SinkExt, StreamExt};
use parking_lot::Mutex;
use siphon_ai_bridge::CallId as BridgeCallId;
use siphon_ai_core::{BridgeDefaults, BridgingAcceptor, CallRegistry};
use siphon_ai_media_glue::MediaSetup;
use siphon_ai_sip_glue::InviteFacts;
use siphon_ai_webrtc_glue::{InboundAudio, OutboundAudio, WebRtcSettings};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

mod common;
use common::{echo_subprotocol, invite, one_route};

/// What the WS server saw — the bridge's side of the call.
#[derive(Default)]
struct ServerSaw {
    start_seen: bool,
    audio_frames: u32,
    frame_len: usize,
    loudest: i32,
}

/// A WS server that echoes audio back, the shape
/// `examples/echo-ws-server-python` has: it proves the bridge carried
/// the browser's audio *and* gives us something to send back down the
/// same path.
async fn echo_ws_server(port_tx: tokio::sync::oneshot::Sender<u16>, saw: Arc<Mutex<ServerSaw>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let _ = port_tx.send(listener.local_addr().unwrap().port());
    let (stream, _) = listener.accept().await.expect("accept");
    let mut ws = tokio_tungstenite::accept_hdr_async(stream, echo_subprotocol)
        .await
        .expect("ws accept");
    while let Some(msg) = ws.next().await {
        match msg {
            Ok(Message::Text(t)) => {
                let v: serde_json::Value = serde_json::from_str(&t).unwrap();
                if v["type"] == "start" {
                    saw.lock().start_seen = true;
                }
                if v["type"] == "stop" {
                    let _ = ws.send(Message::Close(None)).await;
                    break;
                }
            }
            Ok(Message::Binary(pcm)) => {
                {
                    let mut s = saw.lock();
                    s.audio_frames += 1;
                    s.frame_len = pcm.len();
                    s.loudest = s.loudest.max(peak(&pcm));
                }
                // Echo it straight back: bridge → tap → SRTP → browser.
                if ws.send(Message::Binary(pcm)).await.is_err() {
                    break;
                }
            }
            Ok(Message::Close(_)) | Err(_) => break,
            _ => {}
        }
    }
}

fn peak(frame: &[u8]) -> i32 {
    frame
        .chunks_exact(2)
        .map(|b| (i16::from_le_bytes([b[0], b[1]]) as i32).abs())
        .max()
        .unwrap_or(0)
}

/// 20 ms of a 440 Hz tone as PCM16LE at `rate`.
fn tone_frame(rate: u32, seq: u32) -> Vec<u8> {
    let samples = (rate / 50) as usize;
    let mut out = Vec::with_capacity(samples * 2);
    for i in 0..samples {
        let t = (seq as usize * samples + i) as f32 / rate as f32;
        let v = (t * 440.0 * std::f32::consts::TAU).sin() * 12_000.0;
        out.extend_from_slice(&(v as i16).to_le_bytes());
    }
    out
}

/// A browser-shaped offer: created, gathered, then spliced — v1 does
/// no SIP trickle, so the INVITE must carry the candidates (§4.2).
async fn browser_offer() -> (PeerConnection, mpsc::Receiver<PeerEvent>, String) {
    let mut peer = PeerConnection::new(vec![]).await.expect("peer");
    let offer = peer.create_offer().await.expect("offer");
    let mut events = peer.take_events().expect("events");

    let mut candidates = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, events.recv()).await {
            Ok(Some(PeerEvent::LocalCandidate(c))) => {
                candidates.push(format!("a={}\r\n", c.to_sdp_attribute()))
            }
            Ok(Some(PeerEvent::GatheringComplete)) => break,
            Ok(Some(_)) => continue,
            Ok(None) | Err(_) => break,
        }
    }
    assert!(!candidates.is_empty(), "no host candidates gathered");
    let offer = format!("{offer}{}", candidates.concat());
    (peer, events, offer)
}

fn build_acceptor(
    lo: u16,
    hi: u16,
    call_id: &'static str,
) -> (BridgingAcceptor, Arc<SessionManager>, CallRegistry) {
    let session_mgr = SessionManager::new(
        SessionManagerConfig {
            port_pool_config: PortPoolConfig::new(lo, hi).unwrap(),
            ..Default::default()
        },
        None,
    );
    let media = Arc::new(MediaSetup::new(
        Arc::clone(&session_mgr),
        Arc::new(MediaBridgeManager::new()),
        Arc::new(forge_core::EventBus::new()),
        "127.0.0.1",
    ));
    let registry = CallRegistry::new();
    let acceptor = BridgingAcceptor::new(media, BridgeDefaults::default(), registry.clone())
        .with_call_id_factory(Arc::new(move || BridgeCallId::new(call_id)))
        .with_webrtc(Some(WebRtcSettings::default()));
    (acceptor, session_mgr, registry)
}

/// The whole path: INVITE → answer → ICE/DTLS → audio to the WS
/// server → echo → audio back to the browser → hangup → port returned.
#[tokio::test]
async fn a_browser_call_carries_audio_to_the_ws_server_and_back() {
    let saw = Arc::new(Mutex::new(ServerSaw::default()));
    let (port_tx, port_rx) = tokio::sync::oneshot::channel();
    tokio::spawn(echo_ws_server(port_tx, Arc::clone(&saw)));
    let ws_url = format!("ws://127.0.0.1:{}/", port_rx.await.unwrap());

    let (acceptor, session_mgr, registry) = build_acceptor(51500, 51700, "siphon-webrtc-e2e");
    let routes = one_route("browser", &ws_url);
    let route = routes.iter().next().unwrap();

    let (mut browser, mut browser_events, offer) = browser_offer().await;
    let req = invite(&offer, "sip:5000@siphon.example.com", "wss-call@browser");
    let facts = InviteFacts::extract(&req);

    let prepared = acceptor
        .prepare_call(&req, route, &facts, sip_transaction::TransportKind::Wss)
        .await
        .expect("a browser INVITE over WSS is accepted");
    assert!(prepared.is_webrtc, "leg selection picked the classic path");

    browser
        .set_remote_answer(&prepared.answer.answer_text)
        .await
        .expect("browser accepts the daemon's answer");

    let rate = prepared.answer.negotiated_audio_sample_rate;
    let payload_type = prepared.answer.negotiated_payload_type;
    let codec = forge_core::AudioCodec::Opus;
    assert_eq!(rate, 16_000, "Opus decodes to the bridge's 16 kHz");

    let run_handle = acceptor.run_call(prepared, "browser", None);

    // A browser starts sending when its peer connection is up, not
    // when the INVITE is answered — frames pushed before DTLS
    // completes have no SRTP context and are simply lost.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while browser.get_state() != ConnectionState::Connected {
        assert_ne!(
            browser.get_state(),
            ConnectionState::Failed,
            "the browser's transport failed before media"
        );
        assert!(
            tokio::time::Instant::now() < deadline,
            "browser never connected: {:?}",
            browser.get_state()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // The browser talks: 20 ms frames on a real cadence, because the
    // leg's jitter buffer releases on arrival time, not packet count.
    let sender = browser.sender().expect("browser sender");
    let mut out = OutboundAudio::new(codec, sender, rate).expect("encoder");
    let sent = Arc::new(AtomicU32::new(0));
    let sent_task = Arc::clone(&sent);
    let talker = tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_millis(20));
        for seq in 0..500u32 {
            ticker.tick().await;
            // A send error here is a lost frame, not a lost call —
            // exactly as on the real path, so keep talking.
            if out.send_frame(&tone_frame(rate, seq)).await.is_ok() {
                sent_task.fetch_add(1, Ordering::Relaxed);
            }
        }
    });

    // …and listens for the echo coming back down the same path.
    let mut inbound = InboundAudio::new(codec, payload_type, rate).expect("decoder");
    // Both halves of the round trip have to be *given time*, not
    // sampled once: the browser can hear its first echo back before
    // the server has counted ten frames, and asserting on the server's
    // count at that instant is a race.
    let enough =
        |heard: usize, saw: &Arc<Mutex<ServerSaw>>| heard >= 5 && saw.lock().audio_frames >= 10;
    let mut heard: Vec<Vec<u8>> = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while !enough(heard.len(), &saw) && tokio::time::Instant::now() < deadline {
        tokio::select! {
            ev = browser_events.recv() => match ev {
                Some(PeerEvent::Rtp(pkt)) => inbound.push(&pkt),
                Some(PeerEvent::Failed(why)) => panic!("browser transport failed: {why}"),
                None => break,
                _ => {}
            },
            _ = tokio::time::sleep(Duration::from_millis(20)) => {
                heard.extend(inbound.drain());
            }
        }
    }

    // The bridge saw the browser.
    {
        let s = saw.lock();
        assert!(s.start_seen, "the WS server never received `start`");
        assert!(
            s.audio_frames >= 10,
            "the browser's audio did not reach the WS bridge: {} frames",
            s.audio_frames
        );
        assert_eq!(
            s.frame_len,
            (rate / 50) as usize * 2,
            "frames must be exactly 20 ms of PCM16LE"
        );
        assert!(
            s.loudest > 2_000,
            "audio reached the bridge as near-silence (peak {})",
            s.loudest
        );
    }

    // And the browser heard the echo.
    assert!(
        heard.len() >= 5,
        "the server's audio did not reach the browser: {} frames",
        heard.len()
    );
    assert!(
        heard.iter().any(|f| peak(f) > 2_000),
        "the echo reached the browser as near-silence"
    );
    assert_eq!(inbound.decode_errors, 0);

    assert!(
        sent.load(Ordering::Relaxed) > 10,
        "the browser never got frames onto the wire"
    );

    // Hang up the way a BYE does, then prove the slot came back.
    registry
        .lookup("wss-call@browser")
        .expect("registered")
        .shutdown();
    talker.abort();
    tokio::time::timeout(Duration::from_secs(10), run_handle)
        .await
        .expect("the call tears down")
        .expect("the controller task does not panic");

    let (allocated, _available) = session_mgr.port_pool_stats().await;
    assert_eq!(
        allocated, 0,
        "the browser leg's port pair was not returned (§4.4)"
    );
    assert!(
        registry.is_empty(),
        "call left in the registry after teardown"
    );
}

/// The plan's acceptance test on this transport (§5): **kill the page
/// mid-call, and the slot comes back.**
///
/// A closed tab sends no BYE, and forge-webrtc reports nothing when
/// consent stops (see `webrtc-glue/tests/loopback.rs`), so silence is
/// the only signal there is. The tap's inactivity watchdog is what
/// turns that silence into a teardown — and a teardown is only real if
/// the RTP port pair comes back with it (§4.4).
#[tokio::test]
async fn a_browser_that_vanishes_gives_its_port_pair_back() {
    let saw = Arc::new(Mutex::new(ServerSaw::default()));
    let (port_tx, port_rx) = tokio::sync::oneshot::channel();
    tokio::spawn(echo_ws_server(port_tx, Arc::clone(&saw)));
    let ws_url = format!("ws://127.0.0.1:{}/", port_rx.await.unwrap());

    let (acceptor, session_mgr, registry) = build_acceptor(51800, 52000, "siphon-webrtc-vanish");
    // A two-second watchdog so the test asserts the mechanism rather
    // than the default's patience. `[route.media]`, the same knob a
    // deployment tunes per route.
    let routes = siphon_ai_routes::load_from_toml(&format!(
        r#"
        [[route]]
        name = "browser"
        [route.match]
        any = true
        [route.bridge]
        ws_url = "{ws_url}"
        [route.media]
        inactivity_timeout_secs = 2
        "#,
    ))
    .expect("routes");
    let route = routes.iter().next().unwrap();

    let (mut browser, _events, offer) = browser_offer().await;
    let req = invite(&offer, "sip:5000@siphon.example.com", "vanish@browser");
    let facts = InviteFacts::extract(&req);
    let prepared = acceptor
        .prepare_call(&req, route, &facts, sip_transaction::TransportKind::Wss)
        .await
        .expect("accepted");
    assert!(prepared.is_webrtc);
    let rate = prepared.answer.negotiated_audio_sample_rate;
    browser
        .set_remote_answer(&prepared.answer.answer_text)
        .await
        .expect("answer applied");

    let run_handle = acceptor.run_call(prepared, "browser", None);

    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while browser.get_state() != ConnectionState::Connected {
        assert!(
            tokio::time::Instant::now() < deadline,
            "browser never connected: {:?}",
            browser.get_state()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // Talk briefly, so the watchdog is measuring a *stopped* stream
    // rather than one that never started.
    let mut out = OutboundAudio::new(
        forge_core::AudioCodec::Opus,
        browser.sender().expect("sender"),
        rate,
    )
    .expect("encoder");
    for seq in 0..10u32 {
        let _ = out.send_frame(&tone_frame(rate, seq)).await;
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // The tab closes: no BYE, no FIN, nothing.
    browser.close();
    drop(browser);

    // The watchdog is the only thing that can end this call. Two
    // seconds of silence plus teardown; the budget is generous, the
    // assertion is that it happens at all.
    tokio::time::timeout(Duration::from_secs(20), run_handle)
        .await
        .expect("a vanished browser must not hold the call open")
        .expect("the controller task does not panic");

    let (allocated, _available) = session_mgr.port_pool_stats().await;
    assert_eq!(
        allocated, 0,
        "the vanished browser's port pair leaked — the exact bug Phase 0 exists to prevent"
    );
    assert!(registry.is_empty(), "call left in the registry");
}
