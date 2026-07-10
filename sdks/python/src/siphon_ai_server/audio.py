"""Outbound audio framing: arbitrary PCM byte pushes → exact 20 ms frames.

SiphonAI requires every binary WS frame to be exactly one 20 ms PCM16-LE
mono chunk (320 bytes @ 8 kHz, 640 @ 16 kHz) — the piece every integrator
hand-rolls. :class:`AudioSender` buffers whatever a TTS engine produces and
emits spec-sized frames, **paced at real time** (one frame per 20 ms) so a
fast synthesizer can't flood the daemon's bounded playout queue and blunt
barge-in `clear`.
"""

from __future__ import annotations

import asyncio
from typing import Awaitable, Callable

FRAME_MS = 20
SUPPORTED_RATES = (8000, 16000)


def frame_bytes(sample_rate: int) -> int:
    """Bytes per 20 ms PCM16-LE mono frame at `sample_rate`."""
    if sample_rate not in SUPPORTED_RATES:
        raise ValueError(f"unsupported sample rate {sample_rate} (8000 or 16000)")
    return sample_rate // 1000 * FRAME_MS * 2


class AudioSender:
    """Paced 20 ms re-framer over a raw ``send(bytes)`` coroutine.

    ``push()`` never blocks on the wire; a background task drains the
    buffer one frame per 20 ms. ``clear()`` drops everything buffered
    (barge-in); ``flush()`` waits until the buffer has fully played out,
    zero-padding the final partial frame so the tail is audible.
    """

    def __init__(
        self,
        send: Callable[[bytes], Awaitable[None]],
        sample_rate: int,
    ) -> None:
        self._send = send
        self._frame_bytes = frame_bytes(sample_rate)
        self._buffer = bytearray()
        self._wakeup = asyncio.Event()
        self._closed = False
        self._task: asyncio.Task[None] | None = None

    def push(self, pcm: bytes) -> None:
        """Queue PCM16-LE mono bytes of any length for paced sending."""
        if self._closed:
            return
        self._buffer.extend(pcm)
        self._wakeup.set()
        if self._task is None:
            self._task = asyncio.get_running_loop().create_task(self._run())

    def clear(self) -> int:
        """Drop all buffered audio (local half of barge-in). Returns the
        number of bytes dropped — pair with sending the daemon a
        ``clear`` to flush its side too."""
        dropped = len(self._buffer)
        self._buffer.clear()
        return dropped

    async def flush(self) -> None:
        """Wait until everything pushed so far has been sent. The final
        partial frame (if any) is zero-padded to spec size."""
        if self._buffer:
            pad = -len(self._buffer) % self._frame_bytes
            self._buffer.extend(b"\x00" * pad)
            self._wakeup.set()
        while self._buffer and not self._closed:
            await asyncio.sleep(FRAME_MS / 1000)

    async def aclose(self) -> None:
        self._closed = True
        self._buffer.clear()
        if self._task is not None:
            self._task.cancel()
            try:
                await self._task
            except asyncio.CancelledError:
                pass
            self._task = None

    async def _run(self) -> None:
        loop = asyncio.get_running_loop()
        next_at = loop.time()
        while not self._closed:
            if len(self._buffer) < self._frame_bytes:
                self._wakeup.clear()
                # Idle: no whole frame ready — wait for more audio and
                # re-anchor the clock (no "catch-up burst" after silence).
                await self._wakeup.wait()
                next_at = loop.time()
                continue
            frame = bytes(self._buffer[: self._frame_bytes])
            del self._buffer[: self._frame_bytes]
            try:
                await self._send(frame)
            except Exception:
                # The connection owns error reporting; a dead socket just
                # stops the pacer.
                self._closed = True
                return
            next_at += FRAME_MS / 1000
            delay = next_at - loop.time()
            if delay > 0:
                await asyncio.sleep(delay)
            else:
                # Fell behind (slow event loop) — re-anchor rather than
                # bursting frames to catch up.
                next_at = loop.time()
