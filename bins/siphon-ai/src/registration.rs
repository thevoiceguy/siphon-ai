//! Per-`[[register]]` UAC drive task.
//!
//! Each `[[register]]` block (CLAUDE.md §7.2 "registered phone mode")
//! gets one async task that:
//!
//! 1. Builds an [`IntegratedUAC`] for that block's identity + creds.
//! 2. Sends a REGISTER, observes the final response (the upstream UAC
//!    handles 401/407 retry internally).
//! 3. Updates the shared [`RegistrationManager`] state.
//! 4. On success, sleeps until `expires - REFRESH_MARGIN`, then loops.
//! 5. On failure, sleeps an exponential backoff (clamped) and retries.
//! 6. Observes [`RegistrationManager::shutdown_signal`] in every wait
//!    so `Runtime::run`'s shutdown teardown is responsive.
//!
//! Side effects on every status transition:
//! - The Prometheus gauge `siphon_ai_register_state{name,state}` flips
//!   1 on the new row and 0 on every other row of the same `name`.
//! - The counter `siphon_ai_register_attempts_total{name,outcome}`
//!   ticks once.
//! - The lifecycle webhook (if configured + subscribed) gets a
//!   `registration_state_changed` event.
//!
//! Failure-mode taxonomy on the `outcome` label:
//! - `registered`        — 2xx
//! - `auth_failed`       — 401 / 403 / 407 after UAC's retry window
//! - `rejected`          — any other 4xx / 5xx / 6xx
//! - `transport_error`   — `IntegratedUAC::register` returned `Err`
//!
//! v1 keeps DNS resolution dumb: registrar address is the resolved
//! `host:port` from config, no SRV/NAPTR. Per CLAUDE.md "things that
//! are out of scope" — full DNS-driven failover is post-v1.

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use sip_core::Response;
use sip_dns::SipResolver;
use sip_transaction::{TransactionManager, TransportDispatcher};
use sip_uac::integrated::{IntegratedUAC, RequestTarget};
use siphon_ai_config::{RegisterConfig, SipTransport};
use siphon_ai_sip_glue::{
    refresh_delay, RegistrationCommand, RegistrationManager, RegistrationStatus, ShutdownSignal,
};
use siphon_ai_telemetry::metrics::{REGISTER_ATTEMPTS_TOTAL, REGISTER_STATE};
use siphon_ai_webhooks::{
    RegistrationStateChangedEvent, WebhookEvent, WebhookSinkHandle, WEBHOOK_VERSION,
};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{debug, info, instrument, warn};

/// Initial backoff after the first registration failure.
const BACKOFF_INITIAL: Duration = Duration::from_secs(5);

/// Cap on the exponential backoff. After ~6 failures we sit at this
/// ceiling and keep retrying forever (until shutdown).
const BACKOFF_MAX: Duration = Duration::from_secs(300);

/// All possible outcome labels — kept here so the `# HELP` text in
/// the metrics module and the dashboard queries stay in sync.
mod outcome {
    pub const REGISTERED: &str = "registered";
    pub const AUTH_FAILED: &str = "auth_failed";
    pub const REJECTED: &str = "rejected";
    pub const TRANSPORT_ERROR: &str = "transport_error";
}

