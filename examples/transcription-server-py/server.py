#!/usr/bin/env python3
"""
SiphonAI bridge → Deepgram streaming transcription.

A reference WebSocket server for the SiphonAI bridge protocol v1
(`docs/PROTOCOL.md`) that doesn't bridge to an AI agent at all — it
transcribes every call and emits one JSON line per transcript on
stdout. This is the simplest demonstration of SiphonAI's
"non-agent" use case: real-time transcription, compliance
recording, supervisor assist, etc.

Per-call flow:

    SiphonAI ───PCM16-LE 20ms───► this server ───PCM16-LE───► Deepgram WSS
                                       │
                                       ◄────── transcripts ───
                                       │
                                       ▼
                                    stdout

No audio is sent back to the caller. SiphonAI sees a quiet WS
server; the caller talks, the AI doesn't reply. That's the point —
this is an observer, not an agent.

## Required environment

    DEEPGRAM_API_KEY    Deepgram REST API token.

## Run

    python3 server.py --bind 0.0.0.0:8080

Then point SiphonAI at it:

    [bridge]
    ws_url = "ws://localhost:8080"

## Swapping providers

This script is intentionally one file with one provider so the
data-flow is legible. To wire a different STT (AssemblyAI, OpenAI
Realtime, Google Speech, self-hosted Whisper), replace the
`open_deepgram` / `_extract_transcript` pair with equivalents
against the target's streaming WS API. The SiphonAI side and the
transcript-record shape stay identical. See the multi-provider
pattern in `examples/openai-realtime-bridge-py/` (0.1.0 reference)
for a worked toolkit example.

This file is deliberately not unit-tested — it is a runnable
reference. Validate end-to-end by placing a SIP call against
SiphonAI configured to bridge to this server.
"""

from __future__ import annotations

import argparse
import asyncio
import json
import logging
import os
import sys
import urllib.parse
from dataclasses import dataclass
from datetime import datetime, timezone
from typing import Any

import websockets
from websockets.asyncio.client import ClientConnection
from websockets.asyncio.client import connect as ws_connect
from websockets.asyncio.server import ServerConnection, serve

LOG = logging.getLogger("transcription-ws")

# v1 subprotocol; SiphonAI advertises it on the upgrade.
SUBPROTOCOL = "siphon-ai.v1"

# Deepgram's streaming WS endpoint. Query params get filled in
# per-call from the `start` message so the sample-rate matches what
# SiphonAI negotiated (PCMU → 8 kHz, G.722 → 16 kHz).
DEEPGRAM_HOST = "wss://api.deepgram.com/v1/listen"


@dataclass
class Options:
    bind_host: str
    bind_port: int
    api_key: str
    model: str
    language: str
    interim_results: bool
    smart_format: bool
    auth_token: str | None
    log_level: str


# ─── Deepgram client ────────────────────────────────────────────────────────


def _deepgram_url(opts: Options, sample_rate: int) -> str:
    """Build the streaming-listen URL with per-call parameters."""
    params = {
        "encoding": "linear16",
        "sample_rate": str(sample_rate),
        "channels": "1",
        "model": opts.model,
        "language": opts.language,
        "smart_format": "true" if opts.smart_format else "false",
        "interim_results": "true" if opts.interim_results else "false",
    }
    return f"{DEEPGRAM_HOST}?{urllib.parse.urlencode(params)}"


async def _open_deepgram(opts: Options, sample_rate: int) -> ClientConnection:
    """Open the streaming connection. Caller is responsible for closing."""
    url = _deepgram_url(opts, sample_rate)
    LOG.debug("opening deepgram: %s", url)
    # Deepgram expects the API key as a `Token` Authorization header,
    # not Bearer.
    headers = [("Authorization", f"Token {opts.api_key}")]
    return await ws_connect(url, additional_headers=headers, max_size=None)


def _extract_transcript(msg: dict[str, Any]) -> dict[str, Any] | None:
    """
    Pull a `(text, is_final, confidence, ...)` record out of a Deepgram
    message. Returns `None` for messages that don't carry a transcript
    (Metadata, SpeechStarted, UtteranceEnd, etc.).
    """
    if msg.get("type") != "Results":
        return None
    channel = msg.get("channel") or {}
    alternatives = channel.get("alternatives") or []
    if not alternatives:
        return None
    alt = alternatives[0]
    text = alt.get("transcript")
    if not text:
        # Deepgram emits empty interim transcripts; skip the noise.
        return None
    return {
        "text": text,
        "is_final": bool(msg.get("is_final")),
        "speech_final": bool(msg.get("speech_final")),
        "confidence": alt.get("confidence"),
        "start_s": msg.get("start"),
        "duration_s": msg.get("duration"),
        "words": alt.get("words") or [],
    }


