#!/usr/bin/env bash
# The Phase 1 exit check, self-driving — for a box with no display and
# no Chrome. Boots the whole lab (echo WS server, daemon on lab.toml,
# page server), drives the SIP.js page with headless Chromium, and
# asserts the daemon-side truth:
#
#   1. the browser REGISTERs over WSS  -> siphon_ai_registrar_bindings 1
#   2. the browser is killed           -> "registration expired
#      (connection lost)" and the gauge returns to 0
#
# Chromium resolution order:
#   $CHROMIUM -> chromium / chromium-browser / google-chrome on PATH ->
#   Playwright's downloaded headless shell (no root needed: the two
#   missing system libs, libnss3/libnspr4, are fetched with
#   `apt-get download` and dpkg-extracted into a local dir).
#
# Needs: cargo build -p siphon-ai (debug binary), python3, node/npx for
# the Playwright fallback, network (jsdelivr serves SIP.js).
set -uo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
WORK="${TMPDIR:-/tmp}/browser-sip-headless.$$"
mkdir -p "$WORK"

DAEMON_BIN="${DAEMON_BIN:-$REPO_ROOT/target/debug/siphon-ai}"
[[ -x "$DAEMON_BIN" ]] || { echo "build first: cargo build -p siphon-ai" >&2; exit 2; }
[[ -f "$SCRIPT_DIR/certs/wss-key.pem" ]] || "$SCRIPT_DIR/gen-cert.sh"

# Every port is overridable, because this script is routinely run on a
# box that already has a siphon-ai on it (the same lesson run-all.sh
# learned in #541 — a colliding run is the expected case). The config
# is copied with these values substituted, so lab.toml itself stays
# the readable reference.
SIP_PORT="${SIP_PORT:-5070}"
WSS_PORT="${WSS_PORT:-8443}"
PAGE_PORT="${PAGE_PORT:-8088}"
OBS_PORT="${OBS_PORT:-9091}"
ECHO_PORT="${ECHO_PORT:-8765}"
ADMIN_PORT="${ADMIN_PORT:-9092}"

# ─── Chromium ─────────────────────────────────────────────────────
# Resolve a binary; when it's the Playwright fallback, the two NSS
# libraries Debian's server install lacks are fetched without root
# (apt-get download + dpkg -x) and injected via LD_LIBRARY_PATH.
PW_DIR="$SCRIPT_DIR/pw-browsers"
PW_LIBS="$SCRIPT_DIR/pw-libs"
CHROME=""
CHROME_LD=""
if [[ -n "${CHROMIUM:-}" ]]; then
    CHROME="$CHROMIUM"
else
    for c in chromium chromium-browser google-chrome; do
        if command -v "$c" >/dev/null 2>&1; then CHROME=$(command -v "$c"); break; fi
    done
