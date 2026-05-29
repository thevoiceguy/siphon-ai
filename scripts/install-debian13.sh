#!/usr/bin/env bash
#
# install-debian13.sh — automated walk-through of docs/INSTALL_DEBIAN13.md
#
# Runs sections 1-6 of the install guide on a fresh Debian 13 host and
# leaves you with a running siphon-ai systemd service. Mirrors the
# guide step-for-step so any divergence is easy to spot.
#
#   Required (script will prompt if missing):
#     TRUNK_PEER_IP   The peer (FreeSWITCH, ITSP) allowed to send INVITEs.
#                     One IP or CIDR. Pasted into the [[trunk]] block.
#
#   Auto-detected (script will prompt to confirm):
#     PUBLIC_IP       This host's routable address. Pasted into
#                     [node].public_address.
#
#   Optional (defaults shown):
#     SOURCE_DIR=/opt/siphon-ai-src
#     SERVICE_USER=siphon-ai
#     NODE_ID=siphon-prod-1
#     WS_URL=ws://127.0.0.1:8080/
#     SIP_PORT=5060
#     RTP_PORT_MIN=40000
#     RTP_PORT_MAX=40500
#     OBS_LISTEN=127.0.0.1:9091
#     REPO_URL=https://github.com/thevoiceguy/siphon-ai.git
#     REPO_BRANCH=main
#     SKIP_BUILD=0     Set to 1 to reuse an existing target/release build.
#     SKIP_RUSTUP=0    Set to 1 if rustup is already installed/usable.
#     NONINTERACTIVE=0 Set to 1 to fail-fast instead of prompting.
#
# Idempotent: re-running is safe. Existing configs get backed up to
# /etc/siphon-ai/*.bak.<timestamp> before being rewritten.
#
# Skipped on purpose:
#   * Firewall (section 7) — varies by host; see docs/INSTALL_DEBIAN13.md §7
#   * fail2ban — see docs/SECURITY_FAIL2BAN.md
#   * FreeSWITCH side — see docs/FREESWITCH_INTEGRATION.md

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

if ! command -v sudo >/dev/null; then
  fail "sudo not installed."
fi

# Validate Debian 13. The script may work on related releases but we
# only test against trixie.
if [[ -r /etc/os-release ]]; then
  . /etc/os-release
  case "${ID:-}:${VERSION_CODENAME:-}" in
    debian:trixie) ok "Debian 13 (trixie) detected." ;;
    *) warn "Untested OS: ${PRETTY_NAME:-unknown}. Proceeding anyway." ;;
  esac
fi

# ─── Inputs ───────────────────────────────────────────────────────────────

prompt() {
  local var="$1" question="$2" default="${3:-}"
  local current="${!var:-}"
  if [[ -n "$current" ]]; then
    ok "$var=$current (from env)"
    return
  fi
  if [[ "${NONINTERACTIVE:-0}" == "1" ]]; then
    [[ -n "$default" ]] && { printf -v "$var" '%s' "$default"; export "$var"; ok "$var=$default (default, noninteractive)"; return; }
    fail "$var not set and NONINTERACTIVE=1."
  fi
  local prompt_text="$question"
  [[ -n "$default" ]] && prompt_text+=" [$default]"
  prompt_text+=": "
  read -rp "$prompt_text" reply
  reply="${reply:-$default}"
  [[ -z "$reply" ]] && fail "$var is required."
  printf -v "$var" '%s' "$reply"
  export "$var"
}

# Defaults
SOURCE_DIR="${SOURCE_DIR:-/opt/siphon-ai-src}"
SERVICE_USER="${SERVICE_USER:-siphon-ai}"
NODE_ID="${NODE_ID:-siphon-prod-1}"
WS_URL="${WS_URL:-ws://127.0.0.1:8080/}"
SIP_PORT="${SIP_PORT:-5060}"
RTP_PORT_MIN="${RTP_PORT_MIN:-40000}"
RTP_PORT_MAX="${RTP_PORT_MAX:-40500}"
OBS_LISTEN="${OBS_LISTEN:-127.0.0.1:9091}"
REPO_URL="${REPO_URL:-https://github.com/thevoiceguy/siphon-ai.git}"
REPO_BRANCH="${REPO_BRANCH:-main}"
SKIP_BUILD="${SKIP_BUILD:-0}"
SKIP_RUSTUP="${SKIP_RUSTUP:-0}"
NONINTERACTIVE="${NONINTERACTIVE:-0}"

step "Required parameters"
prompt TRUNK_PEER_IP "Trunk peer IP or CIDR (FreeSWITCH, ITSP)"

# Auto-detect public IP from the default route's source.
DETECTED_PUBLIC_IP=$(ip -4 route get 1.1.1.1 2>/dev/null | awk '/src/ {for (i=1; i<=NF; i++) if ($i=="src") print $(i+1)}' || true)
prompt PUBLIC_IP "This host's public IP (in SDP c= line)" "$DETECTED_PUBLIC_IP"

