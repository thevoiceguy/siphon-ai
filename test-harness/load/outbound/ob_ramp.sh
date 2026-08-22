#!/usr/bin/env bash
# ob_ramp.sh <concurrency> <cps> <hold_seconds> [teardown]
#
# Drive N outbound calls through the daemon's own originate endpoint at C
# calls/sec, hold them, then tear them down. `teardown` is `local` (we BYE,
# via the admin API — the default) or `remote` (SIPp BYEs, in which case
# run uas_hold_remote_bye.xml and this script only waits).
#
# Reads ADMIN (host:port), TOKEN, GATEWAY and TO from the environment, with
# the outbound-lab.toml.example defaults.
set -uo pipefail
CONC=${1:-25}; CPS=${2:-5}; HOLD=${3:-120}; TEARDOWN=${4:-local}
ADMIN=${ADMIN:-127.0.0.1:9592}
TOKEN=${TOKEN:-lab-admin-token}
GATEWAY=${GATEWAY:-sipp}
TO=${TO:-7001}
SP=${SP:-/var/tmp/outbound-load}
mkdir -p "$SP"
IDS="$SP/call-ids.$$"; : > "$IDS"

echo "ramp: $CONC calls at $CPS cps, hold ${HOLD}s, teardown=$TEARDOWN"
SLEEP=$(awk -v c="$CPS" 'BEGIN{printf "%.4f", 1/c}')
placed=0; rejected=0
for _ in $(seq 1 "$CONC"); do
  # 202 + a call_id means admitted. Anything else is the daemon's own
  # guardrail talking (503 = max_concurrent, 429 = rate_limit_per_sec) and
  # is counted, not retried — a step that cannot reach its target must say
  # so rather than quietly running small.
  body=$(curl -s --max-time 5 -X POST -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    "http://$ADMIN/admin/v1/calls" \
    -d "{\"to\":\"$TO\",\"gateway\":\"$GATEWAY\"}" 2>/dev/null)
  id=$(sed -n 's/.*"call_id":"\([^"]*\)".*/\1/p' <<<"$body")
  if [[ -n "$id" ]]; then echo "$id" >> "$IDS"; placed=$((placed+1));
  else rejected=$((rejected+1)); fi
  sleep "$SLEEP"
done
echo "placed=$placed rejected=$rejected"
(( rejected > 0 )) && echo "  !! $rejected originate(s) refused — see [outbound] caps before reading this step"

echo "holding ${HOLD}s"
sleep "$HOLD"

if [[ "$TEARDOWN" == "local" ]]; then
  echo "hanging up $placed calls"
  while read -r id; do
    curl -s -o /dev/null --max-time 5 -X POST -H "Authorization: Bearer $TOKEN" \
      "http://$ADMIN/admin/v1/calls/$id/hangup"
  done < "$IDS"
else
  echo "waiting for the far end to BYE"
fi

# Drain: calls_active must reach 0 before any leak audit is meaningful.
for _ in $(seq 1 120); do
  a=$(curl -s --max-time 3 "http://127.0.0.1:${OBS_PORT:-9591}/metrics" \
      | awk '/^siphon_ai_calls_active /{print $2}')
  [[ "$a" == "0" ]] && { echo "drained"; break; }
  sleep 1
done
rm -f "$IDS"
