//! Fetching the `x5u` signing certificate over HTTPS.
//!
//! RFC 8224 §4 / SHAKEN locate the signing certificate at the PASSporT's
//! `x5u` URL. This is async network I/O — deliberately the application
//! layer's job, not the `sip-identity` stack crate's (which has no runtime
//! and no cache lifecycle). The fetch is bounded three ways so a hostile or
//! broken signer can't stall or exhaust the accept path:
//!
//! - **HTTPS only.** A `tel:`/`http:`/file URL is rejected before any I/O —
//!   the certificate's authenticity rests on the chain, but transport
//!   integrity and not leaking the lookup still matter, and SHAKEN mandates
//!   HTTPS.
//! - **Redirect-free.** The client follows no redirects, so an `x5u` can't
//!   bounce the fetch to an attacker-chosen or internal address.
//! - **Size- and time-capped.** A `Content-Length` over the cap is refused
//!   up front, and the per-request timeout (set on the shared client)
//!   bounds the transfer.

use thiserror::Error;

use crate::pem::parse_cert_chain;

/// Cap on the certificate response body. A SHAKEN cert chain is a few KB;
/// 64 KiB is generous headroom while bounding the bytes a signer can make
/// us buffer on the accept path (DoS guard).
const MAX_CERT_BYTES: u64 = 64 * 1024;

/// Why an `x5u` certificate fetch failed.
#[derive(Debug, Error)]
pub enum FetchError {
    /// The `x5u` was not a parseable absolute URL.
    #[error("x5u is not a valid URL: {0}")]
    BadUrl(String),
    /// The `x5u` used a scheme other than `https`.
    #[error("x5u must be https, got scheme {0:?}")]
    InsecureScheme(String),
    /// The HTTP request itself failed (DNS, connect, TLS, timeout, …).
    #[error("x5u request failed: {0}")]
    Request(String),
    /// The server answered with a non-success status.
    #[error("x5u returned HTTP {0}")]
    Status(u16),
    /// The response advertised or delivered more than [`MAX_CERT_BYTES`].
    #[error("x5u response too large (limit {limit} bytes)")]
    TooLarge { limit: u64 },
    /// The response decoded to zero certificates.
    #[error("x5u response contained no certificates")]
    NoCertificates,
}

/// Validate that `x5u` is an absolute `https` URL, returning the parsed URL
/// ready to fetch. Pure so the scheme policy is unit-testable without I/O.
fn require_https(x5u: &str) -> Result<reqwest::Url, FetchError> {
    let url = reqwest::Url::parse(x5u).map_err(|e| FetchError::BadUrl(e.to_string()))?;
    if url.scheme() != "https" {
        return Err(FetchError::InsecureScheme(url.scheme().to_string()));
    }
    Ok(url)
}

/// Fetch and decode the certificate chain at `x5u`. The first returned
/// entry is the end-entity (leaf) certificate; any remainder are
/// intermediates (order preserved from the response). Verification of the
/// chain is the caller's next step.
pub(crate) async fn fetch_chain(
    client: &reqwest::Client,
    x5u: &str,
) -> Result<Vec<Vec<u8>>, FetchError> {
    let url = require_https(x5u)?;

    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| FetchError::Request(e.to_string()))?;

    if !resp.status().is_success() {
        return Err(FetchError::Status(resp.status().as_u16()));
    }

    // Refuse an oversized body before buffering it, when the server is
    // honest enough to advertise the length.
    if let Some(len) = resp.content_length() {
        if len > MAX_CERT_BYTES {
            return Err(FetchError::TooLarge {
                limit: MAX_CERT_BYTES,
            });
        }
    }

    let bytes = resp
        .bytes()
        .await
        .map_err(|e| FetchError::Request(e.to_string()))?;
    if bytes.len() as u64 > MAX_CERT_BYTES {
        return Err(FetchError::TooLarge {
            limit: MAX_CERT_BYTES,
        });
    }

    let chain = parse_cert_chain(&bytes);
    if chain.is_empty() {
        return Err(FetchError::NoCertificates);
    }
    Ok(chain)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_https() {
        let url = require_https("https://cert.example.org/c.crt").unwrap();
        assert_eq!(url.scheme(), "https");
    }

    #[test]
    fn rejects_http() {
        assert!(matches!(
            require_https("http://cert.example.org/c.crt"),
            Err(FetchError::InsecureScheme(s)) if s == "http"
        ));
    }

    #[test]
    fn rejects_non_url() {
        assert!(matches!(
            require_https("not a url"),
            Err(FetchError::BadUrl(_))
        ));
    }

    #[test]
    fn rejects_other_schemes() {
        for u in ["file:///etc/passwd", "tel:+12155551212", "ftp://h/c.crt"] {
            assert!(matches!(
                require_https(u),
                Err(FetchError::InsecureScheme(_)) | Err(FetchError::BadUrl(_))
            ));
        }
    }
}
