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
//! Every **other** section is consumed only at startup (binds sockets,
//! builds process-wide state, or spawns tasks) and is **restart-required**
//! — `[node]`, `[sip]`, `[media]` + the `[bridge]`/codec defaults
//! (`[media].codecs` / `.dtmf` compile in here), `[[trunk]]`,
//! `[[register]]`, `[security]`, `[recording]`, `[conference]`, `[park]`,
//! `[observability]`, `[admin]` (incl. the token table), `[hep]`, and the
//! `[outbound]` limits. A reload whose value for any of those differs from the
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

/// Hash an arbitrary `Debug` rendering to an opaque 64-bit fingerprint.
/// Used so the rolling fingerprints **never retain cleartext** — several
/// of the fingerprinted sections carry secrets (`[bridge].ws_auth_header`,
/// `[[register]].password`, `[hep].capture_password`, gateway
/// credentials). The transient `Debug` string is hashed and dropped; only
/// the hash is stored, and fingerprint *values* are never logged (only the
/// section names in a restart-required warning are). `DefaultHasher` is
/// deterministic within a process, which is all change-detection needs.
fn fp_hash<T: std::fmt::Debug>(value: &T) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    format!("{value:?}").hash(&mut h);
    format!("{:016x}", h.finish())
}

/// Fingerprint **every** config section that is consumed only at startup
/// (binds sockets, builds process-wide state, spawns tasks) and is **not**
/// hot-reloaded. A reload compares these against the running values and
/// warns `restart-required` on any diff — so editing one of these and
/// sending SIGHUP can never silently do nothing.
///
/// The hot-reloadable sections — routes, webhook/CDR sinks, and the
/// gateway *set* ([`gateway_fingerprint`]) — are tracked separately and
/// are deliberately **not** here. `[outbound].limits` (the concurrency cap
/// / rate limit, which also flip outbound on/off) IS here — resizing the
/// live admission semaphore isn't safe — but only the limits, not the
/// gateways (which would otherwise double-count).
pub fn restart_fingerprints(c: &Config) -> Vec<(&'static str, String)> {
    vec![
        ("[node]", fp_hash(&c.node)),
        ("[sip]", fp_hash(&c.sip)),
        ("[media]", fp_hash(&c.media)),
        // `[media].codecs` / `.dtmf` compile into the bridge defaults.
        ("[bridge]/[media] defaults", fp_hash(&c.bridge_defaults)),
        ("[[trunk]]", fp_hash(&c.trunks)),
        ("[[register]]", fp_hash(&c.registrations)),
        // Whole `[security]` — `min_attestation` + its response code +
        // `[security.stir_shaken]`, not just the enabled bool.
        ("[security]", fp_hash(&c.security)),
        ("[recording]", fp_hash(&c.recording)),
        ("[conference]", fp_hash(&c.conference)),
        ("[park]", fp_hash(&c.park)),
        ("[observability]", fp_hash(&c.observability)),
        // Whole `[admin]` — the token table too (so a rotated / revoked
        // token is at least flagged restart-required), not just the listen
        // address. The token hashes are what change; no cleartext.
        ("[admin]", fp_hash(&c.admin)),
        ("[hep]", fp_hash(&c.hep)),
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
/// file doesn't matter. A change here, with `[outbound].limits` unchanged,
/// triggers a gateway rebuild on reload. Credentials are folded in as a
/// **hash** (not `is_some()`, and not cleartext) so rotating a trunk
/// `auth_password` is detected and re-applied.
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
                fp_hash(&g.credentials),
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
        let mut state = ReloadState {
            restart_baseline: ctx.restart_fingerprints,
            webhook_fp: ctx.webhook_fingerprint,
            webhook_spool: ctx.webhook_spool_active,
            cdr_fp: ctx.cdr_fingerprint,
            cdr_spool: ctx.cdr_spool_active,
            gateway_fp: ctx
                .outbound
                .as_ref()
                .map(|o| o.gateway_fingerprint.clone())
                .unwrap_or_default(),
        };

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

            // `apply_reload` mutates `state` in place — advancing a
            // fingerprint ONLY for a section it actually applied, so a
            // section that was warned restart-required (or skipped) keeps
            // its old baseline and is re-detected on the next reload.
            let restart_required = apply_reload(
                new,
                &ctx.route_swap,
                &ctx.webhook_swap,
                &ctx.cdr_swap,
                ctx.hep_telemetry.as_deref(),
                ctx.outbound.as_ref(),
                &mut state,
            )
            .await;

            if !restart_required.is_empty() {
                warn!(sections = ?restart_required, "SIGHUP: these sections changed but require a restart to take effect; NOT applied");
            }
            info!("SIGHUP: config reload applied");
            metrics::counter!(CONFIG_RELOADS_TOTAL, "result" => "applied").increment(1);

            last_text = text;
        }
        warn!("SIGHUP signal stream ended; config hot-reload offline");
    });
}

