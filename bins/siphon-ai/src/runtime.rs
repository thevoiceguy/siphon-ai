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

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
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
    load_rustls_server_config,
    pool::{ConnectionPool, TlsClientConfig, TlsPool},
    run_tcp, run_tls_with_swappable_config, run_udp, send_stream, send_udp, InboundPacket,
    TransportKind as TpTransportKind,
};
use sip_uac::integrated::IntegratedUAC;
use sip_uas::integrated::{IntegratedUAS, UasRequestHandler};
use siphon_ai_cdr::{
    CdrSinkHandle, FileSink, HepCdrSink, MultiSink, NullSink, WebhookSink, WebhookSinkConfig,
};
use siphon_ai_config::{
    CdrConfig, CdrFileConfig, CdrWebhookConfig, Config, HepConfig, MediaConfig, NodeConfig,
    ObservabilityConfig, ParkTimeoutAction as ConfigParkTimeoutAction, SipConfig, SipTlsConfig,
    SipTransport, WebhooksConfig,
};
use siphon_ai_core::{
    default_call_id_factory, BridgingAcceptor, CallControlRegistry, CallRegistry, ConferenceAdmin,
    ConferenceLimits, ConferenceRegistry, ConsultRegistry, OutboundGateway, OutboundGuard,
    OutboundOriginator, OutboundService, ParkAdmin, ParkContext, ParkRegistry, ParkSettings,
    ParkTimeoutAction as CoreParkTimeoutAction, StaticCredentials,
};
use siphon_ai_media_glue::MediaSetup;
use siphon_ai_sip_glue::{
    DialogTerminatorHandle, RegisterSourceResolver, RegistrationEntry, RegistrationManager,
    RoutingHandler,
};
use siphon_ai_telemetry::{
    admin::{
        AdminCallRegistry, AdminConference, AdminOutbound, AdminPark, AdminState,
        CallRegistryHandle, RegistrationRow,
    },
    install_recorder, AdminServer, HepTelemetry, HepTelemetryBuild, HepWorkerHandle,
    LogFilterHandle, ObservabilityServer, ReadinessFlag,
};
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
    /// `Some` when `[admin]` is configured — the authenticated admin
    /// HTTP listener. Dropped on shutdown.
    admin: Option<AdminServer>,
    /// `Some` when `[hep].enabled = true`. Share-by-Arc so the admin
    /// state, CDR sink, and per-call code can borrow it; the UDP
    /// worker lives separately on `hep_worker` for shutdown. The
    /// field is held for its drop-on-shutdown side effect (releasing
    /// the last Arc clone the worker observes via the shared sink).
    #[allow(dead_code)]
    hep_telemetry: Option<Arc<HepTelemetry>>,
    /// The HEP UDP worker JoinHandle. Held for the daemon's
    /// lifetime; aborted on shutdown.
    hep_worker: Option<HepWorkerHandle>,
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
    pub async fn build(config: Config, log_filter: LogFilterHandle) -> Result<Self> {
        Self::build_with_reload(config, None, log_filter).await
    }

    /// Like [`build`](Self::build) but with the `--config` path so a
    /// `SIGHUP` can re-read it for hot reload. `config_path = None`
    /// disables config reload (the daemon still claims SIGHUP for the
    /// TLS-cert hot-reload / no-op) — used by tests that build from an
    /// in-memory config with no backing file.
    pub async fn build_with_reload(
        config: Config,
        config_path: Option<PathBuf>,
        log_filter: LogFilterHandle,
    ) -> Result<Self> {
        // Fingerprint the restart-required sections before the config
        // is destructured, so a later SIGHUP reload can detect changes
        // to sections that need a restart.
        let restart_fingerprints = crate::reload::restart_fingerprints(&config);
        let Config {
            node,
            sip,
            media,
            bridge_defaults,
            routes,
            registrations,
            trunks,
            security,
            recording,
            outbound,
            conference,
            cdr,
            observability,
            webhooks,
            hep,
            park,
            admin,
        } = config;

        // ─── Telemetry: install Prometheus recorder ─────────────────
        // We defer the HTTP listener until after the call registry +
        // registration manager exist so admin routes have dependencies
        // wired in. Prometheus install can happen up-front; the
        // recorder is a global static.
        let readiness = ReadinessFlag::new();
        let prometheus_handle =
            install_recorder().map_err(|e| anyhow!("install Prometheus recorder: {e}"))?;

        // ─── HEP3 / Homer wiring ──────────────────────────────────
        // Built before any SIP / forge traffic so the global emitters
        // are installed before the listeners can fire. When `[hep]
        // .enabled = false` returns `None` with zero cost.
        let hep_built = build_hep_telemetry(&node, hep).await?;
        let (hep_telemetry, hep_worker) = match hep_built {
            Some((t, w)) => (Some(t), Some(w)),
            None => (None, None),
        };

        // Whether a durable spool is active — captured before the
        // configs are moved into the sink builders. A spooling sink
        // can't be hot-swapped (its drain worker is stateful), so a
        // SIGHUP reload treats delivery changes there as restart-only.
        let webhook_spool_active = webhooks.spool_dir.is_some();
        let cdr_spool_active = cdr
            .webhook
            .as_ref()
            .and_then(|w| w.spool_dir.as_ref())
            .is_some();
        // Baseline sink fingerprints so a reload only touches (and only
        // warns about) a sink whose config actually changed.
        let webhook_fingerprint = format!("{:?}", webhooks);
        let cdr_fingerprint = format!("{:?}", cdr);

        // Wrap both sinks so a SIGHUP reload can swap the delegate
        // behind them without touching the acceptor / outbound /
        // registration handles that hold the wrapper.
        let cdr_swap = Arc::new(crate::reload::SwappableCdrSink::new(
            build_cdr_sink(cdr, hep_telemetry.as_deref()).await?,
        ));
        let cdr_sink: CdrSinkHandle = cdr_swap.clone();
        // The same sink fans out call lifecycle events from the
        // acceptor AND registration_state_changed events from the
        // per-[[register]] tasks. Cheap to share — Arc<dyn …>.
        let webhook_swap = Arc::new(crate::reload::SwappableWebhookSink::new(
            build_webhook_sink(webhooks)?,
        ));
        let webhook_sink: WebhookSinkHandle = webhook_swap.clone();
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
        // Translate `[sip].min_session_expires_secs` /
        // `preferred_session_expires_secs` into the upstream policy
        // every accepted INVITE negotiates against. v1 always picks
        // refresher=uac (the peer drives refreshes) — gateway-style
        // UASes don't initiate UPDATE refreshes themselves, so this
        // matches the implementation.
        let session_timer_policy = sip_uas::SessionTimerPolicy {
            min_se: sip.min_session_expires,
            preferred_se: sip.preferred_session_expires,
            force_refresher: Some(sip_core::RefresherRole::Uac),
        };
        // ─── STIR/SHAKEN verifier ──────────────────────────────────
        // Built once and shared across every call (it owns a process-wide
        // signing-cert cache). `None` when verification is disabled, so the
        // accept path stays exactly as it was for a 0.3.x config. The
        // trust-anchor file was already existence-checked at config compile;
        // `from_config` decodes it here and fails startup loud if it can't.
        let verifier = if security.stir_shaken.enabled {
            let v = siphon_ai_stir_shaken::Verifier::from_config(&security.stir_shaken)
                .map_err(|e| anyhow!("build STIR/SHAKEN verifier: {e}"))?;
            info!(
                trust_anchors = %security.stir_shaken.trust_anchors.display(),
                require_identity = security.stir_shaken.require_identity,
                "STIR/SHAKEN verification enabled"
            );
            Some(Arc::new(v))
        } else {
            None
        };

        // Attestation gate (config already cross-validated: a non-`none`
        // floor requires stir_shaken enabled). Inert by default — only bites
        // when a verifier is installed and a floor / require_identity is set.
        let security_policy = siphon_ai_core::AcceptSecurityPolicy {
            min_attestation: security.min_attestation,
            min_attestation_response: security.min_attestation_response,
            require_identity: security.stir_shaken.require_identity,
        };

        // The acceptor consumes `media_setup` + `bridge_defaults` + the CDR
        // and webhook sinks; the outbound service (built below) needs them
        // too. Cheap clones (all Arc / already-cloneable). Used only in the
        // `[outbound]`-enabled branch, so harmless when outbound is off.
        let outbound_media = Arc::clone(&media_setup);
        let outbound_bridge_defaults = bridge_defaults.clone();
        let outbound_cdr_sink = Arc::clone(&cdr_sink);
        let outbound_webhook_sink = Arc::clone(&webhook_sink);

        // Conference registry (0.7.0). Built once and shared (Clone) by
        // the inbound acceptor and the outbound service so a bot on
        // either kind of call can `conference_join`. `None` when
        // `[conference].enabled = false` (the default) — no registry is
        // allocated and joins are rejected with `conference_failed`.
        let conference_registry = if conference.enabled {
            Some(
                ConferenceRegistry::new(ConferenceLimits {
                    enabled: true,
                    max_rooms: conference.max_rooms,
                    max_participants_per_room: conference.max_participants_per_room,
                    join_tones: conference.join_tones,
                })
                // conference_created / conference_ended webhooks.
                .with_webhooks(Arc::clone(&webhook_sink)),
            )
        } else {
            None
        };
        // Bridge-id → CallHandle table the admin conference API uses to
        // reach any active call (inbound or outbound). Shared by the
        // acceptor, the outbound service, and the ConferenceAdmin.
        // Cheap and empty when conferencing is off.
        let control_registry = CallControlRegistry::new();

        // Park context (0.7.0). Built once and shared (Clone) by the
        // acceptor and the outbound service so any call — inbound or
        // outbound — can be parked. `None` when `[park].enabled = false`
        // (the default): no `ParkRegistry` is allocated and park requests
        // are rejected. The `ParkRegistry` inside is also handed to the
        // admin `ParkAdmin` so `GET /admin/v1/parked` and the cap share
        // one table.
        let park_context = if park.enabled {
            let settings = ParkSettings {
                moh_file: park.moh_file,
                timeout: park.timeout,
                timeout_action: match park.timeout_action {
                    ConfigParkTimeoutAction::Hangup => CoreParkTimeoutAction::Hangup,
                    ConfigParkTimeoutAction::Keep => CoreParkTimeoutAction::Keep,
                },
            };
            Some(ParkContext {
                settings,
                registry: ParkRegistry::new(park.max_parked),
                // call_parked / call_retrieved / park_timeout webhooks.
                webhooks: Some(Arc::clone(&webhook_sink)),
            })
        } else {
            None
        };

        let mut acceptor_builder = BridgingAcceptor::new(media_setup, bridge_defaults, registry.clone())
            .with_cdr_sink(cdr_sink)
            .with_webhook_sink(Arc::clone(&webhook_sink))
            .with_session_timer_policy(session_timer_policy)
            .with_call_progress(sip.call_progress)
            .with_allow_delayed_offer(sip.allow_delayed_offer)
            .with_verifier(verifier)
            .with_security_policy(security_policy)
            // Share the same HEP worker the SIP/RTCP/CDR emitters use so
            // the verstat chunk lands on the same Homer call view.
            .with_hep_telemetry(hep_telemetry.clone())
            .with_recording(recording)
            .with_hold_moh_file(media.moh_file.clone())
            .with_control_registry(control_registry.clone());
        if let Some(reg) = &conference_registry {
            acceptor_builder = acceptor_builder.with_conference(reg.clone());
        }
        if let Some(ctx) = &park_context {
            acceptor_builder = acceptor_builder.with_park(ctx.clone());
        }
        let acceptor = Arc::new(acceptor_builder);

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
        // Trunk allowlist gate. Installed only when the operator
        // declared one or more `[[trunk]]` blocks; with zero blocks
        // we leave the gate unset and the routing handler accepts
        // INVITEs from any source (legacy posture, documented as
        // dev / behind-firewall only).
        // Route table behind an ArcSwap so a SIGHUP reload can swap the
        // dialplan for new INVITEs without dropping in-flight calls.
        let route_swap = Arc::new(arc_swap::ArcSwap::from_pointee(routes));
        let mut routing_handler_builder =
            RoutingHandler::new(Arc::clone(&route_swap), Arc::clone(&acceptor))
                .with_dialog_terminator(dialog_terminator)
                .with_register_source_resolver(register_source_resolver(&registration_mgr));
        if !trunks.is_empty() {
            let gate: Arc<dyn siphon_ai_sip_glue::TrunkAllowlist> =
                Arc::new(ConfigTrunkAllowlist::new(trunks));
            routing_handler_builder = routing_handler_builder.with_trunk_gate(gate);
        }
        let routing_handler = Arc::new(routing_handler_builder);

        // ─── SIP transports + transaction manager ──────────────────
        // Bind UDP eagerly so a port-busy error surfaces here, not
        // after we log "ready". TCP / TLS listeners spawn inside
        // spawn_listeners; their accept loops own the bind.
        let udp_socket = Arc::new(
            UdpSocket::bind(sip.listen_addr)
                .await
                .with_context(|| format!("bind UDP {}", sip.listen_addr))?,
        );
        // Client-side connection pools for OUTBOUND TCP/TLS — what lets
        // a gateway or registration with `transport = "tcp"|"tls"`
        // actually dial out. Responses arriving on these client
        // connections are fed back into the inbound packet pipeline in
        // spawn_listeners (set_inbound_tx). Inbound dialogs are
        // unaffected: their requests/responses keep riding the
        // per-connection writer the listener attached.
        let tcp_client_pool = Arc::new(ConnectionPool::new());
        let tls_client_pool = Arc::new(TlsPool::new());
        let tls_client_config = build_tls_client_config(sip.tls_client_extra_ca.as_deref())
            .with_context(|| "build client TLS verification roots")?;
        let dispatcher: Arc<dyn TransportDispatcher> = Arc::new(MultiTransportDispatcher::new(
            Arc::clone(&udp_socket),
            Arc::clone(&tcp_client_pool),
            Arc::clone(&tls_client_pool),
            tls_client_config,
        ));
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
        //
        // The loaded `ServerConfig` is wrapped in an `ArcSwap` so a
        // SIGHUP handler (W5) can hot-swap it for a fresh cert
        // without dropping in-flight TLS sessions — see
        // [`spawn_sighup_reloader`] below.
        let tls_server_config: Option<Arc<arc_swap::ArcSwap<tokio_rustls::rustls::ServerConfig>>> =
            match (
                sip.transports.contains(&SipTransport::Tls),
                sip.tls.as_ref(),
            ) {
                (true, Some(tls)) => {
                    let initial = load_sip_tls_server_config(tls)?;
                    Some(Arc::new(arc_swap::ArcSwap::from(initial)))
                }
                _ => None,
            };

        // The SIGHUP handler (config reload + TLS cert reload) is
        // spawned *after* the outbound service is built (it needs the
        // gateway-reload handle). The TLS swap handle is consumed by the
        // listeners below, so clone it for the handler here.
        let tls_for_reload = tls_server_config.clone();

        // ─── Integrated UAS ────────────────────────────────────────
        let local_uri = sip_local_uri(&node, &sip);
        let contact_uri = sip.contact.as_deref().unwrap_or(&local_uri).to_string();
        let uas: Arc<dyn UasRequestHandler> = Arc::clone(&routing_handler) as _;
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

        // Wire the routing handler's response-auto-fill path. The
        // trunk-rejection 403 and the route-no-match 404/488 paths
        // build responses directly with `UserAgentServer::
        // create_response` and bypass the auto-fill that the rest
        // of the UAS dispatch loop applies (Contact / User-Agent /
        // topmost-Via `rport` + `received`). The handler holds a
        // `Weak<IntegratedUAS>` and upgrades it on every fill call,
        // so this is a one-way reference that doesn't create a
        // cycle with `IntegratedUAS::request_handler`.
        routing_handler.install_uas_filler(Arc::downgrade(&uas));

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
        // Attended-transfer consult lookup (DEV_PLAN_0.6.1 §2.1): one
        // daemon-wide registry shared between the transfer tasks (readers)
        // and the outbound service (writer, below). With [outbound] off it
        // stays empty and attended transfers fail gracefully.
        let consult_registry = ConsultRegistry::new();
        acceptor.install_transfer(
            Arc::clone(&transfer_uac),
            dialog_manager,
            consult_registry.clone(),
        );

        // ─── Per-registration UAC drive tasks ──────────────────────
        // Now that the dispatcher and transaction manager exist, spawn
        // one async task per [[register]] block to drive the REGISTER
        // → refresh / retry loop. Tasks share the manager's shutdown
        // signal so the runtime's teardown path wakes them all at
        // once. See `crate::registration` for the loop semantics.
        // REGISTER Via/Contact must advertise a reachable address, not
        // the socket bind — a wildcard bind (`0.0.0.0`/`::`) is not a
        // routable Contact and registrars can't send INVITEs back to it.
        // `sip_public_addr` combines `[node].public_address` (always
        // populated post-compile; wildcard bind without it is a load
        // error) with the listen port; fall back to the bind addr only
        // for a concrete, non-wildcard listen.
        let registration_advertised_addr = sip_public_addr(&node, &sip)
            .unwrap_or(sip.listen_addr)
            .to_string();
        let registration_listeners = crate::registration::spawn_registration_tasks(
            &registration_mgr,
            &registrations,
            Arc::clone(&transaction_mgr),
            Arc::clone(&dispatcher),
            Arc::clone(&sip_resolver),
            &registration_advertised_addr,
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
            &tcp_client_pool,
            &tls_client_pool,
        )
        .await;

        // ─── Outbound origination service (0.6.0) ──────────────────
        // One authenticated UAC + originator per `[[gateway]]`, then the
        // daemon-wide service the `POST /admin/v1/calls` endpoint drives.
        // Only when `[outbound]` is enabled (a positive concurrency cap);
        // otherwise the endpoint returns 501. As of 0.10.0 the originate
        // endpoint requires an `admin`-role bearer token (it's on the
        // authenticated `[admin]` listener); the cap + rate limit remain
        // the abuse guardrails on top of that (DEV_PLAN_0.6.0 §9.5/§9.6).
        // SIGHUP gateway hot-reload (0.12.1): when outbound is enabled we
        // keep the build deps + the concrete service so a reload can
        // rebuild + swap the gateway set.
        let outbound_reload: Option<crate::reload::OutboundReload>;
        let outbound_handle: Option<AdminOutbound> = if outbound.enabled() {
            // Per-process DTLS cert for answering a peer's DTLS-SRTP offer
            // on an outbound delayed call (0.9.3). Shared across gateways
            // (and reused across reloads), same posture as the inbound
            // acceptor's cert.
            let outbound_dtls_cert = Arc::new(
                forge_rtp::dtls::DtlsCertificate::generate()
                    .map_err(|e| anyhow!("outbound DTLS cert generation failed: {e}"))?,
            );
            let gateway_deps = GatewayBuildDeps {
                outbound_media: Arc::clone(&outbound_media),
                outbound_dtls_cert,
                local_uri: local_uri.clone(),
                contact_uri: contact_uri.clone(),
                listen_addr: sip.listen_addr,
                public_addr: sip_public_addr(&node, &sip),
                transaction_mgr: Arc::clone(&transaction_mgr),
                dispatcher: Arc::clone(&dispatcher),
                sip_resolver: Arc::clone(&sip_resolver),
            };
            let gateways = build_gateways(&outbound, &gateway_deps)?;
            let guard = OutboundGuard::new(outbound.max_concurrent, outbound.rate_limit_per_sec);
            let mut service = OutboundService::new(
                gateways,
                guard,
                outbound_bridge_defaults,
                default_call_id_factory(),
                outbound_cdr_sink,
                outbound_webhook_sink,
                consult_registry.clone(),
            );
            // Outbound bots can join conferences too (§9.1 — a room is
            // composed of any active calls). Share the same registries.
            service = service.with_control_registry(control_registry.clone());
            // Hold music for the WS-reconnect gap on outbound legs (0.7.3) —
            // the same [media].moh_file the inbound acceptor uses.
            service = service.with_moh_file(media.moh_file.clone());
            if let Some(reg) = &conference_registry {
                service = service.with_conference(reg.clone());
            }
            // Outbound bots can park/retrieve too (0.7.0).
            if let Some(ctx) = &park_context {
                service = service.with_park(ctx.clone());
            }
            info!(
                gateways = outbound.gateways.len(),
                max_concurrent = outbound.max_concurrent,
                "outbound origination enabled"
            );
            let service = Arc::new(service);
            outbound_reload = Some(crate::reload::OutboundReload {
                gateway_fingerprint: crate::reload::gateway_fingerprint(&outbound),
                service: Arc::clone(&service),
                deps: gateway_deps,
            });
            Some(service as AdminOutbound)
        } else {
            outbound_reload = None;
            None
        };

        // ─── SIGHUP handler (config reload + TLS cert reload) ──────
        // Spawned here so it has the gateway-reload handle. Always
        // installed: the default SIGHUP disposition is *terminate*, so
        // even a no-config-path / no-TLS daemon must claim the signal or
        // `systemctl reload` would kill it.
        match config_path {
            // Daemon with a backing config file: full SIGHUP reload
            // (config file + gateways + TLS cert).
            Some(path) => {
                let initial_text = std::fs::read_to_string(&path).unwrap_or_default();
                crate::reload::spawn_reload_handler(crate::reload::ReloadContext {
                    config_path: path,
                    initial_text,
                    route_swap: Arc::clone(&route_swap),
                    webhook_swap: Arc::clone(&webhook_swap),
                    cdr_swap: Arc::clone(&cdr_swap),
                    hep_telemetry: hep_telemetry.clone(),
                    webhook_spool_active,
                    cdr_spool_active,
                    webhook_fingerprint,
                    cdr_fingerprint,
                    outbound: outbound_reload,
                    tls: sip.tls.clone().zip(tls_for_reload),
                    restart_fingerprints,
                });
            }
            // No backing file (tests): preserve the prior TLS-cert /
            // no-op SIGHUP behavior.
            None => spawn_sighup_handler(sip.tls.clone(), tls_for_reload),
        }

        // ─── Build admin state + observability HTTP listener ──────
        // Deferred until now so admin endpoints have the call
        // registry, registration manager, hep telemetry, and log-
        // filter reload handle all wired in. Any of these being None
        // makes the corresponding endpoint return 503 rather than
        // crashing — see telemetry::admin docs.
        let call_registry_for_admin = acceptor.registry().clone();
        let registration_mgr_for_admin = registration_mgr.clone();
        let admin_state = AdminState {
            call_registry: Some(Arc::new(RuntimeCallRegistry {
                inner: call_registry_for_admin,
            }) as AdminCallRegistry),
            registration_snapshot: Some(Arc::new(move || {
                registration_mgr_for_admin
                    .snapshot()
                    .into_iter()
                    .map(registration_state_to_row)
                    .collect()
            })),
            log_filter: Some(log_filter),
            hep: hep_telemetry.clone(),
            outbound: outbound_handle,
            // Conference admin CRUD, only when conferencing is on.
            conference: conference_registry.as_ref().map(|reg| {
                Arc::new(ConferenceAdmin::new(reg.clone(), control_registry.clone()))
                    as AdminConference
            }),
            // Park admin CRUD, only when park is on. Shares the same
            // ParkRegistry the acceptor/outbound legs register into.
            park: park_context.as_ref().map(|ctx| {
                Arc::new(ParkAdmin::new(
                    ctx.registry.clone(),
                    control_registry.clone(),
                )) as AdminPark
            }),
        };
        let observability_server =
            build_observability(observability, readiness.clone(), prometheus_handle).await?;

        // Authenticated admin listener (0.10.0), separate from the open
        // observability listener. `None` when no `[admin]` block is
        // configured — `/admin/*` is then not served at all.
        let admin_server = build_admin(admin, admin_state).await?;

        // We're now serving SIP — let the readiness probe flip.
        readiness.mark_ready();

        Ok(Self {
            sip_listen: sip.listen_addr,
            udp_socket,
            transaction_mgr,
            uas,
            listeners,
            observability: observability_server,
            admin: admin_server,
            hep_telemetry,
            hep_worker,
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
        // Stop the authenticated admin listener.
        if let Some(server) = self.admin {
            server.shutdown().await;
        }

        // Drain the HEP UDP worker — aborts the task, bounded wait.
        // The telemetry handle itself is dropped here too (Arc
        // refcount goes to zero once `self.hep_telemetry` is out of
        // scope at function end).
        if let Some(worker) = self.hep_worker {
            worker.shutdown().await;
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

/// Spawn the **open** observability `/health` / `/ready` / `/metrics`
/// HTTP server. The Prometheus recorder is installed earlier in
/// `Runtime::build` (so metric calls in call layers don't crash even
/// when `[observability]` is disabled); the `prometheus` handle here is
/// just the renderer for `/metrics`. `/admin/*` is NOT served here
/// (0.10.0) — see [`build_admin`].
async fn build_observability(
    cfg: ObservabilityConfig,
    readiness: ReadinessFlag,
    prometheus: siphon_ai_telemetry::PrometheusHandle,
) -> Result<Option<ObservabilityServer>> {
    if !cfg.enabled {
        debug!("[observability].enabled = false; skipping HTTP listener");
        return Ok(None);
    }
    let listen = cfg
        .http_listen
        .ok_or_else(|| anyhow!("[observability].http_listen unexpectedly empty"))?;
    let server = ObservabilityServer::start(listen, prometheus, readiness)
        .await
        .with_context(|| format!("bind observability HTTP {listen}"))?;
    Ok(Some(server))
}

/// Spawn the **authenticated** admin HTTP server (`[admin]`). `None`
/// when no `[admin]` block is configured — `/admin/*` is then not served
/// at all (the secure default). Warns when the bind is not loopback: the
/// admin listener is plain HTTP, so the bearer token would travel in the
/// clear on a routable address (pair with a TLS-terminating proxy until
/// native `[admin].tls` lands).
async fn build_admin(
    cfg: Option<siphon_ai_config::AdminConfig>,
    admin_state: AdminState,
) -> Result<Option<AdminServer>> {
    let Some(cfg) = cfg else {
        debug!("no [admin] block; /admin/* is not served");
        return Ok(None);
    };
    if !cfg.listen_addr.ip().is_loopback() {
        warn!(
            target: "siphon_ai_config",
            listen = %cfg.listen_addr,
            "[admin].listen is not a loopback address and the admin listener is plain HTTP — \
             the bearer token travels in the clear on the wire. Bind loopback or front the \
             admin listener with a TLS-terminating proxy."
        );
    }
    let server = AdminServer::start(cfg.listen_addr, cfg.auth, admin_state)
        .await
        .with_context(|| format!("bind admin HTTP {}", cfg.listen_addr))?;
    Ok(Some(server))
}

/// Adapter that exposes `CallRegistry` through the `admin` trait
/// surface without forcing telemetry to depend on `siphon-ai-core`.
struct RuntimeCallRegistry {
    inner: CallRegistry,
}

impl CallRegistryHandle for RuntimeCallRegistry {
    fn snapshot_ids(&self) -> Vec<String> {
        self.inner.snapshot_call_ids()
    }
    fn hangup(&self, sip_call_id: &str) -> bool {
        match self.inner.lookup(sip_call_id) {
            Some(handle) => {
                handle.shutdown();
                true
            }
            None => false,
        }
    }
}

/// Map `siphon-ai-sip-glue`'s `RegistrationState` onto the
/// telemetry crate's `RegistrationRow`. Lives here (not in
/// telemetry) so telemetry doesn't have to dep on sip-glue.
fn registration_state_to_row(s: siphon_ai_sip_glue::RegistrationState) -> RegistrationRow {
    RegistrationRow {
        name: s.name,
        server_addr: s.server_addr.to_string(),
        status: s.status.as_str().to_string(),
        last_attempt_at: s.last_attempt_at.map(|t| t.to_rfc3339()),
        expires_at: s.expires_at.map(|t| t.to_rfc3339()),
        last_error: s.last_error,
    }
}

/// Build the daemon's HEP3 plumbing from compiled `[hep]` config.
/// Returns `Ok(None)` when disabled, `Ok(Some(...))` when wired —
/// installing both `sip-hep` and `forge-hep` global emitters as a
/// side effect so siphon-rs and forge-media start shipping the
/// moment the first packet flows.
async fn build_hep_telemetry(
    node: &NodeConfig,
    cfg: HepConfig,
) -> Result<Option<(Arc<HepTelemetry>, HepWorkerHandle)>> {
    if !cfg.enabled {
        debug!("[hep].enabled = false; HEP shipping disabled");
        return Ok(None);
    }
    // Compile-time validation guarantees these are Some when enabled,
    // but be defensive — surface a clear startup error rather than
    // panicking inside the builder.
    let collector = cfg
        .collector
        .ok_or_else(|| anyhow!("[hep].collector unexpectedly empty when enabled"))?;
    let capture_id = cfg
        .capture_id
        .ok_or_else(|| anyhow!("[hep].capture_id unexpectedly empty when enabled"))?;

    let (telemetry, worker) = HepTelemetry::build(HepTelemetryBuild {
        collector,
        capture_id,
        capture_password: cfg.capture_password,
        queue_capacity: cfg.queue_capacity,
        node_id: node.id.clone(),
    })
    .await
    .with_context(|| format!("build HEP UDP sink for collector {collector}"))?;

    info!(
        collector = %collector,
        capture_id,
        "HEP3 shipping active (SIP + RTCP + RTP-QoS + log + CDR chunks)"
    );
    Ok(Some((Arc::new(telemetry), worker)))
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

/// `[[trunk]]` allowlist consulted by the routing handler on every
/// new INVITE. Walks the operator-declared list in order: an entry
/// matches when its source-IP allowlist (if any) AND its
/// From-host allowlist (if any) both match. The first matching
/// trunk's name becomes the call's `register_source`. No match →
/// the handler responds 403. See `docs/CONFIG.md` §11 for the
/// threat model.
struct ConfigTrunkAllowlist {
    trunks: Vec<siphon_ai_config::TrunkConfig>,
}

impl ConfigTrunkAllowlist {
    fn new(trunks: Vec<siphon_ai_config::TrunkConfig>) -> Self {
        Self { trunks }
    }

    /// Extract the host part of the inbound INVITE's `From:` URI,
    /// lowercased for case-insensitive match. Returns `None` when
    /// the header is missing or the URI doesn't parse — those
    /// requests can still match an IP-only trunk but never a
    /// from_hosts-only one.
    fn extract_from_host(request: &sip_core::Request) -> Option<String> {
        let raw = request.headers().get_smol("From")?;
        // SIP `From` headers look like `"Display" <sip:user@host:port>;tag=…`
        // or bare `sip:user@host` — pull out whatever is between the
        // first `@` and the next `;` / `>` / end-of-string.
        let s = raw.as_str();
        let at = s.find('@')?;
        let after_at = &s[at + 1..];
        let end = after_at
            .find(['>', ';', ' ', '\t'])
            .unwrap_or(after_at.len());
        let host_with_port = &after_at[..end];
        // Strip optional `:port`.
        let host = match host_with_port.rfind(':') {
            Some(colon) => &host_with_port[..colon],
            None => host_with_port,
        };
        if host.is_empty() {
            return None;
        }
        Some(host.to_ascii_lowercase())
    }
}

impl siphon_ai_sip_glue::TrunkAllowlist for ConfigTrunkAllowlist {
    fn identify(
        &self,
        request: &sip_core::Request,
        ctx: &sip_transaction::TransportContext,
    ) -> Option<String> {
        let peer_ip = ctx.peer().ip();
        let from_host = Self::extract_from_host(request);
        for trunk in &self.trunks {
            let ip_ok = trunk.peer_addrs.is_empty()
                || trunk
                    .peer_addrs
                    .iter()
                    .any(|cidr: &siphon_ai_config::TrunkCidr| cidr.contains(peer_ip));
            let host_ok = trunk.from_hosts.is_empty()
                || from_host
                    .as_deref()
                    .map(|h| trunk.from_hosts.iter().any(|allowed| allowed == h))
                    .unwrap_or(false);
            if ip_ok && host_ok {
                return Some(trunk.name.clone());
            }
        }
        None
    }
}

/// Load the TLS server config (cert + key) from disk paths in
/// Verification roots for OUTGOING TLS connections: the bundled
/// webpki (Mozilla CA) roots — sufficient for public trunks like
/// Twilio — plus the operator's `[sip.tls_client].extra_ca` PEM
/// bundle for private-CA deployments and self-signed test rigs.
/// Failure is fatal at startup, same contract as the server side.
fn build_tls_client_config(extra_ca: Option<&std::path::Path>) -> Result<Arc<TlsClientConfig>> {
    let mut roots = tokio_rustls::rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    if let Some(path) = extra_ca {
        let pem = std::fs::read(path)
            .with_context(|| format!("read [sip.tls_client].extra_ca {}", path.display()))?;
        let mut added = 0usize;
        for cert in rustls_pemfile::certs(&mut pem.as_slice()) {
            let cert = cert.with_context(|| format!("parse certificate in {}", path.display()))?;
            roots
                .add(cert)
                .with_context(|| format!("add CA root from {}", path.display()))?;
            added += 1;
        }
        if added == 0 {
            return Err(anyhow!(
                "[sip.tls_client].extra_ca {} contains no certificates",
                path.display()
            ));
        }
        info!(path = %path.display(), added, "client TLS extra CA roots loaded");
    }
    Ok(Arc::new(
        tokio_rustls::rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    ))
}

/// `[sip.tls]`. Failure here is fatal — operators who set
/// `transports = ["tls"]` expect SIPS to actually work, not to
/// silently degrade to cleartext.
pub(crate) fn load_sip_tls_server_config(
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

/// Install the daemon's SIGHUP handler. Always installed —
/// claiming the signal prevents the default Unix disposition
/// (process termination) from firing when an operator runs
/// `systemctl reload siphon-ai` against a deployment that doesn't
/// enable TLS.
///
/// `tls` + `swappable` are `Some` together (both required for hot
/// reload to do anything) or `None` together (handler is a no-op
/// signal consumer). Mixed states fall back to no-op with a `warn!`
/// — they shouldn't happen, but the daemon doesn't crash if they do.
fn spawn_sighup_handler(
    tls: Option<SipTlsConfig>,
    swappable: Option<Arc<arc_swap::ArcSwap<tokio_rustls::rustls::ServerConfig>>>,
) {
    match (tls, swappable) {
        (Some(tls), Some(swap)) => spawn_sighup_reloader(tls, swap),
        (None, None) => spawn_sighup_noop(),
        _ => {
            warn!(
                "inconsistent SIGHUP wiring (one of tls/swappable is set, the other isn't); \
                 falling back to no-op handler"
            );
            spawn_sighup_noop();
        }
    }
}

/// No-op SIGHUP consumer for deployments without TLS. Just claims
/// the signal so the default "terminate" disposition doesn't fire.
fn spawn_sighup_noop() {
    use tokio::signal::unix::{signal, SignalKind};
    tokio::spawn(async move {
        let mut stream = match signal(SignalKind::hangup()) {
            Ok(s) => s,
            Err(e) => {
                warn!(
                    error = %e,
                    "failed to install no-op SIGHUP handler; daemon may terminate on \
                     `systemctl reload` until TLS is configured",
                );
                return;
            }
        };
        info!("SIGHUP handler installed (no TLS configured; signal is a no-op)");
        while stream.recv().await.is_some() {
            info!("SIGHUP received but no TLS configured; ignoring");
        }
    });
}

/// Install a SIGHUP handler that hot-reloads the SIP/TLS cert.
///
/// On every `SIGHUP`, re-reads `[sip.tls].cert` + `.key` from disk,
/// builds a fresh `rustls::ServerConfig`, and stores it into
/// `swappable`. The next inbound TLS connection picks up the new
/// cert; in-flight sessions keep using the cert they handshook with
/// (RFC 5746-compliant rotation — see siphon-rs#49 for the upstream
/// pattern).
///
/// **Failure mode.** A broken PEM file on reload doesn't kill the
/// daemon — we log `error!` and keep the old cert in place. Same
/// shape as nginx's `nginx -s reload` semantics: if the new config
/// is bad, the running config keeps serving.
///
/// **Concurrency.** One background tokio task. Lives for the
/// daemon's lifetime (we never deregister the signal handler). The
/// task is detached — its `JoinHandle` isn't kept anywhere because
/// there's nothing to do with it.
///
/// `tls` is cloned so the task can survive the rest of `RuntimeBuilder`
/// going out of scope; cert/key paths are owned strings in
/// `SipTlsConfig` so this is a cheap clone.
fn spawn_sighup_reloader(
    tls: SipTlsConfig,
    swappable: Arc<arc_swap::ArcSwap<tokio_rustls::rustls::ServerConfig>>,
) {
    use tokio::signal::unix::{signal, SignalKind};

    tokio::spawn(async move {
        let mut stream = match signal(SignalKind::hangup()) {
            Ok(s) => s,
            Err(e) => {
                // Without SIGHUP we lose hot reload but the daemon
                // is still usable — log loud and exit the task.
                error!(
                    error = %e,
                    "failed to install SIGHUP handler; SIP/TLS cert hot-reload disabled",
                );
                return;
            }
        };
        info!(
            cert = %tls.cert_path.display(),
            "SIP/TLS cert hot-reload installed; send SIGHUP to rotate"
        );
        while stream.recv().await.is_some() {
            match load_sip_tls_server_config(&tls) {
                Ok(new_cfg) => {
                    swappable.store(new_cfg);
                    metrics::counter!(
                        "siphon_ai_sip_tls_reload_attempts_total",
                        "outcome" => "ok",
                    )
                    .increment(1);
                    info!(
                        cert = %tls.cert_path.display(),
                        "SIP/TLS cert reloaded on SIGHUP"
                    );
                }
                Err(e) => {
                    metrics::counter!(
                        "siphon_ai_sip_tls_reload_attempts_total",
                        "outcome" => "failed",
                    )
                    .increment(1);
                    error!(
                        cert = %tls.cert_path.display(),
                        error = %e,
                        "SIGHUP cert reload failed; keeping previous cert"
                    );
                }
            }
        }
        // `recv()` returns `None` only on signal-handler teardown,
        // which we don't trigger. If it ever does, log so we know.
        warn!("SIGHUP signal stream ended; cert hot-reload offline");
    });
}

/// Build the lifecycle webhook sink from `[webhooks]` config.
/// Returns `NullSink` when disabled. When enabled, wraps the
/// `HttpSink` in a `FilteredSink` if an `events` allowlist is set.
pub(crate) fn build_webhook_sink(cfg: WebhooksConfig) -> Result<WebhookSinkHandle> {
    if !cfg.enabled {
        return Ok(Arc::new(WebhookNullSink));
    }
    let url = cfg
        .url
        .ok_or_else(|| anyhow!("[webhooks].url unexpectedly empty"))?;
    let signed = cfg.secret.is_some();
    let spooled = cfg.spool_dir.is_some();
    let http = WebhookHttpSink::new(WebhookHttpSinkConfig {
        url: url.clone(),
        auth_header: cfg.auth_header,
        secret: cfg.secret,
        spool_dir: cfg.spool_dir,
        retry_max: cfg.retry_max,
        timeout_ms: cfg.timeout.as_millis() as u64,
    })
    .map_err(|e| anyhow!("lifecycle webhook client build failed: {e}"))?;
    info!(url = %url, allowlist = cfg.events.len(), signed, spooled, "lifecycle webhook sink active");
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

pub(crate) async fn build_cdr_sink(
    cdr: CdrConfig,
    hep: Option<&HepTelemetry>,
) -> Result<CdrSinkHandle> {
    let mut sinks: Vec<CdrSinkHandle> = Vec::new();

    if cdr.enabled {
        if let Some(file_cfg) = cdr.file {
            sinks.push(build_file_sink(&file_cfg).await?);
        }
        if let Some(webhook_cfg) = cdr.webhook {
            sinks.push(build_cdr_webhook_sink(&webhook_cfg)?);
        }
    }

    // HEP CDR shipping is independent of `[cdr].enabled` — operators
    // can ship CDRs to Homer without also writing them to disk or a
    // webhook. Wires up when HEP is installed.
    if let Some(hep) = hep {
        let mut sink = HepCdrSink::new(hep.sink(), hep.capture_id());
        if let Some(pw) = hep.capture_password() {
            sink = sink.with_password(pw);
        }
        sinks.push(Arc::new(sink) as CdrSinkHandle);
        info!("CDR HEP sink active (chunk type 101)");
    }

    Ok(match sinks.len() {
        // No CDR shipping anywhere — silently drop. We only warn
        // when `[cdr].enabled = true` was set but no sub-sinks
        // landed, since that's the misconfig the operator cares about.
        0 => {
            if cdr.enabled {
                warn!(
                    "[cdr].enabled = true but no sub-sinks (file / webhook / hep) are active; CDRs will be dropped"
                );
            }
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
        secret: cfg.secret.clone(),
        spool_dir: cfg.spool_dir.clone(),
        retry_max: cfg.retry_max,
        timeout_ms: cfg.timeout.as_millis() as u64,
    })
    .map_err(|e| anyhow!("CDR webhook client build failed: {e}"))?;
    info!(url = %cfg.url, signed = cfg.secret.is_some(), spooled = cfg.spool_dir.is_some(), "CDR webhook sink active");
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

/// Long-lived dependencies needed to build the outbound gateway table.
/// Bundled so the initial build and a later SIGHUP gateway reload share
/// one construction path (all `Arc`s / owned strings — cheap to hold for
/// the daemon's lifetime). The DTLS cert is generated once and reused
/// across reloads.
pub struct GatewayBuildDeps {
    pub outbound_media: Arc<MediaSetup>,
    pub outbound_dtls_cert: Arc<forge_rtp::dtls::DtlsCertificate>,
    pub local_uri: String,
    pub contact_uri: String,
    pub listen_addr: SocketAddr,
    pub public_addr: Option<SocketAddr>,
    pub transaction_mgr: Arc<TransactionManager>,
    pub dispatcher: Arc<dyn TransportDispatcher>,
    pub sip_resolver: Arc<sip_dns::SipResolver>,
}

/// Build the `name → OutboundGateway` table from `[outbound]` config.
/// One authenticated UAC + delayed-offer registry + originator per
/// `[[gateway]]`. Called once at startup and again on a SIGHUP gateway
/// reload — each call mints fresh UACs (stateless senders over the shared
/// transaction manager), so a reload can fully replace the set.
pub(crate) fn build_gateways(
    outbound: &siphon_ai_config::OutboundConfig,
    deps: &GatewayBuildDeps,
) -> Result<HashMap<String, OutboundGateway>> {
    let mut gateways = HashMap::with_capacity(outbound.gateways.len());
    for gw in &outbound.gateways {
        // Outbound delayed offer: the gateway's UAC and its originator
        // share one registry — `place_delayed` parks per-call media
        // params, the UAC's answer generator picks them up on the 2xx.
        let delayed_registry: siphon_ai_core::DelayedOfferRegistry =
            Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
        let answerer: Arc<dyn sip_uac::integrated::SdpAnswerGenerator> =
            Arc::new(siphon_ai_core::DelayedOfferAnswerer::new(
                (*deps.outbound_media).clone(),
                Arc::clone(&delayed_registry),
                Arc::clone(&deps.outbound_dtls_cert),
            ));
        let uac = build_outbound_uac(
            TransferUacBuild {
                local_uri: &deps.local_uri,
                contact_uri: &deps.contact_uri,
                listen_addr: deps.listen_addr,
                public_addr: deps.public_addr,
                transaction_mgr: Arc::clone(&deps.transaction_mgr),
                dispatcher: Arc::clone(&deps.dispatcher),
                sip_resolver: Arc::clone(&deps.sip_resolver),
            },
            gw.credentials.as_ref(),
            Some(answerer),
        )?;
        let originator = Arc::new(OutboundOriginator::with_delayed_registry(
            (*deps.outbound_media).clone(),
            Arc::new(uac),
            delayed_registry,
        ));
        gateways.insert(
            gw.name.clone(),
            OutboundGateway {
                originator,
                proxy_host: gw.proxy_host.clone(),
                proxy_port: gw.proxy_port,
                transport_uri_param: gw.transport.uri_param(),
                from: gw.from.clone(),
                // Map the config SRTP policy onto the media-glue offer
                // mode (core's SrtpMode is the inbound enum; OutboundSrtp
                // is its outbound mirror).
                srtp: match gw.srtp {
                    siphon_ai_core::SrtpMode::Off => siphon_ai_media_glue::OutboundSrtp::Off,
                    siphon_ai_core::SrtpMode::Preferred => {
                        siphon_ai_media_glue::OutboundSrtp::Preferred
                    }
                    siphon_ai_core::SrtpMode::Required => {
                        siphon_ai_media_glue::OutboundSrtp::Required
                    }
                },
            },
        );
    }
    Ok(gateways)
}

/// Build a per-gateway outbound UAC. Same shape as [`build_transfer_uac`],
/// but — unlike REFER, which inherits the dialog's auth — an originated
/// INVITE may be challenged (401/407) by the trunk, so when the gateway has
/// credentials we install a [`StaticCredentials`] provider for the UAC's
/// auto-retry. One UAC per gateway keeps each trunk's credentials isolated.
fn build_outbound_uac(
    args: TransferUacBuild<'_>,
    credentials: Option<&siphon_ai_config::GatewayCredentials>,
    sdp_answer_generator: Option<Arc<dyn sip_uac::integrated::SdpAnswerGenerator>>,
) -> Result<IntegratedUAC> {
    let mut builder = IntegratedUAC::builder()
        .local_uri(args.local_uri)
        .contact_uri(args.contact_uri)
        .transaction_manager(args.transaction_mgr)
        .dispatcher(args.dispatcher)
        .resolver(args.sip_resolver)
        .local_addr(args.listen_addr.to_string())
        .map_err(|e| anyhow!("outbound UAC local_addr: {e}"))?;
    if let Some(public) = args.public_addr {
        builder = builder
            .public_addr(public.to_string())
            .map_err(|e| anyhow!("outbound UAC public_addr: {e}"))?;
    }
    if let Some(creds) = credentials {
        builder = builder.credential_provider(Arc::new(StaticCredentials::new(
            creds.username.clone(),
            creds.password.clone(),
        )));
    }
    // Outbound delayed offer (RFC 3264): when set, the UAC invokes this to
    // build the SDP answer for the ACK on a 2xx carrying the peer's offer.
    if let Some(generator) = sdp_answer_generator {
        builder = builder.sdp_answer_generator(generator);
    }
    builder
        .build()
        .map_err(|e| anyhow!("outbound UAC build: {e}"))
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

#[allow(clippy::too_many_arguments)]
async fn spawn_listeners(
    sip: &SipConfig,
    udp_socket: Arc<UdpSocket>,
    udp_bound_addr: SocketAddr,
    transaction_mgr: Arc<TransactionManager>,
    uas: Arc<IntegratedUAS>,
    tls_server_config: Option<Arc<arc_swap::ArcSwap<tokio_rustls::rustls::ServerConfig>>>,
    tcp_client_pool: &ConnectionPool,
    tls_client_pool: &TlsPool,
) -> Vec<JoinHandle<()>> {
    let mut handles = Vec::new();
    let (packet_tx, packet_rx) = mpsc::channel::<InboundPacket>(1024);

    // Route responses arriving on OUTBOUND client connections (the
    // dispatcher's TCP/TLS pools) into the same packet pipeline the
    // listeners feed. Without this the pool opens a connection, sends
    // the request, and silently drops everything the peer sends back.
    tcp_client_pool.set_inbound_tx(packet_tx.clone()).await;
    tls_client_pool.set_inbound_tx(packet_tx.clone()).await;

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
    // Uses the swappable variant so a SIGHUP cert reload (W5) can
    // rotate the cert mid-flight without dropping in-flight TLS
    // sessions (siphon-rs#49).
    if want_tls {
        match (tls_server_config, sip.tls.as_ref()) {
            (Some(swappable), Some(tls)) => {
                let bind = tls.listen_addr.to_string();
                let tx = packet_tx.clone();
                handles.push(tokio::spawn(async move {
                    if let Err(e) = run_tls_with_swappable_config(&bind, swappable, tx).await {
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
    // For TCP/TLS the listener hands us the per-connection writer
    // channel via `stream`; threading it into `TransportContext` is
    // what lets the transaction manager send responses back over
    // the same inbound socket instead of opening a fresh outbound
    // connection (or, for TLS, failing outright because the
    // dispatcher has no way to originate one).
    // Capture the receiving listener's local address before
    // `into_parts` consumes the packet — it lets the UAS build a
    // Contact on the same port the request arrived on (so a TLS
    // request on :5061 advertises `:5061;transport=tls`, not the
    // UDP listener's port).
    let local = packet.local();
    let (transport, peer, payload, stream) = packet.into_parts();

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
    let ctx = TransportContext::new(tx_kind, peer, stream).with_local_addr(local);

    if request.method().as_str() == "ACK" {
        // ACK doesn't open a server transaction; notify the matching
        // invite TM key so the UAS can clear its 200 OK retransmission
        // timer (RFC 3261 §17.2.1, §17.1.1.3 — the ACK transaction key
        // uses the original INVITE's method).
        if let Some(branch) = sip_transaction::request_branch_id(&request) {
            let key = sip_transaction::TransactionKey::new(branch, sip_core::Method::Invite, true);
            transaction_mgr.ack_received(&key).await;
        }
        // An early-offer ACK carries no SDP and needs no application
        // handling — clearing the timer above is all there is to do. A
        // **delayed-offer** ACK carries the peer's SDP answer, which the
        // UAS must see (`on_ack` → finalize the call); fall through to
        // dispatch for those. (`receive_request` makes a benign
        // non-INVITE entry for it — the FSM sends no response to an ACK.)
        if request.body().is_empty() {
            return;
        }
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
// (TCP/TLS), `run_tcp` / `run_tls` hand us the per-connection
// writer channel on the `InboundPacket`; `handle_packet` threads
// it into `TransportContext`, and `send_stream` pushes the
// response payload through that channel back to the peer over the
// established socket.
//
// Outbound TCP/TLS connect (no `stream` in ctx) goes through the
// client pools: `ConnectionPool` / `TlsPool` open (or reuse) a
// connection to the peer, and their reader tasks feed responses
// back into the inbound packet pipeline (`set_inbound_tx` in
// `spawn_listeners`). This is what `[[gateway]]` / `[[register]]`
// blocks with `transport = "tcp"|"tls"` ride.

struct MultiTransportDispatcher {
    udp_socket: Arc<UdpSocket>,
    tcp_client_pool: Arc<ConnectionPool>,
    tls_client_pool: Arc<TlsPool>,
    tls_client_config: Arc<TlsClientConfig>,
}

impl MultiTransportDispatcher {
    fn new(
        udp_socket: Arc<UdpSocket>,
        tcp_client_pool: Arc<ConnectionPool>,
        tls_client_pool: Arc<TlsPool>,
        tls_client_config: Arc<TlsClientConfig>,
    ) -> Self {
        Self {
            udp_socket,
            tcp_client_pool,
            tls_client_pool,
            tls_client_config,
        }
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
                // Established connection (inbound dialog, or a pooled
                // client connection threaded back through a flow) —
                // reply over it.
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
                // No connection yet — originate one via the client
                // pools (gateway INVITE, REGISTER to a TLS registrar).
                None => match ctx.transport() {
                    TxTransportKind::Tcp => self
                        .tcp_client_pool
                        .send_tcp(ctx.peer(), payload)
                        .await
                        .with_context(|| format!("outbound TCP connect to {}", ctx.peer())),
                    TxTransportKind::Tls => {
                        // SNI: the UAC threads the DNS target's host
                        // into the ctx; the IP fallback only fires for
                        // literal-IP targets, where certificate
                        // verification needs an IP SAN anyway.
                        let server_name = ctx
                            .server_name()
                            .map(String::from)
                            .unwrap_or_else(|| ctx.peer().ip().to_string());
                        self.tls_client_pool
                            .send_tls(
                                ctx.peer(),
                                server_name.clone(),
                                Arc::clone(&self.tls_client_config),
                                payload,
                            )
                            .await
                            .with_context(|| {
                                format!(
                                    "outbound TLS connect to {} (sni={server_name})",
                                    ctx.peer()
                                )
                            })
                    }
                    _ => unreachable!(),
                },
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

#[cfg(test)]
mod trunk_allowlist_tests {
    use super::*;
    use bytes::Bytes;
    use sip_core::{Headers as SipHeaders, Method, Request, RequestLine, SipUri};
    use siphon_ai_config::{TrunkCidr, TrunkConfig};

    fn invite_with_from(from_header: &str) -> Request {
        let uri = SipUri::parse("sip:9000@siphon.example.com").unwrap();
        let mut h = SipHeaders::new();
        h.push("Via", "SIP/2.0/UDP test;branch=z9hG4bK1").unwrap();
        h.push("From", from_header).unwrap();
        h.push("To", "<sip:9000@siphon.example.com>").unwrap();
        h.push("Call-ID", "trunk-test").unwrap();
        h.push("CSeq", "1 INVITE").unwrap();
        Request::new(RequestLine::new(Method::Invite, uri), h, Bytes::new()).unwrap()
    }

    #[test]
    fn extract_from_host_handles_bracketed_uri() {
        let req = invite_with_from("\"Alice\" <sip:alice@10.0.0.10:5060>;tag=abc");
        assert_eq!(
            ConfigTrunkAllowlist::extract_from_host(&req).as_deref(),
            Some("10.0.0.10"),
        );
    }

    #[test]
    fn extract_from_host_handles_bare_uri() {
        let req = invite_with_from("sip:carrier@sip.carrier.example;tag=xyz");
        assert_eq!(
            ConfigTrunkAllowlist::extract_from_host(&req).as_deref(),
            Some("sip.carrier.example"),
        );
    }

    #[test]
    fn extract_from_host_lowercases() {
        let req = invite_with_from("<sip:bob@SIP.CARRIER.EXAMPLE>;tag=t");
        assert_eq!(
            ConfigTrunkAllowlist::extract_from_host(&req).as_deref(),
            Some("sip.carrier.example"),
        );
    }

    fn allowlist(trunks: Vec<TrunkConfig>) -> ConfigTrunkAllowlist {
        ConfigTrunkAllowlist::new(trunks)
    }

    fn ctx(peer: &str) -> sip_transaction::TransportContext {
        sip_transaction::TransportContext::new(
            sip_transaction::TransportKind::Udp,
            peer.parse().unwrap(),
            None,
        )
    }

    #[test]
    fn ip_only_trunk_matches_in_range() {
        let trunks = vec![TrunkConfig {
            name: "fs".into(),
            peer_addrs: vec![TrunkCidr::parse("10.0.0.0/24").unwrap()],
            from_hosts: vec![],
        }];
        let gate = allowlist(trunks);
        let req = invite_with_from("<sip:caller@somewhere>;tag=t");
        assert_eq!(
            siphon_ai_sip_glue::TrunkAllowlist::identify(&gate, &req, &ctx("10.0.0.5:5060")),
            Some("fs".to_string()),
        );
        assert_eq!(
            siphon_ai_sip_glue::TrunkAllowlist::identify(&gate, &req, &ctx("10.0.1.5:5060")),
            None,
        );
    }

    #[test]
    fn from_host_only_trunk_matches_regardless_of_ip() {
        let trunks = vec![TrunkConfig {
            name: "carrier".into(),
            peer_addrs: vec![],
            from_hosts: vec!["sip.carrier.example".into()],
        }];
        let gate = allowlist(trunks);
        let req = invite_with_from("<sip:in@sip.carrier.example>;tag=t");
        assert_eq!(
            siphon_ai_sip_glue::TrunkAllowlist::identify(&gate, &req, &ctx("203.0.113.99:5060")),
            Some("carrier".to_string()),
        );
        // Wrong From host → no match even if IP would have been ok
        // (which it isn't here since we didn't set peer_addrs).
        let bad_req = invite_with_from("<sip:in@evil.example>;tag=t");
        assert_eq!(
            siphon_ai_sip_glue::TrunkAllowlist::identify(
                &gate,
                &bad_req,
                &ctx("203.0.113.99:5060")
            ),
            None,
        );
    }

    #[test]
    fn ip_and_from_host_both_required_when_both_set() {
        let trunks = vec![TrunkConfig {
            name: "strict".into(),
            peer_addrs: vec![TrunkCidr::parse("10.0.0.10").unwrap()],
            from_hosts: vec!["sip.carrier.example".into()],
        }];
        let gate = allowlist(trunks);
        let good_req = invite_with_from("<sip:in@sip.carrier.example>;tag=t");
        let bad_host_req = invite_with_from("<sip:in@evil.example>;tag=t");
        assert_eq!(
            siphon_ai_sip_glue::TrunkAllowlist::identify(&gate, &good_req, &ctx("10.0.0.10:5060")),
            Some("strict".to_string()),
        );
        // Right IP, wrong From → 403.
        assert_eq!(
            siphon_ai_sip_glue::TrunkAllowlist::identify(
                &gate,
                &bad_host_req,
                &ctx("10.0.0.10:5060"),
            ),
            None,
        );
        // Right From, wrong IP → 403.
        assert_eq!(
            siphon_ai_sip_glue::TrunkAllowlist::identify(&gate, &good_req, &ctx("10.0.0.11:5060")),
            None,
        );
    }

    #[test]
    fn first_matching_trunk_wins() {
        let trunks = vec![
            TrunkConfig {
                name: "first".into(),
                peer_addrs: vec![TrunkCidr::parse("10.0.0.0/16").unwrap()],
                from_hosts: vec![],
            },
            TrunkConfig {
                name: "second".into(),
                peer_addrs: vec![TrunkCidr::parse("10.0.0.0/8").unwrap()],
                from_hosts: vec![],
            },
        ];
        let gate = allowlist(trunks);
        let req = invite_with_from("<sip:c@x>;tag=t");
        assert_eq!(
            siphon_ai_sip_glue::TrunkAllowlist::identify(&gate, &req, &ctx("10.0.5.1:5060")),
            Some("first".to_string()),
        );
    }
}

#[cfg(test)]
mod tls_reload_tests {
    //! SIP/TLS hot-reload (W5).
    //!
    //! The siphon-rs side (PR #49) already proves the
    //! `ArcSwap<ServerConfig>` mechanism with `tls_swap.rs`. What
    //! this layer needs to verify is the load-from-disk + swap
    //! glue: that `load_sip_tls_server_config` produces a
    //! `ServerConfig` we can `arc_swap.store(...)` into, and that
    //! a subsequent load returns a *different* Arc address (so the
    //! swap actually changed the held value).
    //!
    //! The SIGHUP signal-to-store path itself is a 5-line
    //! `signal.recv() → load → store` loop in `spawn_sighup_reloader`
    //! that's awkward to integration-test in-process (sending
    //! SIGHUP to self interacts badly with tokio's runtime + test
    //! harness). We rely on code review for that wire-up.
    use super::*;
    use siphon_ai_config::SipTlsConfig;
    use std::net::SocketAddr;

    /// Self-signed cert + matching key pair, generated once by the
    /// fixtures crate at build time. Same DER blob the bridge
    /// tls.rs test uses; we ship both PEM forms here.
    /// Install rustls's process-wide crypto provider exactly once.
    /// `main()` does this in the daemon path; tests don't run `main`,
    /// so any test that touches a rustls `ServerConfig` has to do it
    /// itself (or rustls panics with "Could not automatically
    /// determine the process-level CryptoProvider").
    fn install_crypto_provider() {
        use std::sync::Once;
        static ONCE: Once = Once::new();
        ONCE.call_once(|| {
            let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        });
    }

    fn write_cert_a(dir: &std::path::Path) -> (std::path::PathBuf, std::path::PathBuf) {
        install_crypto_provider();
        let cert = dir.join("a.pem");
        let key = dir.join("a.key");
        std::fs::write(&cert, FIXTURE_CERT_A).unwrap();
        std::fs::write(&key, FIXTURE_KEY_A).unwrap();
        set_key_perms(&key);
        (cert, key)
    }

    fn write_cert_b(dir: &std::path::Path) -> (std::path::PathBuf, std::path::PathBuf) {
        let cert = dir.join("b.pem");
        let key = dir.join("b.key");
        std::fs::write(&cert, FIXTURE_CERT_B).unwrap();
        std::fs::write(&key, FIXTURE_KEY_B).unwrap();
        set_key_perms(&key);
        (cert, key)
    }

    /// siphon-rs's `load_rustls_server_config` refuses to load a key
    /// file with group/world-readable perms (security check). Mirror
    /// the umask siphon-ai expects in production.
    fn set_key_perms(path: &std::path::Path) {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(path).unwrap().permissions();
        perms.set_mode(0o600);
        std::fs::set_permissions(path, perms).unwrap();
    }

    fn fixture_tls_config(cert: std::path::PathBuf, key: std::path::PathBuf) -> SipTlsConfig {
        SipTlsConfig {
            listen_addr: "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
            cert_path: cert,
            key_path: key,
        }
    }

    #[test]
    fn load_sip_tls_server_config_returns_usable_config() {
        let tmp = tempdir_for_test();
        let (cert, key) = write_cert_a(tmp.path());
        let tls = fixture_tls_config(cert, key);
        let cfg = load_sip_tls_server_config(&tls).expect("load cert A");
        // Sanity: the config is shared as an Arc, and there's at
        // least one cert resolver behind it. The exact resolver
        // surface differs across rustls versions — what we can
        // check portably is that the Arc clone-counts make sense
        // (one strong ref from `load_sip_tls_server_config`'s
        // return value).
        assert_eq!(Arc::strong_count(&cfg), 1);
    }

    #[test]
    fn swap_picks_up_new_cert() {
        let tmp = tempdir_for_test();
        let (cert_a, key_a) = write_cert_a(tmp.path());
        let (cert_b, key_b) = write_cert_b(tmp.path());

        let tls_a = fixture_tls_config(cert_a, key_a);
        let initial = load_sip_tls_server_config(&tls_a).expect("load cert A");
        let swap = Arc::new(arc_swap::ArcSwap::from(initial));

        // Snapshot pre-swap. `load_full` returns an `Arc` clone of
        // the current value — two pre-swap snapshots share the
        // same identity.
        let before = swap.load_full();
        let before2 = swap.load_full();
        assert!(Arc::ptr_eq(&before, &before2));

        // Reload from cert B and store. Same code path the SIGHUP
        // handler runs: `load_sip_tls_server_config` + `store`.
        let tls_b = fixture_tls_config(cert_b, key_b);
        let new = load_sip_tls_server_config(&tls_b).expect("load cert B");
        swap.store(new);

        // Post-swap: the held Arc is now a different identity.
        let after = swap.load_full();
        assert!(!Arc::ptr_eq(&before, &after));
    }

    #[test]
    fn swap_keeps_old_cert_when_new_load_fails() {
        let tmp = tempdir_for_test();
        let (cert_a, key_a) = write_cert_a(tmp.path());

        let tls_a = fixture_tls_config(cert_a, key_a);
        let initial = load_sip_tls_server_config(&tls_a).expect("load cert A");
        let swap = Arc::new(arc_swap::ArcSwap::from(initial));
        let before = swap.load_full();

        // Point at a non-existent cert. The SIGHUP handler's error
        // arm is what we're modelling here: `load_*` returns `Err`,
        // we do NOT call `store`, the swap keeps the old cert.
        let bogus = fixture_tls_config(
            tmp.path().join("nonexistent.pem"),
            tmp.path().join("nonexistent.key"),
        );
        let result = load_sip_tls_server_config(&bogus);
        assert!(result.is_err());

        // Swap state untouched.
        let after = swap.load_full();
        assert!(Arc::ptr_eq(&before, &after));
    }

    /// Minimal `tempdir` without pulling in the `tempfile` crate just
    /// for these tests. Cleanup happens on drop.
    struct TempDir {
        path: std::path::PathBuf,
    }
    impl TempDir {
        fn path(&self) -> &std::path::Path {
            &self.path
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
    fn tempdir_for_test() -> TempDir {
        let base = std::env::temp_dir();
        let pid = std::process::id();
        // Counter to keep test invocations distinct when several run
        // concurrently in the same process.
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let path = base.join(format!("siphon-ai-tls-reload-test-{pid}-{seq}"));
        std::fs::create_dir_all(&path).unwrap();
        TempDir { path }
    }

    // Self-signed RSA-2048 cert A. Generated with:
    //   openssl req -x509 -newkey rsa:2048 -nodes -keyout key -out cert \
    //     -days 36500 -subj "/CN=siphon-ai-reload-test-A"
    const FIXTURE_CERT_A: &[u8] = include_bytes!("fixtures/reload_cert_a.pem");
    const FIXTURE_KEY_A: &[u8] = include_bytes!("fixtures/reload_key_a.pem");
    const FIXTURE_CERT_B: &[u8] = include_bytes!("fixtures/reload_cert_b.pem");
    const FIXTURE_KEY_B: &[u8] = include_bytes!("fixtures/reload_key_b.pem");
}
