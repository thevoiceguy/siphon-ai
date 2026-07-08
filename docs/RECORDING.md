# Call Recording

SiphonAI can record each call's audio to a stereo WAV for compliance and QA.
Recording is **off by default** and added in v0.5.0.

- **What you get:** one `.wav` per recorded call — dual-channel stereo
  PCM16, **caller on the left channel, bot/WS on the right** (per-speaker
  separation, ideal for QA and per-channel transcription).
- **What it is not:** SiphonAI does not analyze the audio. Transcription,
  sentiment, and QA scoring are the WS server's job — the recording is the
  perfect *input* to such a tool, but that tool is yours, not the bridge's
  (CLAUDE.md §4.1).

---

## 1. Enabling

Recording is configured under `[recording]` (see `docs/CONFIG.md`):

```toml
[recording]
mode = "always"                       # "off" (default) | "always" | "on_demand"
dir  = "/var/lib/siphon-ai/recordings"
```

- **`mode = "off"`** (default) — no recording; zero behaviour change.
- **`mode = "always"`** — record every accepted call, for its full duration.
- **`mode = "on_demand"`** — wire recording for the call but stay idle; the
  WS server starts/stops it (see §3).
- **`dir`** — output directory; required when `mode != "off"`. Created at
  startup, so a bad path fails loud at config load. Files are written as
  `<dir>/<call_id>.wav`.

### Per-route override

A `[route.recording]` block overrides the global `mode` for matched calls
(strict override — see `docs/DIALPLAN.md`). The output `dir` is always the
global one, so `[recording].dir` must be set whenever any route enables
recording, even if the global `mode = "off"`:

```toml
[recording]
mode = "off"                          # don't record by default…
dir  = "/var/lib/siphon-ai/recordings"

[[route]]
name = "support"
[route.match]
to = "5000"
[route.recording]
mode = "always"                       # …but always record the support line
```

---

## 2. Output

| Property | Value |
|---|---|
| Container | WAV (RIFF), uncompressed |
| Sample format | PCM16, little-endian |
| Channels | 2 (stereo) — **L = caller, R = bot/WS** |
| Sample rate | The call's negotiated rate (8 kHz or 16 kHz) |
| Path | `<dir>/<call_id>.wav` — `<dir>/<call_id>.wava` with encryption on (§8) |

The `call_id` in the path is the same one on the WS `start` message and the
CDR, so a recording correlates 1:1 with its call.

**In-progress files are `<name>.part`** (0.24.0): the writer streams into
the `.part` and renames it onto the final path only when finalize succeeds.
A bare `.wav`/`.wava` on disk is therefore always a *complete* recording —
safe for a watcher/uploader to pick up — and a crash leaves only a `.part`.
(Before 0.24.0 the file appeared at its final name immediately, with
placeholder header sizes until the call ended.)

---

## 3. On-demand control (WS protocol)

With `mode = "on_demand"`, the WS server drives recording with these control
messages (full spec in `docs/PROTOCOL.md` §4.7):

| Server → SiphonAI | Effect |
|---|---|
| `start_recording` | Begin recording. SiphonAI replies `recording_started` (or `recording_failed`). |
| `stop_recording` | Finalize and close the file now. SiphonAI replies `recording_stopped`. |
| `pause_recording` | Suspend recording — the paused span is **omitted** from the file (dropped, not silenced). |
| `resume_recording` | Resume after a pause. |

SiphonAI emits these back (PROTOCOL.md §3.11):

| SiphonAI → Server | Meaning |
|---|---|
| `recording_started` | Recording began; carries `recording_id`. |
| `recording_stopped` | Recording finalized (on `stop_recording` or call end). |
| `recording_failed` | Recording could not start or write (best-effort — the call is unaffected). |

**Pause is the PCI primitive:** to keep card numbers out of a recording,
`pause_recording` before the caller reads the number and `resume_recording`
after. The paused audio is never written — the recording skips straight from
before to after.

`mode = "always"` covers the whole call automatically; these controls aren't
needed there (a `start_recording` on an already-recording call is a no-op).

> One recording per call in this release; `recording_id` equals `call_id`.

---

## 4. Observability

- **CDR** (`docs/DEPLOY.md`): a recorded call's CDR carries `recording_id`
  and `recording_path`, plus `recording_encrypted: true` when the file is a
  sealed `.wava` (§8). All are omitted when the call wasn't recorded —
  additive fields, no CDR version bump.
