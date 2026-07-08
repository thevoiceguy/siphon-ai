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
    # Test-harness knob: after `start`, wait this long and then send a
    # `hangup`. Lets the outbound SIPp scenario end the call from the
    # WS side (the callee just waits for our BYE). None = disabled.
    auto_hangup_after_ms: int | None
    # Test-harness knob: after `start`, send an ATTENDED transfer
    # naming this consult call id (`replaces_call_id`). Reuses
    # auto_transfer_delay_ms for the pause. None = disabled.
    auto_transfer_replaces: str | None
    # Test-harness knob: after `start`, wait auto_transfer_delay_ms and
    # then `conference_join` into this room. Lets two SIPp callers be
    # mixed in one room without a human driving the control channel.
    # None = disabled.
    auto_conference_join: str | None
    # Test-harness knob: after `start`, wait auto_transfer_delay_ms and
    # then `park` this call (optionally labelling the lot). SiphonAI
    # replies `stop { reason: "park" }` and closes the WS; an operator
    # retrieves the call later via the admin API. None = disabled.
    auto_park_slot: str | None
    # Sentinel distinguishing "--auto-park with no slot" (park, unlabeled)
    # from "--auto-park absent" (don't park). Set alongside auto_park_slot.
    auto_park: bool
    # Test-harness knob: after `start`, wait auto_transfer_delay_ms and then
    # `hold` this call, hold it for ~1s, `resume` it, then `hangup`. Drives
    # the bot-initiated hold/resume SIPp scenario (the caller asserts it
    # receives a sendonly re-INVITE then a sendrecv one). None = disabled.
    auto_hold: bool
    # Test-harness knob: drop the **first** connection's socket this many ms
    # after `start` (an unexpected WS drop), to exercise SiphonAI's WS
    # reconnect (0.7.3). With `[bridge].ws_reconnect_enabled = true` SiphonAI
    # re-dials; the redial's `start` carries `reconnected: true`, and this
    # server ends that resumed call with a `hangup`. `_dropped_once` (mutated
    # at runtime) makes the drop fire only on the first connection so the
    # redial succeeds. None = disabled.
    drop_after_ms: int | None
    _dropped_once: bool = False


# ─── HTTP-side concerns: auth + subprotocol ─────────────────────────────────


