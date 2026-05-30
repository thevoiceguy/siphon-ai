#!/usr/bin/env bash
#
# Scenario 3 — mTLS WebSocket with SPKI-pinned client cert.
#
# Validates the [bridge.tls] connector path end-to-end:
#   1. Generates a throwaway CA + server cert + client cert + SPKI pin.
#   2. Spins up a local TLS WebSocket server (Python) that
#      requires a client cert signed by the test CA.
#   3. Starts siphon-ai with [bridge.tls] pointing at the client
#      cert/key and pinning the server's SPKI hash.
#   4. Drives a single SIPp UAC INVITE → 200 OK → ACK → BYE.
#   5. Verifies the WS server logged the inbound connection (the
#      bridge connected successfully) and that the call dialog
#      completed cleanly.
#   6. Re-runs steps 3-5 with a deliberately-wrong pin; expects
#      the bridge to refuse to connect (the call still completes
#      SIP-wise — siphon-ai's bridge-failure policy is configurable
#      — but the WS server should NOT see a connection attempt).
#
# Fully automated. Runs in ~30 seconds on a warm cache.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
# shellcheck source=lib/common.sh
source "$SCRIPT_DIR/lib/common.sh"

DAEMON_BIN="${DAEMON_BIN:-$REPO_ROOT/target/debug/siphon-ai}"
[[ -x "$DAEMON_BIN" ]] || fail "siphon-ai binary missing at $DAEMON_BIN — run \`cargo build -p siphon-ai\`"
command -v sipp     >/dev/null || fail "sipp not installed — \`apt install sip-tester\`"
command -v openssl  >/dev/null || fail "openssl missing"
command -v python3  >/dev/null || fail "python3 missing"
python3 -c 'import websockets, ssl' 2>/dev/null \
  || fail "python websockets module missing — \`apt install python3-websockets\`"

# ─── Setup ────────────────────────────────────────────────────────────────

step "Scenario 3 — mTLS WSS with SPKI pin"

WORK="$SCRIPT_DIR/.work/scenario-3"
rm -rf "$WORK" && mkdir -p "$WORK/certs" "$WORK/logs"
on_exit_cleanup "rm -rf '$WORK'"

note "Generating throwaway PKI under $WORK/certs/"
gen_test_pki "$WORK/certs/server" "wss-server.localhost"
gen_test_pki "$WORK/certs/client" "siphon-ai-bridge-client"

SERVER_PIN=$(< "$WORK/certs/server/leaf.spki.sha256")
ok "Server SPKI pin: $SERVER_PIN"

# Pick free ports for everything.
WSS_PORT=$(pick_port 18443)
SIP_PORT=$(pick_port 15060)
RTP_LO=$(pick_port 30000)
RTP_HI=$(( RTP_LO + 200 ))
SIPP_PORT=$(pick_port 15080)
note "Ports — WSS=$WSS_PORT, SIP=$SIP_PORT, RTP=${RTP_LO}-${RTP_HI}, SIPp=$SIPP_PORT"

# ─── TLS WS server (Python) ──────────────────────────────────────────────

cat > "$WORK/wss-server.py" <<'PY'
import asyncio, ssl, os
import websockets

WORK = os.environ['WORK']
PORT = int(os.environ['WSS_PORT'])

# `PROTOCOL_TLS_SERVER` + manual cert config is more tolerant than
# `create_default_context(CLIENT_AUTH)`, which sets strict modern
# defaults that don't always interop with rustls in our deployment.
ssl_ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
ssl_ctx.load_cert_chain(f"{WORK}/certs/server/leaf.pem", f"{WORK}/certs/server/leaf.key")
ssl_ctx.load_verify_locations(f"{WORK}/certs/client/ca.pem")
ssl_ctx.verify_mode = ssl.CERT_REQUIRED       # require client cert signed by our CA
ssl_ctx.check_hostname = False                # server side, irrelevant

connected = 0

async def handler(ws):
    # Just sink whatever the bridge sends. siphon-ai's connection
    # alone is the signal we care about for this test.
    global connected
    connected += 1
    print(f"WSS: client connected (count={connected})", flush=True)
    try:
        async for _ in ws:
            pass
    except Exception:
        pass

async def main():
    # `subprotocols=[...]` is REQUIRED for the bridge to complete
    # the WS upgrade. The bridge sends `Sec-WebSocket-Protocol:
    # siphon-ai.v1`; without an exact-match server-side allowlist,
    # Python websockets rejects the upgrade after the TLS handshake
    # has completed, producing a rustls-side "peer closed connection
    # without close_notify" error that takes a while to track down.
    async with websockets.serve(handler, "127.0.0.1", PORT, ssl=ssl_ctx,
                                 subprotocols=["siphon-ai.v1"]):
        print(f"WSS: listening on 127.0.0.1:{PORT}", flush=True)
        await asyncio.Future()

