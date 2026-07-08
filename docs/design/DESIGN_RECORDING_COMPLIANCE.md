# Design: recording compliance & storage (P1 theme)

> **Status: DECISIONS LOCKED (2026-07-08) — §6.** Same design-first cadence
> as observability (→ v0.21–0.23) and security hardening (→ v0.18–0.20):
> design note → locked decisions → chunked PRs → tag-after-merge. The build
> follows §7; deviations get noted back here.

Theme: **P1 "Recording: compliance & storage" from `docs/ROADMAP.md`**, the
next open theme now that observability completeness is done (WS trace
propagation → v0.23.0). The roadmap frames it as: *"Recording (0.5.0) writes
a plaintext WAV to a local dir — fine for a lab, short of what regulated
industries (PCI/HIPAA/call-center) need."* Five sub-items:

1. **Encryption at rest** — envelope encryption with a KMS hook (the
   compliance blocker).
2. **Object-storage sink** — S3-compatible upload + retention, instead of
   local-disk-only.
3. **Consent / announcement hooks** — a configurable "this call may be
   recorded" prompt before capture starts.
4. **Compression & format** — Opus output (smaller than WAV) + path
   templating.
5. **Outbound recording** — extend capture to outbound legs.

The headline finding from the code survey: **the hard part (hot-path-safe
capture) is done; everything here is finalize-time or off-path.** Frames
reach the writer over a bounded channel with `try_send` (never blocks the
audio path, `crates/core/src/call.rs:727-763`), the writer streams to a
`BufWriter` in ≈64 KiB flushes (`crates/recording/src/writer.rs:244-260`),
and the repo already has every infrastructure pattern this theme needs: a
durable spool + drain worker (`crates/http/src/lib.rs`), HMAC-SHA256 signing
(`hmac`/`sha2` are workspace deps), the `${file:}`/`${cred:}` secret
resolver (`crates/config/src/env.rs`), and libopus (via forge-engine's
`opus` feature). Nothing here needs a `forge-media` or `siphon-rs` change
except possibly announcement playback (§4).

---

## 1. The gaps today (docs/RECORDING.md §6–7, code survey)

- **Plaintext at rest, even for encrypted calls.** `docs/RECORDING.md` §6 is
  explicit: a call can be SRTP + WSS end-to-end and the WAV on disk is still
  cleartext. For PCI/HIPAA deployments this is the blocker.
- **Local-file sink only.** Output is `<dir>/<call_id>.wav`
  (`crates/recording/src/config.rs:38-40`), flat, no templating. No upload,
  no retention — "the daemon never deletes" (§6).
- **No finalize atomicity.** The writer creates the file at its final path
  with a zeroed 44-byte placeholder header and patches sizes at finalize
  (`writer.rs:230-286`). A crash leaves a valid-looking `.wav` with a broken
  header, and a downstream consumer can't tell "in progress" from "dead".
- **Consent is entirely the operator's problem.** §6: "SiphonAI does not
  insert prompts. Your WS server can play the prompt." There is no way to
  play any announcement into a non-parked call today (`MohSource` exists but
  is wired only to park/hold, `crates/media-glue/src/moh.rs`), and nothing
  records *whether* consent happened.
- **WAV/PCM16 only.** ~230 MB/hour at 16 kHz stereo; the 32-bit RIFF sizes
  saturate at 4 GiB (`writer.rs:268-279`).
- **Inbound legs only.** Recording is resolved in the inbound acceptor
  (`crates/core/src/acceptor.rs:3474-3492`); originated (outbound) calls
  never get a writer.

---

## 2. Sub-item 1 — Encryption at rest (→ v0.24.0)

### Threat model

Protect recordings against disk theft, stale backups, and object-store
exfiltration. The daemon inevitably sees plaintext audio (it *produces* it),
so the goal is: **ciphertext-only at rest, key material never stored next to
data, and a key-rotation / KMS story.** Envelope encryption is the standard
shape: per-recording random data key (DEK), wrapped by a key-encryption key
(KEK) that lives elsewhere.

