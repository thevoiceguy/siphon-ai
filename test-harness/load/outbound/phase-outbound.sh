#!/usr/bin/env bash
# phase-outbound.sh <daemon-binary> <label> [steps...]
#
# The outbound concurrency phase (LOAD_TEST_PLAN.md §12): originate through
# the daemon's own endpoint at each step, sample under load, drain, then run
# the §6.3 leak audit with `dialogs_active` in it — the gauge that only an
# outbound run can move, and the one issue #548 pinned at a non-zero value
# for the life of the process.
#
# Brings up its own paced_sink and SIPp callee; expects the daemon config
# from outbound-lab.toml.example (or $CONFIG).
set -uo pipefail
BIN=${1:?usage: phase-outbound.sh <daemon-binary> <label> [steps...]}
LABEL=${2:?}
shift 2
STEPS=("${@:-25 50 100}")
[[ $# -gt 0 ]] && STEPS=("$@")
HERE=$(cd "$(dirname "$0")" && pwd)
SP=${SP:-/var/tmp/outbound-load}; mkdir -p "$SP"
CONFIG=${CONFIG:-$HERE/outbound-lab.toml}
HOLD=${HOLD:-120}
CPS=${CPS:-5}
OBS_PORT=${OBS_PORT:-9591}
TEARDOWN=${TEARDOWN:-local}
SCEN=$HERE/uas_hold.xml
if [[ "$TEARDOWN" == "remote" ]]; then
  # The remote-BYE scenario carries a literal PAUSE_MS so the far end's
  # hold matches the run's; substitute it into a temp copy rather than
  # relying on a SIPp keyword, which is one more thing to get wrong.
  SCEN=$SP/uas_hold_remote_bye.$$.xml
  sed "s/PAUSE_MS/$(( HOLD * 1000 ))/" "$HERE/uas_hold_remote_bye.xml" > "$SCEN"
fi

[[ -f "$CONFIG" ]] || { echo "no config at $CONFIG (copy outbound-lab.toml.example)"; exit 2; }

cleanup() { kill "${SINK_PID:-0}" "${DAEMON_PID:-0}" "${SIPP_PID:-0}" 2>/dev/null; }
trap cleanup EXIT

node "$HERE/../paced_sink.mjs" 8770 > "$SP/sink-$LABEL.log" 2>&1 & SINK_PID=$!
RUST_LOG=warn,siphon_ai=info "$BIN" --config "$CONFIG" > "$SP/daemon-$LABEL.log" 2>&1 & DAEMON_PID=$!
sleep 2
kill -0 "$DAEMON_PID" 2>/dev/null || { echo "daemon died on startup:"; tail -20 "$SP/daemon-$LABEL.log"; exit 1; }
echo "daemon $($BIN --version) pid=$DAEMON_PID"

echo "--- idle baseline"
"$HERE/obstat.sh" "$DAEMON_PID" "$OBS_PORT" | tee "$SP/$LABEL-idle.json"

STEP_N=0
for CONC in "${STEPS[@]}"; do
  # Index the sample files: a run may repeat a concurrency deliberately
  # (§6.3 test 2 — the same step twice, to separate pool sizing from a
  # leak), and naming by concurrency alone silently overwrites the first.
  STEP_N=$((STEP_N + 1))
  TAG="$LABEL-s$STEP_N-$CONC"
  echo "=== step $STEP_N: $CONC concurrent, teardown=$TEARDOWN"
  # One SIPp per step, sized to the step: -m ends it after N calls.
  sipp -i 127.0.0.1 -sf "$SCEN" -p 5075 -m "$CONC" -l "$CONC" \
       -timeout 3600s -trace_err -bg > /dev/null 2>&1
  SIPP_PID=$(pgrep -n -f "sipp -i 127.0.0.1 -sf $SCEN")
  sleep 0.5
  SP="$SP" OBS_PORT="$OBS_PORT" "$HERE/ob_ramp.sh" "$CONC" "$CPS" "$HOLD" "$TEARDOWN" &
  RAMP_PID=$!
  # Sample twice under load: once the ramp has landed, once late in the hold.
  sleep $(( CONC / CPS + 10 ))
  "$HERE/obstat.sh" "$DAEMON_PID" "$OBS_PORT" | tee "$SP/$TAG-early.json"
  sleep $(( HOLD / 2 ))
  "$HERE/obstat.sh" "$DAEMON_PID" "$OBS_PORT" | tee "$SP/$TAG-late.json"
  wait "$RAMP_PID"
  pkill -f "sipp -i 127.0.0.1 -sf $SCEN" 2>/dev/null

  # §6.3 leak audit, with the outbound-only gauge in it. Removal is
  # deferred by DialogReaper::DEFAULT_GRACE (32 s) so a BYE retransmit
  # still matches — poll past it rather than reading once.
  echo "--- leak audit (waiting out the 32s dialog grace window)"
  for i in $(seq 1 50); do
    d=$(curl -s "http://127.0.0.1:$OBS_PORT/metrics" | awk '/^siphon_ai_dialogs_active /{print $2}')
    [[ "$d" == "0" ]] && { echo "dialogs_active back to 0 at t=${i}s"; break; }
    sleep 1
  done
  "$HERE/obstat.sh" "$DAEMON_PID" "$OBS_PORT" | tee "$SP/$TAG-drained.json"
done

echo "=== WARN/ERROR in the run"
grep -cE "\bWARN\b|\bERROR\b" "$SP/daemon-$LABEL.log"
grep -E "\bWARN\b|\bERROR\b" "$SP/daemon-$LABEL.log" | sed 's/call_id=[a-z0-9-]*//g' | sort | uniq -c | head