asyncio.run(main())
PY

WORK="$WORK" WSS_PORT="$WSS_PORT" python3 "$WORK/wss-server.py" \
  > "$WORK/logs/wss.log" 2>&1 &
WSS_PID=$!
on_exit_cleanup --always "kill -9 $WSS_PID"

wait_for "grep -q 'WSS: listening' '$WORK/logs/wss.log'" 5 \
  || fail "WSS server didn't start (see $WORK/logs/wss.log)"
ok "WSS server up on 127.0.0.1:$WSS_PORT"

# ─── siphon-ai config (good pin) ─────────────────────────────────────────

write_daemon_config() {
  local pin="$1" outpath="$2"
  cat > "$outpath" <<EOF
[node]
id             = "tls-scenario-3"
public_address = "127.0.0.1"

[sip]
listen     = "127.0.0.1:$SIP_PORT"
transports = ["udp"]

[media]
codecs                  = ["pcmu", "pcma"]
dtmf                    = "rfc2833"
rtp_port_range          = [$RTP_LO, $RTP_HI]
inactivity_timeout_secs = 30

[bridge]
ws_url                = "wss://wss-server.localhost:$WSS_PORT/"
ws_connect_timeout_ms = 3000

[bridge.tls]
client_cert   = "$WORK/certs/client/leaf.pem"
client_key    = "$WORK/certs/client/leaf.key"
pinned_sha256 = "$pin"

[observability]
enabled     = true
http_listen = "127.0.0.1:$(pick_port 19091)"

# CDR file sink — the bridge's per-call disconnect reason lands
# in termination.bridge_disconnect and is the load-bearing
# assertion target (the daemon's structured log doesn't surface
# the rustls error string, but the CDR does).
[cdr]
enabled = true

[cdr.file]
enabled = true
path    = "$WORK/cdr-$pin.jsonl"

[[trunk]]
name       = "sipp-loopback"
peer_addrs = ["127.0.0.1"]

[[route]]
name = "default"
[route.match]
any = true
EOF
}

# ─── Phase 1 — correct pin → expect success ──────────────────────────────

step "Phase 1: correct pin"

write_daemon_config "$SERVER_PIN" "$WORK/siphon-ai-good.toml"

# Resolve wss-server.localhost → 127.0.0.1 for the daemon's TLS hostname
# verification (we set SAN=wss-server.localhost in the server cert).
# Use a hostalias env-var if supported, else a /etc/hosts entry would
# be required — but we sidestep it by using the cert's localhost SAN.
sed -i 's|wss-server.localhost|localhost|g' "$WORK/siphon-ai-good.toml"

"$DAEMON_BIN" --config "$WORK/siphon-ai-good.toml" \
  > "$WORK/logs/daemon-good.log" 2>&1 &
DAEMON_PID=$!
on_exit_cleanup --always "kill -9 $DAEMON_PID"

wait_for "ss -lnHu sport = :${SIP_PORT} | grep -q ." 5 \
  || fail "siphon-ai didn't bind SIP socket (see $WORK/logs/daemon-good.log)"
ok "siphon-ai up on 127.0.0.1:$SIP_PORT"

# Drive a single call via SIPp UAC against the daemon. The
# bundled basic_call_then_bye scenario is fine.
( cd "$WORK/logs" && sipp -sf "$REPO_ROOT/test-harness/sipp-scenarios/basic_call_then_bye.xml" \
  127.0.0.1:"$SIP_PORT" \
  -p "$SIPP_PORT" \
  -m 1 -trace_err > sipp-good.log 2>&1 ) || true   # SIPp may exit nonzero if the
# daemon BYE'd before SIPp got to its own BYE step (which happens once the bridge
# sends `stop` — see CDR assertion below). The state-machine match doesn't gate
# the assertion; the WSS log + CDR do.

sleep 1   # let the daemon's connect / disconnect logs flush

# Assert: WSS server saw the connection.
if grep -q "WSS: client connected" "$WORK/logs/wss.log"; then
  ok "WSS server received mTLS bridge connection"
else
  fail "WSS server never saw a connection. WSS log tail:
$(tail -10 "$WORK/logs/wss.log" | sed 's/^/    /')
Daemon log tail:
$(tail -20 "$WORK/logs/daemon-good.log" | sed 's/^/    /')"
fi