ok "All inputs collected. Starting install."

# ─── 1. System packages ───────────────────────────────────────────────────

step "Section 1: System packages"
sudo apt update -qq
sudo apt install -y \
    build-essential \
    pkg-config \
    libssl-dev \
    ca-certificates \
    curl \
    git \
    libsystemd-dev \
    sip-tester \
    >/dev/null
ok "apt packages installed."

# ─── 1b. Rust toolchain ───────────────────────────────────────────────────

step "Section 1: Rust toolchain (rustup)"

if [[ "$SKIP_RUSTUP" == "1" ]]; then
  ok "Skipping rustup install (SKIP_RUSTUP=1)."
elif command -v rustup >/dev/null; then
  rustup update stable >/dev/null
  ok "rustup already present; stable updated."
else
  # Debian's apt rustc is 1.85 — too old for the workspace. Use
  # rustup so rust-toolchain.toml in the repo picks the right
  # version automatically.
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
    | sh -s -- -y --default-toolchain stable >/dev/null
  # shellcheck source=/dev/null
  source "$HOME/.cargo/env"
  ok "rustup installed."
fi

# Ensure cargo is on PATH for this script's later steps.
if [[ -f "$HOME/.cargo/env" ]]; then
  # shellcheck source=/dev/null
  source "$HOME/.cargo/env"
fi
command -v cargo >/dev/null || fail "cargo not on PATH after rustup install."
ok "rustc: $(rustc --version)"

# ─── 2. Get the source ────────────────────────────────────────────────────

step "Section 2: Get the source ($SOURCE_DIR)"

if [[ ! -d "$SOURCE_DIR" ]]; then
  sudo install -d -o "$USER" -g "$USER" "$SOURCE_DIR"
fi

if [[ -d "$SOURCE_DIR/.git" ]]; then
  ok "$SOURCE_DIR is a git checkout; pulling latest."
  git -C "$SOURCE_DIR" fetch --quiet origin "$REPO_BRANCH"
  git -C "$SOURCE_DIR" checkout --quiet "$REPO_BRANCH"
  git -C "$SOURCE_DIR" pull --ff-only --quiet
else
  if [[ -n "$(ls -A "$SOURCE_DIR" 2>/dev/null)" ]]; then
    fail "$SOURCE_DIR exists and isn't a git repo. Move it aside and retry."
  fi
  git clone --quiet --branch "$REPO_BRANCH" "$REPO_URL" "$SOURCE_DIR"
fi
ok "Source at $(git -C "$SOURCE_DIR" log -1 --oneline)"

# ─── 3. Build ─────────────────────────────────────────────────────────────

step "Section 3: Build"

if [[ "$SKIP_BUILD" == "1" ]]; then
  ok "Skipping build (SKIP_BUILD=1)."
else
  ( cd "$SOURCE_DIR" && cargo build --release -p siphon-ai )
  ok "Release binary: $SOURCE_DIR/target/release/siphon-ai"
fi

[[ -x "$SOURCE_DIR/target/release/siphon-ai" ]] \
  || fail "Build artifact missing at $SOURCE_DIR/target/release/siphon-ai"

# ─── 4. Install layout ────────────────────────────────────────────────────

step "Section 4: Install layout (user + dirs + binary)"

if ! id "$SERVICE_USER" >/dev/null 2>&1; then
  sudo useradd \
    --system \
    --home-dir "/var/lib/$SERVICE_USER" \
    --shell /usr/sbin/nologin \
    "$SERVICE_USER"
  ok "Created service user: $SERVICE_USER"
else
  ok "Service user $SERVICE_USER already exists."
fi

sudo install -d -o root           -g root            -m 0755 /etc/siphon-ai
sudo install -d -o "$SERVICE_USER" -g "$SERVICE_USER" -m 0750 /etc/siphon-ai/env.d
sudo install -d -o "$SERVICE_USER" -g "$SERVICE_USER" -m 0750 /var/log/siphon-ai
sudo install -m 0755 "$SOURCE_DIR/target/release/siphon-ai" /usr/local/bin/siphon-ai
ok "Binary installed to /usr/local/bin/siphon-ai"

# ─── 5. Configure ─────────────────────────────────────────────────────────

step "Section 5: Configure"

backup_if_exists() {
  local path="$1"
  if [[ -e "$path" ]]; then
    local backup
    backup="${path}.bak.$(date +%Y%m%d-%H%M%S)"
    sudo cp -a "$path" "$backup"
    warn "Backed up existing $(basename "$path") → $backup"
  fi
}

backup_if_exists /etc/siphon-ai/siphon-ai.toml
sudo tee /etc/siphon-ai/siphon-ai.toml >/dev/null <<EOF
[node]
id             = "$NODE_ID"
public_address = "$PUBLIC_IP"

