"""SiphonTransport end to end: a real uvicorn server running a Pipecat
pipeline on the transport, and a ``websockets`` client playing the daemon.

The bot under test echoes caller audio and maps DTMF digits to actions so
each test can drive one behaviour:

    #  → EndFrame (bot ends the call)      1 → InterruptionFrame
    2  → OutputDTMFUrgentFrame("9")        3 → 100 ms tone, then an
                                               ordered `transfer` command
    4  → `transfer`, then EndFrame (bot hands off and ends its pipeline)
    5  → 1 s of "TTS" from a fixed-24 kHz service (see NativeRateTTS)
"""

from __future__ import annotations

import asyncio
import json
import socket
import time

import pytest
import uvicorn
import websockets
from fastapi import FastAPI, WebSocket
from pipecat.audio.dtmf.types import KeypadEntry
from pipecat.frames.frames import (
    EndFrame,
    Frame,
    InputAudioRawFrame,
    InterruptionFrame,
    OutputAudioRawFrame,
    OutputDTMFUrgentFrame,
    StartFrame,
    TTSAudioRawFrame,
    TTSSpeakFrame,
    TTSStartedFrame,
    TTSStoppedFrame,
)
from pipecat.pipeline.pipeline import Pipeline
from pipecat.pipeline.worker import PipelineWorker
from pipecat.processors.frame_processor import FrameDirection, FrameProcessor
from pipecat.workers.runner import WorkerRunner
from siphon_ai_pipecat import (
    SiphonHandshakeError,
    SiphonParams,
    SiphonTransport,
    siphon_command,
)
from siphon_ai_server import Dtmf

TOKEN = "s3cret"
CALL_ID = "siphon-t1"
FRAME = 320  # 20 ms @ 8 kHz


class NativeRateTTS(FrameProcessor):
    """Behaves like Pipecat's OpenAI TTS: always synthesizes 24 kHz audio but
    labels frames with the pipeline's output rate from the StartFrame. With
    an 8 kHz pipeline output rate that plays 24 kHz audio 3x slow — the bug
    the live provider test caught."""

    NATIVE_RATE = 24000

    def __init__(self):
        super().__init__()
        self._label_rate = self.NATIVE_RATE

    async def process_frame(self, frame: Frame, direction: FrameDirection):
        await super().process_frame(frame, direction)
        if isinstance(frame, StartFrame):
            self._label_rate = frame.audio_out_sample_rate
        if isinstance(frame, TTSSpeakFrame):
            await self.push_frame(TTSStartedFrame())
            one_second = b"\x10\x00" * self.NATIVE_RATE
            await self.push_frame(TTSAudioRawFrame(one_second, self._label_rate, 1))
            await self.push_frame(TTSStoppedFrame())
            return
        await self.push_frame(frame, direction)


class Echo(FrameProcessor):
    async def process_frame(self, frame: Frame, direction: FrameDirection):
        await super().process_frame(frame, direction)
        if isinstance(frame, InputAudioRawFrame):
            await self.push_frame(
                OutputAudioRawFrame(frame.audio, frame.sample_rate, frame.num_channels)
            )
        else:
            await self.push_frame(frame, direction)


def build_app(seen: dict) -> FastAPI:
    app = FastAPI()

    @app.websocket("/ws")
    async def ws(websocket: WebSocket):
        try:
            transport = await SiphonTransport.accept(
                websocket, SiphonParams(end_drain_timeout_secs=1.0), auth_token=TOKEN
            )
        except SiphonHandshakeError as e:
            seen["handshake_error"] = str(e)
            return
        worker = PipelineWorker(
            Pipeline([transport.input(), Echo(), NativeRateTTS(), transport.output()]),
            params=transport.pipeline_params(),
        )

        @transport.event_handler("on_call_stopped")
        async def stopped(transport, reason):
            seen["stopped"] = reason
            await worker.cancel()

        @transport.event_handler("on_client_disconnected")
        async def gone(transport, websocket):
            await worker.cancel()

        @transport.event_handler("on_siphon_event")
        async def event(transport, ev):
            seen.setdefault("events", []).append(ev.type)
            if not isinstance(ev, Dtmf):
                return
            if ev.digit == "#":
                await worker.queue_frame(EndFrame())
            elif ev.digit == "1":
                await worker.queue_frame(InterruptionFrame())
            elif ev.digit == "2":
                await worker.queue_frame(OutputDTMFUrgentFrame(button=KeypadEntry.NINE))
            elif ev.digit == "3":
                await worker.queue_frames(
                    [
                        OutputAudioRawFrame(b"\x10\x00" * 800, 8000, 1),
                        siphon_command("transfer", target="sip:agent@pbx"),
                    ]
                )
            elif ev.digit == "4":
                await worker.queue_frames(
                    [siphon_command("transfer", target="sip:agent@pbx"), EndFrame()]
                )
            elif ev.digit == "5":
                await worker.queue_frame(TTSSpeakFrame("hello"))

        runner = WorkerRunner(handle_sigint=False)
        await runner.add_workers(worker)
        await runner.run()
        seen["pipeline_done"] = True

    return app


