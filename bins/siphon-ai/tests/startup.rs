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

/// Install rustls's process-wide crypto provider exactly once.
/// `main()` does this in the daemon path; tests don't run `main`,
/// and `Runtime::build` unconditionally constructs the client TLS
/// verification roots (for outbound `transport = "tls"`), so every
/// test that builds a runtime needs this first (or rustls panics
/// with "Could not automatically determine the process-level
/// CryptoProvider").
fn install_crypto_provider() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    });
}

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
    install_crypto_provider();
    let env = MapEnv::new([
        ("TEST_SIP_LISTEN", "127.0.0.1:0"),
        ("TEST_RTP_MIN", "40000"),
        ("TEST_RTP_MAX", "40100"),
    ]);
    let cfg = load_from_str_with_env(FIXTURE, &env).expect("config compiles");

    let runtime = Runtime::build(cfg, siphon_ai_telemetry::LogFilterHandle::noop())
        .await
        .expect("runtime builds");

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

const TCP_FIXTURE: &str = r#"
[node]
id = "siphon-ai-tcp-test"
public_address = "127.0.0.1"

[sip]
listen = "${TEST_SIP_LISTEN}"
transports = ["udp", "tcp"]

[media]
codecs = ["pcmu", "pcma"]
rtp_port_range = [${TEST_RTP_MIN}, ${TEST_RTP_MAX}]

[bridge]
ws_url = "wss://example.test/sip-bridge"

[[route]]
name = "default"
[route.match]
any = true
"#;

