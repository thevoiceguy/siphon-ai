//! `siphon-ai-testkit` — conformance testkit for the SiphonAI WS bridge
//! protocol v1. Plays the *daemon's* side of the protocol against a
//! candidate WebSocket server and validates its behavior: every JSON
//! message against `schemas/siphon-ai.v1.json`, binary frame sizing and
//! pacing, close semantics, unknown-message tolerance.
//!
//! ```text
//! siphon-ai-testkit run ws://localhost:8765/            # all scenarios
//! siphon-ai-testkit run --scenario basic-echo ws://...  # one scenario
//! siphon-ai-testkit run --report report.json ws://...   # machine-readable
//! siphon-ai-testkit list
//! ```
//!
//! Exit code 0 iff every scenario passed — gate your server's CI on it.
//! See `docs/CONFORMANCE.md`.

mod client;
mod report;
mod runner;
mod scenario;
mod validate;

use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};

use report::Report;
use scenario::Scenario;
use validate::MessageValidator;

#[derive(Parser)]
#[command(
    name = "siphon-ai-testkit",
    version,
    about = "Conformance testkit for the SiphonAI WS bridge protocol v1"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run scenarios against a candidate WS server.
    Run {
        /// The server under test, e.g. `ws://127.0.0.1:8765/`.
        url: String,
        /// Scenario name (repeatable), or `all` (the default).
        #[arg(long, short, default_value = "all")]
        scenario: Vec<String>,
        /// Directory of additional `*.toml` scenario files.
        #[arg(long)]
        scenario_dir: Option<PathBuf>,
        /// Write the JSON report here (stdout keeps the human summary).
        #[arg(long)]
        report: Option<PathBuf>,
    },
    /// List available scenarios.
    List {
        /// Directory of additional `*.toml` scenario files.
        #[arg(long)]
        scenario_dir: Option<PathBuf>,
    },
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    match Cli::parse().command {
        Command::List { scenario_dir } => {
            for s in &load_scenarios(scenario_dir.as_deref())? {
                println!("{:<22} {}", s.name, s.description);
            }
            Ok(())
        }
        Command::Run {
            url,
            scenario,
            scenario_dir,
            report,
        } => {
            let available = load_scenarios(scenario_dir.as_deref())?;
            let selected: Vec<&Scenario> = if scenario.iter().any(|s| s == "all") {
                available.iter().collect()
            } else {
                scenario
                    .iter()
                    .map(|want| {
                        available.iter().find(|s| &s.name == want).with_context(|| {
                            format!(
                                "unknown scenario `{want}` — `siphon-ai-testkit list` shows \
                                 what's available"
                            )
                        })
                    })
                    .collect::<Result<_>>()?
            };

            let validator = MessageValidator::new()?;
            let mut results = Vec::with_capacity(selected.len());
            for s in selected {
                eprintln!("running {} …", s.name);
                results.push(runner::run_scenario(&url, s, &validator).await.finalize());
            }

            let full_report = Report::new(&url, results);
            print!("{}", full_report.render_text());
            if let Some(path) = report {
                std::fs::write(&path, serde_json::to_string_pretty(&full_report)?)
                    .with_context(|| format!("writing report to {}", path.display()))?;
                eprintln!("report written to {}", path.display());
            }
            if !full_report.conformant {
                std::process::exit(1);
            }
            Ok(())
        }
    }
}

/// Bundled scenarios plus any from `--scenario-dir` (which may shadow a
/// bundled name to override it).
fn load_scenarios(dir: Option<&std::path::Path>) -> Result<Vec<Scenario>> {
    let mut scenarios = scenario::bundled()?;
    if let Some(dir) = dir {
        let entries = std::fs::read_dir(dir)
            .with_context(|| format!("reading --scenario-dir {}", dir.display()))?;
        let mut paths: Vec<PathBuf> = entries
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|ext| ext == "toml"))
            .collect();
        paths.sort();
        if paths.is_empty() {
            bail!("--scenario-dir {} contains no .toml files", dir.display());
        }
        for path in paths {
            let text = std::fs::read_to_string(&path)
                .with_context(|| format!("reading {}", path.display()))?;
            let parsed = scenario::parse(&path.display().to_string(), &text)?;
            scenarios.retain(|s| s.name != parsed.name);
            scenarios.push(parsed);
        }
    }
    Ok(scenarios)
}
