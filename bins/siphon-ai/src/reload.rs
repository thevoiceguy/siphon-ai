//! SIGHUP config reload (0.12.0).
//!
//! On `SIGHUP` (`systemctl reload siphon-ai`) the daemon re-reads its
//! `--config` file and hot-applies the **reload-safe** sections without
//! dropping calls:
//!
//! - the **route table** — swapped behind an [`ArcSwap`]; new INVITEs
//!   pick up the new dialplan, in-flight calls keep the route they
//!   matched;
//! - the **webhook + CDR sinks** — swapped behind the delegating
//!   wrappers in this module, *unless* a durable spool is active for
//!   that sink (its background drain worker can't be hot-swapped, so
//!   delivery changes there need a restart).
//!
//! It is **fail-safe**: a new config that doesn't load/compile is
//! logged + counted and the running config is kept — a bad edit can't
//! take the daemon down (this is why `siphon-ai check` is the right
//! pre-reload preflight).
//!
//! Sections that bind sockets or build process-wide state (`[sip]`
//! listen/transports, `[node]`, `[observability]`, `[admin]`,
//! `[media]`, `[hep]`, `[security.stir_shaken]`, and — until a focused
//! follow-up — `[[gateway]]`) **require a restart**. A reload whose
//! value for any of those differs from the running one applies the
//! safe sections and logs a prominent warning naming them.

use std::path::PathBuf;
use std::sync::Arc;

use arc_swap::ArcSwap;
use async_trait::async_trait;
use siphon_ai_cdr::{CdrRecord, CdrSink, CdrSinkHandle};
use siphon_ai_config::{Config, SipTlsConfig};
use siphon_ai_routes::RouteSet;
use siphon_ai_telemetry::{HepTelemetry, CONFIG_RELOADS_TOTAL};
use siphon_ai_webhooks::{WebhookEvent, WebhookSink, WebhookSinkHandle};
use tokio_rustls::rustls::ServerConfig;
use tracing::{error, info, warn};

// ─── Swappable sink wrappers ────────────────────────────────────────
//
// Each holds the live inner sink behind an `ArcSwap` and implements the
// sink trait by delegating to whatever is loaded. Handing the wrapper
// out as the sink handle means nothing downstream changes when a reload
// swaps the inner.

// `ArcSwap<T>` stores `Arc<T>`; the sink handles are themselves
// `Arc<dyn …>` (a *sized* pointer), so we swap them as the payload
// `T` — giving a (cheap) double-Arc that sidesteps arc-swap's
// unsized-payload limitation while staying lock-free.

/// Lifecycle-webhook sink whose inner delegate can be swapped at runtime.
pub struct SwappableWebhookSink {
    inner: ArcSwap<WebhookSinkHandle>,
}

impl SwappableWebhookSink {
    pub fn new(inner: WebhookSinkHandle) -> Self {
        Self {
            inner: ArcSwap::from_pointee(inner),
        }
    }
    /// Replace the delegate; subsequent emits use the new sink.
    pub fn store(&self, inner: WebhookSinkHandle) {
        self.inner.store(Arc::new(inner));
    }
}

#[async_trait]
impl WebhookSink for SwappableWebhookSink {
    async fn emit(&self, event: WebhookEvent) {
        let sink = self.inner.load_full();
        sink.emit(event).await;
    }
}

/// CDR sink whose inner delegate can be swapped at runtime.
pub struct SwappableCdrSink {
    inner: ArcSwap<CdrSinkHandle>,
}

impl SwappableCdrSink {
    pub fn new(inner: CdrSinkHandle) -> Self {
        Self {
            inner: ArcSwap::from_pointee(inner),
        }
    }
    pub fn store(&self, inner: CdrSinkHandle) {
        self.inner.store(Arc::new(inner));
    }
}

#[async_trait]
impl CdrSink for SwappableCdrSink {
    async fn emit(&self, record: CdrRecord) {
        let sink = self.inner.load_full();
        sink.emit(record).await;
    }
}

// ─── Restart-required fingerprints ──────────────────────────────────

