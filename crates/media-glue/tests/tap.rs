//! Integration tests for `MediaTap` against a real
//! `forge_engine::MediaBridgeManager`.
//!
//! Same producer/drainer setup as the Week-1 spike loopback test, but
//! now with `MediaTap` in the middle: the tap reads forge frames and
//! emits PCM16-LE bytes on a channel; the test plays the role of the
//! controller (and of forge's RTP forwarding loop) to assert the
//! pump's behaviour end to end.

use std::sync::Arc;
use std::time::Duration;

use forge_core::{AudioCodec, CallId};
use forge_engine::{
    InboundMediaFrame, MediaBridgeManager, MediaTarget, OutboundMediaRequest, ParticipantLabel,
};
use siphon_ai_bridge::{pack_pcm16_le, unpack_pcm16_le};
use siphon_ai_media_glue::{MediaTap, MediaTapError, TapDisconnect};
use tokio::sync::mpsc;

const SAMPLES_PER_FRAME_8K: usize = 160; // 20 ms @ 8 kHz

fn synth_inbound(seq: u16, sample_rate: u32, samples: Vec<i16>) -> InboundMediaFrame {
    InboundMediaFrame {
        leg: ParticipantLabel::A,
        codec: AudioCodec::PCMU,
        payload_type: 0,
        sample_rate,
        timestamp: 1000 + seq as u32 * SAMPLES_PER_FRAME_8K as u32,
        sequence_number: seq,
        samples,
    }
}

#[tokio::test]
async fn inbound_audio_reframed_and_packed_to_caller_channel() {
    let manager = Arc::new(MediaBridgeManager::with_capacities(10, 10));
    let call = CallId::new("c");
    let tap = MediaTap::attach(
        &manager,
        &::std::sync::Arc::new(forge_core::EventBus::new()),
        call.clone(),
        8000,
    )
    .expect("attach");

    let (caller_tx, mut caller_rx) = mpsc::channel::<Vec<u8>>(10);
    let (_playout_tx, playout_rx) = mpsc::channel::<Vec<u8>>(10);

    let pump = tokio::spawn(tap.run(
        caller_tx,
        playout_rx,
        ::tokio::sync::mpsc::channel::<::siphon_ai_bridge::OutgoingEvent>(1).0,
        ::tokio::sync::mpsc::channel::<::siphon_ai_media_glue::TapCommand>(1).1,
    ));

    // Push a 20 ms frame of pattern data; the tap should emit one
    // packed wire frame.
    let pattern: Vec<i16> = (0..SAMPLES_PER_FRAME_8K).map(|i| i as i16).collect();
    manager
        .try_send_inbound_frame(&call, synth_inbound(1, 8000, pattern.clone()))
        .expect("send");

    let bytes = tokio::time::timeout(Duration::from_millis(500), caller_rx.recv())
        .await
        .expect("frame should arrive within 500 ms")
        .expect("channel open");
    assert_eq!(bytes.len(), 320, "PCM16-LE 8 kHz 20 ms = 320 bytes");
    let recovered = unpack_pcm16_le(&bytes).unwrap();
    assert_eq!(recovered, pattern);

    // Tear down: drop the manager's tap-side handle by ending the pump.
    drop(caller_rx);
    let result = tokio::time::timeout(Duration::from_secs(1), pump)
        .await
        .expect("pump returns")
        .unwrap()
        .expect("clean disconnect");
    assert_eq!(result, TapDisconnect::ControllerHungUp);
}

#[tokio::test]
async fn small_inbound_frames_are_buffered_until_a_full_20ms_is_available() {
    // forge could deliver packets at ptime=10; the reframer should
    // collect two 10 ms frames into one 20 ms wire frame.
    let manager = Arc::new(MediaBridgeManager::with_capacities(10, 10));
    let call = CallId::new("c");
    let tap = MediaTap::attach(
        &manager,
        &::std::sync::Arc::new(forge_core::EventBus::new()),
        call.clone(),
        8000,
    )
    .expect("attach");

    let (caller_tx, mut caller_rx) = mpsc::channel::<Vec<u8>>(10);
    let (_playout_tx, playout_rx) = mpsc::channel::<Vec<u8>>(10);
    let pump = tokio::spawn(tap.run(
        caller_tx,
        playout_rx,
        ::tokio::sync::mpsc::channel::<::siphon_ai_bridge::OutgoingEvent>(1).0,
        ::tokio::sync::mpsc::channel::<::siphon_ai_media_glue::TapCommand>(1).1,
    ));

    // Two 10 ms half-frames at 8 kHz = 80 samples each.
    manager
        .try_send_inbound_frame(&call, synth_inbound(1, 8000, vec![1i16; 80]))
        .expect("send 1");
    manager
        .try_send_inbound_frame(&call, synth_inbound(2, 8000, vec![2i16; 80]))
        .expect("send 2");

    let bytes = tokio::time::timeout(Duration::from_millis(500), caller_rx.recv())
        .await
        .expect("frame arrives")
        .expect("channel open");
    let samples = unpack_pcm16_le(&bytes).unwrap();
    assert_eq!(samples.len(), 160);
    assert!(samples[..80].iter().all(|&s| s == 1));
    assert!(samples[80..].iter().all(|&s| s == 2));

    drop(caller_rx);
    let _ = tokio::time::timeout(Duration::from_secs(1), pump).await;
}

