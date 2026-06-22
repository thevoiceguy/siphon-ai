#!/usr/bin/env python3
"""
OpenAI cascaded voice bot for the SiphonAI bridge protocol v1.

A reference WebSocket server (the developer's "BYO AI" side of SiphonAI)
that runs a full **cascaded** voice pipeline using only OpenAI:

    caller audio ──► STT (Whisper / gpt-4o-transcribe)
                        ──► LLM (chat completions)
                            ──► TTS (gpt-4o-mini-tts / tts-1) ──► caller

This is the *cascaded pipeline* counterpart to ``openai-realtime-bridge-py``
(which uses OpenAI's single speech-to-speech Realtime API). Use this one when
you want to pick STT / LLM / TTS independently, swap models, or inspect the
transcript + LLM turns. It's higher-latency than Realtime but far easier to
reason about and customise.

SiphonAI is provider-neutral and contains no AI code — all of that lives
here, in the WS server, which is exactly the layer it belongs in.

How it works
------------
- **Transport (SiphonAI protocol v1).** One WebSocket per call. SiphonAI
  sends a ``start`` JSON message (audio format), then 20 ms PCM16-LE mono
  binary frames of caller audio. We push 20 ms PCM16 frames back to play
  into the call. See ``docs/PROTOCOL.md``.
- **Endpointing (turn-taking) is done here, in the bot.** OpenAI's
  transcription API is batch (you send a complete utterance), so we need to
  know when the caller stopped talking. We run ``webrtcvad`` over the inbound
  20 ms frames — no dependency on any SiphonAI VAD config, so this works with
  the same default route config as the echo server.
- **Barge-in.** When the caller starts talking while the bot is speaking, we
  cancel the in-flight response and send a ``clear`` (flush SiphonAI's
  outbound queue). SiphonAI's own ``auto_clear`` barge-in handles this too;
  the explicit ``clear`` also covers ``notify_only`` deployments.
- **Greeting on connect.** We synthesize a greeting immediately on ``start``
  so the bot speaks first — good UX, and it satisfies SiphonAI's
  ``server_too_slow`` start-deadline (default 5 s; raise
  ``[bridge].server_start_deadline_secs`` if your models cold-start slowly).

Audio format
------------
SiphonAI pins inbound + outbound to PCM16-LE, mono, 20 ms frames at the
negotiated rate (8 kHz → 320 B, 16 kHz → 640 B). OpenAI TTS ``pcm`` output is
24 kHz, so we resample it down to the call rate and re-chunk into exact 20 ms
frames before sending.

Run
---
    export OPENAI_API_KEY=sk-...
    python3 server.py --bind 0.0.0.0:8080

Configure the SiphonAI route's ``ws_url`` to point here. No special route
config is required (the bot endpoints itself), though leaving the default
``[bridge.barge_in] mode = "auto_clear"`` on gives the snappiest barge-in.
"""

from __future__ import annotations

import argparse
import asyncio
import io
import json
import logging
import os
import signal
import time
import wave
from collections import deque
from dataclasses import dataclass, field

# `audioop` is stdlib through Python 3.12 and provided by the `audioop-lts`
# backport on 3.13+ (see requirements.txt). Used only for rate conversion.
import audioop

import webrtcvad
import websockets
from openai import AsyncOpenAI
from websockets.asyncio.server import ServerConnection, serve
from websockets.http11 import Request, Response

LOG = logging.getLogger("openai-bot")
SUBPROTOCOL = "siphon-ai.v1"

# OpenAI TTS `response_format="pcm"` is fixed at 24 kHz, 16-bit, mono, LE.
TTS_PCM_RATE = 24000
SAMPLE_WIDTH = 2  # PCM16


# ─── Config ──────────────────────────────────────────────────────────────────