/// Rolling reload baseline, carried across SIGHUPs. A fingerprint here is
/// advanced **only** when the corresponding section was actually applied —
/// so a section that was warned restart-required (or whose swap was
/// skipped/failed) keeps its previous baseline and is re-detected on the
/// next reload (and reverting a restart-required edit correctly stops
/// warning). `restart_baseline` is the **startup** fingerprint set and is
/// immutable within a process: the live values of restart-required
/// sections never change without a restart, so every reload compares the
/// new file against startup.
pub(crate) struct ReloadState {
    pub restart_baseline: Vec<(&'static str, String)>,
    pub webhook_fp: String,
    pub webhook_spool: bool,
    pub cdr_fp: String,
    pub cdr_spool: bool,
    pub gateway_fp: String,
}

/// Hot-apply the reload-safe sections of a freshly-loaded config: store
/// the new route table, rebuild + swap the webhook / CDR sinks (unless a
/// durable spool is active for that sink), and rebuild + swap the
/// outbound gateway set (when outbound is enabled and its `limits` are
/// unchanged). Mutates `state` in place — advancing a fingerprint only for
/// a section it actually applied (see [`ReloadState`]) — and returns the
/// restart-required sections that changed (for the warning). Split out
/// from the SIGHUP loop so it's unit-testable without signals.
pub(crate) async fn apply_reload(
    new: Config,
    route_swap: &ArcSwap<RouteSet>,
    webhook_swap: &SwappableWebhookSink,
    cdr_swap: &SwappableCdrSink,
    hep: Option<&HepTelemetry>,
    outbound_reload: Option<&OutboundReload>,
    state: &mut ReloadState,
) -> Vec<&'static str> {
    // Restart-required diff: new file vs the IMMUTABLE startup baseline.
    let new_fp = restart_fingerprints(&new);
    let restart_required: Vec<&'static str> = state
        .restart_baseline
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
    // hot-swapped. The fingerprint/spool baseline advances ONLY on a
    // successful swap, so a restart-required (or failed) change stays
    // detected next time.
    match decide_sink_reload(
        new_webhook_fp != state.webhook_fp,
        state.webhook_spool || new_webhook_spool,
    ) {
        SinkReload::Unchanged => {}
        SinkReload::RestartRequired => {
            warn!("SIGHUP: [webhooks] changed but its durable spool is active; webhook delivery changes require a restart (not hot-applied)")
        }
        SinkReload::Swap => match crate::runtime::build_webhook_sink(webhooks) {
            Ok(sink) => {
                webhook_swap.store(sink);
                state.webhook_fp = new_webhook_fp;
                state.webhook_spool = new_webhook_spool;
            }
            Err(e) => warn!(error = %e, "SIGHUP: rebuilding webhook sink failed; keeping previous"),
        },
    }

