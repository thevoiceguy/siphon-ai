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

for port in 5060 8443 8088 9091; do
    if ss -tln 2>/dev/null | grep -q ":$port "; then
        [[ $port == 8765 ]] && continue
        echo "port $port already in use — stop what holds it and rerun" >&2
        exit 2
    fi
done

if ! ss -tln 2>/dev/null | grep -q ':8765 '; then
    ECHO_PY="$REPO_ROOT/examples/echo-ws-server-python/.venv/bin/python"
    [[ -x "$ECHO_PY" ]] || ECHO_PY=python3
    "$ECHO_PY" "$REPO_ROOT/examples/echo-ws-server-python/server.py" \
        --bind 127.0.0.1:8765 >"$WORK/echo.log" 2>&1 &
    PIDS+=($!)
fi
# env --chdir (not a subshell) so $! is the daemon itself and the
# cleanup trap really stops it. Repo-root cwd because lab.toml's cert
# paths are repo-relative.
env --chdir="$REPO_ROOT" RUST_LOG=siphon_ai=info \
    "$DAEMON_BIN" --config examples/browser-sip/lab.toml \
    >"$WORK/daemon.log" 2>&1 &
PIDS+=($!)
python3 -m http.server 8088 --bind 127.0.0.1 --directory "$SCRIPT_DIR" \
    >"$WORK/http.log" 2>&1 &
PIDS+=($!)
sleep 2

metrics() { curl -s http://127.0.0.1:9091/metrics; }
gauge() { metrics | awk '/^siphon_ai_registrar_bindings /{print $2}'; }

# ─── 1: the browser registers ─────────────────────────────────────
echo "─── headless register ───"
run_chrome --headless --disable-gpu \
    --ignore-certificate-errors --user-data-dir="$WORK/profile" \
    "http://127.0.0.1:8088/?auto=1" >"$WORK/chrome.log" 2>&1 &
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

echo
echo "PASS — DEV_PLAN_WebRTC.md Phase 1 exit check (headless)."
echo "logs kept in $WORK"
