//! A browser-shaped peer dialing our media leg, in one process
//! (`DEV_PLAN_WebRTC.md` §5, item 1).
//!
//! This is the plan's workhorse: no browser, no SIPp, no container —
//! just two forge-webrtc peers on loopback, one of them the real
//! [`WebRtcLeg`] a browser call gets. It covers the whole media plane
//! that a fixture-based test cannot: ICE nomination, the DTLS
//! handshake, SRTP in both directions, the Opus transcode, and
//! teardown.
//!
//! # Why the offer is spliced
//!
//! v1 does no SIP trickle (§4.2), so a browser sends its INVITE only
//! after gathering — its offer already carries `a=candidate:` lines.
//! forge-webrtc builds an offer from whatever it has gathered *at that
//! moment*, and gathering starts when the offer is created, so the
//! first offer is always candidate-less. [`browser_offer`] waits for
//! gathering and splices the candidates in, which is exactly the SDP
//! Chrome puts on the wire — and the shape `WebRtcLeg::answer` is
//! built to consume.

use std::time::Duration;

use forge_core::AudioCodec;
use forge_webrtc::{ConnectionState, PeerConnection, PeerEvent};
use siphon_ai_webrtc_glue::{
    Answered, InboundAudio, LegPhase, OutboundAudio, SetupOutcome, WebRtcLeg, WebRtcLegState,
    WebRtcSettings,
};
use tokio::sync::mpsc;

/// How long a loopback connect may take before the test gives up.
/// Host candidates on 127.0.0.1 nominate in milliseconds; this is a
/// CI-runner allowance, not an expectation.
const CONNECT_BUDGET: Duration = Duration::from_secs(10);

/// The far side of the call: a peer that behaves the way a browser
/// does — gather fully, then offer.
struct Browser {
    peer: PeerConnection,
    events: mpsc::Receiver<PeerEvent>,
    offer: String,
}

/// Build a browser-shaped offer: create it, wait for gathering, and
/// splice the gathered candidates into the SDP (see the module docs).
async fn browser_offer() -> Browser {
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
    assert!(
        !candidates.is_empty(),
        "a loopback peer must gather at least one host candidate"
    );

    Browser {
        peer,
        events,
        offer: format!("{offer}{}", candidates.concat()),
    }
}

