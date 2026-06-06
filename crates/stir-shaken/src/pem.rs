//! Certificate decoding helpers: PEM bundles and the trust-anchor file.

use std::io::BufReader;
use std::path::Path;

use thiserror::Error;

/// Failure loading the configured trust-anchor PEM file.
#[derive(Debug, Error)]
pub enum TrustAnchorLoadError {
    #[error("trust anchor file {path} could not be read: {source}")]
    Io {
        path: String,
        source: std::io::Error,
    },
    #[error("trust anchor file {path} contained no PEM certificates")]
    Empty { path: String },
}

/// Parse a certificate blob into one-or-more DER byte vectors. Accepts a PEM
/// bundle (`-----BEGIN CERTIFICATE-----` …) or, when no PEM is found, treats
/// the bytes as a single raw DER certificate. Order is preserved — for an
/// `x5u` response the first entry is the end-entity (leaf) certificate and
/// any remainder are intermediates.
pub fn parse_cert_chain(bytes: &[u8]) -> Vec<Vec<u8>> {
    let mut reader = BufReader::new(bytes);
    let pem_certs: Vec<Vec<u8>> = rustls_pemfile::certs(&mut reader)
        .filter_map(Result::ok)
        .map(|c| c.to_vec())
        .collect();
    if !pem_certs.is_empty() {
        return pem_certs;
    }
    // No PEM blocks — assume a single DER cert (some x5u endpoints serve
    // `application/pkix-cert`).
    if bytes.is_empty() {
        Vec::new()
    } else {
        vec![bytes.to_vec()]
    }
}

/// Load the STI-PA trust anchors from a PEM file into DER byte vectors.
/// Fails loud if the file is missing or holds no certificates.
pub fn load_trust_anchors(path: &Path) -> Result<Vec<Vec<u8>>, TrustAnchorLoadError> {
    let bytes = std::fs::read(path).map_err(|source| TrustAnchorLoadError::Io {
        path: path.display().to_string(),
        source,
    })?;
    let certs = parse_cert_chain(&bytes);
    if certs.is_empty() {
        return Err(TrustAnchorLoadError::Empty {
            path: path.display().to_string(),
        });
    }
    Ok(certs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rcgen::{CertificateParams, KeyPair, PKCS_ECDSA_P256_SHA256};

    /// Generate a self-signed cert, returning its PEM and DER forms.
    fn cert(cn: &str) -> (String, Vec<u8>) {
        let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).unwrap();
        let c = CertificateParams::new(vec![cn.into()])
            .unwrap()
            .self_signed(&key)
            .unwrap();
        (c.pem(), c.der().to_vec())
    }

    #[test]
    fn parses_pem_bundle_in_order() {
        let (pem_a, der_a) = cert("a.example");
        let (pem_b, der_b) = cert("b.example");
        let bundle = format!("{pem_a}{pem_b}");
        let chain = parse_cert_chain(bundle.as_bytes());
        assert_eq!(chain, vec![der_a, der_b]);
    }

    #[test]
    fn falls_back_to_raw_der_when_no_pem() {
        let (_pem, der) = cert("a.example");
        // No PEM armor → treated as a single raw DER cert.
        let chain = parse_cert_chain(&der);
        assert_eq!(chain, vec![der]);
    }

    #[test]
    fn empty_input_yields_no_certs() {
        assert!(parse_cert_chain(&[]).is_empty());
    }

    #[test]
    fn load_trust_anchors_reads_pem_file() {
        let (pem_a, der_a) = cert("root.example");
        let path = std::env::temp_dir().join("siphon_ta_load_ok.pem");
        std::fs::write(&path, pem_a.as_bytes()).unwrap();
        let anchors = load_trust_anchors(&path).unwrap();
        assert_eq!(anchors, vec![der_a]);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn load_trust_anchors_missing_file_is_io_error() {
        let err = load_trust_anchors(Path::new("/nonexistent/sti-pa-roots.pem")).unwrap_err();
        assert!(matches!(err, TrustAnchorLoadError::Io { .. }));
    }

    #[test]
    fn load_trust_anchors_empty_file_is_empty_error() {
        // Only a truly empty file yields `Empty`: per `parse_cert_chain`'s
        // documented fallback, non-empty non-PEM bytes are taken as a single
        // raw DER cert (rejected later at chain validation, and guarded at
        // config load by the config crate's PEM-block check).
        let path = std::env::temp_dir().join("siphon_ta_load_empty.pem");
        std::fs::write(&path, b"").unwrap();
        let err = load_trust_anchors(&path).unwrap_err();
        assert!(matches!(err, TrustAnchorLoadError::Empty { .. }));
        let _ = std::fs::remove_file(&path);
    }
}
