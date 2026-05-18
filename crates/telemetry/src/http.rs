//! Hyper-based HTTP server for `/health`, `/ready`, `/metrics`,
//! and the `/admin/*` operator surface.
//!
//! Single bind, all routes. Spawns a per-connection task; the
//! probe routes are zero-allocation in the steady state
//! (`/health` and `/ready` produce literal byte slices,
//! `/metrics` calls `PrometheusHandle::render` which owns the
//! string allocation).
//!
//! ## Admin endpoints
//!
//! See [`crate::admin`]. They go through the same listener so
//! operators have one HTTP surface to point a probe / dashboard
//! at, not two. Routes that need dependencies the runtime hasn't
//! wired up return 503 (e.g., `/admin/hep/test` with HEP disabled).
//!
//! ## What's NOT here
//!
//! - **No auth** — these endpoints are intended to bind on a
//!   loopback or trusted-network address (k8s pods, localhost). If
//!   exposed publicly, sit them behind an authenticating reverse
//!   proxy. Per CLAUDE.md §12 ("Security v1 minimum") that's the
//!   v1 threat model.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result};
use http_body_util::{BodyExt, Full};
use hyper::body::{Bytes, Incoming};
use hyper::header::CONTENT_TYPE;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use metrics_exporter_prometheus::PrometheusHandle;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tracing::{debug, error, info};

use crate::admin::{self, AdminState};
use crate::readiness::ReadinessFlag;

/// Body byte cap on PUT/POST admin requests. 8 KiB is far more than
/// any directive string or hangup body needs; protects against a
/// noisy probe consuming the daemon's heap.
const ADMIN_MAX_BODY: usize = 8 * 1024;

/// Handle to a running observability HTTP server. Drop or call
/// [`Self::shutdown`] to stop accepting new connections.
pub struct ObservabilityServer {
    bound_addr: SocketAddr,
    /// Listener task. Aborted on shutdown — there's no graceful
    /// per-connection drain, but each per-connection task already
    /// has its own short request/response lifecycle.
    listener: JoinHandle<()>,
}

impl ObservabilityServer {
    /// Bind on `addr` and start serving. The listener task is
    /// spawned before this returns so a port-busy error surfaces
    /// here, not on first request.
    pub async fn start(
        addr: SocketAddr,
        prometheus: PrometheusHandle,
        readiness: ReadinessFlag,
        admin: AdminState,
    ) -> Result<Self> {
        let listener = TcpListener::bind(addr)
            .await
            .with_context(|| format!("bind observability HTTP {}", addr))?;
        let bound_addr = listener.local_addr().unwrap_or(addr);
        info!(addr = %bound_addr, "observability HTTP listener bound");

        let state = Arc::new(SharedState {
            prometheus,
            readiness,
            admin,
        });

        let listener = tokio::spawn(async move {
            run_accept_loop(listener, state).await;
        });

        Ok(Self {
            bound_addr,
            listener,
        })
    }

    pub fn bound_addr(&self) -> SocketAddr {
        self.bound_addr
    }

    /// Stop accepting new connections. In-flight requests on
    /// already-accepted connections finish; this just drops the
    /// listener.
    pub async fn shutdown(self) {
        self.listener.abort();
        let _ = self.listener.await;
    }
}

struct SharedState {
    prometheus: PrometheusHandle,
    readiness: ReadinessFlag,
    admin: AdminState,
}

async fn run_accept_loop(listener: TcpListener, state: Arc<SharedState>) {
    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(pair) => pair,
            Err(e) => {
                // accept() failures are usually fatal (fd exhaustion,
                // socket gone). Log and exit; the JoinHandle drops.
                error!(error = %e, "observability HTTP accept failed; exiting listener");
                return;
            }
        };
        debug!(peer = %peer, "observability HTTP connection accepted");

        let state = Arc::clone(&state);
        tokio::spawn(async move {
            let io = TokioIo::new(stream);
            let svc = service_fn(move |req| {
                let state = Arc::clone(&state);
                async move { Ok::<_, Infallible>(handle_request(req, state).await) }
            });
            if let Err(e) = hyper::server::conn::http1::Builder::new()
                .serve_connection(io, svc)
                .await
            {
                debug!(peer = %peer, error = %e, "observability HTTP connection closed with error");
            }
        });
    }
}

