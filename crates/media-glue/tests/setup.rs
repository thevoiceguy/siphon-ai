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
use siphon_ai_media_glue::{
    Codec, InboundCall, MediaSetup, OutboundOfferRequest, OutboundSrtp, SdpError, SetupError,
};
use tokio::sync::mpsc;

mod common;
use common::{G729_ONLY_OFFER, LINPHONE_PCMU_OFFER};

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
        barge_in_action: ::siphon_ai_media_glue::BargeInAction::Notify,
        barge_in_debounce: None,
        inactivity_timeout: None,
        silence_threshold: None,
        dead_air_threshold: None,
        rtp_stats_interval: None,
        vad: ::siphon_ai_media_glue::VadBackend::default(),
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

    // (h) Peer RTP endpoint from the offer's c= / m= lines is pushed
    //     to forge at accept time so outbound playout starts immediately
    //     instead of waiting on the symmetric-RTP latch.
    assert_eq!(
        media_state.remote_rtp_addr,
        Some("10.0.0.5:7078".parse().unwrap()),
    );
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
    let pump = tokio::spawn(accepted.tap.run(
        caller_tx,
        playout_rx,
        ::tokio::sync::mpsc::channel::<::siphon_ai_bridge::OutgoingEvent>(1).0,
        ::tokio::sync::mpsc::channel::<::siphon_ai_media_glue::TapCommand>(1).1,
    ));

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
            barge_in_action: ::siphon_ai_media_glue::BargeInAction::Notify,
            barge_in_debounce: None,
            inactivity_timeout: None,
            silence_threshold: None,
            dead_air_threshold: None,
            rtp_stats_interval: None,
            vad: ::siphon_ai_media_glue::VadBackend::default(),
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
            barge_in_action: ::siphon_ai_media_glue::BargeInAction::Notify,
            barge_in_debounce: None,
            inactivity_timeout: None,
            silence_threshold: None,
            dead_air_threshold: None,
            rtp_stats_interval: None,
            vad: ::siphon_ai_media_glue::VadBackend::default(),
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

#[tokio::test]
async fn neural_vad_call_gets_a_rate_aligned_neural_detector() {
    // `[media].vad = "neural"` end to end: the session is allocated
    // before codec negotiation (default 16 kHz model), and the PCMU
    // answer (8 kHz bridge rate) must leave the call holding an 8 kHz
    // neural detector — re-aligned at setup, not first-packet.
    let (setup, session_mgr, _bridge_mgr) = fresh_setup(40700, 40800);

    let mut call = pcmu_call("c-neural", LINPHONE_PCMU_OFFER);
    call.vad = ::siphon_ai_media_glue::VadBackend::Neural;
    let accepted = setup.accept_inbound(call).await.expect("accepts");

    {
        let detector = accepted.session.vad_detector().lock().await;
        assert_eq!(detector.backend_name(), "neural");
        assert_eq!(
            detector.required_sample_rate(),
            Some(8000),
            "detector must run at the negotiated bridge rate, not the 16 kHz default"
        );
    }

    session_mgr
        .stop_session(&CallId::new("c-neural"))
        .await
        .expect("teardown");
}

#[tokio::test]
async fn energy_vad_call_keeps_engine_default_detector() {
    // The default path must not grow a per-session config: same
    // detector a pre-0.37 build would have (energy, rate-agnostic).
    let (setup, session_mgr, _bridge_mgr) = fresh_setup(40900, 41000);

    let accepted = setup
        .accept_inbound(pcmu_call("c-energy", LINPHONE_PCMU_OFFER))
        .await
        .expect("accepts");

    {
        let detector = accepted.session.vad_detector().lock().await;
        assert_eq!(detector.backend_name(), "energy_zcr");
        assert_eq!(detector.required_sample_rate(), None);
    }

    session_mgr
        .stop_session(&CallId::new("c-energy"))
        .await
        .expect("teardown");
}

