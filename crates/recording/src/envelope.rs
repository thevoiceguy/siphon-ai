//! The `.wava` encrypted-recording container (DESIGN_RECORDING_COMPLIANCE §2).
//!
//! Envelope encryption: a fresh random 256-bit DEK per recording encrypts
//! the payload in independent AES-256-GCM chunks; the DEK travels in the
//! header, wrapped by the operator's KEK (see [`crate::kek`]). The inner
//! payload is a byte-exact standard WAV, so `decrypt` output is playable
//! as-is.
//!
//! ## On-disk layout (all integers little-endian)
//!
//! ```text
//! header:  magic "SAIWAVA1"
//!          key_id_len u8 | key_id (utf-8)
//!          wrapped_dek_len u16 | wrapped_dek        (see kek::wrap_dek)
//!          chunk_size u32                           (plaintext bytes/chunk)
//! chunk i: generation u32 | ct_len u32 | ciphertext (plaintext + 16-byte tag)
//! ```
//!
//! Chunk `i`'s GCM nonce is `chunk_index u64 || generation u32` (12 bytes),
//! and the AAD for every chunk is the full serialized header — a chunk
//! can't be replayed into a different recording or under a different key id.
//!
//! ## The chunk-0 rewrite
//!
//! WAV finalize back-patches the RIFF sizes at offsets 4 and 40 — inside a
//! sealed stream that's impossible, so finalize **rewrites chunk 0**: the
//! writer retains chunk 0's plaintext, patches the sizes, and re-seals it
//! with `generation = 1`. Bumping the generation changes the nonce (GCM
//! nonce reuse is catastrophic), and because the plaintext length is
//! unchanged the ciphertext overwrites in place. A finalized file therefore
//! has `generation == 1` on chunk 0 and `0` everywhere else — the decoder
//! enforces exactly that, so a truncated/unfinalized capture (a crashed
//! `.part`) is distinguishable and an attacker can't swap the placeholder
//! chunk 0 back in.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use thiserror::Error;
use tokio::fs::File;
use tokio::io::{AsyncSeekExt, AsyncWriteExt, BufWriter};
use zeroize::Zeroizing;

use crate::kek::Kek;

/// Container magic; the trailing `1` is the format version.
pub const MAGIC: &[u8; 8] = b"SAIWAVA1";
/// Plaintext bytes per chunk. Matches the writer's flush cadence (~1 s of
/// stereo 16 kHz audio) so steady-state writes seal whole chunks.
pub const CHUNK_SIZE: usize = 64 * 1024;
/// GCM tag length.
const TAG_LEN: usize = 16;
/// Per-chunk on-disk framing: `generation u32 | ct_len u32`.
const CHUNK_FRAME_LEN: usize = 8;