- **Metric:** `siphon_ai_recordings_total{result="ok"|"degraded"|"failed"}`
  ticks once per recorded call.
  - `ok` — written cleanly.
  - `degraded` — some 20 ms frames were dropped under writer back-pressure;
    the file is **short, not corrupt** (see §5). `recording_path` is still
    on the CDR.
  - `failed` — an I/O error (e.g. disk full); the file is incomplete or
    absent.

---

## 5. How it stays off the hot path

The audio path is sacred (CLAUDE.md §4.3): per-call audio runs at 50
frames/sec and must never block. So recording never touches the audio task —
the media tap only does a **non-blocking copy** of each frame onto a bounded
channel, and a dedicated per-call writer task does the file I/O. If the
writer can't keep up and that channel fills, frames are **dropped** (and the
recording is flagged `degraded`) rather than stalling or gapping the live
call. A recording is always best-effort; it never degrades call quality.

---

## 6. Operating

- **At-rest protection.** SRTP/WSS protect media *in transit*; the recorder
  taps the *decoded* audio, so by default the WAV on disk is cleartext PCM
  regardless of `[media].srtp`. For PCI/HIPAA-grade deployments turn on
  **`[recording.encryption]`** (§8) so nothing plaintext ever touches disk.
  Either way, treat the recording directory as sensitive PII/PCI:
  filesystem permissions and access controls still apply (encryption
  protects stolen disks and stale backups, not a compromised live box).
- **Disk sizing:** recordings are uncompressed — ≈115 MB/hour at 16 kHz,
  ≈58 MB/hour at 8 kHz (stereo PCM16). With `mode = "always"`, size the disk
  for your peak concurrent-call-hours.
- **Retention:** the daemon **does not delete recordings**. Manage retention
  yourself (lifecycle policy on the storage, or a cron job) per your
  compliance window.
- **Consent / announcement:** recording has jurisdiction-specific consent
  law (e.g. two-party-consent regions). Playing any "this call is recorded"
  announcement and obtaining consent are the **operator's responsibility** —
  SiphonAI does not insert prompts. (Your WS server can play the prompt.)

---

## 7. Limitations

- One recording per call (`recording_id == call_id`).
- Local path is `<dir>/<call_id>.wav[a]` — templating applies to the
  object-storage key (`[recording.storage].key_template`, §9), not the
  local dir.
- WAV/PCM16 only; no compressed (Opus) format yet (planned, same design
  note §5).
- Object storage: upload-only (§9) — the daemon never serves or fetches
  recordings back.
- Inbound calls only; outbound-leg recording is planned (§5).
- WAV `data`/`RIFF` sizes are 32-bit, so a single recording over ~4 GiB
  (many hours of 16 kHz stereo) saturates the header sizes — not a concern
  for normal call lengths.

---

## 8. Encryption at rest (`[recording.encryption]`, 0.24.0)

```toml
[recording.encryption]
enabled = true
kek     = "${file:/etc/siphon-ai/recording-kek.hex}"   # 64 hex chars
key_id  = "rec-2026-07"
```

With encryption on, recordings are written as **`.wava` envelopes** —
nothing plaintext ever touches disk. The model is standard **envelope
encryption**:

- Every recording gets a **fresh random 256-bit data key (DEK)** that
  encrypts the audio in independent 64 KiB AES-256-GCM chunks.
- The DEK is stored in the file's header, **wrapped by your KEK** (the
  32-byte key `kek` references). The KEK itself never appears in any
  recording.
- The header names your `key_id`, so **rotation is cheap**: deploy a new
  KEK + `key_id` and new recordings use it; old recordings still name the
  old id — keep retired KEKs in a secure archive to read them. No
  re-encryption of existing audio, ever.

Generate a KEK with `openssl rand -hex 32`, deliver it via `${file:}` (mode
`0400`) or systemd `LoadCredential` + `${cred:}`. Validation is at config
load — a missing/malformed KEK or `key_id` fails startup, never a call. If
wrapping fails at runtime the *recording* fails (`recording_failed`,
`siphon_ai_recordings_total{result="failed"}`); the call continues.

**Or let AWS KMS hold the KEK** (0.25.0) — the key never exists outside
KMS, and unwrapping is IAM-auditable:

```toml
[recording.encryption]
enabled = true
key_id  = "rec-kms-2026"
kms = { key_arn = "arn:aws:kms:us-east-1:…:key/…", region = "us-east-1",
        access_key = "${cred:kms-access-key}", secret_key = "${cred:kms-secret-key}" }
```

