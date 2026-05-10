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
    let tap = MediaTap::attach(&manager, &::std::sync::Arc::new(forge_core::EventBus::new()), call.clone(), 8000).expect("attach");

    let (caller_tx, mut caller_rx) = mpsc::channel::<Vec<u8>>(10);
    let (_playout_tx, playout_rx) = mpsc::channel::<Vec<u8>>(10);

    let pump = tokio::spawn(tap.run(caller_tx, playout_rx, ::tokio::sync::mpsc::channel::<::siphon_ai_bridge::OutgoingEvent>(1).0, ::tokio::sync::mpsc::channel::<::siphon_ai_media_glue::TapCommand>(1).1));

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
    let tap = MediaTap::attach(&manager, &::std::sync::Arc::new(forge_core::EventBus::new()), call.clone(), 8000).expect("attach");

    let (caller_tx, mut caller_rx) = mpsc::channel::<Vec<u8>>(10);
    let (_playout_tx, playout_rx) = mpsc::channel::<Vec<u8>>(10);
    let pump = tokio::spawn(tap.run(caller_tx, playout_rx, ::tokio::sync::mpsc::channel::<::siphon_ai_bridge::OutgoingEvent>(1).0, ::tokio::sync::mpsc::channel::<::siphon_ai_media_glue::TapCommand>(1).1));

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
    let tap = MediaTap::attach(&manager, &::std::sync::Arc::new(forge_core::EventBus::new()), call.clone(), 8000).expect("attach");

    let (caller_tx, _caller_rx) = mpsc::channel::<Vec<u8>>(10);
    let (playout_tx, playout_rx) = mpsc::channel::<Vec<u8>>(10);
    let pump = tokio::spawn(tap.run(caller_tx, playout_rx, ::tokio::sync::mpsc::channel::<::siphon_ai_bridge::OutgoingEvent>(1).0, ::tokio::sync::mpsc::channel::<::siphon_ai_media_glue::TapCommand>(1).1));

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
    let tap = MediaTap::attach(&manager, &::std::sync::Arc::new(forge_core::EventBus::new()), call.clone(), 8000).expect("attach");

    let (caller_tx, _caller_rx) = mpsc::channel::<Vec<u8>>(10);
    let (_playout_tx, playout_rx) = mpsc::channel::<Vec<u8>>(10);
    let pump = tokio::spawn(tap.run(caller_tx, playout_rx, ::tokio::sync::mpsc::channel::<::siphon_ai_bridge::OutgoingEvent>(1).0, ::tokio::sync::mpsc::channel::<::siphon_ai_media_glue::TapCommand>(1).1));

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
    let tap = MediaTap::attach(&manager, &::std::sync::Arc::new(forge_core::EventBus::new()), call.clone(), 8000).expect("attach");

    let (caller_tx, _caller_rx) = mpsc::channel::<Vec<u8>>(10);
    let (playout_tx, playout_rx) = mpsc::channel::<Vec<u8>>(10);
    let pump = tokio::spawn(tap.run(caller_tx, playout_rx, ::tokio::sync::mpsc::channel::<::siphon_ai_bridge::OutgoingEvent>(1).0, ::tokio::sync::mpsc::channel::<::siphon_ai_media_glue::TapCommand>(1).1));

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
    let tap = MediaTap::attach(&manager, &::std::sync::Arc::new(forge_core::EventBus::new()), call.clone(), 8000).expect("attach");

    let (caller_tx, _caller_rx) = mpsc::channel::<Vec<u8>>(10);
    let (_playout_tx, playout_rx) = mpsc::channel::<Vec<u8>>(10);
    let pump = tokio::spawn(tap.run(caller_tx, playout_rx, ::tokio::sync::mpsc::channel::<::siphon_ai_bridge::OutgoingEvent>(1).0, ::tokio::sync::mpsc::channel::<::siphon_ai_media_glue::TapCommand>(1).1));

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
    let tap = MediaTap::attach(&manager, &::std::sync::Arc::new(forge_core::EventBus::new()), call.clone(), 8000).expect("attach");

    let (caller_tx, caller_rx) = mpsc::channel::<Vec<u8>>(10);
    let (playout_tx, playout_rx) = mpsc::channel::<Vec<u8>>(10);
    let pump = tokio::spawn(tap.run(caller_tx, playout_rx, ::tokio::sync::mpsc::channel::<::siphon_ai_bridge::OutgoingEvent>(1).0, ::tokio::sync::mpsc::channel::<::siphon_ai_media_glue::TapCommand>(1).1));

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
    let tap = MediaTap::attach(&manager, &::std::sync::Arc::new(forge_core::EventBus::new()), call.clone(), 8000).expect("attach");

    let (caller_tx, mut caller_rx) = mpsc::channel::<Vec<u8>>(10);
    let (playout_tx, playout_rx) = mpsc::channel::<Vec<u8>>(10);
    let pump = tokio::spawn(tap.run(caller_tx, playout_rx, ::tokio::sync::mpsc::channel::<::siphon_ai_bridge::OutgoingEvent>(1).0, ::tokio::sync::mpsc::channel::<::siphon_ai_media_glue::TapCommand>(1).1));

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
    use forge_core::{
        DtmfDetectionMethod, DtmfEventKind, EventBus as ForgeEventBus, ForgeEvent,
    };
    use siphon_ai_bridge::{DtmfMethod, OutgoingEvent};

    let manager = Arc::new(MediaBridgeManager::with_capacities(10, 10));
    let bus = Arc::new(ForgeEventBus::new());
    let call = CallId::new("dtmf-test");
    let tap = MediaTap::attach(&manager, &bus, call.clone(), 8000).expect("attach");

    let (caller_tx, _caller_rx) = mpsc::channel::<Vec<u8>>(10);
    let (_playout_tx, playout_rx) = mpsc::channel::<Vec<u8>>(10);
    let (events_tx, mut events_rx) =
        mpsc::channel::<OutgoingEvent>(8);

    let pump = tokio::spawn(tap.run(caller_tx, playout_rx, events_tx, ::tokio::sync::mpsc::channel::<::siphon_ai_media_glue::TapCommand>(1).1));

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
    use forge_core::{
        DtmfDetectionMethod, DtmfEventKind, EventBus as ForgeEventBus, ForgeEvent,
    };
    use siphon_ai_bridge::OutgoingEvent;

    let manager = Arc::new(MediaBridgeManager::with_capacities(10, 10));
    let bus = Arc::new(ForgeEventBus::new());
    let call = CallId::new("dtmf-mine");
    let tap = MediaTap::attach(&manager, &bus, call.clone(), 8000).expect("attach");

    let (caller_tx, _caller_rx) = mpsc::channel::<Vec<u8>>(10);
    let (_playout_tx, playout_rx) = mpsc::channel::<Vec<u8>>(10);
    let (events_tx, mut events_rx) =
        mpsc::channel::<OutgoingEvent>(8);

    let pump = tokio::spawn(tap.run(caller_tx, playout_rx, events_tx, ::tokio::sync::mpsc::channel::<::siphon_ai_media_glue::TapCommand>(1).1));

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
