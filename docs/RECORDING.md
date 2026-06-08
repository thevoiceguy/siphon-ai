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
| Path | `<dir>/<call_id>.wav` |

The `call_id` in the path is the same one on the WS `start` message and the
CDR, so a recording correlates 1:1 with its call.

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
  and `recording_path`. Both are omitted when the call wasn't recorded, so
  the CDR schema stays at version 1.
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

- **Recordings are plaintext at rest — even for encrypted calls.** Recording
  works on SRTP calls (the recorder taps the *decoded* audio — forge already
  decrypts the media to bridge it to your WS server), so the WAV on disk is
  always cleartext PCM regardless of `[media].srtp`. SRTP protects the media
  *in transit*; it does nothing for the recording *at rest*. Treat the
  recording directory as sensitive PII/PCI: protect it with volume
  encryption, filesystem permissions, and access controls. SiphonAI does not
  encrypt recordings.
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

## 7. Limitations (v0.5.0)

- One recording per call (`recording_id == call_id`).
- Path is `<dir>/<call_id>.wav` — no templating yet.
- WAV/PCM16 only; no compressed (Opus) format yet.
- Local-file sink only; no object-storage (S3) sink yet.
- WAV `data`/`RIFF` sizes are 32-bit, so a single recording over ~4 GiB
  (many hours of 16 kHz stereo) saturates the header sizes — not a concern
  for normal call lengths.

The compressed-format and object-storage sinks are tracked as 0.5.x
stretch items (see `docs/DEV_PLAN_0.5.0.md` §4).