def free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def start_msg(**extra) -> str:
    return json.dumps(
        {
            "type": "start",
            "version": "1",
            "call_id": CALL_ID,
            "seq": 0,
            "from": "+13125551212",
            "to": "5000",
            "direction": "inbound",
            "audio": {"encoding": "pcm16le", "sample_rate": 8000, "channels": 1, "frame_ms": 20},
            "sip": {"call_id": "abc@pbx", "headers": {}},
            "barge_in_mode": "notify_only",
            **extra,
        }
    )


def ev(type_: str, seq: int, **fields) -> str:
    return json.dumps({"type": type_, "call_id": CALL_ID, "seq": seq, **fields})


def dtmf(digit: str, seq: int) -> str:
    return ev("dtmf", seq, digit=digit, duration_ms=100, method="rfc2833")


class Daemon:
    """The daemon's side of one call: records everything the bot sends."""

    def __init__(self, ws):
        self.ws = ws
        self.audio: list[tuple[float, int]] = []  # (arrival, size)
        self.commands: list[dict] = []
        self.order: list[str] = []  # "audio" / command type, in arrival order
        self.cmd_at: dict[str, float] = {}  # first arrival time per command type
        self.closed = asyncio.Event()
        self._task = asyncio.create_task(self._recv())

    async def _recv(self):
        try:
            async for msg in self.ws:
                if isinstance(msg, bytes):
                    self.audio.append((time.monotonic(), len(msg)))
                    self.order.append("audio")
                else:
                    cmd = json.loads(msg)
                    self.cmd_at.setdefault(cmd["type"], time.monotonic())
                    self.commands.append(cmd)
                    self.order.append(cmd["type"])
                    if cmd["type"] == "mark":  # play-out done at once
                        await self.ws.send(ev("mark", 90, name=cmd["name"]))
        except websockets.ConnectionClosed:
            pass
        self.closed.set()

    def types(self) -> list[str]:
        return [c["type"] for c in self.commands]

    async def wait_for(self, pred, timeout=3.0):
        deadline = time.monotonic() + timeout
        while not pred():
            if time.monotonic() > deadline:
                raise AssertionError(f"timed out; commands={self.commands} audio={len(self.audio)}")
            await asyncio.sleep(0.01)


def scenario(body, *, headers=None, start=None):
    """Run ``body(daemon, seen)`` against a fresh server; return ``seen``."""

    async def main():
        seen: dict = {}
        port = free_port()
        server = uvicorn.Server(
            uvicorn.Config(build_app(seen), host="127.0.0.1", port=port, log_level="warning")
        )
        serve = asyncio.create_task(server.serve())
        while not server.started:
            await asyncio.sleep(0.01)
        try:
            async with websockets.connect(
                f"ws://127.0.0.1:{port}/ws",
                subprotocols=["siphon-ai.v1"],
                additional_headers=headers
                if headers is not None
                else {"Authorization": f"Bearer {TOKEN}"},
            ) as ws:
                seen["subprotocol"] = ws.subprotocol
                await ws.send(start or start_msg())
                await body(Daemon(ws), seen)
        finally:
            await asyncio.sleep(0.2)  # let the server-side task wind down
            server.should_exit = True
            await serve
        return seen

    return asyncio.run(main())


def test_handshake_and_start_deadline_prime():
    async def body(d, seen):
        await d.wait_for(lambda: d.audio)
        assert d.audio[0][1] == FRAME  # one 20 ms silence frame, unprompted
        await d.ws.send(ev("stop", 1, reason="caller_hangup"))
        await asyncio.wait_for(d.closed.wait(), 3)

    seen = scenario(body)
    assert seen["subprotocol"] == "siphon-ai.v1"
    assert seen["stopped"] == "caller_hangup"