### Proposed format: chunked AES-256-GCM envelope (`.wava` container)

A recording streams for its whole life, so one-shot AEAD is out; we encrypt
**independent 64 KiB chunks** (matching today's flush cadence):

```
header:  magic "SAIWAVA1" | key_id (u8 len + utf8) | wrapped_DEK | chunk_size
chunk i: AES-256-GCM(DEK, nonce = chunk_index_u64 || generation_u32, data)
```

- Fresh random 256-bit DEK per recording (`getrandom`, zeroized on drop).
- **The WAV header-patch problem**: today finalize seeks back to patch RIFF
  sizes — impossible inside a sealed stream. Solution: chunk 0 is
  **rewritten at finalize** with the patched sizes, using a bumped
  `generation` counter in its nonce (nonce reuse under GCM is catastrophic;
  the generation field makes the rewrite a *different* nonce, and the
  decoder accepts only the highest generation for chunk 0). This keeps the
  inner payload a byte-exact standard WAV.
- **Finalize atomicity rides along**: encrypted recordings write to
  `<name>.part` and rename on successful finalize — fixing the §1 crash
  ambiguity for the compliance-sensitive path. (Plaintext WAV behavior is
  unchanged; changing it is a separate decision, §6 D5.)
- **Decryption tooling ships in the same release**: a
  `siphon-ai decrypt-recording <file>` subcommand (the daemon binary already
  has `check`/`print-config`/`route-test` subcommands) + the format
  documented in `docs/RECORDING.md` so third parties can implement it.

Dependency impact: `aes-gcm` + `zeroize` promoted from transitive to direct
workspace deps — **no new vendor enters the tree** (both are RustCrypto,
already in `Cargo.lock`). The alternative considered — the `age` format
(rage crate) with X25519 recipients — has attractive properties (daemon
holds only a public key, standard CLI decryption) but fights the chunk-0
rewrite, brings a genuinely new dep tree, and has no natural KMS-wrap seam;
see §6 D1.

### KEK sources: a `KekProvider` trait

```toml
[recording.encryption]
enabled  = false            # default
kek      = "${file:/etc/siphon-ai/rec-kek}"   # 32 bytes; ${cred:} works too
key_id   = "rec-2026-07"    # stamped into headers; enables rotation
# later (v0.25.0): kms = { provider = "aws", key_arn = "...", region = "..." }
```

- **v0.24.0 ships the file/cred KEK** (via the existing secret resolver).
  Rotation = deploy new KEK with new `key_id`; old recordings name the
  `key_id` they were wrapped with, operator keeps retired KEKs in an archive.
- **The KMS hook is the trait, exercised in v0.25.0**: AWS KMS
  `Encrypt`/`Decrypt` of the DEK is a plain SigV4-signed HTTPS call — and
  v0.25.0 hand-rolls SigV4 for S3 anyway (§3), so the KMS provider reuses
  it. No AWS SDK either way.
- Fail-loud at config load (§4.6): unreadable/wrong-size KEK, bad key_id →
  startup error. At runtime, a wrap failure fails the *recording* (existing
  `recording_failed` path, `siphon_ai_recordings_total{result="failed"}`),
  never the call.

Observability: `recording_encrypted` bool on the CDR (additive),
`siphon_ai_recordings_total` gains no new labels (result already covers
failure), format + ops guidance in `docs/RECORDING.md`.

---

## 3. Sub-item 2 — S3-compatible object storage (→ v0.25.0)

### Client: reqwest + hand-rolled SigV4 (no AWS SDK)

`aws-sdk-s3` would be the largest dependency in the tree by far. We need
exactly: `PUT Object` (+ multipart for >5 GiB safety), SigV4 signing —
`hmac`/`sha2`/`reqwest` are already workspace deps. Hand-rolled SigV4 keeps
us S3-*compatible* (MinIO, Cloudflare R2, Backblaze B2, Wasabi) rather than
AWS-shaped, matches the small-dep-tree rule, and doubles as the KMS client
seam (§2).

```toml
[recording.storage]
endpoint   = "https://s3.us-east-1.amazonaws.com"   # or MinIO/R2/B2 URL
bucket     = "call-recordings"
region     = "us-east-1"
access_key = "${cred:s3-access-key}"
secret_key = "${cred:s3-secret-key}"
key_template = "{date}/{call_id}"     # path templating lands here (sub-item 4b)
delete_local_after_upload = false     # local retention knob
spool_dir  = "/var/spool/siphon-ai/uploads"
```

### Durable upload, mirroring the webhook spool

Finalize → write a small job file to `spool_dir` (atomic `.tmp`+rename,
oldest-first naming — the exact `crates/http` envelope pattern,
`lib.rs:589-602`) → a background worker uploads with capped retry/backoff.
Jobs survive restart; upload failure is never a call failure; local file is
deleted only after a durable upload *and* only when
`delete_local_after_upload = true`.

- **Retention/TTL stays the bucket's job.** S3 lifecycle policies are
  strictly better than the daemon re-implementing them; `docs/RECORDING.md`
  gets a worked lifecycle recipe. The daemon's only deletion is
  delete-local-after-upload.
- Observability: `siphon_ai_recording_uploads_total{result}`,
  `siphon_ai_recording_upload_spool_depth`, upload-duration histogram; CDR
  gains additive `recording_url` (the `s3://bucket/key` pointer — CDR
  carries pointers, never audio, `crates/cdr/src/schema.rs:36-37`); new
  additive WS/webhook event `recording_uploaded` (today's events carry only
  `recording_id`, no location).
- Encrypted uploads are just bytes — encryption (§2) composes for free, and
  is the recommended pairing (ciphertext-only object store).

---

## 4. Sub-item 3 — Consent & announcement (→ v0.26.0)

Two complementary halves; both additive, protocol stays v1:

1. **Daemon-played announcement, gating capture.**
   ```toml
   [recording.announcement]
   file = "/etc/siphon-ai/this-call-is-recorded.wav"
   ```
   When set on a recorded call, the caller hears the file immediately after
   answer, **before any frame reaches the recording writer** — "capture
   starts after the prompt" is the compliance guarantee (config invariant:
   announcement requires recording enabled on the route). Playback reuses
   the `MohSource` file-loop machinery (`crates/media-glue/src/moh.rs`)
   minus the looping; the open design question is sequencing — recommend
   **announce-then-bridge** (WS session connects while the announcement
   plays; caller audio starts flowing to the server after it completes) for
   simplicity and an unambiguous CDR stamp. Needs a small media-glue
   addition (play-once injection on a live call); everything else is
   orchestration in `core`.
