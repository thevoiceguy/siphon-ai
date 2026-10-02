#!/usr/bin/env python3
"""Pipecat voice bot behind SiphonAI.

    SIP trunk / PBX ──► SiphonAI ──WS──► this server
                                         (Pipecat: Deepgram STT → OpenAI LLM → OpenAI TTS)

One WebSocket = one call = one Pipecat pipeline, built on
``siphon_ai_pipecat.SiphonTransport`` (``sdks/pipecat``). SiphonAI contains no
AI code; every provider call lives here.

    python3 server.py --bind 0.0.0.0:8080          # the voice bot
    python3 server.py --bind 0.0.0.0:8080 --echo   # no providers: echoes the caller

``--echo`` needs no API keys. It is what CI runs the protocol conformance
testkit against, and the quickest way to check the SiphonAI ↔ Pipecat leg on
its own.
"""

from __future__ import annotations

import argparse
import os
import sys

import uvicorn
from fastapi import FastAPI, WebSocket
from fastapi.responses import PlainTextResponse
from loguru import logger
from pipecat.frames.frames import (
    EndWorkerFrame,
    Frame,
    FunctionCallResultProperties,
    InputAudioRawFrame,
    OutputAudioRawFrame,
    TTSSpeakFrame,
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

DEFAULT_PROMPT = (
    "You are a friendly phone assistant. Callers hear you through a telephone, "
    "so answer in one or two short spoken sentences, with no lists, markdown or emoji. "
    "When the caller says goodbye or is done, call end_call."
)


class Echo(FrameProcessor):
    """Caller audio straight back out: the minimal Pipecat pipeline."""

    async def process_frame(self, frame: Frame, direction: FrameDirection):
        await super().process_frame(frame, direction)
        if isinstance(frame, InputAudioRawFrame):
            await self.push_frame(
                OutputAudioRawFrame(frame.audio, frame.sample_rate, frame.num_channels)
            )
        else:
            await self.push_frame(frame, direction)


def build_bot(transport: SiphonTransport) -> tuple[Pipeline, list[Frame]]:
    """STT → LLM → TTS pipeline plus the frames that greet the caller."""
    # Imported here so --echo runs without the provider extras installed.
    from pipecat.adapters.schemas.function_schema import FunctionSchema
    from pipecat.adapters.schemas.tools_schema import ToolsSchema
    from pipecat.audio.vad.silero import SileroVADAnalyzer
    from pipecat.processors.aggregators.llm_context import LLMContext
    from pipecat.processors.aggregators.llm_response_universal import (
        LLMContextAggregatorPair,
        LLMUserAggregatorParams,
    )
    from pipecat.services.deepgram.stt import DeepgramSTTService
    from pipecat.services.llm_service import FunctionCallParams
    from pipecat.services.openai.llm import OpenAILLMService
    from pipecat.services.openai.tts import OpenAITTSService
    from pipecat.turns.user_start.min_words_user_turn_start_strategy import (
        MinWordsUserTurnStartStrategy,
    )
    from pipecat.turns.user_turn_strategies import UserTurnStrategies

    transfer_target = os.environ.get("BOT_TRANSFER_TARGET")
    start = transport.start

    stt = DeepgramSTTService(api_key=os.environ["DEEPGRAM_API_KEY"])
    llm = OpenAILLMService(
        api_key=os.environ["OPENAI_API_KEY"],
        settings=OpenAILLMService.Settings(
            model=os.environ.get("BOT_LLM_MODEL", "gpt-4.1-mini"),
            system_instruction=os.environ.get("BOT_SYSTEM_PROMPT", DEFAULT_PROMPT)
            + f" The caller is calling from {start.from_ or 'an unknown number'}.",
        ),
    )
    tts = OpenAITTSService(
        api_key=os.environ["OPENAI_API_KEY"],
        settings=OpenAITTSService.Settings(voice=os.environ.get("BOT_TTS_VOICE", "alloy")),
    )

    async def end_call(params: FunctionCallParams):
        # EndWorkerFrame → EndFrame: the transport lets the goodbye finish
        # playing to the caller (mark round-trip), then sends `hangup`.
        # run_llm=False: the goodbye below is the last word. Letting the
        # LLM run on the result made it say a second "Goodbye!".
        await params.result_callback(
            {"status": "ending"}, properties=FunctionCallResultProperties(run_llm=False)
        )
        await params.llm.push_frame(TTSSpeakFrame("Thanks for calling. Goodbye!"))
        await params.llm.push_frame(EndWorkerFrame(), FrameDirection.UPSTREAM)

    async def transfer_call(params: FunctionCallParams):
        await params.result_callback(
            {"status": "transferring"}, properties=FunctionCallResultProperties(run_llm=False)
        )
        await params.llm.push_frame(TTSSpeakFrame("Transferring you now."))
        # A DataFrame: sent after the audio queued ahead of it.
        await params.llm.push_frame(siphon_command("transfer", target=transfer_target))

    tools = [
        FunctionSchema(
            name="end_call",
            description="Hang up once the caller is finished or says goodbye.",
            properties={},
            required=[],
        )
    ]
    llm.register_function("end_call", end_call)
    if transfer_target:
        tools.append(
            FunctionSchema(
                name="transfer_call",
                description="Transfer the caller to a human agent when they ask for one.",
                properties={},
                required=[],
            )
        )
        llm.register_function("transfer_call", transfer_call)

    # Pipecat's default interrupts the bot on any voice Silero accepts,
    # coughs and "mm-hmm" included. With BOT_INTERRUPT_MIN_WORDS=N, the
    # caller must say N words while the bot is talking before it is cut
    # off. Needed for SiphonAI pause mode to resume on backchannels; give
    # the route a [bridge.barge_in] decision_ms that covers STT latency.
    turn_strategies = None
    if min_words := int(os.environ.get("BOT_INTERRUPT_MIN_WORDS", "0")):
        turn_strategies = UserTurnStrategies(
            start=[MinWordsUserTurnStartStrategy(min_words=min_words)]
        )
    context = LLMContext(tools=ToolsSchema(standard_tools=tools))
    aggregators = LLMContextAggregatorPair(
        context,
        user_params=LLMUserAggregatorParams(
            vad_analyzer=SileroVADAnalyzer(), user_turn_strategies=turn_strategies
        ),
    )
    pipeline = Pipeline(
        [
            transport.input(),
            stt,
            aggregators.user(),
            llm,
            tts,
            transport.output(),
            aggregators.assistant(),
        ]
    )
    greeting = os.environ.get("BOT_GREETING", "Hi! Thanks for calling. How can I help?")
    return pipeline, ([TTSSpeakFrame(greeting)] if greeting else [])


def build_app(*, echo: bool, auth_token: str | None) -> FastAPI:
    app = FastAPI()

    @app.get("/healthz", response_class=PlainTextResponse)
    async def healthz() -> str:
        return "ok\n"

    async def call(websocket: WebSocket) -> None:
        try:
            transport = await SiphonTransport.accept(
                websocket, SiphonParams(), auth_token=auth_token
            )
        except SiphonHandshakeError as e:
            logger.warning(f"refused bridge connection: {e}")
            return

        if echo:
            pipeline, greeting = Pipeline([transport.input(), Echo(), transport.output()]), []
        else:
            pipeline, greeting = build_bot(transport)
        worker = PipelineWorker(pipeline, params=transport.pipeline_params())

        @transport.event_handler("on_client_connected")
        async def connected(transport, websocket):
            if greeting:
                await worker.queue_frames(greeting)

        @transport.event_handler("on_call_stopped")
        async def stopped(transport, reason):
            await worker.cancel()

        @transport.event_handler("on_client_disconnected")
        async def gone(transport, websocket):
            await worker.cancel()

        runner = WorkerRunner(handle_sigint=False)
        await runner.add_workers(worker)
        await runner.run()

    # `/` for SiphonAI configs and the conformance testkit, `/ws` for the
    # usual Pipecat telephony path.
    app.add_api_websocket_route("/", call)
    app.add_api_websocket_route("/ws", call)
    return app


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--bind", default="0.0.0.0:8080", help="host:port (default 0.0.0.0:8080)")
    ap.add_argument(
        "--echo", action="store_true", help="echo the caller instead of running the bot"
    )
    ap.add_argument(
        "--auth-token",
        default=os.environ.get("SIPHON_AUTH_TOKEN"),
        help="require `Authorization: Bearer <token>` (SiphonAI [bridge].auth_bearer)",
    )
    ap.add_argument("--log-level", default=os.environ.get("LOG_LEVEL", "INFO"))
    args = ap.parse_args()

    logger.remove()
    logger.add(sys.stderr, level=args.log_level.upper())
    if not args.echo:
        missing = [k for k in ("DEEPGRAM_API_KEY", "OPENAI_API_KEY") if not os.environ.get(k)]
        if missing:
            ap.error(f"missing {', '.join(missing)} (or pass --echo)")

    host, _, port = args.bind.rpartition(":")
    uvicorn.run(
        build_app(echo=args.echo, auth_token=args.auth_token),
        host=host or "0.0.0.0",
        port=int(port),
        log_level=args.log_level.lower(),
        ws_ping_interval=15,
        ws_ping_timeout=10,
    )


if __name__ == "__main__":
    main()