#[tokio::test]
async fn runtime_with_udp_and_tcp_transports_binds_both() {
    install_crypto_provider();
    // The TCP listener uses the same host:port pair as UDP — but
    // UDP and TCP are different namespaces in the kernel, so the
    // "busy" check only enforces uniqueness within a transport.
    // We let the runtime pick a UDP port via :0, then assert TCP
    // is also reachable on that port.
    let env = MapEnv::new([
        ("TEST_SIP_LISTEN", "127.0.0.1:0"),
        ("TEST_RTP_MIN", "40400"),
        ("TEST_RTP_MAX", "40500"),
    ]);
    let cfg = load_from_str_with_env(TCP_FIXTURE, &env).expect("config compiles");

    let runtime = Runtime::build(cfg, siphon_ai_telemetry::LogFilterHandle::noop())
        .await
        .expect("runtime builds");
    let bound = runtime.local_addr().expect("local_addr");
    assert!(bound.port() > 0);

    // The TCP listener spawns asynchronously inside Runtime::build
    // — it's bound by the time the spawned task gets polled, but
    // that may be a tick or two after build returns. Probe with a
    // short retry loop instead of pinning a single sleep.
    let connected = tokio::time::timeout(Duration::from_millis(500), async {
        loop {
            match tokio::net::TcpStream::connect(bound).await {
                Ok(_) => break true,
                Err(_) => tokio::time::sleep(Duration::from_millis(10)).await,
            }
        }
    })
    .await;
    assert!(
        matches!(connected, Ok(true)),
        "TCP listener at {bound} should accept connections"
    );

    // Drive shutdown.
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let run_handle = tokio::spawn(async move {
        let _ = runtime
            .run(async move {
                let _ = shutdown_rx.await;
            })
            .await;
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    shutdown_tx.send(()).expect("shutdown signal");
    tokio::time::timeout(Duration::from_secs(2), run_handle)
        .await
        .expect("runtime exits within 2s")
        .expect("task does not panic");
}

const REGISTER_FIXTURE: &str = r#"
[node]
public_address = "127.0.0.1"

[sip]
listen = "${TEST_SIP_LISTEN}"
transports = ["udp"]

[media]
codecs = ["pcmu"]
rtp_port_range = [${TEST_RTP_MIN}, ${TEST_RTP_MAX}]

[bridge]
ws_url = "wss://example.test/sip-bridge"

[[register]]
name = "pending-trunk"
server = "10.99.0.1"
username = "alice"
password = "secret"

[[register]]
name = "disabled-trunk"
server = "10.99.0.2"
username = "bob"
password = "secret"
register_on_startup = false

[[route]]
name = "default"
[route.match]
any = true
"#;

#[tokio::test]
async fn registrations_seed_into_manager_on_startup() {
    install_crypto_provider();
    let env = MapEnv::new([
        ("TEST_SIP_LISTEN", "127.0.0.1:0"),
        ("TEST_RTP_MIN", "40600"),
        ("TEST_RTP_MAX", "40700"),
    ]);
    let cfg = load_from_str_with_env(REGISTER_FIXTURE, &env).expect("config compiles");

    let runtime = Runtime::build(cfg, siphon_ai_telemetry::LogFilterHandle::noop())
        .await
        .expect("runtime builds");
    let snapshot = runtime.registration_snapshot();
    assert_eq!(snapshot.len(), 2);

    let pending = snapshot
        .iter()
        .find(|s| s.name == "pending-trunk")
        .expect("pending trunk snapshot");
    assert_eq!(
        pending.status,
        siphon_ai_sip_glue::RegistrationStatus::Pending,
    );
    assert_eq!(pending.server_addr.ip().to_string(), "10.99.0.1");

    let disabled = snapshot
        .iter()
        .find(|s| s.name == "disabled-trunk")
        .expect("disabled trunk snapshot");
    assert_eq!(
        disabled.status,
        siphon_ai_sip_glue::RegistrationStatus::Disabled,
    );

    // Drive shutdown.
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let run_handle = tokio::spawn(async move {
        let _ = runtime
            .run(async move {
                let _ = shutdown_rx.await;
            })
            .await;
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    shutdown_tx.send(()).expect("shutdown signal");
    tokio::time::timeout(Duration::from_secs(2), run_handle)
        .await
        .expect("runtime exits within 2s")
        .expect("task does not panic");
}

#[tokio::test]
async fn build_fails_when_listen_port_is_busy() {
    install_crypto_provider();
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

    let result = Runtime::build(cfg, siphon_ai_telemetry::LogFilterHandle::noop()).await;
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

const HEP_FIXTURE: &str = r#"
[node]
id = "siphon-ai-hep-test"
public_address = "127.0.0.1"

[sip]
listen = "${TEST_SIP_LISTEN}"
transports = ["udp"]

[media]
codecs = ["pcmu", "pcma"]
rtp_port_range = [${TEST_RTP_MIN}, ${TEST_RTP_MAX}]

[bridge]
ws_url = "wss://example.test/sip-bridge"

[hep]
enabled = true
collector = "${TEST_HEP_COLLECTOR}"
capture_id = 7777

[[route]]
name = "default"
[route.match]
any = true
"#;

#[tokio::test]
async fn runtime_with_hep_enabled_binds_and_drains_worker_on_shutdown() {
    install_crypto_provider();
    // Bind a UDP socket to stand in for Homer; we don't actually
    // need to receive anything for this smoke — the test asserts the
    // daemon brings up the HEP plumbing without failing the build,
    // and shuts down the worker cleanly.
    let homer = tokio::net::UdpSocket::bind("127.0.0.1:0")
        .await
        .expect("bind fake collector");
    let homer_addr = homer.local_addr().unwrap();

    let env = OwnedMapEnv::new(&[
        ("TEST_SIP_LISTEN", "127.0.0.1:0".to_string()),
        ("TEST_RTP_MIN", "40500".to_string()),
        ("TEST_RTP_MAX", "40600".to_string()),
        ("TEST_HEP_COLLECTOR", homer_addr.to_string()),
    ]);
    let cfg = load_from_str_with_env(HEP_FIXTURE, &env).expect("config compiles");

    let runtime = Runtime::build(cfg, siphon_ai_telemetry::LogFilterHandle::noop())
        .await
        .expect("runtime builds with HEP");
    let bound = runtime.local_addr().expect("local_addr");
    assert!(bound.port() > 0, "kernel must pick a port");

    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let run_handle = tokio::spawn(async move {
        let _ = runtime
            .run(async move {
                let _ = shutdown_rx.await;
            })
            .await;
    });

    tokio::time::sleep(Duration::from_millis(50)).await;
    shutdown_tx.send(()).expect("shutdown signal");
    tokio::time::timeout(Duration::from_secs(2), run_handle)
        .await
        .expect("runtime exits within 2s")
        .expect("task does not panic");

    // Keep `homer` alive until here so the daemon's UDP sink had a
    // real socket to `connect()` to.
    drop(homer);
}
