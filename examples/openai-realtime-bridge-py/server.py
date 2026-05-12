#!/usr/bin/env python3
"""
OpenAI Realtime ↔ SiphonAI bridge.

A reference WebSocket server that speaks the SiphonAI bridge protocol
(see ``docs/PROTOCOL.md`` in the SiphonAI repo) on the caller side
and the OpenAI Realtime API on the model side. Each inbound SiphonAI
connection opens a fresh OpenAI session; audio shuttles in both
directions with the sample-rate conversion the two formats require.

What this example *is*
----------------------
* A small (~400 lines) end-to-end demonstration that runs out of the
  box with a single ``OPENAI_API_KEY`` env var.
* The canonical pattern for plumbing a phone call into a
  conversational LLM: half-duplex VAD-driven turns + barge-in via
  SiphonAI's ``clear`` control message.

What this example is *not*
--------------------------
* A production-grade resampler. We use a stdlib-only linear
  interpolator that's fine for demos and intelligibility but adds
  ~0.5 ms of aliasing distortion. For production swap in
  ``scipy.signal.resample_poly`` (commented stub at the bottom).
* A function-calling / tool-use harness. Add tool definitions in
  ``session.update`` and route ``response.function_call_arguments.*``
  events to your handlers — out of scope for the bridge itself.
* Multi-tenant. One process per session is fine for tens of calls;
  beyond that, run it under a process manager or scale horizontally.

Run
---
    pip install -r requirements.txt
    export OPENAI_API_KEY=sk-...
    python3 server.py --bind 0.0.0.0:8765

Then point SiphonAI's ``bridge.ws_url`` at this server and place a
SIP call. Audio flows caller → SiphonAI → here → OpenAI Realtime,
and the model's spoken response comes back the same path.
"""

from __future__ import annotations

import argparse
import asyncio
import base64
import json
import logging
import os
import signal
import struct
import sys
from dataclasses import dataclass, field
from typing import Any

import websockets
from websockets.asyncio.client import connect as ws_connect
from websockets.asyncio.server import ServerConnection, serve

LOG = logging.getLogger("openai-bridge")
SUBPROTOCOL = "siphon-ai.v1"

# OpenAI Realtime endpoint. The model name goes on the query string;
# any tools / tool configuration / voice selection happen via the
# session.update event we send after the WS handshake.
OPENAI_REALTIME_URL = "wss://api.openai.com/v1/realtime"

# OpenAI Realtime fixes PCM16 input/output sample rate at 24 kHz.
# SiphonAI's start.audio.sample_rate is the caller side (8 or 16 kHz
# in v1); the bridge resamples between the two.
OPENAI_SAMPLE_RATE = 24_000


# ─── Options ─────────────────────────────────────────────────────────────────


@dataclass
class Options:
    bind_host: str
    bind_port: int
    log_level: str
    openai_api_key: str
    openai_model: str
    voice: str
    instructions: str
    turn_detection_threshold: float


def parse_args(argv: list[str] | None = None) -> Options:
    p = argparse.ArgumentParser(
        description="SiphonAI ↔ OpenAI Realtime bridge.",
        epilog="OPENAI_API_KEY env var is required.",
    )
    p.add_argument(
        "--bind",
        default="0.0.0.0:8765",
        metavar="HOST:PORT",
        help="address to listen on for SiphonAI (default: 0.0.0.0:8765)",
    )
    p.add_argument(
        "--model",
        default=os.environ.get("OPENAI_REALTIME_MODEL", "gpt-realtime-2025-10-01"),
        help="OpenAI Realtime model id (default: gpt-realtime-2025-10-01, override with OPENAI_REALTIME_MODEL)",
    )
    p.add_argument(
        "--voice",
        default="alloy",
        choices=["alloy", "echo", "fable", "onyx", "nova", "shimmer", "verse"],
        help="OpenAI voice (default: alloy)",
    )
    p.add_argument(
        "--instructions",
        default="You are a helpful voice assistant on a phone call. Keep replies short, conversational, and avoid lists.",
        help="System instructions sent to the model.",
    )
    p.add_argument(
        "--vad-threshold",
        type=float,
        default=0.5,
        help="OpenAI server-VAD threshold (0.0 quiet → 1.0 loud; default 0.5).",
    )
    p.add_argument(
        "--log-level",
        default="INFO",
        choices=["DEBUG", "INFO", "WARNING", "ERROR"],
    )
    args = p.parse_args(argv)

    key = os.environ.get("OPENAI_API_KEY", "").strip()
    if not key:
        p.error("OPENAI_API_KEY environment variable is required")

    host, _, port = args.bind.partition(":")
    if not port:
        p.error("--bind must be HOST:PORT")
    return Options(
        bind_host=host,
        bind_port=int(port),
        log_level=args.log_level,
        openai_api_key=key,
        openai_model=args.model,
        voice=args.voice,
        instructions=args.instructions,
        turn_detection_threshold=args.vad_threshold,
    )


