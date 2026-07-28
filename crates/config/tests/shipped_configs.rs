//! Guard: every daemon config this repo ships must still parse.
//!
//! `examples/twilio-trunk/siphon-ai.toml` sat in the tree carrying
//! `[[trunk]].sources` long after that key became `peer_addrs`, so the
//! example a new Twilio user copies produced a daemon that refused to
//! start — and nothing in CI noticed, because nothing loaded the
//! configs we publish. See #383/#384.
//!
//! Scope is deliberately the **parse** stage: it catches the drift this
//! missed (renamed keys, removed sections, unknown keys in a route
//! table) without depending on the machine the test runs on. Full
//! validation resolves DNS, binds nothing but checks addresses, and
//! stats files on disk — `examples/homer-stack/siphon-ai-hep.toml`
//! names `host.docker.internal`, which only resolves inside the
//! compose network. Those checks belong to `siphon-ai --config X check`,
//! not to a unit test.

use std::fs;
use std::path::{Path, PathBuf};

use siphon_ai_config::RawConfig;

/// Repo root, resolved from this crate's manifest directory.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repo root resolves from CARGO_MANIFEST_DIR")
}

/// Collects `*.toml` under `dir`, skipping build and VCS directories.
///
/// `tests` is skipped too: a fixture is a pre-substitution template,
/// not a config we publish. `bins/siphon-ai/tests/fixtures/local-dev.toml`
/// carries `rtp_port_range = [${TEST_RTP_MIN}, ${TEST_RTP_MAX}]`, which
/// isn't valid TOML until its own test expands it.
fn collect_toml(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if path.is_dir() {
            if matches!(name.as_ref(), "target" | ".git" | "node_modules" | "tests") {
                continue;
            }
            collect_toml(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "toml") {
            out.push(path);
        }
    }
}

/// A daemon config is a TOML file with a `[node]` table. That keeps
/// manifests, `rustfmt.toml`, `pyproject.toml`, the testkit scenario
/// files and `examples/observability/vector.toml` out without a
/// hand-maintained skip list — add a new example config and it is
/// covered the moment it declares `[node]`.
fn is_daemon_config(text: &str) -> bool {
    text.lines()
        .any(|line| line.trim_start().starts_with("[node]"))
}

#[test]
fn every_shipped_daemon_config_parses() {
    let root = repo_root();
    let mut candidates = Vec::new();
    collect_toml(&root, &mut candidates);
    candidates.sort();

    let mut checked = Vec::new();
    let mut failures = Vec::new();
    for path in candidates {
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        if !is_daemon_config(&text) {
            continue;
        }
        let rel = path.strip_prefix(&root).unwrap_or(&path).to_path_buf();
        if let Err(e) = toml::from_str::<RawConfig>(&text) {
            failures.push(format!("{}: {e}", rel.display()));
        }
        checked.push(rel);
    }

    assert!(
        failures.is_empty(),
        "shipped config(s) no longer parse:\n{}",
        failures.join("\n\n")
    );

    // The walk finding nothing would make this test pass silently, so
    // assert it saw the configs we know are published.
    let names: Vec<String> = checked.iter().map(|p| p.display().to_string()).collect();
    for expected in [
        "configs/local-dev.toml",
        "examples/twilio-trunk/siphon-ai.toml",
    ] {
        assert!(
            names.iter().any(|n| n.replace('\\', "/") == expected),
            "{expected} was not discovered; found: {names:?}"
        );
    }
}
