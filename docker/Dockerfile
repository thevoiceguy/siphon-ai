# Multi-stage Dockerfile for the SiphonAI daemon.
#
# Builds the `siphon-ai` binary against a pinned Rust toolchain, then
# copies it into a minimal Debian-slim runtime. The runtime image
# carries only what's needed to execute the binary — no compiler,
# no source — and is the artifact operators deploy.
#
# Target size: ~50 MB compressed, ~120 MB extracted. The bulk is
# debian-slim's base layer (~30 MB) and the static-ish daemon
# binary (~80 MB unstripped). Strip in a follow-up if size pressure
# materializes; for v0.1.0 we lean on `cargo build --release`'s
# defaults to keep stack traces readable.
#
# Build:
#     docker build -f docker/Dockerfile -t siphon-ai:dev .
#
# Run (assumes docker-compose.yaml handles network + config):
#     docker run --rm -p 5070:5070/udp siphon-ai:dev

# ─── Build stage ───────────────────────────────────────────────────
# Pin the Rust toolchain to match `rust-version` in workspace
# Cargo.toml. Bookworm matches the runtime stage so libc / glibc
# ABIs line up.
FROM rust:1.85-bookworm AS builder

# Cache mount-friendly dep install. The bookworm rust image already
# ships build tools; we add only what siphon-rs / forge-media
# transitively need:
#   * pkg-config + libssl-dev: occasional indirect crates (NOT for
#     the daemon's TLS — that path uses rustls — but bcg729-sys and
#     forge-codecs may invoke pkg-config during build).
#   * cmake: forge-codecs uses cmake for some bundled native libs.
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
        pkg-config libssl-dev cmake \
 && rm -rf /var/lib/apt/lists/*

WORKDIR /build

# Copy the workspace verbatim. We don't try to cache the dep build
# separately (the usual Cargo.toml-only trick) because workspace
# member discovery is recursive and brittle to fake out; cargo's
# fetch cache + Docker's layer cache get us most of the speed win
# without the maintenance cost.
COPY . .

# Build the daemon only. `--locked` keeps the build reproducible —
# if Cargo.lock is missing or out-of-date the build fails loudly
# instead of silently bumping deps. CI builds run this way too.
RUN cargo build --release --locked -p siphon-ai \
 && strip target/release/siphon-ai


# ─── Runtime stage ─────────────────────────────────────────────────
# Debian-slim because:
#   * matches the builder's glibc — no surprise ABI breaks
#   * `ca-certificates` for HTTPS lookups (webhook sink, future
#     hep-rs TLS transport)
#   * smaller than `debian:bookworm` (~30 MB vs ~115 MB) and far
#     better-supported than `distroless` for occasional sh-into
#     debugging
FROM debian:bookworm-slim AS runtime

# Pinned minimal runtime deps. Adding to this list is a one-PR
# event — keep it tight.
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
        ca-certificates \
 && rm -rf /var/lib/apt/lists/*

# Non-root user. SIP and RTP both bind unprivileged ports by
# default in our config (5070 for SIP, 40000-40100 for RTP), so we
# don't need root at runtime.
ARG UID=10001
RUN groupadd --system --gid ${UID} siphon \
 && useradd --system --uid ${UID} --gid siphon --no-create-home siphon
USER siphon

WORKDIR /app

# The binary is the entire delivery. Configs are mounted from the
# host or built into a sibling image (see docker/compose.yaml).
COPY --from=builder /build/target/release/siphon-ai /usr/local/bin/siphon-ai

# Default config path. Operators override via `-v ./my.toml:/app/config.toml`.
ENV SIPHON_AI_CONFIG=/app/config.toml

# Document the runtime surface. Compose / k8s overrides as needed.
#   5070/udp   — SIP signaling
#   5070/tcp   — SIP TCP (when transports = ["tcp"])
#   5061/tcp   — SIPS (when transports = ["tls"])
#   9091/tcp   — Prometheus metrics + /health + /ready
#   40000-40100/udp — RTP port pool (matches local-dev.toml default)
EXPOSE 5070/udp 5070/tcp 5061/tcp 9091/tcp 40000-40100/udp

# `--config` is positional in main.rs — keep the entrypoint thin so
# operators can append `--help` etc. without fighting the wrapper.
ENTRYPOINT ["/usr/local/bin/siphon-ai"]
CMD ["--config", "/app/config.toml"]