def make_request_handler(opts: Options):
    """Build a websockets ``process_request`` hook that enforces bearer auth.

    Returning a ``Response`` here aborts the upgrade with that response.
    Returning ``None`` lets the upgrade proceed.
    """

    def process_request(connection: ServerConnection, request: Request) -> Response | None:
        # Cheap healthcheck endpoint. We short-circuit before the
        # websockets handshake validation runs, so periodic
        # `curl http://.../healthz` from a Docker / k8s probe
        # doesn't show up as an `InvalidUpgrade` error in the log.
        if request.path == "/healthz":
            return connection.respond(200, "ok\n")

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

    # W3C trace context (PROTOCOL.md §2, 0.23.0): present on the upgrade
    # request when the daemon runs with [observability.otlp] enabled. A real
    # server would hand this to its OpenTelemetry SDK — e.g.
    #   ctx = TraceContextTextMapPropagator().extract(dict(request.headers))
    #   with tracer.start_as_current_span("handle-call", context=ctx): ...
    # — so its spans join the daemon's per-call trace in one waterfall. The
    # reference server has no OTel dependency, so it just logs the value.
    # (The same context is mirrored on `start.trace_context` below, for
    # stacks whose WS library hides upgrade headers.)
    if connection.request is not None:
        traceparent = connection.request.headers.get("traceparent")
        if traceparent:
            LOG.info("trace context: traceparent=%s", traceparent)

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
                if msg.get("trace_context"):
                    # Mirror of the `traceparent` upgrade header (PROTOCOL.md
                    # §3.1, 0.23.0) — either place works for continuing the
                    # daemon's per-call OTLP trace. Absent unless the daemon
                    # runs with [observability.otlp] enabled.
                    LOG.info(
                        "start call_id=%s trace_context=%s",
                        call_id,
                        msg["trace_context"],
                    )
                if msg.get("retrieved"):
                    # This session is picking up a previously parked call
                    # (PROTOCOL.md §3.1 / §4.9), not a fresh inbound one.
                    LOG.info("start call_id=%s is a retrieved (parked) call", call_id)
                if msg.get("reconnected"):
                    # SiphonAI re-dialed after an unexpected WS drop (0.7.3,
                    # PROTOCOL.md §5.7). End this resumed call so the harness
                    # caller completes — proving the reconnect recovered.
                    LOG.info("start call_id=%s is a reconnected (resumed) call", call_id)
                    if call_id:
                        asyncio.create_task(
                            _send_after(
                                connection, 300, {"type": "hangup", "call_id": call_id}
                            )
                        )
                elif opts.drop_after_ms is not None and not opts._dropped_once:
                    # First connection: drop the socket after a beat to
                    # simulate an unexpected WS failure mid-call.
                    opts._dropped_once = True
                    asyncio.create_task(_drop_after(connection, opts.drop_after_ms))
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

                if opts.auto_transfer_replaces and call_id:
                    # Test-harness behaviour: complete an attended
                    # transfer against a consult call the harness
                    # placed via POST /admin/v1/calls. No `target` —
                    # SiphonAI derives the Refer-To from the consult
                    # dialog's Contact (PROTOCOL.md §4.4).
                    asyncio.create_task(
                        _send_after(
                            connection,
                            opts.auto_transfer_delay_ms,
                            {
                                "type": "transfer",
                                "call_id": call_id,
                                "replaces_call_id": opts.auto_transfer_replaces,
                            },
                        )
                    )

                if opts.auto_conference_join and call_id:
                    # Test-harness behaviour: join a conference room so
                    # two SIPp callers land in the same mix. The delay
                    # lets the SIP side reach ESTABLISHED first.
                    asyncio.create_task(
                        _send_after(
                            connection,
                            opts.auto_transfer_delay_ms,
                            {
                                "type": "conference_join",
                                "call_id": call_id,
                                "room_id": opts.auto_conference_join,
                            },
                        )
                    )

                if opts.auto_park and call_id:
                    # Test-harness behaviour: park the call from the WS
                    # side. SiphonAI detaches this session (`stop{park}`
                    # + close); the call lives on hold music until an
                    # operator retrieves it. The hook for the 0.7.0
                    # park→retrieve SIPp scenario.
                    park_msg: dict[str, Any] = {"type": "park", "call_id": call_id}
                    if opts.auto_park_slot:
                        park_msg["slot"] = opts.auto_park_slot
                    asyncio.create_task(
                        _send_after(connection, opts.auto_transfer_delay_ms, park_msg)
                    )

                if opts.auto_hold and call_id:
                    # Test-harness behaviour: drive a full bot-initiated
                    # hold cycle — hold the caller, keep them held for ~1s,
                    # resume, then hang up. SiphonAI re-INVITEs the caller
                    # sendonly then sendrecv; the SIPp scenario asserts both
                    # and replies `held`/`resumed` come back. The hangup at
                    # the end lets the caller's scenario complete.
                    asyncio.create_task(_auto_hold_cycle(connection, call_id, opts))

                if opts.auto_hangup_after_ms is not None and call_id:
                    # Test-harness behaviour: end the call from the WS
                    # side after a beat. Used by the outbound SIPp
                    # scenario, where SIPp (the callee) answers and then
                    # waits for SiphonAI's BYE.
                    asyncio.create_task(
                        _send_after(
                            connection,
                            opts.auto_hangup_after_ms,
                            {"type": "hangup", "call_id": call_id},
                        )
                    )

            elif mtype == "stop":
                LOG.info("stop call_id=%s reason=%s", call_id, msg.get("reason"))
                # SiphonAI closes the connection right after `stop`; we
                # simply drop out of the loop.
                break

            elif mtype in {
                "speech_started",
                "speech_stopped",
                "dtmf",
                "mark",
                "hold",
                "resume",
                "held",
                "resumed",
                "error",
                "conference_joined",
                "conference_left",
                "participant_joined",
                "participant_left",
            }:
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


