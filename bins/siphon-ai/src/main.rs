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

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand};
use siphon_ai::{OtelActivation, Runtime};
use siphon_ai_config::Config;
use siphon_ai_telemetry::LogFilterHandle;
use tracing::info;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter, Layer as _};

mod inspect;

#[derive(Parser, Debug)]
#[command(name = "siphon-ai", version, about = "SIP-to-WebSocket media bridge")]
struct Cli {
    /// Path to the TOML configuration file. Required to run the
    /// daemon and by every subcommand. `global` so it can appear
    /// before or after the subcommand — `siphon-ai --config X check`
    /// and `siphon-ai check --config X` both work.
    #[arg(long, short, env = "SIPHON_AI_CONFIG", global = true)]
    config: Option<PathBuf>,

    /// Override the tracing filter (`siphon_ai=debug,siphon=info`).
    /// Defaults to `RUST_LOG` if set, or the built-in default
    /// otherwise. Only affects running the daemon.
    #[arg(long, env = "SIPHON_AI_LOG", global = true)]
    log: Option<String>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Validate and compile the config file, then exit — without
    /// starting the daemon or binding any sockets. Exit code 0 if the
    /// config is valid, 1 otherwise. Safe as a pre-deploy / CI
    /// preflight (e.g. before `systemctl reload`).
    Check,

    /// Print the effective compiled configuration (post-`${VAR}`,
    /// post per-route merge) and exit. Secrets are redacted unless
    /// `--show-secrets` is passed.
    PrintConfig {
        /// Reveal secret values (auth headers, signing secrets,
        /// passwords) instead of `<redacted>`.
        #[arg(long)]
        show_secrets: bool,
    },

    /// Report which route a synthetic call matches (first-match-wins)
    /// and its effective bridge config, then exit. Unset attributes
    /// default to empty / `trunk`.
    RouteTest {
        /// Request-URI user (the dialed number on the RURI).
        #[arg(long = "ruri-user", default_value = "")]
        ruri_user: String,
        /// Request-URI host.
        #[arg(long = "ruri-host", default_value = "")]
        ruri_host: String,
        /// To-header user (dialed number). Also used for `--ruri-user`
        /// when that is left empty.
        #[arg(long, default_value = "")]
        to: String,
        /// To-header host.
        #[arg(long = "to-host", default_value = "")]
        to_host: String,
        /// From-header user (caller).
        #[arg(long, default_value = "")]
        from: String,
        /// From-header host.
        #[arg(long = "from-host", default_value = "")]
        from_host: String,
        /// `register_source`: `trunk` (unregistered inbound) or a
        /// `[[register]]` block name.
        #[arg(long = "register-source", default_value = "trunk")]
        register_source: String,
        /// Repeatable SIP header, `Name: Value`, matched against
        /// `[route.match].header.*`.
        #[arg(long = "header", short = 'H')]
        headers: Vec<String>,
    },

    /// Decrypt an encrypted recording (`.wava`, 0.24.0) into a playable
    /// WAV and exit. Offline tooling — needs the KEK file only, not the
    /// daemon config.
    DecryptRecording {
        /// The `.wava` file to decrypt (a crashed capture's
        /// `.wava.part` works with `--allow-unfinalized`).
        file: PathBuf,
        /// File holding the KEK as 64 hex chars — the same secret
        /// `[recording.encryption].kek` references.
        #[arg(long = "kek-file")]
        kek_file: PathBuf,
        /// Output path. Defaults to the input with a `.wav` extension.
        #[arg(long, short)]
        out: Option<PathBuf>,
        /// Recover an unfinalized capture: accept a generation-0 chunk 0.
        /// The output WAV then has placeholder (zero) size fields in its
        /// header; most tools still play it after `ffmpeg -i in.wav out.wav`
        /// or similar re-muxing.
        #[arg(long)]
        allow_unfinalized: bool,
    },
}

