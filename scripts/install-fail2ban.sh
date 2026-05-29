#!/usr/bin/env bash
#
# install-fail2ban.sh — automated walk-through of docs/SECURITY_FAIL2BAN.md
#
# Installs and enables the fail2ban siphon-ai jail + recidive
# escalation on a host that already has siphon-ai running via
# systemd (i.e. after install-debian13.sh has succeeded).
#
# What it does:
#   1. Installs the fail2ban package.
#   2. Drops contrib/fail2ban/filter.d/siphon-ai.conf into
#      /etc/fail2ban/filter.d/.
#   3. Drops contrib/fail2ban/jail.d/siphon-ai.local and
#      recidive.local into /etc/fail2ban/jail.d/.
#   4. Optionally enables progressive bantime escalation
#      (`bantime.increment`) via a drop-in under jail.d/.
#   5. Validates the config (`fail2ban-client -t`), regex-tests
#      the siphon-ai filter against a known-good log line, and
#      reports active-jail status.
#
# Idempotent: re-running backs up any existing config to
# /etc/fail2ban/.../*.bak.<timestamp> before overwriting, same
# pattern as install-debian13.sh.
#
#   Optional (defaults shown):
#     CONTRIB_DIR=<repo>/contrib/fail2ban  Auto-located relative
#                  to this script. Override if you've checked out
#                  the repo to a non-standard path.
#     BANTIME_INCREMENT=1   Set to 0 to skip the bantime.increment
#                           drop-in.
#     NONINTERACTIVE=0      Set to 1 to fail-fast instead of
#                           prompting.
#
# What it does NOT do:
#   * Touch the host firewall — the jail uses `nftables-allports`
#     which expects nftables to be present and active. On Debian 13
#     it is by default. If you've stuck with iptables for legacy
#     reasons, edit the jail file to use `iptables-allports[...]`
#     before running this script.
#   * Replace the [[trunk]] allowlist. fail2ban is the noise filter
#     ON TOP of the allowlist; see docs/SECURITY_FAIL2BAN.md
#     §"What this DOESN'T do".

set -euo pipefail

# ─── Style ────────────────────────────────────────────────────────────────
if [[ -t 1 ]]; then
  C_HDR=$'\033[1;36m'  # bold cyan
  C_OK=$'\033[0;32m'   # green
  C_WARN=$'\033[0;33m' # yellow
  C_ERR=$'\033[0;31m'  # red
  C_OFF=$'\033[0m'
else
  C_HDR=''; C_OK=''; C_WARN=''; C_ERR=''; C_OFF=''
fi

step()  { printf '\n%s━━━ %s%s\n' "$C_HDR" "$*" "$C_OFF"; }
ok()    { printf '  %s✓%s %s\n' "$C_OK" "$C_OFF" "$*"; }
warn()  { printf '  %s!%s %s\n' "$C_WARN" "$C_OFF" "$*"; }
fail()  { printf '  %s✗%s %s\n' "$C_ERR" "$C_OFF" "$*" >&2; exit 1; }

# ─── Pre-flight ───────────────────────────────────────────────────────────

[[ $EUID -eq 0 ]] && fail "Run as a regular user with sudo, not as root."
command -v sudo >/dev/null || fail "sudo not installed."

if [[ -r /etc/os-release ]]; then
  . /etc/os-release
  case "${ID:-}:${VERSION_CODENAME:-}" in
    debian:trixie) ok "Debian 13 (trixie) detected." ;;
    *) warn "Untested OS: ${PRETTY_NAME:-unknown}. Proceeding anyway." ;;
  esac
fi

# The jail's `journalmatch` keys off `_SYSTEMD_UNIT=siphon-ai.service`,
# so the unit must exist or the jail will silently match nothing
# and bans will never fire. Fail loudly now instead of leaving the
# operator wondering why fail2ban is quiet.
if ! systemctl list-unit-files siphon-ai.service >/dev/null 2>&1; then
  fail "siphon-ai.service is not installed. Run install-debian13.sh first."
fi
ok "siphon-ai.service unit present."

