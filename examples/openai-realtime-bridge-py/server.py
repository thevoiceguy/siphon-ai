#!/usr/bin/env python3
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

OPENAI_REALTIME_URL = "wss://api.openai.com/v1/realtime"
OPENAI_SAMPLE_RATE = 24_000


@dataclass
class Options:
    bind_host: str
    bind_port: int
    log_level: str
    openai_api_key: str
    openai_model: str
    voice: str
    instructions: str
    greeting: str
    turn_detection_threshold: float


def parse_args(argv: list[str] | None = None) -> Options:
    p = argparse.ArgumentParser(description="SiphonAI to OpenAI Realtime bridge.")
    p.add_argument("--bind", default="0.0.0.0:8765", metavar="HOST:PORT")
    p.add_argument("--model", default=os.environ.get("OPENAI_REALTIME_MODEL", "gpt-realtime"))
    p.add_argument("--voice", default=os.environ.get("OPENAI_REALTIME_VOICE", "ash"))
    p.add_argument(
        "--instructions",
        default=os.environ.get(
            "OPENAI_REALTIME_INSTRUCTIONS",
            "You are a helpful voice assistant on a phone call. Always reply in English unless the caller explicitly asks you to switch languages, translate something, or continue in another language. If the caller speaks another language without asking you to switch, you man switch to the matching language. Keep replies short and conversational.",
        ),
    )
    p.add_argument(
        "--greeting",
        default=os.environ.get(
            "OPENAI_REALTIME_GREETING",
            "Greet the caller warmly in English in one short sentence.",
        ),
    )
    p.add_argument(
        "--vad-threshold",
        type=float,
        default=float(os.environ.get("OPENAI_REALTIME_VAD_THRESHOLD", "0.5")),
    )
    p.add_argument(
        "--log-level",
        default=os.environ.get("LOG_LEVEL", "INFO"),
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
        greeting=args.greeting,
        turn_detection_threshold=args.vad_threshold,
    )


def resample_pcm16(samples: bytes, src_rate: int, dst_rate: int) -> bytes:
    if src_rate == dst_rate:
        return samples
    if not samples:
        return b""

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
        v = max(-32768, min(32767, v))
        struct.pack_into("<h", out, i * 2, v)

    return bytes(out)


@dataclass
class SessionContext:
    siphon_ws: ServerConnection
    call_id: str
    caller_rate: int
    openai_ws: Any
    opts: Options
    closing: asyncio.Event = field(default_factory=asyncio.Event)


async def pump_siphon_to_openai(ctx: SessionContext) -> None:
    try:
        async for message in ctx.siphon_ws:
            if isinstance(message, bytes):
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

            if mtype == "dtmf":
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
                await ctx.openai_ws.send(json.dumps({"type": "response.create"}))
                continue

            if mtype in {"speech_started", "speech_stopped", "mark", "error"}:
                LOG.debug("siphon control: %s", msg)
            else:
                LOG.debug("siphon other: %s", mtype)

    except websockets.exceptions.ConnectionClosed as e:
        LOG.info("SiphonAI WS closed: %s", e)
    except Exception:
        LOG.exception("pump_siphon_to_openai failed")
    finally:
        ctx.closing.set()


async def pump_openai_to_siphon(ctx: SessionContext) -> None:
    try:
        async for raw in ctx.openai_ws:
            try:
                evt = json.loads(raw)
            except json.JSONDecodeError:
                LOG.warning("invalid JSON from OpenAI; ignoring")
                continue

            etype = evt.get("type", "")

            if etype in {"response.audio.delta", "response.output_audio.delta"}:
                pcm24 = base64.b64decode(evt.get("delta", ""))
                pcm_caller = resample_pcm16(pcm24, OPENAI_SAMPLE_RATE, ctx.caller_rate)
                if pcm_caller:
                    await _send_audio_frames(ctx, pcm_caller)

            elif etype == "input_audio_buffer.speech_started":
                LOG.info("OpenAI VAD: caller speech started; clearing playout")
                await ctx.siphon_ws.send(
                    json.dumps({"type": "clear", "call_id": ctx.call_id})
                )

            elif etype == "input_audio_buffer.speech_stopped":
                LOG.debug("OpenAI VAD: caller speech stopped")

            elif etype in {
                "response.audio_transcript.done",
                "response.output_audio_transcript.done",
            }:
                LOG.info("model said: %r", evt.get("transcript", ""))

            elif etype in {
                "response.audio_transcript.delta",
                "response.output_audio_transcript.delta",
            }:
                LOG.debug("model transcript delta: %r", evt.get("delta", ""))

            elif etype == "response.done":
                LOG.debug("response.done")

            elif etype == "error":
                err = evt.get("error", {})
                LOG.error(
                    "OpenAI error: type=%s code=%s message=%s",
                    err.get("type"),
                    err.get("code"),
                    err.get("message"),
                )

            elif etype == "session.created":
                LOG.info("OpenAI session created")

            elif etype == "session.updated":
                LOG.info("OpenAI session updated")

            else:
                LOG.debug("openai event: %s", etype)

    except websockets.exceptions.ConnectionClosed as e:
        LOG.info("OpenAI WS closed: %s", e)
    except Exception:
        LOG.exception("pump_openai_to_siphon failed")
    finally:
        ctx.closing.set()


