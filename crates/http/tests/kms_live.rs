//! Live KMS-compatibility check for the hand-rolled Encrypt/Decrypt
//! client. Ignored by default — needs a KMS endpoint. Run against
//! LocalStack with:
//!
//! ```sh
//! docker run -d --name localstack-test -p 4566:4566 -e SERVICES=kms localstack/localstack
//! # create a key, then:
//! KMS_LIVE_ENDPOINT=http://127.0.0.1:4566 KMS_LIVE_KEY_ARN=<arn> \
//!   cargo test -p siphon-ai-http --test kms_live -- --ignored
//! ```

use siphon_ai_http::kms::KmsClient;
use siphon_ai_http::sigv4::SigV4Credentials;

#[tokio::test]
#[ignore = "needs a live KMS endpoint (KMS_LIVE_ENDPOINT + KMS_LIVE_KEY_ARN)"]
async fn encrypt_decrypt_roundtrips_against_live_kms() {
    let endpoint =
        std::env::var("KMS_LIVE_ENDPOINT").unwrap_or_else(|_| "http://127.0.0.1:4566".into());
    let key_arn = std::env::var("KMS_LIVE_KEY_ARN").expect("KMS_LIVE_KEY_ARN required");
    let client = KmsClient::new(
        std::env::var("KMS_LIVE_REGION").unwrap_or_else(|_| "us-east-1".into()),
        SigV4Credentials {
            access_key: std::env::var("KMS_LIVE_ACCESS_KEY").unwrap_or_else(|_| "test".into()),
            secret_key: std::env::var("KMS_LIVE_SECRET_KEY").unwrap_or_else(|_| "test".into()),
        },
        Some(endpoint),
    );

    let dek = [0x42u8; 32];
    let blob = client.encrypt(&key_arn, &dek).await.expect("Encrypt");
    assert!(blob.len() > 32, "blob must be larger than the plaintext");
    let back = client.decrypt(&blob).await.expect("Decrypt");
    assert_eq!(&**back, &dek[..], "roundtrip must restore the DEK");

    // Tampered blob must fail, not return garbage.
    let mut bad = blob.clone();
    let n = bad.len();
    bad[n - 1] ^= 0xFF;
    assert!(
        client.decrypt(&bad).await.is_err(),
        "tampered blob must fail"
    );
}