@dataclass
class Config:
    bind_host: str
    bind_port: int
    auth_token: str | None
    log_level: str
    # OpenAI
    stt_model: str
    llm_model: str
    tts_model: str
    tts_voice: str
    system_prompt: str
    greeting: str
    base_url: str | None
    # Endpointing (webrtcvad)
    vad_aggressiveness: int  # 0..3, higher = more aggressive filtering
    start_speech_ms: int  # consecutive speech needed to open an utterance
    end_silence_ms: int  # trailing silence that closes an utterance
    preroll_ms: int  # audio kept before trigger so onsets aren't clipped
    max_utterance_ms: int  # hard cap so a noisy line can't buffer forever


def config_from(args: argparse.Namespace) -> Config:
    host, _, port = args.bind.partition(":")
    if not port:
        raise SystemExit("--bind must be HOST:PORT")
    return Config(
        bind_host=host,
        bind_port=int(port),
        auth_token=os.environ.get("BOT_AUTH_TOKEN") or args.auth_token,
        log_level=args.log_level,
        stt_model=os.environ.get("BOT_STT_MODEL", "whisper-1"),
        llm_model=os.environ.get("BOT_LLM_MODEL", "gpt-4o-mini"),
        tts_model=os.environ.get("BOT_TTS_MODEL", "gpt-4o-mini-tts"),
        tts_voice=os.environ.get("BOT_TTS_VOICE", "alloy"),
        system_prompt=os.environ.get(
            "BOT_SYSTEM_PROMPT",
            "You are a friendly, concise voice assistant on a phone call. "
            "Keep replies short and conversational — one or two sentences — "
            "since they will be spoken aloud. Do not use markdown or emoji.",
        ),
        greeting=os.environ.get(
            "BOT_GREETING", "Hi! Thanks for calling. How can I help you today?"
        ),
        base_url=os.environ.get("OPENAI_BASE_URL"),
        vad_aggressiveness=int(os.environ.get("BOT_VAD_AGGRESSIVENESS", "2")),
        start_speech_ms=int(os.environ.get("BOT_START_SPEECH_MS", "120")),
        end_silence_ms=int(os.environ.get("BOT_END_SILENCE_MS", "700")),
        preroll_ms=int(os.environ.get("BOT_PREROLL_MS", "200")),
        max_utterance_ms=int(os.environ.get("BOT_MAX_UTTERANCE_MS", "30000")),
    )


# ─── Audio helpers (pure; covered by test_smoke.py) ──────────────────────────


def pcm_to_wav_bytes(pcm: bytes, rate: int) -> bytes:
    """Wrap raw PCM16-LE mono samples in a WAV container for the STT API."""
    buf = io.BytesIO()
    with wave.open(buf, "wb") as w:
        w.setnchannels(1)
        w.setsampwidth(SAMPLE_WIDTH)
        w.setframerate(rate)
        w.writeframes(pcm)
    return buf.getvalue()


def resample_pcm(pcm: bytes, in_rate: int, out_rate: int) -> bytes:
    """Resample mono PCM16-LE between rates (anti-aliased via audioop)."""
    if in_rate == out_rate:
        return pcm
    converted, _ = audioop.ratecv(pcm, SAMPLE_WIDTH, 1, in_rate, out_rate, None)
    return converted


def frame_chunks(pcm: bytes, frame_bytes: int):
    """Yield exactly-``frame_bytes`` PCM frames; zero-pad the final frame.

    SiphonAI requires every outbound binary frame to be exactly one 20 ms
    chunk — never "approximately". A short tail is padded with silence.
    """
    for off in range(0, len(pcm), frame_bytes):
        frame = pcm[off : off + frame_bytes]
        if len(frame) < frame_bytes:
            frame = frame + b"\x00" * (frame_bytes - len(frame))
        yield frame


# ─── Endpointer (webrtcvad turn detection) ───────────────────────────────────