# Assert: CDR confirms a clean bridge lifecycle. Successful disconnect
# reasons are `stop_sent` / `server_closed` / `controller_hung_up`;
# anything starting with `error:` means TLS / cert / pin / WS-upgrade
# failed and the WSS-server-log match above was a fluke we'd want to
# investigate. The CDR is the authoritative wire-side record because
# the daemon's structured log doesn't surface bridge error strings.
cdr_good="$WORK/cdr-${SERVER_PIN}.jsonl"
if [[ ! -s "$cdr_good" ]]; then
  fail "No CDR record written at $cdr_good — the call didn't complete its lifecycle"
fi
bridge_good=$(jq -r '.termination.bridge_disconnect' "$cdr_good" | tail -1)
case "$bridge_good" in
  stop_sent|server_closed|controller_hung_up)
    ok "CDR confirms clean bridge disconnect: $bridge_good" ;;
  error:*)
    fail "CDR shows bridge error despite WSS connection: $bridge_good" ;;
  *)
    warn "CDR bridge_disconnect = $bridge_good (unrecognised but not an error)" ;;
esac

# Teardown phase 1 daemon (keep WSS up for phase 2).
kill -9 "$DAEMON_PID" 2>/dev/null || true
# Wait for the daemon to be properly reaped before reusing its
# ports. `kill -9` is immediate but the OS takes a moment to
# release UDP socket bindings and clean up TCP TIME_WAIT entries
# on the obs/HTTP port. Without this sleep, phase 2's daemon
# sometimes appears to start (logs "daemon ready") but the
# kernel hasn't fully released the prior process's UDP socket,
# leaving us in a state where `ss` doesn't show the new binding.
wait_for "! ss -lnHu sport = :${SIP_PORT} | grep -q ." 5 || true
sleep 1

# ─── Phase 2 — wrong pin → expect bridge connect failure ─────────────────

step "Phase 2: deliberately-wrong pin"

# Same shape as the real pin but a single hex char flipped at the end.
BAD_PIN="${SERVER_PIN:0:63}$(printf '%x' $(( (16#${SERVER_PIN: -1} + 1) % 16 )))"
note "Bad pin (last char flipped): $BAD_PIN"

write_daemon_config "$BAD_PIN" "$WORK/siphon-ai-bad.toml"
sed -i 's|wss-server.localhost|localhost|g' "$WORK/siphon-ai-bad.toml"

"$DAEMON_BIN" --config "$WORK/siphon-ai-bad.toml" \
  > "$WORK/logs/daemon-bad.log" 2>&1 &
DAEMON_PID=$!
on_exit_cleanup --always "kill -9 $DAEMON_PID"

wait_for "ss -lnHu sport = :${SIP_PORT} | grep -q ." 5 \
  || fail "siphon-ai didn't bind SIP socket (see $WORK/logs/daemon-bad.log)"

# Snapshot WSS connection count before the call so we can tell
# whether the bad-pin call adds one.
CONN_BEFORE=$(grep -c "WSS: client connected" "$WORK/logs/wss.log" || true)

( cd "$WORK/logs" && sipp -sf "$REPO_ROOT/test-harness/sipp-scenarios/basic_call_then_bye.xml" \
  127.0.0.1:"$SIP_PORT" \
  -p "$SIPP_PORT" \
  -m 1 -trace_err > sipp-bad.log 2>&1 ) || true   # call may or may not complete; we don't assert here

sleep 1

CONN_AFTER=$(grep -c "WSS: client connected" "$WORK/logs/wss.log" || true)

if (( CONN_AFTER == CONN_BEFORE )); then
  ok "WSS server saw zero new connections (bad-pin call did NOT establish bridge)"
else
  fail "WSS saw $(( CONN_AFTER - CONN_BEFORE )) new connection(s) under bad pin — pin verification not enforced"
fi

# Assert: CDR captures the TLS verification failure as a bridge
# error. The error string from rustls comes through verbatim in
# `termination.bridge_disconnect`. Substring match on words that
# specifically indicate certificate / pin / TLS verification —
# distinct from a plain connection-refused or timeout error.
cdr_bad="$WORK/cdr-${BAD_PIN}.jsonl"
if [[ ! -s "$cdr_bad" ]]; then
  fail "No CDR record written at $cdr_bad — the call didn't complete its lifecycle"
fi
bridge_bad=$(jq -r '.termination.bridge_disconnect' "$cdr_bad" | tail -1)
if printf '%s' "$bridge_bad" | grep -qiE 'tls|cert|pin|verif|trust|spki|close_notify'; then
  ok "CDR captures TLS-layer bridge failure: $bridge_bad"
else
  fail "CDR bridge_disconnect doesn't look like a TLS failure: $bridge_bad
Expected a substring matching tls / cert / pin / verif / trust / spki / close_notify."
fi

verdict_pass "mTLS connector enforces SPKI pin (good pin connects, bad pin doesn't)"
