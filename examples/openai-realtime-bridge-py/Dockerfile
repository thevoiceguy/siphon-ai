# OpenAI Realtime bridge image. Mirrors the echo-ws Dockerfile —
# python:3.13-slim, install `websockets`, run as a non-root user.
#
# Build:
#     docker build -t siphon-ai/openai-bridge:dev \
#         examples/openai-realtime-bridge-py/
#
# Run (with your real key):
#     docker run --rm \
#         -e OPENAI_API_KEY=sk-... \
#         -p 8765:8765 \
#         siphon-ai/openai-bridge:dev

FROM python:3.13-slim

WORKDIR /app
COPY requirements.txt ./
RUN pip install --no-cache-dir -r requirements.txt

COPY server.py ./

ARG UID=10001
RUN groupadd --system --gid ${UID} bridge \
 && useradd --system --uid ${UID} --gid bridge --no-create-home bridge
USER bridge

EXPOSE 8765

ENTRYPOINT ["python3", "/app/server.py"]
CMD ["--bind", "0.0.0.0:8765"]
