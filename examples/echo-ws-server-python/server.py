#!/usr/bin/env python3
"""
Reference echo WebSocket server for the SiphonAI bridge protocol v1.

What it does
------------
- Accepts a SiphonAI WebSocket upgrade (subprotocol ``siphon-ai.v1``).
- Reads the ``start`` text message and logs the call's audio format.
- Echoes every binary audio frame back into the same connection.
- Logs incoming text messages (DTMF, marks, speech events).
- Cleans up on ``stop`` or when SiphonAI closes the connection.

What it does NOT do
-------------------
- Run any AI logic. This is the reference for the *transport* — building a
  real assistant on top of it is the developer's job.
- Send ``hangup``, ``transfer``, ``mark``, or ``send_dtmf``. (See the
  ``--echo-marks`` flag for a tiny exception used in protocol tests.)

Run
---
    python3 server.py --bind 0.0.0.0:8080

Wire format
-----------
See ``docs/PROTOCOL.md`` in the SiphonAI repo. This file is intentionally
the simplest possible compliant server.
"""

from __future__ import annotations

import argparse
import asyncio
import json
import logging
import signal
from dataclasses import dataclass
from typing import Any

import websockets
from websockets.asyncio.server import ServerConnection, serve
from websockets.http11 import Request, Response

LOG = logging.getLogger("echo-ws")
SUBPROTOCOL = "siphon-ai.v1"


@dataclass
class Options:
    bind_host: str
    bind_port: int
    delay_ms: int
    auth_token: str | None
    echo_marks: bool
    log_level: str
    # Test-harness knob: after the `start` message, wait this long and
    # then send a `transfer` message with `target=auto_transfer_target`.
    # Disabled when either field is None — keeps the reference server
    # silent on the control channel by default (see file docstring).
    auto_transfer_target: str | None
    auto_transfer_delay_ms: int


# ─── HTTP-side concerns: auth + subprotocol ─────────────────────────────────


def make_request_handler(opts: Options):
    """Build a websockets ``process_request`` hook that enforces bearer auth.

    Returning a ``Response`` here aborts the upgrade with that response.
    Returning ``None`` lets the upgrade proceed.
    """

    def process_request(connection: ServerConnection, request: Request) -> Response | None:
        if opts.auth_token is None:
            return None
        header = request.headers.get("Authorization", "")
        expected = f"Bearer {opts.auth_token}"
        if header != expected:
            LOG.warning("rejecting upgrade: bad/missing Authorization header")
            return connection.respond(401, "Unauthorized\n")
        return None

    return process_request


# ─── Connection handler ─────────────────────────────────────────────────────


async def handle(connection: ServerConnection, opts: Options) -> None:
    peer = connection.remote_address
    call_id: str | None = None
    frames_echoed = 0
    bytes_echoed = 0

    LOG.info("connect peer=%s subprotocol=%r", peer, connection.subprotocol)

    if connection.subprotocol != SUBPROTOCOL:
        LOG.warning("client did not negotiate %r; proceeding optimistically", SUBPROTOCOL)

    try:
        async for message in connection:
            if isinstance(message, bytes):
                # Binary = audio. Echo it back unchanged.
                if opts.delay_ms > 0:
                    await asyncio.sleep(opts.delay_ms / 1000.0)
                await connection.send(message)
                frames_echoed += 1
                bytes_echoed += len(message)
                if frames_echoed % 50 == 0:
                    # Log once a second at 50 fps so the operator sees life
                    # without being drowned in per-frame chatter.
                    LOG.debug(
                        "echoed %d frames / %d bytes (call_id=%s)",
                        frames_echoed,
                        bytes_echoed,
                        call_id,
                    )
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
                version = msg.get("version")
                audio = msg.get("audio", {})
                sip = msg.get("sip", {})
                LOG.info(
                    "start call_id=%s version=%s from=%s to=%s "
                    "rate=%s ch=%s frame_ms=%s sip_call_id=%s",
                    call_id,
                    version,
                    msg.get("from"),
                    msg.get("to"),
                    audio.get("sample_rate"),
                    audio.get("channels"),
                    audio.get("frame_ms"),
                    sip.get("call_id"),
                )
                if version != "1":
                    LOG.error("unsupported protocol version %r; closing", version)
                    await connection.close(code=1003, reason="unsupported version")
                    return
                if opts.echo_marks:
                    # Optional behavior used by SiphonAI's protocol smoke
                    # tests: round-trip a `mark` to verify the control
                    # path. Disabled by default; the reference echo
                    # server is supposed to be silent on the control
                    # channel.
                    await connection.send(
                        json.dumps({
                            "type": "mark",
                            "call_id": call_id,
                            "name": "echo_ready",
                        })
                    )

                if opts.auto_transfer_target and call_id:
                    # Test-harness behaviour: drive a blind transfer
                    # without a human in the loop. The delay lets the
                    # SIP side reach ESTABLISHED before REFER lands.
                    target = opts.auto_transfer_target
                    delay_ms = opts.auto_transfer_delay_ms
                    cid = call_id
                    asyncio.create_task(
                        _send_after(
                            connection,
                            delay_ms,
                            {"type": "transfer", "call_id": cid, "target": target},
                        )
                    )

            elif mtype == "stop":
                LOG.info("stop call_id=%s reason=%s", call_id, msg.get("reason"))
                # SiphonAI closes the connection right after `stop`; we
                # simply drop out of the loop.
                break

            elif mtype in {"speech_started", "speech_stopped", "dtmf", "mark", "error"}:
                LOG.info("%s: %s", mtype, _redact(msg))

            else:
                LOG.warning("unknown text message type=%r", mtype)

    except websockets.exceptions.ConnectionClosedError as e:
        LOG.info("connection closed unexpectedly: %s", e)
    except websockets.exceptions.ConnectionClosedOK:
        LOG.info("connection closed cleanly")
    finally:
        LOG.info(
            "done peer=%s call_id=%s frames_echoed=%d bytes_echoed=%d",
            peer,
            call_id,
            frames_echoed,
            bytes_echoed,
        )


