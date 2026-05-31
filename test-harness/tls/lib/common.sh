# Shared style + lifecycle helpers for the TLS validation suite.
# Source this from each scenario script. NOT a standalone script —
# everything here mutates the caller's shell state.

# ─── Style ────────────────────────────────────────────────────────────────
if [[ -t 1 ]]; then
  C_HDR=$'\033[1;36m'; C_OK=$'\033[0;32m'; C_WARN=$'\033[0;33m'
  C_ERR=$'\033[0;31m'; C_DIM=$'\033[0;90m'; C_OFF=$'\033[0m'
else
  C_HDR=''; C_OK=''; C_WARN=''; C_ERR=''; C_DIM=''; C_OFF=''
fi

step()   { printf '\n%s━━━ %s%s\n' "$C_HDR" "$*" "$C_OFF"; }
ok()     { printf '  %s✓%s %s\n' "$C_OK"   "$C_OFF" "$*"; }
warn()   { printf '  %s!%s %s\n' "$C_WARN" "$C_OFF" "$*"; }
note()   { printf '  %s·%s %s\n' "$C_DIM"  "$C_OFF" "$*"; }
fail()   { printf '  %s✗%s %s\n' "$C_ERR"  "$C_OFF" "$*" >&2; exit 1; }

verdict_pass()   { printf '\n  %s✓ PASS%s — %s\n' "$C_OK"   "$C_OFF" "$*"; }
verdict_manual() { printf '\n  %s! MANUAL CHECK%s — %s\n' "$C_WARN" "$C_OFF" "$*"; }
verdict_fail()   { printf '\n  %s✗ FAIL%s — %s\n'   "$C_ERR"  "$C_OFF" "$*" >&2; exit 2; }

# ─── Ephemeral-port picker ────────────────────────────────────────────────
#
# Each scenario binds its own ports so a running prod siphon-ai on
# the same box doesn't get in the way. `pick_port [base]` returns
# the next free port at or above `base` (default 15060).
pick_port() {
  local p="${1:-15060}"
  while (( p < 65535 )); do
    if ! ss -lntu 2>/dev/null | awk '{print $5}' | grep -qE ":${p}\$"; then
      printf '%d' "$p"; return 0
    fi
    (( p++ ))
  done
  fail "no free port found above ${1:-15060}"
}

# ─── Cleanup trap registry ────────────────────────────────────────────────
#
# Scenarios push commands onto _CLEANUPS; exit_trap runs them in
# reverse order on EXIT. Lets each scenario register teardown right
# next to its setup, no central bookkeeping.
_CLEANUPS=()
_CLEANUPS_NEVER_SKIP=()
# `on_exit_cleanup "cmd"` runs `cmd` on EXIT *unless* the script
# is failing AND `KEEP_WORK_ON_FAIL=1` (default in the suite).
# `on_exit_cleanup --always "cmd"` runs unconditionally (use for
# killing background pids — we don't want to leak processes even
# when keeping the workdir for inspection).
on_exit_cleanup() {
  if [[ "$1" == "--always" ]]; then shift; _CLEANUPS_NEVER_SKIP+=("$*")
  else                              _CLEANUPS+=("$*"); fi
}
_run_cleanups() {
  local rc=$? i
  # Always: kill pids etc.
  for (( i=${#_CLEANUPS_NEVER_SKIP[@]}-1; i>=0; i-- )); do
    bash -c "${_CLEANUPS_NEVER_SKIP[i]}" >/dev/null 2>&1 || true
  done
  # Skippable (workdir rms etc.): only run on successful exit unless
  # KEEP_WORK_ON_FAIL=0 explicitly opts out.
  if (( rc != 0 )) && [[ "${KEEP_WORK_ON_FAIL:-1}" == "1" ]]; then
    printf '\n  · Exit rc=%d — workdir preserved for inspection.\n' "$rc" >&2
    return
  fi
  for (( i=${#_CLEANUPS[@]}-1; i>=0; i-- )); do
    bash -c "${_CLEANUPS[i]}" >/dev/null 2>&1 || true
  done
}
trap _run_cleanups EXIT INT TERM

# ─── Polling helpers ──────────────────────────────────────────────────────
#
# `wait_for "<bash test>" <max_seconds>` polls until the test
# command succeeds. Returns 0 on success, 1 on timeout. Useful for
# "wait for siphon-ai to bind its socket" / "wait for the
# metric to appear" patterns.
wait_for() {
  local cmd="$1" max="${2:-10}"
  local end=$(( SECONDS + max ))
  while (( SECONDS < end )); do
    if bash -c "$cmd" >/dev/null 2>&1; then return 0; fi
    sleep 0.2
  done
  return 1
}

# ─── PKI helpers ──────────────────────────────────────────────────────────
#
# Generate a throwaway CA + leaf cert/key signed by it. Writes
# <out>/ca.{pem,key}, <out>/leaf.{pem,key}, <out>/leaf.spki.sha256.
# `subject_cn` defaults to "siphon-ai-test".
gen_test_pki() {
  local out="$1" subject_cn="${2:-siphon-ai-test}"
  mkdir -p "$out"

  # CA
  openssl req -x509 -newkey rsa:2048 -nodes -days 365 \
    -keyout "$out/ca.key" -out "$out/ca.pem" \
    -subj "/CN=siphon-ai-test-ca" 2>/dev/null

  # Leaf CSR + sign
  openssl req -new -newkey rsa:2048 -nodes \
    -keyout "$out/leaf.key" -out "$out/leaf.csr" \
    -subj "/CN=$subject_cn" 2>/dev/null
  openssl x509 -req -in "$out/leaf.csr" -CA "$out/ca.pem" -CAkey "$out/ca.key" \
    -CAcreateserial -days 365 -out "$out/leaf.pem" \
    -extfile <(printf 'subjectAltName=DNS:%s,DNS:localhost,IP:127.0.0.1' "$subject_cn") \
    2>/dev/null
  rm -f "$out/leaf.csr" "$out/ca.srl"

  # Pin: SHA-256 of the leaf's SubjectPublicKeyInfo, lowercase hex.
  # Matches the format expected by [bridge.tls.pinned_sha256].
  openssl x509 -in "$out/leaf.pem" -pubkey -noout \
    | openssl pkey -pubin -outform DER 2>/dev/null \
    | openssl dgst -sha256 -hex \
    | awk '{print $NF}' > "$out/leaf.spki.sha256"
}
