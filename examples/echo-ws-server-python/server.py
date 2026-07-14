#!/usr/bin/env python3
"""Reference echo WS server for the SiphonAI bridge protocol v1.

Echoes every audio frame back to the caller — point a softphone at
SiphonAI and you hear yourself. Built on the **`siphon-ai-server` SDK**
(`sdks/python/`), so this file is the canonical example of writing a
SiphonAI bot server with typed events instead of hand-rolled wire code.
(It is also the SIPp CI fixture: every daemon PR drives real calls
through this server — and therefore through the SDK.)

By default the server is silent on the control channel: it never sends
commands unless one of the `--auto-*` test-harness flags asks it to.

Run:
    python3 server.py --bind 0.0.0.0:8080

The SDK is imported from the repo checkout automatically; installing it
(`pip install ./sdks/python` from the repo root) also works.
"""

from __future__ import annotations

import argparse
import asyncio
import logging
import os
import signal
import sys
from dataclasses import dataclass
from pathlib import Path

# Prefer an installed siphon-ai-server; fall back to the in-repo SDK so a
# bare `python3 server.py` works from a checkout with no pip step.
try:
    import siphon_ai_server  # noqa: F401
except ImportError:  # pragma: no cover
    sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "sdks" / "python" / "src"))

from siphon_ai_server import (  # noqa: E402
    AudioFrame,
    BargeInResolved,
    Call,
    SiphonServer,
    SpeechStarted,
    Start,
    Stop,
    UnknownEvent,
)

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
    # Test-harness knobs (see --help): each drives one SIPp scenario.
    auto_transfer_target: str | None
    auto_transfer_delay_ms: int
    auto_hangup_after_ms: int | None
    auto_transfer_replaces: str | None
    auto_conference_join: str | None
    auto_park: bool
    auto_park_slot: str | None
    auto_hold: bool
    # Drop the FIRST connection's socket this many ms after `start` (an
    # unexpected WS drop) to exercise 0.7.3 reconnect; the redial's
    # `start` carries reconnected:true and gets hung up instead.
    drop_after_ms: int | None
    _dropped_once: bool = False


async def handle(call: Call, opts: Options) -> None:
    start = call.start
    LOG.info(
        "start call_id=%s version=%s from=%s to=%s rate=%s ch=%s frame_ms=%s sip_call_id=%s",
        start.call_id,
        start.version,
        start.from_,
        start.to,
        start.audio.sample_rate,
        start.audio.channels,
        start.audio.frame_ms,
        start.sip.call_id,
    )
    if start.trace_context:
        # Mirror of the `traceparent` upgrade header (PROTOCOL.md §3.1) —
        # a real server would hand this to its OpenTelemetry SDK so its
        # spans join the daemon's per-call trace.
        LOG.info(
            "start call_id=%s trace_context=%s", start.call_id, start.trace_context
        )
    if start.retrieved:
        LOG.info("start call_id=%s is a retrieved (parked) call", start.call_id)

    if start.reconnected:
        # SiphonAI re-dialed after an unexpected WS drop (0.7.3). End the
        # resumed call so the harness caller completes — proving the
        # reconnect recovered.
        LOG.info("start call_id=%s is a reconnected (resumed) call", start.call_id)
        asyncio.get_running_loop().create_task(_send_after(300, call.hangup()))
    elif opts.drop_after_ms is not None and not opts._dropped_once:
        opts._dropped_once = True
        asyncio.get_running_loop().create_task(_drop_after(call, opts.drop_after_ms))

    if opts.echo_marks:
        # Used by protocol smoke tests to verify the control round-trip.
        await call.mark("echo_ready")

    # ── test-harness one-shots (each drives one SIPp scenario) ──
    loop = asyncio.get_running_loop()
    if opts.auto_transfer_target:
        loop.create_task(
            _harness(
                _send_after(
                    opts.auto_transfer_delay_ms,
                    call.transfer(opts.auto_transfer_target),
                ),
                "transfer",
            )
        )
    if opts.auto_transfer_replaces:
        loop.create_task(
            _harness(
                _send_after(
                    opts.auto_transfer_delay_ms,
                    call.transfer(replaces_call_id=opts.auto_transfer_replaces),
                ),
                "attended transfer",
            )
        )
    if opts.auto_conference_join:
        loop.create_task(
            _harness(
                _send_after(
                    opts.auto_transfer_delay_ms,
                    call.conference_join(opts.auto_conference_join),
                ),
                "conference_join",
            )
        )
    if opts.auto_park:
        loop.create_task(
            _harness(
                _send_after(
                    opts.auto_transfer_delay_ms, call.park(opts.auto_park_slot)
                ),
                "park",
            )
        )
    if opts.auto_hold:
        loop.create_task(_auto_hold_cycle(call, opts))
    if opts.auto_hangup_after_ms is not None:
        loop.create_task(
            _harness(_send_after(opts.auto_hangup_after_ms, call.hangup()), "hangup")
        )

    # ── the echo loop ──
    frames_echoed = 0
    bytes_echoed = 0
    async for item in call:
        if isinstance(item, AudioFrame):
            if opts.delay_ms > 0:
                await asyncio.sleep(opts.delay_ms / 1000.0)
            # Echo 1:1, mirroring the daemon's own 20 ms cadence — no
            # re-pacing needed (for generated audio use call.send_audio,
            # the SDK's paced re-framer).
            await call.send_audio_frame(item.pcm)
            frames_echoed += 1
            bytes_echoed += len(item.pcm)
            if frames_echoed % 50 == 0:
                LOG.debug(
                    "echoed %d frames / %d bytes (call_id=%s)",
                    frames_echoed,
                    bytes_echoed,
                    start.call_id,
                )
        elif isinstance(item, SpeechStarted) and item.decision_pending:
            # Pause-mode barge-in arbitration (0.32.0). An echo server
            # never wants to stop echoing, so reject the barge-in —
            # unless the harness asks for a confirm via
            # SIPHON_ECHO_BARGE_IN_VERDICT=confirm.
            verdict = os.environ.get("SIPHON_ECHO_BARGE_IN_VERDICT", "reject")
            if verdict == "confirm":
                await call.barge_in_confirm()
            else:
                await call.barge_in_reject()
            LOG.info(
                "speech_started call_id=%s decision_pending deadline_ms=%s verdict=%s",
                start.call_id,
                item.decision_deadline_ms,
                verdict,
            )
        elif isinstance(item, BargeInResolved):
            LOG.info(
                "barge_in_resolved call_id=%s outcome=%s",
                start.call_id,
                item.outcome,
            )
        elif isinstance(item, Stop):
            LOG.info("stop call_id=%s reason=%s", start.call_id, item.reason)
            break
        elif isinstance(item, UnknownEvent):
            LOG.warning("unknown text message type=%r", item.type)
        else:
            LOG.info("%s: %s", item.type, item)

    LOG.info(
        "done call_id=%s frames_echoed=%d bytes_echoed=%d",
        start.call_id,
        frames_echoed,
        bytes_echoed,
    )