// ─── #556: outbound reservation on the shared RTP port pool ──────────
//
// Inbound and outbound draw port pairs from one pool. Without a
// reservation whichever direction asks first keeps what it takes, so an
// inbound surge starves origination completely and nothing on the
// inbound side says so. `[media].reserved_outbound_calls` is the floor
// the inbound allocator refuses at.
//
// The floor is enforced inside forge's allocator, under the same lock
// that removes the port (forge-media #120), so it is exact rather than
// the soft target the original `port_pool_stats()` pre-check gave.
// `the_floor_is_exact_under_concurrent_inbound_setup` is the test that
// would have failed against the pre-check.

/// A `MediaSetup` over a fresh pool holding `reserved` pairs back from
/// inbound. Ranges must span >= 100 ports (forge's minimum), so the
/// pool is 50 pairs and the reservations below are sized against that.
fn reserved_setup(
    min_port: u16,
    max_port: u16,
    reserved: usize,
) -> (MediaSetup, Arc<SessionManager>) {
    let session_mgr = small_session_manager(min_port, max_port);
    let setup = reserved_setup_on(&session_mgr, reserved);
    (setup, session_mgr)
}

/// A `MediaSetup` with a given floor over an *existing* pool, for tests
/// that need two views of the same pool.
fn reserved_setup_on(session_mgr: &Arc<SessionManager>, reserved: usize) -> MediaSetup {
    MediaSetup::new(
        Arc::clone(session_mgr),
        Arc::new(MediaBridgeManager::new()),
        Arc::new(forge_core::EventBus::new()),
        "192.168.1.10",
    )
    .with_reserved_outbound_calls(reserved)
}

fn outbound_req(call_id: &str) -> OutboundOfferRequest {
    OutboundOfferRequest {
        call_id: CallId::new(call_id),
        codecs: vec![Codec::Pcmu],
        dtmf_payload_type: Some(101),
        participant_a: ParticipantId::generate(),
        participant_b: ParticipantId::generate(),
        from_tag: Some("ftag".into()),
        to_tag: None,
        srtp: OutboundSrtp::Off,
        vad: ::siphon_ai_media_glue::VadBackend::default(),
    }
}

#[tokio::test]
async fn reserve_refuses_inbound_while_ports_remain_and_leaves_them_to_outbound() {
    // 50-pair pool, 49 held for origination: exactly one inbound call
    // fits, and everything after it must be refused *while 49 pairs are
    // still free* — that is the whole point of the knob.
    let (setup, session_mgr) = reserved_setup(42000, 42100, 49);

    setup
        .accept_inbound(pcmu_call("c-res-1", LINPHONE_PCMU_OFFER))
        .await
        .expect("first inbound fits under the reservation");

    let (allocated, available) = session_mgr.port_pool_stats().await;
    assert_eq!(allocated, 1);
    assert_eq!(available, 49, "the reserved band, and nothing more");

    let refused = setup
        .accept_inbound(pcmu_call("c-res-2", LINPHONE_PCMU_OFFER))
        .await;
    assert!(
        matches!(refused, Err(SetupError::ResourceLimit(_))),
        "inbound at the reservation must take the exhausted-pool path \
         (503 + Retry-After, rejected_capacity); got {refused:?}"
    );

    // Refusing must not have cost a port, or the reservation would
    // erode itself under a sustained surge.
    let (allocated, available) = session_mgr.port_pool_stats().await;
    assert_eq!(allocated, 1, "a refused INVITE allocates nothing");
    assert_eq!(available, 49);

    // And the reserved band is genuinely usable by origination — the
    // outbound allocator is not gated.
    setup
        .originate_offer(outbound_req("c-res-out"))
        .await
        .expect("origination draws from the reserved band");
    let (allocated, _) = session_mgr.port_pool_stats().await;
    assert_eq!(allocated, 2);

    session_mgr
        .stop_session(&CallId::new("c-res-1"))
        .await
        .expect("teardown");
    session_mgr
        .stop_session(&CallId::new("c-res-out"))
        .await
        .expect("teardown");
}

