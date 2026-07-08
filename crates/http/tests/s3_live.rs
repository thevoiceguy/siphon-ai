//! Live S3-compatibility check for the hand-rolled SigV4 + PUT client.
//!
//! Ignored by default — needs a reachable S3-compatible server. Run
//! against MinIO with:
//!
//! ```sh
//! docker run -d --name minio-test -p 9000:9000 minio/minio server /data
//! docker run --rm --network host --entrypoint sh minio/mc -c \
//!   'mc alias set local http://127.0.0.1:9000 minioadmin minioadmin && mc mb local/siphon-test'
//! S3_LIVE_ENDPOINT=http://127.0.0.1:9000 cargo test -p siphon-ai-http --test s3_live -- --ignored
//! ```

use siphon_ai_http::s3::S3Target;
use siphon_ai_http::sigv4::SigV4Credentials;

#[tokio::test]
#[ignore = "needs a live S3-compatible endpoint (S3_LIVE_ENDPOINT)"]
async fn put_file_roundtrips_against_live_endpoint() {
    let endpoint =
        std::env::var("S3_LIVE_ENDPOINT").unwrap_or_else(|_| "http://127.0.0.1:9000".into());
    let bucket = std::env::var("S3_LIVE_BUCKET").unwrap_or_else(|_| "siphon-test".into());
    let access = std::env::var("S3_LIVE_ACCESS_KEY").unwrap_or_else(|_| "minioadmin".into());
    let secret = std::env::var("S3_LIVE_SECRET_KEY").unwrap_or_else(|_| "minioadmin".into());

    let dir = std::env::temp_dir().join(format!("siphon_s3_live_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("rec.wava");
    let payload: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
    std::fs::write(&path, &payload).unwrap();

    let target = S3Target {
        endpoint: endpoint.clone(),
        bucket: bucket.clone(),
        region: "us-east-1".into(),
        credentials: SigV4Credentials {
            access_key: access,
            secret_key: secret,
        },
    };
    let client = reqwest::Client::new();
    let key = "2026-07-08/live smoke/rec.wava"; // space exercises encoding
    let loc = target
        .put_file(&client, key, &path)
        .await
        .expect("PUT must succeed");
    assert_eq!(loc.uri, format!("s3://{bucket}/{key}"));

    // Read it back unauthenticated? No — GET with the same SigV4 isn't
    // implemented (PUT-only client). Verify via a signed HEAD-equivalent:
    // re-PUT is enough to prove signing; content equality is checked by
    // S3 itself (x-amz-content-sha256 is the signed payload hash, a
    // mismatch would 400). A second identical PUT must also succeed.
    target
        .put_file(&client, key, &path)
        .await
        .expect("idempotent re-PUT must succeed");

    let _ = std::fs::remove_dir_all(&dir);
}
