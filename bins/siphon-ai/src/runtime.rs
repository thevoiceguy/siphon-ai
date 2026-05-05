//! Daemon runtime: wires every layer together.
//!
//! Topology:
//!
//! ```text
//!   sip-transport::run_udp ─► InboundPacket ─► parse_request
//!                                                    │
//!                                                    ▼
//!                            TransactionManager::receive_request
//!                                                    │
//!                                                    ▼
//!                                      IntegratedUAS::dispatch
//!                                                    │
//!                                                    ▼
//!                                           RoutingHandler
//!                                            (sip-glue)
//!                                          ╱            ╲
//!                              on_invite ─►              ─► on_bye/on_cancel
//!                            BridgingAcceptor              CallRegistry
//!                            (siphon-ai-core)            (siphon-ai-core)
//!                                  │
//!                                  ▼
//!                             MediaSetup
//!                       (siphon-ai-media-glue)
//!                                  │
//!                                  ▼
//!                           CallController
//!                       (siphon-ai-core ─ tap + WS bridge)
//! ```
//!
//! ## What's in scope (v1 minimal cut)
//!
//! - UDP transport on `[sip].listen` (other transports deferred).
//! - Inbound INVITE → routed → MediaSetup → 200 OK → CallController.
//! - BYE / CANCEL via the CallRegistry.
//!
//! ## What's deferred
//!
//! - TCP / TLS / WS transports — wiring is straightforward (siphon-rs
//!   `run_tcp` / `run_tls` / `run_ws` mirror `run_udp`); land them once
//!   we have a need.
//! - Outbound REGISTER (UAC mode); requires `[[register]]` config.
//! - Admin / metrics / health HTTP servers.
//! - HEP, CDR, lifecycle webhooks.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use bytes::Bytes;
use forge_engine::{MediaBridgeManager, SessionManager, SessionManagerConfig};
use forge_rtp::PortPoolConfig;
use sip_core::SipUri;
use sip_parse::parse_request;
use sip_transaction::{
    TransactionManager, TransportContext, TransportDispatcher, TransportKind as TxTransportKind,
};
use sip_transport::{run_udp, send_udp, InboundPacket, TransportKind as TpTransportKind};
use sip_uas::integrated::{IntegratedUAS, UasRequestHandler};
use sip_uas::UserAgentServer;
use siphon_ai_cdr::{CdrSinkHandle, FileSink, MultiSink, NullSink, WebhookSink, WebhookSinkConfig};
use siphon_ai_config::{
    CdrConfig, CdrFileConfig, CdrWebhookConfig, Config, MediaConfig, NodeConfig,
    ObservabilityConfig, SipConfig, SipTransport,
};
use siphon_ai_core::{BridgingAcceptor, CallRegistry};
use siphon_ai_media_glue::MediaSetup;
use siphon_ai_sip_glue::{DialogTerminatorHandle, RoutingHandler};
use siphon_ai_telemetry::{install_recorder, ObservabilityServer, ReadinessFlag};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{debug, error, info, instrument, warn};

/// A built-but-not-yet-running daemon. Call [`Runtime::run`] to
/// drive it; the future returned only completes when shutdown is
/// signalled (via the bound `tokio::signal` handler in `main`) or a
/// listener task fails fatally.
pub struct Runtime {
    sip_listen: SocketAddr,
    udp_socket: Arc<UdpSocket>,
    transaction_mgr: Arc<TransactionManager>,
    uas: Arc<IntegratedUAS>,
    /// Task handles we own and abort on shutdown. The `run_udp`
    /// reader, in particular, has no graceful-stop hook upstream —
    /// we rely on aborting it.
    listeners: Vec<JoinHandle<()>>,
    /// `Some` when `[observability].enabled = true`. Dropped on
    /// shutdown to stop the HTTP listener.
    observability: Option<ObservabilityServer>,
}