# Locate the contrib/fail2ban directory relative to this script,
# so the install works regardless of clone path. Honour the
# operator's override if they've moved things.
SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
DEFAULT_CONTRIB="$(cd -- "$SCRIPT_DIR/.." && pwd)/contrib/fail2ban"
CONTRIB_DIR="${CONTRIB_DIR:-$DEFAULT_CONTRIB}"

for f in \
  "$CONTRIB_DIR/filter.d/siphon-ai.conf" \
  "$CONTRIB_DIR/jail.d/siphon-ai.local" \
  "$CONTRIB_DIR/jail.d/recidive.local"; do
  [[ -r "$f" ]] || fail "Missing $f. Set CONTRIB_DIR to your contrib/fail2ban path."
done
ok "Contrib files located: $CONTRIB_DIR"

BANTIME_INCREMENT="${BANTIME_INCREMENT:-1}"
NONINTERACTIVE="${NONINTERACTIVE:-0}"

# ─── Helpers ──────────────────────────────────────────────────────────────

backup_if_exists() {
  local path="$1"
  if [[ -e "$path" ]]; then
    local backup
    backup="${path}.bak.$(date +%Y%m%d-%H%M%S)"
    sudo cp -a "$path" "$backup"
    warn "Backed up existing $(basename "$path") → $backup"
  fi
}

# ─── 1. Install the fail2ban package ──────────────────────────────────────

step "Section 1: Install fail2ban"
if dpkg -s fail2ban >/dev/null 2>&1; then
  ok "fail2ban already installed."
else
  sudo apt update -qq
  sudo apt install -y fail2ban >/dev/null
  ok "fail2ban installed."
fi

# ─── 2. Drop the filter + jail files ──────────────────────────────────────

step "Section 2: Install filter + jails"

backup_if_exists /etc/fail2ban/filter.d/siphon-ai.conf
sudo install -m 0644 -o root -g root \
  "$CONTRIB_DIR/filter.d/siphon-ai.conf" \
  /etc/fail2ban/filter.d/siphon-ai.conf
ok "filter.d/siphon-ai.conf installed."

backup_if_exists /etc/fail2ban/jail.d/siphon-ai.local
sudo install -m 0644 -o root -g root \
  "$CONTRIB_DIR/jail.d/siphon-ai.local" \
  /etc/fail2ban/jail.d/siphon-ai.local
ok "jail.d/siphon-ai.local installed."

backup_if_exists /etc/fail2ban/jail.d/recidive.local
sudo install -m 0644 -o root -g root \
  "$CONTRIB_DIR/jail.d/recidive.local" \
  /etc/fail2ban/jail.d/recidive.local
ok "jail.d/recidive.local installed."

# ─── 3. Optional: bantime escalation ──────────────────────────────────────

step "Section 3: bantime escalation drop-in"
if [[ "$BANTIME_INCREMENT" == "1" ]]; then
  # Live under jail.d/ (not fail2ban.d/) because bantime.increment is
  # a JAIL [DEFAULT], not a daemon-level setting. Prefix `00-` so it
  # loads before the actual jails — drop-ins are alpha-ordered and
  # later files inherit the earlier DEFAULTs.
  backup_if_exists /etc/fail2ban/jail.d/00-bantime-increment.local
  sudo tee /etc/fail2ban/jail.d/00-bantime-increment.local >/dev/null <<'EOF'
# Progressive bantime escalation for repeat offenders.
#
# With increment = true, every fresh ban of an IP multiplies the
# remaining sentence by `factor`. Combined with recidive's 1-week
# base ban, the schedule lands roughly:
#
#   1st recidive hit → 1 week
#   2nd recidive hit → ~24 weeks
#   3rd+ recidive hit → 1 year (maxtime cap)
#
# Same factor applies to the primary siphon-ai jail's 24 h base —
# the multiplier is per-jail, not global.
#
# Installed by scripts/install-fail2ban.sh; unset BANTIME_INCREMENT=0
# at install time to skip this drop-in.
[DEFAULT]
bantime.increment = true
bantime.factor    = 24
bantime.maxtime   = 31536000
EOF
  ok "jail.d/00-bantime-increment.local installed (1 wk → 24 wk → 1 yr)."