# ─── Resampling ──────────────────────────────────────────────────────────────


def resample_pcm16(samples: bytes, src_rate: int, dst_rate: int) -> bytes:
    """Resample mono PCM16-LE between two sample rates.

    Linear interpolation. Adequate for speech intelligibility but
    introduces aliasing on the upsample direction. Swap for
    ``scipy.signal.resample_poly`` in production.
    """
    if src_rate == dst_rate:
        return samples
    if not samples:
        return b""

    # struct.iter_unpack avoids materializing a Python int list for
    # the input — important when SiphonAI sends 8000-sample-rate
    # frames at 50 Hz (16000 samples/sec, each int16).
    src = struct.unpack(f"<{len(samples) // 2}h", samples)
    src_len = len(src)
    if src_len < 2:
        return samples

    ratio = dst_rate / src_rate
    dst_len = max(1, int(src_len * ratio))
    out = bytearray(dst_len * 2)
    step = (src_len - 1) / max(1, dst_len - 1)
    for i in range(dst_len):
        x = i * step
        x0 = int(x)
        x1 = min(x0 + 1, src_len - 1)
        frac = x - x0
        v = int(src[x0] * (1.0 - frac) + src[x1] * frac)
        # Clamp to int16 range — interpolation between two near-max
        # samples can overshoot by 1 LSB at exact boundaries.
        if v > 32767:
            v = 32767
        elif v < -32768:
            v = -32768
        struct.pack_into("<h", out, i * 2, v)
    return bytes(out)


# ─── Per-call session ────────────────────────────────────────────────────────


@dataclass
class SessionContext:
    """All the state needed to bridge one SiphonAI call ↔ one OpenAI
    Realtime session. Lives for the duration of the call.
    """

    siphon_ws: ServerConnection
    call_id: str
    caller_rate: int
    openai_ws: Any  # websockets.asyncio.client.ClientConnection
    opts: Options
    closing: asyncio.Event = field(default_factory=asyncio.Event)


# ─── SiphonAI side: receive ──────────────────────────────────────────────────


async def pump_siphon_to_openai(ctx: SessionContext) -> None:
    """Read frames from the caller-side WS; ship audio to OpenAI."""
    try:
        async for message in ctx.siphon_ws:
            if isinstance(message, bytes):
                # Audio frame: resample → base64 → OpenAI.
                pcm24 = resample_pcm16(message, ctx.caller_rate, OPENAI_SAMPLE_RATE)
                await ctx.openai_ws.send(
                    json.dumps(
                        {
                            "type": "input_audio_buffer.append",
                            "audio": base64.b64encode(pcm24).decode("ascii"),
                        }
                    )
                )
                continue

            # Text frame: SiphonAI control message. We only look at
            # the ones that matter to the model's turn handling.
            try:
                msg = json.loads(message)
            except json.JSONDecodeError:
                LOG.warning("invalid JSON from SiphonAI; ignoring")
                continue

            mtype = msg.get("type")
            if mtype == "stop":
                LOG.info("SiphonAI sent stop: reason=%s", msg.get("reason"))
                ctx.closing.set()
                break
            elif mtype == "dtmf":
                # Surface DTMF as a text message OpenAI can react to.
                # The model rarely needs this but operators sometimes
                # plumb it into menu logic.
                digit = msg.get("digit", "")
                LOG.info("DTMF: %s", digit)
                await ctx.openai_ws.send(
                    json.dumps(
                        {
                            "type": "conversation.item.create",
                            "item": {
                                "type": "message",
                                "role": "user",
                                "content": [
                                    {
                                        "type": "input_text",
                                        "text": f"[caller pressed DTMF digit '{digit}']",
                                    }
                                ],
                            },
                        }
                    )
                )
            elif mtype in {"speech_started", "speech_stopped", "mark", "error"}:
                LOG.debug("siphon control: %s", msg)
            else:
                LOG.debug("siphon other: %s", mtype)
    except websockets.exceptions.ConnectionClosed:
        LOG.info("SiphonAI WS closed")
    finally:
        ctx.closing.set()


# ─── OpenAI side: receive ────────────────────────────────────────────────────


