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
//!   delivery changes there need a restart);
//! - the **outbound gateway set** (`[[gateway]]`) — rebuilt (fresh
//!   per-gateway UACs) and swapped when outbound is enabled and its
//!   `[outbound]` limits are unchanged; in-flight outbound calls keep
//!   the originator they captured.
//!
//! It is **fail-safe**: a new config that doesn't load/compile is
//! logged + counted and the running config is kept — a bad edit can't
//! take the daemon down (this is why `siphon-ai check` is the right
//! pre-reload preflight).
//!
//! Sections that bind sockets or build process-wide state (`[sip]`
//! listen/transports, `[node]`, `[media]` + the `[bridge]`/codec
//! defaults — `[media].codecs` / `.dtmf` compile into the bridge
//! defaults — `[observability]`, `[admin]`, `[hep]`,
//! `[security.stir_shaken]`, and `[outbound]` limits — the concurrency
//! cap / rate limit, which also flip outbound on/off) **require a
//! restart**. A reload whose value for any of those differs from the
//! running one applies the safe sections and logs a prominent warning
//! naming them.

use std::path::PathBuf;
use std::sync::Arc;

use arc_swap::ArcSwap;
use async_trait::async_trait;
use siphon_ai_cdr::{CdrRecord, CdrSink, CdrSinkHandle};
use siphon_ai_config::{Config, OutboundConfig, SipTlsConfig};
use siphon_ai_core::OutboundService;
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
///
/// `[outbound].limits` (the concurrency cap + rate limit, which also
/// flips outbound on/off) is restart-required — resizing the live
/// admission semaphore isn't safe. The *gateway set* itself is
/// hot-reloadable (see [`gateway_fingerprint`]) and is **not** here.
pub fn restart_fingerprints(c: &Config) -> Vec<(&'static str, String)> {
    vec![
        ("[sip].listen", c.sip.listen_addr.to_string()),
        ("[sip].transports", format!("{:?}", c.sip.transports)),
        ("[node]", format!("{}|{}", c.node.id, c.node.public_address)),
        // `[media]` proper (rtp port range, MOH, SRTP).
        ("[media]", format!("{:?}", c.media)),
        // Bridge/codec defaults — `[media].codecs` / `.dtmf` compile in
        // here, along with `[bridge]` ws_url / auth_header / barge-in /
        // timeouts. None of it is hot-reloaded, so fingerprint the whole
        // struct: a change to any of it (codecs, DTMF, …) must surface as
        // restart-required rather than being silently swallowed.
        (
            "[bridge]/[media] defaults",
            format!("{:?}", c.bridge_defaults),
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
            "[outbound].limits",
            format!(
                "{}|{:?}",
                c.outbound.max_concurrent, c.outbound.rate_limit_per_sec
            ),
        ),
    ]
}

/// Fingerprint the **gateway set** (hot-reloadable). Captures every field
/// that affects how a gateway dials, sorted + joined so the order in the
/// file doesn't matter. A change here, with `[outbound].limits`
/// unchanged, triggers a gateway rebuild on reload.
pub fn gateway_fingerprint(outbound: &OutboundConfig) -> String {
    let mut gws: Vec<String> = outbound
        .gateways
        .iter()
        .map(|g| {
            format!(
                "{}|{}:{}|{}|{}|{:?}|creds={}",
                g.name,
                g.proxy_host,
                g.proxy_port,
                g.transport.uri_param(),
                g.from,
                g.srtp,
                g.credentials.is_some(),
            )
        })
        .collect();
    gws.sort_unstable();
    gws.join(",")
}

/// What a SIGHUP reload should do with a sink whose config we re-read.
/// A sink is touched only when its config actually changed; when its
/// durable spool is active the change can't be hot-applied (the drain
/// worker is stateful).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SinkReload {
    /// Config identical to the running one — leave the sink alone (and,
    /// crucially, don't warn about a spool that isn't being changed).
    Unchanged,
    /// Changed, no spool — rebuild + swap.
    Swap,
    /// Changed, but a spool is active — warn restart-required.
    RestartRequired,
}

fn decide_sink_reload(changed: bool, spool_active: bool) -> SinkReload {
    if !changed {
        SinkReload::Unchanged
    } else if spool_active {
        SinkReload::RestartRequired
    } else {
        SinkReload::Swap
    }
}

// ─── SIGHUP reload handler ──────────────────────────────────────────