/// Spawn one driver task per `[[register]]` block.
///
/// Blocks with `register_on_startup = false` run the same drive task
/// **parked**: no REGISTER until the first operator command arrives
/// (0.33.0, `POST /admin/v1/registrations/{name}/refresh`).
///
/// All tasks share the same shutdown signal, so a single
/// `manager.shutdown()` call from the runtime's teardown wakes all of
/// them at once.
pub fn spawn_registration_tasks(
    manager: &RegistrationManager,
    configs: &[RegisterConfig],
    transaction_mgr: Arc<TransactionManager>,
    dispatcher: Arc<dyn TransportDispatcher>,
    resolver: Arc<SipResolver>,
    advertised_addr: &str,
    webhook_sink: WebhookSinkHandle,
) -> Vec<JoinHandle<()>> {
    // Seed every `siphon_ai_register_state` row so a /metrics scrape
    // before the first attempt completes already shows the row at
    // its initial state (pending or disabled). Without this, the
    // gauge is just absent until the first transition.
    for cfg in configs {
        let initial = if cfg.register_on_startup {
            RegistrationStatus::Pending
        } else {
            RegistrationStatus::Disabled
        };
        publish_state(&cfg.name, None, initial);
    }

    let mut handles = Vec::with_capacity(configs.len());
    for cfg in configs {
        // Since 0.33.0 a `register_on_startup = false` block runs the
        // SAME drive task, parked awaiting its first admin command
        // (`POST /admin/v1/registrations/{name}/refresh` — the
        // "tell to register" RPC the Disabled status reserved) —
        // DESIGN_REGISTRATION_ADMIN.md §4.
        handles.push(spawn_one(
            manager.clone(),
            cfg.clone(),
            Arc::clone(&transaction_mgr),
            Arc::clone(&dispatcher),
            Arc::clone(&resolver),
            advertised_addr.to_string(),
            Arc::clone(&webhook_sink),
        ));
    }
    handles
}

fn spawn_one(
    manager: RegistrationManager,
    cfg: RegisterConfig,
    transaction_mgr: Arc<TransactionManager>,
    dispatcher: Arc<dyn TransportDispatcher>,
    resolver: Arc<SipResolver>,
    local_addr_str: String,
    webhook_sink: WebhookSinkHandle,
) -> JoinHandle<()> {
    let signal = manager.shutdown_signal();
    // Registered synchronously at spawn time so the admin endpoints
    // can reach every binding the moment the runtime is up.
    let commands = manager.register_command_channel(&cfg.name);
    tokio::spawn(async move {
        if let Err(e) = drive(
            &manager,
            &cfg,
            transaction_mgr,
            dispatcher,
            resolver,
            local_addr_str,
            webhook_sink,
            commands,
            signal,
        )
        .await
        {
            warn!(name = %cfg.name, error = %e, "registration drive task ended with error");
        }
    })
}

/// SIP side of one REGISTER attempt, abstracted so [`run_loop`]'s
/// select/backoff/restart mechanics are unit-testable without live
/// SIP (the tests script outcomes and record requested expires).
trait RegisterBackend {
    async fn register(&self, expires_secs: u32) -> RegisterOutcome;
}

/// The real backend: an [`IntegratedUAC`] against the configured
/// registrar.
struct UacBackend {
    uac: IntegratedUAC,
    registrar: RequestTarget,
}

impl RegisterBackend for UacBackend {
    async fn register(&self, expires_secs: u32) -> RegisterOutcome {
        perform_register(&self.uac, self.registrar.clone(), expires_secs).await
    }
}

/// Unrecoverable setup (UAC builder / registrar URI), then hand off
/// to the loop.
///
/// 9 args is over clippy's 7-arg threshold; each is independent
/// daemon-side plumbing (manager, cfg, two transaction/transport
/// arcs, resolver, local addr, webhook sink, commands, shutdown).
/// Bundling them into a context struct buys nothing — the call site
/// is one place and each arg is named at construction.
#[allow(clippy::too_many_arguments)]
#[instrument(skip_all, fields(name = %cfg.name, server = %cfg.server_addr))]
async fn drive(
    manager: &RegistrationManager,
    cfg: &RegisterConfig,
    transaction_mgr: Arc<TransactionManager>,
    dispatcher: Arc<dyn TransportDispatcher>,
    resolver: Arc<SipResolver>,
    local_addr_str: String,
    webhook_sink: WebhookSinkHandle,
    commands: mpsc::Receiver<RegistrationCommand>,
    signal: ShutdownSignal,
) -> anyhow::Result<()> {
    let backend = UacBackend {
        uac: build_uac(cfg, transaction_mgr, dispatcher, resolver, &local_addr_str)?,
        registrar: registrar_target(cfg)?,
    };
    run_loop(
        manager,
        &cfg.name,
        cfg.expires.as_secs() as u32,
        !cfg.register_on_startup,
        &backend,
        commands,
        webhook_sink,
        signal,
    )
    .await;
    Ok(())
}

