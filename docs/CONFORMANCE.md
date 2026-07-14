# Protocol conformance testkit

`siphon-ai-testkit` plays the **daemon's side** of the WS bridge protocol
(v1) against your WebSocket server — no SIP, no RTP, no SiphonAI daemon
needed. It drives scripted calls from TOML scenarios and validates
everything your server does:

- every JSON message you send is validated against
  [`schemas/siphon-ai.v1.json`](../schemas/siphon-ai.v1.json) **and**
  parsed with the daemon's own wire types;
- every binary frame must be exactly one 20 ms PCM16-LE chunk (320 B @
  8 kHz, 640 B @ 16 kHz) and must not arrive faster than real time;
- close semantics per [`PROTOCOL.md`](PROTOCOL.md) §5.7 — a bare close
  mid-call is a violation; `hangup` is honored daemon-style
  (`stop { server_hangup }` + clean close);
- unknown-event tolerance — scenarios inject event types your server has
  never seen; dying on them is a violation;
- WS keepalive (§5.6) — your server (or whatever proxy fronts it) must
  answer pings, or production calls get torn down as half-open.

Exit code is `0` iff every scenario passed, so **"conformant with
protocol v1" is a claim your CI can gate on**. SiphonAI's own CI runs the
full set against both bundled SDK echo servers on every PR.

## Quick start

```sh
cargo build -p siphon-ai-protocol-testkit   # or grab a release binary

# your server listening on ws://localhost:8080/ …
siphon-ai-testkit run ws://127.0.0.1:8080/

siphon-ai-testkit run --scenario basic-echo ws://127.0.0.1:8080/
siphon-ai-testkit run --report report.json ws://127.0.0.1:8080/
siphon-ai-testkit list
```

Typical output:

```text
─── basic-echo ── OK (1465 ms, 74 audio frames, 0 commands)
─── dtmf ── OK (965 ms, 49 audio frames, 0 commands)
...
PASS: 5 passed, 0 failed — target ws://127.0.0.1:8080/ is conformant with protocol v1
```

## Bundled scenarios

| Scenario | What it proves |
|---|---|
| `basic-echo` | Audio in/out correctly framed; survives an unknown event type |
| `dtmf` | DTMF events mid-stream don't disturb the session |
| `recording-controls` | Recording lifecycle events tolerated; audio path unaffected |
| `hangup-semantics` | Abrupt drop + `start { reconnected: true }` accepted; `hangup` honored if sent |
| `keepalive` | Pings answered; session survives idle gaps |
| `barge-in-pause` | Pause-mode arbitration (0.32.0): a `speech_started` with `decision_pending: true` draws a `barge_in_confirm`/`barge_in_reject` verdict within the deadline (the reference echo servers reject); `barge_in_resolved` outcomes — including an uncaused `timeout` — are tolerated |

The bundled scenarios assume an **echo-shaped** server (it sends audio
only in response to caller audio) — that's what makes pacing and silence
assertions deterministic. A voicebot that speaks first will want its own
scenario files (below).

## Scenario files

Bundled scenarios live in `crates/protocol-testkit/scenarios/` and are
embedded in the binary. `--scenario-dir <dir>` loads additional `*.toml`
files; a file whose `name` matches a bundled scenario replaces it.

```toml
name = "my-scenario"          # must match the file name (bundled) — free otherwise
description = "One line for `list` output."

[session]                     # all optional
sample_rate = 8000            # 8000 | 16000 — sets the asserted frame size
from = "+13125551212"         # start.from
to = "5000"                   # start.to
pacing_slack_frames = 25      # extra frames beyond real time before a pacing violation

[[steps]]
action = "send_audio"         # stream N × 20 ms tone frames, paced
frames = 50
```

### Steps

| `action` | Fields | Meaning |
|---|---|---|
| `send_audio` | `frames` | Stream N × 20 ms paced tone frames (the daemon's RTP→WS path) |
| `expect_audio` | `min_frames`, `within_ms` | The session's **cumulative** received-frame total must reach `min_frames` (echo arrives concurrently with `send_audio`, so totals — not per-step counts — are race-free; the total resets on `reconnect`) |
| `send_event` | `json` | Inject a daemon event. `call_id`/`seq` are added by the runner; the result must round-trip through the daemon's `BridgeOut` type, so a typo'd scenario fails loudly |
| `send_raw` | `json` | Send a text frame verbatim (plus `call_id`/`seq` if absent) — the unknown-tolerance probe |
| `expect_command` | `type`, `within_ms`, `optional` | Server must send this command in time (`optional = true`: absence is fine, presence is still validated) |
| `expect_silence` | `ms` | No audio or commands for the window (pongs excluded) |
| `ping` | `within_ms` | WS ping; a pong must come back in time |
| `wait` | `ms` | Idle, still receiving + validating whatever arrives |
| `send_stop` | `reason` | The daemon's last message on a session (`caller_hangup`, `server_hangup`, `transfer`, `ws_disconnect`, `park`, `error`) |
| `close` | — | Clean close (1000), waiting for the server's close reply |
| `reconnect` | — | Abrupt drop, fresh socket, `start { reconnected: true }` with `seq` restarting at 0 — a daemon-side WS reconnect (§5.7) |

Two runner behaviors to know:

- **Server `hangup` ends the scenario early, successfully.** The testkit
  reacts like the daemon (`stop { server_hangup }` + clean close), skips
  the remaining steps, and records a note. Reacting to a call by hanging
  up is legal; scenarios therefore can't *require* a server not to.
- **Every scenario has a 60 s hard deadline** so a wedged server fails
  fast instead of hanging CI.

## The JSON report

`--report out.json` writes a machine-readable summary alongside the
stdout text:

```json
{
  "target": "ws://127.0.0.1:8080/",
  "protocol_version": "1",
  "testkit_version": "0.29.0",
  "scenarios": [
    {
      "name": "basic-echo",
      "passed": true,
      "duration_ms": 1465,
      "failures": [],
      "notes": [],
      "audio_frames_received": 74,
      "commands_received": 0
    }
  ],
  "passed": 5,
  "failed": 0,
  "conformant": true
}
```

`conformant` mirrors the exit code. Failure strings are self-contained
("audio frame of 160 bytes — every binary frame must be exactly one 20 ms
chunk (320 bytes at 8000 Hz)"), so piping the report into a PR comment is
enough for a developer to act on.

## Gating your server's CI

```yaml
- run: cargo install --git https://github.com/thevoiceguy/siphon-ai siphon-ai-protocol-testkit
# …start your server on :8080…
- run: siphon-ai-testkit run ws://127.0.0.1:8080/ --report conformance.json
```

The testkit needs no daemon, no SIP stack, and no network beyond the
one WebSocket — it's a single static-linkable binary.

## Relationship to the SDKs

The server SDKs ([`sdks/`](../sdks/)) and this testkit approach the same
contract from opposite ends: the SDKs make it hard to *write* a
non-conformant server, the testkit proves any server — SDK-based or
hand-rolled in any language — actually *is* conformant. SiphonAI's CI
runs the testkit against both SDK echo servers on every PR, which is also
what keeps the testkit itself honest.
