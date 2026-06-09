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

# sipp's `-trace_err` writes <scenario>_<pid>_errors.log to the CWD,
# with no flag to redirect. Pin the CWD to the scenarios directory
# so the path the "(see ${label}_*errors.log)" hint refers to is
# stable regardless of where the script was invoked from — and so
# the CI workflow's failure-dump step has a predictable glob target.
cd "$SCRIPT_DIR"

SIPP_PORT=5080       # SIPp's listen port (any free port works)
DAEMON_PORT=5070     # SiphonAI's listen port (matches local-dev.toml)
DAEMON_BIN="${DAEMON_BIN:-$REPO_ROOT/target/debug/siphon-ai}"
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
    reinvite_hold_resume.xml
    reinvite_unsupported_codec_488.xml
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

# ─── Always-on auxiliary phase: session_progress ─────────────────
# Verifies `[sip.call_progress] mode = "session_progress"` produces a
# 183 with the negotiated answer SDP before the 200 OK (the §4.1
# deliverable from the 0.2.0 plan). The main scenarios run against
# the default config (`instant_answer` in `configs/local-dev.toml`),
# so this phase stops that daemon and brings up a fresh one with a
# session_progress config on the same port.
echo
echo "─── auxiliary phase: session_progress ────────────────"
cleanup
trap - EXIT

SP_DAEMON_LOG=$(mktemp -t siphon-ai-sp.XXXXXX.log)
SP_CONFIG=$(mktemp -t siphon-ai-sp.XXXXXX.toml)
cat >"$SP_CONFIG" <<EOF
[node]
id = "siphon-ai-sipp-sp"
[sip]
listen = "127.0.0.1:$DAEMON_PORT"
[sip.call_progress]
mode = "session_progress"
[media]
codecs = ["pcmu"]
[bridge]
ws_url = "ws://127.0.0.1:8765/"
[[route]]
name = "default"
[route.match]
any = true
EOF

RUST_LOG=siphon_ai=info "$DAEMON_BIN" --config "$SP_CONFIG" \
    >"$SP_DAEMON_LOG" 2>&1 &
SP_DAEMON_PID=$!
sp_cleanup() {
    if kill -0 "$SP_DAEMON_PID" 2>/dev/null; then
        kill "$SP_DAEMON_PID" 2>/dev/null || true
        wait "$SP_DAEMON_PID" 2>/dev/null || true
    fi
}
trap sp_cleanup EXIT
sleep 1.2

total=$((total + 1))
run_scenario session_progress_then_answer.xml || failures=$((failures + 1))

sp_cleanup
trap - EXIT

# ─── Always-on auxiliary phase: STIR/SHAKEN ──────────────────────
# Exercises the accept-path verifier + gate end-to-end:
#   * no Identity header + require_identity        → 428 (reject)
#   * Identity present but unverifiable + min "A"  → 403 (reject)
#   * fully-verifiable Identity + min "A"          → 200 (admitted)
#
# The passing case needs a real, current rig: the `gen_test_passport`
# example mints a throwaway CA, a leaf signing cert, an x5u TLS server
# cert (SAN 127.0.0.1), and a freshly-signed PASSporT whose `iat` is now.
# A local HTTPS server serves the leaf at the x5u URL; the daemon trusts
# the CA both as the STI-PA anchor (chain) and via `x5u_tls_extra_ca`
# (fetch TLS). The 428/403 rejects are pre-media and don't depend on the
# rig, so all three share one daemon config.
echo
echo "─── auxiliary phase: stir_shaken ──────────────────────"
SS_DAEMON_LOG=$(mktemp -t siphon-ai-ss.XXXXXX.log)
SS_CONFIG=$(mktemp -t siphon-ai-ss.XXXXXX.toml)
SS_RIG=$(mktemp -d -t siphon-ai-ss-rig.XXXXXX)
SS_X5U_PORT=8443
SS_X5U_LOG=$(mktemp -t siphon-ai-ss-x5u.XXXXXX.log)

# Build + run the rig generator (reuses the stir-shaken crate's dev-deps).
echo "generating STIR/SHAKEN test rig …"
cargo build -q -p siphon-ai-stir-shaken --example gen_test_passport
SS_IDENTITY=$("$REPO_ROOT/target/debug/examples/gen_test_passport" \
    "$SS_RIG" "https://127.0.0.1:$SS_X5U_PORT/leaf.crt" "+12155551212" "1000")

# Serve the leaf cert over HTTPS with the rig's TLS server cert. Stdlib
# only (http.server + ssl) — no pip deps. Backgrounded; chdir's into the
# rig dir so GET /leaf.crt returns the leaf certificate.
python3 - "$SS_X5U_PORT" "$SS_RIG" >"$SS_X5U_LOG" 2>&1 <<'PY' &
import http.server, ssl, sys, os
port = int(sys.argv[1]); d = sys.argv[2]
os.chdir(d)
ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
ctx.load_cert_chain(os.path.join(d, "server.crt"), os.path.join(d, "server.key"))
httpd = http.server.HTTPServer(("127.0.0.1", port), http.server.SimpleHTTPRequestHandler)
httpd.socket = ctx.wrap_socket(httpd.socket, server_side=True)
httpd.serve_forever()
PY
SS_X5U_PID=$!