/// Fingerprint the config sections that require a daemon **restart** to
/// change (they bind sockets / build process-wide state). A reload
/// compares these against the running values and warns on any diff.
/// `[[gateway]]` is here until gateway hot-reload lands as a follow-up.
pub fn restart_fingerprints(c: &Config) -> Vec<(&'static str, String)> {
    let mut gateways: Vec<&str> = c
        .outbound
        .gateways
        .iter()
        .map(|g| g.name.as_str())
        .collect();
    gateways.sort_unstable();
    vec![
        ("[sip].listen", c.sip.listen_addr.to_string()),
        ("[sip].transports", format!("{:?}", c.sip.transports)),
        ("[node]", format!("{}|{}", c.node.id, c.node.public_address)),
        (
            "[media]",
            format!(
                "{:?}|{:?}|{:?}",
                c.media.rtp_port_range, c.media.moh_file, c.media.srtp
            ),
        ),
        (
            "[observability]",
            format!(
                "{}|{:?}",
                c.observability.enabled, c.observability.http_listen
            ),
        ),
        (
            "[admin]",
            format!("{:?}", c.admin.as_ref().map(|a| a.listen_addr)),
        ),
        ("[hep]", format!("{}|{:?}", c.hep.enabled, c.hep.collector)),
        (
            "[security.stir_shaken]",
            c.security.stir_shaken.enabled.to_string(),
        ),
        (
            "[[gateway]]",
            format!("{}|{:?}", c.outbound.max_concurrent, gateways),
        ),
    ]
}

// ─── SIGHUP reload handler ──────────────────────────────────────────

