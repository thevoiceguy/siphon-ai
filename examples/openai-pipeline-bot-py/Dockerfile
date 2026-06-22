# OpenAI cascaded voice-bot image. Mirrors the other Python examples —
# python:3.13-slim, install deps, run as a non-root user.
#
# Build:
#     docker build -t siphon-ai/openai-pipeline-bot:dev \
#         examples/openai-pipeline-bot-py/
#
# Run (with your real key):
#     docker run --rm \
#         -e OPENAI_API_KEY=sk-... \
#         -p 8080:8080 \
#         siphon-ai/openai-pipeline-bot:dev

FROM python:3.13-slim

WORKDIR /app
COPY requirements.txt ./
RUN pip install --no-cache-dir -r requirements.txt

COPY server.py ./

ARG UID=10001
RUN groupadd --system --gid ${UID} bot \
 && useradd --system --uid ${UID} --gid bot --no-create-home bot
USER bot

EXPOSE 8080

ENTRYPOINT ["python3", "/app/server.py"]
CMD ["--bind", "0.0.0.0:8080"]
