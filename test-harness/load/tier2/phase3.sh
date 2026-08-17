#!/usr/bin/env bash
# Tier-2 phase 3: quality under netem impairment (LOAD_TEST_PLAN.md §10.2).
#
# Two 200-call TLS+SRTP runs, back to back: one clean, one with 20ms±5ms delay
# and 0.5% loss applied to Box B's egress. Quality is read as histogram DELTAS
# around each run, so the 700 calls already in the daemon's counters cannot
# flatter the result.
#
# Impairment is on B's egress only, i.e. the caller->SiphonAI direction — which
# is exactly the stream siphon_ai_rtp_rx_jitter_ms measures. The return path is
# unimpaired; that asymmetry is deliberate and belongs in the write-up.
set -u
SP=${SP:?set SP to a working directory}
B=root@194.195.208.34
M=http://127.0.0.1:9191/metrics
CONC=200; CPS=10; HOLD=300

snap() { curl -s --max-time 5 $M | grep -E '^siphon_ai_rtp_(rx_jitter_ms|mos_estimate)_(bucket|count|sum)' > "$1"; }

run_one() {                       # $1=label  $2=pre file  $3=post file
  : > $SP/cdr-tls.jsonl
  snap "$2"
  ssh -o BatchMode=yes -o ConnectTimeout=10 $B "/root/ramp-tls.sh $CONC $CPS $HOLD" >/dev/null 2>&1
  echo "  [$1] originated $CONC, settling then holding"
  sleep 280
  snap "$3"
  echo "  [$1] quality window captured; draining"
  for i in $(seq 1 40); do
    A=$(curl -s --max-time 4 $M | grep -oP '^siphon_ai_calls_active \K[0-9]+')
    [ "${A:-1}" = "0" ] && break; sleep 15
  done
  sleep 10
  echo "  [$1] drained: cdrs=$(wc -l < $SP/cdr-tls.jsonl) active=$(curl -s --max-time 4 $M | grep -oP '^siphon_ai_calls_active \K[0-9]+')"
  cp $SP/cdr-tls.jsonl $SP/cdr-$1.jsonl
}

echo "=== tier-2 phase 3 (netem) started $(date -u +%FT%TZ) ==="

echo "--- RUN A: clean LAN, 200 concurrent TLS+SRTP ---"
run_one clean $SP/q-clean-pre.txt $SP/q-clean-post.txt

echo "--- applying netem on Box B egress: 20ms +/-5ms, 0.5% loss ---"
ssh -o BatchMode=yes -o ConnectTimeout=10 $B \
  "tc qdisc replace dev eth0 root netem delay 20ms 5ms loss 0.5%; \
   setsid nohup bash -c 'sleep 1800; tc qdisc del dev eth0 root' >/dev/null 2>&1 & \
   sleep 1; tc qdisc show dev eth0 | head -2"
echo "  (a detached 30-min auto-removal is armed, so impairment self-clears if this session dies)"

echo "--- RUN B: impaired, 200 concurrent TLS+SRTP ---"
run_one impaired $SP/q-imp-pre.txt $SP/q-imp-post.txt

echo "--- removing netem ---"
ssh -o BatchMode=yes -o ConnectTimeout=10 $B \
  "tc qdisc del dev eth0 root 2>/dev/null; tc qdisc show dev eth0 | head -2"

echo "=== tier-2 phase 3 finished $(date -u +%FT%TZ) ==="
