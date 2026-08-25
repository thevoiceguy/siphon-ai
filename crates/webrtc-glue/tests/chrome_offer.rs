//! forge-webrtc against a **real browser offer**.
//!
//! The plan's top risk (§7.1) is forge-webrtc's maturity against real
//! browsers rather than against itself. Its own loopback tests pair it
//! with a second forge-webrtc; these pair it with Chrome — the offer
//! is the one captured off the wire (via Homer) during the Phase 1
//! browser check, byte for byte.
//!
//! What this pins is the answer half of §4.2: forge-webrtc owns
//! offer/answer for this leg, so the answer it produces must be one
//! Chrome will accept — mirrored `mid`, the DTLS role that follows
//! `a=setup:actpass`, `rtcp-mux`, exactly one codec, and our own ICE
//! credentials and fingerprint.

use forge_webrtc::{AudioCodec, PeerConnection};
use siphon_ai_webrtc_glue::{inspect, WebRtcSettings};

const CHROME_OFFER: &str = include_str!("../fixtures/chrome-offer.sdp");

fn parse(sdp: &str) -> forge_sdp::SessionDescription {
    <forge_sdp::SessionDescription as forge_sdp::SessionDescriptionExt>::from_str(sdp)
        .expect("fixture parses")
}

/// Answer Chrome's real offer, and check every field the browser will
/// look at before it sends a packet.
#[tokio::test]
async fn forge_webrtc_answers_the_real_chrome_offer() {
    let settings = WebRtcSettings::default();
    let mut peer = PeerConnection::with_config(settings.peer_config())
        .await
        .expect("peer connection");

    peer.set_remote_offer(CHROME_OFFER)
        .await
        .expect("Chrome's offer must be applicable");
    let answer = peer.create_answer().await.expect("answer");

    // Parses as SDP at all, and as an answer to *this* offer.
    let parsed = parse(&answer);
    assert_eq!(parsed.media.len(), 1, "one m-line, mirroring the offer");

    // DTLS role: the offer said actpass, so we take the client role and
    // must say so (RFC 8842 §5.3). Getting this backwards is a silent
    // handshake stall.
    assert!(answer.contains("a=setup:active"), "{answer}");

    // Our own ICE credentials and certificate fingerprint — the browser
    // binds its DTLS check to the latter.
    assert!(answer.contains("a=ice-ufrag:"), "{answer}");
    assert!(answer.contains("a=ice-pwd:"), "{answer}");
    assert!(
        answer.contains(&format!(
            "a=fingerprint:sha-256 {}",
            peer.dtls_fingerprint()
        )),
        "answer must carry OUR fingerprint: {answer}"
    );

    // BUNDLE + rtcp-mux + the offer's mid, all mirrored.
    assert!(answer.contains("a=group:BUNDLE 0"), "{answer}");
    assert!(answer.contains("a=mid:0"), "{answer}");
    assert!(answer.contains("a=rtcp-mux"), "{answer}");

    // Exactly one codec is pinned (forge-media #130), and with the
    // default preference against a browser that offers both, it is
    // Opus at the browser's own payload type.
    assert_eq!(peer.negotiated_codec(), (AudioCodec::Opus, 111));
    assert!(answer.contains("a=rtpmap:111 opus/48000/2"), "{answer}");
    assert!(
        !answer.contains("a=rtpmap:0 PCMU"),
        "an answer offers one codec, not the whole menu: {answer}"
    );

    // The answer must be inspectable as WebRTC-shaped too — it is what
    // the far side runs the same detection over.
    assert!(inspect(&parse(&answer)).is_webrtc());
}

/// The transcode-free path: a deployment whose SIP side is G.711
/// prefers PCMU, and Chrome offers it (RFC 7874 §3 makes G.711
/// mandatory-to-implement), so the whole call runs 8 kHz end to end
/// with no Opus decode at all.
#[tokio::test]
async fn g711_preference_pins_pcmu_against_the_same_offer() {
    let settings = WebRtcSettings {
        prefer_g711: true,
        ..Default::default()
    };
    let mut peer = PeerConnection::with_config(settings.peer_config())
        .await
        .expect("peer connection");
    peer.set_remote_offer(CHROME_OFFER).await.expect("offer");
    let answer = peer.create_answer().await.expect("answer");

    assert_eq!(peer.negotiated_codec(), (AudioCodec::PCMU, 0));
    assert!(answer.contains("a=rtpmap:0 PCMU/8000"), "{answer}");
    assert!(!answer.contains("opus"), "{answer}");
    // 20 ms at 8 kHz — what the bridge's framing will use for this leg.
    assert_eq!(peer.sender().expect("sender").samples_per_20ms(), 160);
}

/// Chrome trickles: its offer carries an mDNS host candidate and a
/// srflx one, and more arrive later. Applying the offer must not
/// depend on any of them being usable — the answer is produced from
/// our own gathering, and connectivity is settled afterwards.
#[tokio::test]
async fn answer_does_not_depend_on_the_browser_mdns_candidate() {
    let shape = inspect(&parse(CHROME_OFFER));
    assert_eq!(shape.mdns_candidates, 1, "fixture still has the mDNS one");

    let mut peer = PeerConnection::with_config(WebRtcSettings::default().peer_config())
        .await
        .expect("peer connection");
    peer.set_remote_offer(CHROME_OFFER).await.expect("offer");
    let answer = peer.create_answer().await.expect("answer");

    // Our side advertises its own candidates — that is what the
    // browser will connect to.
    assert!(
        answer.contains("a=candidate:"),
        "answer should carry our gathered candidates: {answer}"
    );
}