/// The drive loop proper. Every wait — the parked state, the
/// registered-refresh sleep, and the failure backoff — selects over
/// **timer / operator command / shutdown** (0.33.0,
/// DESIGN_REGISTRATION_ADMIN.md §2):
///
/// - `Refresh` fires an immediate REGISTER; during a backoff it also
///   resets the backoff to initial (an operator kick is "retry now
///   with a clean slate").
/// - `Restart` does the same but the next attempt is preceded by a
///   REGISTER `Expires: 0` to clear the registrar-side binding. A
///   failed unregister is warned and the fresh REGISTER proceeds —
///   only the final REGISTER's outcome drives status/metrics/webhook.
/// - `start_parked` (`register_on_startup = false`): no REGISTER
///   until the first command arrives; either command starts the
///   normal cycle (there is no binding to clear yet, so `Restart`
///   from the parked state is identical to `Refresh`).
#[allow(clippy::too_many_arguments)]
async fn run_loop<B: RegisterBackend>(
    manager: &RegistrationManager,
    name: &str,
    expires_secs: u32,
    start_parked: bool,
    backend: &B,
    mut commands: mpsc::Receiver<RegistrationCommand>,
    webhook_sink: WebhookSinkHandle,
    signal: ShutdownSignal,
) {
    if start_parked {
        info!("[[register]] parked (register_on_startup = false); awaiting operator command");
        tokio::select! {
            cmd = commands.recv() => match cmd {
                Some(cmd) => {
                    info!(?cmd, "parked registration started by operator");
                    let prev = manager.get(name).map(|s| s.status);
                    manager.set_status(name, RegistrationStatus::Pending, None, None);
                    if prev != Some(RegistrationStatus::Pending) {
                        publish_state(name, prev, RegistrationStatus::Pending);
                        emit_webhook(
                            &webhook_sink,
                            name,
                            prev,
                            RegistrationStatus::Pending,
                            None,
                            None,
                        )
                        .await;
                    }
                }
                None => {
                    debug!("command channel closed while parked; exiting");
                    return;
                }
            },
            _ = signal.cancelled() => {
                info!("shutdown signal received while parked");
                return;
            }
        }
    }

    info!("registration drive started");

    let mut backoff = BACKOFF_INITIAL;
    // Set when an operator `Restart` is pending: the next attempt
    // clears the binding first.
    let mut unregister_first = false;

    loop {
        let mut prev_status = manager
            .get(name)
            .map(|s| s.status)
            .unwrap_or(RegistrationStatus::Pending);

        if unregister_first {
            unregister_first = false;
            debug!("restart: clearing binding with Expires: 0");
            if let RegisterOutcome::Failed {
                outcome_label,
                error_msg,
            } = backend.register(0).await
            {
                // Best-effort: the goal state is "registered", and the
                // follow-up REGISTER replaces the binding anyway.
                warn!(
                    outcome = outcome_label,
                    error = %error_msg,
                    "restart's unregister failed; proceeding to fresh REGISTER",
                );
            }
        }

        debug!(?prev_status, "sending REGISTER");
        let outcome = backend.register(expires_secs).await;

        match outcome {
            RegisterOutcome::Registered { granted_expires } => {
                let expires_at =
                    Utc::now() + chrono::Duration::seconds(granted_expires.as_secs() as i64);
                metrics::counter!(
                    REGISTER_ATTEMPTS_TOTAL,
                    "name" => name.to_string(),
                    "outcome" => outcome::REGISTERED,
                )
                .increment(1);
                manager.set_status(name, RegistrationStatus::Registered, None, Some(expires_at));
                if prev_status != RegistrationStatus::Registered {
                    publish_state(name, Some(prev_status), RegistrationStatus::Registered);
                    emit_webhook(
                        &webhook_sink,
                        name,
                        Some(prev_status),
                        RegistrationStatus::Registered,
                        None,
                        Some(expires_at),
                    )
                    .await;
                    prev_status = RegistrationStatus::Registered;
                }
                info!(
                    granted_expires_secs = granted_expires.as_secs(),
                    "registration succeeded"
                );
                backoff = BACKOFF_INITIAL;

                // Sleep until refresh time, an operator command, OR
                // shutdown.
                let delay = refresh_delay(granted_expires);
                debug!(delay_secs = delay.as_secs(), "sleeping until refresh");
                match wait_for(delay, &mut commands, &signal).await {
                    Wake::Elapsed => {}
                    Wake::Command(cmd) => {
                        info!(?cmd, "operator command; registering off-cycle");
                        if cmd == RegistrationCommand::Restart {
                            unregister_first = true;
                        }
                    }
                    Wake::Shutdown => {
                        info!("shutdown signal received while registered");
                        return;
                    }
                }
            }
            RegisterOutcome::Failed {
                outcome_label,
                error_msg,
            } => {
                metrics::counter!(
                    REGISTER_ATTEMPTS_TOTAL,
                    "name" => name.to_string(),
                    "outcome" => outcome_label,
                )
                .increment(1);
                manager.set_status(
                    name,
                    RegistrationStatus::Failed,
                    Some(error_msg.clone()),
                    None,
                );
                if prev_status != RegistrationStatus::Failed {
                    publish_state(name, Some(prev_status), RegistrationStatus::Failed);
                    emit_webhook(
                        &webhook_sink,
                        name,
                        Some(prev_status),
                        RegistrationStatus::Failed,
                        Some(error_msg.clone()),
                        None,
                    )
                    .await;
                }
                warn!(
                    outcome = outcome_label,
                    error = %error_msg,
                    backoff_secs = backoff.as_secs(),
                    "registration failed; will retry after backoff"
                );

                match wait_for(backoff, &mut commands, &signal).await {
                    Wake::Elapsed => backoff = (backoff * 2).min(BACKOFF_MAX),
                    Wake::Command(cmd) => {
                        // An operator kick outranks the exponential
                        // politeness timer: retry now, clean slate.
                        info!(?cmd, "operator command during backoff; retrying now");
                        backoff = BACKOFF_INITIAL;
                        if cmd == RegistrationCommand::Restart {
                            unregister_first = true;
                        }
                    }
                    Wake::Shutdown => {
                        info!("shutdown signal received during backoff");
                        return;
                    }
                }
            }
        }
    }
}

