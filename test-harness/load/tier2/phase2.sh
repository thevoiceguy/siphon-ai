#!/usr/bin/env bash
# Tier-2 phase 2: concurrency ramp over SIP/TLS with SRTP, generator on Box B.
# Mirrors LOAD_TEST_PLAN.md §4 (5 min per step) so the numbers are comparable
# to the tier-1 figures in RESULTS-0.48.13.md.
set -u
SP=${SP:?set SP to a working directory}
B=root@194.195.208.34
HOLD=300
CPS=10
M=http://127.0.0.1:9191/metrics
D=$(cat $SP/lab.pid)
HZ=$(getconf CLK_TCK)
NCPU=$(nproc)

met() { curl -s --max-time 4 $M | grep -oP "^$1 \K[0-9.]+" | head -1; }

# Instantaneous CPU% of one core for a local pid, over $2 seconds.
cpu_local() {
  local p=$1 w=$2 u1 s1 u2 s2 t1 t2
  read u1 s1 <<<$(awk '{print $14, $15}' /proc/$p/stat); t1=$(date +%s%N)
  sleep $w
  read u2 s2 <<<$(awk '{print $14, $15}' /proc/$p/stat); t2=$(date +%s%N)
  awk -v a=$u1 -v b=$s1 -v c=$u2 -v d=$s2 -v t1=$t1 -v t2=$t2 -v hz=$HZ \
    'BEGIN{printf "%.1f", 100*((c+d)-(a+b))/hz/((t2-t1)/1e9)}'
}

# Same for freeswitch on Box B.
cpu_remote() {
  ssh -o BatchMode=yes -o ConnectTimeout=10 $B "bash -s" <<REMOTE
P=\$(pgrep -x freeswitch | head -1)
read u1 s1 <<<\$(awk '{print \$14, \$15}' /proc/\$P/stat); t1=\$(date +%s%N)
sleep $1
read u2 s2 <<<\$(awk '{print \$14, \$15}' /proc/\$P/stat); t2=\$(date +%s%N)
awk -v a=\$u1 -v b=\$s1 -v c=\$u2 -v d=\$s2 -v t1=\$t1 -v t2=\$t2 -v hz=\$(getconf CLK_TCK) \
  'BEGIN{printf "%.1f", 100*((c+d)-(a+b))/hz/((t2-t1)/1e9)}'
REMOTE
}

echo "=== tier-2 phase 2 TLS+SRTP started $(date -u +%FT%TZ) ==="
for CONC in 50 100 200; do
  echo ""
  echo "########## STEP: $CONC concurrent ##########"
  : > $SP/cdr-tls.jsonl                      # fresh CDR file per step
  WARN0=$(grep -cE ' WARN | ERROR ' $SP/daemon-tls.log || true)
  RTP0=$(met forge_rtp_packets_received_total)

  ssh -o BatchMode=yes -o ConnectTimeout=10 $B "/root/ramp-tls.sh $CONC $CPS $HOLD" >/dev/null 2>&1
  echo "originated $CONC at $CPS cps, hold ${HOLD}s — settling 60s"
  sleep 60

  # Steady-state window: three samples across the hold.
  for s in 1 2 3; do
    ACT=$(met siphon_ai_calls_active)
    ACPU=$(cpu_local $D 10)
    BCPU=$(cpu_remote 10)
    RSS=$(awk '/VmRSS/{print $2}' /proc/$D/status)
    FDS=$(ls /proc/$D/fd 2>/dev/null | wc -l)
    THR=$(awk '/Threads/{print $2}' /proc/$D/status)
    echo "  sample$s active=$ACT  A_cpu=${ACPU}%core  B_cpu=${BCPU}%core  rss=$((RSS/1024))MB  fds=$FDS  threads=$THR"
    awk -v c="$ACPU" -v a="$ACT" 'BEGIN{if(a>0) printf "           cpu/call=%.3f%%  (%.1f%% of box)\n", c/a, c/'"$NCPU"'}'
    sleep 45
  done

  DROP=$(met siphon_ai_outbound_audio_frames_dropped_total)
  RTP1=$(met forge_rtp_packets_received_total)
  echo "  playout drops=${DROP:-unset}   rtp_pkts_this_step=$(awk -v a=${RTP0:-0} -v b=${RTP1:-0} 'BEGIN{print b-a}')"

  echo "  waiting for self-hangup + drain..."
  for i in $(seq 1 40); do
    A=$(met siphon_ai_calls_active); [ "${A:-1}" = "0" ] && break; sleep 15
  done
  sleep 10
  N=$(wc -l < $SP/cdr-tls.jsonl)
  echo "  drained: calls_active=$(met siphon_ai_calls_active)  cdrs=$N"
  python3 - <<PY
import json,collections
c=collections.Counter(); d=[]
for l in open("$SP/cdr-tls.jsonl"):
    r=json.loads(l); c[r.get("termination",{}).get("cause")]+=1; d.append(r.get("duration_ms",0))
if d: print("  causes=%s  duration_ms min=%d max=%d mean=%d"%(dict(c),min(d),max(d),sum(d)/len(d)))
PY
  WARN1=$(grep -cE ' WARN | ERROR ' $SP/daemon-tls.log || true)
  echo "  WARN/ERROR added this step: $((WARN1-WARN0))"
  cp $SP/cdr-tls.jsonl $SP/cdr-tls-$CONC.jsonl
  sleep 20
done
echo ""
echo "=== tier-2 phase 2 TLS+SRTP finished $(date -u +%FT%TZ) ==="