impl Runtime {
    /// Build the runtime from a parsed config.
    ///
    /// Binds the UDP socket eagerly so a "port already in use" error
    /// surfaces during startup, not after we've logged "ready".
    pub async fn build(config: Config) -> Result<Self> {
        let Config {
            node,
            sip,
            media,
            bridge_defaults,
            routes,
            cdr,
            observability,
        } = config;

        warn_on_unsupported_transports(&sip);

        // ─── Telemetry: install Prometheus recorder + spawn HTTP ───
        let readiness = ReadinessFlag::new();
        let observability_server = build_observability(observability, readiness.clone()).await?;

        let cdr_sink = build_cdr_sink(cdr).await?;

        // ─── Forge media stack ──────────────────────────────────────
        let session_mgr_config = SessionManagerConfig {
            port_pool_config: rtp_port_pool(&media)?,
            ..Default::default()
        };
        let session_mgr = SessionManager::new(session_mgr_config, None);
        // Background task that reaps idle sessions per
        // SessionManagerConfig::cleanup_interval. Must run for forge
        // to enforce its session_timeout.
        session_mgr.start_monitoring().await;

        let bridge_mgr = Arc::new(MediaBridgeManager::new());
        let media_setup = Arc::new(MediaSetup::new(
            Arc::clone(&session_mgr),
            Arc::clone(&bridge_mgr),
            node.public_address.clone(),
        ));

        // ─── Bridging acceptor + dialog registry ───────────────────
        let registry = CallRegistry::new();
        let uas_helper = build_uas_helper(&node, &sip)?;
        let acceptor = Arc::new(
            BridgingAcceptor::new(
                media_setup,
                bridge_defaults,
                Arc::clone(&uas_helper),
                registry.clone(),
            )
            .with_cdr_sink(cdr_sink),
        );

        // ─── SIP routing handler ───────────────────────────────────
        let dialog_terminator: DialogTerminatorHandle = Arc::new(registry);
        let routing_handler = Arc::new(
            RoutingHandler::new(Arc::new(routes), acceptor)
                .with_dialog_terminator(dialog_terminator),
        );

        // ─── UDP transport + transaction manager ───────────────────
        let udp_socket = Arc::new(
            UdpSocket::bind(sip.listen_addr)
                .await
                .with_context(|| format!("bind UDP {}", sip.listen_addr))?,
        );
        let dispatcher: Arc<dyn TransportDispatcher> =
            Arc::new(UdpDispatcher::new(Arc::clone(&udp_socket)));
        let transaction_mgr = Arc::new(TransactionManager::new(Arc::clone(&dispatcher)));

        // ─── Integrated UAS ────────────────────────────────────────
        let local_uri = sip_local_uri(&node, &sip);
        let contact_uri = sip.contact.as_deref().unwrap_or(&local_uri).to_string();
        let uas: Arc<dyn UasRequestHandler> = routing_handler;
        let mut uas_builder = IntegratedUAS::builder()
            .local_uri(&local_uri)
            .contact_uri(&contact_uri)
            .transaction_manager(Arc::clone(&transaction_mgr))
            .dispatcher(Arc::clone(&dispatcher))
            .request_handler(uas);
        uas_builder = uas_builder
            .local_addr(sip.listen_addr.to_string())
            .map_err(|e| anyhow!("local_addr: {e}"))?;
        if let Some(public) = sip_public_addr(&node, &sip) {
            uas_builder = uas_builder
                .public_addr(public.to_string())
                .map_err(|e| anyhow!("public_addr: {e}"))?;
        }
        let uas = Arc::new(uas_builder.build()?);

        // ─── Spawn the inbound UDP reader + dispatch loop ──────────
        let listeners = spawn_listeners(
            Arc::clone(&udp_socket),
            Arc::clone(&transaction_mgr),
            Arc::clone(&uas),
        );

        // We're now serving SIP — let the readiness probe flip.
        readiness.mark_ready();

        Ok(Self {
            sip_listen: sip.listen_addr,
            udp_socket,
            transaction_mgr,
            uas,
            listeners,
            observability: observability_server,
        })
    }

