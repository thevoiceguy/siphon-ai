#!/usr/bin/env bash
# phase-mixed.sh <daemon-binary> <label> <inbound> <outbound>
#
# LOAD_TEST_PLAN.md §13 — both directions at once on one daemon.
#
# §§3-6 load inbound. §12 loads outbound. Neither says anything about the
# resources the two share: ONE RTP port pool, ONE dialog store, one fd
# table. This runs them together and watches those three.
#
# EXHAUST=1 swaps in a port range too small for the combined load, so the
# pool empties mid-run and the failure mode becomes observable rather than
# theoretical.
#
# ORDER decides WHICH direction gets there first, and the two answers are
# not symmetric — one side fails through the originate API and its webhook,
# the other has to fail on the wire with a SIP response code. Run both.
#   inbound-first  (default) — inbound establishes, then originates fail
#   outbound-first           — originates establish, then INVITEs arrive
set -uo pipefail
BIN=${1:?usage: phase-mixed.sh <daemon-binary> <label> <inbound> <outbound>}
LABEL=${2:?}; IN_N=${3:-50}; OUT_N=${4:-50}
HERE=$(cd "$(dirname "$0")" && pwd)
OB=$HERE/../outbound
SP=${SP:-/var/tmp/mixed-load}; mkdir -p "$SP"
HOLD=${HOLD:-120}
CPS=${CPS:-5}
OBS_PORT=${OBS_PORT:-9591}
CONFIG=${CONFIG:-$HERE/mixed-lab.toml}
[[ "${EXHAUST:-0}" == "1" ]] && CONFIG=${CONFIG_EXHAUST:-$HERE/mixed-exhaust.toml}
[[ -f "$CONFIG" ]] || { echo "no config at $CONFIG (copy the .example)"; exit 2; }

cleanup() { kill "${SINK_PID:-0}" "${DAEMON_PID:-0}" "${UAS_PID:-0}" "${UAC_PID:-0}" 2>/dev/null; }
trap cleanup EXIT

node "$HERE/../paced_sink.mjs" 8770 > "$SP/sink-$LABEL.log" 2>&1 & SINK_PID=$!
RUST_LOG=siphon_ai=info "$BIN" --config "$CONFIG" > "$SP/daemon-$LABEL.log" 2>&1 & DAEMON_PID=$!
sleep 2
kill -0 "$DAEMON_PID" 2>/dev/null || { echo "daemon died on startup:"; tail -20 "$SP/daemon-$LABEL.log"; exit 1; }
echo "daemon $($BIN --version) pid=$DAEMON_PID  config=$(basename "$CONFIG")"
echo "mixed: ${IN_N} inbound + ${OUT_N} outbound, ${HOLD}s hold"

echo "--- idle baseline"
"$OB/obstat.sh" "$DAEMON_PID" "$OBS_PORT" | tee "$SP/$LABEL-idle.json"

UAC_SCEN=$SP/uac_hold.$$.xml
sed "s/PAUSE_MS/$(( HOLD * 1000 ))/" "$HERE/uac_hold.xml" > "$UAC_SCEN"
UAC_ERR=$SP/$LABEL-inbound-sipp

# SIPp answering outbound legs is started up front either way — it is
# passive until the daemon dials it, so it cannot affect the ordering.
sipp -i 127.0.0.1 -sf "$OB/uas_hold.xml" -p 5075 -m "$OUT_N" -l "$OUT_N" \
     -timeout 3600s -trace_err -bg > /dev/null 2>&1
UAS_PID=$(pgrep -n -f "sipp -i 127.0.0.1 -sf $OB/uas_hold.xml")

start_inbound() {
  sipp -i 127.0.0.1 -sf "$UAC_SCEN" 127.0.0.1:5070 -m "$IN_N" -r "$CPS" -l "$IN_N" \
       -s loadtest -timeout 3600s -trace_err -error_file "$UAC_ERR" -bg > /dev/null 2>&1
  UAC_PID=$(pgrep -n -f "sipp -i 127.0.0.1 -sf $UAC_SCEN")
  sleep $(( IN_N / CPS + 5 ))
}
start_outbound() {
  SP="$SP" OBS_PORT="$OBS_PORT" "$OB/ob_ramp.sh" "$OUT_N" "$CPS" "$HOLD" local &
  RAMP_PID=$!
  sleep $(( OUT_N / CPS + 10 ))
}

if [[ "${ORDER:-inbound-first}" == "outbound-first" ]]; then
  echo "--- outbound first"
  start_outbound
  "$OB/obstat.sh" "$DAEMON_PID" "$OBS_PORT" | tee "$SP/$LABEL-outbound-only.json"
  echo "--- now the inbound INVITEs arrive"
  start_inbound
else
  echo "--- inbound first"
  start_inbound
  "$OB/obstat.sh" "$DAEMON_PID" "$OBS_PORT" | tee "$SP/$LABEL-inbound-only.json"
  echo "--- now the originates go out"
  start_outbound
fi
echo "--- both directions up"
"$OB/obstat.sh" "$DAEMON_PID" "$OBS_PORT" | tee "$SP/$LABEL-mixed.json"
wait "${RAMP_PID:-$$}" 2>/dev/null

# Inbound legs BYE themselves at the end of their own hold; wait them out
# rather than killing SIPp, which would orphan them.
for _ in $(seq 1 180); do
  a=$(curl -s --max-time 3 "http://127.0.0.1:$OBS_PORT/metrics" | awk '/^siphon_ai_calls_active /{print $2}')
  [[ "$a" == "0" ]] && break
  sleep 2
done

echo "--- leak audit (both directions drained)"
for i in $(seq 1 50); do
  d=$(curl -s "http://127.0.0.1:$OBS_PORT/metrics" | awk '/^siphon_ai_dialogs_active /{print $2}')
  [[ "$d" == "0" ]] && { echo "dialogs_active back to 0 at t=${i}s"; break; }
  sleep 1
done
"$OB/obstat.sh" "$DAEMON_PID" "$OBS_PORT" | tee "$SP/$LABEL-drained.json"

echo "=== what the inbound caller was told (SIPp's own view)"
if [[ -f "$UAC_ERR" ]]; then
  grep -oE "Aborting call|[0-9]{3} [A-Za-z ]+|Unexpected" "$UAC_ERR" | sort | uniq -c | sort -rn | head -5
else
  echo "  (no SIPp error file — every INVITE was answered as the scenario expected)"
fi
echo "=== rejections seen by the daemon"
curl -s "http://127.0.0.1:$OBS_PORT/metrics" | grep -E '^siphon_ai_(calls_total|calls_rejected|outbound_calls_total|rtp_port|sip_response)' | head -12
echo "=== WARN/ERROR"
grep -cE "\bWARN\b|\bERROR\b" "$SP/daemon-$LABEL.log"
grep -E "\bWARN\b|\bERROR\b" "$SP/daemon-$LABEL.log" | sed 's/call_id=[a-z0-9-]*//g' | sort | uniq -c | sort -rn | head -5
rm -f "$UAC_SCEN"
