#!/usr/bin/env bash
# Tier-2 phase 2: same ramp as ramp.sh but over SIP/TLS with SRTP mandatory.
# srtp=required on the daemon side per docs/CONFIG.md — stock FreeSWITCH 488s
# the "preferred" AVP+a=crypto shape, so required is the only workable mode here.
CONC=${1:-50}; CPS=${2:-10}; HOLD=${3:-300}
TARGET="sip:9001@139.177.205.140:5071;transport=tls"
MEDIA=/usr/share/freeswitch/sounds/music/8000/suite-espanola-op-47-leyenda.wav
DELAY=$(python3 -c "print(1/$CPS)")
for i in $(seq 1 "$CONC"); do
  fs_cli -x "bgapi originate {ignore_early_media=true,absolute_codec_string=PCMU,\
rtp_secure_media=mandatory:AES_CM_128_HMAC_SHA1_80,origination_caller_id_number=1000,\
execute_on_answer='sched_hangup +$HOLD NORMAL_CLEARING'}sofia/external/$TARGET \
&endless_playback($MEDIA)" >/dev/null
  sleep "$DELAY"
done
echo "originated $CONC TLS+SRTP calls at $CPS cps, hold ${HOLD}s"
