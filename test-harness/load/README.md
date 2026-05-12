# Load and soak harness

Two SIPp scenarios that validate the Week-5 stability acceptance bar
(`docs/DEV_PLAN.md` §9 Week 5):

| Scenario                  | Goal                                     | Pass criteria |
|---------------------------|------------------------------------------|---------------|
| `long_call_1h.xml`        | One call, sustained for 1 hour with audio | No memory growth beyond ±10 MB RSS, no clock drift, audio still flowing at minute 59. |
| `concurrent_burst_500.xml`| 500 concurrent call setups at 50 cps     | All 500 reach `200 OK`, sustained for 10 minutes, no leaked forge sessions after teardown. |

Both write SIPp's per-scenario logs alongside the XML; both expect the
daemon under test to be running on `127.0.0.1:5060` with a route that
accepts any inbound INVITE (the shipped `configs/local-dev.toml` works).

## Prerequisites

```sh
# Tooling
sudo apt install sip-tester           # or `brew install sipp` on macOS

# Daemon
cargo build -p siphon-ai --release

# An echo WS server on :8765
cd examples/echo-ws-server-python && pip install -r requirements.txt
python server.py --bind 127.0.0.1:8765 &
```

A PCMA-encoded test pcap is required for the long-call scenario — SIPp
needs realistic media to push through forge so memory growth and clock
drift get exercised. We don't ship binary audio in the repo (CLAUDE.md
§5 / §6.4); generate one yourself or grab one from a SIPp sample suite:

```sh
# Generate 1 minute of silence as g.711 a-law and capture it as a pcap.
# SIPp loops the pcap until the call ends, so 1 minute is plenty.
ffmpeg -f lavfi -i "anullsrc=r=8000:cl=mono" -t 60 -ar 8000 -ac 1 \
    -acodec pcm_alaw silence.alaw
# Wrap it in a pcap-with-RTP — many examples online; pcap_audio_create.py
# in the SIPp source tree is one option.
mv silence.pcap test-harness/load/audio_pcma.pcap
```

If you only care about signaling stability, run with the
`SKIP_AUDIO=1` env var (see below) — SIPp will pause without streaming
RTP and the daemon's inactivity watchdog has to be disabled
(`[media].inactivity_timeout_secs = 0`) to keep the call alive for the
full hour.

## Running

### 1-hour single call

```sh
# Start the daemon (terminal A)
SIPHON_AI_CONFIG=configs/local-dev.toml \
    cargo run -p siphon-ai --release -- --config $SIPHON_AI_CONFIG

# Drive the soak (terminal B)
sipp -sf test-harness/load/long_call_1h.xml \
    -m 1 \
    -p 5080 \
    -s 1000 \
    127.0.0.1:5060
```

While it runs, in terminal C watch:

```sh
# Memory growth — should be near-flat after the first 60 s warm-up.
watch -n 30 'ps -o rss= -p $(pgrep -f "target/release/siphon-ai")'

# Active call count — should hold steady at 1.
watch -n 30 'curl -s http://127.0.0.1:9091/metrics | grep siphon_ai_calls_active'

# QoS — packet loss fraction should stay near 0 on the loopback.
watch -n 30 'curl -s http://127.0.0.1:9091/metrics | grep forge_rtcp'
```

### 500-concurrent burst

```sh
sipp -sf test-harness/load/concurrent_burst_500.xml \
    -m 500 \
    -r 50 \
    -p 5080 \
    -s 1000 \
    127.0.0.1:5060
```

`-r 50` is the call rate per second; `-m 500` is the total. The
scenario itself pauses 10 minutes per call before BYE, so the burst
reaches steady-state around T+10s and sustains for the remainder.

Watch the daemon's `/metrics`:

- `siphon_ai_calls_active` should plateau at 500 ± 5 (SIPp's pacing
  isn't perfect).
- `siphon_ai_invites_total{result="rejected"}` should NOT tick. Any
  500/503 indicates the port pool or forge session manager hitting a
  limit — bump `[media].rtp_port_range` or scale horizontally.
- `siphon_ai_calls_total` only ticks during teardown. After the burst
  ends, `siphon_ai_calls_active` should fall back to 0 within ~30s.

## Reading the results

A "passing" run is boring. Common failure modes and where they show:

| Symptom                                   | Likely cause | Where to look |
|-------------------------------------------|--------------|---------------|
| RSS grows linearly over an hour          | A per-call allocation isn't being freed. | `dhat` or `heaptrack` against a shorter run. |
| Audio quality degrades after N minutes    | Jitter buffer drift or codec encoder state leak. | `forge_rtcp_jitter_ms` histogram. |
| Burst hits a `503 Service Unavailable`    | Port pool exhausted before old calls released. | `[media].rtp_port_range` size; ensure post-call `stop_session` is winning. |
| Burst hits a `500 Server Internal Error`  | `start_session` failure path (the one PR #19 fixed). | Daemon log; should NOT happen after #19 unless forge itself is broken. |
| `siphon_ai_hep_packets_dropped_total` rises | HEP queue too small for the call rate. | Bump `[hep].queue_capacity`. |

## Why this isn't in CI

A 1-hour soak gates on wall time, and a 500-call burst gates on a
non-trivial port range — neither belongs in the per-PR test suite.
These run on release gates and on demand. The §11.8 ten-questions
audit in `docs/OPERATIONS.md` validates the diagnostic surface
against a real call; this harness validates that the surface holds
up under realistic call volume.

When the harness is rerun for a release, paste the headline numbers
into the release notes (e.g., "1h soak: RSS held within 4 MB, no
quality degradation; 500-concurrent: all setups completed, no
rejections, teardown drained in 28s").