# ─── Per-call handler ──────────────────────────────────────────────────────


async def handle(connection: ServerConnection, opts: Options) -> None:
    peer = connection.remote_address
    call_id: str | None = None
    sip_call_id: str | None = None
    sample_rate: int | None = None

    LOG.info("connect peer=%s subprotocol=%r", peer, connection.subprotocol)
    if connection.subprotocol != SUBPROTOCOL:
        LOG.warning(
            "client did not negotiate %r; proceeding optimistically", SUBPROTOCOL
        )

    dg: ClientConnection | None = None
    dg_recv_task: asyncio.Task[None] | None = None
    bytes_to_dg = 0
    transcripts_emitted = 0

    async def _drain_deepgram() -> None:
        nonlocal transcripts_emitted
        assert dg is not None
        try:
            async for raw in dg:
                if isinstance(raw, bytes):
                    # Deepgram doesn't send binary in this direction;
                    # skip.
                    continue
                try:
                    msg = json.loads(raw)
                except json.JSONDecodeError:
                    LOG.debug("deepgram sent non-JSON text; skipping")
                    continue
                record = _extract_transcript(msg)
                if record is None:
                    continue
                _emit(call_id, sip_call_id, record)
                transcripts_emitted += 1
        except websockets.exceptions.ConnectionClosed:
            LOG.debug("deepgram connection closed")

    try:
        async for message in connection:
            if isinstance(message, bytes):
                # PCM16-LE 20 ms audio. Forward to Deepgram verbatim
                # once the connection's open. Frames received before
                # `start` (shouldn't happen per spec §3.1 but be
                # defensive) are dropped.
                if dg is None:
                    LOG.debug("dropping pre-start audio frame")
                    continue
                try:
                    await dg.send(message)
                    bytes_to_dg += len(message)
                except websockets.exceptions.ConnectionClosed:
                    LOG.warning("deepgram closed unexpectedly; aborting call")
                    await connection.close(code=1011, reason="stt closed")
                    return
                continue

            # Text frame: parse and dispatch.
            try:
                msg = json.loads(message)
            except json.JSONDecodeError as e:
                LOG.warning("invalid JSON on text frame: %s — ignoring", e)
                continue

            mtype = msg.get("type")
            if mtype == "start":
                call_id = msg.get("call_id")
                audio = msg.get("audio", {})
                sip = msg.get("sip", {})
                sip_call_id = sip.get("call_id")
                sample_rate = int(audio.get("sample_rate", 8000))
                LOG.info(
                    "start call_id=%s sip_call_id=%s rate=%s model=%s lang=%s",
                    call_id,
                    sip_call_id,
                    sample_rate,
                    opts.model,
                    opts.language,
                )
                if msg.get("version") != "1":
                    LOG.error("unsupported protocol version %r; closing", msg.get("version"))
                    await connection.close(code=1003, reason="unsupported version")
                    return
                # Open the Deepgram stream. Run its recv loop as a
                # background task so we can keep pushing audio onto
                # `dg` from the main loop without blocking on receive.
                try:
                    dg = await _open_deepgram(opts, sample_rate)
                except Exception as e:
                    LOG.error("failed to open deepgram: %s", e)
                    await connection.close(code=1011, reason="stt unavailable")
                    return
                dg_recv_task = asyncio.create_task(_drain_deepgram())

            elif mtype == "stop":
                LOG.info("stop call_id=%s reason=%s", call_id, msg.get("reason"))
                # SiphonAI closes the connection right after `stop`;
                # we drop out of the loop and clean up below.
                break

            elif mtype in {
                "speech_started",
                "speech_stopped",
                "silence_detected",
                "dead_air_detected",
                "rtp_stats",
                "dtmf",
                "mark",
                "hold",
                "resume",
                "error",
            }:
                # Informational events — log them for observability
                # but don't reply. A production transcription server
                # might forward these to its own pipeline.
                LOG.debug("%s: %s", mtype, msg)

            else:
                LOG.warning("unknown text message type=%r", mtype)

    except websockets.exceptions.ConnectionClosedError as e:
        LOG.info("siphon connection closed unexpectedly: %s", e)
    except websockets.exceptions.ConnectionClosedOK:
        LOG.info("siphon connection closed cleanly")
    finally:
        # Close Deepgram gracefully — sending the empty close-frame
        # tells DG to flush the final transcript, which the recv loop
        # then picks up and emits before exiting.
        if dg is not None:
            try:
                await dg.close()
            except Exception:
                pass
        if dg_recv_task is not None:
            try:
                await asyncio.wait_for(dg_recv_task, timeout=2.0)
            except (asyncio.TimeoutError, asyncio.CancelledError):
                dg_recv_task.cancel()
        LOG.info(
            "done peer=%s call_id=%s sip_call_id=%s bytes_to_dg=%d transcripts=%d",
            peer,
            call_id,
            sip_call_id,
            bytes_to_dg,
            transcripts_emitted,
        )


