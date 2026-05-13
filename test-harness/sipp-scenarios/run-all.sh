#!/usr/bin/env bash
#
# Run the SIPp signaling regression suite against a locally-running
# SiphonAI daemon. Designed for tight feedback loops, not CI gating:
# the script starts a fresh daemon on an ephemeral port, runs each
# scenario in series, captures failures, and tears the daemon down.
#
# Prerequisites:
#   * sip-tester (sipp) on PATH        (apt install sip-tester)
#   * SiphonAI binary built             (cargo build -p siphon-ai)
#   * An echo WS server on :8765       (examples/echo-ws-server-python/)
#
# Pass `--with-transfer` to additionally run blind_transfer.xml. The
# runner restarts the echo WS server with `--auto-transfer-target`
# pointing back at SIPp's port, so SiphonAI emits REFER mid-call. The
# auto-transfer mode is incompatible with the other scenarios (which
# don't expect REFER), so it runs in a separate phase.
#
# Each scenario prints OK/FAIL inline; the script exits non-zero on
# any failure.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

SIPP_PORT=5080       # SIPp's listen port (any free port works)
DAEMON_PORT=5070     # SiphonAI's listen port (matches local-dev.toml)
DAEMON_BIN="$REPO_ROOT/target/debug/siphon-ai"
DAEMON_CONFIG="${SIPHON_AI_CONFIG:-$REPO_ROOT/configs/local-dev.toml}"

WITH_TRANSFER=0
for arg in "$@"; do
    case "$arg" in
        --with-transfer) WITH_TRANSFER=1 ;;
        *) echo "unknown arg: $arg" >&2; exit 2 ;;
    esac
done

require() {
    command -v "$1" >/dev/null 2>&1 || {
        echo "missing dependency: $1" >&2
        exit 2
    }
}
require sipp

if [[ ! -x "$DAEMON_BIN" ]]; then
    echo "siphon-ai binary not found at $DAEMON_BIN — run \`cargo build -p siphon-ai\` first" >&2
    exit 2
fi
if [[ ! -f "$DAEMON_CONFIG" ]]; then
    echo "config not found at $DAEMON_CONFIG — set SIPHON_AI_CONFIG or create configs/local-dev.toml" >&2
    exit 2
fi

scenarios=(
    basic_call_then_bye.xml
    caller_cancels_during_setup.xml
    unsupported_codec_488.xml
    session_timer_echo.xml
)

run_scenario() {
    local xml="$1"
    local label
    label=$(basename "$xml" .xml)
    echo "─── $label ───────────────────────────────────────"
    # -m 1     run exactly one call
    # -timeout 10s   hard cap on the whole scenario
    # -trace_err     write *_errors.log next to the xml for debugging
    if sipp -sf "$SCRIPT_DIR/$xml" \
            -m 1 \
            -timeout 10s \
            -trace_err \
            -p "$SIPP_PORT" \
            -s 1000 \
            "127.0.0.1:$DAEMON_PORT" >/dev/null 2>&1; then
        echo "  OK"
    else
        echo "  FAIL (see ${label}_*errors.log)"
        return 1
    fi
}

# Spawn the daemon; keep its log so a failed scenario has something
# to grep.
DAEMON_LOG=$(mktemp -t siphon-ai-sipp.XXXXXX.log)
echo "starting siphon-ai (log: $DAEMON_LOG)"
RUST_LOG=siphon_ai=info "$DAEMON_BIN" --config "$DAEMON_CONFIG" >"$DAEMON_LOG" 2>&1 &
DAEMON_PID=$!
cleanup() {
    if kill -0 "$DAEMON_PID" 2>/dev/null; then
        kill "$DAEMON_PID" 2>/dev/null || true
        wait "$DAEMON_PID" 2>/dev/null || true
    fi
}
trap cleanup EXIT

# Give the daemon a beat to bind. A real wait-for-port helper would
# be tidier; for a regression script this is fine.
sleep 1

failures=0
total=${#scenarios[@]}
for s in "${scenarios[@]}"; do
    run_scenario "$s" || failures=$((failures + 1))
done

# ─── Optional second phase: blind_transfer ────────────────────────
# Needs a WS server that proactively emits BridgeIn::Transfer. The
# runner stops the daemon, brings up an echo-ws that auto-emits
# transfer pointing back at the SIPp port, then restarts the daemon
# pointing at it. Skipped when the operator hasn't asked for it
# because it requires a free auxiliary port (8766) and a 1-2s
# restart pause.
if (( WITH_TRANSFER )); then
    echo
    echo "─── auxiliary phase: blind_transfer ───────────────────"
    # Stop the existing daemon — it'd otherwise hold :5070.
    cleanup
    trap - EXIT

    AUX_WS_PORT=8766
    AUX_WS_LOG=$(mktemp -t echo-ws-aux.XXXXXX.log)
    AUX_DAEMON_LOG=$(mktemp -t siphon-ai-aux.XXXXXX.log)
    AUX_CONFIG=$(mktemp -t siphon-ai-aux.XXXXXX.toml)
    cat >"$AUX_CONFIG" <<EOF
[node]
id = "siphon-ai-sipp-aux"
[sip]
listen = "127.0.0.1:$DAEMON_PORT"
[media]
codecs = ["pcmu"]
[bridge]
ws_url = "ws://127.0.0.1:$AUX_WS_PORT/"
[[route]]
name = "default"
[route.match]
any = true
EOF

    "$REPO_ROOT/examples/echo-ws-server-python/.venv/bin/python" \
        "$REPO_ROOT/examples/echo-ws-server-python/server.py" \
        --bind "127.0.0.1:$AUX_WS_PORT" \
        --auto-transfer-target "sip:7000@127.0.0.1:$SIPP_PORT" \
        --auto-transfer-delay-ms 200 \
        >"$AUX_WS_LOG" 2>&1 &
    AUX_WS_PID=$!

    RUST_LOG=siphon_ai=info "$DAEMON_BIN" --config "$AUX_CONFIG" \
        >"$AUX_DAEMON_LOG" 2>&1 &
    AUX_DAEMON_PID=$!
    aux_cleanup() {
        kill "$AUX_WS_PID" "$AUX_DAEMON_PID" 2>/dev/null || true
        wait "$AUX_WS_PID" "$AUX_DAEMON_PID" 2>/dev/null || true
    }
    trap aux_cleanup EXIT
    sleep 1.2

    total=$((total + 1))
    run_scenario blind_transfer.xml || failures=$((failures + 1))

    aux_cleanup
    trap - EXIT
fi

echo
if (( failures == 0 )); then
    echo "all $total scenarios passed"
    exit 0
fi
echo "$failures of $total scenarios failed"
echo "daemon log: $DAEMON_LOG"
exit 1
