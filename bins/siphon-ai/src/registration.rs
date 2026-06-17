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
    refresh_delay, spawn_disabled_task, RegistrationManager, RegistrationStatus, ShutdownSignal,
};
use siphon_ai_telemetry::metrics::{REGISTER_ATTEMPTS_TOTAL, REGISTER_STATE};
use siphon_ai_webhooks::{
    RegistrationStateChangedEvent, WebhookEvent, WebhookSinkHandle, WEBHOOK_VERSION,
};
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
/// Blocks with `register_on_startup = false` get a no-op task that
/// just waits for shutdown (same as before — see
/// [`spawn_disabled_task`]).
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
        if !cfg.register_on_startup {
            info!(
                name = %cfg.name,
                "[[register]] disabled by config (register_on_startup = false)"
            );
            handles.push(spawn_disabled_task(manager.clone(), cfg.name.clone()));
            continue;
        }
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
    tokio::spawn(async move {
        if let Err(e) = drive(
            &manager,
            &cfg,
            transaction_mgr,
            dispatcher,
            resolver,
            local_addr_str,
            webhook_sink,
            signal,
        )
        .await
        {
            warn!(name = %cfg.name, error = %e, "registration drive task ended with error");
        }
    })
}

/// Inner loop. Returns `Err` only on unrecoverable setup failures
/// (UAC builder rejected the config). Per-attempt failures stay in
/// the loop and roll over to a backoff retry.
///
/// 8 args is over clippy's 7-arg threshold; each is independent
/// daemon-side plumbing (manager, cfg, two transaction/transport
/// arcs, resolver, local addr, webhook sink, shutdown). Bundling
/// them into a `Drive Context` struct buys nothing — the call site
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
    signal: ShutdownSignal,
) -> anyhow::Result<()> {
    let uac = build_uac(cfg, transaction_mgr, dispatcher, resolver, &local_addr_str)?;

    info!("registration drive started");

    let mut backoff = BACKOFF_INITIAL;
    let registrar = registrar_target(cfg)?;
    let expires_secs = cfg.expires.as_secs() as u32;

    loop {
        let mut prev_status = manager
            .get(&cfg.name)
            .map(|s| s.status)
            .unwrap_or(RegistrationStatus::Pending);

        debug!(?prev_status, "sending REGISTER");
        let outcome = perform_register(&uac, registrar.clone(), expires_secs).await;

        match outcome {
            RegisterOutcome::Registered { granted_expires } => {
                let expires_at =
                    Utc::now() + chrono::Duration::seconds(granted_expires.as_secs() as i64);
                metrics::counter!(
                    REGISTER_ATTEMPTS_TOTAL,
                    "name" => cfg.name.clone(),
                    "outcome" => outcome::REGISTERED,
                )
                .increment(1);
                manager.set_status(
                    &cfg.name,
                    RegistrationStatus::Registered,
                    None,
                    Some(expires_at),
                );
                if prev_status != RegistrationStatus::Registered {
                    publish_state(&cfg.name, Some(prev_status), RegistrationStatus::Registered);
                    emit_webhook(
                        &webhook_sink,
                        &cfg.name,
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

                // Sleep until refresh time OR shutdown.
                let delay = refresh_delay(granted_expires);
                debug!(delay_secs = delay.as_secs(), "sleeping until refresh");
                if interruptible_sleep(delay, &signal).await.is_shutdown() {
                    info!("shutdown signal received while registered");
                    return Ok(());
                }
            }
            RegisterOutcome::Failed {
                outcome_label,
                error_msg,
            } => {
                metrics::counter!(
                    REGISTER_ATTEMPTS_TOTAL,
                    "name" => cfg.name.clone(),
                    "outcome" => outcome_label,
                )
                .increment(1);
                manager.set_status(
                    &cfg.name,
                    RegistrationStatus::Failed,
                    Some(error_msg.clone()),
                    None,
                );
                if prev_status != RegistrationStatus::Failed {
                    publish_state(&cfg.name, Some(prev_status), RegistrationStatus::Failed);
                    emit_webhook(
                        &webhook_sink,
                        &cfg.name,
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

                if interruptible_sleep(backoff, &signal).await.is_shutdown() {
                    info!("shutdown signal received during backoff");
                    return Ok(());
                }
                backoff = (backoff * 2).min(BACKOFF_MAX);
            }
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

/// Result of an interruptible sleep — either the requested delay
/// elapsed naturally, or the shutdown signal woke us early.
enum SleepOutcome {
    Elapsed,
    Shutdown,
}

impl SleepOutcome {
    fn is_shutdown(&self) -> bool {
        matches!(self, SleepOutcome::Shutdown)
    }
}

async fn interruptible_sleep(delay: Duration, signal: &ShutdownSignal) -> SleepOutcome {
    tokio::select! {
        _ = tokio::time::sleep(delay) => SleepOutcome::Elapsed,
        _ = signal.cancelled() => SleepOutcome::Shutdown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use sip_core::msg::StatusLine;
    use sip_core::{headers::Headers, Response};

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
