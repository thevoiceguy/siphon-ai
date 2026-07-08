//! Key-encryption-key sources for recording encryption
//! (DESIGN_RECORDING_COMPLIANCE §2).
//!
//! Envelope encryption splits keys in two: each recording gets a fresh
//! random **DEK** (data-encryption key) that encrypts the audio, and the
//! DEK itself is stored in the container header **wrapped** by the
//! operator's **KEK**. Rotating the KEK never re-encrypts audio — new
//! recordings just name the new `key_id`.
//!
//! [`Kek`] is the provider seam. v0.24.0 ships the static (file/`${cred:}`
//! resolved) variant; the AWS-KMS variant lands with the SigV4 client in
//! v0.25.0 as a new enum arm. (The design note sketched this seam as a
//! trait; an enum keeps it dyn-compatible with async wrap calls once KMS —
//! a network hop — joins, and config compiles to a closed set anyway.)
//!
//! Wrap format (`wrapped_dek` header field): `nonce (12) || AES-256-GCM
//! ciphertext+tag (48)`, AAD = the `key_id` — a wrapped DEK can't be
//! replayed under a different key id.

use aes_gcm::aead::rand_core::RngCore;
use aes_gcm::aead::{Aead, KeyInit, OsRng, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use thiserror::Error;
use zeroize::Zeroizing;

/// Wrapped-DEK length: 12-byte nonce + 32-byte DEK + 16-byte tag.
pub const WRAPPED_DEK_LEN: usize = 12 + 32 + 16;

#[derive(Debug, Error)]
pub enum KekError {
    #[error(
        "recording was wrapped with KEK '{recording}' but the configured key_id is '{configured}'"
    )]
    KeyIdMismatch {
        recording: String,
        configured: String,
    },
    #[error("malformed wrapped DEK (expected {WRAPPED_DEK_LEN} bytes)")]
    MalformedWrap,
    #[error("DEK unwrap failed authentication (wrong KEK?)")]
    UnwrapAuth,
    #[error("DEK wrap failed")]
    WrapFailed,
    #[error("bad KEK encoding: {0}")]
    BadEncoding(&'static str),
}

/// A key-encryption key + its identifier.
///
/// `PartialEq` exists for config-diffing on reload — it compares key bytes
/// non-constant-time, which is fine there (both sides are our own config,
/// never attacker-supplied guesses).
#[derive(Clone, PartialEq, Eq)]
pub enum Kek {
    /// A 32-byte KEK held in memory (resolved from `${file:}` / `${cred:}`
    /// at config load). Zeroized on drop.
    Static {
        key: Zeroizing<[u8; 32]>,
        key_id: String,
    },
}

impl std::fmt::Debug for Kek {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never Debug-print key material.
        match self {
            Kek::Static { key_id, .. } => f
                .debug_struct("Kek::Static")
                .field("key_id", key_id)
                .finish_non_exhaustive(),
        }
    }
}

impl Kek {
    pub fn new_static(key: [u8; 32], key_id: String) -> Self {
        Kek::Static {
            key: Zeroizing::new(key),
            key_id,
        }
    }

    /// Parse a KEK from its 64-hex-char encoding — the `[recording.
    /// encryption].kek` value and the `decrypt-recording` key file. Hex
    /// (not raw bytes) because config secret references splice the value
    /// into TOML text. Surrounding whitespace is tolerated.
    pub fn from_hex(hex: &str, key_id: String) -> Result<Self, KekError> {
        let hex = hex.trim();
        if hex.len() != 64 {
            return Err(KekError::BadEncoding("expected 64 hex characters"));
        }
        let mut key = Zeroizing::new([0u8; 32]);
        for (i, byte) in key.iter_mut().enumerate() {
            let pair = &hex[i * 2..i * 2 + 2];
            *byte = u8::from_str_radix(pair, 16)
                .map_err(|_| KekError::BadEncoding("non-hex character"))?;
        }
        Ok(Kek::Static { key, key_id })
    }

    /// The identifier stamped into container headers.
    pub fn key_id(&self) -> &str {
        match self {
            Kek::Static { key_id, .. } => key_id,
        }
    }