/// The outbound service + its gateway-build deps, kept so a reload can
/// rebuild + swap the gateway set. `None` when `[outbound]` was disabled
/// at startup (there's no service to reload into — gateway changes are
/// then restart-required, caught by the `[outbound].limits` fingerprint).
pub struct OutboundReload {
    pub service: Arc<OutboundService>,
    pub deps: crate::runtime::GatewayBuildDeps,
    /// Gateway-set fingerprint at startup (rolled forward by the handler).
    pub gateway_fingerprint: String,
}

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
    /// `[webhooks]` / `[cdr]` fingerprints at startup (so a reload only
    /// touches a sink when its config actually changed).
    pub webhook_fingerprint: String,
    pub cdr_fingerprint: String,
    /// Outbound gateway hot-reload handle; `None` when outbound is off.
    pub outbound: Option<OutboundReload>,
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
        let mut webhook_fp = ctx.webhook_fingerprint;
        let mut cdr_fp = ctx.cdr_fingerprint;
        let mut gateway_fp = ctx
            .outbound
            .as_ref()
            .map(|o| o.gateway_fingerprint.clone())
            .unwrap_or_default();

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
                ctx.outbound.as_ref(),
                &fingerprints,
                webhook_spool_active,
                cdr_spool_active,
                &gateway_fp,
                &webhook_fp,
                &cdr_fp,
            )
            .await;

            if !applied.restart_required.is_empty() {
                warn!(sections = ?applied.restart_required, "SIGHUP: these sections changed but require a restart to take effect; NOT applied");
            }
            info!("SIGHUP: config reload applied");
            metrics::counter!(CONFIG_RELOADS_TOTAL, "result" => "applied").increment(1);

            last_text = text;
            fingerprints = applied.fingerprints;
            webhook_spool_active = applied.webhook_spool_active;
            cdr_spool_active = applied.cdr_spool_active;
            webhook_fp = applied.webhook_fingerprint;
            cdr_fp = applied.cdr_fingerprint;
            gateway_fp = applied.gateway_fingerprint;
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
    /// Fresh gateway-set fingerprint (carried to the next reload).
    pub gateway_fingerprint: String,
    /// Fresh `[webhooks]` / `[cdr]` fingerprints (carried forward, so a
    /// reload only touches a sink — and only warns about its spool —
    /// when that sink's config actually changed).
    pub webhook_fingerprint: String,
    pub cdr_fingerprint: String,
}

