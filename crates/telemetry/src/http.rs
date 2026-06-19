//! Hyper-based HTTP servers for the daemon's HTTP surfaces:
//!
//! - [`ObservabilityServer`] — `/health`, `/ready`, `/metrics` on
//!   `[observability].http_listen`. **Unauthenticated** by design: it's
//!   the scrape/probe surface (Prometheus, k8s) and carries no power.
//!   The probe routes are zero-allocation in the steady state.
//! - [`AdminServer`] — the `/admin/*` operator surface (see
//!   [`crate::admin`]) on its own `[admin].listen`, **gated by a bearer
//!   token + RBAC** ([`crate::auth`]). `/admin/*` is **no longer served**
//!   by the observability listener (0.10.0).
//!
//! Each server spawns a per-connection task. Routes that need
//! dependencies the runtime hasn't wired up return 503 (e.g.
//! `/admin/hep/test` with HEP disabled).
//!
//! ## Admin transport security
//!
//! The admin listener is plain HTTP in this cut, so the bearer token
//! travels in the clear on the wire — bind it on **loopback** (the
//! default posture) or front it with a TLS-terminating proxy. A native
//! `[admin].tls` is a follow-up; the runtime warns when the admin bind
//! is not loopback.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result};
use http_body_util::{BodyExt, Full};
use hyper::body::{Bytes, Incoming};
use hyper::header::{AUTHORIZATION, CONTENT_TYPE, WWW_AUTHENTICATE};
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use metrics_exporter_prometheus::PrometheusHandle;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tracing::{debug, error, info, warn};