#[derive(Debug, Error)]
pub enum EnvelopeError {
    #[error("envelope I/O failed for {path:?}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("not a SiphonAI encrypted recording (bad magic)")]
    BadMagic,
    #[error("malformed envelope header: {0}")]
    BadHeader(&'static str),
    #[error("DEK unwrap failed: {0}")]
    Kek(#[from] crate::kek::KekError),
    #[error("chunk {index}: {reason}")]
    BadChunk { index: u64, reason: &'static str },
    #[error(
        "chunk 0 is unfinalized (generation 0) — the recording was cut off \
         before finalize; its WAV header sizes are placeholders"
    )]
    Unfinalized,
    #[error("chunk {index} failed authentication (wrong key or tampered data)")]
    Auth { index: u64 },
}

impl EnvelopeError {
    fn io(path: &Path, source: std::io::Error) -> Self {
        Self::Io {
            path: path.to_path_buf(),
            source,
        }
    }
}

/// `chunk_index || generation` → the 12-byte GCM nonce.
fn nonce_for(index: u64, generation: u32) -> [u8; 12] {
    let mut n = [0u8; 12];
    n[..8].copy_from_slice(&index.to_le_bytes());
    n[8..].copy_from_slice(&generation.to_le_bytes());
    n
}

/// Serialized header for a new envelope; also the AAD for every chunk.
fn build_header(key_id: &str, wrapped_dek: &[u8]) -> Vec<u8> {
    let mut h = Vec::with_capacity(MAGIC.len() + 1 + key_id.len() + 2 + wrapped_dek.len() + 4);
    h.extend_from_slice(MAGIC);
    h.push(key_id.len() as u8);
    h.extend_from_slice(key_id.as_bytes());
    h.extend_from_slice(&(wrapped_dek.len() as u16).to_le_bytes());
    h.extend_from_slice(wrapped_dek);
    h.extend_from_slice(&(CHUNK_SIZE as u32).to_le_bytes());
    h
}

/// Streaming encrypting writer. Feed plaintext with [`Self::write`]; call
/// [`Self::finalize`] with the WAV header patches to seal the container.
///
/// Owns the output `File` (the caller creates it — typically a `.part`
/// path it renames after finalize). Retains chunk 0's plaintext (≤ 64 KiB,
/// per-call, off the hot path) for the finalize rewrite.
pub struct EnvelopeWriter {
    path: PathBuf,
    out: BufWriter<File>,
    cipher: Aes256Gcm,
    header: Vec<u8>,
    /// Plaintext staged for the next chunk (< CHUNK_SIZE between writes).
    pending: Vec<u8>,
    /// Chunk 0's plaintext, kept for the finalize rewrite.
    first_chunk: Option<Vec<u8>>,
    /// File offset where chunk 0's frame begins (== header length).
    chunks_start: u64,
    next_index: u64,
}

impl EnvelopeWriter {
    /// Wrap a fresh random DEK with `kek` and write the container header.
    pub async fn create(path: &Path, file: File, kek: &Kek) -> Result<Self, EnvelopeError> {
        let dek: Zeroizing<[u8; 32]> = crate::kek::fresh_dek();
        let wrapped = kek.wrap_dek(&dek)?;
        let header = build_header(kek.key_id(), &wrapped);
        let dek_bytes: &[u8; 32] = &dek;
        let cipher = Aes256Gcm::new(dek_bytes.into());
        let mut out = BufWriter::new(file);
        out.write_all(&header)
            .await
            .map_err(|e| EnvelopeError::io(path, e))?;
        Ok(Self {
            path: path.to_path_buf(),
            chunks_start: header.len() as u64,
            out,
            cipher,
            header,
            pending: Vec::with_capacity(CHUNK_SIZE),
            first_chunk: None,
            next_index: 0,
        })
    }

    /// Append plaintext; seals and writes a chunk each time a full
    /// `CHUNK_SIZE` accumulates.
    pub async fn write(&mut self, mut data: &[u8]) -> Result<(), EnvelopeError> {
        while !data.is_empty() {
            let take = (CHUNK_SIZE - self.pending.len()).min(data.len());
            self.pending.extend_from_slice(&data[..take]);
            data = &data[take..];
            if self.pending.len() == CHUNK_SIZE {
                self.seal_pending().await?;
            }
        }
        Ok(())
    }

    async fn seal_pending(&mut self) -> Result<(), EnvelopeError> {
        let index = self.next_index;
        let plain = std::mem::replace(&mut self.pending, Vec::with_capacity(CHUNK_SIZE));
        let ct = self.seal(index, 0, &plain)?;
        self.write_chunk_frame(0, &ct).await?;
        if index == 0 {
            self.first_chunk = Some(plain);
        }
        self.next_index += 1;
        Ok(())
    }

    fn seal(&self, index: u64, generation: u32, plain: &[u8]) -> Result<Vec<u8>, EnvelopeError> {
        self.cipher
            .encrypt(
                Nonce::from_slice(&nonce_for(index, generation)),
                Payload {
                    msg: plain,
                    aad: &self.header,
                },
            )
            .map_err(|_| EnvelopeError::BadChunk {
                index,
                reason: "encryption failed",
            })
    }

    async fn write_chunk_frame(&mut self, generation: u32, ct: &[u8]) -> Result<(), EnvelopeError> {
        let mut frame = [0u8; CHUNK_FRAME_LEN];
        frame[..4].copy_from_slice(&generation.to_le_bytes());
        frame[4..].copy_from_slice(&(ct.len() as u32).to_le_bytes());
        self.out
            .write_all(&frame)
            .await
            .map_err(|e| EnvelopeError::io(&self.path, e))?;
        self.out
            .write_all(ct)
            .await
            .map_err(|e| EnvelopeError::io(&self.path, e))?;
        Ok(())
    }