def test_echo_is_framed_and_paced_at_real_time():
    n = 50  # 1 s of caller audio, sent as a burst

    async def body(d, seen):
        await d.wait_for(lambda: d.audio)  # prime frame
        for _ in range(n):
            await d.ws.send(bytes(FRAME))
        await d.wait_for(lambda: len(d.audio) >= n + 1, timeout=5)
        echoed = d.audio[1:]
        assert all(size == FRAME for _, size in d.audio)
        span = echoed[-1][0] - echoed[0][0]
        # Real time is 980 ms for 50 frames; allow the 60 ms lead plus
        # scheduling slack. Pipecat's stock 2× pacing would take ~490 ms.
        assert span >= 0.85, span
        await d.ws.send(ev("stop", 1, reason="caller_hangup"))

    scenario(body)


def test_end_frame_drains_then_hangs_up():
    async def body(d, seen):
        await d.ws.send(dtmf("#", 1))
        await asyncio.wait_for(d.closed.wait(), 5)
        assert d.types() == ["mark", "hangup"]
        assert d.commands[0]["name"] == "pipecat-end"
        assert d.commands[1] == {"type": "hangup", "call_id": CALL_ID, "cause": "normal"}

    seen = scenario(body)
    assert seen["pipeline_done"]


def test_stop_never_draws_a_hangup():
    async def body(d, seen):
        await d.ws.send(ev("stop", 1, reason="caller_hangup"))
        await asyncio.wait_for(d.closed.wait(), 5)
        assert "hangup" not in d.types()

    scenario(body)


def test_interruption_sends_clear():
    async def body(d, seen):
        await d.ws.send(dtmf("1", 1))
        await d.wait_for(lambda: "clear" in d.types())
        await d.ws.send(ev("stop", 2, reason="caller_hangup"))

    scenario(body)


def test_dtmf_out_is_native_send_dtmf():
    async def body(d, seen):
        await d.ws.send(dtmf("2", 1))
        await d.wait_for(lambda: "send_dtmf" in d.types())
        cmd = d.commands[d.types().index("send_dtmf")]
        assert cmd["digit"] == "9" and cmd["duration_ms"] == 160
        await d.ws.send(ev("stop", 2, reason="caller_hangup"))

    scenario(body)


def test_ordered_command_follows_its_audio():
    async def body(d, seen):
        await d.wait_for(lambda: d.audio)  # prime frame first
        await d.ws.send(dtmf("3", 1))
        await d.wait_for(lambda: "transfer" in d.types())
        # 100 ms tone = 5 frames, all ahead of the transfer.
        assert d.order[d.order.index("transfer") - 5 : d.order.index("transfer")] == ["audio"] * 5
        await d.ws.send(ev("stop", 2, reason="transfer"))

    scenario(body)


def test_ending_after_transfer_waits_for_its_stop_instead_of_hanging_up():
    async def body(d, seen):
        await d.ws.send(dtmf("4", 1))
        await d.wait_for(lambda: "transfer" in d.types())
        await asyncio.sleep(0.5)  # REFER still in flight at the PBX
        assert "hangup" not in d.types()  # a BYE now would kill the transfer
        await d.ws.send(ev("stop", 2, reason="transfer"))
        await asyncio.wait_for(d.closed.wait(), 3)
        assert "hangup" not in d.types()

    scenario(body)


def test_failed_transfer_then_end_still_hangs_up():
    async def body(d, seen):
        await d.ws.send(dtmf("4", 1))
        await d.wait_for(lambda: "transfer" in d.types())
        await d.ws.send(ev("error", 2, code="transfer_failed", message="REFER rejected: 403"))
        await asyncio.wait_for(d.closed.wait(), 5)
        assert d.types() == ["transfer", "mark", "hangup"]

    scenario(body)


def test_fixed_rate_tts_plays_at_its_true_length():
    async def body(d, seen):
        await d.wait_for(lambda: d.audio)  # prime frame
        before = len(d.audio)
        await d.ws.send(dtmf("5", 1))
        await asyncio.sleep(1.8)  # 1 s of speech, paced, plus slack
        frames = len(d.audio) - before
        # 1 s = 50 frames (+1 for padding the tail). Pinning the pipeline's
        # output rate to the call's 8 kHz made this ~150: 3x slow.
        assert 48 <= frames <= 52, frames
        await d.ws.send(ev("stop", 2, reason="caller_hangup"))

    scenario(body)


def test_pause_mode_rejects_when_pipecat_does_not_interrupt():
    async def body(d, seen):
        t0 = time.monotonic()
        await d.ws.send(
            ev("speech_started", 1, ts_ms=1, decision_pending=True, decision_deadline_ms=400)
        )
        await d.wait_for(lambda: "barge_in_reject" in d.types())
        assert 0.25 <= time.monotonic() - t0 <= 0.45  # deadline less the 100 ms margin
        await d.ws.send(ev("stop", 2, reason="caller_hangup"))

    scenario(body, start=start_msg(barge_in_mode="pause"))


