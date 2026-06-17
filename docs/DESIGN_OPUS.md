# Design note — Opus codec support

> **Status: DRAFT — decisions in §7 (gating ones LOCKED 2026-06-17).** Same
> design-first pass we did for park / hold / reconnect, because Opus
> challenges the **locked WS audio contract** (CLAUDE.md §4.2, D8: PCM16
> mono, 8 k/16 k only, exact 20 ms frames) and needs an **upstream
> forge-media change** plus a **new native dependency**. The build follows
> this once §7 is fully locked; deviations get noted back here.

Adds **Opus** to the negotiable codec set. Opus is the modern wideband
codec WebRTC/softphones prefer; SiphonAI advertises only G.711/G.722
today and **rejects Opus at config load** (`compile.rs::parse_codecs`).
The blocker the v1 plan named (`docs/DEV_PLAN.md` §15.1: "Opus is post-v1
— its 48 kHz rate doesn't fit the PCM16 / 8k|16k contract, and resampling
lives in forge-media, not here. Reopen when forge-resampler ships") is now
actionable: `forge-resampler` + `forge-codecs/opus` exist at our pinned
forge rev, and forge-engine has an `opus` feature.

---

## 1. The core problem: 48 kHz Opus vs. the 8/16 kHz WS contract

Opus operates at 48 kHz. The WS bridge contract is **fixed** at PCM16
mono, 8 kHz or 16 kHz, exactly 20 ms/frame (160 or 320 samples;
PROTOCOL.md §2.2, CLAUDE.md §4.2). 48 kHz (960 samples/frame) is not a
legal WS frame. So an Opus call MUST be **resampled to 16 kHz** for the
WS path (decision §7.2), and surface as `start.audio.sample_rate: 16000`
— a value the contract already allows (G.722 calls already run at 16 k).
The wire stays Opus (`a=rtpmap:111 opus/48000/2`); only the WS-facing PCM
is 16 k. This keeps the protocol at `version: "1"` — no new WS shape,
just Opus added to the set of codecs that can land a 16 k session.

```
caller RTP (Opus 48k) ─► forge decode (48k) ─► resample 48k→16k ─► tap ─► WS (16k PCM16)
WS (16k PCM16) ─► tap ─► resample 16k→48k ─► forge Opus encode (48k) ─► caller RTP
```

---

## 2. The upstream gap (forge-media) — the gating dependency

forge **has** the parts (decoder, `resample_audio`, a conditional
resample on the playout path) but **mislabels Opus's bridge rate**:

- `MediaSession::codec_audio_sample_rate(codec, clock)` special-cases
  **G.722 → 16000** and otherwise returns the negotiated clock rate. For
  Opus that's **48000**. This value is the rate forge tags decoded frames
  with and the rate it expects on playout — so today the tap would receive
  **48 kHz** Opus frames and forge would expect 48 kHz from the WS. Both
  violate the WS contract.
- The `OpusCodec` is built at `sample_rate: 48000` (`session.rs`), correct
  for the codec; the issue is purely the **bridge-facing rate**.

**Needed forge change (upstream PR):** give Opus a **bridge audio rate of
16 kHz** — exactly the G.722 precedent — so the engine:
- on **decode**: Opus → 48 kHz PCM → **resample 48k→16k** → deliver 16 kHz
  frames to the tap;
- on **playout**: accept 16 kHz from the WS → **resample 16k→48k** →
  Opus-encode.

Mechanically this is "make `codec_audio_sample_rate(Opus) = 16000` and
ensure the decode path resamples 48→16 the way the playout path already
resamples 16→48" — small, mirrors G.722. **This is chunk 1 (a forge-media
PR), not siphon-ai work** (DEV_PLAN §15.1: resampling lives in forge).
Confirm the exact shape with a spike against forge-engine before the PR.

**Resampler quality:** forge's `resample_audio` is linear interpolation.
Adequate for 48↔16 voice as a first cut; if artefacts show, swap to the
`forge-resampler` crate (band-limited) in the same forge PR. Note in §7.

---

## 3. New dependency — libopus (LOCKED: accept)

Enabling forge-engine's `opus` feature pulls `forge-codecs/opus` →
`audiopus` (`audiopus_sys` vendors/links **libopus**, a C library). This
is SiphonAI's **first native build dependency** — a departure from the
deliberately lean, mostly-pure-Rust tree (CLAUDE.md §4.1). **Locked
(§7.1): accept it** — there's no production-ready pure-Rust Opus, and Opus
is the point. Document it: CI and release builds need a C toolchain
(`cc`/`cmake`); `docs/DEPLOY.md` gains a build-prereqs note. The
`forge-engine` dep in siphon-ai's `Cargo.toml` adds `"opus"` to its
feature list (currently `["g722", "dtls"]`), gated so non-Opus
deployments still pay nothing at runtime (the codec is only built when a
route lists it / a peer offers it).

---

## 4. SDP negotiation