    /// Seal the trailing partial chunk, apply the WAV size patches to chunk
    /// 0, and rewrite it with `generation = 1`. `patches` are
    /// `(plaintext_offset, value)` pairs — both WAV size fields sit inside
    /// chunk 0 (offsets 4 and 40 ≪ CHUNK_SIZE).
    pub async fn finalize(mut self, patches: &[(usize, u32)]) -> Result<(), EnvelopeError> {
        if !self.pending.is_empty() || self.next_index == 0 {
            self.seal_pending().await?;
        }
        let mut chunk0 = self.first_chunk.take().ok_or(EnvelopeError::BadChunk {
            index: 0,
            reason: "no chunk 0 to finalize",
        })?;
        for &(offset, value) in patches {
            if offset + 4 > chunk0.len() {
                return Err(EnvelopeError::BadChunk {
                    index: 0,
                    reason: "patch offset outside chunk 0",
                });
            }
            chunk0[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
        }
        // Same plaintext length ⇒ same ciphertext length ⇒ in-place
        // overwrite; generation 1 ⇒ a fresh nonce for the rewrite.
        let ct = self.seal(0, 1, &chunk0)?;
        self.out
            .flush()
            .await
            .map_err(|e| EnvelopeError::io(&self.path, e))?;
        self.out
            .seek(std::io::SeekFrom::Start(self.chunks_start))
            .await
            .map_err(|e| EnvelopeError::io(&self.path, e))?;
        self.write_chunk_frame(1, &ct).await?;
        self.out
            .flush()
            .await
            .map_err(|e| EnvelopeError::io(&self.path, e))?;
        Ok(())
    }
}

/// Decrypt a `.wava` container into `out`, returning the payload byte
/// count. Synchronous — used by the `decrypt-recording` subcommand and
/// tests, never on a call path.
///
/// `allow_unfinalized` accepts a chunk-0 `generation` of 0 (a crashed
/// `.part` capture) — the recovered WAV then has placeholder (zero) size
/// fields the caller must expect.
/// Parse a container header: `(key_id, wrapped_dek, chunk_size)`.
fn read_header<R: Read>(input: &mut R) -> Result<(String, Vec<u8>, usize), EnvelopeError> {
    let err_io = |e: std::io::Error| EnvelopeError::Io {
        path: PathBuf::from("<input>"),
        source: e,
    };
    let mut magic = [0u8; 8];
    input.read_exact(&mut magic).map_err(err_io)?;
    if &magic != MAGIC {
        return Err(EnvelopeError::BadMagic);
    }
    let mut len1 = [0u8; 1];
    input.read_exact(&mut len1).map_err(err_io)?;
    let mut key_id = vec![0u8; len1[0] as usize];
    input.read_exact(&mut key_id).map_err(err_io)?;
    let key_id = String::from_utf8(key_id).map_err(|_| EnvelopeError::BadHeader("key_id utf8"))?;
    let mut len2 = [0u8; 2];
    input.read_exact(&mut len2).map_err(err_io)?;
    let mut wrapped = vec![0u8; u16::from_le_bytes(len2) as usize];
    input.read_exact(&mut wrapped).map_err(err_io)?;
    let mut cs = [0u8; 4];
    input.read_exact(&mut cs).map_err(err_io)?;
    let chunk_size = u32::from_le_bytes(cs) as usize;
    if chunk_size == 0 || chunk_size > 16 * 1024 * 1024 {
        return Err(EnvelopeError::BadHeader("unreasonable chunk_size"));
    }
    Ok((key_id, wrapped, chunk_size))
}

/// Read just the `key_id` a container names — lets tooling tell the
/// operator *which* KEK a recording needs without attempting decryption.
pub fn peek_key_id<R: Read>(mut input: R) -> Result<String, EnvelopeError> {
    read_header(&mut input).map(|(key_id, _, _)| key_id)
}

pub fn decrypt<R: Read, W: Write>(
    mut input: R,
    out: &mut W,
    kek: &Kek,
    allow_unfinalized: bool,
) -> Result<u64, EnvelopeError> {
    let err_io = |e: std::io::Error| EnvelopeError::Io {
        path: PathBuf::from("<input>"),
        source: e,
    };

    // Header — reconstructed byte-for-byte, since it is every chunk's AAD.
    let (key_id, wrapped, chunk_size) = read_header(&mut input)?;
    let header = build_header(&key_id, &wrapped);

    let dek = kek.unwrap_dek(&key_id, &wrapped)?;
    let dek_bytes: &[u8; 32] = &dek;
    let cipher = Aes256Gcm::new(dek_bytes.into());

    let mut index = 0u64;
    let mut total = 0u64;
    loop {
        let mut frame = [0u8; CHUNK_FRAME_LEN];
        match input.read_exact(&mut frame) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof && index > 0 => break,
            Err(e) => return Err(err_io(e)),
        }
        let generation = u32::from_le_bytes(frame[..4].try_into().expect("4 bytes"));
        let ct_len = u32::from_le_bytes(frame[4..].try_into().expect("4 bytes")) as usize;
        if ct_len < TAG_LEN || ct_len > chunk_size + TAG_LEN {
            return Err(EnvelopeError::BadChunk {
                index,
                reason: "ciphertext length out of range",
            });
        }
        match (index, generation) {
            (0, 1) => {}
            (0, 0) if allow_unfinalized => {}
            (0, 0) => return Err(EnvelopeError::Unfinalized),
            (_, 0) if index > 0 => {}
            _ => {
                return Err(EnvelopeError::BadChunk {
                    index,
                    reason: "unexpected generation",
                })
            }
        }
        let mut ct = vec![0u8; ct_len];
        input.read_exact(&mut ct).map_err(err_io)?;
        let plain = cipher
            .decrypt(
                Nonce::from_slice(&nonce_for(index, generation)),
                Payload {
                    msg: &ct,
                    aad: &header,
                },
            )
            .map_err(|_| EnvelopeError::Auth { index })?;
        if index > 0 && plain.len() != chunk_size && {
            // Only the final chunk may be short — peek for more data.
            let mut probe = [0u8; 1];
            match input.read_exact(&mut probe) {
                Ok(()) => true, // more chunks follow a short one → malformed
                Err(_) => false,
            }
        } {
            return Err(EnvelopeError::BadChunk {
                index,
                reason: "short chunk before end of file",
            });
        }
        out.write_all(&plain).map_err(err_io)?;
        total += plain.len() as u64;
        if plain.len() != chunk_size {
            break; // final (short) chunk
        }
        index += 1;
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kek::Kek;

    fn test_kek() -> Kek {
        Kek::new_static([7u8; 32], "test-key".into())
    }

    fn temp_file(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("siphon_env_{}_{}", std::process::id(), tag));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("r.wava")
    }

