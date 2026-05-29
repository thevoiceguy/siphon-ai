//! `siphon-ai` daemon entry point.
//!
//! Responsibilities:
//! 1. Parse CLI / env, load and compile the TOML config.
//! 2. Initialise tracing.
//! 3. Build the runtime (binds UDP, spawns listeners) and run it
//!    until SIGINT / SIGTERM.
//!
//! The actual wiring lives in [`runtime::Runtime`]; this module is
//! the thin shell that bridges process startup into a `Runtime`.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use siphon_ai::Runtime;
use siphon_ai_telemetry::LogFilterHandle;
use tracing::info;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

#[derive(Parser, Debug)]
#[command(name = "siphon-ai", version, about = "SIP-to-WebSocket media bridge")]
struct Cli {
    /// Path to the TOML configuration file.
    #[arg(long, short, env = "SIPHON_AI_CONFIG")]
    config: PathBuf,

    /// Override the tracing filter (`siphon_ai=debug,siphon=info`).
    /// Defaults to `RUST_LOG` if set, or the built-in default
    /// otherwise.
    #[arg(long, env = "SIPHON_AI_LOG")]
    log: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Install rustls' process-wide `CryptoProvider` before any TLS
    // code path runs. Required from rustls 0.23 onward whenever the
    // dep graph contains more than one provider crate — ours pulls
    // both `aws-lc-rs` and `ring` transitively via different
    // upstreams. Without this, enabling `[sip.tls]` panics on
    // startup with:
    //     "Could not automatically determine the process-level
    //      CryptoProvider from Rustls crate features."
    // `aws_lc_rs` is the BoringSSL-derived modern default; `.ok()`
    // makes the call idempotent so a test harness that already
    // installed a provider doesn't break the second install.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    let cli = Cli::parse();
    let log_filter = init_tracing(cli.log.as_deref());

    info!(config = %cli.config.display(), "loading configuration");
    let config = siphon_ai_config::load_from_path(&cli.config)
        .with_context(|| format!("load config {}", cli.config.display()))?;

    info!(
        node_id = %config.node.id,
        sip_listen = %config.sip.listen_addr,
        public_address = %config.node.public_address,
        routes = config.routes.len(),
        "configuration compiled",
    );

    let runtime = Runtime::build(config, log_filter)
        .await
        .context("runtime build failed")?;

    runtime.run(shutdown_signal()).await
}

/// Initialise the global tracing subscriber and return a reload
/// handle the admin endpoint uses to swap the filter at runtime.
///
/// Order of precedence for the filter: `--log` flag > `RUST_LOG` env
/// var > built-in default. The default filter pulls siphon-ai
/// crates in at `info` and silences busy upstream logs that don't
/// add operator value at default verbosity.
///
/// Implementation note: we build the subscriber as
/// `Registry → reload(EnvFilter) → fmt-layer` rather than the
/// shorthand `tracing_subscriber::fmt()` builder, because the
/// shorthand doesn't expose a reload handle. The layered form is
/// the canonical way to make `EnvFilter` mutable at runtime.
fn init_tracing(cli_filter: Option<&str>) -> LogFilterHandle {
    const DEFAULT: &str = "siphon_ai=info,siphon_ai_core=info,siphon_ai_media_glue=info,\
         siphon_ai_sip_glue=info,siphon_ai_bridge=info,siphon_ai_routes=info,\
         siphon_ai_config=info,sip_uas=warn,sip_transaction=warn,\
         sip_transport=warn,forge=warn";

    let env_filter = match cli_filter {
        Some(f) => EnvFilter::try_new(f).unwrap_or_else(|_| EnvFilter::new(DEFAULT)),
        None => EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(DEFAULT)),
    };

    let (filter_layer, reload_handle) = tracing_subscriber::reload::Layer::new(env_filter);
    // `fmt::layer()` defaults to ANSI on regardless of stdout type
    // — unlike the higher-level `fmt::Subscriber::builder()` which
    // does tty auto-detection. Without the explicit `with_ansi`
    // call, every log line under systemd lands in journald with
    // embedded `\x1b[..m` escape sequences. That's harmless to
    // human readers (journalctl strips them on display) but breaks
    // every downstream consumer that does string matching against
    // the journal — most importantly the fail2ban `<HOST>` extractor
    // for our trunk-rejection regex, which silently never matches.
    let fmt_layer = tracing_subscriber::fmt::layer()
        .with_target(true)
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stdout()));
    // `try_init` so tests that initialise the subscriber separately
    // don't crash this process; the second init is a noop. The
    // reload handle works either way because the layer is part of
    // the subscriber, not a global cell.
    let _ = tracing_subscriber::registry()
        .with(filter_layer)
        .with(fmt_layer)
        .try_init();

    LogFilterHandle::new(reload_handle)
}

/// Resolve when SIGINT (Ctrl-C) or SIGTERM is received. On Windows
/// only SIGINT is observable; SIGTERM is a Unix concept.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut sigterm = signal(SignalKind::terminate()).expect("install SIGTERM handler");
        let mut sigint = signal(SignalKind::interrupt()).expect("install SIGINT handler");
        tokio::select! {
            _ = sigterm.recv() => info!("received SIGTERM"),
            _ = sigint.recv() => info!("received SIGINT"),
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
        info!("received Ctrl-C");
    }
}