async def _send_after(delay_ms: int, command) -> None:
    await asyncio.sleep(delay_ms / 1000.0)
    await command


async def _harness(coro, label: str) -> None:
    """Run a delayed test-harness command; a race with WS teardown is
    logged, never fatal (Call commands already swallow closed sockets)."""
    try:
        await coro
        LOG.info("test-harness sent: %s", label)
    except Exception as e:  # pragma: no cover
        LOG.debug("test-harness %s dropped: %s", label, e)


async def _drop_after(call: Call, delay_ms: int) -> None:
    """Abruptly close the socket (no hangup) to trigger 0.7.3 reconnect."""
    await asyncio.sleep(delay_ms / 1000.0)
    LOG.info("test-harness: dropping WS connection to trigger reconnect")
    await call.abort()


async def _auto_hold_cycle(call: Call, opts: Options) -> None:
    """hold → ~1 s → resume → hangup, driving the 0.7.2 SIPp scenario."""
    try:
        await asyncio.sleep(opts.auto_transfer_delay_ms / 1000.0)
        await call.hold()
        LOG.info("test-harness sent: hold")
        await asyncio.sleep(1.0)
        await call.resume()
        LOG.info("test-harness sent: resume")
        await asyncio.sleep(0.5)
        await call.hangup()
        LOG.info("test-harness sent: hangup")
    except Exception as e:  # pragma: no cover
        LOG.debug("auto-hold dropped: %s", e)


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
    server = SiphonServer(
        lambda call: handle(call, opts),
        host=opts.bind_host,
        port=opts.bind_port,
        auth_token=opts.auth_token,
    )
    async with server.listen():
        LOG.info(
            "listening on ws://%s:%d  (subprotocol=%s, auth=%s, delay_ms=%d)",
            opts.bind_host,
            opts.bind_port,
            SUBPROTOCOL,
            "on" if opts.auth_token else "off",
            opts.delay_ms,
        )
        loop = asyncio.get_running_loop()
        stop = loop.create_future()
        for sig in (signal.SIGINT, signal.SIGTERM):
            loop.add_signal_handler(sig, lambda s=sig: stop.set_result(s))
        sig = await stop
        LOG.info("received signal %s, shutting down", getattr(sig, "name", sig))


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
