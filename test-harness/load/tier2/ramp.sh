#!/usr/bin/env bash
# Tier-2 ramp generator (LOAD_TEST_PLAN.md §10.2), with one deliberate change:
# every call carries sched_hangup, so it terminates on its own schedule instead
# of depending on a global `hupall` at the end. If the driving session dies
# mid-run the calls still end by themselves.
#   ramp.sh <concurrency> <cps> <hold_seconds>
CONC=${1:-50}; CPS=${2:-10}; HOLD=${3:-180}
TARGET="sip:9001@139.177.205.140:5070"
MEDIA=/usr/share/freeswitch/sounds/music/8000/suite-espanola-op-47-leyenda.wav
DELAY=$(python3 -c "print(1/$CPS)")
for i in $(seq 1 "$CONC"); do
  fs_cli -x "bgapi originate {ignore_early_media=true,absolute_codec_string=PCMU,\
origination_caller_id_number=1000,execute_on_answer='sched_hangup +$HOLD NORMAL_CLEARING'}\
sofia/external/$TARGET &endless_playback($MEDIA)" >/dev/null
  sleep "$DELAY"
done
echo "originated $CONC calls at $CPS cps, each self-hanging after ${HOLD}s"