/// The config path, from `--config` or `$SIPHON_AI_CONFIG`. A clap
/// `global` arg can't be `required`, so enforce presence here.
fn config_path(cli: &Cli) -> Result<PathBuf> {
    cli.config
        .clone()
        .ok_or_else(|| anyhow!("--config <PATH> is required (or set SIPHON_AI_CONFIG)"))
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

    // Install tracing before anything that loads config — including the
    // read-only subcommands. `siphon-ai check` must surface the same
    // load-time `warn!`s the daemon prints at boot (e.g. the
    // SRTP-key-in-cleartext footgun); without a subscriber those are
    // silently dropped and the preflight is less informative than a real
    // boot.
    let (log_filter, otel_activation) = init_tracing(cli.log.as_deref());

    // Read-only subcommands dispatch here and exit — no socket binding,
    // no runtime (but tracing is live, so config warnings show).
    if let Some(command) = &cli.command {
        // decrypt-recording is pure offline tooling: it takes its key
        // material directly and must work on a box with no daemon config.
        if let Command::DecryptRecording {
            file,
            kek_file,
            out,
            allow_unfinalized,
        } = command
        {
            run_decrypt_recording(file, kek_file, out.as_deref(), *allow_unfinalized);
        }
        let path = config_path(&cli)?;
        match command {
            Command::Check => run_check(&path),
            Command::PrintConfig { show_secrets } => run_print_config(&path, *show_secrets),
            Command::RouteTest {
                ruri_user,
                ruri_host,
                to,
                to_host,
                from,
                from_host,
                register_source,
                headers,
            } => run_route_test(
                &path,
                inspect::RouteTestInput {
                    // RURI user defaults to the To-user when unset — a
                    // common case where they're the same dialed number.
                    ruri_user: if ruri_user.is_empty() {
                        to.clone()
                    } else {
                        ruri_user.clone()
                    },
                    ruri_host: ruri_host.clone(),
                    to_user: to.clone(),
                    to_host: to_host.clone(),
                    from_user: from.clone(),
                    from_host: from_host.clone(),
                    register_source: register_source.clone(),
                    headers: parse_headers(headers)?,
                },
            ),
            // Dispatched above, before the config-path requirement.
            Command::DecryptRecording { .. } => unreachable!("handled before config load"),
        }
    }

    // No subcommand → run the daemon. (`log_filter` / tracing already
    // installed above.)
    let config_path = config_path(&cli)?;

    info!(config = %config_path.display(), "loading configuration");
    let config = siphon_ai_config::load_from_path(&config_path)
        .with_context(|| format!("load config {}", config_path.display()))?;

    info!(
        node_id = %config.node.id,
        sip_listen = %config.sip.listen_addr,
        public_address = %config.node.public_address,
        routes = config.routes.len(),
        "configuration compiled",
    );

    // Pass the path so SIGHUP (`systemctl reload`) can re-read it for
    // hot reload of the reload-safe sections.
    let runtime =
        Runtime::build_with_reload(config, Some(config_path), log_filter, Some(otel_activation))
            .await
            .context("runtime build failed")?;

    runtime.run(shutdown_signal()).await
}

/// Load + compile a config for a read-only subcommand, or print the
/// validation error to stderr and exit 1. Shared by `check`,
/// `print-config`, and `route-test`.
fn load_or_exit(path: &Path) -> Config {
    match siphon_ai_config::load_from_path(path) {
        Ok(config) => config,
        Err(e) => {
            eprintln!("config INVALID: {}", path.display());
            eprintln!("  {e}");
            std::process::exit(1);
        }
    }
}

/// `siphon-ai check` — validate + compile, print a one-screen summary
/// (exit 0) or the error (exit 1). Never starts the daemon.
fn run_check(path: &Path) -> ! {
    let config = load_or_exit(path);
    print_check_summary(path, &config);
    std::process::exit(0);
}

/// `siphon-ai print-config` — render the effective compiled config
/// (secrets redacted unless `show_secrets`) and exit.
fn run_print_config(path: &Path, show_secrets: bool) -> ! {
    let config = load_or_exit(path);
    print!("{}", inspect::render_config(&config, show_secrets));
    std::process::exit(0);
}

/// `siphon-ai route-test` — report the matched route + effective bridge
/// config for the synthetic call, and exit.
fn run_route_test(path: &Path, input: inspect::RouteTestInput) -> ! {
    let config = load_or_exit(path);
    print!("{}", inspect::route_test(&config, &input));
    std::process::exit(0);
}