/// What woke a drive-loop wait.
enum Wake {
    Elapsed,
    Command(RegistrationCommand),
    Shutdown,
}

/// 3-arm wait: timer / operator command / shutdown. A closed command
/// channel (can't happen in production — the manager owns the sender
/// for the process lifetime) just disables that arm for the rest of
/// the wait rather than busy-looping on `None`.
async fn wait_for(
    delay: Duration,
    commands: &mut mpsc::Receiver<RegistrationCommand>,
    signal: &ShutdownSignal,
) -> Wake {
    let sleep = tokio::time::sleep(delay);
    tokio::pin!(sleep);
    let mut commands_open = true;
    loop {
        tokio::select! {
            _ = &mut sleep => return Wake::Elapsed,
            _ = signal.cancelled() => return Wake::Shutdown,
            cmd = commands.recv(), if commands_open => match cmd {
                Some(cmd) => return Wake::Command(cmd),
                None => commands_open = false,
            },
        }
    }
}

enum RegisterOutcome {
    Registered {
        granted_expires: Duration,
    },
    Failed {
        outcome_label: &'static str,
        error_msg: String,
    },
}

async fn perform_register(
    uac: &IntegratedUAC,
    registrar: RequestTarget,
    expires_secs: u32,
) -> RegisterOutcome {
    match uac.register(registrar, Some(expires_secs)).await {
        Ok(resp) => {
            let code = resp.code();
            if (200..300).contains(&code) {
                let granted =
                    response_expires(&resp).unwrap_or(Duration::from_secs(expires_secs as u64));
                RegisterOutcome::Registered {
                    granted_expires: granted,
                }
            } else if matches!(code, 401 | 403 | 407) {
                RegisterOutcome::Failed {
                    outcome_label: outcome::AUTH_FAILED,
                    error_msg: format!("{} {}", code, resp.reason()),
                }
            } else {
                RegisterOutcome::Failed {
                    outcome_label: outcome::REJECTED,
                    error_msg: format!("{} {}", code, resp.reason()),
                }
            }
        }
        Err(e) => RegisterOutcome::Failed {
            outcome_label: outcome::TRANSPORT_ERROR,
            error_msg: e.to_string(),
        },
    }
}

