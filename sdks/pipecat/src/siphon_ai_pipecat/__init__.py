"""Pipecat transport for SiphonAI.

Answer SIP calls with a Pipecat pipeline: SiphonAI bridges SIP/RTP to a
WebSocket (protocol v1, ``docs/PROTOCOL.md``); this package turns that
WebSocket into a Pipecat transport::

    from fastapi import FastAPI, WebSocket
    from siphon_ai_pipecat import SiphonHandshakeError, SiphonTransport

    app = FastAPI()

    @app.websocket("/ws")
    async def ws(websocket: WebSocket):
        try:
            transport = await SiphonTransport.accept(websocket)
        except SiphonHandshakeError:
            return
        pipeline = Pipeline([transport.input(), stt, user_agg, llm, tts,
                             transport.output(), assistant_agg])
        worker = PipelineWorker(pipeline, params=transport.pipeline_params())

        @transport.event_handler("on_call_stopped")
        async def _(transport, reason):
            await worker.cancel()

        runner = WorkerRunner(handle_sigint=False)
    await runner.add_workers(worker)
    await runner.run()

Design notes: ``docs/design/DESIGN_PIPECAT.md``. **No AI code here** — STT,
LLM and TTS are the pipeline's business.
"""

from .frames import BRIDGE_IN_COMMANDS, SiphonEventFrame, siphon_command
from .serializer import SiphonFrameSerializer
from .transport import (
    SiphonHandshakeError,
    SiphonInputTransport,
    SiphonOutputTransport,
    SiphonParams,
    SiphonTransport,
)

__version__ = "0.1.0"

__all__ = [
    "BRIDGE_IN_COMMANDS",
    "SiphonEventFrame",
    "SiphonFrameSerializer",
    "SiphonHandshakeError",
    "SiphonInputTransport",
    "SiphonOutputTransport",
    "SiphonParams",
    "SiphonTransport",
    "siphon_command",
]