def _emit(call_id: str | None, sip_call_id: str | None, record: dict[str, Any]) -> None:
    """
    Print one JSON line per transcript on stdout. Downstream
    pipelines (a log shipper, a webhook bridge, an analytics service)
    consume this stream — that's the integration seam for everything
    beyond a screen-printer.
    """
    line = {
        "ts": datetime.now(timezone.utc).isoformat(),
        "call_id": call_id,
        "sip_call_id": sip_call_id,
        **record,
    }
    # `sys.stdout.write` + flush rather than print() so each transcript
    # is a complete line even when stdout is line-buffered to a pipe.
    sys.stdout.write(json.dumps(line, ensure_ascii=False) + "\n")
    sys.stdout.flush()


# ─── Request gate ──────────────────────────────────────────────────────────


def make_request_handler(opts: Options):
    """
    Optional bearer-token gate on the WS upgrade (same shape as the
    echo server). When `--auth-token TOKEN` is set, the upgrade
    requires `Authorization: Bearer TOKEN`; otherwise it accepts
    anything.
    """

    def process_request(connection: ServerConnection, request) -> Any | None:
        if opts.auth_token is None:
            return None
        got = request.headers.get("authorization", "")
        if got != f"Bearer {opts.auth_token}":
            LOG.warning("upgrade rejected: bad/missing Authorization")
            return connection.respond(401, b"unauthorized\n")
        return None

    return process_request


# ─── CLI / main ────────────────────────────────────────────────────────────


def parse_args(argv: list[str] | None = None) -> Options:
    p = argparse.ArgumentParser(
        description="Streaming transcription WS server (Deepgram) for SiphonAI."
    )
    p.add_argument(
        "--bind",
        default="0.0.0.0:8080",
        metavar="HOST:PORT",
        help="WebSocket listen address (default: 0.0.0.0:8080).",
    )
    p.add_argument(
        "--model",
        default="nova-3",
        help="Deepgram model (default: nova-3). Try nova-2 for the previous generation.",
    )
    p.add_argument(
        "--language",
        default="en",
        help="BCP-47 language code (default: en).",
    )
    p.add_argument(
        "--no-interim",
        action="store_true",
        help="Disable Deepgram interim results; emit finals only.",
    )
    p.add_argument(
        "--no-smart-format",
        action="store_true",
        help="Disable Deepgram smart-format (numerals, punctuation).",
    )
    p.add_argument(
        "--auth-token",
        default=None,
        metavar="TOKEN",
        help="If set, require `Authorization: Bearer TOKEN` on the upgrade.",
    )
    p.add_argument(
        "--log-level",
        default="INFO",
        choices=["DEBUG", "INFO", "WARNING", "ERROR"],
    )
    ns = p.parse_args(argv)

    api_key = os.environ.get("DEEPGRAM_API_KEY", "").strip()
    if not api_key:
        p.error("DEEPGRAM_API_KEY environment variable is required")

    try:
        host, port_str = ns.bind.rsplit(":", 1)
        port = int(port_str)
    except ValueError:
        p.error("--bind must be HOST:PORT")

    return Options(
        bind_host=host,
        bind_port=port,
        api_key=api_key,
        model=ns.model,
        language=ns.language,
        interim_results=not ns.no_interim,
        smart_format=not ns.no_smart_format,
        auth_token=ns.auth_token,
        log_level=ns.log_level,
    )


async def main(opts: Options) -> None:
    auth_state = "on" if opts.auth_token else "off"
    LOG.info(
        "listening on ws://%s:%d  (subprotocol=%s, auth=%s, model=%s, lang=%s, interim=%s)",
        opts.bind_host,
        opts.bind_port,
        SUBPROTOCOL,
        auth_state,
        opts.model,
        opts.language,
        opts.interim_results,
    )
    async with serve(
        lambda conn: handle(conn, opts),
        opts.bind_host,
        opts.bind_port,
        subprotocols=[SUBPROTOCOL],
        process_request=make_request_handler(opts),
        # 30 s ping/pong window matches SiphonAI's bridge.conn defaults.
        ping_interval=20,
        ping_timeout=10,
    ) as server:
        await server.serve_forever()


if __name__ == "__main__":
    opts = parse_args()
    logging.basicConfig(
        format="%(asctime)s %(levelname)s %(name)s: %(message)s",
        level=getattr(logging, opts.log_level),
    )
    try:
        asyncio.run(main(opts))
    except KeyboardInterrupt:
        pass
