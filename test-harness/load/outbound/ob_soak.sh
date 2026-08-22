#!/usr/bin/env bash
# ob_soak.sh <concurrency> <minutes> <hold_seconds>
#
# LOAD_TEST_PLAN.md §6.1's soak, in the outbound direction (§12.6).
#
# Not "N long calls": it CHURNS. It holds `concurrency` outbound calls
# active for the whole window by replacing each one as it ends, so an hour
# at 50 concurrent with a 60 s hold is ~3,000 completed calls rather than
# 50. Per-call leaks scale with completions, not with concurrency — #548
# leaked one dialog per call and would have been invisible in a soak that
# merely held 50 calls open for an hour.
set -uo pipefail
CONC=${1:-50}; MINUTES=${2:-60}; HOLD=${3:-60}
ADMIN=${ADMIN:-127.0.0.1:9592}
OBS=${OBS:-127.0.0.1:9591}
TOKEN=${TOKEN:-lab-admin-token}
GATEWAY=${GATEWAY:-sipp}
TO=${TO:-7001}
WS=${WS:-ws://127.0.0.1:8770/}
SP=${SP:-/var/tmp/outbound-load}; mkdir -p "$SP"
LABEL=${LABEL:-soak}
# Where obstat.sh lives. Overridable because a long soak should be run
# from a FROZEN copy of this script: bash reads a script incrementally, so
# editing the original mid-run makes the live process jump to mangled byte
# offsets and fail in ways that look like logic bugs.
RIG=${RIG:-$(cd "$(dirname "$0")" && pwd)}
DAEMON_PID=${DAEMON_PID:?set DAEMON_PID so the sampler can read /proc}

END=$(( $(date +%s) + MINUTES * 60 ))
declare -A started            # call_id -> epoch it was placed
# Same `set -u` hazard as the loops below: count via the guarded expansion.
count_active() { local n=0 k; for k in ${started[@]+"${!started[@]}"}; do n=$((n+1)); done; echo "$n"; }
placed=0; done_=0; failed=0
next_sample=$(( $(date +%s) + 300 ))

echo "soak: hold $CONC concurrent outbound for ${MINUTES}m, ${HOLD}s per call"
"$RIG/obstat.sh" "$DAEMON_PID" "${OBS##*:}" | tee "$SP/$LABEL-t0.json"

while (( $(date +%s) < END )); do
  now=$(date +%s)
  # Retire anything past its hold.
  #
  # `${arr[@]+"${!arr[@]}"}` is not decoration. Under `set -u` on bash 5.2
  # an EMPTY associative array is "unset": both `${!started[@]}` and
  # `${#started[@]}` abort the script, so the obvious loop and the obvious
  # emptiness guard both die on the first pass, before a single call is
  # placed. The `+` expansion is the one form that yields nothing when
  # empty and the keys when populated.
  for id in ${started[@]+"${!started[@]}"}; do
    if (( now - started[$id] >= HOLD )); then
      curl -s -o /dev/null --max-time 5 -X POST -H "Authorization: Bearer $TOKEN" \
        "http://$ADMIN/admin/v1/calls/$id/hangup"
      unset 'started[$id]'
      done_=$(( done_ + 1 ))
    fi
  done
  # Top back up to concurrency.
  while (( $(count_active) < CONC && $(date +%s) < END )); do
    body=$(curl -s --max-time 10 -X POST -H "Authorization: Bearer $TOKEN" \
      -H "Content-Type: application/json" "http://$ADMIN/admin/v1/calls" \
      -d "{\"to\":\"$TO\",\"gateway\":\"$GATEWAY\",\"ws_url\":\"$WS\"}")
    id=$(sed -n 's/.*"call_id":"\([^"]*\)".*/\1/p' <<<"$body")
    if [[ -n "$id" ]]; then started[$id]=$(date +%s); placed=$(( placed + 1 ));
    else failed=$(( failed + 1 )); break; fi
  done
  # Periodic sample — the shape over time is the point, not the endpoints.
  if (( $(date +%s) >= next_sample )); then
    active=$(count_active)
    echo "[$(date -u +%H:%M:%S)] placed=$placed completed=$done_ failed=$failed active=$active"
    "$RIG/obstat.sh" "$DAEMON_PID" "${OBS##*:}" | tee -a "$SP/$LABEL-series.jsonl"
    next_sample=$(( $(date +%s) + 300 ))
  fi
  sleep 1
done

echo "draining"
for id in ${started[@]+"${!started[@]}"}; do
  curl -s -o /dev/null --max-time 5 -X POST -H "Authorization: Bearer $TOKEN" \
    "http://$ADMIN/admin/v1/calls/$id/hangup"
  done_=$(( done_ + 1 ))
done
for _ in $(seq 1 120); do
  [[ "$(curl -s --max-time 3 "http://$OBS/metrics" | awk '/^siphon_ai_calls_active /{print $2}')" == "0" ]] && break
  sleep 1
done
echo "--- leak audit"
for i in $(seq 1 50); do
  d=$(curl -s "http://$OBS/metrics" | awk '/^siphon_ai_dialogs_active /{print $2}')
  [[ "$d" == "0" ]] && { echo "dialogs_active back to 0 at t=${i}s"; break; }
  sleep 1
done
"$RIG/obstat.sh" "$DAEMON_PID" "${OBS##*:}" | tee "$SP/$LABEL-drained.json"
echo "soak done: placed=$placed completed=$done_ failed=$failed"
