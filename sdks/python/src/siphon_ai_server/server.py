"""The accept loop: WebSocket server speaking `siphon-ai.v1`."""

from __future__ import annotations

import asyncio
import http
import logging
from typing import Awaitable, Callable

from websockets.asyncio.server import Request, Response, ServerConnection, serve
from websockets.exceptions import ConnectionClosed

from .call import Call
from .events import Start, parse_event

__all__ = ["SiphonServer", "SUBPROTOCOL"]

SUBPROTOCOL = "siphon-ai.v1"

# The daemon sends `start` immediately after the upgrade; a socket that
# never does isn't a SiphonAI bridge.
START_DEADLINE_SECS = 10.0

logger = logging.getLogger("siphon_ai_server")

CallHandler = Callable[[Call], Awaitable[None]]


class SiphonServer:
    """Accepts SiphonAI bridge connections and hands each to a handler.

    ```python
    server = SiphonServer(handler, host="0.0.0.0", port=8080)
    await server.serve_forever()
    ```

    The handler receives a :class:`Call` whose `start` has already been
    parsed. Handler exceptions are logged and close that call only.

    `auth_token`: when set, upgrade requests must carry
    `Authorization: Bearer <token>` (rejected with 401 otherwise) —
    matching the daemon's `[bridge].auth_bearer`.
    """

    def __init__(
        self,
        handler: CallHandler,
        *,
        host: str = "0.0.0.0",
        port: int = 8080,
        auth_token: str | None = None,
        ping_interval: float | None = 15.0,
        ping_timeout: float | None = 10.0,
    ) -> None:
        self._handler = handler
        self._host = host
        self._port = port
        self._auth_token = auth_token
        self._ping_interval = ping_interval
        self._ping_timeout = ping_timeout

    async def serve_forever(self) -> None:
        async with self.listen():
            await asyncio.get_running_loop().create_future()  # run until cancelled

    def listen(self):
        """The underlying ``websockets.serve`` context manager, for callers
        that manage their own lifetime."""
        return serve(
            self._connection,
            self._host,
            self._port,
            subprotocols=[SUBPROTOCOL],
            process_request=self._process_request,
            max_size=256 * 1024,  # PROTOCOL.md §2 text-frame cap
            ping_interval=self._ping_interval,
            ping_timeout=self._ping_timeout,
        )

    def _process_request(
        self, connection: ServerConnection, request: Request
    ) -> Response | None:
        # Container/k8s-probe-friendly healthcheck, short-circuited before
        # WS handshake validation so probes don't log as upgrade errors.
        if request.path == "/healthz":
            return connection.respond(http.HTTPStatus.OK, "ok\n")
        if self._auth_token is None:
            return None
        expected = f"Bearer {self._auth_token}"
        if request.headers.get("Authorization") != expected:
            return connection.respond(http.HTTPStatus.UNAUTHORIZED, "unauthorized\n")
        return None

    async def _connection(self, ws: ServerConnection) -> None:
        call_id = ws.request.headers.get("X-Siphon-Call-Id", "?") if ws.request else "?"
        try:
            first = await asyncio.wait_for(ws.recv(), timeout=START_DEADLINE_SECS)
        except (asyncio.TimeoutError, ConnectionClosed):
            logger.warning("connection %s closed before `start`", call_id)
            await ws.close()
            return
        if isinstance(first, bytes):
            logger.warning("connection %s sent audio before `start`; closing", call_id)
            await ws.close(code=1002, reason="expected start")
            return
        event = parse_event(first)
        if isinstance(event, Start) and event.version != "1":
            logger.error(
                "connection %s: unsupported protocol version %r; closing",
                call_id,
                event.version,
            )
            await ws.close(code=1003, reason="unsupported version")
            return
        if not isinstance(event, Start):
            logger.warning(
                "connection %s first message was `%s`, not `start`; closing",
                call_id,
                event.type,
            )
            await ws.close(code=1002, reason="expected start")
            return

        call = Call(ws, event)
        logger.info(
            "call %s: %s -> %s (%d Hz%s)",
            event.call_id,
            event.from_,
            event.to,
            event.audio.sample_rate,
            ", reconnected" if event.reconnected else "",
        )
        try:
            await self._handler(call)
        except ConnectionClosed:
            pass
        except Exception:
            logger.exception("handler failed for call %s", event.call_id)
        finally:
            await call.audio_out.aclose()
            await ws.close()
            logger.info("call %s: session ended", event.call_id)