async def _drop_after(connection: ServerConnection, delay_ms: int) -> None:
    """Test-harness only: after `delay_ms`, abruptly close the socket to
    simulate an unexpected WS drop (no `stop`/`hangup`). Drives 0.7.3
    reconnect. Closes with a non-1000 code so it reads as a failure."""
    await asyncio.sleep(delay_ms / 1000.0)
    LOG.info("test-harness: dropping WS connection to trigger reconnect")
    try:
        await connection.close(code=1011, reason="harness drop")
    except websockets.exceptions.ConnectionClosed:
        pass


async def _auto_hold_cycle(
    connection: ServerConnection, call_id: str, opts: Options
) -> None:
    """Test-harness only: hold → wait → resume → hangup. Drives the
    bot-initiated hold/resume SIPp scenario. Swallows close errors."""
    try:
        await asyncio.sleep(opts.auto_transfer_delay_ms / 1000.0)
        await connection.send(json.dumps({"type": "hold", "call_id": call_id}))
        LOG.info("test-harness sent: hold")
        await asyncio.sleep(1.0)
        await connection.send(json.dumps({"type": "resume", "call_id": call_id}))
        LOG.info("test-harness sent: resume")
        # Give the resume re-INVITE a beat to complete, then end the call.
        await asyncio.sleep(0.5)
        await connection.send(json.dumps({"type": "hangup", "call_id": call_id}))
        LOG.info("test-harness sent: hangup")
    except websockets.exceptions.ConnectionClosed:
        LOG.debug("auto-hold dropped: connection closed mid-cycle")


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
        "--auto-transfer-replaces",
        default=None,
        metavar="CALL_ID",
        help=(
            "test-harness only: after `start`, emit an attended "
            "`transfer` with this replaces_call_id (the consult "
            "call's bridge id). Uses --auto-transfer-delay-ms. See "
            "test-harness/sipp-scenarios/attended_transfer_a.xml."
        ),
    )
    p.add_argument(
        "--auto-hangup-after-ms",
        type=int,
        default=None,
        metavar="MS",
        help=(
            "test-harness only: after `start`, emit a `hangup` after "
            "this many ms. See test-harness/sipp-scenarios/"
            "outbound_uas_answer.xml."
        ),
    )
    p.add_argument(
        "--auto-conference-join",
        default=None,
        metavar="ROOM",
        help=(
            "test-harness only: after `start`, emit a `conference_join` "
            "into this room (uses --auto-transfer-delay-ms for the "
            "pause). Two callers pointed at the same room get mixed — "
            "the hook for the 0.7.0 two-caller conference SIPp scenario."
        ),
    )
    p.add_argument(
        "--auto-park",
        nargs="?",
        const="",
        default=None,
        metavar="SLOT",
        help=(
            "test-harness only: after `start`, emit a `park` (uses "
            "--auto-transfer-delay-ms for the pause). Optional value is "
            "the hold-lot label. SiphonAI replies `stop{park}` and "
            "closes the WS — the hook for the 0.7.0 park→retrieve SIPp "
            "scenario."
        ),
    )
    p.add_argument(
        "--auto-hold",
        action="store_true",
        help=(
            "test-harness only: after `start`, run a full bot-initiated "
            "hold cycle — `hold` (uses --auto-transfer-delay-ms for the "
            "pause), hold ~1s, `resume`, then `hangup`. SiphonAI re-INVITEs "
            "the caller sendonly then sendrecv — the hook for the 0.7.2 "
            "bot-hold SIPp scenario."
        ),
    )
    p.add_argument(
        "--drop-after-ms",
        type=int,
        default=None,
        help=(
            "test-harness only: drop the first connection's socket this many "
            "ms after `start` (an unexpected WS drop). With "
            "[bridge].ws_reconnect_enabled SiphonAI re-dials; the redial's "
            "start carries reconnected:true and this server hangs it up. The "
            "hook for the 0.7.3 WS-reconnect SIPp phase."
        ),
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
        auto_hangup_after_ms=args.auto_hangup_after_ms,
        auto_transfer_replaces=args.auto_transfer_replaces,
        auto_conference_join=args.auto_conference_join,
        auto_park=args.auto_park is not None,
        auto_park_slot=args.auto_park or None,
        auto_hold=args.auto_hold,
        drop_after_ms=args.drop_after_ms,
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
