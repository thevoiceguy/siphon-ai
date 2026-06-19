//! CLI integration tests for the `siphon-ai check` subcommand (config
//! CLI chunk 1). Drives the built binary as a subprocess over fixture
//! TOMLs and asserts on exit code + stdout/stderr.
//!
//! `CARGO_BIN_EXE_siphon-ai` is set by Cargo for integration tests and
//! points at the freshly-built daemon binary.

use std::io::Write;
use std::process::Command;

use tempfile::NamedTempFile;

const BIN: &str = env!("CARGO_BIN_EXE_siphon-ai");

const VALID: &str = r#"
[node]
id = "cli-test"
[sip]
listen = "127.0.0.1:5060"
[bridge]
ws_url = "wss://example/ws"
[[route]]
name = "default"
[route.match]
any = true
"#;

/// A config with no final `any = true` route — valid, but the daemon
/// (and `check`) warns about it.
const NO_DEFAULT_ROUTE: &str = r#"
[node]
id = "cli-test"
[sip]
listen = "127.0.0.1:5060"
[bridge]
ws_url = "wss://example/ws"
[[route]]
name = "only"
[route.match]
to_user = "1000"
"#;

/// An unparseable `[sip].listen` — fails at compile time.
const INVALID: &str = r#"
[sip]
listen = "not-a-socket-addr"
[[route]]
name = "default"
[route.match]
any = true
"#;

fn write_cfg(body: &str) -> NamedTempFile {
    let mut f = NamedTempFile::new().expect("temp file");
    f.write_all(body.as_bytes()).expect("write config");
    f
}

#[test]
fn check_valid_config_exits_zero_with_summary() {
    let cfg = write_cfg(VALID);
    let out = Command::new(BIN)
        .arg("check")
        .arg("--config")
        .arg(cfg.path())
        .output()
        .expect("run check");
    assert!(
        out.status.success(),
        "expected exit 0; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("config OK:"), "stdout: {stdout}");
    assert!(
        stdout.contains("node id:       cli-test"),
        "stdout: {stdout}"
    );
    assert!(stdout.contains("default route: yes"), "stdout: {stdout}");
}

#[test]
fn check_invalid_config_exits_one_with_error() {
    let cfg = write_cfg(INVALID);
    let out = Command::new(BIN)
        .args(["check", "--config"])
        .arg(cfg.path())
        .output()
        .expect("run check");
    assert_eq!(out.status.code(), Some(1), "expected exit 1");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("config INVALID:"), "stderr: {stderr}");
    // The underlying compile error message is surfaced.
    assert!(stderr.contains("listen"), "stderr: {stderr}");
}

#[test]
fn check_config_flag_works_before_or_after_subcommand() {
    let cfg = write_cfg(VALID);
    let path = cfg.path().to_str().unwrap();
    for args in [
        vec!["check", "--config", path],
        vec!["--config", path, "check"],
    ] {
        let out = Command::new(BIN).args(&args).output().expect("run check");
        assert!(
            out.status.success(),
            "args {args:?} expected exit 0; stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

#[test]
fn check_missing_config_errors() {
    let out = Command::new(BIN).arg("check").output().expect("run check");
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("--config"));
}

#[test]
fn check_warns_on_missing_default_route_but_exits_zero() {
    let cfg = write_cfg(NO_DEFAULT_ROUTE);
    let out = Command::new(BIN)
        .arg("check")
        .arg("--config")
        .arg(cfg.path())
        .output()
        .expect("run check");
    assert!(
        out.status.success(),
        "a missing default route is a warning, not an error"
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("default route: NO"), "stdout: {stdout}");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("no default route"),
        "expected a default-route warning on stderr"
    );
}