/// Pull the registrar's grant out of the 200 OK. Looks at the
/// top-level `Expires` header first; falls back to the `expires=`
/// param on the first `Contact`. RFC 3261 §10.2.4 says either is
/// authoritative; most registrars set the top-level header.
fn response_expires(resp: &Response) -> Option<Duration> {
    if let Some(raw) = resp.headers().get("Expires") {
        if let Ok(secs) = raw.trim().parse::<u32>() {
            return Some(Duration::from_secs(secs as u64));
        }
    }
    if let Some(contact) = resp.headers().get("Contact") {
        if let Some(idx) = contact.to_ascii_lowercase().find("expires=") {
            let after = &contact[idx + "expires=".len()..];
            let end = after
                .find(|c: char| c == ';' || c == ',' || c.is_whitespace())
                .unwrap_or(after.len());
            if let Ok(secs) = after[..end].parse::<u32>() {
                return Some(Duration::from_secs(secs as u64));
            }
        }
    }
    None
}

/// Build the `Contact` URI a registrar will route INVITEs back through.
/// `addr` is the daemon's *advertised* SIP address (`host:port`), never
/// the socket bind — see the caller. Kept as a pure helper so the
/// no-wildcard-leak invariant is unit-testable without a live UAC.
fn build_contact_uri(username: &str, addr: &str, transport: SipTransport) -> String {
    let transport_param = match transport {
        SipTransport::Udp => "udp",
        SipTransport::Tcp => "tcp",
        SipTransport::Tls => "tls",
    };
    format!("sip:{username}@{addr};transport={transport_param}")
}

fn build_uac(
    cfg: &RegisterConfig,
    transaction_mgr: Arc<TransactionManager>,
    dispatcher: Arc<dyn TransportDispatcher>,
    resolver: Arc<SipResolver>,
    local_addr_str: &str,
) -> anyhow::Result<IntegratedUAC> {
    // From URI is the AOR we register: sip:<username>@<server_host>.
    let local_uri = format!("sip:{}@{}", cfg.username, cfg.server_host);
    // Contact URI is where the registrar should send INVITEs back —
    // our advertised, reachable SIP address (`[node].public_address` +
    // the listen port), NOT the socket bind address. A wildcard bind
    // (`0.0.0.0`/`::`) must never leak into the Via/Contact a registrar
    // routes to. Transport param matches what we configured for this
    // registration (registrar may use it to pick a connection back to
    // us). `local_addr` below feeds the Via sent-by the same way.
    let contact_uri = build_contact_uri(&cfg.username, local_addr_str, cfg.transport);

    let builder = IntegratedUAC::builder()
        .local_uri(&local_uri)
        .contact_uri(&contact_uri)
        .transaction_manager(transaction_mgr)
        .dispatcher(dispatcher)
        .resolver(resolver)
        .credentials(cfg.auth_username.clone(), cfg.password.clone())
        .local_addr(local_addr_str)
        .map_err(|e| anyhow::anyhow!("local_addr: {e}"))?;

    builder.build()
}