/// Everything the SIGHUP reload loop needs. Built once in
/// `Runtime::build` and moved into the spawned handler.
pub struct ReloadContext {
    pub config_path: PathBuf,
    /// File contents at startup, for change detection.
    pub initial_text: String,
    pub route_swap: Arc<ArcSwap<RouteSet>>,
    pub webhook_swap: Arc<SwappableWebhookSink>,
    pub cdr_swap: Arc<SwappableCdrSink>,
    pub hep_telemetry: Option<Arc<HepTelemetry>>,
    /// `[webhooks].spool_dir` set on the running config.
    pub webhook_spool_active: bool,
    /// `[cdr.webhook].spool_dir` set on the running config.
    pub cdr_spool_active: bool,
    /// `Some` when TLS is configured — the cert is reloaded too (the
    /// prior dedicated TLS-reload behavior, folded into this handler).
    pub tls: Option<(SipTlsConfig, Arc<ArcSwap<ServerConfig>>)>,
    pub restart_fingerprints: Vec<(&'static str, String)>,
}

/// Spawn the SIGHUP handler. On each signal it reloads the TLS cert (if
/// configured) and the config file, hot-applying the reload-safe
/// sections. The TLS-cert reload preserves the prior `[sip.tls]`
/// hot-reload behavior; loaders for the sinks are reused from the
/// daemon's normal build path.
pub fn spawn_reload_handler(ctx: ReloadContext) {
    use tokio::signal::unix::{signal, SignalKind};

    tokio::spawn(async move {
        let mut stream = match signal(SignalKind::hangup()) {
            Ok(s) => s,
            Err(e) => {
                error!(error = %e, "failed to install SIGHUP handler; config + cert hot-reload disabled");
                return;
            }
        };
        info!(
            config = %ctx.config_path.display(),
            "SIGHUP config reload installed; `systemctl reload` re-reads the config file"
        );

        let mut last_text = ctx.initial_text;
        let mut fingerprints = ctx.restart_fingerprints;
        let mut webhook_spool_active = ctx.webhook_spool_active;
        let mut cdr_spool_active = ctx.cdr_spool_active;

        while stream.recv().await.is_some() {
            // 1. TLS cert reload (independent of the config file).
            if let Some((tls, swap)) = &ctx.tls {
                match crate::runtime::load_sip_tls_server_config(tls) {
                    Ok(new_cfg) => {
                        swap.store(new_cfg);
                        metrics::counter!("siphon_ai_sip_tls_reload_attempts_total", "outcome" => "ok").increment(1);
                        info!(cert = %tls.cert_path.display(), "SIP/TLS cert reloaded on SIGHUP");
                    }
                    Err(e) => {
                        metrics::counter!("siphon_ai_sip_tls_reload_attempts_total", "outcome" => "failed").increment(1);
                        error!(cert = %tls.cert_path.display(), error = %e, "SIGHUP cert reload failed; keeping previous cert");
                    }
                }
            }

            // 2. Config reload.
            let text = match std::fs::read_to_string(&ctx.config_path) {
                Ok(t) => t,
                Err(e) => {
                    warn!(config = %ctx.config_path.display(), error = %e, "SIGHUP: could not read config; keeping running config");
                    metrics::counter!(CONFIG_RELOADS_TOTAL, "result" => "failed").increment(1);
                    continue;
                }
            };
            if text == last_text {
                info!("SIGHUP: config file unchanged; nothing to reload");
                metrics::counter!(CONFIG_RELOADS_TOTAL, "result" => "no_change").increment(1);
                continue;
            }
            let new = match siphon_ai_config::load_from_path(&ctx.config_path) {
                Ok(c) => c,
                Err(e) => {
                    error!(config = %ctx.config_path.display(), error = %e, "SIGHUP: new config is INVALID; keeping running config");
                    metrics::counter!(CONFIG_RELOADS_TOTAL, "result" => "failed").increment(1);
                    continue;
                }
            };

            let applied = apply_reload(
                new,
                &ctx.route_swap,
                &ctx.webhook_swap,
                &ctx.cdr_swap,
                ctx.hep_telemetry.as_deref(),
                &fingerprints,
                webhook_spool_active,
                cdr_spool_active,
            )
            .await;

            if !applied.restart_required.is_empty() {
                warn!(sections = ?applied.restart_required, "SIGHUP: these sections changed but require a restart to take effect; NOT applied");
            }
            info!("SIGHUP: config reload applied (routes + sinks)");
            metrics::counter!(CONFIG_RELOADS_TOTAL, "result" => "applied").increment(1);

            last_text = text;
            fingerprints = applied.fingerprints;
            webhook_spool_active = applied.webhook_spool_active;
            cdr_spool_active = applied.cdr_spool_active;
        }
        warn!("SIGHUP signal stream ended; config hot-reload offline");
    });
}

/// Outcome of applying one (already-loaded, changed) config.
pub(crate) struct ReloadApplied {
    /// Restart-required sections whose value changed (for the warning).
    pub restart_required: Vec<&'static str>,
    /// Fresh restart-section fingerprints (carried to the next reload).
    pub fingerprints: Vec<(&'static str, String)>,
    pub webhook_spool_active: bool,
    pub cdr_spool_active: bool,
}

/// Hot-apply the reload-safe sections of a freshly-loaded config:
/// store the new route table, and rebuild + swap the webhook / CDR
/// sinks unless a durable spool is active for that sink. Returns which
/// restart-required sections changed and the updated rolling state.
/// Split out from the SIGHUP loop so it's unit-testable without signals.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn apply_reload(
    new: Config,
    route_swap: &ArcSwap<RouteSet>,
    webhook_swap: &SwappableWebhookSink,
    cdr_swap: &SwappableCdrSink,
    hep: Option<&HepTelemetry>,
    prev_fingerprints: &[(&'static str, String)],
    prev_webhook_spool: bool,
    prev_cdr_spool: bool,
) -> ReloadApplied {
    let new_fp = restart_fingerprints(&new);
    let restart_required: Vec<&'static str> = prev_fingerprints
        .iter()
        .zip(&new_fp)
        .filter(|(old, new)| old.1 != new.1)
        .map(|(old, _)| old.0)
        .collect();

    // Snapshot the new spool state, then take the sections we hot-apply
    // (partial move out of `new`).
    let new_webhook_spool = new.webhooks.spool_dir.is_some();
    let new_cdr_spool = new
        .cdr
        .webhook
        .as_ref()
        .and_then(|w| w.spool_dir.as_ref())
        .is_some();
    let Config {
        routes,
        webhooks,
        cdr,
        ..
    } = new;

    // Routes: always reload-safe.
    route_swap.store(Arc::new(routes));

    // Webhook sink: hot-swap unless a spool is (or becomes) active —
    // its drain worker can't be hot-swapped.
    if prev_webhook_spool || new_webhook_spool {
        warn!("SIGHUP: [webhooks] spool is active; webhook delivery changes require a restart (not hot-applied)");
    } else {
        match crate::runtime::build_webhook_sink(webhooks) {
            Ok(sink) => webhook_swap.store(sink),
            Err(e) => warn!(error = %e, "SIGHUP: rebuilding webhook sink failed; keeping previous"),
        }
    }

    // CDR sink: same spool caveat.
    if prev_cdr_spool || new_cdr_spool {
        warn!("SIGHUP: [cdr.webhook] spool is active; CDR delivery changes require a restart (not hot-applied)");
    } else {
        match crate::runtime::build_cdr_sink(cdr, hep).await {
            Ok(sink) => cdr_swap.store(sink),
            Err(e) => warn!(error = %e, "SIGHUP: rebuilding CDR sink failed; keeping previous"),
        }
    }

    ReloadApplied {
        restart_required,
        fingerprints: new_fp,
        webhook_spool_active: new_webhook_spool,
        cdr_spool_active: new_cdr_spool,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use siphon_ai_config::load_from_str;

    const BASE: &str = r#"
[node]
id = "reload-test"
[sip]
listen = "127.0.0.1:5060"
[bridge]
ws_url = "wss://base/ws"
[[route]]
name = "default"
[route.match]
any = true
"#;

    fn cfg(s: &str) -> Config {
        load_from_str(s).expect("valid config")
    }

    #[tokio::test]
    async fn apply_reload_swaps_routes() {
        let route_swap = ArcSwap::from_pointee(cfg(BASE).routes);
        let wh = SwappableWebhookSink::new(Arc::new(siphon_ai_webhooks::NullSink));
        let cd = SwappableCdrSink::new(Arc::new(siphon_ai_cdr::NullSink));
        // Before: the route table that matches `default`.
        assert_eq!(route_swap.load().iter().next().unwrap().name, "default");

        // New config renames the route; reload should swap it in.
        let new = cfg(&BASE.replace("name = \"default\"", "name = \"renamed\""));
        let fp = restart_fingerprints(&cfg(BASE));
        let out = apply_reload(new, &route_swap, &wh, &cd, None, &fp, false, false).await;

        assert_eq!(route_swap.load().iter().next().unwrap().name, "renamed");
        assert!(
            out.restart_required.is_empty(),
            "only the route changed; nothing restart-required"
        );
    }

    #[tokio::test]
    async fn apply_reload_flags_restart_required_sections() {
        let route_swap = ArcSwap::from_pointee(cfg(BASE).routes);
        let wh = SwappableWebhookSink::new(Arc::new(siphon_ai_webhooks::NullSink));
        let cd = SwappableCdrSink::new(Arc::new(siphon_ai_cdr::NullSink));
        let fp = restart_fingerprints(&cfg(BASE));

        // Change the SIP listen port — a restart-required section.
        let new = cfg(&BASE.replace("127.0.0.1:5060", "127.0.0.1:5999"));
        let out = apply_reload(new, &route_swap, &wh, &cd, None, &fp, false, false).await;

        assert!(
            out.restart_required.contains(&"[sip].listen"),
            "changed listen should be flagged restart-required: {:?}",
            out.restart_required
        );
    }

    #[test]
    fn restart_fingerprints_stable_for_route_only_change() {
        let a = restart_fingerprints(&cfg(BASE));
        let b = restart_fingerprints(&cfg(&BASE.replace("name = \"default\"", "name = \"x\"")));
        assert_eq!(a, b, "a route rename must not change restart fingerprints");
    }
}
