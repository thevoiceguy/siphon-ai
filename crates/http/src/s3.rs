//! Minimal S3-compatible object PUT client on `reqwest` + [`crate::sigv4`]
//! (DESIGN_RECORDING_COMPLIANCE §3 — no AWS SDK).
//!
//! Path-style addressing (`https://endpoint/bucket/key`) so MinIO,
//! Cloudflare R2, Backblaze B2, and AWS all work with one shape. Exactly
//! one operation: `PUT Object` from a local file, streamed (the payload is
//! SHA-256-hashed in a first pass, then streamed in the upload pass — a
//! multi-GiB recording never sits in memory).
//!
//! Single-PUT only: S3's one-request cap is 5 GiB, comfortably above any
//! real recording (WAV header sizes already saturate at 4 GiB). Uploads of
//! larger files fail loud with [`S3Error::TooLarge`] rather than silently
//! truncating. (The design note sketched multipart "for safety"; deviation
//! noted — single-PUT keeps this module a fraction of the size and the cap
//! is unreachable in practice.)

use std::path::Path;

use reqwest::Client;
use sha2::{Digest, Sha256};
use thiserror::Error;
use tracing::debug;

use crate::sigv4::{authorization_header, uri_encode_segment, SigV4Credentials, SignRequest};

/// S3's single-`PUT` object-size cap.
const MAX_SINGLE_PUT: u64 = 5 * 1024 * 1024 * 1024;

/// Where and how to upload: one bucket on one S3-compatible endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct S3Target {
    /// Scheme + host (+ optional port), no trailing slash — e.g.
    /// `https://s3.us-east-1.amazonaws.com` or `http://127.0.0.1:9000`.
    pub endpoint: String,
    pub bucket: String,
    pub region: String,
    pub credentials: SigV4Credentials,
}

#[derive(Debug, Error)]
pub enum S3Error {
    #[error("read {path}: {source}")]
    Read {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("{size} bytes exceeds the 5 GiB single-PUT cap")]
    TooLarge { size: u64 },
    #[error("PUT {url}: {source}")]
    Transport {
        url: String,
        #[source]
        source: reqwest::Error,
    },
    #[error("PUT {url}: HTTP {status}: {body}")]
    Status {
        url: String,
        status: u16,
        /// First KiB of the error body — S3 errors are small XML docs.
        body: String,
    },
}

/// A successful upload's location, for the CDR / `recording_uploaded`
/// event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct S3Location {
    /// `s3://bucket/key` — storage-agnostic pointer for records.
    pub uri: String,
    /// The HTTPS URL the object was PUT to (endpoint-specific).
    pub url: String,
}

impl S3Target {
    /// Upload the file at `path` as `key` (e.g. `2026-07-08/call.wava`).
    ///
    /// Two passes over the file: SHA-256 for the signed
    /// `x-amz-content-sha256`, then a streamed upload body. Retry/backoff
    /// is the caller's job (the upload worker owns the spool + schedule).
    pub async fn put_file(
        &self,
        client: &Client,
        key: &str,
        path: &Path,
    ) -> Result<S3Location, S3Error> {
        let read_err = |source| S3Error::Read {
            path: path.display().to_string(),
            source,
        };

        // Pass 1: size + payload hash, streamed in 64 KiB reads.
        let mut file = tokio::fs::File::open(path).await.map_err(read_err)?;
        let mut hasher = Sha256::new();
        let mut size = 0u64;
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            use tokio::io::AsyncReadExt;
            let n = file.read(&mut buf).await.map_err(read_err)?;
            if n == 0 {
                break;
            }
            size += n as u64;
            hasher.update(&buf[..n]);
        }
        if size > MAX_SINGLE_PUT {
            return Err(S3Error::TooLarge { size });
        }
        let payload_hash: String = hasher
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();

        // Canonical URI: /bucket/key with each segment S3-encoded.
        let canonical_uri: String = std::iter::once(self.bucket.as_str())
            .chain(key.split('/'))
            .map(uri_encode_segment)
            .fold(String::new(), |mut acc, seg| {
                acc.push('/');
                acc.push_str(&seg);
                acc
            });
        let url = format!("{}{canonical_uri}", self.endpoint);

        let host = self
            .endpoint
            .split_once("://")
            .map(|(_, rest)| rest)
            .unwrap_or(&self.endpoint)
            .trim_end_matches('/');
        let timestamp = now_amz();
        let auth = authorization_header(
            &self.credentials,
            &SignRequest {
                method: "PUT",
                canonical_uri: &canonical_uri,
                canonical_query: "",
                headers: &[
                    ("content-length", &size.to_string()),
                    ("host", host),
                    ("x-amz-content-sha256", &payload_hash),
                    ("x-amz-date", &timestamp),
                ],
                payload_hash: &payload_hash,
                timestamp: &timestamp,
                region: &self.region,
                service: "s3",
            },
        );

        // Pass 2: streamed body.
        let file = tokio::fs::File::open(path).await.map_err(read_err)?;
        let stream = tokio_util::io::ReaderStream::new(file);
        let response = client
            .put(&url)
            .header("authorization", auth)
            .header("x-amz-content-sha256", &payload_hash)
            .header("x-amz-date", &timestamp)
            .header("content-length", size)
            .body(reqwest::Body::wrap_stream(stream))
            .send()
            .await
            .map_err(|source| S3Error::Transport {
                url: url.clone(),
                source,
            })?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            let body = body.chars().take(1024).collect();
            return Err(S3Error::Status {
                url,
                status: status.as_u16(),
                body,
            });
        }
        debug!(key, size, "uploaded recording to object storage");
        Ok(S3Location {
            uri: format!("s3://{}/{key}", self.bucket),
            url,
        })
    }
}

/// `YYYYMMDDTHHMMSSZ` UTC now, without a chrono dependency here.
pub(crate) fn now_amz() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock before 1970")
        .as_secs();
    // Days-to-date conversion (civil-from-days, Howard Hinnant's algorithm).
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mo <= 2 { y + 1 } else { y };
    format!("{y:04}{mo:02}{d:02}T{h:02}{m:02}{s:02}Z")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn amz_timestamp_shape_and_sanity() {
        let t = now_amz();
        assert_eq!(t.len(), 16, "{t}");
        assert!(t.ends_with('Z') && t.as_bytes()[8] == b'T', "{t}");
        let year: u32 = t[..4].parse().unwrap();
        assert!((2026..2100).contains(&year), "{t}");
    }

    #[test]
    fn canonical_uri_encodes_segments_keeps_slashes() {
        // Mirrors the put_file construction.
        let uri: String = std::iter::once("bucket")
            .chain("2026-07-08/call one.wava".split('/'))
            .map(uri_encode_segment)
            .fold(String::new(), |mut acc, seg| {
                acc.push('/');
                acc.push_str(&seg);
                acc
            });
        assert_eq!(uri, "/bucket/2026-07-08/call%20one.wava");
    }
}