    /// Bound UDP listen address. The post-bind value, so callers
    /// can read back the actual port when the config supplied `:0`.
    pub fn sip_listen(&self) -> SocketAddr {
        self.sip_listen
    }

    /// Same address as [`Self::sip_listen`] but resolved via the
    /// underlying `UdpSocket`. Used by the startup test that binds
    /// `127.0.0.1:0` and needs the OS-chosen port to drive a probe
    /// against it.
    pub fn local_addr(&self) -> Result<SocketAddr> {
        self.udp_socket.local_addr().map_err(Into::into)
    }

    /// Drive the runtime until `shutdown` resolves. On wake the
    /// inbound listeners are aborted and forge's session monitor is
    /// stopped before returning.
    pub async fn run<S>(self, shutdown: S) -> Result<()>
    where
        S: std::future::Future<Output = ()>,
    {
        info!(listen = %self.sip_listen, "siphon-ai daemon ready");
        shutdown.await;
        info!("shutdown signal received; tearing down");

        for handle in self.listeners {
            handle.abort();
            // Best-effort wait for the abort to land; ignore
            // JoinError (the abort yields Cancelled).
            let _ = handle.await;
        }

        // Stop accepting `/metrics` / `/health` requests.
        if let Some(server) = self.observability {
            server.shutdown().await;
        }

        // Drop the UAS / TM Arcs so any per-call task that's still
        // holding a clone tears down cleanly. We don't wait for
        // active calls — they'll see their channels close and exit
        // on their own. v1 doesn't have a "drain calls cleanly"
        // story; that's a follow-up alongside SIGTERM-with-grace.
        let _ = self.transaction_mgr;
        let _ = self.uas;
        let _ = self.udp_socket;
        Ok(())
    }
}

// ─── Helpers ─────────────────────────────────────────────────────────

/// Install the Prometheus recorder + spawn the `/health` /
/// `/ready` / `/metrics` HTTP server. When the operator hasn't
/// enabled `[observability]` we still install the recorder (so
/// metric calls in the call layers don't crash) but skip the HTTP
/// listener — the recorder is just unconsumed in that case.
async fn build_observability(
    cfg: ObservabilityConfig,
    readiness: ReadinessFlag,
) -> Result<Option<ObservabilityServer>> {
    let handle = install_recorder().context("install Prometheus recorder")?;
    if !cfg.enabled {
        debug!("[observability].enabled = false; skipping HTTP listener");
        return Ok(None);
    }
    let listen = cfg
        .http_listen
        .ok_or_else(|| anyhow!("[observability].http_listen unexpectedly empty"))?;
    let server = ObservabilityServer::start(listen, handle, readiness)
        .await
        .with_context(|| format!("bind observability HTTP {listen}"))?;
    Ok(Some(server))
}

async fn build_cdr_sink(cdr: CdrConfig) -> Result<CdrSinkHandle> {
    if !cdr.enabled {
        return Ok(Arc::new(NullSink));
    }
    let mut sinks: Vec<CdrSinkHandle> = Vec::new();
    if let Some(file_cfg) = cdr.file {
        sinks.push(build_file_sink(&file_cfg).await?);
    }
    if let Some(webhook_cfg) = cdr.webhook {
        sinks.push(build_webhook_sink(&webhook_cfg)?);
    }
    Ok(match sinks.len() {
        // [cdr].enabled = true with no sub-sinks turned on is a
        // configuration tic; emit a warning rather than failing so
        // operators flipping switches mid-investigation aren't
        // blocked.
        0 => {
            warn!("[cdr].enabled = true but no sub-sinks (file / webhook) are enabled; CDRs will be dropped");
            Arc::new(NullSink) as CdrSinkHandle
        }
        1 => sinks.pop().unwrap(),
        _ => Arc::new(MultiSink::new(sinks)) as CdrSinkHandle,
    })
}

async fn build_file_sink(cfg: &CdrFileConfig) -> Result<CdrSinkHandle> {
    let sink = FileSink::open(&cfg.path)
        .await
        .with_context(|| format!("open CDR file {}", cfg.path.display()))?;
    info!(path = %cfg.path.display(), "CDR file sink active");
    Ok(Arc::new(sink) as CdrSinkHandle)
}