cat >"$SS_CONFIG" <<EOF
[node]
id = "siphon-ai-sipp-ss"
[sip]
listen = "127.0.0.1:$DAEMON_PORT"
[media]
codecs = ["pcmu"]
[bridge]
ws_url = "ws://127.0.0.1:8765/"
[security]
min_attestation = "A"
[security.stir_shaken]
enabled = true
trust_anchors = "$SS_RIG/ca.pem"
x5u_tls_extra_ca = "$SS_RIG/ca.pem"
require_identity = true
[[route]]
name = "default"
[route.match]
any = true
EOF

RUST_LOG=siphon_ai=info "$DAEMON_BIN" --config "$SS_CONFIG" \
    >"$SS_DAEMON_LOG" 2>&1 &
SS_DAEMON_PID=$!
ss_cleanup() {
    kill "$SS_DAEMON_PID" "$SS_X5U_PID" 2>/dev/null || true
    wait "$SS_DAEMON_PID" 2>/dev/null || true
}
trap ss_cleanup EXIT
sleep 1.2

# Substitute the freshly-minted Identity into the passing scenario.
SS_PASS_XML=$(mktemp -t siphon-ai-ss-pass.XXXXXX.xml)
sed "s|__IDENTITY__|$SS_IDENTITY|" \
    "$SCRIPT_DIR/stir_shaken_attestation_pass.xml" >"$SS_PASS_XML"

total=$((total + 3))
run_scenario stir_shaken_no_identity_428.xml || failures=$((failures + 1))
run_scenario stir_shaken_attestation_403.xml || failures=$((failures + 1))
# Run the (generated) passing scenario by absolute path.
echo "─── stir_shaken_attestation_pass ─────────────────────"
if sipp -sf "$SS_PASS_XML" -m 1 -timeout 15s -trace_err \
        -p "$SIPP_PORT" -s 1000 "127.0.0.1:$DAEMON_PORT" >/dev/null 2>&1; then
    echo "  OK"
else
    echo "  FAIL (see stir_shaken_attestation_pass_*errors.log; daemon: $SS_DAEMON_LOG)"
    failures=$((failures + 1))
fi

ss_cleanup
trap - EXIT

# ─── Always-on auxiliary phase: recording ─────────────────────────
# Verifies `[recording].mode = "always"` writes a valid stereo WAV. A
# fresh daemon records to a temp dir; after one basic call we assert the
# file exists and is a well-formed stereo PCM16 WAV with audio in it.
# (A signaling-only call records silence frames over the call's duration,
# which still exercises the whole tap → writer → finalize path.) Reuses
# the echo WS on :8765 that the rest of the suite already needs.
echo
echo "─── auxiliary phase: recording ────────────────────────"
REC_DAEMON_LOG=$(mktemp -t siphon-ai-rec.XXXXXX.log)
REC_CONFIG=$(mktemp -t siphon-ai-rec.XXXXXX.toml)
REC_DIR=$(mktemp -d -t siphon-ai-rec.XXXXXX)
cat >"$REC_CONFIG" <<EOF
[node]
id = "siphon-ai-sipp-rec"
[sip]
listen = "127.0.0.1:$DAEMON_PORT"
[media]
codecs = ["pcmu"]
[bridge]
ws_url = "ws://127.0.0.1:8765/"
[recording]
mode = "always"
dir = "$REC_DIR"
[[route]]
name = "default"
[route.match]
any = true
EOF

RUST_LOG=siphon_ai=info "$DAEMON_BIN" --config "$REC_CONFIG" \
    >"$REC_DAEMON_LOG" 2>&1 &
REC_DAEMON_PID=$!
rec_cleanup() {
    kill "$REC_DAEMON_PID" 2>/dev/null || true
    wait "$REC_DAEMON_PID" 2>/dev/null || true
    rm -rf "$REC_DIR" 2>/dev/null || true
}
trap rec_cleanup EXIT
sleep 1.2

total=$((total + 1))
echo "─── recording_writes_valid_wav ───────────────────────"
rec_ok=0
if sipp -sf "$SCRIPT_DIR/basic_call_then_bye.xml" -m 1 -timeout 15s -trace_err \
        -p "$SIPP_PORT" -s 1000 "127.0.0.1:$DAEMON_PORT" >/dev/null 2>&1; then
    sleep 0.6  # let teardown finalize the WAV header
    if python3 - "$REC_DIR" <<'PY'
import sys, glob, wave
wavs = glob.glob(sys.argv[1] + "/*.wav")
assert len(wavs) == 1, f"expected exactly 1 recording, found {len(wavs)}"
w = wave.open(wavs[0], "rb")
assert w.getnchannels() == 2, f"expected stereo, got {w.getnchannels()}"
assert w.getsampwidth() == 2, "expected PCM16"
assert w.getframerate() in (8000, 16000), f"unexpected rate {w.getframerate()}"
assert w.getnframes() > 0, "recording is empty"
PY
    then rec_ok=1; fi