2. **A consent stamp on the CDR** (additive object):
   `consent { announced: bool, announcement_ms, server: Option<String> }` —
   `server` set via a new `BridgeIn::SetRecordingConsent { note }` for
   deployments where the WS server captures consent itself (DTMF "press 1",
   ASR verbal yes — its job per CLAUDE.md §4.1; we record the *fact*, not
   the mechanics). On-demand mode + pause-omits-span (`writer.rs:159-164`)
   already give servers full gating control today; this makes what happened
   auditable.

## 5. Sub-items 4 & 5 — Format/templating & outbound (→ v0.25.0 / v0.26.0)

- **Path templating** (`{date}`, `{call_id}`, `{route}`, `{direction}`)
  lands with the storage sink (§3) where it's most needed, and applies to
  the local `dir` too.
- **Opus output** (`format = "opus"` under `[recording]`): libopus is
  already a native dep (forge-engine `opus` feature, v0.8.0); Ogg
  encapsulation is a small, pure-Rust addition (the `ogg` crate — the one
  genuinely new small dep in this theme, §6 D4). Ogg is streaming-native (no
  finalize back-patch at all — kills the §2 chunk-0 rewrite for Opus
  recordings). ~10× smaller than WAV for voice. FLAC: **out** — lossless
  buys little for 16 kHz telephony audio and adds a second encoder path.
