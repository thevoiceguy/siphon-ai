//! Metrics emitted by `BridgingAcceptor` end-to-end.
//!
//! Uses `metrics::with_local_recorder` to install a per-test
//! Prometheus recorder, drives a real call through `prepare_call` →
//! `run_call`, asserts on the rendered `/metrics`-style text. The
//! local-recorder approach avoids touching the global `metrics`
//! state so tests run in parallel without leaking into each other.

use std::sync::Arc;
use std::time::Duration;

use forge_engine::{MediaBridgeManager, SessionManager, SessionManagerConfig};
use forge_rtp::PortPoolConfig;
use siphon_ai_bridge::CallId as BridgeCallId;
use siphon_ai_core::{BridgeDefaults, BridgingAcceptor, CallRegistry};
use siphon_ai_media_glue::MediaSetup;
use siphon_ai_sip_glue::InviteFacts;
use siphon_ai_telemetry::{
    prometheus_builder, register_descriptions, CALLS_ACTIVE, CALLS_TOTAL, CALL_DURATION_SECONDS,
    INVITES_TOTAL, ROUTE_MATCH_TOTAL, SDP_NEGOTIATE_SECONDS,
};

mod common;
use common::{
    invite, one_route, server_acks_start_then_idles, G729_ONLY_OFFER, LINPHONE_PCMU_OFFER,
};

fn build_acceptor() -> (BridgingAcceptor, Arc<MediaBridgeManager>, CallRegistry) {
    let session_mgr = SessionManager::new(
        SessionManagerConfig {
            port_pool_config: PortPoolConfig::new(60500, 60600).unwrap(),
            ..Default::default()
        },
        None,
    );
    let bridge_mgr = Arc::new(MediaBridgeManager::new());
    let media = Arc::new(MediaSetup::new(
        Arc::clone(&session_mgr),
        Arc::clone(&bridge_mgr),
        Arc::new(forge_core::EventBus::new()),
        "192.168.1.10",
    ));
    let registry = CallRegistry::new();
    let acceptor = BridgingAcceptor::new(media, BridgeDefaults::default(), registry.clone())
        .with_call_id_factory(Arc::new(|| BridgeCallId::new("siphon-metrics-test")));
    (acceptor, bridge_mgr, registry)
}

#[tokio::test(flavor = "current_thread")]
async fn full_call_lifecycle_emits_expected_metrics() {
    // Per-test recorder; the `LocalRecorderGuard` is thread-local
    // and the `current_thread` flavor pins all spawned tasks to the
    // same OS thread, so `metrics::counter!` calls inside the
    // acceptor's spawned task see this recorder.
    let recorder = prometheus_builder().unwrap().build_recorder();
    let handle = recorder.handle();
    let _guard = metrics::set_default_local_recorder(&recorder);
    register_descriptions();

    let (port_tx, port_rx) = tokio::sync::oneshot::channel();
    tokio::spawn(server_acks_start_then_idles(port_tx));
    let port = port_rx.await.unwrap();
    let ws_url = format!("ws://127.0.0.1:{port}/");

    let (acceptor, _bridge_mgr, registry) = build_acceptor();
    let routes = one_route("metrics_route", &ws_url);
    let route = routes.iter().next().unwrap();
    let req = invite(
        LINPHONE_PCMU_OFFER,
        "sip:5000@siphon.example.com",
        "abc-metrics@pbx",
    );
    let facts = InviteFacts::extract(&req);

    let prepared = acceptor.prepare_call(&req, route, &facts).await.unwrap();
    // Mirror what on_matched does on the accept path so the
    // accepted-counter increment is exercised here too.
    metrics::counter!(INVITES_TOTAL, "result" => "accepted").increment(1);
    let run_handle = acceptor.run_call(prepared, "metrics_route", None);

    tokio::time::sleep(Duration::from_millis(150)).await;
    let h = registry.lookup("abc-metrics@pbx").expect("registered");
    h.shutdown();

    tokio::time::timeout(Duration::from_secs(3), run_handle)
        .await
        .expect("run_call completes")
        .expect("task does not panic");

    let out = handle.render();

    // Counters
    assert!(
        out.contains(&format!("{INVITES_TOTAL}{{result=\"accepted\"}} 1")),
        "missing accepted counter:\n{out}"
    );
    assert!(
        out.contains(&format!("{ROUTE_MATCH_TOTAL}{{route=\"metrics_route\"}} 1")),
        "missing route counter:\n{out}"
    );
    assert!(
        out.contains(&format!("{CALLS_TOTAL}{{cause=\"local_shutdown\"}} 1")),
        "missing calls_total local_shutdown:\n{out}"
    );

    // Active gauge: should have been 1 during the call, decremented
    // back to 0 by exit.
    assert!(
        out.contains(&format!("{CALLS_ACTIVE} 0")),
        "active gauge should be 0 after exit:\n{out}"
    );

    // Histograms: at least one bucket touched, count > 0.
    assert!(
        out.contains(&format!("{SDP_NEGOTIATE_SECONDS}_count")),
        "missing sdp_negotiate histogram:\n{out}"
    );
    assert!(
        out.contains(&format!("{CALL_DURATION_SECONDS}_count")),
        "missing call_duration histogram:\n{out}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn rejected_invite_increments_rejected_counter() {
    let recorder = prometheus_builder().unwrap().build_recorder();
    let handle = recorder.handle();
    let _guard = metrics::set_default_local_recorder(&recorder);
    register_descriptions();

    let (acceptor, _, _) = build_acceptor();
    let routes = one_route("metrics_route", "wss://x/y");
    let route = routes.iter().next().unwrap();
    let req = invite(
        G729_ONLY_OFFER,
        "sip:5000@siphon.example.com",
        "abc-rej@pbx",
    );
    let facts = InviteFacts::extract(&req);

    let err = acceptor
        .prepare_call(&req, route, &facts)
        .await
        .unwrap_err();
    // Mirror what `on_matched` does on the reject path so we hit
    // the same metric increments without fabricating a
    // ServerTransactionHandle.
    assert_eq!(err.sip_status().0, 488);
    metrics::counter!(INVITES_TOTAL, "result" => "rejected").increment(1);

    let out = handle.render();
    // SDP negotiate histogram should have an `error` result entry.
    assert!(
        out.contains(&format!(
            "{SDP_NEGOTIATE_SECONDS}_count{{result=\"error\"}}"
        )),
        "expected error-result histogram entry:\n{out}"
    );
    assert!(
        out.contains(&format!("{INVITES_TOTAL}{{result=\"rejected\"}} 1")),
        "missing rejected counter:\n{out}"
    );
}
