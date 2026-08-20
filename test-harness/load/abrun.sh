#!/usr/bin/env bash
# One A/B arm: fresh daemon, idle baseline, flood, settle, measure.
# Fresh process each time so the allocator starts from the same place —
# reusing one daemon would let the first arm's freed memory absorb the
# second's, which is exactly the trap RESULTS-0.48.10 fell into.
set -u
SP="$(cd "$(dirname "$0")" && pwd)"
BIN="${SIPHON_AI_BIN:-$SP/../../target/debug/siphon-ai}"
ARM="$1"; TRACES="$2"; MSGS="$3"
cd "$SP"
nohup "$BIN" --config "$SP/ab-$ARM.toml" > "$SP/ab-$ARM.log" 2>&1 &
PID=$!
for _ in $(seq 1 40); do
    grep -q "daemon ready" "$SP/ab-$ARM.log" 2>/dev/null && break
    sleep 0.25
done
sleep 3                                   # let startup allocations settle
IDLE=$(awk '/VmRSS/{print $2}' /proc/$PID/status)
python3 "$SP/ringflood.py" --traces "$TRACES" --messages "$MSGS" --pad "${PAD:-0}" >/dev/null
sleep 8                                   # let the sink drain its queue
LOADED=$(awk '/VmRSS/{print $2}' /proc/$PID/status)
HWM=$(awk '/VmHWM/{print $2}' /proc/$PID/status)
STATS=$(curl -s http://127.0.0.1:9591/metrics | awk '
  /^siphon_ai_sip_ring_traces/{t=$2}
  /^siphon_ai_sip_ring_messages_total.*captured/{c=$2}
  END{printf "traces=%s captured=%s", (t==""?"-":t), (c==""?"0":c)}')
printf "%-4s idle=%-8s loaded=%-8s delta=%-7s hwm=%-8s %s\n" \
    "$ARM" "$IDLE" "$LOADED" "$((LOADED - IDLE))" "$HWM" "$STATS"
kill $PID 2>/dev/null
wait $PID 2>/dev/null
sleep 1
