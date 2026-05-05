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
use tracing::info;
use tracing_subscriber::EnvFilter;

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
    let cli = Cli::parse();
    init_tracing(cli.log.as_deref());

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

    let runtime = Runtime::build(config)
        .await
        .context("runtime build failed")?;

    runtime.run(shutdown_signal()).await
}

/// Initialise the global tracing subscriber.
///
/// Order of precedence for the filter: `--log` flag > `RUST_LOG` env
/// var > built-in default. The default filter pulls siphon-ai
/// crates in at `info` and silences busy upstream logs that don't
/// add operator value at default verbosity.
fn init_tracing(cli_filter: Option<&str>) {
    const DEFAULT: &str = "siphon_ai=info,siphon_ai_core=info,siphon_ai_media_glue=info,\
         siphon_ai_sip_glue=info,siphon_ai_bridge=info,siphon_ai_routes=info,\
         siphon_ai_config=info,sip_uas=warn,sip_transaction=warn,\
         sip_transport=warn,forge=warn";

    let env_filter = match cli_filter {
        Some(f) => EnvFilter::try_new(f).unwrap_or_else(|_| EnvFilter::new(DEFAULT)),
        None => EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(DEFAULT)),
    };

    let _ = tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_target(true)
        .try_init();
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