fn build_webhook_sink(cfg: &CdrWebhookConfig) -> Result<CdrSinkHandle> {
    let sink = WebhookSink::new(WebhookSinkConfig {
        url: cfg.url.clone(),
        auth_header: cfg.auth_header.clone(),
        retry_max: cfg.retry_max,
        timeout_ms: cfg.timeout.as_millis() as u64,
    })
    .map_err(|e| anyhow!("CDR webhook client build failed: {e}"))?;
    info!(url = %cfg.url, "CDR webhook sink active");
    Ok(Arc::new(sink) as CdrSinkHandle)
}

fn warn_on_unsupported_transports(sip: &SipConfig) {
    for t in &sip.transports {
        if !matches!(t, SipTransport::Udp) {
            warn!(
                transport = ?t,
                "non-UDP SIP transport configured but not yet wired in v1; ignoring"
            );
        }
    }
}

fn rtp_port_pool(media: &MediaConfig) -> Result<PortPoolConfig> {
    match media.rtp_port_range {
        Some((min, max)) => PortPoolConfig::new(min, max)
            .map_err(|e| anyhow!("[media].rtp_port_range invalid: {e}")),
        None => Ok(PortPoolConfig::default()),
    }
}

fn build_uas_helper(node: &NodeConfig, sip: &SipConfig) -> Result<Arc<UserAgentServer>> {
    let local_uri = SipUri::parse(&sip_local_uri(node, sip))
        .map_err(|e| anyhow!("synthesised SIP local URI is invalid: {e}"))?;
    let contact_str = sip
        .contact
        .clone()
        .unwrap_or_else(|| sip_local_uri(node, sip));
    let contact_uri = SipUri::parse(&contact_str)
        .map_err(|e| anyhow!("[sip].contact {contact_str:?} is not a valid SIP URI: {e}"))?;
    Ok(Arc::new(UserAgentServer::new(local_uri, contact_uri)))
}

/// `sip:siphon@<host>` derived from `[sip].listen` (or
/// `[node].public_address` if it's not loopback). Used as the daemon's
/// SIP identity in From / To / Contact headers it generates.
fn sip_local_uri(node: &NodeConfig, sip: &SipConfig) -> String {
    let host = if node.public_address.is_empty() {
        sip.listen_addr.ip().to_string()
    } else {
        node.public_address.clone()
    };
    let user = sip
        .user_agent
        .as_deref()
        .map(extract_user_part)
        .unwrap_or("siphon");
    format!("sip:{user}@{host}")
}

fn extract_user_part(_user_agent: &str) -> &str {
    // The User-Agent header is product info, not a user. We always
    // use a fixed user-part so the daemon's SIP identity is stable;
    // this helper exists so future deployments can plug in a
    // configured value without changing call sites.
    "siphon"
}

fn sip_public_addr(node: &NodeConfig, sip: &SipConfig) -> Option<SocketAddr> {
    // public_address may be just an IP; combine with the listen
    // port to form a SocketAddr the IntegratedUAS Contact-filling
    // logic understands.
    if node.public_address.is_empty() {
        return None;
    }
    let candidate = format!("{}:{}", node.public_address, sip.listen_addr.port());
    candidate.parse().ok()
}

fn spawn_listeners(
    udp_socket: Arc<UdpSocket>,
    transaction_mgr: Arc<TransactionManager>,
    uas: Arc<IntegratedUAS>,
) -> Vec<JoinHandle<()>> {
    let mut handles = Vec::new();
    let (packet_tx, packet_rx) = mpsc::channel::<InboundPacket>(1024);

    // UDP reader: feeds inbound bytes into the packet channel.
    let udp_reader_socket = Arc::clone(&udp_socket);
    handles.push(tokio::spawn(async move {
        if let Err(e) = run_udp(udp_reader_socket, packet_tx).await {
            error!(error = %e, "UDP listener exited");
        }
    }));

    // Packet pump: parse → tm.receive_request → uas.dispatch.
    handles.push(tokio::spawn(async move {
        run_packet_pump(packet_rx, transaction_mgr, uas).await;
    }));

    handles
}

