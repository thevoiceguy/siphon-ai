//! AWS Signature Version 4 request signing (hand-rolled, no AWS SDK —
//! DESIGN_RECORDING_COMPLIANCE §3/§6 D3).
//!
//! Exactly the subset SiphonAI needs: sign an HTTPS request with a static
//! access key so the S3-compatible object-storage sink (`PUT Object`) and
//! the AWS-KMS KEK provider (`Encrypt`/`Decrypt`) can talk to AWS, MinIO,
//! Cloudflare R2, Backblaze B2, etc. on the existing `reqwest`/`hmac`/
//! `sha2` stack. No STS/session tokens, no presigning, no chunked
//! (streaming-trailer) signatures — payloads are hashed up front or sent
//! `UNSIGNED-PAYLOAD` over TLS.
//!
//! Reference: the "Signature Version 4 signing process" documents; the
//! unit test pins the official `GET iam.amazonaws.com` example vector.

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

type HmacSha256 = Hmac<Sha256>;

/// Static credentials for SigV4 (S3/KMS access key pair).
#[derive(Clone, PartialEq, Eq)]
pub struct SigV4Credentials {
    pub access_key: String,
    pub secret_key: String,
}

impl std::fmt::Debug for SigV4Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The secret never reaches logs/fingerprints in cleartext.
        f.debug_struct("SigV4Credentials")
            .field("access_key", &self.access_key)
            .field("secret_key", &"<redacted>")
            .finish()
    }
}

/// Everything SigV4 needs to know about the request being signed.
///
/// `headers` MUST include `host` and any `x-amz-*` headers that will be
/// sent (including `x-amz-date` matching `timestamp` and
/// `x-amz-content-sha256` matching `payload_hash` for S3). Names are
/// lowercased and values trimmed during canonicalization, per spec.
pub struct SignRequest<'a> {
    pub method: &'a str,
    /// URL-encoded path as it will appear on the request line, e.g.
    /// `/bucket/2026-07-08/call.wava`. Must already be URI-encoded the
    /// S3 way (every segment, `/` kept).
    pub canonical_uri: &'a str,
    /// Canonical query string (sorted, URL-encoded), or `""`.
    pub canonical_query: &'a str,
    /// `(name, value)` pairs to sign; this fn sorts/lowercases them.
    pub headers: &'a [(&'a str, &'a str)],
    /// Hex SHA-256 of the payload, or `"UNSIGNED-PAYLOAD"`.
    pub payload_hash: &'a str,
    /// `YYYYMMDDTHHMMSSZ` — the `x-amz-date` value.
    pub timestamp: &'a str,
    pub region: &'a str,
    /// `"s3"` or `"kms"`.
    pub service: &'a str,
}

/// Hex-encoded SHA-256 — the `x-amz-content-sha256` value for a fully
/// buffered payload.
pub fn sha256_hex(data: &[u8]) -> String {
    hex(&Sha256::digest(data))
}

/// Compute the `Authorization` header value for `req`.
pub fn authorization_header(creds: &SigV4Credentials, req: &SignRequest<'_>) -> String {
    // Canonical headers: lowercase names, trimmed values, sorted by name.
    let mut headers: Vec<(String, String)> = req
        .headers
        .iter()
        .map(|(n, v)| (n.to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    headers.sort();
    let canonical_headers: String = headers.iter().map(|(n, v)| format!("{n}:{v}\n")).collect();
    let signed_headers: String = headers
        .iter()
        .map(|(n, _)| n.as_str())
        .collect::<Vec<_>>()
        .join(";");

    let canonical_request = format!(
        "{}\n{}\n{}\n{}\n{}\n{}",
        req.method,
        req.canonical_uri,
        req.canonical_query,
        canonical_headers,
        signed_headers,
        req.payload_hash,
    );

    let date = &req.timestamp[..8];
    let scope = format!("{date}/{}/{}/aws4_request", req.region, req.service);
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{}\n{scope}\n{}",
        req.timestamp,
        sha256_hex(canonical_request.as_bytes()),
    );

    // Signing key: HMAC chain date → region → service → "aws4_request".
    let k_date = hmac(
        format!("AWS4{}", creds.secret_key).as_bytes(),
        date.as_bytes(),
    );
    let k_region = hmac(&k_date, req.region.as_bytes());
    let k_service = hmac(&k_region, req.service.as_bytes());
    let k_signing = hmac(&k_service, b"aws4_request");
    let signature = hex(&hmac(&k_signing, string_to_sign.as_bytes()));

    format!(
        "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}",
        creds.access_key,
    )
}

/// URI-encode one path segment the S3 way: unreserved characters
/// (`A-Za-z0-9-._~`) pass through, everything else becomes `%XX`.
/// (S3 canonical URIs encode each segment but keep the `/` separators.)
pub fn uri_encode_segment(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    for byte in segment.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

fn hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The official SigV4 example from the AWS "Signing AWS API requests"
    /// documentation: `GET https://iam.amazonaws.com/?Action=ListUsers&
    /// Version=2010-05-08` at 2015-08-30T12:36:00Z with the well-known
    /// demo credentials. Pins the whole algorithm end to end.
    #[test]
    fn aws_documented_iam_example_vector() {
        let creds = SigV4Credentials {
            access_key: "AKIDEXAMPLE".into(),
            secret_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into(),
        };
        let auth = authorization_header(
            &creds,
            &SignRequest {
                method: "GET",
                canonical_uri: "/",
                canonical_query: "Action=ListUsers&Version=2010-05-08",
                headers: &[
                    (
                        "content-type",
                        "application/x-www-form-urlencoded; charset=utf-8",
                    ),
                    ("host", "iam.amazonaws.com"),
                    ("x-amz-date", "20150830T123600Z"),
                ],
                payload_hash: &sha256_hex(b""),
                timestamp: "20150830T123600Z",
                region: "us-east-1",
                service: "iam",
            },
        );
        assert_eq!(
            auth,
            "AWS4-HMAC-SHA256 \
             Credential=AKIDEXAMPLE/20150830/us-east-1/iam/aws4_request, \
             SignedHeaders=content-type;host;x-amz-date, \
             Signature=5d672d79c15b13162d9279b0855cfba6789a8edb4c82c400e06b5924a6f2b5d7"
        );
    }

    #[test]
    fn empty_payload_hash_is_the_wellknown_constant() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn uri_encoding_keeps_unreserved_escapes_the_rest() {
        assert_eq!(uri_encode_segment("call-id_1.wava~x"), "call-id_1.wava~x");
        assert_eq!(uri_encode_segment("a b+c/d"), "a%20b%2Bc%2Fd");
        assert_eq!(uri_encode_segment("café"), "caf%C3%A9");
    }
}
