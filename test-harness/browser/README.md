# Browser interop harness

Real browsers against a real daemon — `docs/design/DEV_PLAN_WebRTC.md`
§5, item 2. Runs **nightly** in CI (`.github/workflows/browser-interop.yml`),
and on demand here.

The per-commit media coverage is the in-process loopback
(`crates/webrtc-glue/tests/loopback.rs`, `crates/core/tests/webrtc_call.rs`):
no browser, two seconds, every push. This harness answers the question
that only a real browser can — whether *its* WebRTC stack and *its* SIP
stack agree with ours — which changes when browsers ship, not when we
commit.

## What it asserts

Per engine: REGISTER over WSS → a call the daemon answers with a
forge-webrtc leg → **audio in both directions** → BYE → the RTP port
pair comes back.

The two directions are proved independently, and neither can be
satisfied by an echo:

```
page --tone--> WSS --> siphon-ai --> tone-ws-server   (server measures what arrived)
page <--900Hz- WSS <-- siphon-ai <-- tone-ws-server   (page reports inbound energy)
```

`tone-ws-server.mjs` is a `siphon-ai.v1` WS server that plays a 900 Hz
tone and *measures* the dominant frequency of what it receives (a
Goertzel sweep over the 50 Hz grid a 20 ms window resolves exactly). It
measures rather than assumes because the browser decides what it sends
— see below.

## The engine differences that shaped this

- **Headless Firefox has no audio output device**, so its
  `AudioContext` never leaves `suspended` and `resume()` *never
  settles* — not with a user gesture, not with any `media.autoplay.*`
  pref. Awaiting it hangs the call before the INVITE is built. The page
  therefore waits on it with a budget and falls back to
  `getUserMedia()` (Firefox's fake capture device, a clean 1 kHz tone).
  That is why the WS server measures the frequency instead of
  asserting 450 Hz.
- **Inbound audio is asserted from `getStats()`** (`totalAudioEnergy` on
  the inbound RTP track), which every engine reports and which needs no
  AudioContext. Where WebAudio *does* run (Chromium), the page
  additionally proves the audio is the server's 900 Hz tone rather than
  an echo of its own.
- **WebKit needs a dozen system libraries** a developer box will not
  have (`libenchant`, `libsecret`, `libwebp`…), which is why the
  workflow installs browsers with `--with-deps` and why local runs
  usually mean `BROWSERS=chromium,firefox`.

## Running it

```sh
cargo build -p siphon-ai --features webrtc     # the daemon under test
examples/browser-sip/gen-cert.sh               # once: the lab's WSS cert
cd test-harness/browser
npm install
npx playwright install chromium firefox        # add webkit with --with-deps
npx playwright test                            # all engines
BROWSERS=chromium npx playwright test          # just one
```

Everything else is automatic: `global-setup.mjs` boots the daemon
(`examples/browser-sip/lab.toml`, ports rewritten to free ones), the
tone WS server, and a static server for the page — the same lab
`examples/browser-sip/headless-check.sh` uses, so the config and the
page cannot drift between them.

Set `DAEMON_BIN` to test a binary other than `target/debug/siphon-ai`.
Logs, the daemon's CDR file and the WS server's measurements are left
in the run's work directory, which the setup prints.

## The page

`examples/browser-sip/index.html`, driven with query parameters:

| Parameter | Effect |
|---|---|
| `?auto=1` | register on load, announce the outcome on the console |
| `?call=1` | with `auto`, place the test call too |
| `?tone=1` | send an identifiable tone instead of a microphone, and report what comes back on `window.__labAudio` |
| `?sipjs=U` | load the SIP stack from `U` — the harness serves a vendored copy so a nightly run never depends on a CDN |

## Still to build

The plan's matrix is browsers **×** SIP stacks; this covers Chromium,
Firefox and WebKit against **SIP.js**. A JsSIP lane needs a second page
(the stacks' APIs differ); the `?sipjs=` hook and the measure-don't-
assume server are already shaped for it.

Real Safari — as opposed to WebKit, which is as close as Linux CI
gets — needs a macOS runner. The plan flags Safari as where the
surprises live, so treat a green WebKit as necessary, not sufficient.