async def _send_audio_frames(ctx: SessionContext, pcm: bytes) -> None:
    frame_bytes = (ctx.caller_rate // 50) * 2

    for i in range(0, len(pcm), frame_bytes):
        chunk = pcm[i : i + frame_bytes]
        if len(chunk) < frame_bytes:
            chunk += b"\x00" * (frame_bytes - len(chunk))

        try:
            await ctx.siphon_ws.send(chunk)
        except websockets.exceptions.ConnectionClosed:
            return


async def handle(connection: ServerConnection, opts: Options) -> None:
    peer = connection.remote_address
    LOG.info("connect peer=%s subprotocol=%r", peer, connection.subprotocol)

    if connection.subprotocol != SUBPROTOCOL:
        LOG.warning("client did not negotiate %r; proceeding optimistically", SUBPROTOCOL)

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

    # W3C trace context (PROTOCOL.md §3.1, 0.23.0): present when the daemon
    # runs with [observability.otlp] enabled (also sent as a `traceparent`
    # upgrade header). A production bridge would extract it into its OTel
    # SDK so its STT/LLM/TTS spans join the daemon's per-call trace:
    #   ctx = TraceContextTextMapPropagator().extract(start["trace_context"])
    #   with tracer.start_as_current_span("bridge-call", context=ctx): ...
    # This example has no OTel dependency, so it just logs the value.
    if start.get("trace_context"):
        LOG.info(
            "start call_id=%s trace_context=%s", call_id, start["trace_context"]
        )

    url = f"{OPENAI_REALTIME_URL}?model={opts.openai_model}"

    try:
        openai_ws = await ws_connect(
            url,
            additional_headers={"Authorization": f"Bearer {opts.openai_api_key}"},
            max_size=16 * 1024 * 1024,
            ping_interval=20,
            ping_timeout=20,
        )
    except Exception as e:
        LOG.error("OpenAI WS connect failed: %s", e)
        await connection.close(code=1011, reason="upstream unavailable")
        return

    try:
        await openai_ws.send(
            json.dumps(
                {
                    "type": "session.update",
                    "session": {
                        "type": "realtime",
                        "instructions": opts.instructions,
                        "audio": {
                            "input": {
                                "format": {
                                    "type": "audio/pcm",
                                    "rate": OPENAI_SAMPLE_RATE,
                                },
                                "turn_detection": {
                                    "type": "server_vad",
                                    "threshold": opts.turn_detection_threshold,
                                    "prefix_padding_ms": 300,
                                    "silence_duration_ms": 500,
                                },
                            },
                            "output": {
                                "format": {
                                    "type": "audio/pcm",
                                    "rate": OPENAI_SAMPLE_RATE,
                                },
                                "voice": opts.voice,
                            },
                        },
                    },
                }
            )
        )

        await openai_ws.send(
            json.dumps(
                {
                    "type": "response.create",
                    "response": {
                        "instructions": opts.greeting,
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

        siphon_task = asyncio.create_task(pump_siphon_to_openai(ctx))
        openai_task = asyncio.create_task(pump_openai_to_siphon(ctx))

        try:
            await ctx.closing.wait()
        finally:
            for task in (siphon_task, openai_task):
                if not task.done():
                    task.cancel()
            await asyncio.gather(siphon_task, openai_task, return_exceptions=True)

    except websockets.exceptions.ConnectionClosed as e:
        LOG.error("OpenAI connection closed during setup/session: %s", e)
    except Exception:
        LOG.exception("session failed")
    finally:
        try:
            await openai_ws.close()
        except Exception:
            pass
        LOG.info("session ended call_id=%s", call_id)


def make_request_handler(_opts: Options):
    def process_request(connection: ServerConnection, request):
        if request.path == "/healthz":
            return connection.respond(200, "ok\n")
        return None

    return process_request


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

        loop = asyncio.get_running_loop()
        stop = loop.create_future()

        for sig in (signal.SIGINT, signal.SIGTERM):
            try:
                loop.add_signal_handler(sig, lambda: stop.set_result(None))
            except NotImplementedError:
                pass

        await stop
        server.close()


def cli() -> None:
    opts = parse_args()

    logging.basicConfig(
        level=opts.log_level,
        format="%(asctime)s %(levelname)s %(name)s: %(message)s",
    )

    logging.getLogger("websockets.server").setLevel(logging.WARNING)
    logging.getLogger("websockets.client").setLevel(logging.WARNING)

    try:
        asyncio.run(main(opts))
    except KeyboardInterrupt:
        pass


if __name__ == "__main__":
    sys.exit(cli() or 0)
