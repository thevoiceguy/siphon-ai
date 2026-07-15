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
# Admin API (0.10.0): /admin/* moved to its own authenticated listener,
# separate from the open [observability] /metrics port. Phases that drive
# the admin API ([admin] block + these on a bearer token); phases run
# sequentially so one fixed port/token is reused.
ADMIN_API_PORT=9191
ADMIN_TOKEN="sipp-harness-admin"
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
    # -i 127.0.0.1   pin [local_ip] to IPv4 loopback — on dual-stack hosts
    #                sipp may resolve ::1, advertising an IPv6 Contact that
    #                the IPv4-bound daemon can't reach (in-dialog BYE then
    #                fails with a transport error and UAS scenarios hang)
    if sipp -i 127.0.0.1 -sf "$SCRIPT_DIR/$xml" \
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
if sipp -i 127.0.0.1 -sf "$SS_PASS_XML" -m 1 -timeout 15s -trace_err \
        -p "$SIPP_PORT" -s 1000 "127.0.0.1:$DAEMON_PORT" >/dev/null 2>&1; then
    echo "  OK"
else
    echo "  FAIL (see stir_shaken_attestation_pass_*errors.log; daemon: $SS_DAEMON_LOG)"
    failures=$((failures + 1))
fi

ss_cleanup
trap - EXIT

# ─── Always-on auxiliary phase: inbound digest auth ───────────────
# Exercises [sip.auth] end-to-end: SIPp sends an INVITE with no
# credentials, gets a 401 + WWW-Authenticate, retries with the digest
# Authorization for carrier-a, and is admitted (200 → ACK → BYE). A
# fresh daemon runs with [sip.auth] enabled; no trunks are declared, so
# every INVITE is challenged (legacy-mode require_all).
echo
echo "─── auxiliary phase: digest_auth ──────────────────────"
DA_DAEMON_LOG=$(mktemp -t siphon-ai-da.XXXXXX.log)
DA_CONFIG=$(mktemp -t siphon-ai-da.XXXXXX.toml)
cat >"$DA_CONFIG" <<EOF
[node]
id = "siphon-ai-sipp-da"
[sip]
listen = "127.0.0.1:$DAEMON_PORT"
# MD5 here because SIPp's [authentication] digest client only computes
# MD5 reliably; the SHA-256 verify path is covered by the sip-glue unit
# round-trip test. Both share the same challenge/verify code.
[sip.auth]
enabled = true
realm = "siphon.example"
algorithm = "MD5"
[[sip.auth.user]]
username = "carrier-a"
password = "s3cret-sipp"
[media]
codecs = ["pcmu"]
[bridge]
ws_url = "ws://127.0.0.1:8765/"
[[route]]
name = "default"
[route.match]
any = true
EOF

RUST_LOG=siphon_ai=info "$DAEMON_BIN" --config "$DA_CONFIG" \
    >"$DA_DAEMON_LOG" 2>&1 &
DA_DAEMON_PID=$!
da_cleanup() {
    kill "$DA_DAEMON_PID" 2>/dev/null || true
    wait "$DA_DAEMON_PID" 2>/dev/null || true
}
trap da_cleanup EXIT
sleep 1.2

total=$((total + 1))
run_scenario digest_auth_caller.xml || failures=$((failures + 1))

da_cleanup
trap - EXIT

# ─── Always-on auxiliary phase: INVITE admission control ──────────
# Exercises [sip.admission] per-source rate limiting end-to-end with
# max_per_sec = 1, burst = 1: a warmup call consumes the single token
# (and confirms admission lets a normal call through), then a second
# INVITE from the same source — well within the 1 s refill window — is
# rejected with 503. A fresh daemon runs the tight config; reuses the
# echo WS.
echo
echo "─── auxiliary phase: admission ────────────────────────"
AD_DAEMON_LOG=$(mktemp -t siphon-ai-ad.XXXXXX.log)
AD_CONFIG=$(mktemp -t siphon-ai-ad.XXXXXX.toml)
cat >"$AD_CONFIG" <<EOF
[node]
id = "siphon-ai-sipp-ad"
[sip]
listen = "127.0.0.1:$DAEMON_PORT"
[sip.admission]
max_per_sec = 1
burst = 1
[media]
codecs = ["pcmu"]
[bridge]
ws_url = "ws://127.0.0.1:8765/"
[[route]]
name = "default"
[route.match]
any = true
EOF

RUST_LOG=siphon_ai=info "$DAEMON_BIN" --config "$AD_CONFIG" \
    >"$AD_DAEMON_LOG" 2>&1 &
AD_DAEMON_PID=$!
ad_cleanup() {
    kill "$AD_DAEMON_PID" 2>/dev/null || true
    wait "$AD_DAEMON_PID" 2>/dev/null || true
}
trap ad_cleanup EXIT
sleep 1.2

total=$((total + 2))
run_scenario admission_warmup.xml || failures=$((failures + 1))
run_scenario admission_invite_503.xml || failures=$((failures + 1))

ad_cleanup
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
if sipp -i 127.0.0.1 -sf "$SCRIPT_DIR/basic_call_then_bye.xml" -m 1 -timeout 15s -trace_err \
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
[admin]
listen = "127.0.0.1:$ADMIN_API_PORT"
[[admin.token]]
name = "harness"
token = "$ADMIN_TOKEN"
role = "admin"
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
sipp -i 127.0.0.1 -sf "$SCRIPT_DIR/outbound_uas_answer.xml" \
    -m 1 -timeout 15s -trace_err -p "$SIPP_PORT" >/dev/null 2>&1 &
OB_SIPP_PID=$!
sleep 0.3