    async fn write_envelope(path: &Path, kek: &Kek, payload: &[u8], patches: &[(usize, u32)]) {
        let file = File::create(path).await.unwrap();
        let mut w = EnvelopeWriter::create(path, file, kek).await.unwrap();
        // Write in awkward sizes to exercise chunk-boundary slicing.
        for piece in payload.chunks(CHUNK_SIZE / 3 + 11) {
            w.write(piece).await.unwrap();
        }
        w.finalize(patches).await.unwrap();
    }

    fn decrypt_file(
        path: &Path,
        kek: &Kek,
        allow_unfinalized: bool,
    ) -> Result<Vec<u8>, EnvelopeError> {
        let f = std::fs::File::open(path).unwrap();
        let mut out = Vec::new();
        decrypt(std::io::BufReader::new(f), &mut out, kek, allow_unfinalized)?;
        Ok(out)
    }

    #[tokio::test]
    async fn roundtrip_multi_chunk_with_patches() {
        let path = temp_file("roundtrip");
        let kek = test_kek();
        // 2.5 chunks of recognizable data.
        let payload: Vec<u8> = (0..CHUNK_SIZE * 5 / 2).map(|i| (i % 251) as u8).collect();
        write_envelope(&path, &kek, &payload, &[(4, 0xAABBCCDD), (40, 0x11223344)]).await;

        let plain = decrypt_file(&path, &kek, false).unwrap();
        let mut expected = payload.clone();
        expected[4..8].copy_from_slice(&0xAABBCCDDu32.to_le_bytes());
        expected[40..44].copy_from_slice(&0x11223344u32.to_le_bytes());
        assert_eq!(plain, expected, "decrypt must yield patched payload");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[tokio::test]
    async fn tiny_payload_single_chunk() {
        let path = temp_file("tiny");
        let kek = test_kek();
        let payload = vec![0x5A; 100]; // < one chunk, still gets patched + finalized
        write_envelope(&path, &kek, &payload, &[(4, 56)]).await;
        let plain = decrypt_file(&path, &kek, false).unwrap();
        assert_eq!(plain.len(), 100);
        assert_eq!(&plain[4..8], &56u32.to_le_bytes());
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[tokio::test]
    async fn wrong_kek_fails_auth_not_panics() {
        let path = temp_file("wrongkey");
        let kek = test_kek();
        write_envelope(&path, &kek, &vec![1u8; 500], &[]).await;
        let other = Kek::new_static([9u8; 32], "test-key".into());
        match decrypt_file(&path, &other, false) {
            Err(EnvelopeError::Kek(_)) => {}
            other => panic!("expected KEK unwrap failure, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[tokio::test]
    async fn tampered_ciphertext_fails_auth() {
        let path = temp_file("tamper");
        let kek = test_kek();
        write_envelope(&path, &kek, &vec![3u8; CHUNK_SIZE + 300], &[]).await;
        // Flip one byte late in the file (inside chunk 1's ciphertext).
        let mut bytes = std::fs::read(&path).unwrap();
        let n = bytes.len();
        bytes[n - 10] ^= 0xFF;
        std::fs::write(&path, &bytes).unwrap();
        match decrypt_file(&path, &kek, false) {
            Err(EnvelopeError::Auth { index: 1 }) => {}
            other => panic!("expected auth failure on chunk 1, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[tokio::test]
    async fn unfinalized_rejected_unless_allowed() {
        let path = temp_file("unfinal");
        let kek = test_kek();
        // Write without finalize: chunk 0 keeps generation 0.
        let file = File::create(&path).await.unwrap();
        let mut w = EnvelopeWriter::create(&path, file, &kek).await.unwrap();
        w.write(&vec![8u8; CHUNK_SIZE]).await.unwrap(); // seals chunk 0 (gen 0)
        w.out.flush().await.unwrap();
        drop(w);

        match decrypt_file(&path, &kek, false) {
            Err(EnvelopeError::Unfinalized) => {}
            other => panic!("expected Unfinalized, got {other:?}"),
        }
        let plain = decrypt_file(&path, &kek, true).unwrap();
        assert_eq!(plain, vec![8u8; CHUNK_SIZE]);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[tokio::test]
    async fn chunk0_placeholder_swap_is_rejected() {
        // An attacker restoring the pre-finalize chunk 0 (generation 0)
        // into a finalized file must be caught by the generation rule.
        let path = temp_file("swap");
        let kek = test_kek();

        // Capture the pre-finalize bytes of chunk 0 by writing one chunk,
        // flushing, and snapshotting the file before finalize.
        let file = File::create(&path).await.unwrap();
        let mut w = EnvelopeWriter::create(&path, file, &kek).await.unwrap();
        w.write(&vec![4u8; CHUNK_SIZE + 32]).await.unwrap();
        w.out.flush().await.unwrap();
        let before = std::fs::read(&path).unwrap();
        w.finalize(&[(4, 999)]).await.unwrap();
        let mut after = std::fs::read(&path).unwrap();

        // Splice the old (gen 0) chunk-0 frame back over the finalized one.
        // `before` = header + chunk-0 frame only (the 32 trailing bytes were
        // still pending), so chunk 0 ends exactly at `before.len()`.
        let chunk0_frame_len = CHUNK_FRAME_LEN + CHUNK_SIZE + TAG_LEN;
        let start = before.len() - chunk0_frame_len;
        after[start..start + chunk0_frame_len]
            .copy_from_slice(&before[start..start + chunk0_frame_len]);
        std::fs::write(&path, &after).unwrap();

        match decrypt_file(&path, &kek, false) {
            Err(EnvelopeError::Unfinalized) => {}
            other => panic!("expected Unfinalized on swapped chunk 0, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}
