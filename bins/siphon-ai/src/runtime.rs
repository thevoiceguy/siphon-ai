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
//! ## What's in scope (v1)
//!
//! - **SIP transports**: UDP, TCP, and TLS (SIPS). UDP and TCP
//!   share the same `[sip].listen` address per RFC 3261 §18; TLS
//!   binds on `[sip.tls].listen` (default 5061).
//! - Inbound INVITE → routed → MediaSetup → 200 OK → CallController.
//! - BYE / CANCEL via the CallRegistry.
//! - CDR (file + webhook), lifecycle webhooks (call_start, call_end),
//!   Prometheus metrics + `/health` + `/ready`.
//!
//! ## What's deferred
//!
//! - WebSocket SIP transport (`run_ws` / `run_wss`) — same shape as
//!   TCP/TLS, deferred until we have a deployment that needs it.
//! - Outbound REGISTER (UAC mode) — requires `[[register]]` config.
//! - HEP / Homer (depends on the upstream `hep-rs` crate).
//! - Admin endpoints (dynamic log level, force-hangup).

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use bytes::Bytes;
use forge_core::EventBus as ForgeEventBus;
use forge_engine::{MediaBridgeManager, SessionManager, SessionManagerConfig};
use forge_rtp::PortPoolConfig;
use sip_parse::{parse_request, parse_response};
use sip_transaction::{
    TransactionManager, TransportContext, TransportDispatcher, TransportKind as TxTransportKind,
};
use sip_transport::{
    load_rustls_server_config, run_tcp, run_tls, run_udp, send_stream, send_udp, InboundPacket,
    TransportKind as TpTransportKind,
};
use sip_uac::integrated::IntegratedUAC;
use sip_uas::integrated::{IntegratedUAS, UasRequestHandler};
use siphon_ai_cdr::{CdrSinkHandle, FileSink, MultiSink, NullSink, WebhookSink, WebhookSinkConfig};
use siphon_ai_config::{
    CdrConfig, CdrFileConfig, CdrWebhookConfig, Config, MediaConfig, NodeConfig,
    ObservabilityConfig, SipConfig, SipTlsConfig, SipTransport, WebhooksConfig,
};
use siphon_ai_core::{BridgingAcceptor, CallRegistry};
use siphon_ai_media_glue::MediaSetup;
use siphon_ai_sip_glue::{
    DialogTerminatorHandle, RegisterSourceResolver, RegistrationEntry, RegistrationManager,
    RoutingHandler,
};
use siphon_ai_telemetry::{install_recorder, ObservabilityServer, ReadinessFlag};
use siphon_ai_webhooks::{
    FilteredSink as WebhookFilteredSink, HttpSink as WebhookHttpSink,
    HttpSinkConfig as WebhookHttpSinkConfig, NullSink as WebhookNullSink, WebhookSinkHandle,
};
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
    /// Per-`[[register]]` background tasks. v1's tasks are no-ops
    /// (UAC drive lands in a follow-up); we still own the handles
    /// so shutdown awaits them cleanly.
    registration_mgr: RegistrationManager,
    registration_listeners: Vec<JoinHandle<()>>,
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
            registrations,
            cdr,
            observability,
            webhooks,
        } = config;

        // ─── Telemetry: install Prometheus recorder + spawn HTTP ───
        let readiness = ReadinessFlag::new();
        let observability_server = build_observability(observability, readiness.clone()).await?;

        let cdr_sink = build_cdr_sink(cdr).await?;
        // The same sink fans out call lifecycle events from the
        // acceptor AND registration_state_changed events from the
        // per-[[register]] tasks. Cheap to share — Arc<dyn …>.
        let webhook_sink = build_webhook_sink(webhooks)?;
        let webhook_sink_for_registrations = Arc::clone(&webhook_sink);

        // ─── Forge media stack ──────────────────────────────────────
        // One process-wide EventBus. Forge's session manager publishes
        // ForgeEvents (DTMF detect, session-state, quality reports) on
        // it; per-call MediaTaps subscribe and forward the ones the
        // bridge protocol covers (currently DTMF) over the WS as
        // BridgeOut events.
        let event_bus = Arc::new(ForgeEventBus::new());

        let session_mgr_config = SessionManagerConfig {
            port_pool_config: rtp_port_pool(&media)?,
            ..Default::default()
        };
        let session_mgr = SessionManager::new(session_mgr_config, Some(Arc::clone(&event_bus)));
        // Background task that reaps idle sessions per
        // SessionManagerConfig::cleanup_interval. Must run for forge
        // to enforce its session_timeout.
        session_mgr.start_monitoring().await;

        let bridge_mgr = Arc::new(MediaBridgeManager::new());
        let media_setup = Arc::new(MediaSetup::new(
            Arc::clone(&session_mgr),
            Arc::clone(&bridge_mgr),
            Arc::clone(&event_bus),
            node.public_address.clone(),
        ));

        // ─── Bridging acceptor + dialog registry ───────────────────
        // Built without the IntegratedUAS here because the routing
        // handler (which the UAS needs as its request handler) holds
        // an Arc to this acceptor. We close the cycle below via
        // `acceptor.install_uas(...)` once the UAS exists.
        let registry = CallRegistry::new();
        let acceptor = Arc::new(
            BridgingAcceptor::new(media_setup, bridge_defaults, registry.clone())
                .with_cdr_sink(cdr_sink)
                .with_webhook_sink(webhook_sink),
        );

        // ─── Registration manager ──────────────────────────────────
        // Seed the manager up-front so a /metrics scrape during the
        // cold-start window already shows pending/disabled rows. The
        // actual UAC drive tasks need the TransactionManager +
        // dispatcher, so they're spawned further down once those exist.
        let registration_mgr = RegistrationManager::new();
        let register_entries: Vec<RegistrationEntry> = registrations
            .iter()
            .map(|cfg| RegistrationEntry {
                name: cfg.name.clone(),
                server_addr: cfg.server_addr,
                register_on_startup: cfg.register_on_startup,
            })
            .collect();
        registration_mgr.seed(&register_entries);

        // ─── SIP routing handler ───────────────────────────────────
        let dialog_terminator: DialogTerminatorHandle = Arc::new(registry);
        let routing_handler = Arc::new(
            RoutingHandler::new(Arc::new(routes), Arc::clone(&acceptor))
                .with_dialog_terminator(dialog_terminator)
                .with_register_source_resolver(register_source_resolver(&registration_mgr)),
        );

        // ─── SIP transports + transaction manager ──────────────────
        // Bind UDP eagerly so a port-busy error surfaces here, not
        // after we log "ready". TCP / TLS listeners spawn inside
        // spawn_listeners; their accept loops own the bind.
        let udp_socket = Arc::new(
            UdpSocket::bind(sip.listen_addr)
                .await
                .with_context(|| format!("bind UDP {}", sip.listen_addr))?,
        );
        let dispatcher: Arc<dyn TransportDispatcher> =
            Arc::new(MultiTransportDispatcher::new(Arc::clone(&udp_socket)));
        let transaction_mgr = Arc::new(TransactionManager::new(Arc::clone(&dispatcher)));

        // System DNS resolver, shared by every per-[[register]] UAC
        // task. Cheap to clone (Arc inside). Full SRV/NAPTR-driven
        // failover lives in the resolver itself; v1 only relies on
        // the literal-IP fast path because the config crate rejects
        // hostnames with a clear v1.1 message.
        let sip_resolver = Arc::new(
            sip_dns::SipResolver::from_system()
                .with_context(|| "construct SipResolver from system DNS config")?,
        );

        // Load the TLS server config if `tls` is enabled. Loading
        // here (instead of inside spawn_listeners) makes a bad
        // cert/key path fail at startup with a clear error rather
        // than after the listener tries to accept.
        let tls_server_config = match (
            sip.transports.contains(&SipTransport::Tls),
            sip.tls.as_ref(),
        ) {
            (true, Some(tls)) => Some(load_sip_tls_server_config(tls)?),
            _ => None,
        };

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

        // Close the BridgingAcceptor ↔ IntegratedUAS cycle now that
        // both exist. The acceptor uses this handle in `on_matched`
        // to send the 200 OK via `IntegratedUAS::accept_invite`,
        // which registers the confirmed dialog with the SAME
        // dialog_manager that `IntegratedUAS::dispatch` consults on
        // the follow-up BYE.
        acceptor.install_uas(Arc::clone(&uas));

        // ─── Daemon-wide REFER UAC ─────────────────────────────────
        // One IntegratedUAC instance handles every accepted call's
        // BridgeIn::Transfer. Distinct from the per-[[register]] UACs
        // (those have AOR-specific credentials and identities); this
        // one is anonymous and uses the daemon's own SIP identity.
        // It MUST share the UAS's DialogManager so the per-call REFER
        // can find the dialog the UAS established on the inbound
        // 200 OK. CLAUDE.md §4.4: per-call state lives inside the
        // controller; process-wide plumbing is what we share here.
        let transfer_uac = build_transfer_uac(TransferUacBuild {
            local_uri: &local_uri,
            contact_uri: &contact_uri,
            listen_addr: sip.listen_addr,
            public_addr: sip_public_addr(&node, &sip),
            transaction_mgr: Arc::clone(&transaction_mgr),
            dispatcher: Arc::clone(&dispatcher),
            sip_resolver: Arc::clone(&sip_resolver),
        })?;
        let transfer_uac = Arc::new(transfer_uac);
        let dialog_manager = uas.dialog_manager();
        acceptor.install_transfer(Arc::clone(&transfer_uac), dialog_manager);

        // ─── Per-registration UAC drive tasks ──────────────────────
        // Now that the dispatcher and transaction manager exist, spawn
        // one async task per [[register]] block to drive the REGISTER
        // → refresh / retry loop. Tasks share the manager's shutdown
        // signal so the runtime's teardown path wakes them all at
        // once. See `crate::registration` for the loop semantics.
        let registration_listeners = crate::registration::spawn_registration_tasks(
            &registration_mgr,
            &registrations,
            Arc::clone(&transaction_mgr),
            Arc::clone(&dispatcher),
            Arc::clone(&sip_resolver),
            &sip,
            Arc::clone(&webhook_sink_for_registrations),
        );

        // ─── Spawn the inbound UDP/TCP/TLS readers + pump ─────────
        let udp_bound_addr = udp_socket
            .local_addr()
            .with_context(|| "read UDP local_addr after bind")?;
        let listeners = spawn_listeners(
            &sip,
            Arc::clone(&udp_socket),
            udp_bound_addr,
            Arc::clone(&transaction_mgr),
            Arc::clone(&uas),
            tls_server_config,
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
            registration_mgr,
            registration_listeners,
        })
    }

    /// Snapshot of registration state (one entry per
    /// `[[register]]` block). Useful for tests and admin
    /// introspection.
    pub fn registration_snapshot(&self) -> Vec<siphon_ai_sip_glue::RegistrationState> {
        self.registration_mgr.snapshot()
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

        // Tell registration tasks to exit. They observe the
        // shutdown notify on their next loop iter.
        self.registration_mgr.shutdown();
        for handle in self.registration_listeners {
            // Bound the wait so a flaky task doesn't block
            // shutdown; abort if still alive after a beat.
            match tokio::time::timeout(Duration::from_millis(250), handle).await {
                Ok(_) => {}
                Err(_) => debug!("registration task did not exit within 250ms"),
            }
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

/// Build the `RegisterSourceResolver` closure that the routing
/// handler installs. Looks the inbound peer up in the registration
/// manager; falls back to `"trunk"` for unregistered inbound (the
/// historical default).
fn register_source_resolver(mgr: &RegistrationManager) -> RegisterSourceResolver {
    let mgr = mgr.clone();
    Arc::new(move |_req, ctx| match mgr.resolve_source(ctx.peer()) {
        Some(name) => name,
        None => "trunk".to_string(),
    })
}

/// Load the TLS server config (cert + key) from disk paths in
/// `[sip.tls]`. Failure here is fatal — operators who set
/// `transports = ["tls"]` expect SIPS to actually work, not to
/// silently degrade to cleartext.
fn load_sip_tls_server_config(
    tls: &SipTlsConfig,
) -> Result<Arc<tokio_rustls::rustls::ServerConfig>> {
    let cert = tls
        .cert_path
        .to_str()
        .ok_or_else(|| anyhow!("[sip.tls].cert path is not valid UTF-8"))?;
    let key = tls
        .key_path
        .to_str()
        .ok_or_else(|| anyhow!("[sip.tls].key path is not valid UTF-8"))?;
    let cfg = load_rustls_server_config(cert, key).with_context(|| {
        format!(
            "load TLS cert={} key={}",
            tls.cert_path.display(),
            tls.key_path.display()
        )
    })?;
    info!(
        cert = %tls.cert_path.display(),
        key = %tls.key_path.display(),
        listen = %tls.listen_addr,
        "TLS server config loaded"
    );
    Ok(cfg)
}

/// Build the lifecycle webhook sink from `[webhooks]` config.
/// Returns `NullSink` when disabled. When enabled, wraps the
/// `HttpSink` in a `FilteredSink` if an `events` allowlist is set.
fn build_webhook_sink(cfg: WebhooksConfig) -> Result<WebhookSinkHandle> {
    if !cfg.enabled {
        return Ok(Arc::new(WebhookNullSink));
    }
    let url = cfg
        .url
        .ok_or_else(|| anyhow!("[webhooks].url unexpectedly empty"))?;
    let http = WebhookHttpSink::new(WebhookHttpSinkConfig {
        url: url.clone(),
        auth_header: cfg.auth_header,
        retry_max: cfg.retry_max,
        timeout_ms: cfg.timeout.as_millis() as u64,
    })
    .map_err(|e| anyhow!("lifecycle webhook client build failed: {e}"))?;
    info!(url = %url, allowlist = cfg.events.len(), "lifecycle webhook sink active");
    let sink: WebhookSinkHandle = if cfg.events.is_empty() {
        Arc::new(http)
    } else {
        Arc::new(WebhookFilteredSink::new(
            Arc::new(http) as WebhookSinkHandle,
            cfg.events,
        ))
    };
    Ok(sink)
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
        sinks.push(build_cdr_webhook_sink(&webhook_cfg)?);
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

fn build_cdr_webhook_sink(cfg: &CdrWebhookConfig) -> Result<CdrSinkHandle> {
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

/// Inputs to [`build_transfer_uac`]. Bundled into a struct so the
/// callsite stays readable and clippy is happy (the function would
/// otherwise have eight positional arguments).
struct TransferUacBuild<'a> {
    local_uri: &'a str,
    contact_uri: &'a str,
    listen_addr: SocketAddr,
    public_addr: Option<SocketAddr>,
    transaction_mgr: Arc<TransactionManager>,
    dispatcher: Arc<dyn TransportDispatcher>,
    sip_resolver: Arc<sip_dns::SipResolver>,
}

/// Build the daemon-wide UAC used by `BridgeIn::Transfer` to send
/// REFER inside an existing UAS-accepted dialog. No credentials —
/// the dialog is already authenticated; REFER inherits the dialog's
/// authentication state. Shares the system DNS resolver with the
/// per-[[register]] UACs so SRV/NAPTR resolution happens once.
fn build_transfer_uac(args: TransferUacBuild<'_>) -> Result<IntegratedUAC> {
    let mut builder = IntegratedUAC::builder()
        .local_uri(args.local_uri)
        .contact_uri(args.contact_uri)
        .transaction_manager(args.transaction_mgr)
        .dispatcher(args.dispatcher)
        .resolver(args.sip_resolver)
        .local_addr(args.listen_addr.to_string())
        .map_err(|e| anyhow!("transfer UAC local_addr: {e}"))?;
    if let Some(public) = args.public_addr {
        builder = builder
            .public_addr(public.to_string())
            .map_err(|e| anyhow!("transfer UAC public_addr: {e}"))?;
    }
    builder
        .build()
        .map_err(|e| anyhow!("transfer UAC build: {e}"))
}

fn rtp_port_pool(media: &MediaConfig) -> Result<PortPoolConfig> {
    match media.rtp_port_range {
        Some((min, max)) => PortPoolConfig::new(min, max)
            .map_err(|e| anyhow!("[media].rtp_port_range invalid: {e}")),
        None => Ok(PortPoolConfig::default()),
    }
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
    sip: &SipConfig,
    udp_socket: Arc<UdpSocket>,
    udp_bound_addr: SocketAddr,
    transaction_mgr: Arc<TransactionManager>,
    uas: Arc<IntegratedUAS>,
    tls_server_config: Option<Arc<tokio_rustls::rustls::ServerConfig>>,
) -> Vec<JoinHandle<()>> {
    let mut handles = Vec::new();
    let (packet_tx, packet_rx) = mpsc::channel::<InboundPacket>(1024);

    let want_udp = sip.transports.contains(&SipTransport::Udp);
    let want_tcp = sip.transports.contains(&SipTransport::Tcp);
    let want_tls = sip.transports.contains(&SipTransport::Tls);

    // UDP reader: feeds inbound bytes into the packet channel.
    if want_udp {
        let udp_reader_socket = Arc::clone(&udp_socket);
        let tx = packet_tx.clone();
        handles.push(tokio::spawn(async move {
            if let Err(e) = run_udp(udp_reader_socket, tx).await {
                error!(error = %e, "UDP listener exited");
            }
        }));
    }

    // TCP listener — same host:port as UDP per RFC 3261 §18 (the
    // SIP convention is to listen on the same port for udp/tcp).
    // Use `udp_bound_addr` rather than the config's `listen_addr`
    // so port-0 ("kernel picks") works — UDP picks first, TCP
    // binds to the same kernel-chosen port.
    if want_tcp {
        let bind = udp_bound_addr.to_string();
        let tx = packet_tx.clone();
        handles.push(tokio::spawn(async move {
            if let Err(e) = run_tcp(&bind, tx).await {
                error!(error = %e, "TCP listener exited");
            }
        }));
    }

    // TLS listener — separate bind (default port 5061 = SIPS).
    if want_tls {
        match (tls_server_config, sip.tls.as_ref()) {
            (Some(cfg), Some(tls)) => {
                let bind = tls.listen_addr.to_string();
                let tx = packet_tx.clone();
                handles.push(tokio::spawn(async move {
                    if let Err(e) = run_tls(&bind, cfg, tx).await {
                        error!(error = %e, "TLS listener exited");
                    }
                }));
            }
            _ => {
                // Compile-time validation guarantees both are
                // Some when the transport is enabled, but be loud
                // if that contract ever breaks.
                error!(
                    "TLS transport enabled but no [sip.tls] config / server config available; \
                     SIPS connections will be refused"
                );
            }
        }
    }

    // Drop our local clone so the channel closes when every
    // listener task does. The packet pump exits cleanly when its
    // recv() returns None.
    drop(packet_tx);

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
        // Not a request. Try parsing as a response — the daemon's
        // UAC drives outbound REGISTERs (CLAUDE.md §7.2 registered
        // mode), and the registrar's 200 OK / 401 / 4xx responses
        // arrive on this same socket. The transaction manager
        // matches them to in-flight client transactions by Via
        // branch.
        if let Some(response) = parse_response(&payload) {
            transaction_mgr.receive_response(response).await;
            return;
        }
        debug!(peer = %peer, "inbound bytes did not parse as a SIP request or response");
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

// ─── Multi-transport dispatcher (UDP + TCP + TLS) ─────────────────
//
// We're inbound-only (UAS) in v1: every response goes back over the
// same transport the request arrived on. For UDP we own the socket
// and `send_udp` writes to the peer. For stream transports
// (TCP/TLS), `run_tcp` / `run_tls` set `ctx.stream()` to the per-
// connection writer channel; `send_stream` pushes the payload
// through that channel back to the peer.
//
// Outbound TCP/TLS connect (without a `stream` already set in
// ctx) is what UAC mode would need — for v1 we error out cleanly
// rather than silently falling back to UDP.

struct MultiTransportDispatcher {
    udp_socket: Arc<UdpSocket>,
}

impl MultiTransportDispatcher {
    fn new(udp_socket: Arc<UdpSocket>) -> Self {
        Self { udp_socket }
    }
}

#[async_trait]
impl TransportDispatcher for MultiTransportDispatcher {
    async fn dispatch(&self, ctx: &TransportContext, payload: Bytes) -> Result<()> {
        match ctx.transport() {
            TxTransportKind::Udp => send_udp(self.udp_socket.as_ref(), &ctx.peer(), &payload)
                .await
                .with_context(|| format!("send_udp to {}", ctx.peer())),
            TxTransportKind::Tcp | TxTransportKind::Tls => match ctx.stream() {
                Some(writer) => {
                    let target = match ctx.transport() {
                        TxTransportKind::Tcp => sip_transport::TransportKind::Tcp,
                        TxTransportKind::Tls => sip_transport::TransportKind::Tls,
                        _ => unreachable!(),
                    };
                    send_stream(target, writer, payload)
                        .await
                        .with_context(|| format!("send_stream to {}", ctx.peer()))
                }
                None => Err(anyhow!(
                    "outbound {:?} without an existing stream is not supported in v1 \
                     (peer={}); inbound-only UAS",
                    ctx.transport(),
                    ctx.peer()
                )),
            },
            other => Err(anyhow!(
                "transport {other:?} is not enabled in this build (peer={})",
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
