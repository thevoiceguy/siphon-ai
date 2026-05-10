//! End-to-end tests for `MediaSetup`: real `SessionManager`, real
//! offer SDP, real `MediaBridgeManager` / `MediaTap`. These are the
//! tests that prove the daemon can actually answer a call.

use std::sync::Arc;
use std::time::Duration;

use forge_core::{CallId, ParticipantId};
use forge_engine::{
    InboundMediaFrame, MediaBridgeManager, MediaTarget, OutboundMediaRequest, ParticipantLabel,
    SessionManager, SessionManagerConfig,
};
use forge_rtp::PortPoolConfig;
use forge_sdp::{MediaType, SessionDescription, SessionDescriptionExt};
use siphon_ai_bridge::{pack_pcm16_le, unpack_pcm16_le};
use siphon_ai_media_glue::{Codec, InboundCall, MediaSetup, SdpError, SetupError};
use tokio::sync::mpsc;

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

const G729_ONLY_OFFER: &str = "v=0\r\n\
o=- 1 1 IN IP4 10.0.0.5\r\n\
s=Talk\r\n\
c=IN IP4 10.0.0.5\r\n\
t=0 0\r\n\
m=audio 7000 RTP/AVP 18\r\n\
a=rtpmap:18 G729/8000\r\n\
a=sendrecv\r\n";

fn small_session_manager(min: u16, max: u16) -> Arc<SessionManager> {
    let config = SessionManagerConfig {
        port_pool_config: PortPoolConfig::new(min, max).expect("valid port range"),
        ..Default::default()
    };
    SessionManager::new(config, None)
}

fn fresh_setup(
    min_port: u16,
    max_port: u16,
) -> (MediaSetup, Arc<SessionManager>, Arc<MediaBridgeManager>) {
    let session_mgr = small_session_manager(min_port, max_port);
    let bridge_mgr = Arc::new(MediaBridgeManager::new());
    let setup = MediaSetup::new(
        Arc::clone(&session_mgr),
        Arc::clone(&bridge_mgr),
            Arc::new(forge_core::EventBus::new()),
        "192.168.1.10",
    );
    (setup, session_mgr, bridge_mgr)
}