    /// Wrap a DEK for storage in a container header.
    pub fn wrap_dek(&self, dek: &[u8; 32]) -> Result<Vec<u8>, KekError> {
        match self {
            Kek::Static { key, key_id } => {
                let key_bytes: &[u8; 32] = key;
                let cipher = Aes256Gcm::new(key_bytes.into());
                let mut nonce = [0u8; 12];
                OsRng.fill_bytes(&mut nonce);
                let ct = cipher
                    .encrypt(
                        Nonce::from_slice(&nonce),
                        Payload {
                            msg: dek,
                            aad: key_id.as_bytes(),
                        },
                    )
                    .map_err(|_| KekError::WrapFailed)?;
                let mut out = Vec::with_capacity(WRAPPED_DEK_LEN);
                out.extend_from_slice(&nonce);
                out.extend_from_slice(&ct);
                Ok(out)
            }
        }
    }

    /// Unwrap a DEK read from a container header. `recording_key_id` is the
    /// id the container names; it must match this KEK's.
    pub fn unwrap_dek(
        &self,
        recording_key_id: &str,
        wrapped: &[u8],
    ) -> Result<Zeroizing<[u8; 32]>, KekError> {
        match self {
            Kek::Static { key, key_id } => {
                if recording_key_id != key_id {
                    return Err(KekError::KeyIdMismatch {
                        recording: recording_key_id.to_string(),
                        configured: key_id.clone(),
                    });
                }
                if wrapped.len() != WRAPPED_DEK_LEN {
                    return Err(KekError::MalformedWrap);
                }
                let key_bytes: &[u8; 32] = key;
                let cipher = Aes256Gcm::new(key_bytes.into());
                let plain = cipher
                    .decrypt(
                        Nonce::from_slice(&wrapped[..12]),
                        Payload {
                            msg: &wrapped[12..],
                            aad: key_id.as_bytes(),
                        },
                    )
                    .map_err(|_| KekError::UnwrapAuth)?;
                let mut dek = Zeroizing::new([0u8; 32]);
                dek.copy_from_slice(&plain);
                // `plain` holds the raw DEK too — wipe it.
                drop(Zeroizing::new(plain));
                Ok(dek)
            }
        }
    }
}

/// A fresh random 256-bit DEK.
pub fn fresh_dek() -> Zeroizing<[u8; 32]> {
    let mut dek = Zeroizing::new([0u8; 32]);
    OsRng.fill_bytes(&mut *dek);
    dek
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_unwrap_roundtrip() {
        let kek = Kek::new_static([1u8; 32], "k-1".into());
        let dek = fresh_dek();
        let wrapped = kek.wrap_dek(&dek).unwrap();
        assert_eq!(wrapped.len(), WRAPPED_DEK_LEN);
        let back = kek.unwrap_dek("k-1", &wrapped).unwrap();
        assert_eq!(*back, *dek);
    }

    #[test]
    fn wrong_kek_or_key_id_fails() {
        let kek = Kek::new_static([1u8; 32], "k-1".into());
        let wrapped = kek.wrap_dek(&fresh_dek()).unwrap();

        let other = Kek::new_static([2u8; 32], "k-1".into());
        assert!(matches!(
            other.unwrap_dek("k-1", &wrapped),
            Err(KekError::UnwrapAuth)
        ));

        let renamed = Kek::new_static([1u8; 32], "k-2".into());
        assert!(matches!(
            renamed.unwrap_dek("k-1", &wrapped),
            Err(KekError::KeyIdMismatch { .. })
        ));
    }

    #[test]
    fn distinct_deks_and_nonces_every_time() {
        let kek = Kek::new_static([1u8; 32], "k".into());
        let (d1, d2) = (fresh_dek(), fresh_dek());
        assert_ne!(*d1, *d2, "fresh DEKs must differ");
        let (w1, w2) = (kek.wrap_dek(&d1).unwrap(), kek.wrap_dek(&d1).unwrap());
        assert_ne!(w1, w2, "wrap must use a fresh nonce every time");
    }
}