#[tokio::test]
async fn reserve_of_zero_leaves_the_pool_unreserved() {
    // The default. Pre-0.50 behaviour must be byte-for-byte unchanged:
    // inbound may take the last pair in the pool.
    let (setup, session_mgr) = reserved_setup(42200, 42300, 0);

    for i in 0..50 {
        setup
            .accept_inbound(pcmu_call(&format!("c-unres-{i}"), LINPHONE_PCMU_OFFER))
            .await
            .unwrap_or_else(|e| panic!("inbound {i} of 50 must fit an unreserved pool: {e}"));
    }

    let (allocated, available) = session_mgr.port_pool_stats().await;
    assert_eq!((allocated, available), (50, 0));

    // 51st is a genuinely exhausted pool — same error type, different
    // cause, which is exactly why the split lives in a metric and not
    // on the wire.
    let exhausted = setup
        .accept_inbound(pcmu_call("c-unres-51", LINPHONE_PCMU_OFFER))
        .await;
    assert!(matches!(exhausted, Err(SetupError::ResourceLimit(_))));

    for i in 0..50 {
        session_mgr
            .stop_session(&CallId::new(format!("c-unres-{i}")))
            .await
            .expect("teardown");
    }
}

#[tokio::test]
async fn reserve_is_checked_after_the_offer_parse() {
    // Ordering matters for the caller's answer: a malformed offer is
    // the peer's bug (488) and must not be reported as our capacity
    // (503) just because the pool happens to be at the floor.
    let (setup, _session_mgr) = reserved_setup(42400, 42500, 50);

    let mut call = pcmu_call("c-res-parse", LINPHONE_PCMU_OFFER);
    call.offer_sdp = "not actually sdp";
    let result = setup.accept_inbound(call).await;

    assert!(
        matches!(result, Err(SetupError::Sdp(SdpError::Parse(_)))),
        "parse must fail before the reservation check; got {result:?}"
    );
}

/// The floor holds when inbound setups race, which the pre-#120
/// `port_pool_stats()`-then-allocate gate could not promise: `K`
/// concurrent callers each read the same free count and each decided
/// they were clear, dipping up to `K-1` pairs under. Now the check
/// happens inside the allocation's own critical section.
///
/// 100-pair pool, floor 60: exactly 40 inbound calls may land no matter
/// how they interleave, leaving exactly 60 for origination.
#[tokio::test]
async fn the_floor_is_exact_under_concurrent_inbound_setup() {
    let session_mgr = small_session_manager(42600, 42800);
    let setup = Arc::new(
        MediaSetup::new(
            Arc::clone(&session_mgr),
            Arc::new(MediaBridgeManager::new()),
            Arc::new(forge_core::EventBus::new()),
            "192.168.1.10",
        )
        .with_reserved_outbound_calls(60),
    );

    let mut tasks = Vec::new();
    for i in 0..64 {
        let setup = Arc::clone(&setup);
        tasks.push(tokio::spawn(async move {
            setup
                .accept_inbound(pcmu_call(&format!("c-race-{i}"), LINPHONE_PCMU_OFFER))
                .await
                .is_ok()
        }));
    }
    let mut admitted = 0;
    for t in tasks {
        if t.await.unwrap() {
            admitted += 1;
        }
    }

    assert_eq!(
        admitted, 40,
        "capacity 100 minus a floor of 60 — exactly, not approximately"
    );
    let (allocated, available) = session_mgr.port_pool_stats().await;
    assert_eq!((allocated, available), (40, 60));
}

/// The metric that splits "pool empty" from "the rest is reserved"
/// classifies forge's message. Pin it against an error a *real* pool at
/// its floor produces, so an upstream rewording fails here instead of
/// silently zeroing `siphon_ai_rtp_reserve_blocks_total`.
#[tokio::test]
async fn reserve_refusal_is_recognised_from_the_real_forge_error() {
    let session_mgr = small_session_manager(42800, 43000); // 100 pairs
    let setup = reserved_setup_on(&session_mgr, 99);

    setup
        .accept_inbound(pcmu_call("c-msg-1", LINPHONE_PCMU_OFFER))
        .await
        .expect("one call fits under a floor of 99");

    let err = setup
        .accept_inbound(pcmu_call("c-msg-2", LINPHONE_PCMU_OFFER))
        .await
        .expect_err("the second is at the floor");
    let SetupError::ResourceLimit(detail) = err else {
        panic!("expected ResourceLimit, got {err:?}");
    };
    assert!(
        detail.contains("reserved floor"),
        "media-glue classifies the reserve refusal on this substring; forge \
         reworded it and the reserve metric is now silently dead: {detail}"
    );

    // And an exhausted pool must NOT look like a floor refusal.
    let plain = reserved_setup_on(&session_mgr, 0);
    for i in 0..99 {
        plain
            .accept_inbound(pcmu_call(&format!("c-msg-fill-{i}"), LINPHONE_PCMU_OFFER))
            .await
            .unwrap_or_else(|e| panic!("fill {i}: {e}"));
    }
    let empty = plain
        .accept_inbound(pcmu_call("c-msg-empty", LINPHONE_PCMU_OFFER))
        .await
        .expect_err("pool is empty");
    let SetupError::ResourceLimit(detail) = empty else {
        panic!("expected ResourceLimit");
    };
    assert!(
        !detail.contains("reserved floor"),
        "an exhausted pool must not be counted as a reservation refusal: {detail}"
    );
}