async fn handle_request(req: Request<Incoming>, state: Arc<SharedState>) -> Response<Full<Bytes>> {
    let method = req.method().clone();
    let path = req.uri().path().to_string();

    // Probe routes first — they're hot, parameter-less, and the
    // existing /metrics scraper polls /metrics every few seconds.
    match (&method, path.as_str()) {
        (&hyper::Method::GET, "/health") => {
            return respond(StatusCode::OK, "text/plain", b"ok\n");
        }
        (&hyper::Method::GET, "/ready") => {
            if state.readiness.is_ready() {
                return respond(StatusCode::OK, "text/plain", b"ready\n");
            }
            return respond(
                StatusCode::SERVICE_UNAVAILABLE,
                "text/plain",
                b"not ready\n",
            );
        }
        (&hyper::Method::GET, "/metrics") => {
            let body = state.prometheus.render();
            return respond(
                StatusCode::OK,
                "text/plain; version=0.0.4; charset=utf-8",
                body.as_bytes(),
            );
        }
        _ => {}
    }

    // Admin routes — read the body (bounded) once, then dispatch.
    if path.starts_with("/admin") {
        let body = match read_admin_body(req).await {
            Ok(b) => b,
            Err(resp) => return resp,
        };
        if let Some(resp) = admin::dispatch(&method, &path, body, &state.admin).await {
            return resp;
        }
    }

    respond(StatusCode::NOT_FOUND, "text/plain", b"not found\n")
}

/// Bounded body reader for `/admin/*` PUT / POST. Returns the bytes
/// on success or a ready-to-send error response on failure.
///
/// Enforces the cap via [`http_body_util::Limited`], which fails
/// streaming reads the moment more than `ADMIN_MAX_BODY` bytes have
/// been received. The old version trusted `size_hint().upper()` for
/// a cheap pre-check and then unconditionally `collect()`-ed — but
/// chunked-transfer-encoded PUTs leave `size_hint().upper()` as
/// `None`, so the pre-check is skipped and the subsequent `collect()`
/// buffers arbitrary bytes before failing (or OOMing).
async fn read_admin_body(req: Request<Incoming>) -> Result<Bytes, Response<Full<Bytes>>> {
    use http_body_util::Limited;

    let limited = Limited::new(req.into_body(), ADMIN_MAX_BODY);
    match limited.collect().await {
        Ok(collected) => Ok(collected.to_bytes()),
        Err(e) => {
            debug!(error = %e, "admin body exceeded cap or read failed");
            // `Limited` errors when the cap is exceeded OR when the
            // underlying body fails. We map both to 413 since the
            // adversarial case (chunked flood) is the one that
            // matters; legitimate ill-formed bodies are a rounding
            // error and the operator can re-issue with curl -v.
            Err(respond(
                StatusCode::PAYLOAD_TOO_LARGE,
                "application/json",
                br#"{"error":"admin request body exceeds 8 KiB"}"#,
            ))
        }
    }
}

