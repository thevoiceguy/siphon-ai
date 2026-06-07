//! Compiled STIR/SHAKEN configuration and trust-anchor plumbing.
//!
//! These are the *typed* forms the config crate compiles `[security]` /
//! `[security.stir_shaken]` into. Verification itself (cert fetch, ES256,
//! chain validation) is not wired here — this revision only validates that
//! the trust-anchor file is present and PEM-shaped so misconfiguration
//! fails loud at startup rather than on the first signed call.

use std::path::{Path, PathBuf};
use std::time::Duration;

use thiserror::Error;

/// Default verification-certificate cache TTL (1 hour) — matches HTTP cache
/// semantics on the `x5u`/`info` cert responses (plan §9 decision 2).
pub const DEFAULT_CERT_CACHE_TTL: Duration = Duration::from_secs(3600);

/// Compiled `[security.stir_shaken]` block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StirShakenConfig {
    /// Master switch. When `false` (default) no Identity parsing or
    /// verification runs and no `verstat` is surfaced — a 0.3.x deployment
    /// upgrades with zero behaviour change.
    pub enabled: bool,
    /// Path to the PEM bundle of STI-PA trust anchors. `contrib/sti-pa-roots.pem`
    /// is a template the operator populates with the authentic root(s) (plan §9
    /// decision 1; see `contrib/README.md`).
    pub trust_anchors: PathBuf,
    /// How long a fetched signing certificate is cached before re-fetch.
    pub cert_cache_ttl: Duration,
    /// Reject inbound INVITEs that carry no `Identity` header with 428
    /// ("Use Identity Header", RFC 8224 §6.2.2) instead of admitting them
    /// as unsigned.
    pub require_identity: bool,
}

impl Default for StirShakenConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            trust_anchors: PathBuf::new(),
            cert_cache_ttl: DEFAULT_CERT_CACHE_TTL,
            require_identity: false,
        }
    }
}

/// Failure loading / validating the trust-anchor file.
#[derive(Debug, Error)]
pub enum TrustAnchorError {
    /// The file could not be read (missing, permissions, …).
    #[error("trust anchor file {path:?} could not be read: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    /// The file was read but contains no PEM `CERTIFICATE` blocks.
    #[error("trust anchor file {path:?} contains no PEM certificates")]
    NoCertificates { path: PathBuf },
}

/// Count `-----BEGIN CERTIFICATE-----` blocks in a PEM bundle. Pure so the
/// counting rule is unit-testable without touching the filesystem.
fn count_pem_certificates(pem: &str) -> usize {
    pem.matches("-----BEGIN CERTIFICATE-----").count()
}

/// Validate that a trust-anchor file exists and holds at least one PEM
/// certificate. Returns the certificate count. Used at config-load time so
/// a missing/empty anchor file is a loud startup failure, not a silent
/// "every call fails verification" at runtime.
///
/// This intentionally does **not** parse the certificates — DER decoding
/// and chain construction belong to the verifier, which lands with the
/// crypto dependencies.
pub fn validate_trust_anchors(path: &Path) -> Result<usize, TrustAnchorError> {
    let contents = std::fs::read_to_string(path).map_err(|source| TrustAnchorError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let count = count_pem_certificates(&contents);
    if count == 0 {
        return Err(TrustAnchorError::NoCertificates {
            path: path.to_path_buf(),
        });
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_disabled_with_1h_ttl() {
        let c = StirShakenConfig::default();
        assert!(!c.enabled);
        assert!(!c.require_identity);
        assert_eq!(c.cert_cache_ttl, DEFAULT_CERT_CACHE_TTL);
    }

    #[test]
    fn counts_pem_blocks() {
        assert_eq!(count_pem_certificates(""), 0);
        assert_eq!(count_pem_certificates("not a cert"), 0);
        let one = "-----BEGIN CERTIFICATE-----\nMII...\n-----END CERTIFICATE-----\n";
        assert_eq!(count_pem_certificates(one), 1);
        assert_eq!(count_pem_certificates(&one.repeat(3)), 3);
    }

    #[test]
    fn missing_file_is_io_error() {
        let err = validate_trust_anchors(Path::new("/nonexistent/sti-pa-roots.pem")).unwrap_err();
        assert!(matches!(err, TrustAnchorError::Io { .. }));
    }
}
