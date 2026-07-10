#!/usr/bin/env python3
"""
End-to-end smoke test for the Python echo WS server.

Spins up the server in-process, opens a client, drives a tiny call:
``start`` -> 5 binary audio frames -> assert each comes back -> ``stop``
-> clean close. Also exercises the auth path and the ``--echo-marks``
flag.

Run:
    python3 -m unittest test_smoke.py -v
"""

from __future__ import annotations

import asyncio
import contextlib
import json
import logging
import os
import unittest

import websockets
from websockets.asyncio.client import connect

import server as srv

logging.basicConfig(level=os.environ.get("LOG_LEVEL", "WARNING"))


def _start_msg(call_id: str = "smoke-1", sample_rate: int = 8000) -> dict:
    """A canonical PROTOCOL.md §3.1 `start` message."""
    return {
        "type": "start",
        "version": "1",
        "call_id": call_id,
        "seq": 0,
        "from": "+13125551212",
        "to": "5000",
        "direction": "inbound",
        "audio": {
            "encoding": "pcm16le",
            "sample_rate": sample_rate,
            "channels": 1,
            "frame_ms": 20,
        },
        "sip": {
            "call_id": "abc@pbx.example.com",
            "headers": {"User-Agent": "smoke-test"},
        },
    }


@contextlib.asynccontextmanager
async def running_server(opts: srv.Options):
    """Start the echo server on a background task; cancel it on exit."""

    sdk_server = srv.SiphonServer(
        lambda call: srv.handle(call, opts),
        host=opts.bind_host,
        port=opts.bind_port,
        auth_token=opts.auth_token,
        ping_interval=None,  # disable pings in tests; we want deterministic IO
        ping_timeout=None,
    )
    async with sdk_server.listen() as server:
        yield server


def _opts(**overrides) -> srv.Options:
    base = dict(
        bind_host="127.0.0.1",
        bind_port=0,  # OS-assigned; we read it back below
        delay_ms=0,
        auth_token=None,
        echo_marks=False,
        log_level="WARNING",
        # Test-harness auto-* knobs all default off; mirrors parse_args
        # defaults so the dataclass (which has no field defaults) is
        # constructible here as fields are added across releases.
        auto_transfer_target=None,
        auto_transfer_delay_ms=200,
        auto_hangup_after_ms=None,
        auto_transfer_replaces=None,
        auto_conference_join=None,
        auto_park=False,
        auto_park_slot=None,
        auto_hold=False,
        drop_after_ms=None,
    )
    base.update(overrides)
    return srv.Options(**base)


async def _bound_port(server) -> int:
    """Return the port the websockets server is actually listening on."""
    sock = next(iter(server.sockets))
    return sock.getsockname()[1]


# ─── Tests ─────────────────────────────────────────────────────────────────


class EchoServerSmokeTest(unittest.IsolatedAsyncioTestCase):

    async def test_start_then_audio_round_trip_then_stop(self):
        opts = _opts()
        async with running_server(opts) as server:
            port = await _bound_port(server)
            url = f"ws://127.0.0.1:{port}"

            async with connect(url, subprotocols=[srv.SUBPROTOCOL]) as ws:
                self.assertEqual(ws.subprotocol, srv.SUBPROTOCOL)
                await ws.send(json.dumps(_start_msg()))

                # Five 8-kHz / 20-ms PCM16 frames (320 bytes each).
                frames = [bytes([i, 0] * 160) for i in range(1, 6)]
                for f in frames:
                    await ws.send(f)

                received: list[bytes] = []
                while len(received) < len(frames):
                    msg = await asyncio.wait_for(ws.recv(), timeout=1.0)
                    self.assertIsInstance(msg, bytes, "echo must return binary frames")
                    received.append(msg)
                self.assertEqual(received, frames, "echo content must match input")

                await ws.send(json.dumps({
                    "type": "stop",
                    "call_id": "smoke-1",
                    "seq": 200,
                    "reason": "caller_hangup",
                }))
                # Server breaks out of its loop and lets us close.
                await ws.close()

    async def test_echo_marks_emits_mark_after_start(self):
        opts = _opts(echo_marks=True)
        async with running_server(opts) as server:
            port = await _bound_port(server)
            async with connect(
                f"ws://127.0.0.1:{port}", subprotocols=[srv.SUBPROTOCOL]
            ) as ws:
                await ws.send(json.dumps(_start_msg()))
                msg = await asyncio.wait_for(ws.recv(), timeout=1.0)
                self.assertIsInstance(msg, str, "expected text frame")
                payload = json.loads(msg)
                self.assertEqual(payload["type"], "mark")
                self.assertEqual(payload["call_id"], "smoke-1")
                self.assertEqual(payload["name"], "echo_ready")
                await ws.close()

    async def test_unsupported_version_closes_with_1003(self):
        opts = _opts()
        async with running_server(opts) as server:
            port = await _bound_port(server)
            async with connect(
                f"ws://127.0.0.1:{port}", subprotocols=[srv.SUBPROTOCOL]
            ) as ws:
                bad = _start_msg()
                bad["version"] = "99"
                await ws.send(json.dumps(bad))
                # The server closes the connection. Recv raises with the
                # close code.
                with self.assertRaises(websockets.exceptions.ConnectionClosed) as cm:
                    await asyncio.wait_for(ws.recv(), timeout=1.0)
                self.assertEqual(cm.exception.rcvd.code, 1003)

    async def test_auth_token_required_when_configured(self):
        opts = _opts(auth_token="s3cret")
        async with running_server(opts) as server:
            port = await _bound_port(server)

            # Wrong token → 401 on the upgrade.
            with self.assertRaises(websockets.exceptions.InvalidStatus) as cm:
                async with connect(
                    f"ws://127.0.0.1:{port}",
                    subprotocols=[srv.SUBPROTOCOL],
                    additional_headers={"Authorization": "Bearer wrong"},
                ):
                    pass
            self.assertEqual(cm.exception.response.status_code, 401)

            # Correct token → succeeds.
            async with connect(
                f"ws://127.0.0.1:{port}",
                subprotocols=[srv.SUBPROTOCOL],
                additional_headers={"Authorization": "Bearer s3cret"},
            ) as ws:
                await ws.send(json.dumps(_start_msg()))
                # Send & receive one frame to prove the connection is live.
                await ws.send(b"\x01\x00" * 160)
                echoed = await asyncio.wait_for(ws.recv(), timeout=1.0)
                self.assertEqual(echoed, b"\x01\x00" * 160)
                await ws.close()

    async def test_invalid_json_is_ignored_not_fatal(self):
        opts = _opts()
        async with running_server(opts) as server:
            port = await _bound_port(server)
            async with connect(
                f"ws://127.0.0.1:{port}", subprotocols=[srv.SUBPROTOCOL]
            ) as ws:
                await ws.send(json.dumps(_start_msg()))
                await ws.send("this is not json")
                # Echo path must still work after the bad text frame.
                payload = b"\x02\x00" * 160
                await ws.send(payload)
                echoed = await asyncio.wait_for(ws.recv(), timeout=1.0)
                self.assertEqual(echoed, payload)
                await ws.close()


if __name__ == "__main__":
    unittest.main()