// ─── PortReservation (DEV_PLAN_WebRTC.md §4.4) ───────────────────────
//
// A WebRTC leg has no forge `MediaSession`, so it gets no ports as a
// side effect of one. It still fills a call slot, so it draws a pair
// explicitly — under the same floor, visible in the same gauge.

/// The gauge an operator watches is sampled from pool truth, so a
/// browser call is only visible in it if the pair is genuinely held.
#[tokio::test]
async fn reserved_pair_shows_in_the_pool_while_held() {
    let (setup, mgr) = reserved_setup(43000, 43100, 0);
    let (allocated_before, available_before) = mgr.port_pool_stats().await;
    assert_eq!(allocated_before, 0);

    let reservation = setup.reserve_port_pair().await.expect("pool has room");
    assert_eq!(mgr.port_pool_stats().await.0, 1, "pair must be held");
    assert!(
        (43000..43100).contains(&reservation.rtp_port()),
        "media must bind inside the configured range, got {}",
        reservation.rtp_port()
    );
    assert_eq!(reservation.rtp_port() % 2, 0, "RTP port must be even");

    drop(reservation);
    // Release is spawned (forge's release is async, `Drop` is not), so
    // let the runtime run it.
    tokio::task::yield_now().await;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    assert_eq!(
        mgr.port_pool_stats().await,
        (0, available_before),
        "dropping the reservation must return the pair"
    );
}

/// The whole point of holding a *pair*: a browser call and a SIP call
/// cost the same, so the reserved band means one thing regardless of
/// which kind of leg is consuming the pool.
#[tokio::test]
async fn browser_legs_respect_the_outbound_reservation() {
    // 50 pairs, 49 reserved for origination — one inbound slot.
    let (setup, mgr) = reserved_setup(43200, 43300, 49);

    let first = setup.reserve_port_pair().await.expect("one slot is free");
    let second = setup.reserve_port_pair().await;
    assert!(
        second.is_err(),
        "a browser leg must not dip into the outbound reservation"
    );

    // And the refusal is the same shape a SIP call gets, so the caller
    // rejects with the same 503 rather than needing a second path.
    assert!(matches!(second, Err(SetupError::ResourceLimit(_))));

    drop(first);
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert_eq!(mgr.port_pool_stats().await.0, 0);
}

/// Browser and SIP legs draw from one pool, not two: a held browser
/// pair really does reduce what a SIP call can get.
#[tokio::test]
async fn browser_and_sip_legs_share_one_pool() {
    let (setup, mgr) = reserved_setup(43400, 43500, 0);
    let (_, capacity) = mgr.port_pool_stats().await;

    let held: Vec<_> = {
        let mut v = Vec::new();
        for _ in 0..capacity {
            v.push(setup.reserve_port_pair().await.expect("within capacity"));
        }
        v
    };
    assert_eq!(mgr.port_pool_stats().await.0, capacity);

    // Pool is now dry — an inbound SIP call must be refused, which is
    // what proves the two leg types are competing for one resource.
    assert!(
        setup.reserve_port_pair().await.is_err(),
        "an exhausted pool must refuse"
    );

    drop(held);
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert_eq!(mgr.port_pool_stats().await.0, 0, "all pairs returned");
}
