//! Fleet configuration: which siphon-ai nodes to watch and how.
//!
//! Two mutually exclusive sources (DESIGN_SIGHTGLASS.md §8):
//! a `config.toml` with one `[[node]]` per daemon, or `--target` for
//! an ad-hoc single-node fleet. Everything is resolved (token files
//! read, names validated) before the terminal is put into raw mode,
//! so config errors print as plain CLI errors.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::Deserialize;

use crate::Cli;

/// On-disk shape of `~/.config/sightglass/config.toml`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    /// Per-node poll cadence. Default 1000 (§2: 1 s).
    poll_interval_ms: Option<u64>,
    #[serde(default)]
    node: Vec<FileNode>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileNode {
    /// Unique display name — the `NodeId` shown in every Node column
    /// and confirm modal.
    name: String,
    /// Admin listener base URL, e.g. `https://prod-1.example.com:9090`.
    url: String,
    /// File holding the admin bearer token for this node.
    token_file: Option<PathBuf>,
    /// PEM CA bundle for a privately-signed admin TLS cert.
    ca: Option<PathBuf>,
}

/// One fully resolved node: secrets read, url normalized.
#[derive(Debug, Clone)]
pub struct Node {
    pub name: String,
    /// Base URL without a trailing slash.
    pub url: String,
    pub token: Option<String>,
    /// PEM bytes of a private CA, when configured.
    pub ca_pem: Option<Vec<u8>>,
}

/// The resolved fleet the rest of the program runs on.
#[derive(Debug, Clone)]
pub struct Fleet {
    pub poll_interval: Duration,
    pub nodes: Vec<Node>,
}

/// Resolve CLI args + config file into a [`Fleet`].
pub fn load(cli: &Cli) -> Result<Fleet> {
    if let Some(target) = &cli.target {
        let token = match &cli.token_file {
            Some(p) => Some(read_token(p)?),
            // Env fallback is single-node only (§8) — a fleet config
            // can't disambiguate which node one env token belongs to.
            None => std::env::var("SIGHTGLASS_TOKEN")
                .ok()
                .map(|t| t.trim().to_string()),
        };
        let ca_pem = cli.ca.as_deref().map(read_ca).transpose()?;
        let url = normalize_url(target)?;
        return Ok(Fleet {
            poll_interval: Duration::from_millis(1000),
            nodes: vec![Node {
                name: display_name_for(&url),
                url,
                token,
                ca_pem,
            }],
        });
    }

    let path = match &cli.config {
        Some(p) => p.clone(),
        None => default_config_path()?,
    };
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("reading fleet config {}", path.display()))?;
    let parsed: FileConfig =
        toml::from_str(&raw).with_context(|| format!("parsing fleet config {}", path.display()))?;
    resolve(parsed, path.parent().unwrap_or(Path::new(".")))
}

fn resolve(cfg: FileConfig, base_dir: &Path) -> Result<Fleet> {
    if cfg.node.is_empty() {
        bail!("fleet config has no [[node]] entries (or use --target for an ad-hoc node)");
    }
    let mut nodes = Vec::with_capacity(cfg.node.len());
    for n in &cfg.node {
        if n.name.trim().is_empty() {
            bail!("[[node]] with url {:?} has an empty name", n.url);
        }
        if nodes.iter().any(|r: &Node| r.name == n.name) {
            bail!(
                "duplicate [[node]] name {:?} — names are the NodeId and must be unique",
                n.name
            );
        }
        let token = n
            .token_file
            .as_deref()
            .map(|p| read_token(&resolve_path(base_dir, p)))
            .transpose()
            .with_context(|| format!("node {:?}", n.name))?;
        let ca_pem =
            n.ca.as_deref()
                .map(|p| read_ca(&resolve_path(base_dir, p)))
                .transpose()
                .with_context(|| format!("node {:?}", n.name))?;
        nodes.push(Node {
            name: n.name.clone(),
            url: normalize_url(&n.url).with_context(|| format!("node {:?}", n.name))?,
            token,
            ca_pem,
        });
    }
    let poll_ms = cfg.poll_interval_ms.unwrap_or(1000);
    if poll_ms < 100 {
        bail!("poll_interval_ms {poll_ms} is below the 100ms floor");
    }
    Ok(Fleet {
        poll_interval: Duration::from_millis(poll_ms),
        nodes,
    })
}

/// Token/CA paths in the config resolve relative to the config file's
/// directory (with `~/` expanded), so a checked-out config dir is
/// relocatable.
fn resolve_path(base_dir: &Path, p: &Path) -> PathBuf {
    if let Ok(rest) = p.strip_prefix("~") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        base_dir.join(p)
    }
}

fn read_token(path: &Path) -> Result<String> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("reading token file {}", path.display()))?;
    let token = raw.trim().to_string();
    if token.is_empty() {
        bail!("token file {} is empty", path.display());
    }
    Ok(token)
}

fn read_ca(path: &Path) -> Result<Vec<u8>> {
    std::fs::read(path).with_context(|| format!("reading CA bundle {}", path.display()))
}

fn normalize_url(url: &str) -> Result<String> {
    let url = url.trim().trim_end_matches('/');
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        bail!("node url {url:?} must start with http:// or https://");
    }
    Ok(url.to_string())
}

/// Ad-hoc `--target` nodes are named by host[:port] — good enough to
/// label a one-node fleet.
fn display_name_for(url: &str) -> String {
    url.trim_start_matches("http://")
        .trim_start_matches("https://")
        .split('/')
        .next()
        .unwrap_or(url)
        .to_string()
}

fn default_config_path() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").context("HOME is not set; pass --config or --target")?;
    Ok(PathBuf::from(home).join(".config/sightglass/config.toml"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(toml_src: &str) -> Result<Fleet> {
        resolve(toml::from_str(toml_src).unwrap(), Path::new("/tmp"))
    }

    #[test]
    fn two_node_fleet_parses() {
        let fleet = parse(
            r#"
            poll_interval_ms = 500
            [[node]]
            name = "prod-1"
            url = "https://prod-1.example.com:9090/"
            [[node]]
            name = "prod-2"
            url = "http://127.0.0.1:9090"
            "#,
        )
        .unwrap();
        assert_eq!(fleet.poll_interval, Duration::from_millis(500));
        assert_eq!(fleet.nodes.len(), 2);
        // trailing slash normalized away
        assert_eq!(fleet.nodes[0].url, "https://prod-1.example.com:9090");
    }

    #[test]
    fn duplicate_node_names_are_rejected() {
        let err = parse(
            r#"
            [[node]]
            name = "a"
            url = "http://h1:9090"
            [[node]]
            name = "a"
            url = "http://h2:9090"
            "#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("duplicate"), "{err}");
    }

    #[test]
    fn empty_fleet_and_bad_scheme_are_rejected() {
        assert!(parse("").is_err());
        assert!(parse(
            r#"
            [[node]]
            name = "a"
            url = "ftp://nope"
            "#
        )
        .is_err());
    }

    #[test]
    fn target_names_derive_from_host() {
        assert_eq!(
            display_name_for("https://prod-1.example.com:9090"),
            "prod-1.example.com:9090"
        );
        assert_eq!(
            display_name_for("http://127.0.0.1:9090/admin"),
            "127.0.0.1:9090"
        );
    }
}