# Place the call. 202 + a call_id means admitted; the rest plays out
# between the daemon, SIPp, and the echo-ws hangup.
ob_resp=$(curl -s -o /dev/null -w "%{http_code}" \
    -X POST -H "Authorization: Bearer $ADMIN_TOKEN" "http://127.0.0.1:$ADMIN_API_PORT/admin/v1/calls" \
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

# ─── Always-on auxiliary phase: outbound delayed offer ────────────
# (0.9.0 chunk 2) Same roles-inverted setup as `outbound`, but the
# runner POSTs with `delayed_offer: true`: SiphonAI sends an offerless
# INVITE, SIPp answers 200 with its own SDP OFFER, and SiphonAI's
# gateway UAC must put the SDP ANSWER in the ACK. The scenario's
# check_it asserts that ACK answer; pass also requires the daemon to
# report the call ANSWERED.
echo
echo "─── auxiliary phase: outbound_delayed ─────────────────"
ODO_WS_PORT=8783
ODO_ADMIN_PORT=9097
ODO_WS_LOG=$(mktemp -t echo-ws-odo.XXXXXX.log)
ODO_DAEMON_LOG=$(mktemp -t siphon-ai-odo.XXXXXX.log)
ODO_CONFIG=$(mktemp -t siphon-ai-odo.XXXXXX.toml)
cat >"$ODO_CONFIG" <<EOF
[node]
id = "siphon-ai-sipp-odo"
[sip]
listen = "127.0.0.1:$DAEMON_PORT"
[media]
codecs = ["pcmu"]
[bridge]
ws_url = "ws://127.0.0.1:$ODO_WS_PORT/"
[observability]
enabled = true
http_listen = "127.0.0.1:$ODO_ADMIN_PORT"
[admin]
listen = "127.0.0.1:$ADMIN_API_PORT"
[[admin.token]]
name = "harness"
token = "$ADMIN_TOKEN"
role = "admin"
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

ODO_PYTHON="$REPO_ROOT/examples/echo-ws-server-python/.venv/bin/python"
[[ -x "$ODO_PYTHON" ]] || ODO_PYTHON=python3
"$ODO_PYTHON" "$REPO_ROOT/examples/echo-ws-server-python/server.py" \
    --bind "127.0.0.1:$ODO_WS_PORT" \
    --auto-hangup-after-ms 1500 \
    >"$ODO_WS_LOG" 2>&1 &
ODO_WS_PID=$!

RUST_LOG=siphon_ai=info "$DAEMON_BIN" --config "$ODO_CONFIG" \
    >"$ODO_DAEMON_LOG" 2>&1 &
ODO_DAEMON_PID=$!
odo_cleanup() {
    kill "$ODO_WS_PID" "$ODO_DAEMON_PID" 2>/dev/null || true
    wait "$ODO_WS_PID" "$ODO_DAEMON_PID" 2>/dev/null || true
}
trap odo_cleanup EXIT
sleep 1.2

total=$((total + 1))
echo "─── outbound_delayed_uas ─────────────────────────────"
odo_ok=0
sipp -i 127.0.0.1 -sf "$SCRIPT_DIR/outbound_delayed_uas.xml" \
    -m 1 -timeout 15s -trace_err -p "$SIPP_PORT" >/dev/null 2>&1 &
ODO_SIPP_PID=$!
sleep 0.3
odo_resp=$(curl -s -o /dev/null -w "%{http_code}" \
    -X POST -H "Authorization: Bearer $ADMIN_TOKEN" "http://127.0.0.1:$ADMIN_API_PORT/admin/v1/calls" \
    -d '{"to": "7001", "gateway": "sipp", "delayed_offer": true}')
if [[ "$odo_resp" == "202" ]] && wait "$ODO_SIPP_PID"; then
    if curl -s "http://127.0.0.1:$ODO_ADMIN_PORT/metrics" \
        | grep -q 'siphon_ai_outbound_calls_total{result="answered"} 1'; then
        odo_ok=1
    fi
fi
if (( odo_ok )); then
    echo "  OK"
else
    echo "  FAIL (originate=$odo_resp; daemon: $ODO_DAEMON_LOG; ws: $ODO_WS_LOG)"
    failures=$((failures + 1))
fi

odo_cleanup
trap - EXIT

# ─── Always-on auxiliary phase: outbound delayed offer + SRTP ─────
# (0.9.1) Outbound delayed offer where the peer offers SDES SRTP in its
# 2xx and SiphonAI answers it in the ACK. The `[[gateway]].srtp =
# "required"` makes the answer mandatory. The scenario's check_it
# asserts the ACK carries `a=crypto`; pass also requires the daemon to
# report the call ANSWERED **and** the SRTP result `encrypted`.
echo
echo "─── auxiliary phase: outbound_delayed_srtp ────────────"
ODS_WS_PORT=8785
ODS_ADMIN_PORT=9098
ODS_WS_LOG=$(mktemp -t echo-ws-ods.XXXXXX.log)
ODS_DAEMON_LOG=$(mktemp -t siphon-ai-ods.XXXXXX.log)
ODS_CONFIG=$(mktemp -t siphon-ai-ods.XXXXXX.toml)
cat >"$ODS_CONFIG" <<EOF
[node]
id = "siphon-ai-sipp-ods"
[sip]
listen = "127.0.0.1:$DAEMON_PORT"
[media]
codecs = ["pcmu"]
[bridge]
ws_url = "ws://127.0.0.1:$ODS_WS_PORT/"
[observability]
enabled = true
http_listen = "127.0.0.1:$ODS_ADMIN_PORT"
[admin]
listen = "127.0.0.1:$ADMIN_API_PORT"
[[admin.token]]
name = "harness"
token = "$ADMIN_TOKEN"
role = "admin"
[outbound]
max_concurrent = 2
[[gateway]]
name = "sipp"
proxy = "127.0.0.1:$SIPP_PORT"
from = "sip:harness@127.0.0.1"
srtp = "required"
[[route]]
name = "default"
[route.match]
any = true
EOF

ODS_PYTHON="$REPO_ROOT/examples/echo-ws-server-python/.venv/bin/python"
[[ -x "$ODS_PYTHON" ]] || ODS_PYTHON=python3
"$ODS_PYTHON" "$REPO_ROOT/examples/echo-ws-server-python/server.py" \
    --bind "127.0.0.1:$ODS_WS_PORT" \
    --auto-hangup-after-ms 1500 \
    >"$ODS_WS_LOG" 2>&1 &
ODS_WS_PID=$!

RUST_LOG=siphon_ai=info "$DAEMON_BIN" --config "$ODS_CONFIG" \
    >"$ODS_DAEMON_LOG" 2>&1 &
ODS_DAEMON_PID=$!
ods_cleanup() {
    kill "$ODS_WS_PID" "$ODS_DAEMON_PID" 2>/dev/null || true
    wait "$ODS_WS_PID" "$ODS_DAEMON_PID" 2>/dev/null || true
}
trap ods_cleanup EXIT
sleep 1.2

total=$((total + 1))
echo "─── outbound_delayed_srtp_uas ────────────────────────"
ods_ok=0
sipp -i 127.0.0.1 -sf "$SCRIPT_DIR/outbound_delayed_srtp_uas.xml" \
    -m 1 -timeout 15s -trace_err -p "$SIPP_PORT" >/dev/null 2>&1 &
ODS_SIPP_PID=$!
sleep 0.3
ods_resp=$(curl -s -o /dev/null -w "%{http_code}" \
    -X POST -H "Authorization: Bearer $ADMIN_TOKEN" "http://127.0.0.1:$ADMIN_API_PORT/admin/v1/calls" \
    -d '{"to": "7001", "gateway": "sipp", "delayed_offer": true}')
if [[ "$ods_resp" == "202" ]] && wait "$ODS_SIPP_PID"; then
    if curl -s "http://127.0.0.1:$ODS_ADMIN_PORT/metrics" \
        | grep -q 'siphon_ai_outbound_srtp_total{result="encrypted"} 1'; then
        ods_ok=1
    fi
fi
if (( ods_ok )); then
    echo "  OK"
else
    echo "  FAIL (originate=$ods_resp; daemon: $ODS_DAEMON_LOG; ws: $ODS_WS_LOG)"
    failures=$((failures + 1))
fi

ods_cleanup
trap - EXIT

# ─── Always-on auxiliary phase: outbound delayed offer + DTLS ─────
# (0.9.3) Outbound delayed offer where the peer offers DTLS-SRTP
# (UDP/TLS/RTP/SAVPF + a=fingerprint + a=setup:actpass) in its 2xx and
# SiphonAI answers it in the ACK. The scenario's check_it asserts the
# ACK carries `a=fingerprint:sha-256`; pass also requires the daemon to
# report ANSWERED + the SRTP result `encrypted`. (SIPp doesn't run a
# real DTLS handshake — signalling-only; forge unit-tests the media.)
echo
echo "─── auxiliary phase: outbound_delayed_dtls ────────────"
ODD_WS_PORT=8789
ODD_ADMIN_PORT=9099
ODD_WS_LOG=$(mktemp -t echo-ws-odd.XXXXXX.log)
ODD_DAEMON_LOG=$(mktemp -t siphon-ai-odd.XXXXXX.log)
ODD_CONFIG=$(mktemp -t siphon-ai-odd.XXXXXX.toml)
cat >"$ODD_CONFIG" <<EOF
[node]
id = "siphon-ai-sipp-odd"
[sip]
listen = "127.0.0.1:$DAEMON_PORT"
[media]
codecs = ["pcmu"]
[bridge]
ws_url = "ws://127.0.0.1:$ODD_WS_PORT/"
[observability]
enabled = true
http_listen = "127.0.0.1:$ODD_ADMIN_PORT"
[admin]
listen = "127.0.0.1:$ADMIN_API_PORT"
[[admin.token]]
name = "harness"
token = "$ADMIN_TOKEN"
role = "admin"
[outbound]
max_concurrent = 2
[[gateway]]
name = "sipp"
proxy = "127.0.0.1:$SIPP_PORT"
from = "sip:harness@127.0.0.1"
srtp = "required"
[[route]]
name = "default"
[route.match]
any = true
EOF

ODD_PYTHON="$REPO_ROOT/examples/echo-ws-server-python/.venv/bin/python"
[[ -x "$ODD_PYTHON" ]] || ODD_PYTHON=python3
"$ODD_PYTHON" "$REPO_ROOT/examples/echo-ws-server-python/server.py" \
    --bind "127.0.0.1:$ODD_WS_PORT" \
    --auto-hangup-after-ms 1500 \
    >"$ODD_WS_LOG" 2>&1 &
ODD_WS_PID=$!

RUST_LOG=siphon_ai=info "$DAEMON_BIN" --config "$ODD_CONFIG" \
    >"$ODD_DAEMON_LOG" 2>&1 &
ODD_DAEMON_PID=$!
odd_cleanup() {
    kill "$ODD_WS_PID" "$ODD_DAEMON_PID" 2>/dev/null || true
    wait "$ODD_WS_PID" "$ODD_DAEMON_PID" 2>/dev/null || true
}
trap odd_cleanup EXIT
sleep 1.2

total=$((total + 1))
echo "─── outbound_delayed_dtls_uas ────────────────────────"
odd_ok=0
sipp -i 127.0.0.1 -sf "$SCRIPT_DIR/outbound_delayed_dtls_uas.xml" \
    -m 1 -timeout 15s -trace_err -p "$SIPP_PORT" >/dev/null 2>&1 &
ODD_SIPP_PID=$!
sleep 0.3
odd_resp=$(curl -s -o /dev/null -w "%{http_code}" \
    -X POST -H "Authorization: Bearer $ADMIN_TOKEN" "http://127.0.0.1:$ADMIN_API_PORT/admin/v1/calls" \
    -d '{"to": "7001", "gateway": "sipp", "delayed_offer": true}')
if [[ "$odd_resp" == "202" ]] && wait "$ODD_SIPP_PID"; then
    if curl -s "http://127.0.0.1:$ODD_ADMIN_PORT/metrics" \
        | grep -q 'siphon_ai_outbound_srtp_total{result="encrypted"} 1'; then
        odd_ok=1
    fi
fi
if (( odd_ok )); then
    echo "  OK"
else
    echo "  FAIL (originate=$odd_resp; daemon: $ODD_DAEMON_LOG; ws: $ODD_WS_LOG)"
    failures=$((failures + 1))
fi

odd_cleanup
trap - EXIT

# ─── Always-on auxiliary phase: attended transfer ─────────────────
# The full 0.6.1 three-party flow with SIPp on both far ends:
#   * leg A  — SIPp UAC calls in (the transferee); its echo-ws is
#     started with --auto-transfer-replaces so the WS side completes
#     the attended transfer shortly after the bridge comes up.
#   * leg C  — a consult call the harness places via the originate
#     API to a second SIPp running outbound_uas_answer.xml; its own
#     echo-ws auto-hangs-up later (the consult leg must outlive the
#     REFER — SiphonAI does NOT tear it down at transfer time).
# Pass = leg A saw a REFER whose Refer-To embeds a Replaces built
# from the consult dialog (check_it in the scenario) + the metric
# reads attended/accepted.
echo
echo "─── auxiliary phase: attended_transfer ────────────────"
AT_CONSULT_PORT=5081
AT_ADMIN_PORT=9091
AT_A_WS_PORT=8768
AT_C_WS_PORT=8769
AT_A_WS_LOG=$(mktemp -t echo-ws-at-a.XXXXXX.log)
AT_C_WS_LOG=$(mktemp -t echo-ws-at-c.XXXXXX.log)
AT_DAEMON_LOG=$(mktemp -t siphon-ai-at.XXXXXX.log)
AT_CONFIG=$(mktemp -t siphon-ai-at.XXXXXX.toml)
cat >"$AT_CONFIG" <<EOF
[node]
id = "siphon-ai-sipp-at"
[sip]
listen = "127.0.0.1:$DAEMON_PORT"
[media]
codecs = ["pcmu"]
[bridge]
ws_url = "ws://127.0.0.1:$AT_A_WS_PORT/"
[observability]
enabled = true
http_listen = "127.0.0.1:$AT_ADMIN_PORT"
[admin]
listen = "127.0.0.1:$ADMIN_API_PORT"
[[admin.token]]
name = "harness"
token = "$ADMIN_TOKEN"
role = "admin"
[outbound]
max_concurrent = 2
[[gateway]]
name = "sipp"
proxy = "127.0.0.1:$AT_CONSULT_PORT"
from = "sip:harness@127.0.0.1"
[[route]]
name = "default"
[route.match]
any = true
EOF

AT_PYTHON="$REPO_ROOT/examples/echo-ws-server-python/.venv/bin/python"
[[ -x "$AT_PYTHON" ]] || AT_PYTHON=python3

# Consult-leg echo-ws: hang the consult call up well AFTER the
# transfer completes, so its dialog is still live when the REFER's
# Replaces references it.
"$AT_PYTHON" "$REPO_ROOT/examples/echo-ws-server-python/server.py" \
    --bind "127.0.0.1:$AT_C_WS_PORT" \
    --auto-hangup-after-ms 6000 \
    >"$AT_C_WS_LOG" 2>&1 &
AT_C_WS_PID=$!

RUST_LOG=siphon_ai=info "$DAEMON_BIN" --config "$AT_CONFIG" \
    >"$AT_DAEMON_LOG" 2>&1 &
AT_DAEMON_PID=$!
AT_A_WS_PID=""
AT_C_SIPP_PID=""
# Kill the consult-side sipp too: on a failed run it never sees its
# BYE and -timeout won't reap a call still in progress, so it would
# squat on $AT_CONSULT_PORT past the end of the script.
at_cleanup() {
    kill "$AT_C_WS_PID" "$AT_DAEMON_PID" $AT_A_WS_PID $AT_C_SIPP_PID 2>/dev/null || true
    wait "$AT_C_WS_PID" "$AT_DAEMON_PID" $AT_A_WS_PID $AT_C_SIPP_PID 2>/dev/null || true
}
trap at_cleanup EXIT
sleep 1.2

total=$((total + 1))
echo "─── attended_transfer ────────────────────────────────"
at_ok=0
# Consult callee first, then originate leg C through the gateway.
sipp -i 127.0.0.1 -sf "$SCRIPT_DIR/outbound_uas_answer.xml" \
    -m 1 -timeout 20s -trace_err -p "$AT_CONSULT_PORT" >/dev/null 2>&1 &
AT_C_SIPP_PID=$!
sleep 0.3
at_consult_id=$(curl -s -X POST -H "Authorization: Bearer $ADMIN_TOKEN" "http://127.0.0.1:$ADMIN_API_PORT/admin/v1/calls" \
    -d "{\"to\": \"agent\", \"gateway\": \"sipp\", \"ws_url\": \"ws://127.0.0.1:$AT_C_WS_PORT/\"}" \
    | sed -n 's/.*"call_id":"\([^"]*\)".*/\1/p')

# Wait for the consult leg to be ANSWERED (registered as a consult
# target) before leg A's transfer can reference it.
at_answered=0
for _ in $(seq 1 20); do
    if curl -s "http://127.0.0.1:$AT_ADMIN_PORT/metrics" \
        | grep -q 'siphon_ai_outbound_calls_total{result="answered"} 1'; then
        at_answered=1; break
    fi
    sleep 0.2
done

if [[ -n "$at_consult_id" ]] && (( at_answered )); then
    # Leg A's echo-ws completes the attended transfer once bridged.
    "$AT_PYTHON" "$REPO_ROOT/examples/echo-ws-server-python/server.py" \
        --bind "127.0.0.1:$AT_A_WS_PORT" \
        --auto-transfer-replaces "$at_consult_id" \
        --auto-transfer-delay-ms 300 \
        >"$AT_A_WS_LOG" 2>&1 &
    AT_A_WS_PID=$!
    sleep 0.5

    if sipp -i 127.0.0.1 -sf "$SCRIPT_DIR/attended_transfer_a.xml" \
            -m 1 -timeout 15s -trace_err -p "$SIPP_PORT" -s 1000 \
            "127.0.0.1:$DAEMON_PORT" >/dev/null 2>&1 \
        && wait "$AT_C_SIPP_PID"; then
        # Both far ends are happy; cross-check the daemon's view.
        if curl -s "http://127.0.0.1:$AT_ADMIN_PORT/metrics" \
            | grep 'siphon_ai_transfers_total' \
            | grep 'mode="attended"' \
            | grep -q 'result="accepted"'; then
            at_ok=1
        fi
    fi
fi
if (( at_ok )); then
    echo "  OK"
else
    echo "  FAIL (consult_id=$at_consult_id answered=$at_answered;" \
         "daemon: $AT_DAEMON_LOG; ws A: $AT_A_WS_LOG; ws C: $AT_C_WS_LOG)"
    failures=$((failures + 1))
fi

at_cleanup
trap - EXIT

# ─── Always-on auxiliary phase: park → timeout → hangup ───────────
# SIPp calls in; the echo-ws parks the call (--auto-park) → the WS
# detaches and the caller hears hold music; [park].timeout_secs=1 with
# timeout_action="hangup" fires and SiphonAI BYEs the caller. Pass =
# SIPp saw the BYE AND parks_total{result="ok"} ticked.
echo
echo "─── auxiliary phase: park_timeout ─────────────────────"
PK_WS_PORT=8770
PK_ADMIN_PORT=9091
PK_WS_LOG=$(mktemp -t echo-ws-pk.XXXXXX.log)
PK_DAEMON_LOG=$(mktemp -t siphon-ai-pk.XXXXXX.log)
PK_CONFIG=$(mktemp -t siphon-ai-pk.XXXXXX.toml)
cat >"$PK_CONFIG" <<EOF
[node]
id = "siphon-ai-sipp-pk"
[sip]
listen = "127.0.0.1:$DAEMON_PORT"
[media]
codecs = ["pcmu"]
[bridge]
ws_url = "ws://127.0.0.1:$PK_WS_PORT/"
[observability]
enabled = true
http_listen = "127.0.0.1:$PK_ADMIN_PORT"
[admin]
listen = "127.0.0.1:$ADMIN_API_PORT"
[[admin.token]]
name = "harness"
token = "$ADMIN_TOKEN"
role = "admin"
[park]
enabled = true
timeout_secs = 1
timeout_action = "hangup"
[[route]]
name = "default"
[route.match]
any = true
EOF

PK_PYTHON="$REPO_ROOT/examples/echo-ws-server-python/.venv/bin/python"
[[ -x "$PK_PYTHON" ]] || PK_PYTHON=python3
"$PK_PYTHON" "$REPO_ROOT/examples/echo-ws-server-python/server.py" \
    --bind "127.0.0.1:$PK_WS_PORT" \
    --auto-park \
    >"$PK_WS_LOG" 2>&1 &
PK_WS_PID=$!

RUST_LOG=siphon_ai=info "$DAEMON_BIN" --config "$PK_CONFIG" \
    >"$PK_DAEMON_LOG" 2>&1 &
PK_DAEMON_PID=$!
pk_cleanup() {
    kill "$PK_WS_PID" "$PK_DAEMON_PID" 2>/dev/null || true
    wait "$PK_WS_PID" "$PK_DAEMON_PID" 2>/dev/null || true
}
trap pk_cleanup EXIT
sleep 1.2

total=$((total + 1))
echo "─── park_timeout_hangup ──────────────────────────────"
pk_ok=0
if sipp -i 127.0.0.1 -sf "$SCRIPT_DIR/park_caller.xml" -m 1 -timeout 15s -trace_err \
        -p "$SIPP_PORT" -s 1000 "127.0.0.1:$DAEMON_PORT" >/dev/null 2>&1; then
    if curl -s "http://127.0.0.1:$PK_ADMIN_PORT/metrics" \
        | grep -q 'siphon_ai_parks_total{result="ok"} 1'; then
        pk_ok=1
    fi
fi
if (( pk_ok )); then
    echo "  OK"
else
    echo "  FAIL (daemon: $PK_DAEMON_LOG; ws: $PK_WS_LOG)"
    failures=$((failures + 1))
fi

pk_cleanup
trap - EXIT

# ─── Always-on auxiliary phase: park → retrieve → hangup ──────────
# SIPp calls in; echo-ws A parks the call (--auto-park, no timeout). The
# runner waits until the call shows up in GET /admin/v1/parked, then
# POSTs a retrieve onto echo-ws B (which auto-hangs-up). SiphonAI opens a
# fresh WS to B, B hangs up, and SiphonAI BYEs the caller. Pass = SIPp
# saw the BYE AND retrieves_total{result="ok"} ticked.
echo
echo "─── auxiliary phase: park_retrieve ────────────────────"
PR_WS_A_PORT=8770
PR_WS_B_PORT=8771
PR_ADMIN_PORT=9091
PR_WS_A_LOG=$(mktemp -t echo-ws-pr-a.XXXXXX.log)
PR_WS_B_LOG=$(mktemp -t echo-ws-pr-b.XXXXXX.log)
PR_DAEMON_LOG=$(mktemp -t siphon-ai-pr.XXXXXX.log)
PR_CONFIG=$(mktemp -t siphon-ai-pr.XXXXXX.toml)
cat >"$PR_CONFIG" <<EOF
[node]
id = "siphon-ai-sipp-pr"
[sip]
listen = "127.0.0.1:$DAEMON_PORT"
[media]
codecs = ["pcmu"]
[bridge]
ws_url = "ws://127.0.0.1:$PR_WS_A_PORT/"
[observability]
enabled = true
http_listen = "127.0.0.1:$PR_ADMIN_PORT"
[admin]
listen = "127.0.0.1:$ADMIN_API_PORT"
[[admin.token]]
name = "harness"
token = "$ADMIN_TOKEN"
role = "admin"
[park]
enabled = true
timeout_secs = 0
[[route]]
name = "default"
[route.match]
any = true
EOF

PR_PYTHON="$REPO_ROOT/examples/echo-ws-server-python/.venv/bin/python"
[[ -x "$PR_PYTHON" ]] || PR_PYTHON=python3
# A parks the inbound call; B is the retrieve target and hangs up shortly
# after it receives its (retrieved) start.
"$PR_PYTHON" "$REPO_ROOT/examples/echo-ws-server-python/server.py" \
    --bind "127.0.0.1:$PR_WS_A_PORT" --auto-park >"$PR_WS_A_LOG" 2>&1 &
PR_WS_A_PID=$!
"$PR_PYTHON" "$REPO_ROOT/examples/echo-ws-server-python/server.py" \
    --bind "127.0.0.1:$PR_WS_B_PORT" --auto-hangup-after-ms 1500 >"$PR_WS_B_LOG" 2>&1 &
PR_WS_B_PID=$!

RUST_LOG=siphon_ai=info "$DAEMON_BIN" --config "$PR_CONFIG" \
    >"$PR_DAEMON_LOG" 2>&1 &
PR_DAEMON_PID=$!
PR_SIPP_PID=""
pr_cleanup() {
    kill "$PR_WS_A_PID" "$PR_WS_B_PID" "$PR_DAEMON_PID" $PR_SIPP_PID 2>/dev/null || true
    wait "$PR_WS_A_PID" "$PR_WS_B_PID" "$PR_DAEMON_PID" $PR_SIPP_PID 2>/dev/null || true
}
trap pr_cleanup EXIT
sleep 1.2

total=$((total + 1))
echo "─── park_retrieve_hangup ─────────────────────────────"
pr_ok=0
# Caller in the background — it answers and then waits for the BYE.
sipp -i 127.0.0.1 -sf "$SCRIPT_DIR/park_caller.xml" -m 1 -timeout 20s -trace_err \
    -p "$SIPP_PORT" -s 1000 "127.0.0.1:$DAEMON_PORT" >/dev/null 2>&1 &
PR_SIPP_PID=$!
sleep 0.3

# Wait until the call is parked, then grab its bridge call_id.
parked_id=""
for _ in $(seq 1 25); do
    parked_id=$(curl -s -H "Authorization: Bearer $ADMIN_TOKEN" "http://127.0.0.1:$ADMIN_API_PORT/admin/v1/parked" \
        | sed -n 's/.*"call_id":"\([^"]*\)".*/\1/p' | head -1)
    [[ -n "$parked_id" ]] && break
    sleep 0.2
done

if [[ -n "$parked_id" ]]; then
    curl -s -o /dev/null -X POST \
        -H "Authorization: Bearer $ADMIN_TOKEN" "http://127.0.0.1:$ADMIN_API_PORT/admin/v1/calls/$parked_id/retrieve" \
        -d "{\"ws_url\": \"ws://127.0.0.1:$PR_WS_B_PORT/\"}"
    # echo-ws B hangs up → SiphonAI BYEs the caller → SIPp completes.
    if wait "$PR_SIPP_PID" && curl -s "http://127.0.0.1:$PR_ADMIN_PORT/metrics" \
        | grep -q 'siphon_ai_retrieves_total{result="ok"} 1'; then
        pr_ok=1
    fi
fi
if (( pr_ok )); then
    echo "  OK"
else
    echo "  FAIL (parked_id=$parked_id; daemon: $PR_DAEMON_LOG;" \
         "ws A: $PR_WS_A_LOG; ws B: $PR_WS_B_LOG)"
    failures=$((failures + 1))
fi

pr_cleanup
trap - EXIT

# ─── Always-on auxiliary phase: conference (two callers) ──────────
# Two SIPp callers (on different ports) both bridge to one echo-ws
# started with --auto-conference-join, so both legs land in the SAME
# room. While both are up the runner asserts the daemon mixed them
# (conference_participants=4 — two calls × SIP leg + WS session); after
# both hang up the room ends (conferences_active=0).
echo
echo "─── auxiliary phase: conference ───────────────────────"
CF_WS_PORT=8772
CF_ADMIN_PORT=9091
CF_SIPP2_PORT=5082
CF_WS_LOG=$(mktemp -t echo-ws-cf.XXXXXX.log)
CF_DAEMON_LOG=$(mktemp -t siphon-ai-cf.XXXXXX.log)
CF_CONFIG=$(mktemp -t siphon-ai-cf.XXXXXX.toml)
cat >"$CF_CONFIG" <<EOF
[node]
id = "siphon-ai-sipp-cf"
[sip]
listen = "127.0.0.1:$DAEMON_PORT"
[media]
codecs = ["pcmu"]
[bridge]
ws_url = "ws://127.0.0.1:$CF_WS_PORT/"
[observability]
enabled = true
http_listen = "127.0.0.1:$CF_ADMIN_PORT"
[admin]
listen = "127.0.0.1:$ADMIN_API_PORT"
[[admin.token]]
name = "harness"
token = "$ADMIN_TOKEN"
role = "admin"
[conference]
enabled = true
[[route]]
name = "default"
[route.match]
any = true
EOF

CF_PYTHON="$REPO_ROOT/examples/echo-ws-server-python/.venv/bin/python"
[[ -x "$CF_PYTHON" ]] || CF_PYTHON=python3
"$CF_PYTHON" "$REPO_ROOT/examples/echo-ws-server-python/server.py" \
    --bind "127.0.0.1:$CF_WS_PORT" \
    --auto-conference-join confroom \
    >"$CF_WS_LOG" 2>&1 &
CF_WS_PID=$!

RUST_LOG=siphon_ai=info "$DAEMON_BIN" --config "$CF_CONFIG" \
    >"$CF_DAEMON_LOG" 2>&1 &
CF_DAEMON_PID=$!
CF_S1_PID=""
CF_S2_PID=""
cf_cleanup() {
    kill "$CF_WS_PID" "$CF_DAEMON_PID" $CF_S1_PID $CF_S2_PID 2>/dev/null || true
    wait "$CF_WS_PID" "$CF_DAEMON_PID" $CF_S1_PID $CF_S2_PID 2>/dev/null || true
}
trap cf_cleanup EXIT
sleep 1.2

total=$((total + 1))
echo "─── conference_two_callers ───────────────────────────"
cf_ok=0
# Two concurrent callers → both join "confroom" via the echo-ws.
sipp -i 127.0.0.1 -sf "$SCRIPT_DIR/conference_caller.xml" -m 1 -timeout 20s -trace_err \
    -p "$SIPP_PORT" -s 1000 "127.0.0.1:$DAEMON_PORT" >/dev/null 2>&1 &
CF_S1_PID=$!
sipp -i 127.0.0.1 -sf "$SCRIPT_DIR/conference_caller.xml" -m 1 -timeout 20s -trace_err \
    -p "$CF_SIPP2_PORT" -s 1000 "127.0.0.1:$DAEMON_PORT" >/dev/null 2>&1 &
CF_S2_PID=$!

# Wait until both legs are mixed into the room.
cf_mixed=0
for _ in $(seq 1 30); do
    if curl -s "http://127.0.0.1:$CF_ADMIN_PORT/metrics" \
        | grep -q 'siphon_ai_conference_participants 4'; then
        cf_mixed=1; break
    fi
    sleep 0.2
done

if (( cf_mixed )) && wait "$CF_S1_PID" && wait "$CF_S2_PID"; then
    # Both callers hung up; the room ends once both legs tear down — async
    # after the BYE/200, so poll briefly rather than scraping once.
    for _ in $(seq 1 15); do
        if curl -s "http://127.0.0.1:$CF_ADMIN_PORT/metrics" \
            | grep -q 'siphon_ai_conferences_active 0'; then
            cf_ok=1; break
        fi
        sleep 0.2
    done
fi
if (( cf_ok )); then
    echo "  OK"
else
    echo "  FAIL (mixed=$cf_mixed; daemon: $CF_DAEMON_LOG; ws: $CF_WS_LOG)"
    failures=$((failures + 1))
fi

cf_cleanup
trap - EXIT

# ─── Always-on auxiliary phase: outbound SRTP (SDES) ──────────────
# Like the outbound phase, but the gateway sets srtp = "required", so
# SiphonAI's INVITE offers RTP/SAVP + a=crypto. SIPp (the callee) answers
# RTP/SAVP with its own a=crypto; SiphonAI installs keys and bridges. Pass
# = SIPp completed INVITE → ACK → BYE AND the daemon's
# siphon_ai_outbound_srtp_total{result="encrypted"} metric reads 1.
echo
echo "─── auxiliary phase: outbound_srtp ────────────────────"
OBS_WS_PORT=8773
OBS_ADMIN_PORT=9091
OBS_WS_LOG=$(mktemp -t echo-ws-obs.XXXXXX.log)
OBS_DAEMON_LOG=$(mktemp -t siphon-ai-obs.XXXXXX.log)
OBS_CONFIG=$(mktemp -t siphon-ai-obs.XXXXXX.toml)
cat >"$OBS_CONFIG" <<EOF
[node]
id = "siphon-ai-sipp-obs"
[sip]
listen = "127.0.0.1:$DAEMON_PORT"
[media]
codecs = ["pcmu"]
[bridge]
ws_url = "ws://127.0.0.1:$OBS_WS_PORT/"
[observability]
enabled = true
http_listen = "127.0.0.1:$OBS_ADMIN_PORT"
[admin]
listen = "127.0.0.1:$ADMIN_API_PORT"
[[admin.token]]
name = "harness"
token = "$ADMIN_TOKEN"
role = "admin"
[outbound]
max_concurrent = 2
[[gateway]]
name = "sipp"
proxy = "127.0.0.1:$SIPP_PORT"
from = "sip:harness@127.0.0.1"
srtp = "required"
[[route]]
name = "default"
[route.match]
any = true
EOF

OBS_PYTHON="$REPO_ROOT/examples/echo-ws-server-python/.venv/bin/python"
[[ -x "$OBS_PYTHON" ]] || OBS_PYTHON=python3
"$OBS_PYTHON" "$REPO_ROOT/examples/echo-ws-server-python/server.py" \
    --bind "127.0.0.1:$OBS_WS_PORT" \
    --auto-hangup-after-ms 1500 \
    >"$OBS_WS_LOG" 2>&1 &
OBS_WS_PID=$!

RUST_LOG=siphon_ai=info "$DAEMON_BIN" --config "$OBS_CONFIG" \
    >"$OBS_DAEMON_LOG" 2>&1 &
OBS_DAEMON_PID=$!
OBS_SIPP_PID=""
obs_cleanup() {
    kill "$OBS_WS_PID" "$OBS_DAEMON_PID" $OBS_SIPP_PID 2>/dev/null || true
    wait "$OBS_WS_PID" "$OBS_DAEMON_PID" $OBS_SIPP_PID 2>/dev/null || true
}
trap obs_cleanup EXIT
sleep 1.2

total=$((total + 1))
echo "─── outbound_srtp_uas_answer ─────────────────────────"
obs_ok=0
sipp -i 127.0.0.1 -sf "$SCRIPT_DIR/outbound_srtp_uas_answer.xml" \
    -m 1 -timeout 15s -trace_err -p "$SIPP_PORT" >/dev/null 2>&1 &
OBS_SIPP_PID=$!
sleep 0.3
obs_resp=$(curl -s -o /dev/null -w "%{http_code}" \
    -X POST -H "Authorization: Bearer $ADMIN_TOKEN" "http://127.0.0.1:$ADMIN_API_PORT/admin/v1/calls" \
    -d '{"to": "7002", "gateway": "sipp"}')
if [[ "$obs_resp" == "202" ]] && wait "$OBS_SIPP_PID"; then
    if curl -s "http://127.0.0.1:$OBS_ADMIN_PORT/metrics" \
        | grep -q 'siphon_ai_outbound_srtp_total{result="encrypted"} 1'; then
        obs_ok=1
    fi
fi
if (( obs_ok )); then
    echo "  OK"
else
    echo "  FAIL (originate=$obs_resp; daemon: $OBS_DAEMON_LOG; ws: $OBS_WS_LOG)"
    failures=$((failures + 1))
fi

obs_cleanup
trap - EXIT

# ─── Always-on auxiliary phase: bot-initiated hold/resume ─────────
# SIPp calls in; the echo-ws (--auto-hold) drives a full bot-hold cycle:
# `hold` → ~1s → `resume` → `hangup`. SiphonAI becomes the re-INVITE
# offerer, so the caller scenario asserts it RECEIVES a sendonly re-INVITE
# then a sendrecv one (the inverse of reinvite_hold_resume, which sends
# them). Pass = the SIPp scenario completed (both check_it asserts held)
# AND holds_total{result="ok"} == 2 (hold + resume).
echo
echo "─── auxiliary phase: bot_hold ─────────────────────────"
BH_WS_PORT=8774
BH_ADMIN_PORT=9091
BH_WS_LOG=$(mktemp -t echo-ws-bh.XXXXXX.log)
BH_DAEMON_LOG=$(mktemp -t siphon-ai-bh.XXXXXX.log)
BH_CONFIG=$(mktemp -t siphon-ai-bh.XXXXXX.toml)
cat >"$BH_CONFIG" <<EOF
[node]
id = "siphon-ai-sipp-bh"
[sip]
listen = "127.0.0.1:$DAEMON_PORT"
[media]
codecs = ["pcmu"]
[bridge]
ws_url = "ws://127.0.0.1:$BH_WS_PORT/"
[observability]
enabled = true
http_listen = "127.0.0.1:$BH_ADMIN_PORT"
[admin]
listen = "127.0.0.1:$ADMIN_API_PORT"
[[admin.token]]
name = "harness"
token = "$ADMIN_TOKEN"
role = "admin"
[[route]]
name = "default"
[route.match]
any = true
EOF

BH_PYTHON="$REPO_ROOT/examples/echo-ws-server-python/.venv/bin/python"
[[ -x "$BH_PYTHON" ]] || BH_PYTHON=python3
"$BH_PYTHON" "$REPO_ROOT/examples/echo-ws-server-python/server.py" \
    --bind "127.0.0.1:$BH_WS_PORT" \
    --auto-hold \
    >"$BH_WS_LOG" 2>&1 &
BH_WS_PID=$!

RUST_LOG=siphon_ai=info "$DAEMON_BIN" --config "$BH_CONFIG" \
    >"$BH_DAEMON_LOG" 2>&1 &
BH_DAEMON_PID=$!
bh_cleanup() {
    kill "$BH_WS_PID" "$BH_DAEMON_PID" 2>/dev/null || true
    wait "$BH_WS_PID" "$BH_DAEMON_PID" 2>/dev/null || true
}
trap bh_cleanup EXIT
sleep 1.2

total=$((total + 1))
echo "─── bot_hold_resume ──────────────────────────────────"
bh_ok=0
if sipp -i 127.0.0.1 -sf "$SCRIPT_DIR/bot_hold_caller.xml" -m 1 -timeout 15s -trace_err \
        -p "$SIPP_PORT" -s 1000 "127.0.0.1:$DAEMON_PORT" >/dev/null 2>&1; then
    # Both directions succeeded → holds_total{result="ok"} ticked twice.
    if curl -s "http://127.0.0.1:$BH_ADMIN_PORT/metrics" \
        | grep -q 'siphon_ai_holds_total{result="ok"} 2'; then
        bh_ok=1
    fi
fi
if (( bh_ok )); then
    echo "  OK"
else
    echo "  FAIL (daemon: $BH_DAEMON_LOG; ws: $BH_WS_LOG)"
    failures=$((failures + 1))
fi

bh_cleanup
trap - EXIT

# ─── Always-on auxiliary phase: WS reconnect mid-call ─────────────
# SIPp calls in and waits (park_caller.xml just answers + waits for a
# server BYE). The echo-ws (--drop-after-ms) abruptly closes the socket
# mid-call; with [bridge].ws_reconnect_enabled the daemon keeps the call
# up on hold music and re-dials. The redial's start carries
# reconnected:true, the echo-ws hangs that resumed call up, and SiphonAI
# BYEs the caller. Pass = SIPp saw the BYE AND
# ws_reconnects_total{result="recovered"} == 1. (Exhaustion is covered by
# the controller unit test ws_reconnect_exhausts_and_tears_down.)
echo
echo "─── auxiliary phase: ws_reconnect ─────────────────────"
RC_WS_PORT=8775
RC_ADMIN_PORT=9091
RC_WS_LOG=$(mktemp -t echo-ws-rc.XXXXXX.log)
RC_DAEMON_LOG=$(mktemp -t siphon-ai-rc.XXXXXX.log)
RC_CONFIG=$(mktemp -t siphon-ai-rc.XXXXXX.toml)
cat >"$RC_CONFIG" <<EOF
[node]
id = "siphon-ai-sipp-rc"
[sip]
listen = "127.0.0.1:$DAEMON_PORT"
[media]
codecs = ["pcmu"]
[bridge]
ws_url = "ws://127.0.0.1:$RC_WS_PORT/"
ws_reconnect_enabled = true
ws_reconnect_max_secs = 10
[observability]
enabled = true
http_listen = "127.0.0.1:$RC_ADMIN_PORT"
[admin]
listen = "127.0.0.1:$ADMIN_API_PORT"
[[admin.token]]
name = "harness"
token = "$ADMIN_TOKEN"
role = "admin"
[[route]]
name = "default"
[route.match]
any = true
EOF

RC_PYTHON="$REPO_ROOT/examples/echo-ws-server-python/.venv/bin/python"
[[ -x "$RC_PYTHON" ]] || RC_PYTHON=python3
"$RC_PYTHON" "$REPO_ROOT/examples/echo-ws-server-python/server.py" \
    --bind "127.0.0.1:$RC_WS_PORT" \
    --drop-after-ms 700 \
    >"$RC_WS_LOG" 2>&1 &
RC_WS_PID=$!

RUST_LOG=siphon_ai=info "$DAEMON_BIN" --config "$RC_CONFIG" \
    >"$RC_DAEMON_LOG" 2>&1 &
RC_DAEMON_PID=$!
rc_cleanup() {
    kill "$RC_WS_PID" "$RC_DAEMON_PID" 2>/dev/null || true
    wait "$RC_WS_PID" "$RC_DAEMON_PID" 2>/dev/null || true
}
trap rc_cleanup EXIT
sleep 1.2

total=$((total + 1))
echo "─── ws_reconnect_recovers ────────────────────────────"
rc_ok=0
if sipp -i 127.0.0.1 -sf "$SCRIPT_DIR/park_caller.xml" -m 1 -timeout 20s -trace_err \
        -p "$SIPP_PORT" -s 1000 "127.0.0.1:$DAEMON_PORT" >/dev/null 2>&1; then
    if curl -s "http://127.0.0.1:$RC_ADMIN_PORT/metrics" \
        | grep -q 'siphon_ai_ws_reconnects_total{result="recovered"} 1'; then
        rc_ok=1
    fi
fi
if (( rc_ok )); then
    echo "  OK"
else
    echo "  FAIL (daemon: $RC_DAEMON_LOG; ws: $RC_WS_LOG)"
    failures=$((failures + 1))
fi

rc_cleanup
trap - EXIT

# ─── Always-on auxiliary phase: outbound WS reconnect ─────────────
# Like the outbound phase (SIPp is the callee, SiphonAI the UAC dialing
# via a gateway), but the echo-ws (--drop-after-ms) drops mid-call and
# [bridge].ws_reconnect_enabled makes SiphonAI re-dial. The redial's start
# carries reconnected:true, the echo-ws hangs that resumed call up, and
# SiphonAI BYEs the SIPp callee. Proves reconnect works on the outbound
# originate path too (0.7.4). Pass = SIPp completed AND
# ws_reconnects_total{result="recovered"} == 1.
echo
echo "─── auxiliary phase: outbound_reconnect ───────────────"
OR_WS_PORT=8776
OR_ADMIN_PORT=9091
OR_WS_LOG=$(mktemp -t echo-ws-or.XXXXXX.log)
OR_DAEMON_LOG=$(mktemp -t siphon-ai-or.XXXXXX.log)
OR_CONFIG=$(mktemp -t siphon-ai-or.XXXXXX.toml)
cat >"$OR_CONFIG" <<EOF
[node]
id = "siphon-ai-sipp-or"
[sip]
listen = "127.0.0.1:$DAEMON_PORT"
[media]
codecs = ["pcmu"]
[bridge]
ws_url = "ws://127.0.0.1:$OR_WS_PORT/"
ws_reconnect_enabled = true
ws_reconnect_max_secs = 10
[observability]
enabled = true
http_listen = "127.0.0.1:$OR_ADMIN_PORT"
[admin]
listen = "127.0.0.1:$ADMIN_API_PORT"
[[admin.token]]
name = "harness"
token = "$ADMIN_TOKEN"
role = "admin"
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

OR_PYTHON="$REPO_ROOT/examples/echo-ws-server-python/.venv/bin/python"
[[ -x "$OR_PYTHON" ]] || OR_PYTHON=python3
"$OR_PYTHON" "$REPO_ROOT/examples/echo-ws-server-python/server.py" \
    --bind "127.0.0.1:$OR_WS_PORT" \
    --drop-after-ms 700 \
    >"$OR_WS_LOG" 2>&1 &
OR_WS_PID=$!

RUST_LOG=siphon_ai=info "$DAEMON_BIN" --config "$OR_CONFIG" \
    >"$OR_DAEMON_LOG" 2>&1 &
OR_DAEMON_PID=$!
OR_SIPP_PID=""
or_cleanup() {
    kill "$OR_WS_PID" "$OR_DAEMON_PID" $OR_SIPP_PID 2>/dev/null || true
    wait "$OR_WS_PID" "$OR_DAEMON_PID" $OR_SIPP_PID 2>/dev/null || true
}
trap or_cleanup EXIT
sleep 1.2

total=$((total + 1))
echo "─── outbound_reconnect_recovers ──────────────────────"
or_ok=0
sipp -i 127.0.0.1 -sf "$SCRIPT_DIR/outbound_uas_answer.xml" \
    -m 1 -timeout 20s -trace_err -p "$SIPP_PORT" >/dev/null 2>&1 &
OR_SIPP_PID=$!
sleep 0.3
or_resp=$(curl -s -o /dev/null -w "%{http_code}" \
    -X POST -H "Authorization: Bearer $ADMIN_TOKEN" "http://127.0.0.1:$ADMIN_API_PORT/admin/v1/calls" \
    -d '{"to": "7001", "gateway": "sipp"}')
if [[ "$or_resp" == "202" ]] && wait "$OR_SIPP_PID"; then
    if curl -s "http://127.0.0.1:$OR_ADMIN_PORT/metrics" \
        | grep -q 'siphon_ai_ws_reconnects_total{result="recovered"} 1'; then
        or_ok=1
    fi
fi
if (( or_ok )); then
    echo "  OK"
else
    echo "  FAIL (originate=$or_resp; daemon: $OR_DAEMON_LOG; ws: $OR_WS_LOG)"
    failures=$((failures + 1))
fi

or_cleanup
trap - EXIT

# ─── Always-on auxiliary phase: outbound bot-hold ─────────────────
# Like the outbound phase (SIPp the callee, SiphonAI the UAC), but the
# echo-ws (--auto-hold) drives a bot-initiated hold cycle, so SiphonAI
# sends a hold re-INVITE (a=sendonly) then a resume (a=sendrecv) on the
# outbound (Direct) dialog via the gateway UAC. outbound_bot_hold_uas.xml
# asserts the callee receives both and answers each. Proves bot-hold works
# on outbound legs (0.7.5). Pass = SIPp completed (both direction asserts
# held) AND siphon_ai_holds_total{result="ok"} == 2.
echo
echo "─── auxiliary phase: outbound_bot_hold ────────────────"
OH_WS_PORT=8777
OH_ADMIN_PORT=9091
OH_WS_LOG=$(mktemp -t echo-ws-oh.XXXXXX.log)
OH_DAEMON_LOG=$(mktemp -t siphon-ai-oh.XXXXXX.log)
OH_CONFIG=$(mktemp -t siphon-ai-oh.XXXXXX.toml)
cat >"$OH_CONFIG" <<EOF
[node]
id = "siphon-ai-sipp-oh"
[sip]
listen = "127.0.0.1:$DAEMON_PORT"
[media]
codecs = ["pcmu"]
[bridge]
ws_url = "ws://127.0.0.1:$OH_WS_PORT/"
[observability]
enabled = true
http_listen = "127.0.0.1:$OH_ADMIN_PORT"
[admin]
listen = "127.0.0.1:$ADMIN_API_PORT"
[[admin.token]]
name = "harness"
token = "$ADMIN_TOKEN"
role = "admin"
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

OH_PYTHON="$REPO_ROOT/examples/echo-ws-server-python/.venv/bin/python"
[[ -x "$OH_PYTHON" ]] || OH_PYTHON=python3
"$OH_PYTHON" "$REPO_ROOT/examples/echo-ws-server-python/server.py" \
    --bind "127.0.0.1:$OH_WS_PORT" \
    --auto-hold \
    >"$OH_WS_LOG" 2>&1 &
OH_WS_PID=$!

RUST_LOG=siphon_ai=info "$DAEMON_BIN" --config "$OH_CONFIG" \
    >"$OH_DAEMON_LOG" 2>&1 &
OH_DAEMON_PID=$!
OH_SIPP_PID=""
oh_cleanup() {
    kill "$OH_WS_PID" "$OH_DAEMON_PID" $OH_SIPP_PID 2>/dev/null || true
    wait "$OH_WS_PID" "$OH_DAEMON_PID" $OH_SIPP_PID 2>/dev/null || true
}
trap oh_cleanup EXIT
sleep 1.2

total=$((total + 1))
echo "─── outbound_bot_hold ────────────────────────────────"
oh_ok=0
sipp -i 127.0.0.1 -sf "$SCRIPT_DIR/outbound_bot_hold_uas.xml" \
    -m 1 -timeout 20s -trace_err -p "$SIPP_PORT" >/dev/null 2>&1 &
OH_SIPP_PID=$!
sleep 0.3
oh_resp=$(curl -s -o /dev/null -w "%{http_code}" \
    -X POST -H "Authorization: Bearer $ADMIN_TOKEN" "http://127.0.0.1:$ADMIN_API_PORT/admin/v1/calls" \
    -d '{"to": "7001", "gateway": "sipp"}')
if [[ "$oh_resp" == "202" ]] && wait "$OH_SIPP_PID"; then
    if curl -s "http://127.0.0.1:$OH_ADMIN_PORT/metrics" \
        | grep -q 'siphon_ai_holds_total{result="ok"} 2'; then
        oh_ok=1
    fi
fi
if (( oh_ok )); then
    echo "  OK"
else
    echo "  FAIL (originate=$oh_resp; daemon: $OH_DAEMON_LOG; ws: $OH_WS_LOG)"
    failures=$((failures + 1))
fi

oh_cleanup
trap - EXIT

# ─── Always-on auxiliary phase: Opus negotiation ──────────────────
# A daemon with `[media].codecs = ["opus","pcmu"]`; SIPp offers Opus at a
# dynamic PT (96, opus/48000/2) and asserts the 200 OK answers Opus AND
# carries our Opus fmtp re-keyed onto that PT (mono / 16 kHz: stereo=0,
# maxplaybackrate=16000 — the 0.8.2 fmtp follow-up). Both are check_it
# eregs in the scenario, so a clean SIPp run (rc 0) means negotiation +
# fmtp succeeded. Proves Opus negotiation + the 16 kHz bridge session
# end-to-end (0.8.0). Signalling only — SIPp can't encode Opus media; the
# codec round-trip is forge unit-tested.
echo
echo "─── auxiliary phase: opus ─────────────────────────────"
OP_WS_PORT=8778
OP_WS_LOG=$(mktemp -t echo-ws-op.XXXXXX.log)
OP_DAEMON_LOG=$(mktemp -t siphon-ai-op.XXXXXX.log)
OP_CONFIG=$(mktemp -t siphon-ai-op.XXXXXX.toml)
cat >"$OP_CONFIG" <<EOF
[node]
id = "siphon-ai-sipp-op"
[sip]
listen = "127.0.0.1:$DAEMON_PORT"
[media]
codecs = ["opus", "pcmu"]
[bridge]
ws_url = "ws://127.0.0.1:$OP_WS_PORT/"
[[route]]
name = "default"
[route.match]
any = true
EOF

OP_PYTHON="$REPO_ROOT/examples/echo-ws-server-python/.venv/bin/python"
[[ -x "$OP_PYTHON" ]] || OP_PYTHON=python3
"$OP_PYTHON" "$REPO_ROOT/examples/echo-ws-server-python/server.py" \
    --bind "127.0.0.1:$OP_WS_PORT" >"$OP_WS_LOG" 2>&1 &
OP_WS_PID=$!

RUST_LOG=siphon_ai=info "$DAEMON_BIN" --config "$OP_CONFIG" \
    >"$OP_DAEMON_LOG" 2>&1 &
OP_DAEMON_PID=$!
op_cleanup() {
    kill "$OP_WS_PID" "$OP_DAEMON_PID" 2>/dev/null || true
    wait "$OP_WS_PID" "$OP_DAEMON_PID" 2>/dev/null || true
}
trap op_cleanup EXIT
sleep 1.2

total=$((total + 1))
echo "─── opus_negotiation ─────────────────────────────────"
# The scenario's check_it asserts the 200 OK carries an opus rtpmap; a
# clean SIPp run (rc 0) means negotiation succeeded. Cross-check the
# daemon logged a 16 kHz Opus session.
if sipp -i 127.0.0.1 -sf "$SCRIPT_DIR/opus_caller.xml" -m 1 -timeout 12s -trace_err \
        -p "$SIPP_PORT" -s 1000 "127.0.0.1:$DAEMON_PORT" >/dev/null 2>&1 \
    && grep -q "negotiated=opus sample_rate=16000" "$OP_DAEMON_LOG"; then
    echo "  OK"
else
    echo "  FAIL (daemon: $OP_DAEMON_LOG; ws: $OP_WS_LOG)"
    failures=$((failures + 1))
fi

op_cleanup
trap - EXIT

# ─── auxiliary phase: delayed_offer ───────────────────────────────
# A daemon with [media].codecs = ["pcmu"] and the default
# [sip].allow_delayed_offer = true. SIPp sends an INVITE with NO SDP
# (RFC 3264 delayed offer, the CUCM-without-MTP case); SiphonAI must
# answer 200 OK carrying its own offer, then read SIPp's answer from
# the ACK. The scenario's check_it asserts the 200 carried an `m=audio`
# + PCMU rtpmap; a clean SIPp run (rc 0) means the offerless INVITE was
# accepted, finalized from the ACK, and BYE'd. Proves the inbound
# delayed-offer flow end-to-end (0.9.0).
echo
echo "─── auxiliary phase: delayed_offer ────────────────────"
DO_WS_PORT=8781
DO_WS_LOG=$(mktemp -t echo-ws-do.XXXXXX.log)
DO_DAEMON_LOG=$(mktemp -t siphon-ai-do.XXXXXX.log)
DO_CONFIG=$(mktemp -t siphon-ai-do.XXXXXX.toml)
cat >"$DO_CONFIG" <<EOF
[node]
id = "siphon-ai-sipp-do"
[sip]
listen = "127.0.0.1:$DAEMON_PORT"
[media]
codecs = ["pcmu"]
[bridge]
ws_url = "ws://127.0.0.1:$DO_WS_PORT/"
[[route]]
name = "default"
[route.match]
any = true
EOF

DO_PYTHON="$REPO_ROOT/examples/echo-ws-server-python/.venv/bin/python"
[[ -x "$DO_PYTHON" ]] || DO_PYTHON=python3
"$DO_PYTHON" "$REPO_ROOT/examples/echo-ws-server-python/server.py" \
    --bind "127.0.0.1:$DO_WS_PORT" >"$DO_WS_LOG" 2>&1 &
DO_WS_PID=$!

RUST_LOG=siphon_ai=info "$DAEMON_BIN" --config "$DO_CONFIG" \
    >"$DO_DAEMON_LOG" 2>&1 &
DO_DAEMON_PID=$!
do_cleanup() {
    kill "$DO_WS_PID" "$DO_DAEMON_PID" 2>/dev/null || true
    wait "$DO_WS_PID" "$DO_DAEMON_PID" 2>/dev/null || true
}
trap do_cleanup EXIT
sleep 1.2

total=$((total + 1))
echo "─── delayed_offer ─────────────────────────────────────"
# rc 0 = the 200 carried our offer and the call completed. Cross-check
# the daemon both ACCEPTED the offerless INVITE AND actually FINALIZED
# from the ACK answer (the bridge connected). The bridge check guards
# the on_ack-dispatch path: a regression that drops the body-carrying
# ACK would still pass the 200-OK assert but never bridge.
if sipp -i 127.0.0.1 -sf "$SCRIPT_DIR/delayed_offer_caller.xml" -m 1 -timeout 12s -trace_err \
        -p "$SIPP_PORT" -s 1000 "127.0.0.1:$DAEMON_PORT" >/dev/null 2>&1 \
    && grep -q "delayed-offer 200 OK sent; awaiting ACK answer" "$DO_DAEMON_LOG" \
    && grep -q "bridge connected" "$DO_DAEMON_LOG"; then
    echo "  OK"
else
    echo "  FAIL (daemon: $DO_DAEMON_LOG; ws: $DO_WS_LOG)"
    failures=$((failures + 1))
fi

do_cleanup
trap - EXIT

# ─── auxiliary phase: delayed_offer + SRTP ────────────────────────
# (0.9.2) Inbound delayed offer where SiphonAI OFFERS SDES SRTP in the
# 200 OK (we're the offerer). `[media].srtp = "required"` makes the
# offer RTP/SAVP + a=crypto; SIPp answers SRTP in the ACK and SiphonAI
# installs the key. The scenario's check_it asserts the 200 carried
# `a=crypto`; pass also requires the daemon to log the delayed accept.
echo
echo "─── auxiliary phase: delayed_offer_srtp ───────────────"
DOS_WS_PORT=8787
DOS_WS_LOG=$(mktemp -t echo-ws-dos.XXXXXX.log)
DOS_DAEMON_LOG=$(mktemp -t siphon-ai-dos.XXXXXX.log)
DOS_CONFIG=$(mktemp -t siphon-ai-dos.XXXXXX.toml)
cat >"$DOS_CONFIG" <<EOF
[node]
id = "siphon-ai-sipp-dos"
[sip]
listen = "127.0.0.1:$DAEMON_PORT"
[media]
codecs = ["pcmu"]
srtp = "required"
[bridge]
ws_url = "ws://127.0.0.1:$DOS_WS_PORT/"
[[route]]
name = "default"
[route.match]
any = true
EOF

DOS_PYTHON="$REPO_ROOT/examples/echo-ws-server-python/.venv/bin/python"
[[ -x "$DOS_PYTHON" ]] || DOS_PYTHON=python3
"$DOS_PYTHON" "$REPO_ROOT/examples/echo-ws-server-python/server.py" \
    --bind "127.0.0.1:$DOS_WS_PORT" >"$DOS_WS_LOG" 2>&1 &
DOS_WS_PID=$!

RUST_LOG=siphon_ai=info "$DAEMON_BIN" --config "$DOS_CONFIG" \
    >"$DOS_DAEMON_LOG" 2>&1 &
DOS_DAEMON_PID=$!
dos_cleanup() {
    kill "$DOS_WS_PID" "$DOS_DAEMON_PID" 2>/dev/null || true
    wait "$DOS_WS_PID" "$DOS_DAEMON_PID" 2>/dev/null || true
}
trap dos_cleanup EXIT
sleep 1.2

total=$((total + 1))
echo "─── delayed_offer_srtp ────────────────────────────────"
if sipp -i 127.0.0.1 -sf "$SCRIPT_DIR/delayed_offer_srtp_caller.xml" -m 1 -timeout 12s -trace_err \
        -p "$SIPP_PORT" -s 1000 "127.0.0.1:$DAEMON_PORT" >/dev/null 2>&1 \
    && grep -q "delayed-offer 200 OK sent; awaiting ACK answer" "$DOS_DAEMON_LOG"; then
    echo "  OK"
else
    echo "  FAIL (daemon: $DOS_DAEMON_LOG; ws: $DOS_WS_LOG)"
    failures=$((failures + 1))
fi

dos_cleanup
trap - EXIT

# ─── auxiliary phase: delayed_offer + DTLS offer ──────────────────
# (0.9.4) Inbound delayed offer where SiphonAI OFFERS DTLS-SRTP in the
# 200 OK (we're the offerer). `[media].srtp = "required"` +
# `[media].srtp_offer = "dtls"` make the offer UDP/TLS/RTP/SAVPF +
# a=fingerprint + a=setup:actpass; SIPp answers DTLS in the ACK and
# SiphonAI enables the handshake. The scenario's check_it asserts the
# 200 carried `a=fingerprint:sha-256`; pass also requires the daemon to
# log the delayed accept. (SIPp doesn't run a real DTLS handshake.)
echo
echo "─── auxiliary phase: delayed_offer_dtls ───────────────"
DOD_WS_PORT=8791
DOD_WS_LOG=$(mktemp -t echo-ws-dod.XXXXXX.log)
DOD_DAEMON_LOG=$(mktemp -t siphon-ai-dod.XXXXXX.log)
DOD_CONFIG=$(mktemp -t siphon-ai-dod.XXXXXX.toml)
cat >"$DOD_CONFIG" <<EOF
[node]
id = "siphon-ai-sipp-dod"
[sip]
listen = "127.0.0.1:$DAEMON_PORT"
[media]
codecs = ["pcmu"]
srtp = "required"
srtp_offer = "dtls"
[bridge]
ws_url = "ws://127.0.0.1:$DOD_WS_PORT/"
[[route]]
name = "default"
[route.match]
any = true
EOF

DOD_PYTHON="$REPO_ROOT/examples/echo-ws-server-python/.venv/bin/python"
[[ -x "$DOD_PYTHON" ]] || DOD_PYTHON=python3
"$DOD_PYTHON" "$REPO_ROOT/examples/echo-ws-server-python/server.py" \
    --bind "127.0.0.1:$DOD_WS_PORT" >"$DOD_WS_LOG" 2>&1 &
DOD_WS_PID=$!

RUST_LOG=siphon_ai=info "$DAEMON_BIN" --config "$DOD_CONFIG" \
    >"$DOD_DAEMON_LOG" 2>&1 &
DOD_DAEMON_PID=$!
dod_cleanup() {
    kill "$DOD_WS_PID" "$DOD_DAEMON_PID" 2>/dev/null || true
    wait "$DOD_WS_PID" "$DOD_DAEMON_PID" 2>/dev/null || true
}
trap dod_cleanup EXIT
sleep 1.2

total=$((total + 1))
echo "─── delayed_offer_dtls ────────────────────────────────"
if sipp -i 127.0.0.1 -sf "$SCRIPT_DIR/delayed_offer_dtls_caller.xml" -m 1 -timeout 12s -trace_err \
        -p "$SIPP_PORT" -s 1000 "127.0.0.1:$DAEMON_PORT" >/dev/null 2>&1 \
    && grep -q "delayed-offer 200 OK sent; awaiting ACK answer" "$DOD_DAEMON_LOG"; then
    echo "  OK"
else
    echo "  FAIL (daemon: $DOD_DAEMON_LOG; ws: $DOD_WS_LOG)"
    failures=$((failures + 1))
fi

dod_cleanup
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

    # Same venv-then-system fallback as the outbound/attended phases —
    # local runs without the CI-prepped venv just need `websockets`
    # importable by python3.
    AUX_PYTHON="$REPO_ROOT/examples/echo-ws-server-python/.venv/bin/python"
    [[ -x "$AUX_PYTHON" ]] || AUX_PYTHON=python3
    "$AUX_PYTHON" \
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

# ─── Always-on auxiliary phase: pause-mode barge-in ───────────────
# Exercises reversible barge-in (0.32.0, DESIGN_REVERSIBLE_BARGE_IN.md)
# end-to-end with REAL caller media — the one thing the signaling-only
# scenarios can't do, because forge-vad needs actual speech energy:
#   * gen_tone_pcap.py synthesizes ~3 s of G.711 tone RTP at run time
#     (stdlib only; no binary audio in the repo),
#   * SIPp replays it as the caller — the echo WS bridges it back, so
#     the bot is mid-playout when the VAD fires,
#   * the pause arbitration arms; the echo server's default verdict
#     (barge_in_reject) resumes the retained playout.
# Pass = the SIPp scenario completed AND
# siphon_ai_barge_in_decisions_total{outcome="rejected"} ticked.
echo
echo "─── auxiliary phase: barge_in_pause ───────────────────"
BI_WS_PORT=8793
BI_OBS_PORT=9091
BI_WS_LOG=$(mktemp -t echo-ws-bi.XXXXXX.log)
BI_DAEMON_LOG=$(mktemp -t siphon-ai-bi.XXXXXX.log)
BI_CONFIG=$(mktemp -t siphon-ai-bi.XXXXXX.toml)
BI_PCAP=$(mktemp -t siphon-ai-bi.XXXXXX.pcap)
BI_XML=$(mktemp -t siphon-ai-bi.XXXXXX.xml)

python3 "$SCRIPT_DIR/gen_tone_pcap.py" "$BI_PCAP" 3.0
sed "s|__PCAP__|$BI_PCAP|" \
    "$SCRIPT_DIR/barge_in_pause_caller.xml" >"$BI_XML"

cat >"$BI_CONFIG" <<EOF
[node]
id = "siphon-ai-sipp-bi"
[sip]
listen = "127.0.0.1:$DAEMON_PORT"
[media]
codecs = ["pcmu"]
[bridge]
ws_url = "ws://127.0.0.1:$BI_WS_PORT/"
[bridge.barge_in]
mode = "pause"
# Generous window so a slow CI box can't race the echo's verdict into
# a timeout; the reject normally lands within a few ms.
decision_ms = 2000
[observability]
enabled = true
http_listen = "127.0.0.1:$BI_OBS_PORT"
[[route]]
name = "default"
[route.match]
any = true
EOF

BI_PYTHON="$REPO_ROOT/examples/echo-ws-server-python/.venv/bin/python"
[[ -x "$BI_PYTHON" ]] || BI_PYTHON=python3
"$BI_PYTHON" "$REPO_ROOT/examples/echo-ws-server-python/server.py" \
    --bind "127.0.0.1:$BI_WS_PORT" \
    >"$BI_WS_LOG" 2>&1 &
BI_WS_PID=$!

RUST_LOG=siphon_ai=info "$DAEMON_BIN" --config "$BI_CONFIG" \
    >"$BI_DAEMON_LOG" 2>&1 &
BI_DAEMON_PID=$!
bi_cleanup() {
    kill "$BI_WS_PID" "$BI_DAEMON_PID" 2>/dev/null || true
    wait "$BI_WS_PID" "$BI_DAEMON_PID" 2>/dev/null || true
}
trap bi_cleanup EXIT
sleep 1.2

total=$((total + 1))
echo "─── barge_in_pause ───────────────────────────────────"
bi_ok=0
if sipp -i 127.0.0.1 -sf "$BI_XML" -m 1 -timeout 15s -trace_err \
        -p "$SIPP_PORT" -s 1000 "127.0.0.1:$DAEMON_PORT" >/dev/null 2>&1; then
    # The echo's reject must have resolved the arbitration.
    if curl -s "http://127.0.0.1:$BI_OBS_PORT/metrics" \
        | grep -q 'siphon_ai_barge_in_decisions_total{outcome="rejected"}'; then
        bi_ok=1
    fi
fi
if (( bi_ok )); then
    echo "  OK"
else
    echo "  FAIL (daemon: $BI_DAEMON_LOG; ws: $BI_WS_LOG)"
    failures=$((failures + 1))
fi

bi_cleanup
trap - EXIT

# ─── Always-on auxiliary phase: registration admin ────────────────
# Exercises the 0.33.0 registration write actions end-to-end
# (DESIGN_REGISTRATION_ADMIN.md §6) with SIPp as the REGISTRAR — the
# suite's first registrar-side scenario. The [[register]] block is
# parked (register_on_startup = false):
#   * POST …/refresh starts the parked binding (its FIRST REGISTER —
#     the "tell to register" RPC), and the status flips to registered;
#   * POST …/restart drives REGISTER Expires:0 then a fresh REGISTER
#     (the scenario asserts the Expires:0 on the wire);
#   * an unknown name 404s;
#   * siphon_ai_register_admin_triggers_total ticks for both actions.
echo
echo "─── auxiliary phase: registration_admin ───────────────"
RA_OBS_PORT=9091
RA_DAEMON_LOG=$(mktemp -t siphon-ai-ra.XXXXXX.log)
RA_CONFIG=$(mktemp -t siphon-ai-ra.XXXXXX.toml)
cat >"$RA_CONFIG" <<EOF
[node]
id = "siphon-ai-sipp-ra"
[sip]
listen = "127.0.0.1:$DAEMON_PORT"
[media]
codecs = ["pcmu"]
[bridge]
ws_url = "ws://127.0.0.1:8765/"
[[register]]
name = "pbx-a"
server = "127.0.0.1"
port = $SIPP_PORT
transport = "udp"
username = "siphon"
password = "harness"
expires_secs = 3600
register_on_startup = false
[observability]
enabled = true
http_listen = "127.0.0.1:$RA_OBS_PORT"
[admin]
listen = "127.0.0.1:$ADMIN_API_PORT"
[[admin.token]]
name = "harness"
token = "$ADMIN_TOKEN"
role = "admin"
[[route]]
name = "default"
[route.match]
any = true
EOF

RUST_LOG=siphon_ai=info "$DAEMON_BIN" --config "$RA_CONFIG" \
    >"$RA_DAEMON_LOG" 2>&1 &
RA_DAEMON_PID=$!
# SIPp is the registrar; no remote target (it only answers).
sipp -i 127.0.0.1 -sf "$SCRIPT_DIR/register_admin_uas.xml" \
    -m 1 -timeout 20s -trace_err -p "$SIPP_PORT" >/dev/null 2>&1 &
RA_SIPP_PID=$!
ra_cleanup() {
    kill "$RA_DAEMON_PID" "$RA_SIPP_PID" 2>/dev/null || true
    wait "$RA_DAEMON_PID" "$RA_SIPP_PID" 2>/dev/null || true
}
trap ra_cleanup EXIT
sleep 1.2

total=$((total + 1))
echo "─── register_admin ───────────────────────────────────"
ra_ok=0
ra_auth=(-H "Authorization: Bearer $ADMIN_TOKEN")
ra_base="http://127.0.0.1:$ADMIN_API_PORT/admin/v1/registrations"
# Parked: no REGISTER yet, status disabled.
ra_status=$(curl -s "${ra_auth[@]}" "http://127.0.0.1:$ADMIN_API_PORT/admin/registrations" \
    | grep -o '"status":"[a-z]*"' | head -1)
# Unknown name → 404.
ra_404=$(curl -s -o /dev/null -w "%{http_code}" -X POST "${ra_auth[@]}" "$ra_base/nope/refresh")
# Refresh starts the parked binding.
ra_refresh=$(curl -s -o /dev/null -w "%{http_code}" -X POST "${ra_auth[@]}" "$ra_base/pbx-a/refresh")
sleep 1.5
ra_registered=$(curl -s "${ra_auth[@]}" "http://127.0.0.1:$ADMIN_API_PORT/admin/registrations" \
    | grep -c '"status":"registered"')
# Restart drives Expires:0 + fresh REGISTER (asserted inside the
# SIPp scenario).
ra_restart=$(curl -s -o /dev/null -w "%{http_code}" -X POST "${ra_auth[@]}" "$ra_base/pbx-a/restart")
if [[ "$ra_status" == '"status":"disabled"' && "$ra_404" == "404" \
      && "$ra_refresh" == "202" && "$ra_registered" == "1" \
      && "$ra_restart" == "202" ]] && wait "$RA_SIPP_PID"; then
    if curl -s "http://127.0.0.1:$RA_OBS_PORT/metrics" \
        | grep -q 'siphon_ai_register_admin_triggers_total{name="pbx-a",action="restart"} 1'; then
        ra_ok=1
    fi
fi
if (( ra_ok )); then
    echo "  OK"
else
    echo "  FAIL (status=$ra_status 404=$ra_404 refresh=$ra_refresh registered=$ra_registered restart=$ra_restart; daemon: $RA_DAEMON_LOG)"
    failures=$((failures + 1))
fi

ra_cleanup
trap - EXIT

# ─── Always-on auxiliary phase: graceful drain ────────────────────
# Exercises the 0.17.0 SIGTERM drain end-to-end (DESIGN_GRACEFUL_SHUTDOWN
# §5):
#   * a call still up at the deadline is force-terminated *gracefully* —
#     the daemon sends it a real BYE (drain_graceful_bye.xml),
#   * a NEW INVITE arriving mid-drain is rejected 503 (drain_invite_503),
#   * the daemon exits within the drain window + grace.
# A short drain_timeout_secs keeps the phase quick. Self-contained echo
# WS on its own port. The straggler SIPp uses a different local port than
# the 503 probe so the two instances don't fight over one bind.
echo
echo "─── auxiliary phase: graceful drain ───────────────────"
DRAIN_TIMEOUT_SECS=4
DRAIN_WS_PORT=8767
DRAIN_SIPP_PORT=5082          # straggler UAC (distinct from $SIPP_PORT)
DRAIN_WS_LOG=$(mktemp -t echo-ws-drain.XXXXXX.log)
DRAIN_DAEMON_LOG=$(mktemp -t siphon-ai-drain.XXXXXX.log)
DRAIN_CONFIG=$(mktemp -t siphon-ai-drain.XXXXXX.toml)
cat >"$DRAIN_CONFIG" <<EOF
[node]
id = "siphon-ai-sipp-drain"
[sip]
listen = "127.0.0.1:$DAEMON_PORT"
[media]
codecs = ["pcmu"]
[bridge]
ws_url = "ws://127.0.0.1:$DRAIN_WS_PORT/"
[shutdown]
drain_timeout_secs = $DRAIN_TIMEOUT_SECS
[[route]]
name = "default"
[route.match]
any = true
EOF

DRAIN_PYTHON="$REPO_ROOT/examples/echo-ws-server-python/.venv/bin/python"
[[ -x "$DRAIN_PYTHON" ]] || DRAIN_PYTHON=python3
"$DRAIN_PYTHON" \
    "$REPO_ROOT/examples/echo-ws-server-python/server.py" \
    --bind "127.0.0.1:$DRAIN_WS_PORT" \
    >"$DRAIN_WS_LOG" 2>&1 &
DRAIN_WS_PID=$!

RUST_LOG=siphon_ai=info "$DAEMON_BIN" --config "$DRAIN_CONFIG" \
    >"$DRAIN_DAEMON_LOG" 2>&1 &
DRAIN_DAEMON_PID=$!
drain_cleanup() {
    kill "$DRAIN_WS_PID" "$DRAIN_DAEMON_PID" 2>/dev/null || true
    wait "$DRAIN_WS_PID" "$DRAIN_DAEMON_PID" 2>/dev/null || true
}
trap drain_cleanup EXIT
sleep 1.2

# 1) Place a call that stays up — it becomes the deadline straggler.
sipp -i 127.0.0.1 -sf "$SCRIPT_DIR/drain_graceful_bye.xml" \
    -m 1 -timeout 20s -trace_err \
    -p "$DRAIN_SIPP_PORT" -s 1000 \
    "127.0.0.1:$DAEMON_PORT" >/dev/null 2>&1 &
DRAIN_STRAGGLER_PID=$!
sleep 1.5     # let the call establish before we pull the trigger

# 2) Trigger the drain.
echo "  sending SIGTERM to draining daemon (pid $DRAIN_DAEMON_PID)"
kill -TERM "$DRAIN_DAEMON_PID" 2>/dev/null || true

# 3) New INVITE mid-drain → 503.
sleep 0.4
total=$((total + 1))
echo "─── drain_invite_503 ─────────────────────────────────"
if sipp -i 127.0.0.1 -sf "$SCRIPT_DIR/drain_invite_503.xml" \
        -m 1 -timeout 8s -trace_err \
        -p "$SIPP_PORT" -s 1000 \
        "127.0.0.1:$DAEMON_PORT" >/dev/null 2>&1; then
    echo "  OK"
else
    echo "  FAIL (new INVITE not 503'd; daemon: $DRAIN_DAEMON_LOG)"
    failures=$((failures + 1))
fi

# 4) The straggler must receive a graceful BYE at the deadline.
total=$((total + 1))
echo "─── drain_graceful_bye ───────────────────────────────"
if wait "$DRAIN_STRAGGLER_PID"; then
    echo "  OK"
else
    echo "  FAIL (straggler got no BYE; daemon: $DRAIN_DAEMON_LOG)"
    failures=$((failures + 1))
fi

# 5) The daemon must exit on its own within the drain window + grace.
total=$((total + 1))
echo "─── drain_daemon_exits ───────────────────────────────"
exited=0
for _ in $(seq 1 24); do      # up to ~12s (timeout 4s + grace 2s + slack)
    if ! kill -0 "$DRAIN_DAEMON_PID" 2>/dev/null; then
        exited=1
        break
    fi
    sleep 0.5
done
if (( exited == 1 )); then
    echo "  OK"
else
    echo "  FAIL (daemon still alive after drain; log: $DRAIN_DAEMON_LOG)"
    failures=$((failures + 1))
fi

drain_cleanup
trap - EXIT

echo
if (( failures == 0 )); then
    echo "all $total scenarios passed"
    exit 0
fi
echo "$failures of $total scenarios failed"
echo "daemon log: $DAEMON_LOG"
exit 1