Our `Codec::Opus` already exists (`sdp.rs`): PT **111** (dynamic;
`a=rtpmap:111 opus/48000/2`), `clock_rate 48000`, `rtpmap_channels "2"`.
Changes:

- **`Codec::Opus.audio_sample_rate()` → 16000** (today returns 48000) so
  `AnswerOutcome.negotiated_audio_sample_rate` → `start.audio.sample_rate
  = 16000` and the tap/bridge expect 16 k frames. This is the siphon-ai
  half of the §2 rate fix; it must match what the forge PR delivers.
- **fmtp** (`a=fmtp:111 …`): advertise a sane line —
  `maxplaybackrate=16000; sprop-maxcapturerate=16000` (we only consume/
  produce 16 k after resample, so signal it), `stereo=0; sprop-stereo=0`
  (we are mono — see §5), `useinbandfec=1` (cheap loss resilience),
  `usedtx=0` (DTX off — our 20 ms cadence + comfort logic doesn't want
  variable framing in v1). Exact params §7.3.
- **Dynamic PT:** on an inbound offer we answer with the **offerer's**
  Opus PT (not hard-111); on outbound we offer 111. (Mirror existing PT
  handling.)
- **Codec priority:** Opus is opt-in via `[media].codecs` order; the
  default list stays `["pcmu","pcma"]` (no behaviour change unless an
  operator adds `"opus"`).

---

## 5. Stereo → mono

Opus rtpmap carries `/2` (the encoding-params convention) and a peer may
send stereo. The WS contract is **mono**. forge's bridge frames are mono
(`Vec<i16>` single channel), so the decode path must **downmix** stereo
Opus → mono before/with the 48→16 resample. Confirm forge's Opus decode
yields mono (it likely decodes to the configured channel count); if it
yields stereo, the downmix lands in the same forge PR (§2). We advertise
`stereo=0` so compliant peers send mono anyway; the downmix is the
defensive path. §7.4.

---

## 6. What does NOT change

- WS protocol `version` stays `"1"` — Opus calls are just 16 k sessions;
  no new message or field. A server that already handles 16 k (G.722)
  handles Opus transparently.
- CDR schema unchanged (the `audio.codec` field already carries the
  negotiated codec name; `"opus"` is just a new value).
- No change to hold / park / conference / reconnect / recording — they
  operate on the post-decode 16 k PCM, codec-agnostic.
- DTMF: RFC 2833 `telephone-event` still negotiated alongside Opus as
  today (Opus has no in-band DTMF).

---

## 7. Decisions

**LOCKED (2026-06-17):**
1. **libopus native dependency — accept.** Enable forge-engine `opus`;
   document the C-toolchain build prereq. §3.
2. **WS rate — 16 kHz.** Opus 48 k → resample → 16 k PCM on the WS
   (`start.audio.sample_rate = 16000`); wire stays `opus/48000/2`. §1.
3. **Design-first.** This note → forge-media spike+PR → siphon-ai
   enablement → SDP/tests/release.

**To confirm (during the spike / chunk 1):**
4. **Resampler:** forge's built-in linear `resample_audio` first cut, vs.
   the band-limited `forge-resampler` crate. Recommend **start with the
   built-in**; upgrade if voice quality is poor. §2.
5. **fmtp params:** `useinbandfec=1`, `usedtx=0`, `maxplaybackrate=16000`,
   `stereo=0` — confirm against a real softphone (Linphone) + a carrier.
   §4.
6. **Stereo downmix location:** confirm forge's Opus decode is mono with
   `stereo=0`; if not, add downmix in the forge PR. §5.
7. **Version:** likely **0.8.0** — Opus is a notable capability and the
   first native dep (arguably a minor-version signal), though additive.
   Confirm at release time.

---

## 8. Implementation chunks

Mirrors the park / hold / reconnect cadence — plan PR, then chunks, then
harness + release.

- **Plan** (this note).
- **Chunk 1 — forge-media PR (the gating upstream work).** Spike
  forge-engine's Opus path; make the Opus **bridge rate 16 kHz** with
  48↔16 resampling (mirror G.722) and mono downmix; tests in forge.
  Bump siphon-ai's forge pin to the merged rev. *No siphon-ai behaviour
  yet.*
- **Chunk 2 — siphon-ai enablement.** Add `"opus"` to the `forge-engine`
  feature list; stop rejecting Opus in `compile.rs::parse_codecs`; set
  `Codec::Opus.audio_sample_rate() = 16000`; SDP fmtp (§4) + dynamic-PT
  answer + stereo=0; unit tests (negotiation, rate mapping). Opus opt-in
  via `[media].codecs`.
- **Chunk 3 — harness + docs + release.** A SIPp Opus scenario (offer
  `opus/48000/2`, assert a 16 k bridge / a completed call) — needs a SIPp
  build with Opus media or a media-less signalling assert; `docs/CONFIG.md`
  (Opus in `[media].codecs`), `docs/DEPLOY.md` (libopus build prereq),
  PROTOCOL note (Opus → 16 k sessions); CHANGELOG; version bump; tag.