fn registrar_target(cfg: &RegisterConfig) -> anyhow::Result<RequestTarget> {
    let scheme = match cfg.transport {
        SipTransport::Tls => "sips",
        _ => "sip",
    };
    let uri_str = format!("{scheme}:{}", cfg.server_addr);
    let uri = sip_core::SipUri::parse(&uri_str)
        .map_err(|e| anyhow::anyhow!("registrar URI {uri_str:?}: {e}"))?;
    Ok(RequestTarget::Uri(uri))
}

/// Emit the gauge for the new state and zero out the previous
/// row. For the very first publish (`prev` = None) we just set the
/// new row.
fn publish_state(name: &str, prev: Option<RegistrationStatus>, new: RegistrationStatus) {
    if let Some(prev) = prev {
        if prev != new {
            metrics::gauge!(
                REGISTER_STATE,
                "name" => name.to_string(),
                "state" => prev.as_str(),
            )
            .set(0.0);
        }
    }
    metrics::gauge!(
        REGISTER_STATE,
        "name" => name.to_string(),
        "state" => new.as_str(),
    )
    .set(1.0);
}

async fn emit_webhook(
    sink: &WebhookSinkHandle,
    name: &str,
    prev: Option<RegistrationStatus>,
    new: RegistrationStatus,
    last_error: Option<String>,
    expires_at: Option<DateTime<Utc>>,
) {
    let event = WebhookEvent::RegistrationStateChanged(RegistrationStateChangedEvent {
        version: WEBHOOK_VERSION,
        name: name.to_string(),
        timestamp: Utc::now(),
        status: new.as_str().to_string(),
        previous_status: prev.map(|p| p.as_str().to_string()),
        last_error,
        expires_at,
    });
    sink.emit(event).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use sip_core::msg::StatusLine;
    use sip_core::{headers::Headers, Response};
    use siphon_ai_sip_glue::RegistrationEntry;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    // ─── run_loop mechanics (scripted backend, virtual time) ────────

    /// One recorded backend call: the requested `expires` and the
    /// (virtual) time it happened.
    type Call = (u32, tokio::time::Instant);

    /// Scripted [`RegisterBackend`]: pops outcomes off a queue
    /// (repeating `Registered` once exhausted) and records every call.
    #[derive(Clone, Default)]
    struct FakeBackend {
        script: Arc<Mutex<VecDeque<RegisterOutcome>>>,
        calls: Arc<Mutex<Vec<Call>>>,
    }

    impl RegisterBackend for FakeBackend {
        async fn register(&self, expires_secs: u32) -> RegisterOutcome {
            self.calls
                .lock()
                .unwrap()
                .push((expires_secs, tokio::time::Instant::now()));
            self.script
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(RegisterOutcome::Registered {
                    granted_expires: Duration::from_secs(100_000),
                })
        }
    }

    fn ok_outcome() -> RegisterOutcome {
        RegisterOutcome::Registered {
            granted_expires: Duration::from_secs(100_000),
        }
    }

    fn failed_outcome() -> RegisterOutcome {
        RegisterOutcome::Failed {
            outcome_label: outcome::REJECTED,
            error_msg: "503 Service Unavailable".into(),
        }
    }

    struct LoopFixture {
        manager: RegistrationManager,
        backend: FakeBackend,
        handle: JoinHandle<()>,
    }

    /// Spawn `run_loop` against a fresh manager + scripted backend.
    fn spawn_loop(script: Vec<RegisterOutcome>, start_parked: bool) -> LoopFixture {
        let manager = RegistrationManager::new();
        manager.seed(&[RegistrationEntry {
            name: "pbx".into(),
            server_addr: "10.0.0.1:5060".parse().unwrap(),
            register_on_startup: !start_parked,
        }]);
        let backend = FakeBackend {
            script: Arc::new(Mutex::new(script.into())),
            calls: Arc::new(Mutex::new(Vec::new())),
        };
        let commands = manager.register_command_channel("pbx");
        let signal = manager.shutdown_signal();
        let handle = {
            let manager = manager.clone();
            let backend = backend.clone();
            tokio::spawn(async move {
                run_loop(
                    &manager,
                    "pbx",
                    3600,
                    start_parked,
                    &backend,
                    commands,
                    Arc::new(siphon_ai_webhooks::NullSink),
                    signal,
                )
                .await;
            })
        };
        LoopFixture {
            manager,
            backend,
            handle,
        }
    }

    /// Poll (1 ms virtual-time steps) until the backend has seen `n`
    /// calls. Panics after a bounded number of steps so a hung loop
    /// fails the test rather than wedging it.
    async fn wait_calls(backend: &FakeBackend, n: usize) {
        for _ in 0..10_000 {
            if backend.calls.lock().unwrap().len() >= n {
                return;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        panic!(
            "backend never reached {n} calls (got {})",
            backend.calls.lock().unwrap().len()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn refresh_command_registers_off_cycle() {
        let fx = spawn_loop(vec![ok_outcome()], false);
        wait_calls(&fx.backend, 1).await;

        fx.manager
            .send_command("pbx", RegistrationCommand::Refresh)
            .expect("send");
        wait_calls(&fx.backend, 2).await;

        let calls = fx.backend.calls.lock().unwrap().clone();
        assert_eq!(calls[1].0, 3600, "refresh sends a normal REGISTER");
        // Off-cycle: the granted expires was 100 000 s (refresh timer
        // ≈ 99 940 s out); the command-triggered attempt landed within
        // the test's millisecond-scale polling, not at the timer.
        let delta = calls[1].1 - calls[0].1;
        assert!(
            delta < Duration::from_secs(1_000),
            "expected an off-cycle REGISTER, got one {delta:?} later",
        );

        fx.manager.shutdown();
        let _ = fx.handle.await;
    }

    #[tokio::test(start_paused = true)]
    async fn restart_command_clears_binding_then_reregisters() {
        let fx = spawn_loop(vec![ok_outcome()], false);
        wait_calls(&fx.backend, 1).await;

        fx.manager
            .send_command("pbx", RegistrationCommand::Restart)
            .expect("send");
        wait_calls(&fx.backend, 3).await;

        let calls = fx.backend.calls.lock().unwrap().clone();
        let expires: Vec<u32> = calls.iter().map(|c| c.0).collect();
        assert_eq!(
            expires,
            vec![3600, 0, 3600],
            "restart = Expires:0 unregister, then a fresh REGISTER",
        );

        fx.manager.shutdown();
        let _ = fx.handle.await;
    }

    #[tokio::test(start_paused = true)]
    async fn restart_proceeds_past_failed_unregister() {
        // Script: initial register OK, the restart's unregister FAILS,
        // the follow-up register succeeds — status must end Registered.
        let fx = spawn_loop(vec![ok_outcome(), failed_outcome(), ok_outcome()], false);
        wait_calls(&fx.backend, 1).await;

        fx.manager
            .send_command("pbx", RegistrationCommand::Restart)
            .expect("send");
        wait_calls(&fx.backend, 3).await;
        // Let the outcome handling finish before asserting status.
        tokio::time::sleep(Duration::from_millis(5)).await;

        assert_eq!(
            fx.manager.get("pbx").unwrap().status,
            RegistrationStatus::Registered,
            "only the final REGISTER drives status",
        );

        fx.manager.shutdown();
        let _ = fx.handle.await;
    }

    #[tokio::test(start_paused = true)]
    async fn command_during_backoff_retries_immediately() {
        // First attempt fails → 5 s backoff. A command inside that
        // window must retry now, not at the backoff deadline.
        let fx = spawn_loop(vec![failed_outcome(), ok_outcome()], false);
        wait_calls(&fx.backend, 1).await;

        fx.manager
            .send_command("pbx", RegistrationCommand::Refresh)
            .expect("send");
        wait_calls(&fx.backend, 2).await;

        let calls = fx.backend.calls.lock().unwrap().clone();
        let delta = calls[1].1 - calls[0].1;
        assert!(
            delta < Duration::from_secs(4),
            "expected the kick to preempt the 5 s backoff, got {delta:?}",
        );

        fx.manager.shutdown();
        let _ = fx.handle.await;
    }

    #[tokio::test(start_paused = true)]
    async fn parked_binding_registers_only_on_first_command() {
        let fx = spawn_loop(vec![ok_outcome()], true);

        // Give the loop time (virtual) to misbehave if it were going
        // to: no REGISTER may fire while parked.
        tokio::time::sleep(Duration::from_secs(600)).await;
        assert!(
            fx.backend.calls.lock().unwrap().is_empty(),
            "parked binding must not REGISTER until told to",
        );
        assert_eq!(
            fx.manager.get("pbx").unwrap().status,
            RegistrationStatus::Disabled,
        );

        fx.manager
            .send_command("pbx", RegistrationCommand::Refresh)
            .expect("send");
        wait_calls(&fx.backend, 1).await;
        tokio::time::sleep(Duration::from_millis(5)).await;
        assert_eq!(
            fx.manager.get("pbx").unwrap().status,
            RegistrationStatus::Registered,
        );

        fx.manager.shutdown();
        let _ = fx.handle.await;
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_exits_parked_and_sleeping_loops() {
        let parked = spawn_loop(vec![], true);
        let sleeping = spawn_loop(vec![ok_outcome()], false);
        wait_calls(&sleeping.backend, 1).await;

        parked.manager.shutdown();
        sleeping.manager.shutdown();
        parked.handle.await.expect("parked task exits");
        sleeping.handle.await.expect("sleeping task exits");
    }

    fn response_with(headers: &[(&str, &str)]) -> Response {
        let mut h = Headers::new();
        for (k, v) in headers {
            h.push(*k, *v).unwrap();
        }
        Response::new(StatusLine::new(200, "OK").unwrap(), h, Bytes::new()).unwrap()
    }

    #[test]
    fn response_expires_reads_top_level_header() {
        let resp = response_with(&[("Expires", "1800")]);
        assert_eq!(response_expires(&resp), Some(Duration::from_secs(1800)));
    }

    #[test]
    fn response_expires_falls_back_to_contact_param() {
        let resp = response_with(&[("Contact", "<sip:bob@10.0.0.1>;expires=240;q=1.0")]);
        assert_eq!(response_expires(&resp), Some(Duration::from_secs(240)));
    }

    #[test]
    fn response_expires_returns_none_when_absent() {
        let resp = response_with(&[("Contact", "<sip:bob@10.0.0.1>")]);
        assert_eq!(response_expires(&resp), None);
    }

    #[test]
    fn response_expires_ignores_garbage_value() {
        let resp = response_with(&[("Expires", "not-a-number")]);
        assert_eq!(response_expires(&resp), None);
    }

    #[test]
    fn contact_uri_carries_advertised_addr_and_transport() {
        let uri = build_contact_uri("+15551234567", "203.0.113.199:5060", SipTransport::Udp);
        assert_eq!(uri, "sip:+15551234567@203.0.113.199:5060;transport=udp");
    }

    #[test]
    fn contact_uri_never_carries_wildcard_bind() {
        // Regression: a wildcard SIP bind (`0.0.0.0`) must not leak into
        // the Contact a registrar routes back through. The drive task is
        // fed `[node].public_address` (the advertised addr), never the
        // socket bind, so the wildcard never reaches this builder.
        let uri = build_contact_uri("alice", "10.0.0.7:5061", SipTransport::Tcp);
        assert!(
            !uri.contains("0.0.0.0"),
            "contact leaked wildcard bind: {uri}"
        );
        assert!(uri.contains("10.0.0.7:5061"));
        assert!(uri.ends_with(";transport=tcp"));
    }
}
