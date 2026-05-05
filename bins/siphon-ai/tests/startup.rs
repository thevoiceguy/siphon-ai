//! Smoke test for the daemon startup path.
//!
//! Builds the runtime against a tiny fixture config (UDP bind on
//! `127.0.0.1:0`, real forge SessionManager, real `IntegratedUAS`),
//! confirms the bound port is non-zero, then drives a clean
//! shutdown via the same notify channel `main` will use for
//! SIGTERM.
//!
//! The actual SIP / RTP / WS plumbing is exercised by the
//! lower-layer integration tests (acceptor_prepare.rs,
//! registry_bye.rs, controller_lifecycle.rs); this test just
//! verifies that the runtime composition compiles, binds, and
//! tears down without panicking.

use std::time::Duration;

use siphon_ai::Runtime;
use siphon_ai_config::{load_from_str_with_env, EnvSource};
use std::collections::HashMap;
use tokio::sync::oneshot;

const FIXTURE: &str = include_str!("fixtures/local-dev.toml");

/// Test env source. Random port allocation lives at the OS layer
/// (the daemon binds via `UdpSocket::bind`) so we just pin a
/// `127.0.0.1:0` listen and let the kernel pick a free port.
struct MapEnv(HashMap<String, String>);
impl MapEnv {
    fn new<I: IntoIterator<Item = (&'static str, &'static str)>>(items: I) -> Self {
        Self(
            items
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        )
    }
}
impl EnvSource for MapEnv {
    fn lookup(&self, name: &str) -> Option<String> {
        self.0.get(name).cloned()
    }
}

#[tokio::test]
async fn runtime_starts_and_shuts_down_cleanly() {
    let env = MapEnv::new([
        ("TEST_SIP_LISTEN", "127.0.0.1:0"),
        ("TEST_RTP_MIN", "40000"),
        ("TEST_RTP_MAX", "40100"),
    ]);
    let cfg = load_from_str_with_env(FIXTURE, &env).expect("config compiles");

    let runtime = Runtime::build(cfg).await.expect("runtime builds");

    // The kernel should have picked a non-zero port.
    let bound = runtime.local_addr().expect("local_addr");
    assert!(
        bound.port() > 0,
        "kernel must pick a real port, got {bound}"
    );
    assert_eq!(bound.ip().to_string(), "127.0.0.1");

    // Drive shutdown the same way `main` does — pass a future that
    // resolves when we send on the oneshot.
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let run_handle = tokio::spawn(async move {
        let _ = runtime
            .run(async move {
                let _ = shutdown_rx.await;
            })
            .await;
    });

    // Give listeners a moment to spawn so we can observe a clean
    // teardown rather than racing the abort.
    tokio::time::sleep(Duration::from_millis(50)).await;

    shutdown_tx.send(()).expect("shutdown signal");
    tokio::time::timeout(Duration::from_secs(2), run_handle)
        .await
        .expect("runtime exits within 2s")
        .expect("task does not panic");
}

#[tokio::test]
async fn build_fails_when_listen_port_is_busy() {
    // Bind a placeholder UDP socket on an ephemeral port, then ask
    // the runtime to bind the same one — the second bind must
    // surface as a startup error, not a silent succeed-and-overlap.
    let placeholder = tokio::net::UdpSocket::bind("127.0.0.1:0")
        .await
        .expect("placeholder bind");
    let busy = placeholder.local_addr().unwrap();

    let env = OwnedMapEnv::new(&[
        ("TEST_SIP_LISTEN", busy.to_string()),
        ("TEST_RTP_MIN", "40200".to_string()),
        ("TEST_RTP_MAX", "40300".to_string()),
    ]);
    let cfg = load_from_str_with_env(FIXTURE, &env).expect("config compiles");

    let result = Runtime::build(cfg).await;
    assert!(result.is_err(), "expected bind conflict, got Ok");

    drop(placeholder);
}

/// Variant of `MapEnv` that takes owned values — needed because the
/// busy-port test uses a runtime-derived bind string.
struct OwnedMapEnv(HashMap<String, String>);
impl OwnedMapEnv {
    fn new(items: &[(&str, String)]) -> Self {
        Self(
            items
                .iter()
                .map(|(k, v)| (k.to_string(), v.clone()))
                .collect(),
        )
    }
}
impl EnvSource for OwnedMapEnv {
    fn lookup(&self, name: &str) -> Option<String> {
        self.0.get(name).cloned()
    }
}
