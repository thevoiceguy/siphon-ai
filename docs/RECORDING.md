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
- Path is `<dir>/<call_id>.wav[a]` — no templating yet (planned for the
  object-storage release, `docs/design/DESIGN_RECORDING_COMPLIANCE.md` §3).
- WAV/PCM16 only; no compressed (Opus) format yet (planned, same design
  note §5).
- Local-file sink only; no object-storage (S3) sink yet (planned, §3).
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