/// `siphon-ai decrypt-recording` — unseal a `.wava` into a playable WAV
/// and exit (0.24.0 tooling; format spec in `docs/RECORDING.md`).
fn run_decrypt_recording(
    file: &Path,
    kek_file: &Path,
    out: Option<&Path>,
    allow_unfinalized: bool,
) -> ! {
    match decrypt_recording(file, kek_file, out, allow_unfinalized) {
        Ok((out_path, bytes)) => {
            println!(
                "decrypted {} → {} ({bytes} WAV bytes)",
                file.display(),
                out_path.display()
            );
            std::process::exit(0);
        }
        Err(err) => {
            eprintln!("error: {err:#}");
            std::process::exit(1);
        }
    }
}

fn decrypt_recording(
    file: &Path,
    kek_file: &Path,
    out: Option<&Path>,
    allow_unfinalized: bool,
) -> Result<(PathBuf, u64)> {
    use std::io::{Seek, SeekFrom, Write};

    let kek_hex = std::fs::read_to_string(kek_file)
        .with_context(|| format!("read KEK file {}", kek_file.display()))?;
    let mut input = std::io::BufReader::new(
        std::fs::File::open(file).with_context(|| format!("open {}", file.display()))?,
    );
    // The container names the KEK that wrapped it; surface that id in
    // errors (so the operator knows *which* retired KEK to fetch) and use
    // it for the supplied key.
    let key_id = siphon_ai_recording::peek_key_id(&mut input)
        .with_context(|| format!("{} is not a readable encrypted recording", file.display()))?;
    input.seek(SeekFrom::Start(0)).context("rewind input")?;
    let kek = siphon_ai_recording::Kek::from_hex(&kek_hex, key_id.clone()).with_context(|| {
        format!(
            "KEK file {} (recording needs key_id {key_id:?})",
            kek_file.display()
        )
    })?;

    let out_path = out
        .map(Path::to_path_buf)
        .unwrap_or_else(|| file.with_extension("wav"));
    if out_path == file {
        return Err(anyhow!("output path equals input; pass --out"));
    }
    let mut out_file = std::io::BufWriter::new(
        std::fs::File::create(&out_path)
            .with_context(|| format!("create {}", out_path.display()))?,
    );
    let bytes = siphon_ai_recording::decrypt(input, &mut out_file, &kek, allow_unfinalized)
        .with_context(|| {
            format!(
                "decrypt {} (wrapped with key_id {key_id:?})",
                file.display()
            )
        })?;
    out_file.flush().context("flush output")?;
    Ok((out_path, bytes))
}

/// Parse `--header 'Name: Value'` flags into `(name, value)` pairs.
fn parse_headers(raw: &[String]) -> Result<Vec<(String, String)>> {
    raw.iter()
        .map(|h| {
            let (k, v) = h
                .split_once(':')
                .ok_or_else(|| anyhow!("bad --header {h:?}; expected 'Name: Value'"))?;
            Ok((k.trim().to_string(), v.trim().to_string()))
        })
        .collect()
}