Exactly one of `kek` / `kms`. Each recording start makes one KMS `Encrypt`
call on the writer task (never the audio path; 10 s timeout, failure fails
the recording only). Decrypt with
`siphon-ai decrypt-recording <file> --kms-region us-east-1` and
`AWS_ACCESS_KEY_ID`/`AWS_SECRET_ACCESS_KEY` in the environment — the
ciphertext blob names its own KMS key. `endpoint` (or `--kms-endpoint`)
targets KMS-compatible emulators like LocalStack.

### Decrypting

```sh
siphon-ai decrypt-recording /var/lib/siphon-ai/recordings/<call_id>.wava \
    --kek-file /etc/siphon-ai/recording-kek.hex
# → <call_id>.wav next to the input (or --out PATH)
```

The subcommand is offline tooling — it needs only the KEK file, not the
daemon config. A wrong key fails loud and prints the `key_id` the recording
was wrapped with, so you know which archived KEK to fetch. A crashed
capture (`.wava.part`) can be recovered with `--allow-unfinalized`; its WAV
header sizes are placeholders (re-mux with `ffmpeg -i out.wav fixed.wav` if
a tool refuses it).

### Container format (`SAIWAVA1`)

For third-party decrypt implementations. All integers little-endian:

```
header:  magic "SAIWAVA1"
         key_id_len u8 | key_id (utf-8)
         wrapped_dek_len u16 | wrapped_dek
         chunk_size u32                       # plaintext bytes per chunk
chunk i: generation u32 | ct_len u32 | ciphertext (plaintext + 16-byte tag)
```

- `wrapped_dek` = `nonce (12) || AES-256-GCM(KEK, DEK)` with the `key_id`
  bytes as AAD.
- Chunk `i`'s cipher is AES-256-GCM under the DEK with nonce
  `chunk_index u64 || generation u32` and the full serialized header as
  AAD (a chunk can't be replayed into another recording).
- The decrypted payload is a byte-exact standard WAV.
- **Generation rule:** finalize rewrites chunk 0 with the patched WAV
  header sizes under `generation = 1` (a fresh nonce — the plaintext
  length is unchanged so it overwrites in place). A valid finalized file
  has generation 1 on chunk 0 and 0 on every other chunk; anything else
  (including generation 0 on chunk 0 — an unfinalized capture) must be
  rejected by default.

---

## 9. Object storage (`[recording.storage]`, 0.25.0)

```toml
[recording.storage]
enabled      = true
endpoint     = "https://s3.us-east-1.amazonaws.com"   # MinIO/R2/B2 work too
bucket       = "call-recordings"
region       = "us-east-1"
access_key   = "${cred:s3-access-key}"
secret_key   = "${cred:s3-secret-key}"
key_template = "{date}/{call_id}"
delete_local_after_upload = false
spool_dir    = "/var/spool/siphon-ai/uploads"
```

When a recording finalizes, the daemon writes a small **job file** to
`spool_dir` and a background worker uploads the recording with retries
(path-style `PUT`, SigV4 — S3-compatible stores are first-class). The
design goals:

- **Durable**: jobs survive restarts; an unreachable endpoint backs up in
  the spool (`siphon_ai_recording_upload_spool_depth`) instead of losing
  uploads. A job that keeps failing is dropped after a large retry budget
  (`siphon_ai_recording_uploads_total{result="dropped"}`) — the recording
  stays on local disk.
- **Off every call path** (CLAUDE.md §4.7): enqueue is one file write at
  teardown; uploads happen on a background worker.
- **Deterministic destination**: the CDR's `recording_url`
  (`s3://bucket/key`) is stamped at enqueue; the **`recording_uploaded`**
  lifecycle webhook (after `call_end`) confirms arrival with `url` and
  `size_bytes`.
- **Local retention**: `delete_local_after_upload = true` removes the
  local file only after a durable upload. TTL/retention in the bucket is
  the bucket lifecycle policy's job — a worked AWS example:

  ```json
  { "Rules": [ { "ID": "expire-recordings", "Status": "Enabled",
      "Filter": { "Prefix": "" },
      "Expiration": { "Days": 365 } } ] }
  ```

  (`aws s3api put-bucket-lifecycle-configuration --bucket call-recordings
  --lifecycle-configuration file://policy.json`, or the MinIO/R2
  equivalent.)
- **Pair with encryption** (§8): with `[recording.encryption]` on, the
  bucket only ever holds sealed `.wava` envelopes — a leaked bucket leaks
  ciphertext.

Uploads are `PUT`-only and capped at S3's 5 GiB single-request limit —
far above any real recording (WAV sizes saturate at 4 GiB).