    // CDR sink: same change-gated, advance-on-apply logic.
    match decide_sink_reload(new_cdr_fp != state.cdr_fp, state.cdr_spool || new_cdr_spool) {
        SinkReload::Unchanged => {}
        SinkReload::RestartRequired => {
            warn!("SIGHUP: [cdr] changed but its durable spool is active; CDR delivery changes require a restart (not hot-applied)")
        }
        SinkReload::Swap => match crate::runtime::build_cdr_sink(cdr, hep).await {
            Ok(sink) => {
                cdr_swap.store(sink);
                state.cdr_fp = new_cdr_fp;
                state.cdr_spool = new_cdr_spool;
            }
            Err(e) => warn!(error = %e, "SIGHUP: rebuilding CDR sink failed; keeping previous"),
        },
    }

    // Gateways: rebuild + swap the set when outbound is running, its
    // `limits` (cap/rate — restart-required) didn't change, and the
    // gateway set actually differs. The baseline advances ONLY on a
    // successful swap — so a swap skipped because `limits` changed doesn't
    // mask a later real gateway change (it stays detected once limits are
    // reverted). Enable/disable + cap changes ride the `[outbound].limits`
    // restart-required warning.
    if let Some(ob) = outbound_reload {
        let limits_changed = restart_required.contains(&"[outbound].limits");
        if outbound.enabled() && !limits_changed && new_gateway_fp != state.gateway_fp {
            match crate::runtime::build_gateways(&outbound, &ob.deps) {
                Ok(gateways) => {
                    let n = gateways.len();
                    ob.service.reload_gateways(gateways);
                    state.gateway_fp = new_gateway_fp;
                    info!(gateways = n, "SIGHUP: outbound gateways reloaded");
                }
                Err(e) => {
                    warn!(error = %e, "SIGHUP: rebuilding gateways failed; keeping previous set")
                }
            }
        }
    }

    restart_required
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

    /// A fresh rolling state with `c` as the startup baseline.
    fn state(c: &Config) -> ReloadState {
        ReloadState {
            restart_baseline: restart_fingerprints(c),
            webhook_fp: format!("{:?}", c.webhooks),
            webhook_spool: c.webhooks.spool_dir.is_some(),
            cdr_fp: format!("{:?}", c.cdr),
            cdr_spool: c
                .cdr
                .webhook
                .as_ref()
                .and_then(|w| w.spool_dir.as_ref())
                .is_some(),
            gateway_fp: gateway_fingerprint(&c.outbound),
        }
    }

    fn null_sinks() -> (SwappableWebhookSink, SwappableCdrSink) {
        (
            SwappableWebhookSink::new(Arc::new(siphon_ai_webhooks::NullSink)),
            SwappableCdrSink::new(Arc::new(siphon_ai_cdr::NullSink)),
        )
    }

    #[tokio::test]
    async fn apply_reload_swaps_routes() {
        let route_swap = ArcSwap::from_pointee(cfg(BASE).routes);
        let (wh, cd) = null_sinks();
        let mut st = state(&cfg(BASE));
        assert_eq!(route_swap.load().iter().next().unwrap().name, "default");

        let new = cfg(&BASE.replace("name = \"default\"", "name = \"renamed\""));
        let restart_required = apply_reload(new, &route_swap, &wh, &cd, None, None, &mut st).await;

        assert_eq!(route_swap.load().iter().next().unwrap().name, "renamed");
        assert!(
            restart_required.is_empty(),
            "only the route changed; nothing restart-required"
        );
    }

    #[tokio::test]
    async fn apply_reload_flags_restart_required_sections() {
        let route_swap = ArcSwap::from_pointee(cfg(BASE).routes);
        let (wh, cd) = null_sinks();
        let mut st = state(&cfg(BASE));

        // Change the SIP listen port — a restart-required section.
        let new = cfg(&BASE.replace("127.0.0.1:5060", "127.0.0.1:5999"));
        let restart_required = apply_reload(new, &route_swap, &wh, &cd, None, None, &mut st).await;

        assert!(
            restart_required.contains(&"[sip]"),
            "changed listen should be flagged restart-required: {restart_required:?}"
        );
    }

