#!/usr/bin/env bash
# Snapshot the SIP-ladder ring under load. Reads the ring gauges, the
# call count, and RSS together so a trace count is always attributable
# to a concurrency.
M=$(mktemp); curl -s http://127.0.0.1:9091/metrics > "$M"
PID=$(systemctl show siphon-ai -p MainPID --value)
g() { awk -v k="$1" '$1==k {print $2}' "$M"; }
l() { awk -v k="$1" '$1 ~ k {print $2}' "$M"; }
printf "%-22s %s\n" "at" "$(date -u +%T)Z"
printf "%-22s %s\n" "calls_active"   "$(g siphon_ai_calls_active)"
printf "%-22s %s\n" "dialogs_active" "$(g siphon_ai_dialogs_active)"
printf "%-22s %s\n" "ring_traces"    "$(g siphon_ai_sip_ring_traces)"
for r in captured dropped_call_cap dropped_trace_cap; do
  v=$(awk -v r="result=\"$r\"" '$1 ~ /^siphon_ai_sip_ring_messages_total/ && $1 ~ r {print $2}' "$M")
  printf "%-22s %s\n" "msgs_$r" "${v:-0}"
done
printf "%-22s %s kB\n" "rss" "$(awk '/VmRSS/{print $2}' /proc/$PID/status)"
printf "%-22s %s\n" "threads" "$(awk '/Threads/{print $2}' /proc/$PID/status)"
printf "%-22s %s\n" "warns" "$(journalctl -u siphon-ai --since "@$(date -d "$(systemctl show siphon-ai -p ExecMainStartTimestamp --value | sed 's/^[A-Za-z]* //')" +%s 2>/dev/null || echo 0)" -q 2>/dev/null | grep -c ' WARN ')"
rm -f "$M"
