#!/usr/bin/env python3
"""Offline smoke tests for the OpenAI pipeline bot's pure helpers.

No network and no OPENAI_API_KEY needed — importing ``server`` only defines
things (``main`` is guarded by ``__main__``). Run:

    pip install -r requirements.txt
    python3 -m pytest test_smoke.py     # or: python3 test_smoke.py
"""

from __future__ import annotations

import io
import wave

import server
from server import Config, Endpointer, frame_chunks, pcm_to_wav_bytes, resample_pcm


def _cfg(**over) -> Config:
    base = dict(
        bind_host="0.0.0.0", bind_port=8080, auth_token=None, log_level="INFO",
        stt_model="whisper-1", llm_model="gpt-4o-mini", tts_model="gpt-4o-mini-tts",
        tts_voice="alloy", system_prompt="sp", greeting="hi", base_url=None,
        vad_aggressiveness=2, start_speech_ms=40, end_silence_ms=60,
        preroll_ms=40, max_utterance_ms=30000,
    )
    base.update(over)
    return Config(**base)


def test_pcm_to_wav_roundtrips_header() -> None:
    pcm = b"\x01\x02" * 160  # 160 samples = 20 ms @ 8 kHz
    wav = pcm_to_wav_bytes(pcm, 8000)
    with wave.open(io.BytesIO(wav), "rb") as w:
        assert w.getnchannels() == 1
        assert w.getsampwidth() == 2
        assert w.getframerate() == 8000
        assert w.readframes(w.getnframes()) == pcm


def test_resample_changes_length_proportionally() -> None:
    # 24 kHz → 8 kHz should be ~1/3 the samples.
    pcm24 = b"\x00\x00" * 2400  # 2400 samples @ 24 kHz = 100 ms
    pcm8 = resample_pcm(pcm24, 24000, 8000)
    out_samples = len(pcm8) // 2
    assert 760 <= out_samples <= 840, out_samples  # ~800
    # Identity rate is a no-op (same object semantics not required, just equal).
    assert resample_pcm(pcm24, 8000, 8000) == pcm24


def test_frame_chunks_are_exact_and_padded() -> None:
    frame_bytes = 320
    # 2.5 frames of data → 3 frames out, last one zero-padded.
    pcm = b"\xAB" * (frame_bytes * 2 + 100)
    frames = list(frame_chunks(pcm, frame_bytes))
    assert len(frames) == 3
    assert all(len(f) == frame_bytes for f in frames)
    assert frames[2].endswith(b"\x00" * (frame_bytes - 100))


class _FakeVad:
    """Scripted VAD: returns the next bool each call (sticks on the last)."""

    def __init__(self, script: list[bool]) -> None:
        self._s = script
        self._i = 0

    def is_speech(self, frame: bytes, rate: int) -> bool:
        v = self._s[min(self._i, len(self._s) - 1)]
        self._i += 1
        return v


def test_endpointer_emits_start_then_end() -> None:
    fb = 320
    ep = Endpointer(8000, fb, _cfg(preroll_ms=40, start_speech_ms=40, end_silence_ms=60))
    # 2-frame preroll/start, 3-frame end-silence.
    script = [False, True, True, True, False, False, False]
    ep._vad = _FakeVad(script)  # type: ignore[assignment]

    events = [ep.process(b"\x00" * fb) for _ in script]
    kinds = [e if isinstance(e, str) else (e[0] if e else None) for e in events]

    assert "start" in kinds, kinds
    end = next(e for e in events if isinstance(e, tuple) and e[0] == "end")
    # Utterance = 2 preroll/speech frames + frames 3..6 = 6 frames total.
    assert len(end[1]) == 6 * fb, len(end[1])
    # Exactly one start and one end over the run.
    assert kinds.count("start") == 1
    assert sum(1 for e in events if isinstance(e, tuple)) == 1


def test_frame_bytes_match_rate() -> None:
    # The session computes 20 ms of PCM16 from the negotiated rate.
    assert 8000 // 50 * server.SAMPLE_WIDTH == 320
    assert 16000 // 50 * server.SAMPLE_WIDTH == 640


if __name__ == "__main__":
    for name, fn in sorted(globals().items()):
        if name.startswith("test_") and callable(fn):
            fn()
            print(f"ok  {name}")
    print("all smoke tests passed")
