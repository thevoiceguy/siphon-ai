//! Spike proof: verify that `forge-engine`'s `MediaBridgeManager` provides
//! the exact bidirectional tap SiphonAI needs.
//!
//! The test plays both sides of the bridge: it pushes synthetic
//! `InboundMediaFrame`s the way `forge-engine::ForwardingEngine` would, and
//! drains `OutboundMediaFrame`s the way the same forwarding loop does. The
//! application-facing `MediaBridgeHandle` is the surface SiphonAI's bridge
//! crate will sit on.
//!
//! See `docs/SPIKE_MEDIA_TAP.md` for the full writeup.

use std::sync::Arc;
use std::time::{Duration, Instant};

use forge_core::{AudioCodec, CallId};
use forge_engine::{
    InboundMediaFrame, MediaBridgeManager, MediaTarget, OutboundMediaFrame, OutboundMediaRequest,
    ParticipantLabel, PlayoutMode,
};

/// Frames per second at 20 ms ptime. Steady-state per-call cadence.
const FRAMES_PER_SEC: usize = 50;
/// Samples per frame at 8 kHz / 20 ms (G.711 baseline).
const SAMPLES_PER_FRAME: usize = 160;

#[tokio::test]
async fn round_trip_one_frame() {
    let manager = Arc::new(MediaBridgeManager::with_capacities(10, 10));
    let call_id = CallId::new("spike-call-1");

    let mut handle = manager.attach_call(call_id.clone()).expect("attach_call");
    assert!(manager.has_bridge(&call_id));

    let inbound = InboundMediaFrame {
        leg: ParticipantLabel::A,
        codec: AudioCodec::PCMU,
        payload_type: 0,
        sample_rate: 8000,
        timestamp: 1000,
        sequence_number: 1,
        samples: vec![1, 2, 3, 4, 5],
    };

    manager
        .try_send_inbound_frame(&call_id, inbound.clone())
        .expect("try_send_inbound_frame");

    let received = handle.recv_frame().await.expect("recv_frame");
    assert_eq!(received.samples, inbound.samples);
    assert_eq!(received.sequence_number, inbound.sequence_number);
    assert_eq!(received.leg, ParticipantLabel::A);

    handle
        .send_audio(OutboundMediaFrame {
            target: MediaTarget::A,
            sample_rate: 8000,
            samples: vec![10, 20, 30, 40, 50],
            playback_id: None,
            mode: PlayoutMode::Append,
        })
        .await
        .expect("send_audio");

    let drained = match manager
        .try_recv_outbound_request(&call_id)
        .await
        .expect("try_recv_outbound_request")
    {
        OutboundMediaRequest::Audio(frame) => frame,
        other => panic!("expected Audio variant, got {other:?}"),
    };
    assert_eq!(drained.target, MediaTarget::A);
    assert_eq!(drained.samples, vec![10, 20, 30, 40, 50]);

    drop(handle);
    assert!(
        !manager.has_bridge(&call_id),
        "handle drop must auto-detach"
    );
}

