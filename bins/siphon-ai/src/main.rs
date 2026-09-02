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

    /// Log output format: `text` (human-readable, the default) or
    /// `json` (one JSON object per line, for a log shipper). Orthogonal
    /// to `--log`, which selects *what* is logged, not how it is
    /// rendered.
    #[arg(long = "log-format", env = "SIPHON_AI_LOG_FORMAT", value_enum,
          default_value_t = LogFormat::Text, global = true)]
    log_format: LogFormat,

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
        /// Output format: `text` (human-readable, the default) or
        /// `json` (pretty-printed, for `jq` / deploy diffing).
        #[arg(long, value_enum, default_value_t = PrintFormat::Text)]
        format: PrintFormat,
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
        /// `[recording.encryption].kek` references. Mutually exclusive
        /// with `--kms-region`.
        #[arg(long = "kek-file", conflicts_with = "kms_region")]
        kek_file: Option<PathBuf>,
        /// Unwrap the recording's data key via AWS KMS instead of a
        /// local KEK file (`[recording.encryption.kms]` recordings).
        /// Credentials come from `AWS_ACCESS_KEY_ID` /
        /// `AWS_SECRET_ACCESS_KEY`; the ciphertext blob names its own
        /// KMS key, so no key ARN is needed.
        #[arg(long = "kms-region")]
        kms_region: Option<String>,
        /// Override the KMS endpoint (LocalStack / KMS-compatible
        /// emulators). Defaults to the public AWS endpoint for the
        /// region.
        #[arg(long = "kms-endpoint", requires = "kms_region")]
        kms_endpoint: Option<String>,
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

/// `print-config` output format. JSON is an inspection view (nulls
/// for unset, `"<redacted>"` strings), not a loadable config.
#[derive(clap::ValueEnum, Clone, Copy, Debug)]
enum PrintFormat {
    Text,
    Json,
}

/// How the daemon renders log events (`--log-format`).
///
/// This picks the *formatter*, never the filter — `--log` /
/// `SIPHON_AI_LOG` still decide which events are emitted, and
/// `PUT /admin/v1/log` still retunes that at runtime. It also does not
/// touch the read-only subcommands' report output: `check`,
/// `print-config` and `route-test` write their summaries straight to
/// stdout, not through `tracing`, so those stay human-readable in both
/// modes. What the flag *does* change for a subcommand is the
/// rendering of the load-time `warn!`s it surfaces.
#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
enum LogFormat {
    /// The default: one human-readable line per event, ANSI-coloured
    /// when stdout is a terminal.
    Text,
    /// One JSON object per line, event fields flattened to the top
    /// level. For OpenTelemetry Collector / Fluent Bit / Vector /
    /// Promtail — anything that indexes fields rather than text.
    Json,
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
    let (log_filter, otel_activation) = init_tracing(cli.log.as_deref(), cli.log_format);

