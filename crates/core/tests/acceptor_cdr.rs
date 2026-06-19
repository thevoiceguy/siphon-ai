//! End-to-end CDR emission: drive a call from `prepare_call` →
//! `run_call` against a real WS server and a recording CDR sink.
//! Confirms the spawned task emits exactly one CDR record with the
//! shape consumers will see.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use forge_engine::{MediaBridgeManager, SessionManager, SessionManagerConfig};
use forge_rtp::PortPoolConfig;
use parking_lot::Mutex;
use siphon_ai_bridge::CallId as BridgeCallId;
use siphon_ai_cdr::{CdrRecord, CdrSink, Direction as CdrDirection, TerminationCause};
use siphon_ai_core::{BridgeDefaults, BridgingAcceptor, CallRegistry};
use siphon_ai_media_glue::MediaSetup;
use siphon_ai_sip_glue::InviteFacts;

mod common;
use common::{invite, one_route, server_acks_start_then_idles, LINPHONE_PCMU_OFFER};

/// Recording sink that holds onto every emitted record.
#[derive(Default)]
struct Recorder {
    records: Mutex<Vec<CdrRecord>>,
}

#[async_trait]
impl CdrSink for Recorder {
    async fn emit(&self, record: CdrRecord) {
        self.records.lock().push(record);
    }
}

#[tokio::test]
async fn run_call_emits_cdr_when_controller_exits() {
    // Stand up a real WS server so the controller actually runs.
    let (port_tx, port_rx) = tokio::sync::oneshot::channel();
    tokio::spawn(server_acks_start_then_idles(port_tx));
    let port = port_rx.await.unwrap();
    let ws_url = format!("ws://127.0.0.1:{port}/");

    // Build the acceptor with a recording sink.
    let session_mgr = SessionManager::new(
        SessionManagerConfig {
            port_pool_config: PortPoolConfig::new(60100, 60200).unwrap(),
            ..Default::default()
        },
        None,
    );
    let bridge_mgr = Arc::new(MediaBridgeManager::new());
    let media = Arc::new(MediaSetup::new(
        Arc::clone(&session_mgr),
        bridge_mgr,
        Arc::new(forge_core::EventBus::new()),
        "192.168.1.10",
    ));
    let registry = CallRegistry::new();
    let recorder = Arc::new(Recorder::default());

    let acceptor = BridgingAcceptor::new(media, BridgeDefaults::default(), registry.clone())
        .with_call_id_factory(Arc::new(|| BridgeCallId::new("siphon-cdr-test")))
        .with_cdr_sink(Arc::clone(&recorder) as Arc<dyn CdrSink>);

    let routes = one_route("main_reception", &ws_url);
    let route = routes.iter().next().unwrap();
    let req = invite(
        LINPHONE_PCMU_OFFER,
        "sip:5000@siphon.example.com",
        "abc-cdr@pbx.example.com",
    );
    let facts = InviteFacts::extract(&req);

    let prepared = acceptor
        .prepare_call(&req, route, &facts)
        .await
        .expect("prepare");

    // Drive the call. Trigger the BYE-style shutdown a moment
    // later so the controller exits via LocalShutdown.
    let run_handle = acceptor.run_call(prepared, "main_reception", None);

    // Give the bridge time to handshake and send `start`.
    tokio::time::sleep(Duration::from_millis(150)).await;

    // Tell the controller to stop via the registry — same path BYE
    // uses.
    let h = registry
        .lookup("abc-cdr@pbx.example.com")
        .expect("registered");
    h.shutdown();

    // The spawned task ends after run_call returns; await it so we
    // know the CDR has been emitted before checking.
    tokio::time::timeout(Duration::from_secs(3), run_handle)
        .await
        .expect("run_call completes")
        .expect("task does not panic");

    let recs = recorder.records.lock();
    assert_eq!(recs.len(), 1, "expected exactly one CDR; got {recs:?}");
    let r = &recs[0];

    assert_eq!(r.version, siphon_ai_cdr::CDR_VERSION); // 2 since 0.9.5
    assert_eq!(r.call_id, "siphon-cdr-test");
    assert_eq!(r.sip_call_id, "abc-cdr@pbx.example.com");
    assert_eq!(r.from, "+13125551234");
    assert_eq!(r.to, "5000");
    assert_eq!(r.direction, CdrDirection::Inbound);
    assert_eq!(r.route, "main_reception");
    assert_eq!(r.ws_url, ws_url);
    assert_eq!(r.audio.codec, "PCMU");
    assert_eq!(r.audio.payload_type, 0);
    assert_eq!(r.audio.sample_rate, 8000);
    assert_eq!(r.termination.cause, TerminationCause::LocalShutdown);
    assert!(r.duration_ms < 5_000, "duration {} too long", r.duration_ms);
    assert!(r.ended_at >= r.started_at);

    // Registry was deregistered on the way out.
    assert!(registry.is_empty(), "registry should be empty after exit");
}

#[tokio::test]
async fn null_sink_is_the_default_when_no_sink_configured() {
    // Acceptor without with_cdr_sink should not panic when the
    // controller exits and an emit is attempted — the default
    // NullSink takes the record and drops it.
    let (port_tx, port_rx) = tokio::sync::oneshot::channel();
    tokio::spawn(server_acks_start_then_idles(port_tx));
    let port = port_rx.await.unwrap();
    let ws_url = format!("ws://127.0.0.1:{port}/");

    let session_mgr = SessionManager::new(
        SessionManagerConfig {
            port_pool_config: PortPoolConfig::new(60300, 60400).unwrap(),
            ..Default::default()
        },
        None,
    );
    let bridge_mgr = Arc::new(MediaBridgeManager::new());
    let media = Arc::new(MediaSetup::new(
        Arc::clone(&session_mgr),
        bridge_mgr,
        Arc::new(forge_core::EventBus::new()),
        "192.168.1.10",
    ));
    let registry = CallRegistry::new();
    let acceptor = BridgingAcceptor::new(media, BridgeDefaults::default(), registry.clone())
        .with_call_id_factory(Arc::new(|| BridgeCallId::new("siphon-null-cdr")));

    let routes = one_route("main_reception", &ws_url);
    let route = routes.iter().next().unwrap();
    let req = invite(
        LINPHONE_PCMU_OFFER,
        "sip:5000@siphon.example.com",
        "abc-cdr@pbx.example.com",
    );
    let facts = InviteFacts::extract(&req);
    let prepared = acceptor
        .prepare_call(&req, route, &facts)
        .await
        .expect("prepare");

    let run_handle = acceptor.run_call(prepared, "main_reception", None);
    tokio::time::sleep(Duration::from_millis(150)).await;
    let h = registry
        .lookup("abc-cdr@pbx.example.com")
        .expect("registered");
    h.shutdown();
    tokio::time::timeout(Duration::from_secs(3), run_handle)
        .await
        .expect("run_call completes")
        .expect("task does not panic");
}