use crate::admin::{self, AdminState};
use crate::auth::{route_label, AdminAuth, AuthReject};
use crate::metrics::ADMIN_REQUESTS_TOTAL;
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
    ) -> Result<Self> {
        let listener = TcpListener::bind(addr)
            .await
            .with_context(|| format!("bind observability HTTP {}", addr))?;
        let bound_addr = listener.local_addr().unwrap_or(addr);
        info!(addr = %bound_addr, "observability HTTP listener bound");

        let state = Arc::new(SharedState {
            prometheus,
            readiness,
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

    // /admin/* is NOT served here (0.10.0) — it lives on the
    // authenticated `AdminServer`. Anything else is a 404.
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

// ─── Admin server (authenticated /admin/* on its own listener) ──────

/// Handle to the running **authenticated** admin HTTP server. Bound on
/// `[admin].listen`; every request is gated by a bearer token + RBAC
/// ([`crate::auth`]) before reaching [`crate::admin::dispatch`].
pub struct AdminServer {
    bound_addr: SocketAddr,
    listener: JoinHandle<()>,
}

struct AdminSharedState {
    admin: AdminState,
    auth: AdminAuth,
}

impl AdminServer {
    /// Bind on `addr` and start serving the gated `/admin/*` surface.
    pub async fn start(addr: SocketAddr, auth: AdminAuth, admin: AdminState) -> Result<Self> {
        let listener = TcpListener::bind(addr)
            .await
            .with_context(|| format!("bind admin HTTP {}", addr))?;
        let bound_addr = listener.local_addr().unwrap_or(addr);
        info!(addr = %bound_addr, "admin HTTP listener bound (bearer-token auth)");

        let state = Arc::new(AdminSharedState { admin, auth });
        let listener = tokio::spawn(async move {
            loop {
                let (stream, peer) = match listener.accept().await {
                    Ok(pair) => pair,
                    Err(e) => {
                        error!(error = %e, "admin HTTP accept failed; exiting listener");
                        return;
                    }
                };
                let state = Arc::clone(&state);
                tokio::spawn(async move {
                    let io = TokioIo::new(stream);
                    let svc = service_fn(move |req| {
                        let state = Arc::clone(&state);
                        async move { Ok::<_, Infallible>(handle_admin_request(req, state, peer).await) }
                    });
                    if let Err(e) = hyper::server::conn::http1::Builder::new()
                        .serve_connection(io, svc)
                        .await
                    {
                        debug!(peer = %peer, error = %e, "admin HTTP connection closed with error");
                    }
                });
            }
        });

        Ok(Self {
            bound_addr,
            listener,
        })
    }

    pub fn bound_addr(&self) -> SocketAddr {
        self.bound_addr
    }

    pub async fn shutdown(self) {
        self.listener.abort();
        let _ = self.listener.await;
    }
}

async fn handle_admin_request(
    req: Request<Incoming>,
    state: Arc<AdminSharedState>,
    peer: SocketAddr,
) -> Response<Full<Bytes>> {
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let endpoint = route_label(&method, &path);
    let auth_header = req
        .headers()
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);

    // Authenticate + authorize BEFORE reading the body — an
    // unauthenticated caller never makes us buffer its payload.
    match state.auth.authorize(&method, &path, auth_header.as_deref()) {
        Err(AuthReject::Unauthenticated) => {
            warn!(peer = %peer, endpoint, "admin request rejected: 401 unauthenticated");
            metrics::counter!(ADMIN_REQUESTS_TOTAL, "endpoint" => endpoint, "role" => "none", "result" => "unauthenticated").increment(1);
            let mut resp = respond(
                StatusCode::UNAUTHORIZED,
                "application/json",
                br#"{"error":"unauthenticated"}"#,
            );
            resp.headers_mut()
                .insert(WWW_AUTHENTICATE, "Bearer".parse().expect("static header"));
            resp
        }
        Err(AuthReject::Forbidden { required, have }) => {
            warn!(peer = %peer, endpoint, required = required.as_str(), have = have.as_str(), "admin request rejected: 403 forbidden");
            metrics::counter!(ADMIN_REQUESTS_TOTAL, "endpoint" => endpoint, "role" => have.as_str(), "result" => "forbidden").increment(1);
            respond(
                StatusCode::FORBIDDEN,
                "application/json",
                br#"{"error":"forbidden: insufficient role"}"#,
            )
        }
        Ok(token) => {
            // Capture owned audit fields before the body read / dispatch.
            let actor = token.name.clone();
            let role = token.role.as_str();
            let body = match read_admin_body(req).await {
                Ok(b) => b,
                Err(resp) => return resp,
            };
            match admin::dispatch(&method, &path, body, &state.admin).await {
                Some(resp) => {
                    info!(peer = %peer, actor = %actor, role, endpoint, status = resp.status().as_u16(), "admin request");
                    metrics::counter!(ADMIN_REQUESTS_TOTAL, "endpoint" => endpoint, "role" => role, "result" => "ok").increment(1);
                    resp
                }
                None => {
                    // Authenticated, but an unknown /admin route.
                    info!(peer = %peer, actor = %actor, role, endpoint, status = 404, "admin request (unknown route)");
                    metrics::counter!(ADMIN_REQUESTS_TOTAL, "endpoint" => endpoint, "role" => role, "result" => "not_found").increment(1);
                    respond(StatusCode::NOT_FOUND, "text/plain", b"not found\n")
                }
            }
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
        let server = ObservabilityServer::start("127.0.0.1:0".parse().unwrap(), handle, readiness)
            .await
            .expect("start");
        let url = format!("http://{}", server.bound_addr());
        (server, url)
    }

    /// An `AdminServer` with three tokens (one per role) over an empty
    /// `AdminState` (every endpoint's deps are `None` → 503, which is
    /// fine: these tests exercise the auth gate, not dispatch).
    async fn spawn_admin_server() -> (AdminServer, String) {
        use crate::auth::{AdminAuth, AdminToken, Role};
        let auth = AdminAuth::new(vec![
            AdminToken::new("ro", "ro-tok", Role::ReadOnly),
            AdminToken::new("op", "op-tok", Role::Operator),
            AdminToken::new("ad", "ad-tok", Role::Admin),
        ]);
        let server =
            AdminServer::start("127.0.0.1:0".parse().unwrap(), auth, AdminState::default())
                .await
                .expect("admin start");
        let url = format!("http://{}", server.bound_addr());
        (server, url)
    }

    /// GET with an optional `Authorization` header.
    async fn get_auth(url: String, bearer: Option<&str>) -> StatusCode {
        let client: Client<_, Empty<Bytes>> = Client::builder(TokioExecutor::new()).build_http();
        let mut b = Request::builder().method(Method::GET).uri(url);
        if let Some(t) = bearer {
            b = b.header(AUTHORIZATION, format!("Bearer {t}"));
        }
        let resp = tokio::time::timeout(
            Duration::from_secs(2),
            client.request(b.body(Empty::new()).unwrap()),
        )
        .await
        .expect("request returns")
        .expect("ok");
        resp.status()
    }

    #[tokio::test]
    async fn admin_listener_does_not_serve_admin() {
        // /admin/* must NOT be on the observability listener anymore.
        let (server, url) = spawn_test_server(ReadinessFlag::new()).await;
        let (status, _) = get(format!("{url}/admin/calls")).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        server.shutdown().await;
    }

    #[tokio::test]
    async fn admin_requires_auth_and_enforces_roles() {
        let (server, url) = spawn_admin_server().await;
        // No token → 401.
        assert_eq!(
            get_auth(format!("{url}/admin/calls"), None).await,
            StatusCode::UNAUTHORIZED
        );
        // Bad token → 401.
        assert_eq!(
            get_auth(format!("{url}/admin/calls"), Some("wrong")).await,
            StatusCode::UNAUTHORIZED
        );
        // readonly token on a GET → authorized; dispatch's dep is None
        // so it 503s, but crucially it is NOT 401/403.
        let s = get_auth(format!("{url}/admin/calls"), Some("ro-tok")).await;
        assert!(
            s != StatusCode::UNAUTHORIZED && s != StatusCode::FORBIDDEN,
            "readonly GET should pass the gate, got {s}"
        );
        // readonly token on an unknown route still passes the gate (404).
        assert_eq!(
            get_auth(format!("{url}/admin/nope"), Some("ro-tok")).await,
            StatusCode::NOT_FOUND
        );
        server.shutdown().await;
    }

    #[tokio::test]
    async fn admin_forbids_below_minimum_role() {
        let (server, url) = spawn_admin_server().await;
        // GET /admin/v1/conferences requires readonly — a readonly token
        // passes the gate. (We can't easily POST originate here, but the
        // role table is unit-tested; this confirms the 403 wiring.)
        // Use a path that needs operator with a readonly token via the
        // HTTP layer: POST hangup. The body read happens after auth, so
        // GET is fine for the gate check — but hangup is POST. Send POST.
        let client: Client<_, Empty<Bytes>> = Client::builder(TokioExecutor::new()).build_http();
        let req = Request::builder()
            .method(Method::POST)
            .uri(format!("{url}/admin/calls/abc/hangup"))
            .header(AUTHORIZATION, "Bearer ro-tok")
            .body(Empty::new())
            .unwrap();
        let resp = tokio::time::timeout(Duration::from_secs(2), client.request(req))
            .await
            .expect("returns")
            .expect("ok");
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        server.shutdown().await;
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

        // The body cap is enforced AFTER auth now, so the request needs a
        // valid admin token (PUT /admin/log requires admin) to reach it.
        let (server, url) = spawn_admin_server().await;
        let addr_str = url.strip_prefix("http://").unwrap();
        let mut sock = TcpStream::connect(addr_str).await.expect("connect");

        // Build a chunked PUT to /admin/log. Eight 2 KiB chunks =
        // 16 KiB total, well past the 8 KiB cap. Each chunk's size
        // prefix is the byte count in hex; "0\r\n\r\n" terminates.
        let chunk_payload = "a".repeat(2048);
        let chunk_header = format!("{:x}\r\n", chunk_payload.len());
        let head = "PUT /admin/log HTTP/1.1\r\n\
                    Host: localhost\r\n\
                    Authorization: Bearer ad-tok\r\n\
                    Transfer-Encoding: chunked\r\n\
                    Content-Type: text/plain\r\n\
                    Connection: close\r\n\r\n";
        sock.write_all(head.as_bytes()).await.expect("write head");
        for _ in 0..8 {
            sock.write_all(chunk_header.as_bytes())
                .await
                .expect("chunk header");
            sock.write_all(chunk_payload.as_bytes())
                .await
                .expect("chunk body");
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