else
  ok "Skipped bantime.increment drop-in (BANTIME_INCREMENT=0)."
fi

# ─── 4. Validate config ───────────────────────────────────────────────────

step "Section 4: Validate config"

# `fail2ban-client -t` is the canonical dry-run: parses every
# .conf/.local under /etc/fail2ban/ and reports any errors without
# touching the running daemon. Fail loud here so we don't enable a
# broken config.
if ! sudo fail2ban-client -t >/dev/null 2>&1; then
  warn "fail2ban-client -t reported errors; rerunning verbosely:"
  sudo fail2ban-client -t || true
  fail "Config validation failed. Fix the errors above before continuing."
fi
ok "fail2ban-client -t: OK"

# Sanity-check the siphon-ai filter regex against the exact log
# line shape the daemon emits. Catches mismatches after a future
# tracing-output change in siphon-ai's handler.rs.
SAMPLE='2026-01-01T00:00:00.000000Z  WARN on_invite{method="INVITE" peer=1.2.3.4:5060}: siphon_ai_sip_glue::handler: INVITE rejected: no trunk matched (403 Forbidden) peer=1.2.3.4:5060'
if sudo fail2ban-regex "$SAMPLE" /etc/fail2ban/filter.d/siphon-ai.conf 2>&1 | grep -q "^Success, the total number of match is 1"; then
  ok "fail2ban-regex matches the canonical 403 log line."
else
  warn "fail2ban-regex did NOT match the canonical log line."
  warn "  The filter regex may be stale relative to siphon-ai's current log format."
  warn "  Run: sudo fail2ban-regex '<your log line>' /etc/fail2ban/filter.d/siphon-ai.conf"
fi

# ─── 5. Enable and start ──────────────────────────────────────────────────

step "Section 5: Enable + start fail2ban"

# `enable --now` is idempotent: enables, then starts if stopped.
# If it was already running on the OLD config, the reload below
# picks up our new jails without dropping any active bans.
sudo systemctl enable --now fail2ban
ok "fail2ban.service enabled and running."

# Reload (not restart) so existing bans persist across the config
# change. `reload` re-parses configs and applies diffs in place;
# `restart` would clear the ban list and force every scanner to
# re-earn their 5-strikes before they're banned again.
sudo fail2ban-client reload
ok "fail2ban-client reload (existing bans preserved)."

sleep 1

# ─── 6. Verify the jails are live ─────────────────────────────────────────

step "Section 6: Verify"

if status=$(sudo fail2ban-client status siphon-ai 2>&1); then
  ok "siphon-ai jail active."
  printf '%s\n' "$status" | sed 's/^/    /'
else
  fail "siphon-ai jail not active. fail2ban-client status output:\n$status"
fi

if status=$(sudo fail2ban-client status recidive 2>&1); then
  ok "recidive jail active."
  printf '%s\n' "$status" | sed 's/^/    /'
else
  warn "recidive jail not active (this is unusual — was the file installed?)."
fi

# ─── Done ─────────────────────────────────────────────────────────────────

cat <<EOF

${C_OK}━━━ fail2ban install complete${C_OFF}

Active jails:
  siphon-ai   5 strikes in 10 min → 24 h ban (nftables-allports)
  recidive    3 bans in 1 week    → 1 wk ban (escalates per bantime.increment)

Cheat-sheet:
  sudo fail2ban-client status                  # all active jails
  sudo fail2ban-client status siphon-ai        # primary jail detail
  sudo fail2ban-client status recidive         # repeat-offender jail
  sudo fail2ban-client set siphon-ai unbanip 1.2.3.4
  sudo journalctl -u fail2ban -f               # tail ban/unban events
  sudo nft list ruleset | grep -A5 f2b-        # peek at the kernel ban list

If the daemon's log format changes (siphon-ai upgrade), re-test the
regex with:
  sudo fail2ban-regex '<a real 403 line from journalctl>' \\
       /etc/fail2ban/filter.d/siphon-ai.conf

Full doc: docs/SECURITY_FAIL2BAN.md
EOF