fi
if (( rec_ok )); then
    echo "  OK"
else
    echo "  FAIL (recording invalid or call failed; daemon: $REC_DAEMON_LOG)"
    failures=$((failures + 1))
fi

rec_cleanup
trap - EXIT

# ─── Always-on auxiliary phase: outbound origination ──────────────
# Roles inverted: SIPp is the CALLEE (UAS), SiphonAI the UAC. A fresh
# daemon comes up with `[outbound]` enabled and a `[[gateway]]`
# pointing at SIPp's port; the runner POSTs /admin/v1/calls, SIPp
# answers (180 → 200 + SDP), the WS bridge runs against a dedicated
# echo-ws instance that auto-emits `hangup` after ~1.5s, and SiphonAI
# BYEs the dialog. This is the live SIP answer-path test for 0.6.0
# (everything below the originate endpoint ran only against unit
# tests until now). Needs its own echo-ws because the auto-hangup
# knob is incompatible with the long-lived calls other phases expect.
echo
echo "─── auxiliary phase: outbound ─────────────────────────"
OB_WS_PORT=8767
OB_ADMIN_PORT=9091
OB_WS_LOG=$(mktemp -t echo-ws-ob.XXXXXX.log)
OB_DAEMON_LOG=$(mktemp -t siphon-ai-ob.XXXXXX.log)
OB_CONFIG=$(mktemp -t siphon-ai-ob.XXXXXX.toml)
cat >"$OB_CONFIG" <<EOF
[node]
id = "siphon-ai-sipp-ob"
[sip]
listen = "127.0.0.1:$DAEMON_PORT"
[media]
codecs = ["pcmu"]
[bridge]
ws_url = "ws://127.0.0.1:$OB_WS_PORT/"
[observability]
enabled = true
http_listen = "127.0.0.1:$OB_ADMIN_PORT"
[outbound]
max_concurrent = 2
[[gateway]]
name = "sipp"
proxy = "127.0.0.1:$SIPP_PORT"
from = "sip:harness@127.0.0.1"
[[route]]
name = "default"
[route.match]
any = true
EOF

# Dedicated echo-ws with the auto-hangup harness knob. Prefer the
# venv the CI workflow preps (same as the transfer phase); fall back
# to system python3 for local runs with `websockets` installed.
OB_PYTHON="$REPO_ROOT/examples/echo-ws-server-python/.venv/bin/python"
[[ -x "$OB_PYTHON" ]] || OB_PYTHON=python3
"$OB_PYTHON" "$REPO_ROOT/examples/echo-ws-server-python/server.py" \
    --bind "127.0.0.1:$OB_WS_PORT" \
    --auto-hangup-after-ms 1500 \
    >"$OB_WS_LOG" 2>&1 &
OB_WS_PID=$!

RUST_LOG=siphon_ai=info "$DAEMON_BIN" --config "$OB_CONFIG" \
    >"$OB_DAEMON_LOG" 2>&1 &
OB_DAEMON_PID=$!
ob_cleanup() {
    kill "$OB_WS_PID" "$OB_DAEMON_PID" 2>/dev/null || true
    wait "$OB_WS_PID" "$OB_DAEMON_PID" 2>/dev/null || true
}
trap ob_cleanup EXIT
sleep 1.2

total=$((total + 1))
echo "─── outbound_uas_answer ──────────────────────────────"
ob_ok=0
# SIPp listens as the callee; no remote target needed until we make
# it ring. -bg would detach past our PID bookkeeping, so plain &.
sipp -sf "$SCRIPT_DIR/outbound_uas_answer.xml" \
    -m 1 -timeout 15s -trace_err -p "$SIPP_PORT" >/dev/null 2>&1 &
OB_SIPP_PID=$!
sleep 0.3

# Place the call. 202 + a call_id means admitted; the rest plays out
# between the daemon, SIPp, and the echo-ws hangup.
ob_resp=$(curl -s -o /dev/null -w "%{http_code}" \
    -X POST "http://127.0.0.1:$OB_ADMIN_PORT/admin/v1/calls" \
    -d '{"to": "7001", "gateway": "sipp"}')
if [[ "$ob_resp" == "202" ]] && wait "$OB_SIPP_PID"; then
    # SIPp saw INVITE → ACK → BYE. Cross-check the daemon agrees the
    # call was ANSWERED (metric from chunk 5a).
    if curl -s "http://127.0.0.1:$OB_ADMIN_PORT/metrics" \
        | grep -q 'siphon_ai_outbound_calls_total{result="answered"} 1'; then
        ob_ok=1
    fi
fi
if (( ob_ok )); then
    echo "  OK"
else
    echo "  FAIL (originate=$ob_resp; daemon: $OB_DAEMON_LOG; ws: $OB_WS_LOG)"
    failures=$((failures + 1))
fi

ob_cleanup
trap - EXIT

# ─── Optional third phase: blind_transfer ─────────────────────────
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
