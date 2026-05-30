#!/usr/bin/env bash
#
# Drives the 0.3.0 TLS validation suite. By default runs only the
# fully-automated subset (scenarios 3 + 4); the semi-automated and
# manual scenarios print their procedure files for the operator to
# follow.
#
#   ./run-all.sh                  Auto-only (3 + 4); print pointers for 1 / 2 / 5
#   ./run-all.sh --auto-only      Same; explicit
#   ./run-all.sh --all            Run auto + prompt for manual scenarios in order
#   ./run-all.sh -h | --help      This message

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib/common.sh
source "$SCRIPT_DIR/lib/common.sh"

MODE="${1:-}"
case "$MODE" in
  ""|--auto-only) MODE=auto ;;
  --all)          MODE=all ;;
  -h|--help)
    sed -n '2,12p' "$0" | sed 's|^# \?||'
    exit 0
    ;;
  *) fail "unknown arg: $MODE (try --help)" ;;
esac

results=()

run_scenario() {
  local label="$1" script="$2"
  step "$label"
  if bash "$script"; then
    results+=("$label  PASS")
  else
    results+=("$label  FAIL")
  fi
}

# ─── Auto scenarios ───────────────────────────────────────────────────────
run_scenario "Scenario 3 — mTLS WSS"           "$SCRIPT_DIR/scenario-3-mtls-wss.sh"
run_scenario "Scenario 4 — SIP/TLS cert reload" "$SCRIPT_DIR/scenario-4-cert-reload.sh"

# ─── Semi / manual scenarios ──────────────────────────────────────────────
if [[ "$MODE" == all ]]; then
  step "Scenario 1 — SDES against Twilio"
  note "Hand-driven. Procedure: $SCRIPT_DIR/scenario-1-sdes-twilio.md"
  note "Running pre-flight check now…"
  bash "$SCRIPT_DIR/scenario-1-sdes-twilio.sh" --preflight || true
  note "Place the call by hand, then run:"
  note "  $SCRIPT_DIR/scenario-1-sdes-twilio.sh --postcall"
  results+=("Scenario 1 — SDES Twilio              MANUAL")

  step "Scenario 2 — DTLS-SRTP via WebRTC gateway"
  note "Hand-driven; SIPp can't drive DTLS. Procedure: $SCRIPT_DIR/scenario-2-dtls-srtp.md"
  results+=("Scenario 2 — DTLS-SRTP                MANUAL")

  step "Scenario 5 — REGISTER over TLS"
  note "Semi-automated; needs a TLS PBX. Procedure: $SCRIPT_DIR/scenario-5-register-tls.md"
  results+=("Scenario 5 — REGISTER over TLS         MANUAL")
else
  step "Skipped (run with --all to include): scenarios 1 / 2 / 5"
  note "Scenario 1 (SDES vs Twilio)        — see scenario-1-sdes-twilio.md"
  note "Scenario 2 (DTLS-SRTP via WebRTC)  — see scenario-2-dtls-srtp.md"
  note "Scenario 5 (REGISTER over TLS)     — see scenario-5-register-tls.md"
fi

# ─── Summary ─────────────────────────────────────────────────────────────
step "Summary"
for line in "${results[@]}"; do
  case "$line" in
    *PASS*)   ok "$line" ;;
    *MANUAL*) warn "$line" ;;
    *)        printf '  %s✗%s %s\n' "$C_ERR" "$C_OFF" "$line" ;;
  esac
done

# Non-zero exit if any auto scenario failed
for line in "${results[@]}"; do
  [[ "$line" == *FAIL* ]] && exit 2
done
exit 0