#[instrument(skip_all)]
async fn run_packet_pump(
    mut packet_rx: mpsc::Receiver<InboundPacket>,
    transaction_mgr: Arc<TransactionManager>,
    uas: Arc<IntegratedUAS>,
) {
    while let Some(packet) = packet_rx.recv().await {
        handle_packet(&transaction_mgr, &uas, packet).await;
    }
    debug!("packet pump exiting (channel closed)");
}

async fn handle_packet(
    transaction_mgr: &Arc<TransactionManager>,
    uas: &Arc<IntegratedUAS>,
    packet: InboundPacket,
) {
    let peer = packet.peer();
    let transport = packet.transport();
    let payload = packet.into_payload();

    let Some(request) = parse_request(&payload) else {
        // Could be a response (we're UAS-only in v1, so unsolicited
        // responses are dropped) or junk. parse_response would
        // catch the former; we don't care for now.
        debug!(peer = %peer, "inbound bytes did not parse as a SIP request");
        return;
    };

    let tx_kind = map_transport_kind(transport);
    let ctx = TransportContext::new(tx_kind, peer, None);

    if request.method().as_str() == "ACK" {
        // ACK doesn't open a server transaction; just notify the
        // matching invite TM key so the UAS can clear its 200 OK
        // retransmission timer (RFC 3261 §17.2.1, §17.1.1.3 — the
        // ACK transaction key uses the original INVITE's method).
        if let Some(branch) = sip_transaction::request_branch_id(&request) {
            let key = sip_transaction::TransactionKey::new(branch, sip_core::Method::Invite, true);
            transaction_mgr.ack_received(&key).await;
        }
        return;
    }

    let handle = transaction_mgr
        .receive_request(request.clone(), ctx.clone())
        .await;

    let uas = Arc::clone(uas);
    tokio::spawn(async move {
        if let Err(e) = uas.dispatch(&request, handle, &ctx).await {
            warn!(error = %e, "UAS dispatch failed");
        }
    });
}

fn map_transport_kind(kind: TpTransportKind) -> TxTransportKind {
    match kind {
        TpTransportKind::Udp => TxTransportKind::Udp,
        TpTransportKind::Tcp => TxTransportKind::Tcp,
        TpTransportKind::Tls => TxTransportKind::Tls,
        TpTransportKind::Sctp => TxTransportKind::Sctp,
        TpTransportKind::TlsSctp => TxTransportKind::TlsSctp,
        TpTransportKind::Ws => TxTransportKind::Ws,
        TpTransportKind::Wss => TxTransportKind::Wss,
    }
}

// ─── UDP-only transport dispatcher ───────────────────────────────────
//
// Production siphond carries TCP / TLS / WS pools too; the v1 cut
// here just speaks UDP. Sending a non-UDP response yields a clear
// error rather than a silent fallback so a misconfiguration is
// loud.

struct UdpDispatcher {
    socket: Arc<UdpSocket>,
}

impl UdpDispatcher {
    fn new(socket: Arc<UdpSocket>) -> Self {
        Self { socket }
    }
}

#[async_trait]
impl TransportDispatcher for UdpDispatcher {
    async fn dispatch(&self, ctx: &TransportContext, payload: Bytes) -> Result<()> {
        match ctx.transport() {
            TxTransportKind::Udp => send_udp(self.socket.as_ref(), &ctx.peer(), &payload)
                .await
                .with_context(|| format!("send_udp to {}", ctx.peer())),
            other => Err(anyhow!(
                "siphon-ai v1 only speaks UDP outbound; got {other:?} (peer={})",
                ctx.peer()
            )),
        }
    }
}

/// How long to wait for inbound listeners to settle on shutdown.
/// Currently informational only — `Runtime::run` aborts immediately
/// — but reserved for the future drain-active-calls path.
#[allow(dead_code)]
pub const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);