fi
if [[ -z "$CHROME" ]]; then
    CHROME=$(find "$PW_DIR" -name headless_shell 2>/dev/null | head -1)
    if [[ -z "$CHROME" ]]; then
        echo "downloading headless Chromium via Playwright (one-time)…" >&2
        PLAYWRIGHT_BROWSERS_PATH="$PW_DIR" npx -y playwright@1.49.1 install chromium >&2 || true
        CHROME=$(find "$PW_DIR" -name headless_shell 2>/dev/null | head -1)
        [[ -n "$CHROME" ]] || { echo "chromium download failed" >&2; exit 2; }
    fi
    if ldd "$CHROME" 2>/dev/null | grep -q "not found"; then
        if [[ ! -f "$PW_LIBS/usr/lib/x86_64-linux-gnu/libnss3.so" ]]; then
            echo "fetching libnss3/libnspr4 (no root: apt-get download)…" >&2
            mkdir -p "$PW_LIBS" && (
                cd "$PW_LIBS" && apt-get download libnss3 libnspr4 >&2 &&
                for d in *.deb; do dpkg -x "$d" .; done && rm -f ./*.deb
            ) || { echo "lib fetch failed" >&2; exit 2; }
        fi
        CHROME_LD="$PW_LIBS/usr/lib/x86_64-linux-gnu"
        if LD_LIBRARY_PATH="$CHROME_LD" ldd "$CHROME" 2>/dev/null | grep -q "not found"; then
            echo "chromium still missing libraries:" >&2
            LD_LIBRARY_PATH="$CHROME_LD" ldd "$CHROME" | grep "not found" >&2
            exit 2
        fi
    fi
fi
# Only ever backgrounded; exec so $! is chromium itself and killing it
# is killing the browser, not an intermediate subshell.
run_chrome() {
    if [[ -n "$CHROME_LD" ]]; then
        LD_LIBRARY_PATH="$CHROME_LD" exec "$CHROME" "$@"
    else
        exec "$CHROME" "$@"
    fi
}
echo "chromium: $CHROME"

# ─── The lab stack ────────────────────────────────────────────────
PIDS=()
cleanup() { for p in "${PIDS[@]}"; do kill "$p" 2>/dev/null; done; }
trap cleanup EXIT

declare -A PORT_VAR=([$SIP_PORT]=SIP_PORT [$WSS_PORT]=WSS_PORT
                     [$PAGE_PORT]=PAGE_PORT [$OBS_PORT]=OBS_PORT)
for port in "$SIP_PORT" "$WSS_PORT" "$PAGE_PORT" "$OBS_PORT"; do
    if ss -tln 2>/dev/null | grep -q ":$port "; then
        echo "port $port is already in use (${PORT_VAR[$port]}); rerun with e.g." >&2
        echo "  ${PORT_VAR[$port]}=$((port + 10)) $0" >&2
        exit 2
    fi
done

# lab.toml with this run's ports substituted, so the committed config
# stays the readable reference while a busy box picks free ports.
# Cert paths become absolute since the copy is read from $WORK.
CONFIG="$WORK/lab.toml"
sed -e "s|127\.0\.0\.1:5070|127.0.0.1:$SIP_PORT|" \
    -e "s|127\.0\.0\.1:8443|127.0.0.1:$WSS_PORT|" \
    -e "s|127\.0\.0\.1:8088|127.0.0.1:$PAGE_PORT|g" \
    -e "s|localhost:8088|localhost:$PAGE_PORT|g" \
    -e "s|127\.0\.0\.1:9091|127.0.0.1:$OBS_PORT|" \
    -e "s|127\.0\.0\.1:9092|127.0.0.1:$ADMIN_PORT|" \
    -e "s|127\.0\.0\.1:8765|127.0.0.1:$ECHO_PORT|" \
    -e "s|examples/browser-sip/certs|$SCRIPT_DIR/certs|g" \
    -e "s|examples/browser-sip/cdr.jsonl|$WORK/cdr.jsonl|" \
    "$SCRIPT_DIR/lab.toml" >"$CONFIG"

# The page needs the same treatment, and for the same reason: it dials
# the WSS port from a literal in the HTML, so serving the committed
# file straight from $SCRIPT_DIR made a WSS_PORT override silently
# half-apply — daemon moved, browser did not, and the failure looked
# like a TLS problem rather than a port one.
PAGE_DIR="$WORK/page"
mkdir -p "$PAGE_DIR"
sed -e "s|127\.0\.0\.1:8443|127.0.0.1:$WSS_PORT|g" \
    "$SCRIPT_DIR/index.html" >"$PAGE_DIR/index.html"

if ! ss -tln 2>/dev/null | grep -q ":$ECHO_PORT "; then
    ECHO_PY="$REPO_ROOT/examples/echo-ws-server-python/.venv/bin/python"
    [[ -x "$ECHO_PY" ]] || ECHO_PY=python3
    "$ECHO_PY" "$REPO_ROOT/examples/echo-ws-server-python/server.py" \
        --bind "127.0.0.1:$ECHO_PORT" >"$WORK/echo.log" 2>&1 &
    PIDS+=($!)
fi
# Not a subshell, so $! is the daemon itself and the cleanup trap
# really stops it (an orphaned daemon holds the ports for the rerun).
RUST_LOG=warn,siphon_ai=info "$DAEMON_BIN" --config "$CONFIG" \
    >"$WORK/daemon.log" 2>&1 &
PIDS+=($!)
python3 -m http.server "$PAGE_PORT" --bind 127.0.0.1 --directory "$PAGE_DIR" \
    >"$WORK/http.log" 2>&1 &
PIDS+=($!)
sleep 2

metrics() { curl -s "http://127.0.0.1:$OBS_PORT/metrics"; }
gauge() { metrics | awk '/^siphon_ai_registrar_bindings /{print $2}'; }

# ─── 1: the browser registers ─────────────────────────────────────
echo "─── headless register ───"
run_chrome --headless --disable-gpu \
    --ignore-certificate-errors --user-data-dir="$WORK/profile" \
    "http://127.0.0.1:$PAGE_PORT/?auto=1" >"$WORK/chrome.log" 2>&1 &
CHROME_PID=$!
PIDS+=("$CHROME_PID")

deadline=$((SECONDS + 30)); registered=0
while (( SECONDS < deadline )); do
    [[ "$(gauge)" == "1" ]] && { registered=1; break; }
    sleep 1
done
if (( ! registered )); then
    echo "FAIL: browser never registered (bindings=$(gauge))" >&2
    echo "  daemon: $WORK/daemon.log  chrome: $WORK/chrome.log" >&2
    exit 1
fi
echo "  OK — siphon_ai_registrar_bindings 1 (REGISTER over WSS, digest, Origin all real)"

# ─── 2: kill the browser, the binding must expire ─────────────────
echo "─── tab-close expiry ───"
kill "$CHROME_PID" 2>/dev/null; wait "$CHROME_PID" 2>/dev/null
deadline=$((SECONDS + 60)); expired=0
while (( SECONDS < deadline )); do
    [[ "$(gauge)" == "0" ]] && { expired=1; break; }
    sleep 2
done
if (( ! expired )); then
    echo "FAIL: binding never expired after browser death" >&2
    exit 1
fi
grep -q "registration expired (connection lost)" "$WORK/daemon.log" \
    && echo "  OK — expired via connection-loss grace (daemon log confirms)" \
    || echo "  OK — gauge returned to 0 (expiry path in daemon log differs)"

# ─── 3: a call draws a port pair from the pool (§4.4) ─────────────
#
# The three claims, checked against daemon truth rather than intent:
# the browser leg is counted in the capacity gauge, its socket binds
# inside `[media].rtp_port_range`, and the pair comes back when the
# leg ends. The range in lab.toml is deliberately narrow, because
# "landed in the range" only means something if the range is small.
echo "─── call: port-pool accounting ───"
pairs() { metrics | awk '/^siphon_ai_rtp_port_pairs_allocated /{print $2}'; }
active() { metrics | awk '/^siphon_ai_calls_active /{print $2}'; }

RANGE_LO=$(awk '/^rtp_port_range/{gsub(/[^0-9 ]/," ");print $1}' "$SCRIPT_DIR/lab.toml")
RANGE_HI=$(awk '/^rtp_port_range/{gsub(/[^0-9 ]/," ");print $2}' "$SCRIPT_DIR/lab.toml")
echo "  range from lab.toml: $RANGE_LO-$RANGE_HI"

baseline_pairs="$(pairs)"
if [[ -z "$baseline_pairs" ]]; then
    echo "FAIL: siphon_ai_rtp_port_pairs_allocated is not published" >&2
    exit 1
fi
if [[ "$baseline_pairs" != "0" ]]; then
    echo "FAIL: pool not idle before the call (allocated=$baseline_pairs)" >&2
    exit 1
fi

run_chrome --headless --disable-gpu \
    --use-fake-ui-for-media-stream --use-fake-device-for-media-stream \
    --ignore-certificate-errors --user-data-dir="$WORK/profile2" \
    "http://127.0.0.1:$PAGE_PORT/?auto=1&call=1" >"$WORK/chrome2.log" 2>&1 &
CHROME_PID=$!
PIDS+=("$CHROME_PID")

deadline=$((SECONDS + 45)); up=0
while (( SECONDS < deadline )); do
    [[ "$(active)" == "1" ]] && { up=1; break; }
    sleep 1
done
if (( ! up )); then
    echo "FAIL: browser call never became active (calls_active=$(active))" >&2
    echo "  daemon: $WORK/daemon.log  chrome: $WORK/chrome2.log" >&2
    exit 1
fi

# (a) counted in the gauge an operator watches for capacity.
# The gauge is *sampled* from pool truth on a timer rather than
# incremented at the allocation site (deliberately — a site-updated
# gauge under-counts under exactly the leak it exists to catch), so it
# lags the call by up to one sampler period. Poll rather than race it.
deadline=$((SECONDS + 20)); counted=0
while (( SECONDS < deadline )); do
    [[ "$(pairs)" == "1" ]] && { counted=1; break; }
    sleep 1
done
if (( ! counted )); then
    echo "FAIL: browser call not counted in the port pool (allocated=$(pairs), want 1)" >&2
    exit 1
fi
echo "  OK — siphon_ai_rtp_port_pairs_allocated 1 during the call"

# (b) bound inside the operator's firewalled range
RTP_PORT=$(grep -o 'rtp_port=[0-9]*' "$WORK/daemon.log" | tail -1 | cut -d= -f2)
if [[ -z "$RTP_PORT" ]]; then
    echo "FAIL: daemon never logged the leg's rtp_port" >&2
    exit 1
fi
if (( RTP_PORT < RANGE_LO || RTP_PORT > RANGE_HI )); then
    echo "FAIL: media bound to $RTP_PORT, outside [media].rtp_port_range $RANGE_LO-$RANGE_HI" >&2
    exit 1
fi
if (( RTP_PORT % 2 != 0 )); then
    echo "FAIL: rtp_port $RTP_PORT is odd" >&2
    exit 1
fi
echo "  OK — media bound to $RTP_PORT, inside $RANGE_LO-$RANGE_HI"

# The socket is not merely *claimed* to be there — it is listening.
if command -v ss >/dev/null 2>&1; then
    if ss -uln 2>/dev/null | grep -q ":$RTP_PORT\b"; then
        echo "  OK — a UDP socket really is bound on $RTP_PORT"
    else
        echo "  note: ss shows no socket on $RTP_PORT (may lack permission to see it)"
    fi
fi

# (d) while the call is up, the admin API says where its media is
# (§4.6). This is the sightglass surface: a browser call that never
# got audio is diagnosed by *which* phase it is stuck in, and that is
# only answerable while the call still exists.
admin() { curl -s -H "Authorization: Bearer lab-token-not-a-secret" \
    "http://127.0.0.1:$ADMIN_PORT/admin/v1$1"; }
state="$(admin /calls | python3 -c '
import json, sys
calls = json.load(sys.stdin)["calls"]
print(next((c.get("webrtc_state", "") for c in calls), ""))
')"
if [[ "$state" != "connected" ]]; then
    echo "FAIL: admin /calls reports webrtc_state=${state:-absent}, want connected" >&2
    admin /calls >&2
    exit 1
fi
echo "  OK — GET /admin/v1/calls: webrtc_state=connected"

# (c) the pair comes back when the leg ends — the leak Phase 0 is about
echo "─── call teardown returns the pair ───"
kill "$CHROME_PID" 2>/dev/null; wait "$CHROME_PID" 2>/dev/null
deadline=$((SECONDS + 90)); returned=0
while (( SECONDS < deadline )); do
    [[ "$(pairs)" == "0" && "$(active)" == "0" ]] && { returned=1; break; }
    sleep 2
done
if (( ! returned )); then
    echo "FAIL: pair not returned after the call (allocated=$(pairs), active=$(active))" >&2
    exit 1
fi
echo "  OK — pool back to 0 allocated, 0 calls active"

# ─── 4: the leg is observable (§4.6) ──────────────────────────────
#
# A browser call that produced no metrics would look identical to one
# that never happened. These are read *after* teardown so the end-of-leg
# observations (end reason, transcode cost) are in as well.
echo "─── leg observability ───"
connected="$(metrics | grep '^siphon_ai_webrtc_legs_total{' | grep 'result="connected"' | head -1)"
if [[ -z "$connected" ]]; then
    echo "FAIL: no siphon_ai_webrtc_legs_total{result=\"connected\"} after a call that carried audio" >&2
    metrics | grep '^siphon_ai_webrtc' >&2 || echo "  (no webrtc series at all)" >&2
    exit 1
fi
echo "  OK — $connected"

# The codec label has to be the one the leg actually negotiated, not a
# default: lab.toml leaves [webrtc].prefer_g711 off, so Chrome and the
# daemon land on Opus.
if ! grep -q 'codec="opus"' <<<"$connected"; then
    echo "FAIL: expected codec=\"opus\" on the connected leg, got: $connected" >&2
    exit 1
fi

# ICE and DTLS are timed apart — the reason the setup wait is
# event-driven. Both histograms must have observed this leg.
for h in siphon_ai_webrtc_ice_seconds siphon_ai_webrtc_dtls_seconds; do
    count="$(metrics | awk -v h="${h}_count" '$1==h{print $2}')"
    if [[ -z "$count" || "$count" == "0" ]]; then
        echo "FAIL: $h recorded nothing for a leg that connected (count=${count:-absent})" >&2
        exit 1
    fi
    sum="$(metrics | awk -v h="${h}_sum" '$1==h{print $2}')"
    echo "  OK — $h count=$count sum=${sum}s"
done

ended="$(metrics | grep '^siphon_ai_webrtc_legs_ended_total{' | head -1)"
if [[ -z "$ended" ]]; then
    echo "FAIL: the leg ended but siphon_ai_webrtc_legs_ended_total is absent" >&2
    exit 1
fi
echo "  OK — $ended"

# Transcode cost, recorded once per leg per direction.
for d in decode encode; do
    line="$(metrics | grep "^siphon_ai_webrtc_transcode_seconds_sum{" | grep "direction=\"$d\"" | head -1)"
    if [[ -z "$line" ]]; then
        echo "FAIL: no transcode cost recorded for direction=$d" >&2
        exit 1
    fi
    echo "  OK — $line"
done

# ─── 5: the browser call's CDR names the leg (§4.6, CDR v9) ───────
#
# The record is the only place an operator can later answer "was this
# call a browser, and was its audio encrypted?" — metrics aggregate it
# away. Read after teardown, because that is when the CDR is written.
echo "─── CDR: leg_transport + media_type ───"
CDR_FILE="$WORK/cdr.jsonl"
deadline=$((SECONDS + 20)); wrote=0
while (( SECONDS < deadline )); do
    [[ -s "$CDR_FILE" ]] && { wrote=1; break; }
    sleep 1
done
if (( ! wrote )); then
    echo "FAIL: no CDR written to $CDR_FILE after a completed browser call" >&2
    exit 1
fi
CDR_LINE="$(tail -1 "$CDR_FILE")"
read -r cdr_version cdr_transport cdr_media <<<"$(python3 - "$CDR_FILE" <<'PYEOF'
import json, sys
with open(sys.argv[1]) as f:
    rec = json.loads(f.readlines()[-1])
print(rec.get("version"), rec.get("leg_transport"), rec.get("media_type"))
PYEOF
)"
if [[ "$cdr_transport" != "wss" || "$cdr_media" != "webrtc" ]]; then
    echo "FAIL: browser call recorded as leg_transport=$cdr_transport media_type=$cdr_media" >&2
    echo "  want wss / webrtc — record: $CDR_LINE" >&2
    exit 1
fi
echo "  OK — CDR v$cdr_version: leg_transport=$cdr_transport media_type=$cdr_media"

echo
echo "PASS — DEV_PLAN_WebRTC.md Phase 1 exit check + §4.4 port accounting + §4.6 metrics, live leg state, and CDR v9 (headless)."
echo "logs kept in $WORK"
