use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;

#[derive(Parser, Debug)]
#[command(name = "siphon-ai", version, about = "SIP-to-WebSocket media bridge")]
struct Cli {
    /// Path to the TOML configuration file.
    #[arg(long, short, env = "SIPHON_AI_CONFIG")]
    config: PathBuf,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    println!(
        "siphon-ai v{} — config: {} (scaffold; daemon not yet implemented)",
        env!("CARGO_PKG_VERSION"),
        cli.config.display()
    );
    Ok(())
}