- **Outbound recording**: extend the §2 resolution to originated calls (the
  media tap and writer are direction-agnostic; the gap is purely that the
  outbound path never resolves a `RecordingSetup`). Per-gateway default +
  per-originate-request override (`"recording": "on" | "off"` in
  `POST /admin/v1/calls`), mirroring how outbound got SRTP/hold/transfer
  parity in 0.7.x–0.9.x. Toll-fraud-adjacent caution: recording an outbound
  leg is a config/API opt-in, never implied.

---

## 6. Decisions (LOCKED 2026-07-08 — all seven as recommended)

- **D1 — Encryption format — LOCKED: custom chunked GCM.** Custom chunked AES-256-GCM envelope with
  wrapped DEK + `decrypt-recording` subcommand (recommended), vs the `age`
  format. Custom wins on: chunk-0 rewrite (WAV finalize), KMS-wrap seam, no
  new vendor (`aes-gcm`/`zeroize` already in-lock). `age` wins on standard
  tooling + public-key-only daemon. **Locked: custom chunked GCM.**
- **D2 — KEK sources**: file/`${cred:}` KEK in v0.24.0; `KekProvider` trait
  from day one; AWS-KMS provider in v0.25.0 sharing the SigV4 client. No
  AWS SDK. **Locked: yes.**
- **D3 — S3 client**: reqwest + hand-rolled SigV4, S3-compatible targets
  first-class, durable spool mirroring `crates/http`. **Locked: yes.**
- **D4 — Format**: Opus-in-Ogg opt-in output, `ogg` crate as the theme's one
  new small dep; FLAC out of scope. **Locked: yes, landing in v0.25.0.**
- **D5 — Plaintext finalize atomicity**: encrypted output gets
  `.part`+rename in v0.24.0. Also change *plaintext* WAV to `.part`+rename?
  It's a behavior change (consumers watching the dir see names change) but
  fixes the crash ambiguity everywhere. **Locked: yes, with a
  CHANGELOG-flagged behavior note.**
- **D6 — Consent shape**: daemon announcement gating capture +
  additive CDR consent stamp + `SetRecordingConsent` control message
  (protocol stays v1; `PROTOCOL.md` documented same-PR per §4.2).
  **Locked: yes.**
- **D7 — Release slicing**: v0.24.0 encryption → v0.25.0 storage +
  templating + Opus + KMS provider → v0.26.0 consent + outbound.
  **Locked: yes.**

Everything in this theme is **off by default**; recording itself stays
`mode = "off"` unless configured, WAV stays the default format, CDR changes
are additive (no `CDR_VERSION` bump expected — flag if a parser-breaking
change sneaks in), and the WS protocol stays v1.

## 7. What this theme is NOT

- **No transcription, redaction, or PII detection** — AI is the WS server's
  job (CLAUDE.md §4.1). Pause-omits-span is the redaction primitive.
- **No key management UI / no key escrow** — KEK lifecycle is the
  operator's (or KMS's) job; we consume references.
- **No daemon-side retention scheduler** — bucket lifecycle owns TTL; the
  daemon only deletes local files after durable upload, opt-in.
- **No recording playback/serving endpoints** — the daemon writes; it never
  serves audio.
- **No SRTP-key reuse for storage encryption** — call crypto and storage
  crypto are unrelated domains.

## 8. Build order

1. **v0.24.0** — `KekProvider` + envelope writer + `.part` finalize +
   `decrypt-recording` + docs/format spec. Verify: encrypt → decrypt →
   byte-identical WAV; kill -9 mid-call leaves only `.part`; SIPp regression
   green with encryption on.
2. **v0.25.0** — SigV4 client + upload spool/worker + `key_template` +
   `recording_uploaded` event/CDR URL + AWS-KMS KekProvider + Opus/Ogg
   format. Verify against MinIO in compose (new
   `examples/` or test-harness stub) + a real R2/S3 smoke.
3. **v0.26.0** — announcement playback (media-glue play-once) + consent CDR
   stamp + `SetRecordingConsent` + outbound recording resolution. Verify:
   SIPp hears the announcement before echo; outbound SIPp answer-path
   produces a recording; theme retrospective back into this note.