    // Bug 3: the restart-required baseline must NOT advance past an
    // un-applied value — reverting a restart-required edit stops warning.
    #[tokio::test]
    async fn restart_required_edit_then_revert_stops_warning() {
        let route_swap = ArcSwap::from_pointee(cfg(BASE).routes);
        let (wh, cd) = null_sinks();
        let mut st = state(&cfg(BASE));

        // Edit a restart-required section — warned, not applied.
        let edited = cfg(&BASE.replace("127.0.0.1:5060", "127.0.0.1:5999"));
        let rr1 = apply_reload(edited, &route_swap, &wh, &cd, None, None, &mut st).await;
        assert!(rr1.contains(&"[sip]"));

        // Revert it — must NOT warn (baseline stayed at startup).
        let reverted = cfg(BASE);
        let rr2 = apply_reload(reverted, &route_swap, &wh, &cd, None, None, &mut st).await;
        assert!(
            rr2.is_empty(),
            "reverting a restart-required edit must not warn: {rr2:?}"
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

    // Spool warning fires only on a real `[webhooks]` change, not on every
    // reload while a spool is configured.
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

    // Bug 1: every non-hot-reloadable section is fingerprinted, so a change
    // to it surfaces as restart-required instead of silently doing nothing.
    #[test]
    fn restart_fingerprints_cover_all_non_hot_reloadable_sections() {
        let labels: Vec<&str> = restart_fingerprints(&cfg(BASE))
            .into_iter()
            .map(|(l, _)| l)
            .collect();
        for expected in [
            "[node]",
            "[sip]",
            "[media]",
            "[bridge]/[media] defaults",
            "[[trunk]]",
            "[[register]]",
            "[security]",
            "[recording]",
            "[conference]",
            "[park]",
            "[observability]",
            "[admin]",
            "[hep]",
            "[outbound].limits",
        ] {
            assert!(
                labels.contains(&expected),
                "restart fingerprint missing {expected} (silent-drift risk)"
            );
        }
    }

    // Bug 1 (concrete): tightening the inbound `[[trunk]]` allowlist is a
    // restart-required change, not a silent no-op.
    #[test]
    fn trunk_change_is_restart_required() {
        const WITH_TRUNK: &str = r#"
[node]
id = "x"
[sip]
listen = "127.0.0.1:5060"
[bridge]
ws_url = "wss://b/ws"
[[trunk]]
name = "carrier"
peer_addrs = ["203.0.113.10"]
[[route]]
name = "d"
[route.match]
any = true
"#;
        let base = restart_fingerprints(&cfg(WITH_TRUNK));
        let changed = cfg(&WITH_TRUNK.replace("203.0.113.10", "203.0.113.20"));
        let diff: Vec<_> = base
            .iter()
            .zip(&restart_fingerprints(&changed))
            .filter(|(a, b)| a.1 != b.1)
            .map(|(a, _)| a.0)
            .collect();
        assert_eq!(diff, vec!["[[trunk]]"]);
    }

    // Bug 2: rotating a gateway `auth_password` (Some -> Some) changes the
    // gateway fingerprint so the swap actually re-applies it. The
    // fingerprint folds in a HASH, never the cleartext.
    #[test]
    fn gateway_credential_rotation_changes_fingerprint() {
        const WITH_CREDS: &str = r#"
[node]
id = "x"
[sip]
listen = "127.0.0.1:5060"
[bridge]
ws_url = "wss://b/ws"
[outbound]
max_concurrent = 2
[[gateway]]
name = "twilio"
proxy = "sip.twilio.example"
transport = "tls"
from = "sip:+1@x"
auth_username = "u"
auth_password = "secret-OLD"
[[route]]
name = "d"
[route.match]
any = true
"#;
        let base = gateway_fingerprint(&cfg(WITH_CREDS).outbound);
        let rotated =
            gateway_fingerprint(&cfg(&WITH_CREDS.replace("secret-OLD", "secret-NEW")).outbound);
        assert_ne!(
            base, rotated,
            "rotating auth_password must change the gateway fingerprint"
        );
        // Defense-in-depth: the cleartext password is not in the fingerprint.
        assert!(
            !base.contains("secret-OLD"),
            "fingerprint leaked the password"
        );
    }
}
