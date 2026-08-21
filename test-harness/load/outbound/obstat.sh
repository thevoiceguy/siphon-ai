#!/usr/bin/env bash
# One-shot sample of the gauges an outbound run turns on, as one JSON line.
#
# `udp_sockets` counts the daemon's unconnected UDP sockets: two RTP/RTCP
# per active call plus its SIP listener(s), so expect 2N+1 on a UDP-only
# config. It is a whole-process count, so it spans BOTH directions — which
# is the point in the mixed phase (§13).
# Companion to ../ringstat.sh. Usage: obstat.sh <daemon-pid> [obs-port]
#
# A series that does not exist yet reads 0, not null: several of these are
# published only once the first call has touched the code path, and an idle
# baseline row of nulls is harder to diff than one of zeros.
set -uo pipefail
PID=${1:?usage: obstat.sh <daemon-pid> [obs-port]}
PORT=${2:-9591}

# CPU as a rate, not ps's lifetime average (§8) — utime+stime over 5 s.
# The window comes FIRST, and everything else is read after it closes, so
# every field in the line below describes the same instant. Reading
# /metrics before the sleep and /proc after it produced lines whose
# `calls_active` and `fds` disagreed by 5 seconds of ramp — which reads as
# a daemon that allocates no descriptors for outbound legs.
read -r _ _ _ _ _ _ _ _ _ _ _ _ _ u1 s1 _ < "/proc/$PID/stat"
sleep 5
read -r _ _ _ _ _ _ _ _ _ _ _ _ _ u2 s2 _ < "/proc/$PID/stat"
HZ=$(getconf CLK_TCK)
CPU=$(awk -v d=$(( (u2-u1)+(s2-s1) )) -v hz="$HZ" 'BEGIN{printf "%.1f", d*100/hz/5}')

M=$(curl -s --max-time 3 "http://127.0.0.1:$PORT/metrics" || true)

# Exact series name, no labels.
g() { awk -v k="$1" '$1 == k {print $2; f=1} END{if(!f) print 0}' <<<"$M"; }
# Sum every series matching a regex (label sets vary run to run).
sum() { awk -v re="$1" '$1 ~ re {s+=$2} END{printf "%d", s+0}' <<<"$M"; }

# RTP sockets are unconnected UDP, so `ss -l` is the right listing. Count
# the daemon's own, not every 42xxx/43xxx socket on the box — a second
# instance or a leftover run would otherwise be counted into this one.
PORTS=$(ss -lunp 2>/dev/null | grep -c "pid=$PID," || true)

printf '{"ts":"%s","cpu_pct_of_one_core":%s,"rss_kb":%s,"fds":%s,"threads":%s,' \
  "$(date -u +%FT%TZ)" "$CPU" \
  "$(awk '/^VmRSS/{print $2}' "/proc/$PID/status")" \
  "$(ls "/proc/$PID/fd" 2>/dev/null | wc -l)" \
  "$(awk '/^Threads/{print $2}' "/proc/$PID/status")"
printf '"calls_active":%s,"dialogs_active":%s,"outbound_answered":%s,"outbound_not_answered":%s,"udp_sockets":%s}\n' \
  "$(g siphon_ai_calls_active)" \
  "$(g siphon_ai_dialogs_active)" \
  "$(sum '^siphon_ai_outbound_calls_total\{result="answered"\}$')" \
  "$(sum '^siphon_ai_outbound_calls_total\{result="(busy|declined|no_answer|rejected|unreachable|failed)"\}$')" \
  "${PORTS:-0}"