fn pcmu_call(call_id: &str, offer: &'static str) -> InboundCall<'static> {
    InboundCall {
        call_id: CallId::new(call_id),
        offer_sdp: offer,
        codecs: vec![Codec::Pcmu, Codec::Pcma],
        dtmf_payload_type: Some(101),
        participant_a: ParticipantId::new("caller"),
        participant_b: ParticipantId::new("siphon-ws"),
        from_tag: Some("from-tag-1".to_string()),
        to_tag: Some("to-tag-1".to_string()),
    }
}

#[tokio::test]
async fn happy_path_returns_answer_session_and_attached_tap() {
    let (setup, session_mgr, bridge_mgr) = fresh_setup(40100, 40200);
    let call_id = CallId::new("c-happy");

    let accepted = setup
        .accept_inbound(pcmu_call("c-happy", LINPHONE_PCMU_OFFER))
        .await
        .expect("accept inbound");

    // (a) Negotiated metadata reflects the offer (PCMU).
    assert_eq!(accepted.answer.negotiated_codec, Codec::Pcmu);
    assert_eq!(accepted.answer.negotiated_payload_type, 0);
    assert_eq!(accepted.answer.negotiated_audio_sample_rate, 8000);

    // (b) Session was created with the right call_id and is the one
    //     we'd find by going back to the manager.
    assert_eq!(accepted.session.call_id(), &call_id);
    assert!(session_mgr.get_session(&call_id).is_some());

    // (c) Forge allocated a port, and that port is what the answer
    //     advertises — the whole point of doing setup post-allocation.
    let allocated_rtp_port = accepted.session.ports().rtp_port;
    assert!(
        accepted
            .answer
            .answer_text
            .contains(&format!("m=audio {} RTP/AVP", allocated_rtp_port)),
        "answer must advertise the forge-allocated port; got: {}",
        accepted.answer.answer_text
    );
    assert!(accepted
        .answer
        .answer_text
        .contains("c=IN IP4 192.168.1.10"));

    // (d) Re-parse the answer to make sure it's well-formed enough
    //     for the SIP UAS to put on the wire.
    let reparsed =
        SessionDescription::from_str(&accepted.answer.answer_text).expect("answer parses");
    let audio = reparsed.find_media(MediaType::Audio).expect("audio media");
    assert_eq!(audio.port, allocated_rtp_port);

    // (e) Tap is attached on the bridge manager.
    assert!(bridge_mgr.has_bridge(&call_id));
    assert_eq!(accepted.tap.sample_rate(), 8000);
    assert_eq!(accepted.tap.call_id(), &call_id);

    // (f) The session sees the bridge manager (i.e., forwarding will
    //     actually plumb into us).
    let mbm = accepted.session.media_bridge_manager().await;
    assert!(mbm.is_some(), "session should reference the bridge manager");

    // (g) Negotiated codec landed on participant A.
    let media_state = session_mgr
        .participant_media_state(&call_id, ParticipantLabel::A)
        .await
        .expect("participant state");
    assert_eq!(media_state.codec, forge_core::AudioCodec::PCMU);
    assert_eq!(media_state.payload_type, 0);
    assert_eq!(media_state.clock_rate, 8000);
    assert_eq!(media_state.telephone_event_payload_type, 101);
}

#[tokio::test]
async fn pre_attached_tap_pumps_real_audio() {
    // The user's original ask: "hands back a pre-attached MediaTap
    // ready for CallController." Prove it: drive the returned tap
    // with a synthetic inbound frame and read it on the controller-
    // side channel.
    let (setup, _session_mgr, bridge_mgr) = fresh_setup(40300, 40400);

    let accepted = setup
        .accept_inbound(pcmu_call("c-pump", LINPHONE_PCMU_OFFER))
        .await
        .expect("accept inbound");
    let call_id = accepted.session.call_id().clone();

    let (caller_tx, mut caller_rx) = mpsc::channel::<Vec<u8>>(10);
    let (playout_tx, playout_rx) = mpsc::channel::<Vec<u8>>(10);
    let pump = tokio::spawn(accepted.tap.run(caller_tx, playout_rx, ::tokio::sync::mpsc::channel::<::siphon_ai_bridge::OutgoingEvent>(1).0));

    // Push 20 ms of inbound at 8 kHz (160 samples).
    let pattern: Vec<i16> = (0..160).map(|i| (i as i16) * 3).collect();
    bridge_mgr
        .try_send_inbound_frame(
            &call_id,
            InboundMediaFrame {
                leg: ParticipantLabel::A,
                codec: forge_core::AudioCodec::PCMU,
                payload_type: 0,
                sample_rate: 8000,
                timestamp: 1000,
                sequence_number: 1,
                samples: pattern.clone(),
            },
        )
        .expect("inbound");

    let bytes = tokio::time::timeout(Duration::from_millis(500), caller_rx.recv())
        .await
        .expect("frame arrives")
        .expect("channel open");
    assert_eq!(unpack_pcm16_le(&bytes).unwrap(), pattern);

    // And the outbound side reaches forge.
    let echo = pack_pcm16_le(&pattern);
    playout_tx.send(echo).await.expect("send playout");
    let drained = tokio::time::timeout(Duration::from_millis(500), async {
        loop {
            if let Some(req) = bridge_mgr.try_recv_outbound_request(&call_id).await {
                return req;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("forge sees outbound");
    match drained {
        OutboundMediaRequest::Audio(frame) => {
            assert_eq!(frame.target, MediaTarget::A);
            assert_eq!(frame.samples, pattern);
        }
        other => panic!("expected Audio variant, got {other:?}"),
    }

    drop(caller_rx);
    drop(playout_tx);
    let _ = tokio::time::timeout(Duration::from_secs(1), pump).await;
}

#[tokio::test]
async fn no_common_codec_rolls_back_session() {
    // G.729-only offer, but we only advertise PCMU/PCMA. Negotiation
    // fails — and the session created in step (2) must be torn down,
    // or we'd leak ports on every misconfigured peer.
    let (setup, session_mgr, bridge_mgr) = fresh_setup(40500, 40600);

    let result = setup
        .accept_inbound(InboundCall {
            call_id: CallId::new("c-no-codec"),
            offer_sdp: G729_ONLY_OFFER,
            codecs: vec![Codec::Pcmu, Codec::Pcma],
            dtmf_payload_type: None,
            participant_a: ParticipantId::generate(),
            participant_b: ParticipantId::generate(),
            from_tag: None,
            to_tag: None,
        })
        .await;
    assert!(matches!(
        result,
        Err(SetupError::Sdp(SdpError::NoCommonCodec))
            | Err(SetupError::Sdp(SdpError::AudioRejected))
    ));

    // Rollback is via tokio::spawn; give it a moment.
    tokio::time::sleep(Duration::from_millis(50)).await;

    assert_eq!(session_mgr.session_count(), 0, "session must be cleaned up");
    let (allocated, _) = session_mgr.port_pool_stats().await;
    assert_eq!(allocated, 0, "port pool must release allocations");
    assert!(
        !bridge_mgr.has_bridge(&CallId::new("c-no-codec")),
        "no tap should remain attached"
    );
}

#[tokio::test]
async fn malformed_offer_does_not_allocate_ports() {
    let (setup, session_mgr, _bridge_mgr) = fresh_setup(40700, 40800);

    let err = setup
        .accept_inbound(InboundCall {
            call_id: CallId::new("c-bad"),
            offer_sdp: "totally not sdp",
            codecs: vec![Codec::Pcmu],
            dtmf_payload_type: None,
            participant_a: ParticipantId::generate(),
            participant_b: ParticipantId::generate(),
            from_tag: None,
            to_tag: None,
        })
        .await
        .unwrap_err();
    assert!(matches!(err, SetupError::Sdp(SdpError::Parse(_))));

    assert_eq!(session_mgr.session_count(), 0);
    let (allocated, _) = session_mgr.port_pool_stats().await;
    assert_eq!(allocated, 0);
}

#[tokio::test]
async fn answer_port_matches_what_forge_allocated() {
    // Belt-and-suspenders: the LocalCapabilities port we feed the
    // negotiator must equal forge's chosen RTP port. If anything
    // ever drifts (e.g., we accidentally rebuilt caps from a stale
    // value), this fails loudly.
    let (setup, _, _) = fresh_setup(40900, 41000);
    let accepted = setup
        .accept_inbound(pcmu_call("c-port", LINPHONE_PCMU_OFFER))
        .await
        .expect("accept");

    let port = accepted.session.ports().rtp_port;
    let parsed = SessionDescription::from_str(&accepted.answer.answer_text).expect("parse answer");
    let audio = parsed.find_media(MediaType::Audio).expect("audio");
    assert_eq!(audio.port, port);
}