@dataclass
class Endpointer:
    """Per-call utterance detector over 20 ms PCM16 frames.

    Emits ``"start"`` once the caller has been speaking for ``start_speech_ms``
    and ``("end", pcm)`` once they've been silent for ``end_silence_ms``. A
    short pre-roll is prepended so the onset isn't clipped.
    """

    rate: int
    frame_bytes: int
    cfg: Config
    _vad: webrtcvad.Vad = field(init=False)
    _triggered: bool = False
    _utterance: bytearray = field(default_factory=bytearray)
    _preroll: deque = field(init=False)
    _speech_run: int = 0
    _silence_run: int = 0

    def __post_init__(self) -> None:
        self._vad = webrtcvad.Vad(self.cfg.vad_aggressiveness)
        self._preroll = deque(maxlen=max(1, self.cfg.preroll_ms // 20))
        self._start_frames = max(1, self.cfg.start_speech_ms // 20)
        self._end_frames = max(1, self.cfg.end_silence_ms // 20)
        self._max_frames = max(1, self.cfg.max_utterance_ms // 20)

    def process(self, frame: bytes):
        """Feed one 20 ms frame. Returns None, "start", or ("end", pcm)."""
        # webrtcvad only accepts exact 10/20/30 ms 16-bit mono frames; a
        # wrong-sized frame (shouldn't happen on a compliant bridge) is
        # treated as non-speech but still buffered if we're mid-utterance.
        is_speech = (
            len(frame) == self.frame_bytes and self._vad.is_speech(frame, self.rate)
        )

        if not self._triggered:
            self._preroll.append(frame)
            if is_speech:
                self._speech_run += 1
                if self._speech_run >= self._start_frames:
                    self._triggered = True
                    self._utterance = bytearray(b"".join(self._preroll))
                    self._preroll.clear()
                    self._silence_run = 0
                    return "start"
            else:
                self._speech_run = 0
            return None

        # Triggered: accumulate until trailing silence (or the hard cap).
        self._utterance += frame
        if is_speech:
            self._silence_run = 0
        else:
            self._silence_run += 1

        ended = (
            self._silence_run >= self._end_frames
            or len(self._utterance) >= self._max_frames * self.frame_bytes
        )
        if ended:
            utt = bytes(self._utterance)
            self._reset()
            return ("end", utt)
        return None

    def _reset(self) -> None:
        self._triggered = False
        self._utterance = bytearray()
        self._speech_run = 0
        self._silence_run = 0
        self._preroll.clear()


# ─── Call session ────────────────────────────────────────────────────────────


class CallSession:
    """Owns one call: VAD turn-taking, the OpenAI pipeline, and playback."""

    def __init__(self, conn: ServerConnection, client: AsyncOpenAI, cfg: Config):
        self.conn = conn
        self.client = client
        self.cfg = cfg
        self.call_id: str | None = None
        self.rate = 8000
        self.frame_bytes = 320
        self.endpointer: Endpointer | None = None
        # Conversation history for the LLM (system prompt seeded on start).
        self.history: list[dict[str, str]] = []
        # The in-flight response (STT→LLM→TTS→playback). Cancelled on barge-in.
        self.turn: asyncio.Task | None = None

    # -- lifecycle --

    async def on_start(self, msg: dict) -> None:
        self.call_id = msg.get("call_id")
        audio = msg.get("audio", {})
        self.rate = int(audio.get("sample_rate", 8000))
        self.frame_bytes = self.rate // 50 * SAMPLE_WIDTH  # 20 ms of PCM16
        self.endpointer = Endpointer(self.rate, self.frame_bytes, self.cfg)
        self.history = [{"role": "system", "content": self.cfg.system_prompt}]
        LOG.info(
            "start call_id=%s rate=%d frame_bytes=%d from=%s to=%s",
            self.call_id, self.rate, self.frame_bytes, msg.get("from"), msg.get("to"),
        )
        # Speak first: greeting doubles as the start-deadline keep-alive.
        self.turn = asyncio.create_task(self._speak(self.cfg.greeting))

    async def on_audio(self, frame: bytes) -> None:
        if self.endpointer is None:
            return
        event = self.endpointer.process(frame)
        if event == "start":
            # Caller began talking. If the bot is mid-response, that's a
            # barge-in: cancel playback and flush SiphonAI's outbound queue.
            await self._barge_in()
        elif isinstance(event, tuple) and event[0] == "end":
            utterance = event[1]
            # Latest utterance wins — cancel any still-running response.
            await self._cancel_turn()
            self.turn = asyncio.create_task(self._respond(utterance))

    async def close(self) -> None:
        await self._cancel_turn()

    # -- barge-in / cancellation --

    async def _barge_in(self) -> None:
        if self.turn and not self.turn.done():
            LOG.info("barge-in (call_id=%s) — cancelling response", self.call_id)
            await self._cancel_turn()
            await self._send_clear()

    async def _cancel_turn(self) -> None:
        if self.turn and not self.turn.done():
            self.turn.cancel()
            try:
                await self.turn
            except asyncio.CancelledError:
                pass
        self.turn = None

    async def _send_clear(self) -> None:
        if self.call_id:
            await self._send_json({"type": "clear", "call_id": self.call_id})

    # -- pipeline --

    async def _respond(self, utterance_pcm: bytes) -> None:
        """STT → LLM → TTS → playback for one caller utterance."""
        t0 = time.monotonic()
        try:
            transcript = await self._transcribe(utterance_pcm)
            if not transcript.strip():
                LOG.info("empty transcript — ignoring")
                return
            LOG.info("caller: %s", transcript)

            self.history.append({"role": "user", "content": transcript})
            reply = await self._chat()
            self.history.append({"role": "assistant", "content": reply})
            LOG.info("bot: %s  (think=%.0fms)", reply, (time.monotonic() - t0) * 1000)

            await self._speak(reply)
        except asyncio.CancelledError:
            LOG.debug("response cancelled (barge-in or newer utterance)")
            raise
        except Exception:
            LOG.exception("pipeline error; staying on the call")

    async def _transcribe(self, pcm: bytes) -> str:
        wav = pcm_to_wav_bytes(pcm, self.rate)
        resp = await self.client.audio.transcriptions.create(
            model=self.cfg.stt_model,
            file=("utterance.wav", wav, "audio/wav"),
        )
        return resp.text or ""

    async def _chat(self) -> str:
        resp = await self.client.chat.completions.create(
            model=self.cfg.llm_model,
            messages=self.history,
            temperature=0.6,
        )
        return (resp.choices[0].message.content or "").strip()

    async def _speak(self, text: str) -> None:
        """Synthesize ``text`` and stream it into the call as 20 ms frames.

        For clarity this collects the full TTS audio before playback begins;
        for lower latency you'd stream OpenAI's PCM chunks straight into the
        pacer (resampling incrementally with an audioop.ratecv state).
        """
        if not text.strip():
            return
        pcm24 = bytearray()
        async with self.client.audio.speech.with_streaming_response.create(
            model=self.cfg.tts_model,
            voice=self.cfg.tts_voice,
            input=text,
            response_format="pcm",
        ) as response:
            async for chunk in response.iter_bytes():
                pcm24 += chunk

        pcm = resample_pcm(bytes(pcm24), TTS_PCM_RATE, self.rate)
        await self._play(pcm)

    async def _play(self, pcm: bytes) -> None:
        """Send PCM as paced 20 ms frames (real-time, drift-corrected)."""
        loop = asyncio.get_running_loop()
        next_t = loop.time()
        for frame in frame_chunks(pcm, self.frame_bytes):
            await self.conn.send(frame)
            next_t += 0.02
            delay = next_t - loop.time()
            if delay > 0:
                # Cancellation (barge-in) lands here as CancelledError.
                await asyncio.sleep(delay)

    async def _send_json(self, payload: dict) -> None:
        try:
            await self.conn.send(json.dumps(payload))
        except websockets.exceptions.ConnectionClosed:
            pass


# ─── HTTP-side concerns: auth + healthz ──────────────────────────────────────


def make_request_handler(cfg: Config):
    def process_request(connection: ServerConnection, request: Request) -> Response | None:
        if request.path == "/healthz":
            return connection.respond(200, "ok\n")
        if cfg.auth_token is None:
            return None
        if request.headers.get("Authorization", "") != f"Bearer {cfg.auth_token}":
            LOG.warning("rejecting upgrade: bad/missing Authorization header")
            return connection.respond(401, "Unauthorized\n")
        return None

    return process_request


# ─── Connection handler ──────────────────────────────────────────────────────


async def handle(connection: ServerConnection, client: AsyncOpenAI, cfg: Config) -> None:
    peer = connection.remote_address
    LOG.info("connect peer=%s subprotocol=%r", peer, connection.subprotocol)
    session = CallSession(connection, client, cfg)
    try:
        async for message in connection:
            if isinstance(message, bytes):
                await session.on_audio(message)
                continue
            try:
                msg = json.loads(message)
            except json.JSONDecodeError as e:
                LOG.warning("invalid JSON text frame: %s — ignoring", e)
                continue
            mtype = msg.get("type")
            if mtype == "start":
                if msg.get("version") != "1":
                    await connection.close(code=1003, reason="unsupported version")
                    return
                await session.on_start(msg)
            elif mtype == "stop":
                LOG.info("stop call_id=%s reason=%s", session.call_id, msg.get("reason"))
                break
            elif mtype == "error":
                LOG.warning("SiphonAI error: %s", {k: v for k, v in msg.items() if k != "type"})
            elif mtype in {"speech_started", "speech_stopped", "dtmf", "mark"}:
                # We do our own endpointing; these are informational here.
                LOG.debug("%s: %s", mtype, {k: v for k, v in msg.items() if k != "type"})
            else:
                LOG.debug("unhandled message type=%r", mtype)
    except websockets.exceptions.ConnectionClosedError as e:
        LOG.info("connection closed unexpectedly: %s", e)
    except websockets.exceptions.ConnectionClosedOK:
        LOG.info("connection closed cleanly")
    finally:
        await session.close()
        LOG.info("done peer=%s call_id=%s", peer, session.call_id)


# ─── Entrypoint ──────────────────────────────────────────────────────────────


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    p = argparse.ArgumentParser(description="OpenAI cascaded voice bot for SiphonAI v1.")
    p.add_argument("--bind", default="0.0.0.0:8080", metavar="HOST:PORT")
    p.add_argument(
        "--auth-token",
        default=None,
        help="require Authorization: Bearer <token> (or set BOT_AUTH_TOKEN)",
    )
    p.add_argument("--log-level", default="INFO", choices=["DEBUG", "INFO", "WARNING", "ERROR"])
    return p.parse_args(argv)


async def main(cfg: Config) -> None:
    if not os.environ.get("OPENAI_API_KEY"):
        raise SystemExit("OPENAI_API_KEY is not set")
    client = AsyncOpenAI(base_url=cfg.base_url) if cfg.base_url else AsyncOpenAI()

    async def handler(connection: ServerConnection) -> None:
        await handle(connection, client, cfg)

    async with serve(
        handler,
        host=cfg.bind_host,
        port=cfg.bind_port,
        subprotocols=[SUBPROTOCOL],
        process_request=make_request_handler(cfg),
        max_size=256 * 1024,  # PROTOCOL.md §2.1
        ping_interval=15,
        ping_timeout=10,
    ) as server:
        LOG.info(
            "listening on ws://%s:%d  (stt=%s llm=%s tts=%s/%s, auth=%s)",
            cfg.bind_host, cfg.bind_port, cfg.stt_model, cfg.llm_model,
            cfg.tts_model, cfg.tts_voice, "on" if cfg.auth_token else "off",
        )
        loop = asyncio.get_running_loop()
        stop = loop.create_future()
        for sig in (signal.SIGINT, signal.SIGTERM):
            loop.add_signal_handler(sig, lambda s=sig: stop.set_result(s))
        try:
            await stop
        finally:
            server.close()
            await server.wait_closed()


if __name__ == "__main__":
    args = parse_args()
    cfg = config_from(args)
    logging.basicConfig(
        level=getattr(logging, cfg.log_level),
        format="%(asctime)s %(levelname)s %(name)s: %(message)s",
    )
    try:
        asyncio.run(main(cfg))
    except KeyboardInterrupt:
        pass