def _redact(msg: dict[str, Any]) -> dict[str, Any]:
    """Strip noisy fields when logging known control messages."""
    return {k: v for k, v in msg.items() if k != "type"}


async def _send_after(
    connection: ServerConnection, delay_ms: int, payload: dict[str, Any]
) -> None:
    """Sleep, then send a JSON payload. Swallows close errors so a
    race with WS teardown doesn't crash the task."""
    await asyncio.sleep(delay_ms / 1000.0)
    try:
        await connection.send(json.dumps(payload))
        LOG.info("test-harness sent: %s", _redact(payload))
    except websockets.exceptions.ConnectionClosed:
        LOG.debug("auto-send dropped: connection closed before delay elapsed")


# ─── Entrypoint ──────────────────────────────────────────────────────────────


def parse_args(argv: list[str] | None = None) -> Options:
    p = argparse.ArgumentParser(
        description="Reference echo WS server for the SiphonAI bridge protocol v1.",
    )
    p.add_argument(
        "--bind",
        default="0.0.0.0:8080",
        metavar="HOST:PORT",
        help="address to listen on (default: 0.0.0.0:8080)",
    )
    p.add_argument(
        "--delay-ms",
        type=int,
        default=0,
        help="echo each audio frame back after this many ms (default: 0)",
    )
    p.add_argument(
        "--auth-token",
        default=None,
        help="if set, require Authorization: Bearer <token> on the upgrade request",
    )
    p.add_argument(
        "--echo-marks",
        action="store_true",
        help="send a `mark` event back after `start` (used by protocol smoke tests)",
    )
    p.add_argument(
        "--auto-transfer-target",
        default=None,
        metavar="SIP_URI",
        help=(
            "test-harness only: after `start`, emit a `transfer` "
            "message with this target SIP URI. See "
            "test-harness/sipp-scenarios/blind_transfer.xml."
        ),
    )
    p.add_argument(
        "--auto-transfer-delay-ms",
        type=int,
        default=200,
        help="ms to wait after `start` before emitting the transfer (default: 200)",
    )
    p.add_argument(
        "--log-level",
        default="INFO",
        choices=["DEBUG", "INFO", "WARNING", "ERROR"],
    )
    args = p.parse_args(argv)

    host, _, port = args.bind.partition(":")
    if not port:
        p.error("--bind must be HOST:PORT")
    return Options(
        bind_host=host,
        bind_port=int(port),
        delay_ms=args.delay_ms,
        auth_token=args.auth_token,
        echo_marks=args.echo_marks,
        log_level=args.log_level,
        auto_transfer_target=args.auto_transfer_target,
        auto_transfer_delay_ms=args.auto_transfer_delay_ms,
    )


async def main(opts: Options) -> None:
    async def handler(connection: ServerConnection) -> None:
        await handle(connection, opts)

    async with serve(
        handler,
        host=opts.bind_host,
        port=opts.bind_port,
        subprotocols=[SUBPROTOCOL],
        process_request=make_request_handler(opts),
        # 256 KiB matches PROTOCOL.md §2.1.
        max_size=256 * 1024,
        ping_interval=15,
        ping_timeout=10,
    ) as server:
        LOG.info(
            "listening on ws://%s:%d  (subprotocol=%s, auth=%s, delay_ms=%d)",
            opts.bind_host,
            opts.bind_port,
            SUBPROTOCOL,
            "on" if opts.auth_token else "off",
            opts.delay_ms,
        )

        # Wait for SIGINT / SIGTERM.
        loop = asyncio.get_running_loop()
        stop = loop.create_future()
        for sig in (signal.SIGINT, signal.SIGTERM):
            loop.add_signal_handler(sig, lambda s=sig: stop.set_result(s))
        try:
            sig = await stop
            LOG.info("received signal %s, shutting down", sig.name if hasattr(sig, "name") else sig)
        finally:
            server.close()
            await server.wait_closed()


if __name__ == "__main__":
    options = parse_args()
    logging.basicConfig(
        level=getattr(logging, options.log_level),
        format="%(asctime)s %(levelname)s %(name)s: %(message)s",
    )
    try:
        asyncio.run(main(options))
    except KeyboardInterrupt:
        pass
