#!/usr/bin/env bash
# tier3_run.sh <calls> <hold_seconds>
#
# LOAD_TEST_PLAN.md §10.3 — the live-carrier run, in its low-concurrency
# form. Places SEQUENTIAL trombone calls: originate through the carrier
# gateway to our own DID, so the call comes back in as a second leg and
# real media flows both ways with no human at either end.
#
# THIS DIALS THE PUBLIC PSTN AND BILLS BOTH LEGS OF EVERY CALL.
#
# One trombone at a time. A trombone occupies TWO channels (one out, one
# back in), so on a 3-channel trunk this leaves exactly one free — which is
# deliberate: a genuine inbound call must not be rejected because a test is
# running. Do not "parallelise" this without recounting channels.
set -uo pipefail
CALLS=${1:-60}; HOLD=${2:-30}
ADMIN=${ADMIN:-127.0.0.1:9092}
OBS=${OBS:-127.0.0.1:9091}
GATEWAY=${GATEWAY:?set GATEWAY to the carrier [[gateway]] name}
TO=${TO:?set TO to the DID that routes back to this node}
WS=${WS:-ws://127.0.0.1:8081/?mode=sustain}
SP=${SP:-/var/tmp/tier3}; mkdir -p "$SP"
: "${TOKEN:?set TOKEN to an admin-role bearer token}"

# Guardrails. A live trunk is not a loopback: a run that has stopped
# working must stop dialling, not keep paying to fail.
MAX_CALLS=200
CONSECUTIVE_FAIL_ABORT=3
(( CALLS > MAX_CALLS )) && { echo "refusing $CALLS calls (cap $MAX_CALLS)"; exit 2; }

active() { curl -s --max-time 5 "http://$OBS/metrics" | awk '/^siphon_ai_calls_active /{print $2}'; }

echo "tier3: $CALLS sequential trombones, ${HOLD}s hold, gateway=$GATEWAY to=$TO"
echo "  two channels per call; one left free for genuine inbound traffic"
ok=0; fail=0; streak=0
for i in $(seq 1 "$CALLS"); do
  # Never start on top of existing traffic — that would both skew the
  # measurement and eat the spare channel.
  for _ in $(seq 1 30); do [[ "$(active)" == "0" ]] && break; sleep 2; done
  if [[ "$(active)" != "0" ]]; then
    echo "[$i] channel busy for 60s — skipping"; fail=$((fail+1)); streak=$((streak+1))
  else
    t0=$(date -u +%s.%N)
    body=$(curl -s --max-time 10 -X POST -H "Authorization: Bearer $TOKEN" \
      -H "Content-Type: application/json" "http://$ADMIN/admin/v1/calls" \
      -d "{\"to\":\"$TO\",\"gateway\":\"$GATEWAY\",\"ws_url\":\"$WS\"}")
    id=$(sed -n 's/.*"call_id":"\([^"]*\)".*/\1/p' <<<"$body")
    if [[ -z "$id" ]]; then
      echo "[$i] originate refused: $body"; fail=$((fail+1)); streak=$((streak+1))
    else
      # Both legs up = the trombone closed. One leg only means the carrier
      # took the call but never routed it back, which is a failed sample.
      up=0
      for _ in $(seq 1 20); do [[ "$(active)" == "2" ]] && { up=1; break; }; sleep 0.5; done
      if (( up )); then
        sleep "$HOLD"
        curl -s -o /dev/null --max-time 10 -X POST -H "Authorization: Bearer $TOKEN" \
          "http://$ADMIN/admin/v1/calls/$id/hangup"
        ok=$((ok+1)); streak=0
        echo "[$i] ok ($id)"
      else
        echo "[$i] trombone did not close (active=$(active)) — hanging up"
        curl -s -o /dev/null --max-time 10 -X POST -H "Authorization: Bearer $TOKEN" \
          "http://$ADMIN/admin/v1/calls/$id/hangup"
        fail=$((fail+1)); streak=$((streak+1))
      fi
    fi
  fi
  (( streak >= CONSECUTIVE_FAIL_ABORT )) && {
    echo "ABORT: $streak consecutive failures — stopping so the trunk isn't billed for a broken run"; break; }
  # Drain before the next one, so each sample is a clean single call.
  for _ in $(seq 1 30); do [[ "$(active)" == "0" ]] && break; sleep 1; done
done
echo "done: ok=$ok fail=$fail"