async def pump_openai_to_siphon(ctx: SessionContext) -> None:
    """Read events from OpenAI; forward audio + barge-in to SiphonAI."""
    try:
        async for raw in ctx.openai_ws:
            # OpenAI Realtime is JSON-only over WS.
            try:
                evt = json.loads(raw)
            except json.JSONDecodeError:
                LOG.warning("invalid JSON from OpenAI; ignoring")
                continue

            etype = evt.get("type", "")

            if etype == "response.audio.delta":
                # Incremental audio chunk, base64 PCM16 at 24 kHz.
                # Decode → resample to caller rate → binary WS frame.
                pcm24 = base64.b64decode(evt.get("delta", ""))
                pcm_caller = resample_pcm16(pcm24, OPENAI_SAMPLE_RATE, ctx.caller_rate)
                if pcm_caller:
                    await _send_audio_frames(ctx, pcm_caller)

            elif etype == "input_audio_buffer.speech_started":
                # OpenAI's server-side VAD detected the caller
                # starting to speak. Tell SiphonAI to drop anything
                # queued for playout so the user isn't talked over.
                LOG.info("OpenAI VAD: caller speech started — clearing playout")
                await ctx.siphon_ws.send(
                    json.dumps({"type": "clear", "call_id": ctx.call_id})
                )

            elif etype == "input_audio_buffer.speech_stopped":
                LOG.debug("OpenAI VAD: caller speech stopped")

            elif etype == "response.audio_transcript.done":
                LOG.info("model said: %r", evt.get("transcript", ""))

            elif etype == "response.done":
                LOG.debug("response.done")

            elif etype == "error":
                err = evt.get("error", {})
                LOG.error("OpenAI error: %s — %s", err.get("type"), err.get("message"))

            elif etype == "session.created":
                LOG.info("OpenAI session created")

            elif etype == "session.updated":
                LOG.debug("OpenAI session updated")

            else:
                LOG.debug("openai event: %s", etype)
    except websockets.exceptions.ConnectionClosed:
        LOG.info("OpenAI WS closed")
    finally:
        ctx.closing.set()


