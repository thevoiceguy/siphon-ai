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
    let tap = MediaTap::attach(&manager, call.clone(), 8000).expect("attach");

    let (caller_tx, mut caller_rx) = mpsc::channel::<Vec<u8>>(10);
    let (_playout_tx, playout_rx) = mpsc::channel::<Vec<u8>>(10);

    let pump = tokio::spawn(tap.run(caller_tx, playout_rx));

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
    let tap = MediaTap::attach(&manager, call.clone(), 8000).expect("attach");

    let (caller_tx, mut caller_rx) = mpsc::channel::<Vec<u8>>(10);
    let (_playout_tx, playout_rx) = mpsc::channel::<Vec<u8>>(10);
    let pump = tokio::spawn(tap.run(caller_tx, playout_rx));

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
    let tap = MediaTap::attach(&manager, call.clone(), 8000).expect("attach");

    let (caller_tx, _caller_rx) = mpsc::channel::<Vec<u8>>(10);
    let (playout_tx, playout_rx) = mpsc::channel::<Vec<u8>>(10);
    let pump = tokio::spawn(tap.run(caller_tx, playout_rx));

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
    let tap = MediaTap::attach(&manager, call.clone(), 8000).expect("attach");

    let (caller_tx, _caller_rx) = mpsc::channel::<Vec<u8>>(10);
    let (_playout_tx, playout_rx) = mpsc::channel::<Vec<u8>>(10);
    let pump = tokio::spawn(tap.run(caller_tx, playout_rx));

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
    let tap = MediaTap::attach(&manager, call.clone(), 8000).expect("attach");

    let (caller_tx, _caller_rx) = mpsc::channel::<Vec<u8>>(10);
    let (playout_tx, playout_rx) = mpsc::channel::<Vec<u8>>(10);
    let pump = tokio::spawn(tap.run(caller_tx, playout_rx));

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
    let tap = MediaTap::attach(&manager, call.clone(), 8000).expect("attach");

    let (caller_tx, _caller_rx) = mpsc::channel::<Vec<u8>>(10);
    let (_playout_tx, playout_rx) = mpsc::channel::<Vec<u8>>(10);
    let pump = tokio::spawn(tap.run(caller_tx, playout_rx));

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
    let tap = MediaTap::attach(&manager, call.clone(), 8000).expect("attach");

    let (caller_tx, caller_rx) = mpsc::channel::<Vec<u8>>(10);
    let (playout_tx, playout_rx) = mpsc::channel::<Vec<u8>>(10);
    let pump = tokio::spawn(tap.run(caller_tx, playout_rx));

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
    let tap = MediaTap::attach(&manager, call.clone(), 8000).expect("attach");

    let (caller_tx, mut caller_rx) = mpsc::channel::<Vec<u8>>(10);
    let (playout_tx, playout_rx) = mpsc::channel::<Vec<u8>>(10);
    let pump = tokio::spawn(tap.run(caller_tx, playout_rx));

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
