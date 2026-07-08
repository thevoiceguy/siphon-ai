//! Minimal AWS KMS client — `Encrypt` / `Decrypt` only — on `reqwest` +
//! [`crate::sigv4`] (DESIGN_RECORDING_COMPLIANCE §2: the KMS KEK provider
//! shares the S3 sink's hand-rolled SigV4; no AWS SDK).
//!
//! Used to wrap/unwrap the per-recording DEK when
//! `[recording.encryption.kms]` is configured: `Encrypt` at recording
//! start (the wrapped DEK lands in the container header), `Decrypt` from
//! the `decrypt-recording` tooling. Symmetric-KMS `Decrypt` needs no key
//! id — the ciphertext blob names its key — so tooling only needs region
//! + credentials.
//!
//! Wire shape: `POST /` with `Content-Type: application/x-amz-json-1.1`,
//! `X-Amz-Target: TrentService.<Op>`, JSON body, SigV4 service `"kms"`.
//! `endpoint` is overridable for KMS-compatible emulators (LocalStack).

use serde::Deserialize;
use zeroize::Zeroizing;

use crate::sigv4::{authorization_header, sha256_hex, SigV4Credentials, SignRequest};

/// KMS answers fast or not at all — a wrap that hangs must not stall a
/// recording start indefinitely.
const KMS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

#[derive(Debug, thiserror::Error)]
pub enum KmsError {
    #[error("KMS {op} transport: {source}")]
    Transport {
        op: &'static str,
        #[source]
        source: reqwest::Error,
    },
    #[error("KMS {op}: HTTP {status}: {body}")]
    Status {
        op: &'static str,
        status: u16,
        body: String,
    },
    #[error("KMS {op}: malformed response: {reason}")]
    Malformed { op: &'static str, reason: String },
}

/// Where and as-whom to call KMS.
#[derive(Clone)]
pub struct KmsClient {
    /// `https://kms.<region>.amazonaws.com`, or an emulator URL.
    pub endpoint: String,
    pub region: String,
    pub credentials: SigV4Credentials,
    client: reqwest::Client,
}

impl PartialEq for KmsClient {
    fn eq(&self, other: &Self) -> bool {
        self.endpoint == other.endpoint
            && self.region == other.region
            && self.credentials == other.credentials
    }
}
impl Eq for KmsClient {}

impl std::fmt::Debug for KmsClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KmsClient")
            .field("endpoint", &self.endpoint)
            .field("region", &self.region)
            .finish_non_exhaustive()
    }
}

impl KmsClient {
    /// `endpoint = None` → the public AWS endpoint for `region`.
    pub fn new(region: String, credentials: SigV4Credentials, endpoint: Option<String>) -> Self {
        let endpoint = endpoint
            .unwrap_or_else(|| format!("https://kms.{region}.amazonaws.com"))
            .trim_end_matches('/')
            .to_string();
        Self {
            endpoint,
            region,
            credentials,
            client: reqwest::Client::builder()
                .timeout(KMS_TIMEOUT)
                .build()
                .expect("reqwest client builds"),
        }
    }

    /// `Encrypt`: wrap `plaintext` (the DEK) under `key_arn`. Returns the
    /// ciphertext blob to store in the recording header.
    pub async fn encrypt(&self, key_arn: &str, plaintext: &[u8]) -> Result<Vec<u8>, KmsError> {
        use base64::Engine as _;
        let b64 = base64::engine::general_purpose::STANDARD;
        let body = serde_json::json!({
            "KeyId": key_arn,
            "Plaintext": b64.encode(plaintext),
        })
        .to_string();

        #[derive(Deserialize)]
        struct EncryptResponse {
            #[serde(rename = "CiphertextBlob")]
            ciphertext_blob: String,
        }
        let resp: EncryptResponse = self.call("Encrypt", body).await?;
        b64.decode(&resp.ciphertext_blob)
            .map_err(|e| KmsError::Malformed {
                op: "Encrypt",
                reason: format!("CiphertextBlob base64: {e}"),
            })
    }

    /// `Decrypt`: unwrap a ciphertext blob. Symmetric KMS infers the key
    /// from the blob, so no key id is needed.
    pub async fn decrypt(&self, ciphertext: &[u8]) -> Result<Zeroizing<Vec<u8>>, KmsError> {
        use base64::Engine as _;
        let b64 = base64::engine::general_purpose::STANDARD;
        let body = serde_json::json!({
            "CiphertextBlob": b64.encode(ciphertext),
        })
        .to_string();

        #[derive(Deserialize)]
        struct DecryptResponse {
            #[serde(rename = "Plaintext")]
            plaintext: String,
        }
        let resp: DecryptResponse = self.call("Decrypt", body).await?;
        b64.decode(&resp.plaintext)
            .map(Zeroizing::new)
            .map_err(|e| KmsError::Malformed {
                op: "Decrypt",
                reason: format!("Plaintext base64: {e}"),
            })
    }

    async fn call<T: serde::de::DeserializeOwned>(
        &self,
        op: &'static str,
        body: String,
    ) -> Result<T, KmsError> {
        let host = self
            .endpoint
            .split_once("://")
            .map(|(_, rest)| rest)
            .unwrap_or(&self.endpoint)
            .to_string();
        let target = format!("TrentService.{op}");
        let payload_hash = sha256_hex(body.as_bytes());
        let timestamp = crate::s3::now_amz();
        let auth = authorization_header(
            &self.credentials,
            &SignRequest {
                method: "POST",
                canonical_uri: "/",
                canonical_query: "",
                headers: &[
                    ("content-type", "application/x-amz-json-1.1"),
                    ("host", &host),
                    ("x-amz-date", &timestamp),
                    ("x-amz-target", &target),
                ],
                payload_hash: &payload_hash,
                timestamp: &timestamp,
                region: &self.region,
                service: "kms",
            },
        );
        let response = self
            .client
            .post(format!("{}/", self.endpoint))
            .header("authorization", auth)
            .header("content-type", "application/x-amz-json-1.1")
            .header("x-amz-target", &target)
            .header("x-amz-date", &timestamp)
            .body(body)
            .send()
            .await
            .map_err(|source| KmsError::Transport { op, source })?;

        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(KmsError::Status {
                op,
                status: status.as_u16(),
                body: text.chars().take(1024).collect(),
            });
        }
        serde_json::from_str(&text).map_err(|e| KmsError::Malformed {
            op,
            reason: e.to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_endpoint_is_regional_aws() {
        let c = KmsClient::new(
            "eu-west-2".into(),
            SigV4Credentials {
                access_key: "a".into(),
                secret_key: "s".into(),
            },
            None,
        );
        assert_eq!(c.endpoint, "https://kms.eu-west-2.amazonaws.com");
        let c = KmsClient::new(
            "us-east-1".into(),
            SigV4Credentials {
                access_key: "a".into(),
                secret_key: "s".into(),
            },
            Some("http://127.0.0.1:4566/".into()),
        );
        assert_eq!(c.endpoint, "http://127.0.0.1:4566");
    }
}