def test_pause_mode_holds_bot_audio_until_the_verdict():
    async def body(d, seen):
        await d.wait_for(lambda: d.audio)  # prime frame
        base = len(d.audio)
        await d.ws.send(dtmf("5", 1))  # 1 s of speech = 50 frames
        await d.wait_for(lambda: len(d.audio) >= base + 10)  # 200 ms in
        await d.ws.send(
            ev("speech_started", 2, ts_ms=1, decision_pending=True, decision_deadline_ms=600)
        )
        await asyncio.sleep(0.1)  # let frames already in flight land
        held_from = d.order.count("audio")
        await d.wait_for(lambda: "barge_in_reject" in d.types())
        # Nothing streamed into the daemon's pause (audio arriving before
        # the reject): on a reject it would sit behind the retained tail
        # and then be evicted (§5.5). The lead burst at release follows it.
        during_pause = d.order[: d.order.index("barge_in_reject")].count("audio") - held_from
        assert during_pause <= 1, during_pause
        # Released on the verdict, and none of the bot's speech is lost.
        await d.wait_for(lambda: len(d.audio) >= base + 50, timeout=3)
        await asyncio.sleep(0.3)
        assert 50 <= len(d.audio) - base <= 52, len(d.audio) - base
        await d.ws.send(ev("stop", 3, reason="caller_hangup"))

    scenario(body, start=start_msg(barge_in_mode="pause"))


def test_pause_mode_release_keeps_schedule_instead_of_bursting():
    async def body(d, seen):
        await d.wait_for(lambda: d.audio)  # prime frame
        await d.ws.send(dtmf("5", 1))
        await d.wait_for(lambda: len(d.audio) >= 12)
        await d.ws.send(
            ev(
                "speech_started",
                2,
                ts_ms=1,
                decision_pending=True,
                decision_deadline_ms=400,
                offset_ms=5000,
            )
        )
        await d.wait_for(lambda: "barge_in_reject" in d.types())
        t_reject = d.cmd_at["barge_in_reject"]
        await d.ws.send(ev("barge_in_resolved", 3, outcome="rejected", offset_ms=5300))
        await asyncio.sleep(0.15)
        burst = sum(1 for t, _ in d.audio if t_reject <= t <= t_reject + 0.1)
        # Frames sent ahead before the pause are still queued in the
        # daemon (a reject re-queues them). Re-anchoring sent a fresh lead
        # burst on top (~9 frames in 100 ms) and grew that backlog with
        # every pause until §5.5 evicted audio. The schedule now shifts by
        # the pause instead: real time again, with no burst.
        assert burst <= 5, burst
        await d.ws.send(ev("stop", 4, reason="caller_hangup"))

    scenario(body, start=start_msg(barge_in_mode="pause"))


def test_pause_mode_interruption_confirms_and_cancels_reject():
    async def body(d, seen):
        await d.ws.send(
            ev("speech_started", 1, ts_ms=1, decision_pending=True, decision_deadline_ms=400)
        )
        await d.ws.send(dtmf("1", 2))  # Pipecat interrupts → clear ≡ confirm
        await d.wait_for(lambda: "clear" in d.types())
        await asyncio.sleep(0.5)  # past the deadline
        assert "barge_in_reject" not in d.types()
        await d.ws.send(ev("stop", 3, reason="caller_hangup"))

    scenario(body, start=start_msg(barge_in_mode="pause"))


def test_events_reach_the_app_and_unknown_types_are_harmless():
    async def body(d, seen):
        await d.ws.send(ev("from_the_future", 1))
        await d.ws.send(ev("hold", 2, direction="sendonly"))
        await d.ws.send(ev("stop", 3, reason="caller_hangup"))
        await asyncio.wait_for(d.closed.wait(), 3)

    seen = scenario(body)
    assert seen["events"][:3] == ["from_the_future", "hold", "stop"]


def test_bad_token_refused_before_upgrade():
    with pytest.raises(websockets.InvalidStatus):
        scenario(lambda d, s: asyncio.sleep(0), headers={"Authorization": "Bearer nope"})


def test_unsupported_version_closes_1003():
    async def body(d, seen):
        await asyncio.wait_for(d.closed.wait(), 3)
        assert d.ws.close_code == 1003

    seen = scenario(body, start=start_msg(version="2"))
    assert seen["handshake_error"] == "unsupported version"
