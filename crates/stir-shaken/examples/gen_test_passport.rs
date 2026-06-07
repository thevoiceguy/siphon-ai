//! Generate a self-contained STIR/SHAKEN test rig: a throwaway CA, a leaf
//! signing cert, an x5u TLS server cert, and a fully ES256-signed PASSporT
//! `Identity` header whose `iat` is *now* — so a verifier configured to
//! trust the CA accepts the resulting call.
//!
//! Used by `test-harness/sipp-scenarios/run-all.sh` for the passing-
//! attestation scenario, and handy as an operator lab tool. Everything is
//! generated at run time so the cert validity window and `iat` are current.
//!
//! Usage:
//!   gen_test_passport <out_dir> <x5u_url> <orig_tn> <dest_tn>
//!
//! Writes into `<out_dir>`:
//!   ca.pem       — the CA (use as both `trust_anchors` and
//!                  `x5u_tls_extra_ca`)
//!   leaf.crt     — the SHAKEN signing cert, served at `<x5u_url>`
//!   server.crt   — the x5u HTTPS server cert (SAN: IP 127.0.0.1)
//!   server.key   — its private key
//! Prints the full `Identity` header value to stdout (and nothing else).

use std::error::Error;
use std::net::{IpAddr, Ipv4Addr};
use std::time::{SystemTime, UNIX_EPOCH};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, IsCa,
    KeyPair, KeyUsagePurpose, SanType, PKCS_ECDSA_P256_SHA256,
};
use ring::rand::SystemRandom;
use ring::signature::{EcdsaKeyPair, ECDSA_P256_SHA256_FIXED_SIGNING};
use time::OffsetDateTime;

fn b64(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 5 {
        eprintln!("usage: gen_test_passport <out_dir> <x5u_url> <orig_tn> <dest_tn>");
        std::process::exit(2);
    }
    let out_dir = std::path::PathBuf::from(&args[1]);
    let x5u_url = &args[2];
    let orig_tn = &args[3];
    let dest_tn = &args[4];

    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64;
    let not_before = OffsetDateTime::from_unix_timestamp(now - 3600)?;
    let not_after = OffsetDateTime::from_unix_timestamp(now + 365 * 24 * 3600)?;

    // Distinct DNs per cert — if subject == issuer the certs look
    // self-signed to a verifier and the chain won't build.
    let dn = |cn: &str| {
        let mut d = DistinguishedName::new();
        d.push(DnType::CommonName, cn);
        d
    };

    // ── CA (acts as the STI-PA root AND the x5u-TLS root) ──────────────
    let ca_key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256)?;
    let mut ca_params = CertificateParams::new(Vec::new())?;
    ca_params.distinguished_name = dn("siphon-test-sti-pa-root");
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params.key_usages = vec![
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::CrlSign,
        KeyUsagePurpose::DigitalSignature,
    ];
    ca_params.not_before = not_before;
    ca_params.not_after = not_after;
    let ca = ca_params.self_signed(&ca_key)?;

    // ── Leaf SHAKEN signing cert (chains to the CA) ────────────────────
    let leaf_key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256)?;
    let mut leaf_params = CertificateParams::new(Vec::new())?;
    leaf_params.distinguished_name = dn("sti.test.siphon");
    leaf_params.not_before = not_before;
    leaf_params.not_after = not_after;
    let leaf = leaf_params.signed_by(&leaf_key, &ca, &ca_key)?;

    // ── x5u HTTPS server cert (TLS; SAN must match the fetch host) ─────
    let server_key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256)?;
    let mut server_params = CertificateParams::new(Vec::new())?;
    server_params.distinguished_name = dn("x5u.test.siphon");
    server_params.subject_alt_names = vec![SanType::IpAddress(IpAddr::V4(Ipv4Addr::LOCALHOST))];
    server_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    server_params.not_before = not_before;
    server_params.not_after = not_after;
    let server = server_params.signed_by(&server_key, &ca, &ca_key)?;

    // ── Sign the PASSporT with the leaf's key (ES256, JOSE fixed r‖s) ──
    let header = format!(r#"{{"alg":"ES256","typ":"passport","ppt":"shaken","x5u":"{x5u_url}"}}"#);
    let payload = format!(
        r#"{{"attest":"A","dest":{{"tn":["{dest_tn}"]}},"iat":{now},"orig":{{"tn":"{orig_tn}"}},"origid":"123e4567-e89b-12d3-a456-426655440000"}}"#
    );
    let signing_input = format!("{}.{}", b64(header.as_bytes()), b64(payload.as_bytes()));
    let rng = SystemRandom::new();
    let signer = EcdsaKeyPair::from_pkcs8(
        &ECDSA_P256_SHA256_FIXED_SIGNING,
        &leaf_key.serialize_der(),
        &rng,
    )
    .map_err(|e| format!("load leaf key into ring: {e}"))?;
    let sig = signer
        .sign(&rng, signing_input.as_bytes())
        .map_err(|e| format!("sign PASSporT: {e}"))?;
    let token = format!("{signing_input}.{}", b64(sig.as_ref()));
    let identity = format!("{token};info=<{x5u_url}>;alg=ES256;ppt=shaken");

    // ── Emit ──────────────────────────────────────────────────────────
    std::fs::create_dir_all(&out_dir)?;
    std::fs::write(out_dir.join("ca.pem"), ca.pem())?;
    std::fs::write(out_dir.join("leaf.crt"), leaf.pem())?;
    std::fs::write(out_dir.join("server.crt"), server.pem())?;
    std::fs::write(out_dir.join("server.key"), server_key.serialize_pem())?;

    // Only the Identity header value goes to stdout, so callers can capture it.
    println!("{identity}");
    Ok(())
}