/// Drive both sides until each reports `Connected`, or fail loudly
/// with the states they got stuck in.
async fn connect(leg: &WebRtcLeg, browser: &PeerConnection) {
    let deadline = tokio::time::Instant::now() + CONNECT_BUDGET;
    loop {
        if leg.state() == ConnectionState::Connected
            && browser.get_state() == ConnectionState::Connected
        {
            return;
        }
        assert_ne!(leg.state(), ConnectionState::Failed, "leg failed");
        assert_ne!(
            browser.get_state(),
            ConnectionState::Failed,
            "browser failed"
        );
        assert!(
            tokio::time::Instant::now() < deadline,
            "not connected inside {CONNECT_BUDGET:?}: leg={:?} browser={:?}",
            leg.state(),
            browser.get_state()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// A 20 ms PCM16LE frame of a 440 Hz tone at `rate`, loud enough that
/// "did audio survive the codec" is answerable by amplitude.
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

/// Peak absolute sample of a PCM16LE frame.
fn peak(frame: &[u8]) -> i32 {
    frame
        .chunks_exact(2)
        .map(|b| (i16::from_le_bytes([b[0], b[1]]) as i32).abs())
        .max()
        .unwrap_or(0)
}

/// The whole media plane in one test: a browser-shaped peer offers,
/// our leg answers, ICE and DTLS complete, and audio survives the
/// round trip in both directions.
#[tokio::test]
async fn a_browser_shaped_peer_completes_a_call_and_audio_flows_both_ways() {
    let mut browser = browser_offer().await;

    // Our side, exactly as the acceptor builds it (port 0 = OS-chosen;
    // the pool's port is §4.4's concern, not this test's).
    let Answered {
        leg,
        answer_sdp,
        mut events,
    } = WebRtcLeg::answer(&browser.offer, &WebRtcSettings::default(), 0)
        .await
        .expect("leg answers a browser-shaped offer");
    assert!(
        answer_sdp.contains("a=candidate:"),
        "complete-gathering-before-answer: {answer_sdp}"
    );

    browser
        .peer
        .set_remote_answer(&answer_sdp)
        .await
        .expect("browser accepts our answer");

    // The tap's own setup wait, driven exactly as `WebRtcTap::run`
    // drives it — including the phase it publishes for the admin API.
    let state = WebRtcLegState::default();
    let outcome = leg
        .wait_for_setup(&mut events, CONNECT_BUDGET, &state)
        .await;
    assert!(
        matches!(outcome, SetupOutcome::Connected { .. }),
        "setup did not complete: {outcome:?}"
    );
    assert_eq!(state.phase(), LegPhase::Connected);
    connect(&leg, &browser.peer).await;

    // Opus by default on both sides → the bridge sees 16 kHz.
    let (codec, payload_type) = leg.codec();
    assert_eq!(codec, AudioCodec::Opus);
    let rate = leg.bridge_sample_rate();
    assert_eq!(rate, 16_000);

    // Browser → leg. The browser encodes with the same helper the leg
    // uses, which is the point: one framing contract, both directions.
    let mut browser_out =
        OutboundAudio::new(codec, browser.peer.sender().expect("browser sender"), rate)
            .expect("browser encoder");
    let mut leg_in = InboundAudio::new(codec, payload_type, rate).expect("leg decoder");

    for seq in 0..25u32 {
        browser_out
            .send_frame(&tone_frame(rate, seq))
            .await
            .expect("browser sends");
    }

    // Collect what arrives, draining on the same 20 ms cadence the tap
    // uses — the jitter buffer only releases a packet once it has been
    // held for its target depth.
    let mut heard = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while heard.len() < 10 && tokio::time::Instant::now() < deadline {
        tokio::select! {
            ev = events.recv() => match ev {
                Some(PeerEvent::Rtp(pkt)) => leg_in.push(&pkt),
                Some(PeerEvent::Failed(why)) => panic!("leg transport failed: {why}"),
                None => break,
                _ => {}
            },
            _ = tokio::time::sleep(Duration::from_millis(20)) => {
                heard.extend(leg_in.drain());
            }
        }
    }
    assert!(
        heard.len() >= 10,
        "expected the browser's audio to reach the bridge; got {} frames \
         (decode_errors={}, other_payload={})",
        heard.len(),
        leg_in.decode_errors,
        leg_in.other_payload_packets
    );
    assert_eq!(heard[0].len(), (rate / 50) as usize * 2, "20 ms PCM16LE");
    assert_eq!(leg_in.decode_errors, 0);
    assert!(
        heard.iter().any(|f| peak(f) > 2_000),
        "the tone came through the Opus round trip as near-silence"
    );

    // Leg → browser, the same assertion mirrored.
    let mut leg_out = OutboundAudio::new(codec, leg.peer().sender().expect("leg sender"), rate)
        .expect("leg encoder");
    let mut browser_in = InboundAudio::new(codec, payload_type, rate).expect("browser decoder");
    for seq in 0..25u32 {
        leg_out
            .send_frame(&tone_frame(rate, seq))
            .await
            .expect("leg sends");
    }

    let mut browser_heard = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while browser_heard.len() < 10 && tokio::time::Instant::now() < deadline {
        tokio::select! {
            ev = browser.events.recv() => match ev {
                Some(PeerEvent::Rtp(pkt)) => browser_in.push(&pkt),
                Some(PeerEvent::Failed(why)) => panic!("browser transport failed: {why}"),
                None => break,
                _ => {}
            },
            _ = tokio::time::sleep(Duration::from_millis(20)) => {
                browser_heard.extend(browser_in.drain());
            }
        }
    }
    assert!(
        browser_heard.len() >= 10,
        "expected our audio to reach the browser; got {} frames",
        browser_heard.len()
    );
    assert_eq!(browser_in.decode_errors, 0);
    assert!(
        browser_heard.iter().any(|f| peak(f) > 2_000),
        "the tone we sent reached the browser as near-silence"
    );
    assert_eq!(leg_out.encode_errors, 0);
    assert!(
        leg_out.encode_nanos > 0 && leg_in.decode_nanos > 0,
        "the transcode cost §4.6 reports must be measured on a real call"
    );
}

/// **Why a browser leg needs the inactivity watchdog** (§4.6).
///
/// A page that closes sends no BYE, and — as this test pins — nothing
/// in-band either: forge-webrtc sends RFC 7675 consent keepalives but
/// never fails a transport when the replies stop, so the far side's
/// disappearance produces *no event on our leg at all*. Silence is the
/// only signal, which is why the tap runs
/// `[media].inactivity_timeout_secs` on a browser call exactly as it
/// does on a SIP one, and why the metric for it is
/// `siphon_ai_webrtc_legs_ended_total{reason="inactivity"}`.
///
/// This test therefore asserts a *gap*. If it ever fails because an
/// ending arrived, that is good news and an action item: forge-webrtc
/// learned to detect consent failure, so give it its own end reason
/// and delete this test.
#[tokio::test]
async fn a_vanished_browser_is_silent_not_signalled() {
    let mut browser = browser_offer().await;
    let Answered {
        leg,
        answer_sdp,
        mut events,
    } = WebRtcLeg::answer(&browser.offer, &WebRtcSettings::default(), 0)
        .await
        .expect("answer");
    browser
        .peer
        .set_remote_answer(&answer_sdp)
        .await
        .expect("answer applied");
    let state = WebRtcLegState::default();
    assert!(matches!(
        leg.wait_for_setup(&mut events, CONNECT_BUDGET, &state)
            .await,
        SetupOutcome::Connected { .. }
    ));

    // The tab closes.
    browser.peer.close();
    drop(browser);

    let watch = Duration::from_secs(2);
    let deadline = tokio::time::Instant::now() + watch;
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(200), events.recv()).await {
            Ok(Some(PeerEvent::Closed)) | Ok(Some(PeerEvent::Failed(_))) | Ok(None) => panic!(
                "the leg was told the browser went away — forge-webrtc now \
                 detects this. Give it its own `legs_ended_total` reason \
                 (it is a better signal than the inactivity watchdog) and \
                 delete this test."
            ),
            Ok(Some(PeerEvent::Rtp(_))) => {
                panic!("media kept arriving from a closed peer")
            }
            Ok(Some(_)) | Err(_) => continue,
        }
    }
    // Nothing arrived, and the leg still believes it is connected —
    // the state an unwatched browser call would sit in forever.
    assert_eq!(state.phase(), LegPhase::Connected);
    assert_eq!(leg.state(), ConnectionState::Connected);
}

/// Setup, media and teardown repeated back to back. A leak in any of
/// them — a socket, a task, a jitter buffer that never drains — shows
/// up as a later iteration failing, which a single-call test cannot
/// see. Deliberately modest (the load harness runs the hundreds the
/// plan asks for); this is the per-commit tripwire.
#[tokio::test]
async fn repeated_calls_do_not_degrade() {
    for i in 0..15u32 {
        let mut browser = browser_offer().await;
        let Answered {
            leg,
            answer_sdp,
            mut events,
        } = WebRtcLeg::answer(&browser.offer, &WebRtcSettings::default(), 0)
            .await
            .unwrap_or_else(|e| panic!("call {i} could not be answered: {e}"));
        browser
            .peer
            .set_remote_answer(&answer_sdp)
            .await
            .unwrap_or_else(|e| panic!("call {i} answer rejected: {e}"));

        let state = WebRtcLegState::default();
        let outcome = leg
            .wait_for_setup(&mut events, CONNECT_BUDGET, &state)
            .await;
        assert!(
            matches!(outcome, SetupOutcome::Connected { .. }),
            "call {i} never connected: {outcome:?}"
        );

        // One frame each way is enough to prove SRTP is keyed; the
        // volume assertions live in the test above.
        let (codec, payload_type) = leg.codec();
        let rate = leg.bridge_sample_rate();
        let mut out =
            OutboundAudio::new(codec, browser.peer.sender().expect("sender"), rate).expect("enc");
        let mut inb = InboundAudio::new(codec, payload_type, rate).expect("dec");
        out.send_frame(&tone_frame(rate, 0)).await.expect("send");
        let mut got = false;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        while !got && tokio::time::Instant::now() < deadline {
            if let Ok(Some(PeerEvent::Rtp(pkt))) =
                tokio::time::timeout(Duration::from_millis(100), events.recv()).await
            {
                inb.push(&pkt);
                got = true;
            }
        }
        assert!(got, "call {i} carried no media");

        // Teardown is a drop on both sides — the shape the tap and the
        // acceptor rely on (§4.4: the port reservation releases on
        // `Drop`, so nothing may depend on an explicit close).
        drop(leg);
        drop(browser);
    }
}