/// One-screen summary of a valid compiled config — what the daemon
/// would run with. A missing default route warns (matching the
/// daemon's startup warning) but does not fail the check.
fn print_check_summary(path: &Path, config: &Config) {
    use std::fmt::Write as _;

    let transports = config
        .sip
        .transports
        .iter()
        .map(|t| match t {
            siphon_ai_config::SipTransport::Udp => "udp",
            siphon_ai_config::SipTransport::Tcp => "tcp",
            siphon_ai_config::SipTransport::Tls => "tls",
        })
        .collect::<Vec<_>>()
        .join(", ");

    // Optional subsystems that are switched on.
    let mut enabled: Vec<String> = Vec::new();
    if config.outbound.max_concurrent > 0 && !config.outbound.gateways.is_empty() {
        enabled.push(format!(
            "outbound({} gateway(s))",
            config.outbound.gateways.len()
        ));
    }
    if !matches!(config.recording.mode, siphon_ai_config::RecordingMode::Off) {
        enabled.push("recording".into());
    }
    if config.cdr.enabled {
        let mut sinks = Vec::new();
        if config.cdr.file.is_some() {
            sinks.push("file");
        }
        if config.cdr.webhook.is_some() {
            sinks.push("webhook");
        }
        enabled.push(format!("cdr({})", sinks.join("+")));
    }
    if config.webhooks.enabled {
        enabled.push("webhooks".into());
    }
    if config.audit.enabled {
        let mut sinks = Vec::new();
        if config.audit.file.is_some() {
            sinks.push("file");
        }
        if config.audit.webhook.is_some() {
            sinks.push("webhook");
        }
        enabled.push(format!("audit({})", sinks.join("+")));
    }
    if config.conference.enabled {
        enabled.push("conference".into());
    }
    if config.park.enabled {
        enabled.push("park".into());
    }
    if config.hep.enabled {
        enabled.push("hep".into());
    }
    if config.admin.is_some() {
        enabled.push("admin".into());
    }
    if config.security.stir_shaken.enabled {
        enabled.push("stir_shaken".into());
    }

    let mut out = String::new();
    let _ = writeln!(out, "config OK: {}", path.display());
    let _ = writeln!(out, "  node id:       {}", config.node.id);
    let _ = writeln!(
        out,
        "  sip listen:    {} [{}]",
        config.sip.listen_addr, transports
    );
    let _ = writeln!(out, "  public addr:   {}", config.node.public_address);
    let default = if config.routes.has_default() {
        "yes"
    } else {
        "NO — add a final `any = true` route"
    };
    let _ = writeln!(
        out,
        "  routes:        {} (default route: {default})",
        config.routes.len()
    );
    let _ = writeln!(
        out,
        "  registrations: {}    trunks: {}",
        config.registrations.len(),
        config.trunks.len()
    );
    let _ = writeln!(
        out,
        "  enabled:       {}",
        if enabled.is_empty() {
            "(none)".to_string()
        } else {
            enabled.join(", ")
        }
    );
    print!("{out}");

    if !config.routes.has_default() {
        eprintln!("warning: no default route (`any = true`) — calls matching no route get SIP 404");
    }
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
fn init_tracing(cli_filter: Option<&str>) -> (LogFilterHandle, OtelActivation) {
    const DEFAULT: &str = "siphon_ai=info,siphon_ai_core=info,siphon_ai_media_glue=info,\
         siphon_ai_sip_glue=info,siphon_ai_bridge=info,siphon_ai_routes=info,\
         siphon_ai_config=info,sip_uas=warn,sip_transaction=warn,\
         sip_transport=warn,forge=warn";

    let env_filter = match cli_filter {
        Some(f) => EnvFilter::try_new(f).unwrap_or_else(|_| EnvFilter::new(DEFAULT)),
        None => EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(DEFAULT)),
    };

    let (filter_layer, reload_handle) = tracing_subscriber::reload::Layer::new(env_filter);

    // OTLP trace layer, installed **concrete** with a reloadable per-layer
    // filter that starts `OFF`. The real OTLP tracer isn't known until config
    // loads (it carries the endpoint), and `init_tracing` runs before that so
    // config-load warnings still print — `LazyGlobalTracer` defers the global
    // tracer lookup to the first span build, and the `OFF` filter keeps the
    // layer at zero per-span cost while OTLP is disabled (the common case).
    // The runtime installs the global OTLP provider and then calls
    // `OtelActivation::activate`, which opens the filter.
    //
    // The layer itself must NOT sit behind `reload::Layer`: W3C trace
    // propagation (0.23.0) extracts span context via
    // `OpenTelemetrySpanExt::context()`, whose `WithContext` downcast
    // `reload` refuses to forward — spans would export but extraction would
    // silently return no context. See `otel.rs` for the full story.
    let (otel_filter, otel_filter_handle) =
        tracing_subscriber::reload::Layer::new(tracing_subscriber::filter::LevelFilter::OFF);
    let otel_layer = tracing_opentelemetry::layer()
        .with_tracer(siphon_ai::otel::LazyGlobalTracer::default())
        .with_filter(otel_filter);
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
        .with(otel_layer)
        .with(fmt_layer)
        .try_init();

    // Deferred activation: open the OTLP layer's filter. The runtime calls
    // this after installing the OTLP provider, so `LazyGlobalTracer`'s first
    // span build binds to the real provider, never the no-op default.
    let activation = OtelActivation::new(Box::new(move || {
        otel_filter_handle.reload(tracing_subscriber::filter::LevelFilter::TRACE)
    }));

    (LogFilterHandle::new(reload_handle), activation)
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