async def _send_audio_frames(ctx: SessionContext, pcm: bytes) -> None:
    """Slice OpenAI's variable-length audio chunks into the 20 ms
    frames the SiphonAI protocol mandates.

    PCM16 mono: bytes_per_frame = (sample_rate / 50) * 2.
    """
    frame_bytes = (ctx.caller_rate // 50) * 2
    for i in range(0, len(pcm), frame_bytes):
        chunk = pcm[i : i + frame_bytes]
        if len(chunk) < frame_bytes:
            # Pad the final fragment with silence so the wire size
            # matches the protocol's invariant. SiphonAI's playout
            # path tolerates a partial tail, but consistency keeps
            # the frame count metrics clean.
            chunk = chunk + b"\x00" * (frame_bytes - len(chunk))
        try:
            await ctx.siphon_ws.send(chunk)
        except websockets.exceptions.ConnectionClosed:
            return


# ─── Per-connection driver ───────────────────────────────────────────────────


async def handle(connection: ServerConnection, opts: Options) -> None:
    peer = connection.remote_address
    LOG.info("connect peer=%s subprotocol=%r", peer, connection.subprotocol)

    if connection.subprotocol != SUBPROTOCOL:
        LOG.warning("client did not negotiate %r; proceeding optimistically", SUBPROTOCOL)

    # Read the SiphonAI `start` message first — it carries the
    # caller's sample rate, which determines the resampling ratio
    # for the whole call.
    try:
        first = await asyncio.wait_for(connection.recv(), timeout=10)
    except asyncio.TimeoutError:
        LOG.warning("no start message within 10s; closing")
        await connection.close(code=1002, reason="missing start")
        return
    except websockets.exceptions.ConnectionClosed:
        return

    if not isinstance(first, str):
        LOG.warning("first message was binary, expected start JSON")
        await connection.close(code=1002, reason="missing start")
        return

    try:
        start = json.loads(first)
    except json.JSONDecodeError as e:
        LOG.warning("start is not JSON: %s", e)
        await connection.close(code=1002, reason="invalid start")
        return

    if start.get("type") != "start":
        LOG.warning("first message type=%r, expected 'start'", start.get("type"))
        await connection.close(code=1002, reason="expected start")
        return

    call_id = start.get("call_id", "")
    audio = start.get("audio", {})
    caller_rate = int(audio.get("sample_rate", 8000))
    LOG.info(
        "start call_id=%s caller_rate=%d from=%s to=%s",
        call_id,
        caller_rate,
        start.get("from"),
        start.get("to"),
    )

    # Open the OpenAI session. The bearer header is auth; the
    # `realtime=v1` beta header is what OpenAI's docs require.
    url = f"{OPENAI_REALTIME_URL}?model={opts.openai_model}"
    additional_headers = {
        "Authorization": f"Bearer {opts.openai_api_key}",
        "OpenAI-Beta": "realtime=v1",
    }
    try:
        openai_ws = await ws_connect(
            url,
            additional_headers=additional_headers,
            max_size=16 * 1024 * 1024,
            ping_interval=20,
            ping_timeout=20,
        )
    except Exception as e:
        LOG.error("OpenAI WS connect failed: %s", e)
        await connection.close(code=1011, reason="upstream unavailable")
        return

    try:
        # Configure the session before any audio flows. Server VAD
        # means we don't need to manage turn boundaries by hand.
        await openai_ws.send(
            json.dumps(
                {
                    "type": "session.update",
                    "session": {
                        "modalities": ["audio", "text"],
                        "voice": opts.voice,
                        "instructions": opts.instructions,
                        "input_audio_format": "pcm16",
                        "output_audio_format": "pcm16",
                        "turn_detection": {
                            "type": "server_vad",
                            "threshold": opts.turn_detection_threshold,
                            "prefix_padding_ms": 300,
                            "silence_duration_ms": 500,
                        },
                    },
                }
            )
        )

        ctx = SessionContext(
            siphon_ws=connection,
            call_id=call_id,
            caller_rate=caller_rate,
            openai_ws=openai_ws,
            opts=opts,
        )

        # Pump in parallel; whichever side closes first sets the
        # `closing` event and the other side bails on its next
        # iteration.
        siphon_task = asyncio.create_task(pump_siphon_to_openai(ctx))
        openai_task = asyncio.create_task(pump_openai_to_siphon(ctx))
        try:
            await ctx.closing.wait()
        finally:
            for task in (siphon_task, openai_task):
                if not task.done():
                    task.cancel()
            await asyncio.gather(siphon_task, openai_task, return_exceptions=True)
    finally:
        try:
            await openai_ws.close()
        except Exception:
            pass
        LOG.info("session ended call_id=%s", call_id)


# ─── HTTP-side concerns: auth + subprotocol ──────────────────────────────────


def make_request_handler(_opts: Options):
    """Build a websockets ``process_request`` hook for the SiphonAI-
    side server. We don't enforce auth on incoming connections in
    this example — SiphonAI's own `bridge.ws_auth_header` already
    covers that path. The hook stays here as a hook point in case
    operators want to add bearer-auth in front.
    """

    def process_request(connection: ServerConnection, request):
        # Same /healthz short-circuit as the echo server — keeps
        # Docker / k8s probes out of the WS error log.
        if request.path == "/healthz":
            return connection.respond(200, "ok\n")
        return None

    return process_request


# ─── Entrypoint ──────────────────────────────────────────────────────────────


async def main(opts: Options) -> None:
    async def handler(connection: ServerConnection) -> None:
        await handle(connection, opts)

    async with serve(
        handler,
        host=opts.bind_host,
        port=opts.bind_port,
        subprotocols=[SUBPROTOCOL],
        process_request=make_request_handler(opts),
        max_size=256 * 1024,
        ping_interval=15,
        ping_timeout=10,
    ) as server:
        LOG.info(
            "listening on ws://%s:%d  (model=%s, voice=%s)",
            opts.bind_host,
            opts.bind_port,
            opts.openai_model,
            opts.voice,
        )

        # Wait for SIGINT/SIGTERM.
        loop = asyncio.get_running_loop()
        stop = loop.create_future()
        for sig in (signal.SIGINT, signal.SIGTERM):
            try:
                loop.add_signal_handler(sig, lambda: stop.set_result(None))
            except NotImplementedError:
                # Windows signal handlers are limited; fall through.
                pass
        await stop
        server.close()


def cli() -> None:
    opts = parse_args()
    logging.basicConfig(
        level=opts.log_level,
        format="%(asctime)s %(levelname)s %(name)s: %(message)s",
    )
    # websockets logs handshake details at INFO; keep them quiet
    # unless the operator opts in.
    logging.getLogger("websockets.server").setLevel(logging.WARNING)
    logging.getLogger("websockets.client").setLevel(logging.WARNING)

    try:
        asyncio.run(main(opts))
    except KeyboardInterrupt:
        pass


if __name__ == "__main__":
    sys.exit(cli() or 0)


# ─── Production resampler swap-in (for reference) ────────────────────────────
#
# from scipy.signal import resample_poly
# import numpy as np
#
# def resample_pcm16(samples: bytes, src_rate: int, dst_rate: int) -> bytes:
#     if src_rate == dst_rate:
#         return samples
#     pcm = np.frombuffer(samples, dtype="<i2")
#     # gcd-based up/down factors keep the FIR length tractable.
#     from math import gcd
#     g = gcd(src_rate, dst_rate)
#     up, down = dst_rate // g, src_rate // g
#     out = resample_poly(pcm, up, down).astype("<i2")
#     return out.tobytes()