    // Read-only subcommands dispatch here and exit — no socket binding,
    // no runtime (but tracing is live, so config warnings show).
    if let Some(command) = &cli.command {
        // decrypt-recording is pure offline tooling: it takes its key
        // material directly and must work on a box with no daemon config.
        if let Command::DecryptRecording {
            file,
            kek_file,
            kms_region,
            kms_endpoint,
            out,
            allow_unfinalized,
        } = command
        {
            run_decrypt_recording(
                file,
                kek_file.as_deref(),
                kms_region.as_deref(),
                kms_endpoint.as_deref(),
                out.as_deref(),
                *allow_unfinalized,
            )
            .await;
        }
        let path = config_path(&cli)?;
        match command {
            Command::Check => run_check(&path),
            Command::PrintConfig {
                show_secrets,
                format,
            } => run_print_config(&path, *show_secrets, *format),
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
    // `check` is the pre-upgrade gate, so it must fail on everything
    // startup would fail on — including a config whose features this
    // binary was not built with. Compiling cleanly is necessary but
    // not sufficient (DEV_PLAN_WebRTC.md Phase 2 §4.5).
    if let Err(e) = siphon_ai::runtime::check_build_features(&config) {
        eprintln!("config error: {e:#}");
        std::process::exit(1);
    }
    print_check_summary(path, &config);
    std::process::exit(0);
}

/// `siphon-ai print-config` — render the effective compiled config
/// (secrets redacted unless `show_secrets`) and exit.
fn run_print_config(path: &Path, show_secrets: bool, format: PrintFormat) -> ! {
    let config = load_or_exit(path);
    let rendered = match format {
        PrintFormat::Text => inspect::render_config(&config, show_secrets),
        PrintFormat::Json => inspect::render_config_json(&config, show_secrets),
    };
    print!("{rendered}");
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
/// and exit (0.24.0 tooling, KMS mode 0.25.0; format spec in
/// `docs/RECORDING.md`).
async fn run_decrypt_recording(
    file: &Path,
    kek_file: Option<&Path>,
    kms_region: Option<&str>,
    kms_endpoint: Option<&str>,
    out: Option<&Path>,
    allow_unfinalized: bool,
) -> ! {
    match decrypt_recording(
        file,
        kek_file,
        kms_region,
        kms_endpoint,
        out,
        allow_unfinalized,
    )
    .await
    {
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

async fn decrypt_recording(
    file: &Path,
    kek_file: Option<&Path>,
    kms_region: Option<&str>,
    kms_endpoint: Option<&str>,
    out: Option<&Path>,
    allow_unfinalized: bool,
) -> Result<(PathBuf, u64)> {
    use std::io::{Seek, SeekFrom, Write};

    let mut input = std::io::BufReader::new(
        std::fs::File::open(file).with_context(|| format!("open {}", file.display()))?,
    );
    // The container names the KEK that wrapped it; surface that id in
    // errors (so the operator knows *which* retired KEK to fetch) and use
    // it for the supplied key.
    let key_id = siphon_ai_recording::peek_key_id(&mut input)
        .with_context(|| format!("{} is not a readable encrypted recording", file.display()))?;
    input.seek(SeekFrom::Start(0)).context("rewind input")?;
    let kek = match (kek_file, kms_region) {
        (Some(kek_file), None) => {
            let kek_hex = std::fs::read_to_string(kek_file)
                .with_context(|| format!("read KEK file {}", kek_file.display()))?;
            siphon_ai_recording::Kek::from_hex(&kek_hex, key_id.clone()).with_context(|| {
                format!(
                    "KEK file {} (recording needs key_id {key_id:?})",
                    kek_file.display()
                )
            })?
        }
        (None, Some(region)) => {
            let creds = siphon_ai_http::sigv4::SigV4Credentials {
                access_key: std::env::var("AWS_ACCESS_KEY_ID")
                    .context("AWS_ACCESS_KEY_ID is required with --kms-region")?,
                secret_key: std::env::var("AWS_SECRET_ACCESS_KEY")
                    .context("AWS_SECRET_ACCESS_KEY is required with --kms-region")?,
            };
            let client = siphon_ai_http::kms::KmsClient::new(
                region.to_string(),
                creds,
                kms_endpoint.map(str::to_string),
            );
            // Empty key_id ⇒ skip the id check: KMS Decrypt resolves the
            // key from the blob itself; the container id is informational.
            siphon_ai_recording::Kek::AwsKms {
                client,
                key_arn: String::new(),
                key_id: String::new(),
            }
        }
        _ => {
            return Err(anyhow!(
                "pass exactly one of --kek-file or --kms-region                  (recording names key_id {key_id:?})"
            ))
        }
    };

    // Sealed extensions map to their payload format: .wava → .wav,
    // .opusa → .opus.
    let default_ext = match file.extension().and_then(|e| e.to_str()) {
        Some("opusa") => "opus",
        _ => "wav",
    };
    let out_path = out
        .map(Path::to_path_buf)
        .unwrap_or_else(|| file.with_extension(default_ext));
    if out_path == file {
        return Err(anyhow!("output path equals input; pass --out"));
    }
    let mut out_file = std::io::BufWriter::new(
        std::fs::File::create(&out_path)
            .with_context(|| format!("create {}", out_path.display()))?,
    );
    let bytes = siphon_ai_recording::decrypt(input, &mut out_file, &kek, allow_unfinalized)
        .await
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
            siphon_ai_config::SipTransport::Ws => "ws",
            siphon_ai_config::SipTransport::Wss => "wss",
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
    if config.quality.enabled {
        let mut sinks = Vec::new();
        if config.quality.file.is_some() {
            sinks.push("file");
        }
        if config.quality.webhook.is_some() {
            sinks.push("webhook");
        }
        enabled.push(format!("quality({})", sinks.join("+")));
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

/// The daemon's built-in log filter, used when neither `--log` nor
/// `RUST_LOG` supplies one.
///
/// The leading bare `warn` is a global floor, and it is the whole
/// point of this directive's shape. Without it `EnvFilter` treats the
/// list as an allowlist and *discards every unlisted target entirely*.
///
/// `EnvFilter` matches targets by **prefix**, so `siphon_ai=info`
/// already covers every first-party `siphon_ai_*` crate — those were
/// never muted. What the old allowlist did drop was everything whose
/// target matched no listed prefix: `hep_rs`, and the siphon-rs crates
/// that aren't `sip_uas` / `sip_transaction` / `sip_transport`
/// (`sip_uac`, `sip_dialog`, `sip_auth`, …), plus every third-party
/// dependency. Two of those matter in practice, confirmed against
/// production on 0.48.11: `hep_rs`'s "collector unreachable" warning
/// (#460) and `sip_uac`'s registration-auth warnings both went from
/// **zero** log lines in three days to firing normally once the floor
/// was in place.
///
/// A floor makes the filter fail-safe: a new crate, or a dependency,
/// can never be silently muted; it can only be turned *up* from warn
/// by naming it below.
///
/// Upstream `sip_*` / `forge*` / `hep_rs` no longer need explicit
/// entries — the floor already puts them exactly where their old
/// `=warn` directives did, without the drift risk of a list.
const DEFAULT_LOG_FILTER: &str = "warn,\
     siphon_ai=info,siphon_ai_core=info,siphon_ai_media_glue=info,\
     siphon_ai_sip_glue=info,siphon_ai_bridge=info,siphon_ai_routes=info,\
     siphon_ai_config=info,siphon_ai_telemetry=info,siphon_ai_cdr=info,\
     siphon_ai_http=info,siphon_ai_recording=info,siphon_ai_audit=info,\
     siphon_ai_quality=info,siphon_ai_webhooks=info,siphon_ai_security=info,\
     siphon_ai_stir_shaken=info";

/// Initialise the global tracing subscriber and return a reload
/// handle the admin endpoint uses to swap the filter at runtime.
///
/// Order of precedence for the filter: `--log` flag > `RUST_LOG` env
/// var > built-in default. The default filter puts a `warn` floor
/// under everything — so no crate's warnings can be lost by omission
/// — and lifts the siphon-ai crates to `info` on top of it.
///
/// Note the precedence is *replace*, not merge: a `--log` or
/// `RUST_LOG` value supplies the whole directive, floor included. An
/// operator narrowing to one target (`RUST_LOG=siphon_ai_core=debug`)
/// therefore also drops the floor and mutes everything else — add a
/// leading `warn,` to keep it.
///
/// Implementation note: we build the subscriber as
/// `Registry → reload(EnvFilter) → fmt-layer` rather than the
/// shorthand `tracing_subscriber::fmt()` builder, because the
/// shorthand doesn't expose a reload handle. The layered form is
/// the canonical way to make `EnvFilter` mutable at runtime.
fn init_tracing(
    cli_filter: Option<&str>,
    log_format: LogFormat,
) -> (LogFilterHandle, OtelActivation) {
    const DEFAULT: &str = DEFAULT_LOG_FILTER;

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
    // The formatting layer, `text` (default) or `json`. The two arms
    // have different types, so both are boxed into one
    // `Box<dyn Layer<_>>` before joining the registry.
    //
    // `text` is deliberately the default and is byte-for-byte what the
    // daemon has always emitted: it is the right output for
    // `journalctl` on one node, and — see the `with_ansi` note below
    // — for the fail2ban regex that reads the journal. `json` is for
    // the case a text line cannot serve: a shipper that indexes
    // *fields*, where `call_id` / `route` / `from_user` should arrive
    // as queryable keys instead of substrings some regex has to pick
    // back out (and silently stops picking out the day a call site
    // gains a field).
    let fmt_layer: Box<dyn tracing_subscriber::Layer<_> + Send + Sync> = match log_format {
        // `fmt::layer()` defaults to ANSI on regardless of stdout type
        // — unlike the higher-level `fmt::Subscriber::builder()` which
        // does tty auto-detection. Without the explicit `with_ansi`
        // call, every log line under systemd lands in journald with
        // embedded `\x1b[..m` escape sequences. That's harmless to
        // human readers (journalctl strips them on display) but breaks
        // every downstream consumer that does string matching against
        // the journal — most importantly the fail2ban `<HOST>` extractor
        // for our trunk-rejection regex, which silently never matches.
        LogFormat::Text => Box::new(
            tracing_subscriber::fmt::layer()
                .with_target(true)
                .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stdout())),
        ),
        // No `with_ansi` here on purpose rather than `with_ansi(false)`:
        // the JSON writer never emits escape sequences, so passing the
        // flag would only suggest it might.
        //
        // `flatten_event` puts the event's own fields at the top level
        // instead of nesting them under `fields`, which is what a
        // collector can index without a transform. `with_current_span`
        // keeps the innermost span's fields — that is where `call_id`
        // lives, since every per-call function is `#[instrument(fields(call_id))]`.
        // `with_span_list(false)` drops the full ancestor chain, which
        // would repeat those same fields on every line and put no bound
        // on line size.
        LogFormat::Json => Box::new(
            tracing_subscriber::fmt::layer()
                .json()
                .flatten_event(true)
                .with_current_span(true)
                .with_span_list(false)
                .with_target(true),
        ),
    };
    // `try_init` so tests that initialise the subscriber separately
    // don't crash this process; the second init is a noop. The
    // reload handle works either way because the layer is part of
    // the subscriber, not a global cell.
    // Recent-errors ring (0.49.0, DESIGN_SIGHTGLASS.md §6.1): capture
    // warn/error into the in-memory ring behind `GET /admin/v1/errors`.
    // Installed here — before config loads — so config-load warnings are
    // themselves captured; the runtime applies the configured capacity
    // later. The per-layer WARN filter keeps it at zero cost for the
    // info/debug firehose. Caveat: the reloadable `filter_layer` above
    // is a global filter gating every layer, so an operator narrowing
    // the directive below WARN (`PUT /admin/v1/log` with `off`, or a
    // target filter that drops a crate entirely) also narrows what the
    // ring sees — documented in CONFIG.md.
    let ring_layer = siphon_ai_telemetry::error_ring::ErrorRingLayer
        .with_filter(tracing_subscriber::filter::LevelFilter::WARN);
    let _ = tracing_subscriber::registry()
        .with(filter_layer)
        .with(otel_layer)
        .with(fmt_layer)
        .with(ring_layer)
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

#[cfg(test)]
mod tests {
    use std::io;
    use std::sync::{Arc, Mutex};

    use super::DEFAULT_LOG_FILTER;
    use tracing_subscriber::fmt::MakeWriter;
    use tracing_subscriber::EnvFilter;

    /// Collects formatted events so a test can assert on what a
    /// subscriber actually emitted.
    #[derive(Clone, Default)]
    struct Buffer(Arc<Mutex<Vec<u8>>>);

    impl Buffer {
        fn contents(&self) -> String {
            String::from_utf8_lossy(&self.0.lock().expect("buffer poisoned")).into_owned()
        }
    }

    impl io::Write for Buffer {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0
                .lock()
                .expect("buffer poisoned")
                .extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for Buffer {
        type Writer = Self;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    /// Emit one event under `directive` and report whether it survived
    /// the filter. Drives a real subscriber rather than inspecting the
    /// directive string — the string is what regressed in #460, so it
    /// cannot also be the oracle.
    fn survives(directive: &str, emit: impl FnOnce()) -> String {
        let buf = Buffer::default();
        let subscriber = tracing_subscriber::fmt()
            .with_env_filter(EnvFilter::try_new(directive).expect("directive parses"))
            .with_writer(buf.clone())
            .finish();
        tracing::subscriber::with_default(subscriber, emit);
        buf.contents()
    }

    /// #460: the default filter was an allowlist with no global level,
    /// so `EnvFilter` discarded every unlisted target outright — which
    /// is how `hep_rs`'s "collector unreachable" warning went missing
    /// for several releases while the daemon looked healthy.
    #[test]
    fn default_filter_passes_warnings_from_unlisted_targets() {
        let out = survives(DEFAULT_LOG_FILTER, || {
            tracing::warn!(target: "hep_rs::udp", "HEP UDP send failed");
        });
        assert!(
            out.contains("HEP UDP send failed"),
            "a WARN from an unlisted target must still reach the sink; \
             got {out:?}"
        );

        // Same guarantee for a dependency nobody listed. Note the
        // target deliberately does NOT begin with `siphon_ai`:
        // `EnvFilter` matches by prefix, so a hypothetical
        // `siphon_ai_new_crate` would already be covered by the
        // `siphon_ai=info` directive and would prove nothing. The
        // floor is what covers everything else.
        let out = survives(DEFAULT_LOG_FILTER, || {
            tracing::error!(target: "some_unlisted_dependency", "boom");
        });
        assert!(
            out.contains("boom"),
            "an unlisted dependency must not be silently muted; got {out:?}"
        );
    }

    /// Render one event inside a `call_id`-bearing span through the
    /// same JSON formatter `--log-format json` installs, and return the
    /// line. Mirrors the real layer's configuration exactly — a copy
    /// that drifted would prove nothing.
    fn json_line(emit: impl FnOnce()) -> serde_json::Value {
        use tracing_subscriber::layer::SubscriberExt;

        let buf = Buffer::default();
        let layer = tracing_subscriber::fmt::layer()
            .json()
            .flatten_event(true)
            .with_current_span(true)
            .with_span_list(false)
            .with_target(true)
            .with_writer(buf.clone());
        let subscriber = tracing_subscriber::registry()
            .with(EnvFilter::try_new("info").expect("directive parses"))
            .with(layer);
        tracing::subscriber::with_default(subscriber, emit);
        let out = buf.contents();
        serde_json::from_str(out.trim()).unwrap_or_else(|e| panic!("not JSON: {e}: {out:?}"))
    }

    /// The point of the whole flag (#588): under `json`, a field a
    /// call site attached is a *key*, not a substring of the message
    /// that a shipper has to regex back out.
    #[test]
    fn json_format_promotes_event_fields_to_top_level_keys() {
        let line = json_line(|| {
            tracing::info!(target: "siphon_ai::call", from = "sip:alice@pbx", "received invite");
        });
        assert_eq!(
            line["fields"],
            serde_json::Value::Null,
            "flatten_event(true) should leave no `fields` nesting: {line}"
        );
        assert_eq!(
            line["from"], "sip:alice@pbx",
            "event field must be a top-level key: {line}"
        );
        assert_eq!(line["message"], "received invite", "{line}");
        assert_eq!(line["target"], "siphon_ai::call", "{line}");
        assert_eq!(line["level"], "INFO", "{line}");
    }

    /// `call_id` lives on the span, not the event — every per-call
    /// function is `#[instrument(fields(call_id = ...))]`. It reaches
    /// the line via `with_current_span(true)`, and the acceptance
    /// criterion is that it is queryable rather than buried in text.
    #[test]
    fn json_format_carries_call_id_from_the_current_span() {
        let line = json_line(|| {
            let span = tracing::info_span!("handle_invite", call_id = "abc123@pbx");
            let _g = span.enter();
            tracing::info!("call answered");
        });
        assert_eq!(
            line["span"]["call_id"], "abc123@pbx",
            "the current span's call_id must ride along: {line}"
        );
        // `with_span_list(false)`: the ancestor chain is not repeated.
        assert_eq!(
            line["spans"],
            serde_json::Value::Null,
            "span list should stay off so line size is bounded: {line}"
        );
    }

    /// The default must not move. Existing operators read this in
    /// `journalctl`, and the fail2ban `<HOST>` extractor string-matches
    /// it — a silent switch to JSON would break bans, not just eyes.
    #[test]
    fn text_is_the_default_log_format() {
        use clap::Parser as _;
        let cli = super::Cli::parse_from(["siphon-ai", "--config", "/dev/null"]);
        assert_eq!(cli.log_format, super::LogFormat::Text);
    }

    /// `--log-format` is `global = true`, so it parses on either side
    /// of a subcommand — same ergonomics as `--config` / `--log`.
    #[test]
    fn log_format_parses_before_and_after_a_subcommand() {
        use clap::Parser as _;
        for argv in [
            vec![
                "siphon-ai",
                "--log-format",
                "json",
                "check",
                "--config",
                "x",
            ],
            vec![
                "siphon-ai",
                "check",
                "--config",
                "x",
                "--log-format",
                "json",
            ],
        ] {
            let cli = super::Cli::parse_from(&argv);
            assert_eq!(cli.log_format, super::LogFormat::Json, "argv: {argv:?}");
        }
    }

    /// The floor is a floor, not a ceiling: first-party crates still
    /// get `info`, and the upstream chatter the old directive muted
    /// stays muted at that level.
    #[test]
    fn default_filter_keeps_info_for_first_party_and_mutes_upstream_info() {
        let out = survives(DEFAULT_LOG_FILTER, || {
            tracing::info!(target: "siphon_ai_core", "call started");
        });
        assert!(out.contains("call started"), "got {out:?}");

        let out = survives(DEFAULT_LOG_FILTER, || {
            tracing::info!(target: "sip_transport", "chatty");
        });
        assert!(
            !out.contains("chatty"),
            "upstream INFO should stay below the warn floor; got {out:?}"
        );
    }
}
