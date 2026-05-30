#!/usr/bin/env bash
#
# Scenario 1 — SDES against Twilio (validation helper).
#
# The call itself is hand-driven (see scenario-1-sdes-twilio.md).
# This script does the parts that ARE scriptable:
#
#   --preflight    Validate the live /etc/siphon-ai/siphon-ai.toml
#                  has the right [sip.tls] + [media].srtp + trunk
#                  shape before you place the test call.
#
#   --postcall     Pull the most recent CDR from
#                  /var/log/siphon-ai/cdr.jsonl and check the
#                  expected fields are populated.
#
# Default (no args) prints the script's usage + the manual
# procedure summary.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib/common.sh
source "$SCRIPT_DIR/lib/common.sh"

CONFIG="${SIPHON_AI_CONFIG:-/etc/siphon-ai/siphon-ai.toml}"
CDR="${SIPHON_AI_CDR:-/var/log/siphon-ai/cdr.jsonl}"

usage() {
  cat <<EOF
Scenario 1 — SDES against Twilio (helper script).

  $0 --preflight    Validate the daemon config is ready for the test
  $0 --postcall     Validate the most recent CDR record looks right

See scenario-1-sdes-twilio.md for the full manual procedure.
EOF
}

preflight() {
  step "Scenario 1 — pre-flight checks"
  [[ -r "$CONFIG" ]] || fail "config not readable: $CONFIG (override with SIPHON_AI_CONFIG=)"
  ok "Config readable: $CONFIG"

  # ─── transports include tls ───
  if sudo grep -E '^transports\s*=' "$CONFIG" | grep -q '"tls"'; then
    ok "[sip].transports includes \"tls\""
  else
    fail "[sip].transports does NOT include \"tls\" — required for SDES interop with Twilio"
  fi

  # ─── [sip.tls] cert + key exist and are readable ───
  cert=$(sudo awk '/^\[sip\.tls\]/{flag=1;next} /^\[/{flag=0} flag && /^cert/{print $NF}' "$CONFIG" | tr -d '"')
  key=$( sudo awk '/^\[sip\.tls\]/{flag=1;next} /^\[/{flag=0} flag && /^key/ {print $NF}' "$CONFIG" | tr -d '"')
  [[ -n "$cert" && -n "$key" ]] || fail "[sip.tls].cert and/or [sip.tls].key missing"
  sudo test -r "$cert" || fail "[sip.tls].cert not readable: $cert"
  sudo test -r "$key"  || fail "[sip.tls].key not readable: $key"
  ok "[sip.tls].cert readable: $cert"
  ok "[sip.tls].key readable: $key"

  # Cert expiry sanity — refuse to test against a cert that
  # expires in <30 days.
  if sudo openssl x509 -in "$cert" -noout -checkend $((30*86400)) >/dev/null 2>&1; then
    notafter=$(sudo openssl x509 -in "$cert" -noout -enddate | cut -d= -f2)
    ok "Cert valid >30d (notAfter: $notafter)"
  else
    warn "Cert expires within 30 days — rotate before relying on this test"
  fi

  # ─── [media].srtp set ───
  srtp=$(sudo awk '/^\[media\]/{flag=1;next} /^\[/{flag=0} flag && /^srtp/{print $NF}' "$CONFIG" | tr -d '"')
  case "$srtp" in
    preferred|required) ok "[media].srtp = \"$srtp\"" ;;
    off|"")             fail "[media].srtp is \"${srtp:-unset}\" — must be \"preferred\" or \"required\" for SDES test" ;;
    *)                  fail "[media].srtp = \"$srtp\" is not a recognised value" ;;
  esac

  # ─── At least one [[trunk]] covers Twilio ───
  # Twilio NA-VA edge IPs that have been seen in production
  # captures this session; the doc has the full list.
  if sudo grep -A20 '\[\[trunk\]\]' "$CONFIG" | grep -qE '54\.(172|244)\.'; then
    ok "At least one [[trunk]] mentions a known Twilio edge range (54.172 / 54.244)"
  else
    warn "No [[trunk]] block references known Twilio edge ranges (54.172 / 54.244). \
Add the regions you accept from per docs/TWILIO_INTEROP.md"
  fi

  verdict_pass "Daemon config is shape-correct for the SDES Twilio test. Now place the call by hand."
}

postcall() {
  step "Scenario 1 — post-call CDR validation"
  sudo test -r "$CDR" || fail "CDR file not readable: $CDR"

  last=$(sudo tail -1 "$CDR")
  [[ -n "$last" ]] || fail "CDR file is empty"

  echo "$last" | jq -e '.version == 1'           >/dev/null || fail "CDR version != 1"
  echo "$last" | jq -e '.direction == "inbound"' >/dev/null || fail "CDR direction != inbound"

  route=$(echo "$last" | jq -r '.route')
  if [[ "$route" == twilio* ]]; then
    ok "Route = $route"
  else
    warn "Route = $route — expected a route name starting with 'twilio'. Are you sure the last call was the test call?"
  fi

  codec=$(echo "$last" | jq -r '.audio.codec')
  case "$codec" in
    PCMU|PCMA) ok "Codec = $codec" ;;
    *)         warn "Codec = $codec — Twilio defaults to PCMU; verify your offer codec order" ;;
  esac

  dur_ms=$(echo "$last" | jq -r '.duration_ms')
  ok "Call duration: ${dur_ms} ms"
  if (( dur_ms < 3000 )); then
    warn "Call was under 3 s — too short to validate two-way audio. Re-run with a longer hold."
  fi

  cause=$(echo "$last" | jq -r '.termination.cause')
  ok "Termination cause: $cause"

  verdict_manual "CDR shape is correct. SDES specifically is confirmed by the sngrep capture of the 200 OK SDP — see scenario-1-sdes-twilio.md §4."
}

case "${1:-}" in
  --preflight) preflight ;;
  --postcall)  postcall ;;
  -h|--help|"") usage ;;
  *) usage; exit 1 ;;
esac
