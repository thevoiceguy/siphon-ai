//! End-to-end lifecycle webhook emission: drive a call from
//! `prepare_call` → `run_call` against a real WS server, assert
//! the spawned task fires exactly one `call_start` and one
//! `call_end` event, in order, with the shape downstream
//! consumers see.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use forge_engine::{MediaBridgeManager, SessionManager, SessionManagerConfig};
use forge_rtp::PortPoolConfig;
use parking_lot::Mutex;
use siphon_ai_bridge::CallId as BridgeCallId;
use siphon_ai_core::{BridgeDefaults, BridgingAcceptor, CallRegistry};
use siphon_ai_media_glue::MediaSetup;
use siphon_ai_sip_glue::InviteFacts;
use siphon_ai_webhooks::{WebhookEvent, WebhookSink};

mod common;
use common::{invite, one_route, server_acks_start_then_idles, LINPHONE_PCMU_OFFER};

#[derive(Default)]
struct Recorder {
    events: Mutex<Vec<WebhookEvent>>,
}

#[async_trait]
impl WebhookSink for Recorder {
    async fn emit(&self, event: WebhookEvent) {
        self.events.lock().push(event);
    }
}

#[tokio::test]
async fn run_call_emits_call_start_then_call_end() {
    let (port_tx, port_rx) = tokio::sync::oneshot::channel();
    tokio::spawn(server_acks_start_then_idles(port_tx));
    let port = port_rx.await.unwrap();
    let ws_url = format!("ws://127.0.0.1:{port}/");

    let session_mgr = SessionManager::new(
        SessionManagerConfig {
            port_pool_config: PortPoolConfig::new(60700, 60800).unwrap(),
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
        .with_call_id_factory(Arc::new(|| BridgeCallId::new("siphon-webhook-test")))
        .with_webhook_sink(Arc::clone(&recorder) as Arc<dyn WebhookSink>);

    let routes = one_route("webhook_route", &ws_url);
    let route = routes.iter().next().unwrap();
    let req = invite(
        LINPHONE_PCMU_OFFER,
        "sip:5000@siphon.example.com",
        "abc-webhook@pbx",
    );
    let facts = InviteFacts::extract(&req);

    let prepared = acceptor
        .prepare_call(&req, route, &facts)
        .await
        .expect("prepare");
    let run_handle = acceptor.run_call(prepared, "webhook_route", None);

    tokio::time::sleep(Duration::from_millis(150)).await;
    let h = registry.lookup("abc-webhook@pbx").expect("registered");
    h.shutdown();

    tokio::time::timeout(Duration::from_secs(3), run_handle)
        .await
        .expect("run_call completes")
        .expect("task does not panic");

    // The call_start emit is on its own spawn — give it a beat
    // even after run_call returns.
    tokio::time::sleep(Duration::from_millis(50)).await;

    let events = recorder.events.lock();
    assert_eq!(events.len(), 2, "expected start+end, got {events:?}");

    // Order matters: receivers expect start before end.
    match &events[0] {
        WebhookEvent::CallStart(s) => {
            assert_eq!(s.version, 1);
            assert_eq!(s.call_id, "siphon-webhook-test");
            assert_eq!(s.sip_call_id, "abc-webhook@pbx");
            assert_eq!(s.from, "+13125551234");
            assert_eq!(s.to, "5000");
            assert_eq!(s.route, "webhook_route");
            assert_eq!(s.ws_url, ws_url);
        }
        other => panic!("expected CallStart first, got {other:?}"),
    }
    match &events[1] {
        WebhookEvent::CallEnd(e) => {
            assert_eq!(e.version, 1);
            assert_eq!(e.call_id, "siphon-webhook-test");
            assert_eq!(e.termination_cause, "local_shutdown");
            assert!(e.duration_ms < 5_000, "duration {} too long", e.duration_ms);
        }
        other => panic!("expected CallEnd second, got {other:?}"),
    }
}

#[tokio::test]
async fn null_webhook_sink_is_the_default() {
    // No with_webhook_sink call. Confirms the spawned task doesn't
    // panic when the default NullSink is in place.
    let (port_tx, port_rx) = tokio::sync::oneshot::channel();
    tokio::spawn(server_acks_start_then_idles(port_tx));
    let port = port_rx.await.unwrap();
    let ws_url = format!("ws://127.0.0.1:{port}/");

    let session_mgr = SessionManager::new(
        SessionManagerConfig {
            port_pool_config: PortPoolConfig::new(60900, 61000).unwrap(),
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
        .with_call_id_factory(Arc::new(|| BridgeCallId::new("siphon-null-webhook")));

    let routes = one_route("webhook_route", &ws_url);
    let route = routes.iter().next().unwrap();
    let req = invite(
        LINPHONE_PCMU_OFFER,
        "sip:5000@siphon.example.com",
        "abc-null@pbx",
    );
    let facts = InviteFacts::extract(&req);
    let prepared = acceptor
        .prepare_call(&req, route, &facts)
        .await
        .expect("prepare");

    let run_handle = acceptor.run_call(prepared, "webhook_route", None);
    tokio::time::sleep(Duration::from_millis(150)).await;
    let h = registry.lookup("abc-null@pbx").expect("registered");
    h.shutdown();
    tokio::time::timeout(Duration::from_secs(3), run_handle)
        .await
        .expect("run_call completes")
        .expect("task does not panic");
}