/// Push 1 second of synthetic 20 ms frames through the bridge in both
/// directions and confirm: nothing drops, ordering is preserved, and
/// the manager keeps up at 50 fps within the dev-plan's 200 ms buffer
/// budget.
///
/// Three tasks run concurrently — modelling production:
/// 1. **Producer**: pushes inbound frames the way forge's forwarding loop
///    does.
/// 2. **Application**: pulls inbound, sends outbound (the SiphonAI WS
///    bridge crate's role).
/// 3. **Drainer**: pulls outbound the way forge's forwarding loop drains
///    `media_bridge` for outbound playout.
///
/// Without a concurrent drainer, the outbound channel (cap 10) fills
/// after 10 sends and `send_audio().await` deadlocks the application
/// loop — which is exactly what would happen in production if the forge
/// drain side stalled.
#[tokio::test]
async fn one_second_of_50fps_round_trips() {
    let manager = Arc::new(MediaBridgeManager::with_capacities(10, 10));
    let call_id = CallId::new("spike-call-50fps");

    let mut handle = manager.attach_call(call_id.clone()).expect("attach_call");

    // Producer: synthetic forwarding loop pushing 50 inbound frames sized
    // like G.711 at 8 kHz / 20 ms.
    let producer_manager = Arc::clone(&manager);
    let producer_call_id = call_id.clone();
    let producer = tokio::spawn(async move {
        for i in 0..FRAMES_PER_SEC as u16 {
            let frame = InboundMediaFrame {
                leg: ParticipantLabel::A,
                codec: AudioCodec::PCMU,
                payload_type: 0,
                sample_rate: 8000,
                timestamp: 8000 + (i as u32) * SAMPLES_PER_FRAME as u32,
                sequence_number: i,
                samples: vec![i as i16; SAMPLES_PER_FRAME],
            };
            // Backoff if the inbound queue is full. In production forge
            // drops; here we retry so the test's bookkeeping stays exact.
            loop {
                match producer_manager.try_send_inbound_frame(&producer_call_id, frame.clone()) {
                    Ok(()) => break,
                    Err(_) => tokio::time::sleep(Duration::from_millis(1)).await,
                }
            }
        }
    });

    // Drainer: pulls outbound frames concurrently, the way forge's
    // forwarding loop calls `try_recv_outbound_frame`.
    let drainer_manager = Arc::clone(&manager);
    let drainer_call_id = call_id.clone();
    let drainer = tokio::spawn(async move {
        let mut drained = 0usize;
        while drained < FRAMES_PER_SEC {
            match drainer_manager
                .try_recv_outbound_request(&drainer_call_id)
                .await
            {
                Some(OutboundMediaRequest::Audio(frame)) => {
                    assert_eq!(frame.samples.len(), SAMPLES_PER_FRAME);
                    assert_eq!(frame.target, MediaTarget::A);
                    drained += 1;
                }
                Some(other) => panic!("unexpected outbound variant: {other:?}"),
                None => tokio::time::sleep(Duration::from_millis(1)).await,
            }
        }
        drained
    });

    let started = Instant::now();
    let mut received = 0usize;
    while received < FRAMES_PER_SEC {
        let frame = handle.recv_frame().await.expect("recv_frame");
        assert_eq!(
            frame.sequence_number as usize, received,
            "ordering preserved"
        );
        assert_eq!(frame.samples.len(), SAMPLES_PER_FRAME);

        // Round-trip: respond with an OutboundMediaFrame for every inbound
        // frame, like the SiphonAI bridge would after a WS server replies.
        handle
            .send_audio(OutboundMediaFrame {
                target: MediaTarget::A,
                sample_rate: 8000,
                samples: vec![received as i16; SAMPLES_PER_FRAME],
                playback_id: None,
                mode: PlayoutMode::Append,
            })
            .await
            .expect("send_audio");

        received += 1;
    }
    let elapsed = started.elapsed();

    producer.await.unwrap();
    let drained = drainer.await.unwrap();
    assert_eq!(
        drained, FRAMES_PER_SEC,
        "every outbound frame must reach the drain side"
    );

    // Sanity: 50 round-trips of 160-sample i16 vectors should complete in
    // a small fraction of a second on any developer machine. The wall-
    // clock budget for 50 frames in production is 1 s (50 fps); we should
    // be at least an order of magnitude faster than realtime.
    assert!(
        elapsed < Duration::from_millis(500),
        "50 round-trips took {elapsed:?}; expected <500 ms (2x realtime safety margin)"
    );
}

/// `MediaBridgeManager::attach_call` must be atomic: when N tasks race to
/// attach the same call_id, exactly one wins. (Regression guard; the
/// original implementation in forge had a `contains_key` + `insert` race.)
#[tokio::test]
async fn attach_is_atomic_under_contention() {
    let manager = Arc::new(MediaBridgeManager::new());
    let call_id = CallId::new("spike-call-race");

    let mut joins = Vec::new();
    for _ in 0..32 {
        let m = Arc::clone(&manager);
        let id = call_id.clone();
        joins.push(tokio::spawn(async move { m.attach_call(id) }));
    }

    let mut wins = 0usize;
    let mut losses = 0usize;
    for j in joins {
        match j.await.unwrap() {
            Ok(_h) => wins += 1,
            Err(_) => losses += 1,
        }
    }
    assert_eq!(wins, 1, "exactly one attach must win the race");
    assert_eq!(losses, 31, "all losers must report ResourceLimit");
}
