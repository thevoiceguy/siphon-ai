#!/usr/bin/env python3
"""Offline smoke test: the app builds and the bot pipeline constructs.

No network and no real API keys: provider clients are only constructed,
never called. Run:

    pip install -r requirements.txt
    python3 -m pytest test_smoke.py
"""

from __future__ import annotations

import asyncio
import json

import pytest
import server
from fastapi.testclient import TestClient


def test_echo_app_serves_health_and_both_ws_paths():
    app = server.build_app(echo=True, auth_token=None)
    client = TestClient(app)
    assert client.get("/healthz").text == "ok\n"
    paths = {r.path for r in app.routes}
    assert {"/", "/ws"} <= paths


@pytest.mark.parametrize(
    "provider, tts_class",
    [
        (None, "DeepgramTTSService"),  # the default
        ("openai", "OpenAITTSService"),
        ("deepgram", "DeepgramTTSService"),
    ],
)
def test_bot_pipeline_builds(monkeypatch, provider, tts_class):
    pytest.importorskip("pipecat.services.deepgram.stt")
    pytest.importorskip("pipecat.audio.vad.silero")
    if provider is None:
        monkeypatch.delenv("BOT_TTS_PROVIDER", raising=False)
    else:
        monkeypatch.setenv("BOT_TTS_PROVIDER", provider)
    monkeypatch.setenv("DEEPGRAM_API_KEY", "test")
    monkeypatch.setenv("OPENAI_API_KEY", "test")
    monkeypatch.setenv("BOT_TRANSFER_TARGET", "sip:agent@pbx.example.com")

    from siphon_ai_pipecat import SiphonTransport
    from siphon_ai_server import parse_event

    start = parse_event(
        json.dumps(
            {
                "type": "start",
                "version": "1",
                "call_id": "c1",
                "seq": 0,
                "from": "+13125551212",
                "to": "5000",
                "direction": "inbound",
                "audio": {
                    "encoding": "pcm16le",
                    "sample_rate": 8000,
                    "channels": 1,
                    "frame_ms": 20,
                },
                "sip": {"call_id": "x@y", "headers": {}},
                "barge_in_mode": "notify_only",
            }
        )
    )

    class FakeWebSocket:  # never touched: nothing is started
        headers: dict = {}

    async def build():
        transport = SiphonTransport(FakeWebSocket(), start)
        return server.build_bot(transport)

    pipeline, greeting = asyncio.run(build())
    assert len(greeting) == 1
    assert len(pipeline.processors) >= 7
    (tts,) = [p for p in pipeline.processors if type(p).__name__ == tts_class]
    if tts_class == "DeepgramTTSService":
        # Synthesizes at the wire rate, so frames are labelled correctly.
        assert tts._init_sample_rate == 8000
    else:
        assert tts._settings.model == "tts-1"


def test_unknown_tts_provider_is_refused(monkeypatch):
    monkeypatch.setenv("BOT_TTS_PROVIDER", "nope")

    class _Start:
        class audio:
            sample_rate = 8000

    class _T:
        start = _Start

    with pytest.raises(ValueError, match="BOT_TTS_PROVIDER"):
        server.build_tts(_T())


if __name__ == "__main__":
    raise SystemExit(pytest.main([__file__, "-q"]))