[sip]
listen     = "0.0.0.0:$SIP_PORT"
transports = ["udp"]
user_agent = "SiphonAI/0.1.0"

[media]
codecs                  = ["pcmu", "pcma"]
dtmf                    = "rfc2833"
rtp_port_range          = [$RTP_PORT_MIN, $RTP_PORT_MAX]
inactivity_timeout_secs = 60

[bridge]
ws_url                = "$WS_URL"
ws_connect_timeout_ms = 3000

[observability]
enabled     = true
http_listen = "$OBS_LISTEN"

# Trunk allowlist — INVITEs from any other peer get 403.
[[trunk]]
name       = "freeswitch-main"
peer_addrs = ["$TRUNK_PEER_IP"]

[[route]]
name = "fs-9000"
[route.match]
register_source  = "freeswitch-main"
request_uri_user = "9000"

[[route]]
name = "default"
[route.match]
any = true
EOF
sudo chown root:"$SERVICE_USER" /etc/siphon-ai/siphon-ai.toml
sudo chmod 0640 /etc/siphon-ai/siphon-ai.toml
ok "/etc/siphon-ai/siphon-ai.toml written"

if [[ ! -e /etc/siphon-ai/env ]]; then
  sudo tee /etc/siphon-ai/env >/dev/null <<'EOF'
BRIDGE_TOKEN=replace-me
HEP_PASSWORD=replace-me
EOF
  sudo chown root:"$SERVICE_USER" /etc/siphon-ai/env
  sudo chmod 0640 /etc/siphon-ai/env
  ok "/etc/siphon-ai/env created (placeholders — edit before going to prod)"
else
  ok "/etc/siphon-ai/env exists, left as-is"
fi

# ─── 6. systemd unit ──────────────────────────────────────────────────────

step "Section 6: systemd unit"

backup_if_exists /etc/systemd/system/siphon-ai.service
sudo tee /etc/systemd/system/siphon-ai.service >/dev/null <<EOF
[Unit]
Description=SiphonAI — SIP-to-WebSocket bridge
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=$SERVICE_USER
Group=$SERVICE_USER
EnvironmentFile=-/etc/siphon-ai/env
ExecStart=/usr/local/bin/siphon-ai --config /etc/siphon-ai/siphon-ai.toml
Restart=always
RestartSec=5
StandardOutput=journal
StandardError=journal
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
PrivateTmp=true
ReadWritePaths=/var/log/siphon-ai
LimitNOFILE=65536

[Install]
WantedBy=multi-user.target
EOF
sudo systemctl daemon-reload
sudo systemctl enable --now siphon-ai
ok "siphon-ai.service enabled and started"

# Give the daemon a beat to bind its sockets before we probe it.
sleep 2

# ─── 8. Verify ────────────────────────────────────────────────────────────

step "Section 8: Verify"

# Service should be running. Fail loud if not — config errors surface
# here, not later when calls land.
if ! systemctl is-active --quiet siphon-ai; then
  sudo systemctl status siphon-ai --no-pager -n 30 >&2 || true
  fail "siphon-ai service is not active. See journalctl output above."
fi
ok "systemctl is-active siphon-ai: $(systemctl is-active siphon-ai)"

# Probe the admin HTTP endpoints — these answer iff [observability] came up.
obs_host_port="$OBS_LISTEN"
[[ "$obs_host_port" == "0.0.0.0:"* ]] && obs_host_port="127.0.0.1:${obs_host_port##*:}"

if h=$(curl -sf --max-time 3 "http://$obs_host_port/health"); then
  ok "/health → $h"
else
  warn "/health probe failed at http://$obs_host_port/health"
fi

if r=$(curl -sf --max-time 3 "http://$obs_host_port/ready"); then
  ok "/ready → $r"
else
  warn "/ready probe failed at http://$obs_host_port/ready"
fi

# ─── Done ─────────────────────────────────────────────────────────────────

cat <<EOF

${C_OK}━━━ Install complete${C_OFF}

The daemon is running. SIP listens on UDP $SIP_PORT, RTP on
$RTP_PORT_MIN-$RTP_PORT_MAX, and Prometheus + admin on $OBS_LISTEN.

Tail logs:        sudo journalctl -u siphon-ai -f
Active calls:     curl -s http://$obs_host_port/admin/calls | jq
Metrics:          curl -s http://$obs_host_port/metrics | grep siphon_ai_

Next:
  1. Edit /etc/siphon-ai/env to set real secrets (if you reference
     any \${VAR} in the TOML config).
  2. Open the firewall (docs/INSTALL_DEBIAN13.md §7).
  3. Set up fail2ban — run scripts/install-fail2ban.sh, or see
     docs/SECURITY_FAIL2BAN.md for the manual walk-through.
  4. Wire FreeSWITCH (docs/FREESWITCH_INTEGRATION.md) — note the
     bypass_media=true requirement in that doc.
  5. Stand up the bot (docs/BOT_LOCALHOST_SETUP.md).

EOF