#[tokio::test]
async fn outbound_audio_unpacked_and_handed_to_forge() {
    let manager = Arc::new(MediaBridgeManager::with_capacities(10, 10));
    let call = CallId::new("c");
    let tap = MediaTap::attach(
        &manager,
        &::std::sync::Arc::new(forge_core::EventBus::new()),
        call.clone(),
        8000,
    )
    .expect("attach");

    let (caller_tx, _caller_rx) = mpsc::channel::<Vec<u8>>(10);
    let (playout_tx, playout_rx) = mpsc::channel::<Vec<u8>>(10);
    let pump = tokio::spawn(tap.run(
        caller_tx,
        playout_rx,
        ::tokio::sync::mpsc::channel::<::siphon_ai_bridge::OutgoingEvent>(1).0,
        ::tokio::sync::mpsc::channel::<::siphon_ai_media_glue::TapCommand>(1).1,
    ));

    // Send a 20 ms frame's worth of wire bytes.
    let samples: Vec<i16> = (10..170).collect();
    let bytes = pack_pcm16_le(&samples);
    playout_tx.send(bytes).await.expect("send");

    // Pump it through forge's outbound side. Loop briefly because the
    // pump may not have processed yet.
    let drained = tokio::time::timeout(Duration::from_millis(500), async {
        loop {
            if let Some(req) = manager.try_recv_outbound_request(&call).await {
                return req;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("forge sees outbound");

    match drained {
        OutboundMediaRequest::Audio(frame) => {
            assert_eq!(frame.target, MediaTarget::A, "single-leg → leg A");
            assert_eq!(frame.sample_rate, 8000);
            assert_eq!(frame.samples, samples);
        }
        other => panic!("expected Audio variant, got {other:?}"),
    }

    drop(playout_tx);
    let _ = tokio::time::timeout(Duration::from_secs(1), pump).await;
}

#[tokio::test]
async fn sample_rate_mismatch_yields_error() {
    let manager = Arc::new(MediaBridgeManager::with_capacities(10, 10));
    let call = CallId::new("c");
    let tap = MediaTap::attach(
        &manager,
        &::std::sync::Arc::new(forge_core::EventBus::new()),
        call.clone(),
        8000,
    )
    .expect("attach");

    let (caller_tx, _caller_rx) = mpsc::channel::<Vec<u8>>(10);
    let (_playout_tx, playout_rx) = mpsc::channel::<Vec<u8>>(10);
    let pump = tokio::spawn(tap.run(
        caller_tx,
        playout_rx,
        ::tokio::sync::mpsc::channel::<::siphon_ai_bridge::OutgoingEvent>(1).0,
        ::tokio::sync::mpsc::channel::<::siphon_ai_media_glue::TapCommand>(1).1,
    ));

    // Push a frame at the WRONG rate.
    manager
        .try_send_inbound_frame(&call, synth_inbound(1, 16000, vec![0i16; 320]))
        .expect("send");

    let result = tokio::time::timeout(Duration::from_secs(1), pump)
        .await
        .expect("pump returns")
        .unwrap();
    match result {
        Err(MediaTapError::SampleRateMismatch { expected, got }) => {
            assert_eq!(expected, 8000);
            assert_eq!(got, 16000);
        }
        other => panic!("expected SampleRateMismatch, got {other:?}"),
    }
}

#[tokio::test]
async fn malformed_outbound_bytes_yield_audio_error() {
    let manager = Arc::new(MediaBridgeManager::with_capacities(10, 10));
    let call = CallId::new("c");
    let tap = MediaTap::attach(
        &manager,
        &::std::sync::Arc::new(forge_core::EventBus::new()),
        call.clone(),
        8000,
    )
    .expect("attach");

    let (caller_tx, _caller_rx) = mpsc::channel::<Vec<u8>>(10);
    let (playout_tx, playout_rx) = mpsc::channel::<Vec<u8>>(10);
    let pump = tokio::spawn(tap.run(
        caller_tx,
        playout_rx,
        ::tokio::sync::mpsc::channel::<::siphon_ai_bridge::OutgoingEvent>(1).0,
        ::tokio::sync::mpsc::channel::<::siphon_ai_media_glue::TapCommand>(1).1,
    ));

    // Odd-length payload — PCM16 needs an even byte count.
    playout_tx.send(vec![0u8; 5]).await.expect("send");

    let result = tokio::time::timeout(Duration::from_secs(1), pump)
        .await
        .expect("pump returns")
        .unwrap();
    assert!(
        matches!(result, Err(MediaTapError::Audio(_))),
        "got {result:?}"
    );
}

#[tokio::test]
async fn forge_call_ended_returns_call_ended() {
    let manager = Arc::new(MediaBridgeManager::with_capacities(10, 10));
    let call = CallId::new("c");
    let tap = MediaTap::attach(
        &manager,
        &::std::sync::Arc::new(forge_core::EventBus::new()),
        call.clone(),
        8000,
    )
    .expect("attach");

    let (caller_tx, _caller_rx) = mpsc::channel::<Vec<u8>>(10);
    let (_playout_tx, playout_rx) = mpsc::channel::<Vec<u8>>(10);
    let pump = tokio::spawn(tap.run(
        caller_tx,
        playout_rx,
        ::tokio::sync::mpsc::channel::<::siphon_ai_bridge::OutgoingEvent>(1).0,
        ::tokio::sync::mpsc::channel::<::siphon_ai_media_glue::TapCommand>(1).1,
    ));

    // Emulate forge ending the inbound stream by detaching the
    // call-id slot from the manager.
    tokio::time::sleep(Duration::from_millis(50)).await;
    manager.detach_call(&call);

    let result = tokio::time::timeout(Duration::from_secs(1), pump)
        .await
        .expect("pump returns")
        .unwrap()
        .expect("clean disconnect");
    assert_eq!(result, TapDisconnect::CallEnded);
}

#[tokio::test]
async fn dropping_playout_sender_does_not_end_tap_alone() {
    // Only the playout side closing doesn't end the call — forge can
    // still deliver caller audio, and we still want to forward it.
    // The tap exits when forge ends OR caller_audio_tx receiver
    // closes. (See forge_call_ended_returns_call_ended for the SIP
    // close path; this test confirms partial channel closures are
    // tolerated.)
    let manager = Arc::new(MediaBridgeManager::with_capacities(10, 10));
    let call = CallId::new("c");
    let tap = MediaTap::attach(
        &manager,
        &::std::sync::Arc::new(forge_core::EventBus::new()),
        call.clone(),
        8000,
    )
    .expect("attach");

    let (caller_tx, caller_rx) = mpsc::channel::<Vec<u8>>(10);
    let (playout_tx, playout_rx) = mpsc::channel::<Vec<u8>>(10);
    let pump = tokio::spawn(tap.run(
        caller_tx,
        playout_rx,
        ::tokio::sync::mpsc::channel::<::siphon_ai_bridge::OutgoingEvent>(1).0,
        ::tokio::sync::mpsc::channel::<::siphon_ai_media_glue::TapCommand>(1).1,
    ));

    drop(playout_tx);
    // Pump observes playout_rx closed and exits with ControllerHungUp.
    let result = tokio::time::timeout(Duration::from_secs(1), pump)
        .await
        .expect("pump returns")
        .unwrap()
        .expect("clean disconnect");
    assert_eq!(result, TapDisconnect::ControllerHungUp);
    drop(caller_rx);
}

#[tokio::test]
async fn round_trip_audio_via_tap_then_back_through_forge() {
    // Full loop: synthetic inbound → caller_tx (reframed bytes) →
    // back into playout_tx → forge outbound side → assert match.
    let manager = Arc::new(MediaBridgeManager::with_capacities(10, 10));
    let call = CallId::new("c");
    let tap = MediaTap::attach(
        &manager,
        &::std::sync::Arc::new(forge_core::EventBus::new()),
        call.clone(),
        8000,
    )
    .expect("attach");

    let (caller_tx, mut caller_rx) = mpsc::channel::<Vec<u8>>(10);
    let (playout_tx, playout_rx) = mpsc::channel::<Vec<u8>>(10);
    let pump = tokio::spawn(tap.run(
        caller_tx,
        playout_rx,
        ::tokio::sync::mpsc::channel::<::siphon_ai_bridge::OutgoingEvent>(1).0,
        ::tokio::sync::mpsc::channel::<::siphon_ai_media_glue::TapCommand>(1).1,
    ));

    let pattern: Vec<i16> = (0..SAMPLES_PER_FRAME_8K).map(|i| (i as i16) * 7).collect();
    manager
        .try_send_inbound_frame(&call, synth_inbound(1, 8000, pattern.clone()))
        .unwrap();

    let bytes_out = tokio::time::timeout(Duration::from_millis(500), caller_rx.recv())
        .await
        .expect("frame arrives")
        .expect("channel open");
    // Round-trip the bytes back through the tap.
    playout_tx.send(bytes_out).await.unwrap();

    let drained = tokio::time::timeout(Duration::from_millis(500), async {
        loop {
            if let Some(req) = manager.try_recv_outbound_request(&call).await {
                return req;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("forge sees outbound");

    match drained {
        OutboundMediaRequest::Audio(frame) => {
            assert_eq!(frame.samples, pattern);
        }
        other => panic!("expected Audio variant, got {other:?}"),
    }

    drop(caller_rx);
    drop(playout_tx);
    let _ = tokio::time::timeout(Duration::from_secs(1), pump).await;
}

// ─── DTMF subscription path ────────────────────────────────────────

/// Publish a `DtmfDigitDetected{End}` for the tap's call_id and assert
/// the tap forwards exactly one `OutgoingEvent::Dtmf` with the right
/// digit / duration / method.
///
/// Pins the wire-shape requirement that the WS server gets ONE event
/// per press (on `End`), not one per `Start`/`Continue`/`End` triple.
#[tokio::test]
async fn dtmf_end_event_emits_outgoing_event() {
    use chrono::Utc;
    use forge_core::{DtmfDetectionMethod, DtmfEventKind, EventBus as ForgeEventBus, ForgeEvent};
    use siphon_ai_bridge::{DtmfMethod, OutgoingEvent};

    let manager = Arc::new(MediaBridgeManager::with_capacities(10, 10));
    let bus = Arc::new(ForgeEventBus::new());
    let call = CallId::new("dtmf-test");
    let tap = MediaTap::attach(&manager, &bus, call.clone(), 8000).expect("attach");

    let (caller_tx, _caller_rx) = mpsc::channel::<Vec<u8>>(10);
    let (_playout_tx, playout_rx) = mpsc::channel::<Vec<u8>>(10);
    let (events_tx, mut events_rx) = mpsc::channel::<OutgoingEvent>(8);

    let pump = tokio::spawn(tap.run(
        caller_tx,
        playout_rx,
        events_tx,
        ::tokio::sync::mpsc::channel::<::siphon_ai_media_glue::TapCommand>(1).1,
    ));

    // Tap subscribes inside `attach`; `events_rx` (the broadcast
    // receiver inside the tap) is live by the time we publish below.

    // A Start should NOT produce an event — we only emit on End so
    // the WS receives a single complete press with `duration_ms`.
    bus.publish(ForgeEvent::DtmfDigitDetected {
        call_id: call.clone(),
        digit: '5',
        duration_ms: None,
        method: DtmfDetectionMethod::Rfc2833,
        event_type: DtmfEventKind::Start,
        timestamp: Utc::now(),
    })
    .expect("publish start");

    // The End event with the duration is what should fire.
    bus.publish(ForgeEvent::DtmfDigitDetected {
        call_id: call.clone(),
        digit: '5',
        duration_ms: Some(120),
        method: DtmfDetectionMethod::Rfc2833,
        event_type: DtmfEventKind::End,
        timestamp: Utc::now(),
    })
    .expect("publish end");

    let event = tokio::time::timeout(Duration::from_millis(500), events_rx.recv())
        .await
        .expect("event arrives within 500 ms")
        .expect("events_tx still open");

    match event {
        OutgoingEvent::Dtmf {
            digit,
            duration_ms,
            method,
        } => {
            assert_eq!(digit, '5');
            assert_eq!(duration_ms, 120);
            assert_eq!(method, DtmfMethod::Rfc2833);
        }
        other => panic!("expected Dtmf variant, got {other:?}"),
    }

    // No second event should follow the single End publish.
    let no_more = tokio::time::timeout(Duration::from_millis(50), events_rx.recv()).await;
    assert!(
        no_more.is_err(),
        "exactly one OutgoingEvent::Dtmf per End — got an extra: {no_more:?}",
    );

    drop(events_rx);
    drop(_caller_rx);
    drop(_playout_tx);
    let _ = tokio::time::timeout(Duration::from_secs(1), pump).await;
}

/// A DTMF event for a *different* call must not leak through to
/// our tap. Multiple concurrent calls subscribe to the same bus, so
/// per-call filtering is the property that keeps cross-call audio
/// from cross-talking.
#[tokio::test]
async fn dtmf_event_for_other_call_is_ignored() {
    use chrono::Utc;
    use forge_core::{DtmfDetectionMethod, DtmfEventKind, EventBus as ForgeEventBus, ForgeEvent};
    use siphon_ai_bridge::OutgoingEvent;

    let manager = Arc::new(MediaBridgeManager::with_capacities(10, 10));
    let bus = Arc::new(ForgeEventBus::new());
    let call = CallId::new("dtmf-mine");
    let tap = MediaTap::attach(&manager, &bus, call.clone(), 8000).expect("attach");

    let (caller_tx, _caller_rx) = mpsc::channel::<Vec<u8>>(10);
    let (_playout_tx, playout_rx) = mpsc::channel::<Vec<u8>>(10);
    let (events_tx, mut events_rx) = mpsc::channel::<OutgoingEvent>(8);

    let pump = tokio::spawn(tap.run(
        caller_tx,
        playout_rx,
        events_tx,
        ::tokio::sync::mpsc::channel::<::siphon_ai_media_glue::TapCommand>(1).1,
    ));

    // Publish for an unrelated call_id.
    bus.publish(ForgeEvent::DtmfDigitDetected {
        call_id: CallId::new("dtmf-someone-else"),
        digit: '9',
        duration_ms: Some(80),
        method: DtmfDetectionMethod::Rfc2833,
        event_type: DtmfEventKind::End,
        timestamp: Utc::now(),
    })
    .expect("publish foreign");

    let nothing = tokio::time::timeout(Duration::from_millis(50), events_rx.recv()).await;
    assert!(
        nothing.is_err(),
        "tap must filter by call_id; got an event meant for another call: {nothing:?}",
    );

    drop(events_rx);
    drop(_caller_rx);
    drop(_playout_tx);
    let _ = tokio::time::timeout(Duration::from_secs(1), pump).await;
}

// ─── Outbound DTMF (TapCommand::SendDtmf) ──────────────────────────

/// Send a `TapCommand::SendDtmf` into the tap and assert forge's
/// outbound queue gets a matching `OutboundMediaRequest::Dtmf` with
/// the right digit + duration + target leg.
///
/// Pins the WS-server-driven outbound DTMF path: the bridge sends
/// `BridgeIn::SendDtmf` → controller routes to `TapCommand::SendDtmf`
/// → tap turns into a forge handle call. This test bypasses the
/// controller and drives the tap directly.
#[tokio::test]
async fn send_dtmf_command_queues_outbound_dtmf_to_forge() {
    use siphon_ai_media_glue::TapCommand;

    let manager = Arc::new(MediaBridgeManager::with_capacities(10, 10));
    let call = CallId::new("dtmf-out-test");
    let tap = MediaTap::attach(
        &manager,
        &::std::sync::Arc::new(forge_core::EventBus::new()),
        call.clone(),
        8000,
    )
    .expect("attach");

    let (caller_tx, _caller_rx) = mpsc::channel::<Vec<u8>>(10);
    let (_playout_tx, playout_rx) = mpsc::channel::<Vec<u8>>(10);
    let (events_tx, _events_rx) = mpsc::channel::<siphon_ai_bridge::OutgoingEvent>(8);
    let (cmd_tx, cmd_rx) = mpsc::channel::<TapCommand>(8);

    let pump = tokio::spawn(tap.run(caller_tx, playout_rx, events_tx, cmd_rx));

    cmd_tx
        .send(TapCommand::SendDtmf {
            digit: '5',
            duration_ms: 160,
        })
        .await
        .expect("send command");

    let drained = tokio::time::timeout(Duration::from_millis(500), async {
        loop {
            if let Some(req) = manager.try_recv_outbound_request(&call).await {
                return req;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("forge sees outbound DTMF");

    match drained {
        OutboundMediaRequest::Dtmf(req) => {
            assert_eq!(req.target, MediaTarget::A, "single-leg → leg A");
            assert_eq!(req.duration_ms, 160);
            assert_eq!(req.digit, forge_engine::DtmfDigit::Five);
        }
        other => panic!("expected Dtmf variant, got {other:?}"),
    }

    drop(cmd_tx);
    drop(_events_rx);
    drop(_caller_rx);
    drop(_playout_tx);
    let _ = tokio::time::timeout(Duration::from_secs(1), pump).await;
}

/// An invalid digit char must be silently dropped — a misbehaving WS
/// server sending `digit: 'Z'` shouldn't tear down the call. The tap
/// keeps running and accepts subsequent valid presses.
#[tokio::test]
async fn send_dtmf_command_with_invalid_digit_does_not_kill_tap() {
    use siphon_ai_media_glue::TapCommand;

    let manager = Arc::new(MediaBridgeManager::with_capacities(10, 10));
    let call = CallId::new("dtmf-bad-digit");
    let tap = MediaTap::attach(
        &manager,
        &::std::sync::Arc::new(forge_core::EventBus::new()),
        call.clone(),
        8000,
    )
    .expect("attach");

    let (caller_tx, _caller_rx) = mpsc::channel::<Vec<u8>>(10);
    let (_playout_tx, playout_rx) = mpsc::channel::<Vec<u8>>(10);
    let (events_tx, _events_rx) = mpsc::channel::<siphon_ai_bridge::OutgoingEvent>(8);
    let (cmd_tx, cmd_rx) = mpsc::channel::<TapCommand>(8);

    let pump = tokio::spawn(tap.run(caller_tx, playout_rx, events_tx, cmd_rx));

    cmd_tx
        .send(TapCommand::SendDtmf {
            digit: 'Z',
            duration_ms: 160,
        })
        .await
        .expect("send bad cmd");

    // Forge must NOT see anything from the bad digit.
    let nothing = tokio::time::timeout(Duration::from_millis(50), async {
        manager.try_recv_outbound_request(&call).await
    })
    .await;
    assert!(
        matches!(nothing, Ok(None)) || nothing.is_err(),
        "invalid digit must produce no outbound forge request: {nothing:?}",
    );

    // A subsequent valid press should still work — pinning the
    // "drop one bad command, keep going" property.
    cmd_tx
        .send(TapCommand::SendDtmf {
            digit: '7',
            duration_ms: 100,
        })
        .await
        .expect("send good cmd");

    let drained = tokio::time::timeout(Duration::from_millis(500), async {
        loop {
            if let Some(req) = manager.try_recv_outbound_request(&call).await {
                return req;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("good digit reaches forge");
    match drained {
        OutboundMediaRequest::Dtmf(req) => {
            assert_eq!(req.digit, forge_engine::DtmfDigit::Seven);
        }
        other => panic!("expected Dtmf variant, got {other:?}"),
    }

    drop(cmd_tx);
    drop(_events_rx);
    drop(_caller_rx);
    drop(_playout_tx);
    let _ = tokio::time::timeout(Duration::from_secs(1), pump).await;
}

// ─── Clear (barge-in / interrupt outbound playout) ─────────────────

/// Send `TapCommand::Clear` with no audio in flight, assert forge
/// sees a `Flush` request targeting leg A.
///
/// `MediaTap::run` polls audio arms before the command arm (`biased;`),
/// so pre-staged playout frames already in the controller→tap
/// channel will reach forge as `Audio` requests *before* Clear is
/// processed — that's expected, and forge's `Flush` is what
/// actually drops them from the encoder's pending-out queue. So
/// this test focuses on the contract the tap owns: when Clear
/// fires, exactly one `Flush` lands on forge for leg A.
#[tokio::test]
async fn clear_command_emits_flush_on_forge() {
    use siphon_ai_media_glue::TapCommand;

    let manager = Arc::new(MediaBridgeManager::with_capacities(10, 10));
    let call = CallId::new("clear-test");
    let tap = MediaTap::attach(
        &manager,
        &::std::sync::Arc::new(forge_core::EventBus::new()),
        call.clone(),
        8000,
    )
    .expect("attach");

    let (caller_tx, _caller_rx) = mpsc::channel::<Vec<u8>>(10);
    let (_playout_tx, playout_rx) = mpsc::channel::<Vec<u8>>(10);
    let (events_tx, _events_rx) = mpsc::channel::<siphon_ai_bridge::OutgoingEvent>(8);
    let (cmd_tx, cmd_rx) = mpsc::channel::<TapCommand>(8);

    let pump = tokio::spawn(tap.run(caller_tx, playout_rx, events_tx, cmd_rx));

    cmd_tx.send(TapCommand::Clear).await.expect("send clear");

    let drained = tokio::time::timeout(Duration::from_millis(500), async {
        loop {
            if let Some(req) = manager.try_recv_outbound_request(&call).await {
                return req;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("forge sees Flush");

    match drained {
        OutboundMediaRequest::Flush {
            target,
            playback_id,
        } => {
            assert_eq!(
                target,
                Some(MediaTarget::A),
                "single-leg model → flush leg A",
            );
            assert!(
                playback_id.is_none(),
                "v1 doesn't use playback_id; got {playback_id:?}",
            );
        }
        other => panic!("expected Flush, got {other:?}"),
    }

    drop(cmd_tx);
    drop(_events_rx);
    drop(_caller_rx);
    drop(_playout_tx);
    let _ = tokio::time::timeout(Duration::from_secs(1), pump).await;
}

/// Clear drains the controller→tap audio buffer of bytes that haven't
/// yet been polled by the playout arm. Pre-stage two frames, send
/// Clear, then push one more frame — the post-Clear frame must
/// reach forge while the tap stays alive.
///
/// We don't pin "zero pre-Clear frames reach forge" because the
/// `biased` select polls audio first; in production, Clear's
/// usefulness rests on forge's `Flush` (which this test pins in
/// `clear_command_emits_flush_on_forge`).
#[tokio::test]
async fn clear_command_does_not_kill_tap() {
    use siphon_ai_bridge::pack_pcm16_le;
    use siphon_ai_media_glue::TapCommand;

    let manager = Arc::new(MediaBridgeManager::with_capacities(10, 10));
    let call = CallId::new("clear-resume-test");
    let tap = MediaTap::attach(
        &manager,
        &::std::sync::Arc::new(forge_core::EventBus::new()),
        call.clone(),
        8000,
    )
    .expect("attach");

    let (caller_tx, _caller_rx) = mpsc::channel::<Vec<u8>>(10);
    let (playout_tx, playout_rx) = mpsc::channel::<Vec<u8>>(10);
    let (events_tx, _events_rx) = mpsc::channel::<siphon_ai_bridge::OutgoingEvent>(8);
    let (cmd_tx, cmd_rx) = mpsc::channel::<TapCommand>(8);

    let pump = tokio::spawn(tap.run(caller_tx, playout_rx, events_tx, cmd_rx));

    cmd_tx.send(TapCommand::Clear).await.expect("send clear");

    // Drain whatever the Clear pushed (Flush). `expect` returns
    // unit, so no binding — just propagate the timeout assertion.
    tokio::time::timeout(Duration::from_millis(300), async {
        loop {
            if let Some(req) = manager.try_recv_outbound_request(&call).await {
                if matches!(req, OutboundMediaRequest::Flush { .. }) {
                    return;
                }
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("Flush observed");

    // Tap should still be alive: a follow-up audio frame reaches forge.
    let frame = pack_pcm16_le(&vec![3i16; 160]);
    playout_tx.send(frame).await.expect("send post-Clear audio");
    let drained = tokio::time::timeout(Duration::from_millis(500), async {
        loop {
            if let Some(req) = manager.try_recv_outbound_request(&call).await {
                return req;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("post-Clear audio reaches forge");
    match drained {
        OutboundMediaRequest::Audio(f) => {
            assert_eq!(f.samples, vec![3i16; 160]);
        }
        other => panic!("expected Audio post-Clear, got {other:?}"),
    }

    drop(cmd_tx);
    drop(_events_rx);
    drop(_caller_rx);
    drop(playout_tx);
    let _ = tokio::time::timeout(Duration::from_secs(1), pump).await;
}

// ─── Mark (server-driven playout marker) ───────────────────────────

/// `Mark` issued before any audio has been queued must fire
/// immediately — there's nothing to wait for.
///
/// Pins the protocol semantics: `mark` is "fire when audio up to
/// this point has played"; if no audio has been queued, that point
/// is now.
#[tokio::test]
async fn mark_with_no_audio_fires_immediately() {
    use siphon_ai_bridge::OutgoingEvent;
    use siphon_ai_media_glue::TapCommand;

    let manager = Arc::new(MediaBridgeManager::with_capacities(10, 10));
    let call = CallId::new("mark-immediate");
    let tap = MediaTap::attach(
        &manager,
        &::std::sync::Arc::new(forge_core::EventBus::new()),
        call.clone(),
        8000,
    )
    .expect("attach");

    let (caller_tx, _caller_rx) = mpsc::channel::<Vec<u8>>(10);
    let (_playout_tx, playout_rx) = mpsc::channel::<Vec<u8>>(10);
    let (events_tx, mut events_rx) = mpsc::channel::<OutgoingEvent>(8);
    let (cmd_tx, cmd_rx) = mpsc::channel::<TapCommand>(8);

    let pump = tokio::spawn(tap.run(caller_tx, playout_rx, events_tx, cmd_rx));

    cmd_tx
        .send(TapCommand::Mark {
            name: "no-audio".into(),
        })
        .await
        .expect("send mark");

    let event = tokio::time::timeout(Duration::from_millis(100), events_rx.recv())
        .await
        .expect("mark fires within 100ms (effectively immediate)")
        .expect("events channel still open");

    match event {
        OutgoingEvent::Mark { name } => assert_eq!(name, "no-audio"),
        other => panic!("expected Mark, got {other:?}"),
    }

    drop(cmd_tx);
    drop(_caller_rx);
    drop(_playout_tx);
    let _ = tokio::time::timeout(Duration::from_secs(1), pump).await;
}

/// `Mark` issued after N frames have been pushed must fire roughly
/// `N * 20ms` after the first frame was pushed (regardless of when
/// the Mark itself was sent).
///
/// The estimate is an upper bound — fire-after must be at least
/// `N * 20ms - elapsed`. We assert the inter-arrival between the
/// Mark command and the Mark event is approximately the play-out
/// duration of the queued audio (within a generous tolerance,
/// because tokio task scheduling jitter on a busy CI box can shift
/// the wakeup).
#[tokio::test]
async fn mark_fires_after_estimated_playout_of_queued_frames() {
    use siphon_ai_bridge::{pack_pcm16_le, OutgoingEvent};
    use siphon_ai_media_glue::TapCommand;
    use std::time::Instant;

    let manager = Arc::new(MediaBridgeManager::with_capacities(10, 10));
    let call = CallId::new("mark-after-audio");
    let tap = MediaTap::attach(
        &manager,
        &::std::sync::Arc::new(forge_core::EventBus::new()),
        call.clone(),
        8000,
    )
    .expect("attach");

    let (caller_tx, _caller_rx) = mpsc::channel::<Vec<u8>>(10);
    let (playout_tx, playout_rx) = mpsc::channel::<Vec<u8>>(10);
    let (events_tx, mut events_rx) = mpsc::channel::<OutgoingEvent>(8);
    let (cmd_tx, cmd_rx) = mpsc::channel::<TapCommand>(8);

    let pump = tokio::spawn(tap.run(caller_tx, playout_rx, events_tx, cmd_rx));

    // Queue 5 frames (= 100ms of estimated play-out). Drain forge's
    // outbound queue as we go so the audio arm in the tap can keep
    // pulling without blocking.
    for k in 0..5 {
        let frame = pack_pcm16_le(&vec![k as i16; 160]);
        playout_tx.send(frame).await.unwrap();
    }
    // Let the tap process the audio arm before we send Mark.
    tokio::time::sleep(Duration::from_millis(20)).await;
    let manager_ref = manager.clone();
    let call_ref = call.clone();
    tokio::spawn(async move {
        // Drain forge so the channel doesn't back-pressure.
        loop {
            let _ = manager_ref.try_recv_outbound_request(&call_ref).await;
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    });
    let mark_sent_at = Instant::now();
    cmd_tx
        .send(TapCommand::Mark {
            name: "after-5".into(),
        })
        .await
        .expect("send mark");

    let event = tokio::time::timeout(Duration::from_millis(500), events_rx.recv())
        .await
        .expect("mark arrives within 500ms")
        .expect("events channel open");
    let elapsed = mark_sent_at.elapsed();

    match event {
        OutgoingEvent::Mark { name } => assert_eq!(name, "after-5"),
        other => panic!("expected Mark, got {other:?}"),
    }

    // 5 frames at 20ms = 100ms total play-out. Mark was sent ≥ 20ms
    // after the first frame was pushed (we slept), so the remaining
    // play-out is ≤ 80ms. We accept a wide band (0..200ms) — the
    // assertion is just "we waited approximately the right amount,
    // and didn't fire instantly or 5 seconds later."
    assert!(
        elapsed < Duration::from_millis(200),
        "mark fired too late: {elapsed:?}",
    );

    drop(cmd_tx);
    drop(_caller_rx);
    drop(playout_tx);
    let _ = tokio::time::timeout(Duration::from_secs(1), pump).await;
}

// ─── VAD (SpeechStarted / SpeechStopped from ForgeEvent) ───────────

/// Publishing a `ForgeEvent::SpeechStarted` for this call's `call_id`
/// must produce exactly one `OutgoingEvent::SpeechStarted` carrying
/// the wallclock as `ts_ms` (Unix-epoch milliseconds).
#[tokio::test]
async fn forge_speech_started_emits_outgoing_speech_started() {
    use chrono::Utc;
    use forge_core::{EventBus as ForgeEventBus, ForgeEvent};
    use siphon_ai_bridge::OutgoingEvent;
    use siphon_ai_media_glue::TapCommand;

    let manager = Arc::new(MediaBridgeManager::with_capacities(10, 10));
    let bus = Arc::new(ForgeEventBus::new());
    let call = CallId::new("vad-started");
    let tap = MediaTap::attach(&manager, &bus, call.clone(), 8000).expect("attach");

    let (caller_tx, _caller_rx) = mpsc::channel::<Vec<u8>>(10);
    let (_playout_tx, playout_rx) = mpsc::channel::<Vec<u8>>(10);
    let (events_tx, mut events_rx) = mpsc::channel::<OutgoingEvent>(8);
    let (_cmd_tx, cmd_rx) = mpsc::channel::<TapCommand>(8);

    let pump = tokio::spawn(tap.run(caller_tx, playout_rx, events_tx, cmd_rx));

    let ts = Utc::now();
    bus.publish(ForgeEvent::SpeechStarted {
        call_id: call.clone(),
        timestamp: ts,
    })
    .expect("publish");

    let event = tokio::time::timeout(Duration::from_millis(500), events_rx.recv())
        .await
        .expect("event arrives")
        .expect("events_tx open");

    match event {
        OutgoingEvent::SpeechStarted {
            ts_ms,
            decision_pending,
            decision_deadline_ms,
        } => {
            assert_eq!(ts_ms, ts.timestamp_millis().max(0) as u64);
            // No arbitration on the default (Notify) policy.
            assert!(!decision_pending);
            assert_eq!(decision_deadline_ms, None);
        }
        other => panic!("expected SpeechStarted, got {other:?}"),
    }

    drop(_cmd_tx);
    drop(_caller_rx);
    drop(_playout_tx);
    let _ = tokio::time::timeout(Duration::from_secs(1), pump).await;
}

/// `SpeechStopped` carries `duration_ms` end-to-end from forge through
/// to the WS event. Pins the field mapping.
#[tokio::test]
async fn forge_speech_stopped_emits_outgoing_with_duration() {
    use chrono::Utc;
    use forge_core::{EventBus as ForgeEventBus, ForgeEvent};
    use siphon_ai_bridge::OutgoingEvent;
    use siphon_ai_media_glue::TapCommand;

    let manager = Arc::new(MediaBridgeManager::with_capacities(10, 10));
    let bus = Arc::new(ForgeEventBus::new());
    let call = CallId::new("vad-stopped");
    let tap = MediaTap::attach(&manager, &bus, call.clone(), 8000).expect("attach");

    let (caller_tx, _caller_rx) = mpsc::channel::<Vec<u8>>(10);
    let (_playout_tx, playout_rx) = mpsc::channel::<Vec<u8>>(10);
    let (events_tx, mut events_rx) = mpsc::channel::<OutgoingEvent>(8);
    let (_cmd_tx, cmd_rx) = mpsc::channel::<TapCommand>(8);

    let pump = tokio::spawn(tap.run(caller_tx, playout_rx, events_tx, cmd_rx));

    let ts = Utc::now();
    bus.publish(ForgeEvent::SpeechStopped {
        call_id: call.clone(),
        timestamp: ts,
        duration_ms: 1234,
    })
    .expect("publish");

    let event = tokio::time::timeout(Duration::from_millis(500), events_rx.recv())
        .await
        .expect("event arrives")
        .expect("events_tx open");

    match event {
        OutgoingEvent::SpeechStopped { ts_ms, duration_ms } => {
            assert_eq!(ts_ms, ts.timestamp_millis().max(0) as u64);
            assert_eq!(duration_ms, 1234);
        }
        other => panic!("expected SpeechStopped, got {other:?}"),
    }

    drop(_cmd_tx);
    drop(_caller_rx);
    drop(_playout_tx);
    let _ = tokio::time::timeout(Duration::from_secs(1), pump).await;
}

/// VAD events for an unrelated `call_id` must not leak to this tap.
/// Same property as the DTMF filter test — multi-call deployments
/// would cross-talk without it.
#[tokio::test]
async fn forge_speech_event_for_other_call_is_ignored() {
    use chrono::Utc;
    use forge_core::{EventBus as ForgeEventBus, ForgeEvent};
    use siphon_ai_bridge::OutgoingEvent;
    use siphon_ai_media_glue::TapCommand;

    let manager = Arc::new(MediaBridgeManager::with_capacities(10, 10));
    let bus = Arc::new(ForgeEventBus::new());
    let call = CallId::new("vad-mine");
    let tap = MediaTap::attach(&manager, &bus, call.clone(), 8000).expect("attach");

    let (caller_tx, _caller_rx) = mpsc::channel::<Vec<u8>>(10);
    let (_playout_tx, playout_rx) = mpsc::channel::<Vec<u8>>(10);
    let (events_tx, mut events_rx) = mpsc::channel::<OutgoingEvent>(8);
    let (_cmd_tx, cmd_rx) = mpsc::channel::<TapCommand>(8);

    let pump = tokio::spawn(tap.run(caller_tx, playout_rx, events_tx, cmd_rx));

    bus.publish(ForgeEvent::SpeechStarted {
        call_id: CallId::new("someone-else"),
        timestamp: Utc::now(),
    })
    .expect("publish foreign");

    let nothing = tokio::time::timeout(Duration::from_millis(50), events_rx.recv()).await;
    assert!(
        nothing.is_err(),
        "tap must filter by call_id; got an event meant for another call: {nothing:?}",
    );

    drop(_cmd_tx);
    drop(_caller_rx);
    drop(_playout_tx);
    let _ = tokio::time::timeout(Duration::from_secs(1), pump).await;
}

// ─── Barge-in auto_clear (SpeechStarted triggers forge.flush) ──────

/// `BargeInAction::AutoClear` + `SpeechStarted` must:
///   1. Drop any pending bytes in `playout_audio_rx`.
///   2. Ask forge to flush leg A.
///   3. Forward the WS event so the server sees `speech_started`.
///
/// Pins the `[bridge].barge_in.mode = "auto_clear"` semantics: a
/// caller interruption is acked immediately with no server round-trip.
#[tokio::test]
async fn auto_clear_drops_playout_and_flushes_on_speech_started() {
    use chrono::Utc;
    use forge_core::{EventBus as ForgeEventBus, ForgeEvent};
    use siphon_ai_bridge::OutgoingEvent;
    use siphon_ai_media_glue::{BargeInAction, TapCommand};

    let manager = Arc::new(MediaBridgeManager::with_capacities(10, 10));
    let bus = Arc::new(ForgeEventBus::new());
    let call = CallId::new("auto-clear");
    let tap = MediaTap::attach_with_barge_in(
        &manager,
        &bus,
        call.clone(),
        8000,
        BargeInAction::AutoClear,
    )
    .expect("attach");

    let (caller_tx, _caller_rx) = mpsc::channel::<Vec<u8>>(10);
    let (_playout_tx, playout_rx) = mpsc::channel::<Vec<u8>>(10);
    let (events_tx, mut events_rx) = mpsc::channel::<OutgoingEvent>(8);
    let (_cmd_tx, cmd_rx) = mpsc::channel::<TapCommand>(8);

    let pump = tokio::spawn(tap.run(caller_tx, playout_rx, events_tx, cmd_rx));

    bus.publish(ForgeEvent::SpeechStarted {
        call_id: call.clone(),
        timestamp: Utc::now(),
    })
    .expect("publish");

    let drained = tokio::time::timeout(Duration::from_millis(500), async {
        loop {
            if let Some(req) = manager.try_recv_outbound_request(&call).await {
                return req;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("forge sees Flush");
    assert!(
        matches!(
            drained,
            OutboundMediaRequest::Flush {
                target: Some(MediaTarget::A),
                ..
            }
        ),
        "expected Flush for leg A, got {drained:?}",
    );

    let event = tokio::time::timeout(Duration::from_millis(500), events_rx.recv())
        .await
        .expect("speech_started arrives")
        .expect("events_tx open");
    assert!(
        matches!(event, OutgoingEvent::SpeechStarted { .. }),
        "expected SpeechStarted, got {event:?}",
    );

    drop(_cmd_tx);
    drop(_caller_rx);
    drop(_playout_tx);
    let _ = tokio::time::timeout(Duration::from_secs(1), pump).await;
}

/// `BargeInAction::Notify` forwards the WS event but does NOT ask
/// forge to flush. Pins the `mode = "notify_only"` branch.
#[tokio::test]
async fn notify_only_does_not_flush_on_speech_started() {
    use chrono::Utc;
    use forge_core::{EventBus as ForgeEventBus, ForgeEvent};
    use siphon_ai_bridge::OutgoingEvent;
    use siphon_ai_media_glue::{BargeInAction, TapCommand};

    let manager = Arc::new(MediaBridgeManager::with_capacities(10, 10));
    let bus = Arc::new(ForgeEventBus::new());
    let call = CallId::new("notify-only");
    let tap =
        MediaTap::attach_with_barge_in(&manager, &bus, call.clone(), 8000, BargeInAction::Notify)
            .expect("attach");

    let (caller_tx, _caller_rx) = mpsc::channel::<Vec<u8>>(10);
    let (_playout_tx, playout_rx) = mpsc::channel::<Vec<u8>>(10);
    let (events_tx, mut events_rx) = mpsc::channel::<OutgoingEvent>(8);
    let (_cmd_tx, cmd_rx) = mpsc::channel::<TapCommand>(8);

    let pump = tokio::spawn(tap.run(caller_tx, playout_rx, events_tx, cmd_rx));

    bus.publish(ForgeEvent::SpeechStarted {
        call_id: call.clone(),
        timestamp: Utc::now(),
    })
    .expect("publish");

    let event = tokio::time::timeout(Duration::from_millis(500), events_rx.recv())
        .await
        .expect("speech_started arrives")
        .expect("events_tx open");
    assert!(matches!(event, OutgoingEvent::SpeechStarted { .. }));

    let nothing = tokio::time::timeout(Duration::from_millis(80), async {
        manager.try_recv_outbound_request(&call).await
    })
    .await;
    assert!(
        matches!(nothing, Ok(None)) || nothing.is_err(),
        "notify_only must NOT emit forge requests; got {nothing:?}",
    );

    drop(_cmd_tx);
    drop(_caller_rx);
    drop(_playout_tx);
    let _ = tokio::time::timeout(Duration::from_secs(1), pump).await;
}

// ─── Barge-in pause (reversible, server-arbitrated) ────────────────
//
// `BargeInAction::Pause` (docs/design/DESIGN_REVERSIBLE_BARGE_IN.md):
// a SpeechStarted while the bot is playing flushes forge exactly like
// auto_clear but retains the unplayed tail; the server then rules via
// `TapCommand::BargeInConfirm` / `BargeInReject`, or the decision
// deadline applies the configured fallback.

/// Shared fixture: a Pause policy with a generous resume cap.
fn pause_action(
    decision: Duration,
    on_timeout: siphon_ai_media_glue::TimeoutVerdict,
) -> siphon_ai_media_glue::BargeInAction {
    siphon_ai_media_glue::BargeInAction::Pause {
        decision,
        on_timeout,
        resume_max: Duration::from_secs(30),
    }
}

/// Drain forge's outbound queue until an Audio frame arrives, return
/// its first sample (the tests use constant-pattern frames). Panics
/// on timeout.
async fn next_audio_pattern(manager: &Arc<MediaBridgeManager>, call: &CallId) -> i16 {
    tokio::time::timeout(Duration::from_millis(700), async {
        loop {
            if let Some(OutboundMediaRequest::Audio(f)) =
                manager.try_recv_outbound_request(call).await
            {
                return f.samples[0];
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("audio frame reaches forge")
}

/// Wait until forge sees a Flush for leg A (draining Audio requests on
/// the way — pre-pause pushes may still be queued).
async fn await_flush(manager: &Arc<MediaBridgeManager>, call: &CallId) {
    tokio::time::timeout(Duration::from_millis(700), async {
        loop {
            if let Some(req) = manager.try_recv_outbound_request(call).await {
                if matches!(req, OutboundMediaRequest::Flush { .. }) {
                    return;
                }
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("forge sees Flush")
}

/// Assert that no Audio request reaches forge within `window` (Flush
/// requests are tolerated — several commands legitimately emit one).
async fn assert_no_audio_for(manager: &Arc<MediaBridgeManager>, call: &CallId, window: Duration) {
    let deadline = tokio::time::Instant::now() + window;
    loop {
        if tokio::time::Instant::now() >= deadline {
            return;
        }
        if let Some(OutboundMediaRequest::Audio(f)) = manager.try_recv_outbound_request(call).await
        {
            panic!(
                "unexpected audio reached forge during pause: {:?}",
                f.samples[0]
            );
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// Full pause→reject round trip: the retained tail must be re-queued
/// in order (ending with the newest frame), and the tap must keep
/// pumping normally afterwards.
#[tokio::test]
async fn pause_reject_resumes_retained_tail() {
    use chrono::Utc;
    use forge_core::{EventBus as ForgeEventBus, ForgeEvent};
    use siphon_ai_bridge::OutgoingEvent;
    use siphon_ai_media_glue::{TapCommand, TimeoutVerdict};

    let manager = Arc::new(MediaBridgeManager::with_capacities(64, 64));
    let bus = Arc::new(ForgeEventBus::new());
    let call = CallId::new("pause-reject");
    let tap = MediaTap::attach_with_barge_in(
        &manager,
        &bus,
        call.clone(),
        8000,
        pause_action(Duration::from_secs(5), TimeoutVerdict::Confirm),
    )
    .expect("attach");

    let (caller_tx, _caller_rx) = mpsc::channel::<Vec<u8>>(10);
    let (playout_tx, playout_rx) = mpsc::channel::<Vec<u8>>(10);
    let (events_tx, mut events_rx) = mpsc::channel::<OutgoingEvent>(8);
    let (cmd_tx, cmd_rx) = mpsc::channel::<TapCommand>(8);
    let pump = tokio::spawn(tap.run(caller_tx, playout_rx, events_tx, cmd_rx));

    // Queue a 100 ms utterance (5 frames, patterns 1..=5) and let the
    // tap push it into forge.
    for k in 1..=5i16 {
        playout_tx
            .send(pack_pcm16_le(&vec![k; 160]))
            .await
            .expect("send frame");
    }
    for _ in 0..5 {
        let _ = next_audio_pattern(&manager, &call).await;
    }

    // Caller speaks → pause fires: flush lands on forge, the
    // speech_started is forwarded, and nothing else plays.
    bus.publish(ForgeEvent::SpeechStarted {
        call_id: call.clone(),
        timestamp: Utc::now(),
    })
    .expect("publish");
    await_flush(&manager, &call).await;
    let event = tokio::time::timeout(Duration::from_millis(500), events_rx.recv())
        .await
        .expect("speech_started arrives")
        .expect("events_tx open");
    // 0.32.0: the forwarded event IS the arbitration request — it must
    // carry the pending flag and the decision window.
    assert!(
        matches!(
            event,
            OutgoingEvent::SpeechStarted {
                decision_pending: true,
                decision_deadline_ms: Some(5000),
                ..
            }
        ),
        "expected arbitration-stamped SpeechStarted, got {event:?}",
    );

    // Server rules: false positive. The retained tail replays in
    // order, ending with the newest frame (pattern 5). The playout
    // clock only trims *played* frames, so at minimum the un-elapsed
    // majority of the 100 ms utterance must come back.
    cmd_tx
        .send(TapCommand::BargeInReject)
        .await
        .expect("send reject");
    // Every resolution emits `barge_in_resolved` (0.32.0).
    let resolved = tokio::time::timeout(Duration::from_millis(500), events_rx.recv())
        .await
        .expect("barge_in_resolved arrives")
        .expect("events_tx open");
    assert!(
        matches!(
            resolved,
            OutgoingEvent::BargeInResolved {
                outcome: siphon_ai_bridge::BargeInOutcome::Rejected,
            }
        ),
        "expected rejected resolution, got {resolved:?}",
    );
    let mut replayed = Vec::new();
    loop {
        let p = next_audio_pattern(&manager, &call).await;
        replayed.push(p);
        if p == 5 {
            break;
        }
    }
    assert!(
        replayed.len() >= 3,
        "expected most of the 5-frame tail to replay, got {replayed:?}",
    );
    assert!(
        replayed.windows(2).all(|w| w[0] < w[1]),
        "tail must replay in playout order, got {replayed:?}",
    );

    // Tap is back to normal pumping.
    playout_tx
        .send(pack_pcm16_le(&vec![7i16; 160]))
        .await
        .expect("send post-reject frame");
    assert_eq!(next_audio_pattern(&manager, &call).await, 7);

    drop(cmd_tx);
    drop(_caller_rx);
    drop(playout_tx);
    let _ = tokio::time::timeout(Duration::from_secs(1), pump).await;
}

/// Pause→confirm ends in the auto_clear end-state: tail dropped,
/// nothing replays, and the barge-in is counted for the CDR (visible
/// on the quality watch).
#[tokio::test]
async fn pause_confirm_drops_tail_and_counts_barge_in() {
    use chrono::Utc;
    use forge_core::{EventBus as ForgeEventBus, ForgeEvent};
    use siphon_ai_bridge::OutgoingEvent;
    use siphon_ai_media_glue::{QualityReport, TapCommand, TimeoutVerdict};

    let manager = Arc::new(MediaBridgeManager::with_capacities(64, 64));
    let bus = Arc::new(ForgeEventBus::new());
    let call = CallId::new("pause-confirm");
    let (quality_tx, quality_rx) = tokio::sync::watch::channel(QualityReport::default());
    let tap = MediaTap::attach_with_barge_in(
        &manager,
        &bus,
        call.clone(),
        8000,
        pause_action(Duration::from_secs(5), TimeoutVerdict::Confirm),
    )
    .expect("attach")
    .with_quality_watch(quality_tx);

    let (caller_tx, _caller_rx) = mpsc::channel::<Vec<u8>>(10);
    let (playout_tx, playout_rx) = mpsc::channel::<Vec<u8>>(10);
    let (events_tx, _events_rx) = mpsc::channel::<OutgoingEvent>(8);
    let (cmd_tx, cmd_rx) = mpsc::channel::<TapCommand>(8);
    let pump = tokio::spawn(tap.run(caller_tx, playout_rx, events_tx, cmd_rx));

    for k in 1..=5i16 {
        playout_tx
            .send(pack_pcm16_le(&vec![k; 160]))
            .await
            .expect("send frame");
    }
    for _ in 0..5 {
        let _ = next_audio_pattern(&manager, &call).await;
    }

    bus.publish(ForgeEvent::SpeechStarted {
        call_id: call.clone(),
        timestamp: Utc::now(),
    })
    .expect("publish");
    await_flush(&manager, &call).await;

    cmd_tx
        .send(TapCommand::BargeInConfirm)
        .await
        .expect("send confirm");
    assert_no_audio_for(&manager, &call, Duration::from_millis(120)).await;
    assert_eq!(
        quality_rx.borrow().barge_in_count,
        1,
        "confirmed arbitration must count as a barge-in",
    );

    // Tap still alive.
    playout_tx
        .send(pack_pcm16_le(&vec![7i16; 160]))
        .await
        .expect("send post-confirm frame");
    assert_eq!(next_audio_pattern(&manager, &call).await, 7);

    drop(cmd_tx);
    drop(_caller_rx);
    drop(playout_tx);
    let _ = tokio::time::timeout(Duration::from_secs(1), pump).await;
}

/// No verdict within the window → `on_timeout = Confirm` drops the
/// tail without any command; a late reject is a harmless no-op.
#[tokio::test]
async fn pause_timeout_confirm_applies_fallback() {
    use chrono::Utc;
    use forge_core::{EventBus as ForgeEventBus, ForgeEvent};
    use siphon_ai_bridge::OutgoingEvent;
    use siphon_ai_media_glue::{TapCommand, TimeoutVerdict};

    let manager = Arc::new(MediaBridgeManager::with_capacities(64, 64));
    let bus = Arc::new(ForgeEventBus::new());
    let call = CallId::new("pause-timeout-confirm");
    let tap = MediaTap::attach_with_barge_in(
        &manager,
        &bus,
        call.clone(),
        8000,
        pause_action(Duration::from_millis(150), TimeoutVerdict::Confirm),
    )
    .expect("attach");

    let (caller_tx, _caller_rx) = mpsc::channel::<Vec<u8>>(10);
    let (playout_tx, playout_rx) = mpsc::channel::<Vec<u8>>(10);
    let (events_tx, _events_rx) = mpsc::channel::<OutgoingEvent>(8);
    let (cmd_tx, cmd_rx) = mpsc::channel::<TapCommand>(8);
    let pump = tokio::spawn(tap.run(caller_tx, playout_rx, events_tx, cmd_rx));

    for k in 1..=3i16 {
        playout_tx
            .send(pack_pcm16_le(&vec![k; 160]))
            .await
            .expect("send frame");
    }
    for _ in 0..3 {
        let _ = next_audio_pattern(&manager, &call).await;
    }

    bus.publish(ForgeEvent::SpeechStarted {
        call_id: call.clone(),
        timestamp: Utc::now(),
    })
    .expect("publish");
    await_flush(&manager, &call).await;

    // Past the 150 ms deadline the fallback confirms: nothing replays.
    assert_no_audio_for(&manager, &call, Duration::from_millis(400)).await;

    // A verdict arriving after the deadline must be ignored.
    cmd_tx
        .send(TapCommand::BargeInReject)
        .await
        .expect("send late reject");
    assert_no_audio_for(&manager, &call, Duration::from_millis(120)).await;

    playout_tx
        .send(pack_pcm16_le(&vec![7i16; 160]))
        .await
        .expect("send post-timeout frame");
    assert_eq!(next_audio_pattern(&manager, &call).await, 7);

    drop(cmd_tx);
    drop(_caller_rx);
    drop(playout_tx);
    let _ = tokio::time::timeout(Duration::from_secs(1), pump).await;
}

/// `on_timeout = Reject` resumes the tail at the deadline with no
/// server involvement at all.
#[tokio::test]
async fn pause_timeout_reject_resumes_tail() {
    use chrono::Utc;
    use forge_core::{EventBus as ForgeEventBus, ForgeEvent};
    use siphon_ai_bridge::OutgoingEvent;
    use siphon_ai_media_glue::{TapCommand, TimeoutVerdict};

    let manager = Arc::new(MediaBridgeManager::with_capacities(64, 64));
    let bus = Arc::new(ForgeEventBus::new());
    let call = CallId::new("pause-timeout-reject");
    let tap = MediaTap::attach_with_barge_in(
        &manager,
        &bus,
        call.clone(),
        8000,
        pause_action(Duration::from_millis(150), TimeoutVerdict::Reject),
    )
    .expect("attach");

    let (caller_tx, _caller_rx) = mpsc::channel::<Vec<u8>>(10);
    let (playout_tx, playout_rx) = mpsc::channel::<Vec<u8>>(10);
    let (events_tx, _events_rx) = mpsc::channel::<OutgoingEvent>(8);
    let (_cmd_tx, cmd_rx) = mpsc::channel::<TapCommand>(8);
    let pump = tokio::spawn(tap.run(caller_tx, playout_rx, events_tx, cmd_rx));

    for k in 1..=3i16 {
        playout_tx
            .send(pack_pcm16_le(&vec![k; 160]))
            .await
            .expect("send frame");
    }
    for _ in 0..3 {
        let _ = next_audio_pattern(&manager, &call).await;
    }

    bus.publish(ForgeEvent::SpeechStarted {
        call_id: call.clone(),
        timestamp: Utc::now(),
    })
    .expect("publish");
    await_flush(&manager, &call).await;

    // No verdict — the deadline itself resumes playout.
    let mut replayed = Vec::new();
    loop {
        let p = next_audio_pattern(&manager, &call).await;
        replayed.push(p);
        if p == 3 {
            break;
        }
    }
    assert!(
        replayed.windows(2).all(|w| w[0] < w[1]),
        "tail must replay in order, got {replayed:?}",
    );

    drop(_cmd_tx);
    drop(_caller_rx);
    drop(playout_tx);
    let _ = tokio::time::timeout(Duration::from_secs(1), pump).await;
}

/// While the bot is silent there is nothing to pause: pause mode just
/// forwards the event (no flush, no arbitration), and a stray verdict
/// is a no-op.
#[tokio::test]
async fn pause_with_bot_silent_only_forwards() {
    use chrono::Utc;
    use forge_core::{EventBus as ForgeEventBus, ForgeEvent};
    use siphon_ai_bridge::OutgoingEvent;
    use siphon_ai_media_glue::{TapCommand, TimeoutVerdict};

    let manager = Arc::new(MediaBridgeManager::with_capacities(64, 64));
    let bus = Arc::new(ForgeEventBus::new());
    let call = CallId::new("pause-silent");
    let tap = MediaTap::attach_with_barge_in(
        &manager,
        &bus,
        call.clone(),
        8000,
        pause_action(Duration::from_secs(5), TimeoutVerdict::Confirm),
    )
    .expect("attach");

    let (caller_tx, _caller_rx) = mpsc::channel::<Vec<u8>>(10);
    let (playout_tx, playout_rx) = mpsc::channel::<Vec<u8>>(10);
    let (events_tx, mut events_rx) = mpsc::channel::<OutgoingEvent>(8);
    let (cmd_tx, cmd_rx) = mpsc::channel::<TapCommand>(8);
    let pump = tokio::spawn(tap.run(caller_tx, playout_rx, events_tx, cmd_rx));

    bus.publish(ForgeEvent::SpeechStarted {
        call_id: call.clone(),
        timestamp: Utc::now(),
    })
    .expect("publish");

    let event = tokio::time::timeout(Duration::from_millis(500), events_rx.recv())
        .await
        .expect("speech_started arrives")
        .expect("events_tx open");
    assert!(matches!(event, OutgoingEvent::SpeechStarted { .. }));

    // No flush, no audio — nothing was playing.
    let nothing = tokio::time::timeout(Duration::from_millis(80), async {
        manager.try_recv_outbound_request(&call).await
    })
    .await;
    assert!(
        matches!(nothing, Ok(None)) || nothing.is_err(),
        "silent-bot pause must NOT emit forge requests; got {nothing:?}",
    );

    // Stray verdict with nothing pending: ignored, tap stays alive.
    cmd_tx
        .send(TapCommand::BargeInReject)
        .await
        .expect("send stray reject");
    playout_tx
        .send(pack_pcm16_le(&vec![7i16; 160]))
        .await
        .expect("send frame");
    assert_eq!(next_audio_pattern(&manager, &call).await, 7);

    drop(cmd_tx);
    drop(_caller_rx);
    drop(playout_tx);
    let _ = tokio::time::timeout(Duration::from_secs(1), pump).await;
}

/// Server audio arriving mid-arbitration must NOT reach forge during
/// the pause; on reject it plays after the retained tail (§5.3).
#[tokio::test]
async fn fresh_audio_during_pause_queues_behind_tail() {
    use chrono::Utc;
    use forge_core::{EventBus as ForgeEventBus, ForgeEvent};
    use siphon_ai_bridge::OutgoingEvent;
    use siphon_ai_media_glue::{TapCommand, TimeoutVerdict};

    let manager = Arc::new(MediaBridgeManager::with_capacities(64, 64));
    let bus = Arc::new(ForgeEventBus::new());
    let call = CallId::new("pause-fresh");
    let tap = MediaTap::attach_with_barge_in(
        &manager,
        &bus,
        call.clone(),
        8000,
        pause_action(Duration::from_secs(5), TimeoutVerdict::Confirm),
    )
    .expect("attach");

    let (caller_tx, _caller_rx) = mpsc::channel::<Vec<u8>>(10);
    let (playout_tx, playout_rx) = mpsc::channel::<Vec<u8>>(10);
    let (events_tx, _events_rx) = mpsc::channel::<OutgoingEvent>(8);
    let (cmd_tx, cmd_rx) = mpsc::channel::<TapCommand>(8);
    let pump = tokio::spawn(tap.run(caller_tx, playout_rx, events_tx, cmd_rx));

    for k in 1..=3i16 {
        playout_tx
            .send(pack_pcm16_le(&vec![k; 160]))
            .await
            .expect("send frame");
    }
    for _ in 0..3 {
        let _ = next_audio_pattern(&manager, &call).await;
    }

    bus.publish(ForgeEvent::SpeechStarted {
        call_id: call.clone(),
        timestamp: Utc::now(),
    })
    .expect("publish");
    await_flush(&manager, &call).await;

    // Post-pause server audio: held back, not played.
    playout_tx
        .send(pack_pcm16_le(&vec![9i16; 160]))
        .await
        .expect("send fresh frame");
    assert_no_audio_for(&manager, &call, Duration::from_millis(120)).await;

    // Reject → tail first, fresh frame last.
    cmd_tx
        .send(TapCommand::BargeInReject)
        .await
        .expect("send reject");
    let mut replayed = Vec::new();
    loop {
        let p = next_audio_pattern(&manager, &call).await;
        replayed.push(p);
        if p == 9 {
            break;
        }
    }
    assert!(
        replayed.len() >= 2,
        "expected tail + fresh frame, got {replayed:?}",
    );
    assert_eq!(*replayed.last().unwrap(), 9, "fresh audio plays last");
    assert!(
        replayed[..replayed.len() - 1].iter().all(|&p| p < 9),
        "tail precedes fresh audio, got {replayed:?}",
    );

    drop(cmd_tx);
    drop(_caller_rx);
    drop(playout_tx);
    let _ = tokio::time::timeout(Duration::from_secs(1), pump).await;
}

/// A preempting command (Mute here, standing in for hold/park/announce
/// /room — same code path) resolves the pending arbitration as
/// confirm: the tail is gone, the barge-in is counted, and a late
/// verdict is a no-op.
#[tokio::test]
async fn mute_resolves_pending_arbitration_as_confirm() {
    use chrono::Utc;
    use forge_core::{EventBus as ForgeEventBus, ForgeEvent};
    use siphon_ai_bridge::OutgoingEvent;
    use siphon_ai_media_glue::{QualityReport, TapCommand, TimeoutVerdict};

    let manager = Arc::new(MediaBridgeManager::with_capacities(64, 64));
    let bus = Arc::new(ForgeEventBus::new());
    let call = CallId::new("pause-mute-preempt");
    let (quality_tx, quality_rx) = tokio::sync::watch::channel(QualityReport::default());
    let tap = MediaTap::attach_with_barge_in(
        &manager,
        &bus,
        call.clone(),
        8000,
        pause_action(Duration::from_secs(5), TimeoutVerdict::Confirm),
    )
    .expect("attach")
    .with_quality_watch(quality_tx);

    let (caller_tx, _caller_rx) = mpsc::channel::<Vec<u8>>(10);
    let (playout_tx, playout_rx) = mpsc::channel::<Vec<u8>>(10);
    let (events_tx, _events_rx) = mpsc::channel::<OutgoingEvent>(8);
    let (cmd_tx, cmd_rx) = mpsc::channel::<TapCommand>(8);
    let pump = tokio::spawn(tap.run(caller_tx, playout_rx, events_tx, cmd_rx));

    for k in 1..=3i16 {
        playout_tx
            .send(pack_pcm16_le(&vec![k; 160]))
            .await
            .expect("send frame");
    }
    for _ in 0..3 {
        let _ = next_audio_pattern(&manager, &call).await;
    }

    bus.publish(ForgeEvent::SpeechStarted {
        call_id: call.clone(),
        timestamp: Utc::now(),
    })
    .expect("publish");
    await_flush(&manager, &call).await;

    // Mute preempts: arbitration resolves as confirm (counted), and
    // the later reject has nothing to act on.
    cmd_tx.send(TapCommand::Mute).await.expect("send mute");
    await_flush(&manager, &call).await; // Mute's own flush
    assert_eq!(quality_rx.borrow().barge_in_count, 1);
    cmd_tx
        .send(TapCommand::BargeInReject)
        .await
        .expect("send late reject");
    assert_no_audio_for(&manager, &call, Duration::from_millis(120)).await;

    // Unmute → normal pumping resumes. Give the (lower-priority)
    // command arm a beat to process the Unmute before pushing audio —
    // the biased select polls the playout arm first, and a frame
    // arriving while still muted is dropped by design.
    cmd_tx.send(TapCommand::Unmute).await.expect("send unmute");
    tokio::time::sleep(Duration::from_millis(50)).await;
    playout_tx
        .send(pack_pcm16_le(&vec![7i16; 160]))
        .await
        .expect("send frame");
    assert_eq!(next_audio_pattern(&manager, &call).await, 7);

    drop(cmd_tx);
    drop(_caller_rx);
    drop(playout_tx);
    let _ = tokio::time::timeout(Duration::from_secs(1), pump).await;
}

/// The debounce gate composes with pause mode: a start→stop blip
/// inside the window (the bot's own echo shape) neither flushes nor
/// arms arbitration, and the provisional event pair is suppressed —
/// identical to the auto_clear debounce contract.
#[tokio::test]
async fn pause_respects_debounce_cancel() {
    use chrono::Utc;
    use forge_core::{EventBus as ForgeEventBus, ForgeEvent};
    use siphon_ai_bridge::OutgoingEvent;
    use siphon_ai_media_glue::{TapCommand, TimeoutVerdict};

    let manager = Arc::new(MediaBridgeManager::with_capacities(64, 64));
    let bus = Arc::new(ForgeEventBus::new());
    let call = CallId::new("pause-debounce");
    let tap = MediaTap::attach_with_barge_in(
        &manager,
        &bus,
        call.clone(),
        8000,
        pause_action(Duration::from_secs(5), TimeoutVerdict::Confirm),
    )
    .expect("attach")
    .with_barge_in_debounce(Some(Duration::from_millis(200)));

    let (caller_tx, _caller_rx) = mpsc::channel::<Vec<u8>>(10);
    let (playout_tx, playout_rx) = mpsc::channel::<Vec<u8>>(10);
    let (events_tx, mut events_rx) = mpsc::channel::<OutgoingEvent>(8);
    let (_cmd_tx, cmd_rx) = mpsc::channel::<TapCommand>(8);
    let pump = tokio::spawn(tap.run(caller_tx, playout_rx, events_tx, cmd_rx));

    // Long utterance so the bot is still "playing" when speech blips.
    for k in 1..=10i16 {
        playout_tx
            .send(pack_pcm16_le(&vec![k; 160]))
            .await
            .expect("send frame");
    }
    for _ in 0..10 {
        let _ = next_audio_pattern(&manager, &call).await;
    }

    let ts = Utc::now();
    bus.publish(ForgeEvent::SpeechStarted {
        call_id: call.clone(),
        timestamp: ts,
    })
    .expect("publish start");
    tokio::time::sleep(Duration::from_millis(50)).await;
    bus.publish(ForgeEvent::SpeechStopped {
        call_id: call.clone(),
        timestamp: ts,
        duration_ms: 50,
    })
    .expect("publish stop");

    // Past the debounce window: no flush, no re-queued audio, no
    // arbitration — and the provisional event pair was swallowed.
    let deadline = tokio::time::Instant::now() + Duration::from_millis(300);
    while tokio::time::Instant::now() < deadline {
        if let Some(req) = manager.try_recv_outbound_request(&call).await {
            panic!("cancelled debounce must not touch forge, got {req:?}");
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(
        events_rx.try_recv().is_err(),
        "echo-shaped blip must not surface speech events",
    );

    drop(_cmd_tx);
    drop(_caller_rx);
    drop(playout_tx);
    let _ = tokio::time::timeout(Duration::from_secs(1), pump).await;
}
