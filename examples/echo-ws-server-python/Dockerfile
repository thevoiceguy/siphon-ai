# Reference echo WS server image. Tiny: python:3.13-slim + one
# Python dep (`websockets`). Used by `docker/compose.yaml` as the
# canary WS endpoint SiphonAI dials into.
#
# Build:
#     docker build -t siphon-ai/echo-ws:dev examples/echo-ws-server-python/
#
# Run:
#     docker run --rm -p 8765:8765 siphon-ai/echo-ws:dev \
#         --bind 0.0.0.0:8765

FROM python:3.13-slim

# requirements.txt is one line (`websockets>=13`) — install at
# build time, not at container start, so cold starts are fast.
WORKDIR /app
COPY requirements.txt ./
RUN pip install --no-cache-dir -r requirements.txt

COPY server.py ./

# Non-root user. The server binds 8765 (unprivileged) so we don't
# need any caps.
ARG UID=10001
RUN groupadd --system --gid ${UID} echo \
 && useradd --system --uid ${UID} --gid echo --no-create-home echo
USER echo

EXPOSE 8765

ENTRYPOINT ["python3", "/app/server.py"]
CMD ["--bind", "0.0.0.0:8765"]