/// Hot-apply the reload-safe sections of a freshly-loaded config: store
/// the new route table, rebuild + swap the webhook / CDR sinks (unless a
/// durable spool is active for that sink), and rebuild + swap the
/// outbound gateway set (when outbound is enabled and its `limits` are
/// unchanged). Returns which restart-required sections changed and the
/// updated rolling state. Split out from the SIGHUP loop so it's
/// unit-testable without signals.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn apply_reload(
    new: Config,
    route_swap: &ArcSwap<RouteSet>,
    webhook_swap: &SwappableWebhookSink,
    cdr_swap: &SwappableCdrSink,
    hep: Option<&HepTelemetry>,
    outbound_reload: Option<&OutboundReload>,
    prev_fingerprints: &[(&'static str, String)],
    prev_webhook_spool: bool,
    prev_cdr_spool: bool,
    prev_gateway_fp: &str,
    prev_webhook_fp: &str,
    prev_cdr_fp: &str,
) -> ReloadApplied {
    let new_fp = restart_fingerprints(&new);
    let restart_required: Vec<&'static str> = prev_fingerprints
        .iter()
        .zip(&new_fp)
        .filter(|(old, new)| old.1 != new.1)
        .map(|(old, _)| old.0)
        .collect();
    let new_gateway_fp = gateway_fingerprint(&new.outbound);

    // Snapshot the new spool state + sink fingerprints, then take the
    // sections we hot-apply (partial move out of `new`).
    let new_webhook_spool = new.webhooks.spool_dir.is_some();
    let new_cdr_spool = new
        .cdr
        .webhook
        .as_ref()
        .and_then(|w| w.spool_dir.as_ref())
        .is_some();
    let new_webhook_fp = format!("{:?}", new.webhooks);
    let new_cdr_fp = format!("{:?}", new.cdr);
    let Config {
        routes,
        webhooks,
        cdr,
        outbound,
        ..
    } = new;

    // Routes: always reload-safe.
    route_swap.store(Arc::new(routes));

    // Webhook sink: act only when `[webhooks]` actually changed. Hot-swap
    // unless a spool is (or becomes) active — its drain worker can't be
    // hot-swapped. (Warning only on a real change, not every reload.)
    match decide_sink_reload(
        new_webhook_fp != prev_webhook_fp,
        prev_webhook_spool || new_webhook_spool,
    ) {
        SinkReload::Unchanged => {}
        SinkReload::RestartRequired => {
            warn!("SIGHUP: [webhooks] changed but its durable spool is active; webhook delivery changes require a restart (not hot-applied)")
        }
        SinkReload::Swap => match crate::runtime::build_webhook_sink(webhooks) {
            Ok(sink) => webhook_swap.store(sink),
            Err(e) => warn!(error = %e, "SIGHUP: rebuilding webhook sink failed; keeping previous"),
        },
    }

    // CDR sink: same change-gated logic.
    match decide_sink_reload(new_cdr_fp != prev_cdr_fp, prev_cdr_spool || new_cdr_spool) {
        SinkReload::Unchanged => {}
        SinkReload::RestartRequired => {
            warn!("SIGHUP: [cdr] changed but its durable spool is active; CDR delivery changes require a restart (not hot-applied)")
        }
        SinkReload::Swap => match crate::runtime::build_cdr_sink(cdr, hep).await {
            Ok(sink) => cdr_swap.store(sink),
            Err(e) => warn!(error = %e, "SIGHUP: rebuilding CDR sink failed; keeping previous"),
        },
    }

    // Gateways: rebuild + swap the set when outbound is running, its
    // `limits` (cap/rate — restart-required) didn't change, and the
    // gateway set actually differs. Enable/disable + cap changes are
    // covered by the `[outbound].limits` restart-required warning.
    if let Some(ob) = outbound_reload {
        let limits_changed = restart_required.contains(&"[outbound].limits");
        if outbound.enabled() && !limits_changed && new_gateway_fp != prev_gateway_fp {
            match crate::runtime::build_gateways(&outbound, &ob.deps) {
                Ok(gateways) => {
                    let n = gateways.len();
                    ob.service.reload_gateways(gateways);
                    info!(gateways = n, "SIGHUP: outbound gateways reloaded");
                }
                Err(e) => {
                    warn!(error = %e, "SIGHUP: rebuilding gateways failed; keeping previous set")
                }
            }
        }
    }

    ReloadApplied {
        restart_required,
        fingerprints: new_fp,
        webhook_spool_active: new_webhook_spool,
        cdr_spool_active: new_cdr_spool,
        gateway_fingerprint: new_gateway_fp,
        webhook_fingerprint: new_webhook_fp,
        cdr_fingerprint: new_cdr_fp,
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
        let out = apply_reload(
            new,
            &route_swap,
            &wh,
            &cd,
            None,
            None,
            &fp,
            false,
            false,
            "",
            "",
            "",
        )
        .await;

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
        let out = apply_reload(
            new,
            &route_swap,
            &wh,
            &cd,
            None,
            None,
            &fp,
            false,
            false,
            "",
            "",
            "",
        )
        .await;

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

    const OUTBOUND: &str = r#"
[node]
id = "reload-test"
[sip]
listen = "127.0.0.1:5060"
[bridge]
ws_url = "wss://base/ws"
[outbound]
max_concurrent = 4
[[gateway]]
name = "twilio"
proxy = "sip.twilio.example"
from = "sip:+1@x"
[[route]]
name = "default"
[route.match]
any = true
"#;

    #[test]
    fn gateway_fingerprint_tracks_gateway_changes_not_restart_sections() {
        let base = cfg(OUTBOUND);
        // A changed proxy changes the gateway fingerprint...
        let changed = cfg(&OUTBOUND.replace("sip.twilio.example", "sip.other.example"));
        assert_ne!(
            gateway_fingerprint(&base.outbound),
            gateway_fingerprint(&changed.outbound)
        );
        // ...but does NOT appear in the restart-required fingerprints
        // (the gateway set is hot-reloadable).
        assert_eq!(
            restart_fingerprints(&base),
            restart_fingerprints(&changed),
            "a gateway change must not be flagged restart-required"
        );
    }

    #[test]
    fn outbound_limits_change_is_restart_required() {
        let base = restart_fingerprints(&cfg(OUTBOUND));
        let capped = restart_fingerprints(&cfg(
            &OUTBOUND.replace("max_concurrent = 4", "max_concurrent = 8")
        ));
        let changed: Vec<_> = base
            .iter()
            .zip(&capped)
            .filter(|(a, b)| a.1 != b.1)
            .map(|(a, _)| a.0)
            .collect();
        assert_eq!(changed, vec!["[outbound].limits"]);
    }

    // Bug 3: a `[media].codecs` change must be flagged restart-required,
    // not silently swallowed.
    #[test]
    fn media_codecs_change_is_restart_required() {
        const WITH_CODECS: &str = r#"
[node]
id = "x"
[sip]
listen = "127.0.0.1:5060"
[media]
codecs = ["pcmu", "pcma"]
[bridge]
ws_url = "wss://b/ws"
[[route]]
name = "default"
[route.match]
any = true
"#;
        let base = restart_fingerprints(&cfg(WITH_CODECS));
        let changed_cfg =
            cfg(&WITH_CODECS.replace(r#"codecs = ["pcmu", "pcma"]"#, r#"codecs = ["pcma"]"#));
        let changed: Vec<_> = base
            .iter()
            .zip(&restart_fingerprints(&changed_cfg))
            .filter(|(a, b)| a.1 != b.1)
            .map(|(a, _)| a.0)
            .collect();
        assert_eq!(
            changed,
            vec!["[bridge]/[media] defaults"],
            "a codec change must surface as restart-required"
        );
    }

    // Bug 2: the spool warning fires only on a real `[webhooks]` change,
    // not on every reload while a spool is configured.
    #[test]
    fn sink_reload_decision_only_acts_on_change() {
        // Unchanged + spool active → leave it alone (no warning).
        assert_eq!(decide_sink_reload(false, true), SinkReload::Unchanged);
        // Unchanged + no spool → still nothing (don't needlessly rebuild).
        assert_eq!(decide_sink_reload(false, false), SinkReload::Unchanged);
        // Changed + spool active → restart-required (warn).
        assert_eq!(decide_sink_reload(true, true), SinkReload::RestartRequired);
        // Changed + no spool → hot-swap.
        assert_eq!(decide_sink_reload(true, false), SinkReload::Swap);
    }
}
