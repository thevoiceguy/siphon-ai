# siphon-ai-pipecat

A [Pipecat](https://www.pipecat.ai/) transport for
[SiphonAI](https://github.com/thevoiceguy/siphon-ai). Answer SIP calls,
from a carrier trunk or a PBX extension, with a Pipecat pipeline.

```
SIP trunk / PBX ──SIP+RTP──► SiphonAI ──WebSocket (protocol v1)──► SiphonTransport ──► your Pipecat pipeline (STT · LLM · TTS)
```

SiphonAI handles SIP, RTP, codecs, jitter, DTMF, hold and transfer. This
package turns its per-call WebSocket into a Pipecat transport. **It contains
no AI code.** Your pipeline chooses the STT, LLM and TTS services.

## Install

Not yet on PyPI. Install from the repo, with the protocol SDK it builds on:

```bash
pip install ./sdks/python ./sdks/pipecat
```

Requires Python 3.10 or later and `pipecat-ai` 1.12 or later (below 2.0).

## Quickstart

```python
from fastapi import FastAPI, WebSocket
from pipecat.pipeline.pipeline import Pipeline
from pipecat.pipeline.worker import PipelineWorker
from pipecat.workers.runner import WorkerRunner
from siphon_ai_pipecat import SiphonHandshakeError, SiphonTransport

app = FastAPI()


@app.websocket("/ws")
async def ws(websocket: WebSocket):
    try:
        transport = await SiphonTransport.accept(websocket)  # handshake + `start`
    except SiphonHandshakeError:
        return
    # transport.start is the typed `start` message: from_, to, direction,
    # sip.headers, audio.sample_rate, reconnected, barge_in_mode, …

    pipeline = Pipeline(
        [
            transport.input(),
            stt,
            user_aggregator,
            llm,
            tts,
            transport.output(),
            assistant_aggregator,
        ]
    )
    worker = PipelineWorker(pipeline, params=transport.pipeline_params())

    @transport.event_handler("on_client_connected")
    async def greet(
        transport, websocket
    ): ...  # queue the greeting, e.g. await worker.queue_frames([LLMRunFrame()])

    @transport.event_handler("on_call_stopped")
    async def stopped(transport, reason):  # caller_hangup, transfer, park, …
        await worker.cancel()

    @transport.event_handler("on_client_disconnected")
    async def gone(transport, websocket):
        await worker.cancel()

    runner = WorkerRunner(handle_sigint=False)
    await runner.add_workers(worker)
    await runner.run()
```

To run a complete bot, see [`examples/pipecat-bot-py`](../../examples/pipecat-bot-py/).

## SiphonAI configuration

Point a route at the bot and let Pipecat decide interruptions:

```toml
[[route]]
name = "pipecat"
[route.match]
any = true
[route.bridge]
ws_url = "ws://127.0.0.1:8080/ws"

[route.bridge.barge_in]
mode = "notify_only"     # or "pause", see below
```

| `barge_in_mode` | What happens |
|---|---|
| `notify_only` (**recommended**) | Only Pipecat's turn strategy can cut the bot. When Pipecat emits an `InterruptionFrame`, the transport sends `clear`. |
| `pause` | SiphonAI ducks the bot within one frame when the caller speaks, then the transport arbitrates. If Pipecat interrupts before the deadline, the transport sends `clear`, which confirms the barge-in. If not, it sends `barge_in_reject` and the bot resumes mid-word. This gives the fastest perceived reaction. |
| `auto_clear` (daemon default) | Works, but SiphonAI flushes the bot on its own voice activity detection (VAD). That includes a cough Pipecat would have ignored, after which Pipecat still believes the bot is talking. The transport logs a warning. |

## What the transport handles

- **Framing and pacing.** Outbound audio is sent as exact 20 ms PCM16 frames
  at real time, at most `playout_lead_ms` (60 ms) ahead.
  Pipecat's stock WebSocket output streams at 2× real time, which would
  overrun SiphonAI's 200 ms playout window and drop audio. As a side
  effect, Pipecat's "bot speaking" state now tracks what the caller
  actually hears.
- **Sample rates.** The transport's input and output rates are pinned to the
  call's negotiated rate (8 or 16 kHz). Pipecat resamples TTS output, and
  `transport.pipeline_params()` makes TTS services synthesize at that rate
  where they can.
- **Start deadline.** One 20 ms silence frame goes out as soon as the pipeline
  runs, so a listen-first bot or a slow cold start doesn't trip SiphonAI's
  `server_too_slow` 5-second deadline (`prime_start_deadline`).
- **Ending the call.** An `EndFrame` (for example an `EndWorkerFrame` after
  "goodbye") waits for the caller to *hear* the last audio, using a `mark`
  round-trip, and then sends `hangup`. A `CancelFrame` hangs up immediately.
  Neither sends a hangup after SiphonAI has already sent `stop`, or over a
  pending `transfer`/`park`: the transport waits for that handoff's `stop`
  and hangs up only if the handoff fails. This
  matters because with `ws_reconnect_enabled` a bare socket close would be
  redialed instead of ending the call (`auto_hang_up`).
- **DTMF.** Caller digits arrive as `InputDTMFFrame` (keys A–D, which Pipecat
  doesn't model, arrive only as events). An `OutputDTMFFrame` or
  `OutputDTMFUrgentFrame` is sent as RFC 2833 through SiphonAI.
- **Every other event** (`speech_started`, `hold`, `rtp_stats`, `mark`,
  `silence_detected`, …) is pushed downstream as a `SiphonEventFrame`. The
  event is a typed `siphon_ai_server` object, also delivered to the
  `on_siphon_event(transport, event)` handler. Unknown future event types
  arrive as `UnknownEvent` and never break the call.

SiphonAI's own `speech_started`/`speech_stopped` events are **not** fed into
Pipecat's VAD or turn detection. Pipecat runs its own VAD on the audio, and
two VADs driving one aggregator would race.

## Sending commands

From a function-call handler, sending immediately:

```python
await transport.transfer("sip:agent@pbx.example.com")
await transport.hold()
await transport.resume()
await transport.park(slot="lot-3")
await transport.send_command("start_recording")
```

To send a command in order behind the audio already queued ("say goodbye,
then transfer"), push a frame:

```python
from siphon_ai_pipecat import siphon_command

await worker.queue_frames(
    [
        TTSSpeakFrame("Transferring you now."),
        siphon_command("transfer", target="sip:agent@pbx.example.com"),
    ]
)
```

Only protocol v1 command types are accepted. An unknown `type` raises,
because SiphonAI would treat it as a fatal `protocol_error`.

## Parameters (`SiphonParams`)

| Parameter | Default | Meaning |
|---|---|---|
| `playout_lead_ms` | `60` | How far outbound audio may run ahead of real time. Must be below 200. |
| `prime_start_deadline` | `True` | Send one silence frame when the pipeline starts. |
| `auto_hang_up` | `True` | Send `hangup` when the pipeline ends. Disable for bots that `park`/`transfer` and then just stop. |
| `end_drain_timeout_secs` | `2.0` | How long to wait for the last audio to play before the `EndFrame` hangup. |
| `pause_decision_margin_ms` | `100` | Pause mode: send `barge_in_reject` this long before the deadline. |
| `dtmf_duration_ms` | `160` | Per-digit duration for `OutputDTMFFrame`. |

`SiphonParams` extends Pipecat's `TransportParams`, so audio filters, mixers
and similar settings still apply. The sample rates are the exception: they
are always set to the call's rate.

`SiphonTransport.accept(websocket, params, auth_token=...)` checks
`Authorization: Bearer` against SiphonAI's `[bridge].auth_bearer`.

## Things to know

- **The call is already answered.** SiphonAI sends `200 OK` before it opens
  the WebSocket, so a bot cannot reject a call (PROTOCOL.md §4.3). Screen
  calls with routes, `[[trunk]]` or `[sip.admission]` instead.
- **Reconnects get a fresh pipeline.** With `ws_reconnect_enabled`, a dropped
  socket is redialed and arrives as a new connection with
  `transport.start.reconnected == True`. Keep conversation state keyed by
  `transport.start.call_id` if the bot should pick up where it left off.

Design rationale: [`docs/design/DESIGN_PIPECAT.md`](../../docs/design/DESIGN_PIPECAT.md).
Wire protocol: [`docs/PROTOCOL.md`](../../docs/PROTOCOL.md).