fn respond(status: StatusCode, content_type: &'static str, body: &[u8]) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .header(CONTENT_TYPE, content_type)
        .body(Full::new(Bytes::copy_from_slice(body)))
        .expect("response builder accepts the headers we set")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::{prometheus_builder, register_descriptions, INVITES_TOTAL};
    use http_body_util::BodyExt;
    use http_body_util::Empty;
    use hyper::Method;
    use hyper_util::client::legacy::Client;
    use hyper_util::rt::TokioExecutor;
    use std::time::Duration;

    /// Build a server bound on an ephemeral loopback port for a
    /// test. Returns the server and a base URL.
    async fn spawn_test_server(readiness: ReadinessFlag) -> (ObservabilityServer, String) {
        let recorder = prometheus_builder().expect("builder").build_recorder();
        let handle = recorder.handle();
        // Tag a metric so /metrics has something interesting; we
        // can't install globally inside the test (other tests do
        // the same), so just reuse the handle without installing.
        let server = ObservabilityServer::start(
            "127.0.0.1:0".parse().unwrap(),
            handle,
            readiness,
            AdminState::default(),
        )
        .await
        .expect("start");
        let url = format!("http://{}", server.bound_addr());
        (server, url)
    }

    async fn get(url: String) -> (StatusCode, String) {
        let client: Client<_, Empty<Bytes>> = Client::builder(TokioExecutor::new()).build_http();
        let req = Request::builder()
            .method(Method::GET)
            .uri(url)
            .body(Empty::new())
            .unwrap();
        let resp = tokio::time::timeout(Duration::from_secs(2), client.request(req))
            .await
            .expect("request returns")
            .expect("ok");
        let status = resp.status();
        let body = resp
            .into_body()
            .collect()
            .await
            .expect("collect body")
            .to_bytes();
        (status, String::from_utf8_lossy(&body).into_owned())
    }

    #[tokio::test]
    async fn health_is_always_200() {
        let readiness = ReadinessFlag::new();
        let (server, url) = spawn_test_server(readiness).await;
        let (status, body) = get(format!("{url}/health")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, "ok\n");
        server.shutdown().await;
    }

    #[tokio::test]
    async fn ready_returns_503_until_marked_ready() {
        let readiness = ReadinessFlag::new();
        let (server, url) = spawn_test_server(readiness.clone()).await;

        let (status_pre, body_pre) = get(format!("{url}/ready")).await;
        assert_eq!(status_pre, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body_pre, "not ready\n");

        readiness.mark_ready();
        let (status_post, body_post) = get(format!("{url}/ready")).await;
        assert_eq!(status_post, StatusCode::OK);
        assert_eq!(body_post, "ready\n");

        server.shutdown().await;
    }

    #[tokio::test]
    async fn unknown_path_yields_404() {
        let (server, url) = spawn_test_server(ReadinessFlag::new()).await;
        let (status, _) = get(format!("{url}/totally-not-a-route")).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        server.shutdown().await;
    }

    /// Regression: a chunked-transfer-encoded PUT longer than the
    /// 8 KiB admin cap must be rejected with 413 instead of being
    /// buffered to completion.
    ///
    /// Previously, `read_admin_body` only checked `size_hint().upper()`
    /// before calling `collect()`. For chunked encoding `size_hint()`
    /// returns `(0, None)`, so the cheap pre-check was skipped and
    /// `collect()` ran unbounded — a hostile client could force
    /// arbitrary buffering. This test issues exactly that wire-level
    /// shape (raw TCP, `Transfer-Encoding: chunked`, ~16 KiB total
    /// across several chunks) and asserts the daemon refuses it.
    #[tokio::test]
    async fn admin_chunked_body_over_cap_is_rejected() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpStream;

        let (server, url) = spawn_test_server(ReadinessFlag::new()).await;
        let addr_str = url.strip_prefix("http://").unwrap();
        let mut sock = TcpStream::connect(addr_str).await.expect("connect");

        // Build a chunked PUT to /admin/log. Eight 2 KiB chunks =
        // 16 KiB total, well past the 8 KiB cap. Each chunk's size
        // prefix is the byte count in hex; "0\r\n\r\n" terminates.
        let chunk_payload = "a".repeat(2048);
        let chunk_header = format!("{:x}\r\n", chunk_payload.len());
        let head = "PUT /admin/log HTTP/1.1\r\n\
                    Host: localhost\r\n\
                    Transfer-Encoding: chunked\r\n\
                    Content-Type: text/plain\r\n\
                    Connection: close\r\n\r\n";
        sock.write_all(head.as_bytes()).await.expect("write head");
        for _ in 0..8 {
            sock.write_all(chunk_header.as_bytes()).await.expect("chunk header");
            sock.write_all(chunk_payload.as_bytes()).await.expect("chunk body");
            sock.write_all(b"\r\n").await.expect("chunk crlf");
        }
        sock.write_all(b"0\r\n\r\n").await.expect("terminator");

        // Read the response. We only care about the status line.
        let mut buf = Vec::with_capacity(2048);
        let _ = tokio::time::timeout(Duration::from_secs(3), sock.read_to_end(&mut buf)).await;
        let response = String::from_utf8_lossy(&buf);
        assert!(
            response.starts_with("HTTP/1.1 413"),
            "expected 413 Payload Too Large, got:\n{response}",
        );

        server.shutdown().await;
    }

    #[tokio::test]
    async fn metrics_renders_prometheus_text_with_help_lines() {
        // Use a per-test recorder via with_local_recorder so the
        // metric we record is visible to the rendered handle without
        // touching the global recorder.
        let recorder = prometheus_builder().expect("builder").build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            register_descriptions();
            metrics::counter!(INVITES_TOTAL, "result" => "accepted").increment(7);
        });

        let server = ObservabilityServer::start(
            "127.0.0.1:0".parse().unwrap(),
            handle,
            ReadinessFlag::new(),
            AdminState::default(),
        )
        .await
        .expect("start");
        let url = format!("http://{}", server.bound_addr());

        let (status, body) = get(format!("{url}/metrics")).await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            body.contains(&format!("# HELP {INVITES_TOTAL} ")),
            "missing HELP in:\n{body}"
        );
        assert!(
            body.contains(&format!("{INVITES_TOTAL}{{result=\"accepted\"}} 7")),
            "missing counter line in:\n{body}"
        );

        server.shutdown().await;
    }
}
