# Multi-stage Dockerfile for the SiphonAI daemon.
#
# Builds a fully-static `x86_64-unknown-linux-musl` binary on
# Alpine, then drops it into a fresh `alpine:3` runtime carrying
# only ca-certificates. Target size: ~50 MB. The static linkage
# means there are no glibc/musl ABI gotchas at deploy time — the
# image can be replaced with `scratch` or `distroless/static` if
# operators want to shave another 7 MB.
#
# Build:
#     docker build -f docker/Dockerfile -t siphon-ai:dev .
#
# Run (assumes docker-compose.yaml handles network + config):
#     docker run --rm -p 5070:5070/udp siphon-ai:dev

# ─── Build stage ───────────────────────────────────────────────────
# Alpine + the matching Rust toolchain.  The `-alpine` image already
# carries an x86_64-unknown-linux-musl target via the musl libc on
# the system, so `cargo build --target x86_64-unknown-linux-musl`
# Just Works — no rustup target add dance required.
FROM rust:1.85-alpine AS builder

# Musl + the C build tools forge-codecs and friends invoke at
# compile time. `linux-headers` is required by tokio's TCP/UDP code
# paths under musl; `make` is dragged in by a couple of `-sys`
# crates that ship vendored C. We deliberately list each package
# rather than installing `build-base` so the build context shows
# which deps each addition adds.
RUN apk add --no-cache \
        musl-dev \
        linux-headers \
        cmake \
        make \
        g++ \
        pkgconfig \
        perl

WORKDIR /build

# Copy the workspace verbatim. `.dockerignore` at the repo root
# excludes `target/`, `.git/`, etc. so this is small.
COPY . .

# Build the daemon only. `--locked` keeps the build reproducible.
#
# The `RUSTFLAGS` enable static linking against musl explicitly —
# the alpine rust image defaults to it for musl targets but pinning
# the flag here prevents surprises on toolchain upgrades.
#
# `--target x86_64-unknown-linux-musl` produces a binary that runs
# on any Linux kernel ≥ 3.2 without dynamic library deps. `strip`
# drops debug info, halving the binary size.
ENV RUSTFLAGS="-C target-feature=+crt-static"
RUN cargo build \
        --release \
        --locked \
        --target x86_64-unknown-linux-musl \
        -p siphon-ai \
 && strip target/x86_64-unknown-linux-musl/release/siphon-ai


# ─── Runtime stage ─────────────────────────────────────────────────
# Alpine because:
#   * matches the builder's musl — fully-static binary, no surprises
#   * a working /bin/sh for `docker exec` debugging when calls go sideways
#   * `apk add ca-certificates` is one line and ~1 MB
#   * `distroless/static` is a valid drop-in if operators want to
#     shave another ~7 MB at the cost of `docker exec` ergonomics.
FROM alpine:3 AS runtime

RUN apk add --no-cache ca-certificates

# Non-root user. SIP and RTP both bind unprivileged ports by
# default in our config (5070 for SIP, 40000-40100 for RTP), so we
# don't need root at runtime.
ARG UID=10001
RUN addgroup -S -g ${UID} siphon \
 && adduser -S -D -H -u ${UID} -G siphon siphon
USER siphon

WORKDIR /app

# The binary is the entire delivery. Configs are mounted from the
# host or built into a sibling image (see docker/compose.yaml).
COPY --from=builder \
     /build/target/x86_64-unknown-linux-musl/release/siphon-ai \
     /usr/local/bin/siphon-ai

# Default config path. Operators override via `-v ./my.toml:/app/config.toml`.
ENV SIPHON_AI_CONFIG=/app/config.toml

# Document the runtime surface. Compose / k8s overrides as needed.
#   5070/udp   — SIP signaling
#   5070/tcp   — SIP TCP (when transports = ["tcp"])
#   5061/tcp   — SIPS (when transports = ["tls"])
#   9091/tcp   — Prometheus metrics + /health + /ready
#   40000-40100/udp — RTP port pool (matches local-dev.toml default)
EXPOSE 5070/udp 5070/tcp 5061/tcp 9091/tcp 40000-40100/udp

ENTRYPOINT ["/usr/local/bin/siphon-ai"]
CMD ["--config", "/app/config.toml"]
