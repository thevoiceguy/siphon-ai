#!/usr/bin/env bash
#
# install-bot-debian13.sh — automated walk-through of
# docs/BOT_LOCALHOST_SETUP.md
#
# Stands up `examples/deepgram-llm-bot-node` under systemd on the same
# Debian 13 box that already runs siphon-ai. Mirrors the doc step-for-step:
#
#   1. Node 22+ via NodeSource (the apt nodejs is too old for the SDK deps).
#   2. `npm install` in the bot directory.
#   3. `/etc/siphon-bot/env` with API keys + bind address.
#   4. systemd unit at `/etc/systemd/system/siphon-bot.service`.
#   5. (optional) update siphon-ai daemon's `[bridge].ws_url` to the
#      bot's loopback URL.
#   6. enable + start, post-flight `journalctl` check.
#
#   Required (prompts if missing):
#     DEEPGRAM_API_KEY   STT + TTS key.
#     LLM_API_KEY        Key for the LLM endpoint. Defaults to the
#                        same env-var name the bot reads:
#                        `OPENAI_API_KEY` (OpenAI default) or
#                        `BOT_LLM_API_KEY` (any other provider).
#                        Set BOT_LLM_BASE_URL too if non-OpenAI.
#
#   Optional (defaults shown):
#     SOURCE_DIR=/opt/siphon-ai-src
#     BOT_USER=siphon               Operator account (NOT the
#                                   siphon-ai daemon user — the bot
#                                   needs internet egress to Deepgram
#                                   and the LLM provider, and we don't
#                                   want a daemon-user breach to also
#                                   own those credentials).
#     BOT_BIND=127.0.0.1:8080
#     BOT_LLM_MODEL                 e.g. `llama-3.3-70b-versatile`
#     BOT_LLM_BASE_URL              e.g. `https://api.groq.com/openai/v1`
#     BOT_LLM_MAX_TOKENS            cap response length
#     BOT_LLM_TEMPERATURE
#     BOT_SYSTEM_PROMPT             override the default
#     BOT_GREETING                  override the default
#     SIPHON_AI_TOML=/etc/siphon-ai/siphon-ai.toml
#                                   If present, the script offers to
#                                   re-point its `[bridge].ws_url` at
#                                   ws://<BOT_BIND>/ on this host.
#     SKIP_NPM_INSTALL=0            re-run skipping the heavy step.
#     UPDATE_DAEMON_WS_URL=ask      "yes" / "no" / "ask"
#     NONINTERACTIVE=0              fail fast on missing inputs.
#
# Idempotent: re-running is safe. Existing `/etc/siphon-bot/env`
# and the systemd unit get backed up with a timestamped suffix
# before being rewritten so operator edits aren't silently lost.

set -euo pipefail

# ─── Style ────────────────────────────────────────────────────────────────
if [[ -t 1 ]]; then
  C_HDR=$'\033[1;36m'; C_OK=$'\033[0;32m'; C_WARN=$'\033[0;33m'
  C_ERR=$'\033[0;31m'; C_OFF=$'\033[0m'
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

# ─── Inputs ───────────────────────────────────────────────────────────────

prompt_secret() {
  local var="$1" question="$2"
  local current="${!var:-}"
  if [[ -n "$current" ]]; then
    ok "$var=<set from env>"
    return
  fi
  if [[ "${NONINTERACTIVE:-0}" == "1" ]]; then
    fail "$var not set and NONINTERACTIVE=1."
  fi
  read -srp "$question: " reply
  printf '\n'
  [[ -z "$reply" ]] && fail "$var is required."
  printf -v "$var" '%s' "$reply"
  export "$var"
}

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
  local p="$question"
  [[ -n "$default" ]] && p+=" [$default]"
  p+=": "
  read -rp "$p" reply
  reply="${reply:-$default}"
  [[ -z "$reply" ]] && fail "$var is required."
  printf -v "$var" '%s' "$reply"
  export "$var"
}

# Defaults
SOURCE_DIR="${SOURCE_DIR:-/opt/siphon-ai-src}"
BOT_USER="${BOT_USER:-siphon}"
BOT_BIND="${BOT_BIND:-127.0.0.1:8080}"
SIPHON_AI_TOML="${SIPHON_AI_TOML:-/etc/siphon-ai/siphon-ai.toml}"
SKIP_NPM_INSTALL="${SKIP_NPM_INSTALL:-0}"
UPDATE_DAEMON_WS_URL="${UPDATE_DAEMON_WS_URL:-ask}"
NONINTERACTIVE="${NONINTERACTIVE:-0}"

# Optional LLM tuning passthrough — only injected into env file if set.
BOT_LLM_BASE_URL="${BOT_LLM_BASE_URL:-}"
BOT_LLM_MODEL="${BOT_LLM_MODEL:-}"
BOT_LLM_MAX_TOKENS="${BOT_LLM_MAX_TOKENS:-}"
BOT_LLM_TEMPERATURE="${BOT_LLM_TEMPERATURE:-}"
BOT_SYSTEM_PROMPT="${BOT_SYSTEM_PROMPT:-}"
BOT_GREETING="${BOT_GREETING:-}"

step "Required parameters"
prompt_secret DEEPGRAM_API_KEY "Deepgram API key (STT + TTS)"

# LLM API key: prefer BOT_LLM_API_KEY (any provider) if BOT_LLM_BASE_URL
# is set, otherwise fall back to OPENAI_API_KEY semantics.
if [[ -n "$BOT_LLM_BASE_URL" ]]; then
  prompt_secret BOT_LLM_API_KEY "LLM API key for $BOT_LLM_BASE_URL"
  LLM_KEY_VAR="BOT_LLM_API_KEY"
  LLM_KEY_VAL="$BOT_LLM_API_KEY"
else
  prompt_secret OPENAI_API_KEY "OpenAI API key (or set BOT_LLM_BASE_URL for a different provider)"
  LLM_KEY_VAR="OPENAI_API_KEY"
  LLM_KEY_VAL="$OPENAI_API_KEY"
fi

ok "All inputs collected."

# ─── 1. Node.js via NodeSource ────────────────────────────────────────────

step "Section 1: Node 22+ (NodeSource)"

NODE_OK=0
if command -v node >/dev/null; then
  ver=$(node -p 'process.versions.node.split(".")[0]')
  if [[ "$ver" -ge 20 ]]; then
    ok "Node $(node --version) already installed (>=20)."
    NODE_OK=1
  else
    warn "Node $(node --version) is too old; upgrading via NodeSource."
  fi
fi

if [[ $NODE_OK -eq 0 ]]; then
  # NodeSource repo for the current LTS major.
  curl -fsSL https://deb.nodesource.com/setup_22.x | sudo -E bash - >/dev/null 2>&1
  sudo apt install -y nodejs >/dev/null
  ok "Installed Node $(node --version)."
fi

# ─── 2. Install bot dependencies ──────────────────────────────────────────

step "Section 2: Bot dependencies ($SOURCE_DIR/examples/deepgram-llm-bot-node)"

BOT_DIR="$SOURCE_DIR/examples/deepgram-llm-bot-node"
if [[ ! -d "$BOT_DIR" ]]; then
  fail "$BOT_DIR not found. Clone the repo to $SOURCE_DIR first (see docs/INSTALL_DEBIAN13.md §2)."
fi

if [[ "$SKIP_NPM_INSTALL" == "1" ]]; then
  ok "Skipping npm install (SKIP_NPM_INSTALL=1)."
elif [[ -d "$BOT_DIR/node_modules" ]] && [[ -f "$BOT_DIR/package-lock.json" ]]; then
  # `npm ci` is faster and stricter when a lockfile already exists.
  ( cd "$BOT_DIR" && npm ci --no-fund --no-audit >/dev/null )
  ok "npm ci complete."
else
  ( cd "$BOT_DIR" && npm install --no-fund --no-audit >/dev/null )
  ok "npm install complete."
fi

# ─── 3. /etc/siphon-bot/env ───────────────────────────────────────────────

step "Section 3: /etc/siphon-bot/env"

sudo install -d -o root -g root -m 0755 /etc/siphon-bot

backup_if_exists() {
  local path="$1"
  if [[ -e "$path" ]]; then
    local backup="${path}.bak.$(date +%Y%m%d-%H%M%S)"
    sudo cp -a "$path" "$backup"
    warn "Backed up existing $(basename "$path") → $backup"
  fi
}

backup_if_exists /etc/siphon-bot/env
{
  printf 'DEEPGRAM_API_KEY=%s\n'     "$DEEPGRAM_API_KEY"
  printf '%s=%s\n'                    "$LLM_KEY_VAR" "$LLM_KEY_VAL"
  [[ -n "$BOT_LLM_BASE_URL"   ]] && printf 'BOT_LLM_BASE_URL=%s\n'   "$BOT_LLM_BASE_URL"
  [[ -n "$BOT_LLM_MODEL"      ]] && printf 'BOT_LLM_MODEL=%s\n'      "$BOT_LLM_MODEL"
  [[ -n "$BOT_LLM_MAX_TOKENS" ]] && printf 'BOT_LLM_MAX_TOKENS=%s\n' "$BOT_LLM_MAX_TOKENS"
  [[ -n "$BOT_LLM_TEMPERATURE" ]] && printf 'BOT_LLM_TEMPERATURE=%s\n' "$BOT_LLM_TEMPERATURE"
  [[ -n "$BOT_SYSTEM_PROMPT"  ]] && printf 'BOT_SYSTEM_PROMPT=%s\n'  "$BOT_SYSTEM_PROMPT"
  [[ -n "$BOT_GREETING"       ]] && printf 'BOT_GREETING=%s\n'       "$BOT_GREETING"
  printf 'BOT_BIND=%s\n' "$BOT_BIND"
} | sudo tee /etc/siphon-bot/env >/dev/null
sudo chown root:"$BOT_USER" /etc/siphon-bot/env
sudo chmod 0640 /etc/siphon-bot/env
ok "/etc/siphon-bot/env written (mode 0640, owned root:$BOT_USER)"

# ─── 4. systemd unit ──────────────────────────────────────────────────────

step "Section 4: systemd unit"

# Verify the bot user exists. We don't create it — the doc explicitly
# assumes the operator account ('siphon' by default) is already present
# from the OS install.
if ! id "$BOT_USER" >/dev/null 2>&1; then
  fail "User '$BOT_USER' doesn't exist. Either create it or rerun with BOT_USER=<existing-user>."
fi

backup_if_exists /etc/systemd/system/siphon-bot.service
sudo tee /etc/systemd/system/siphon-bot.service >/dev/null <<EOF
[Unit]
Description=SiphonAI Deepgram/LLM voice agent
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=$BOT_USER
Group=$BOT_USER
WorkingDirectory=$BOT_DIR
EnvironmentFile=/etc/siphon-bot/env
ExecStart=/usr/bin/node server.js
Restart=always
RestartSec=5
StandardOutput=journal
StandardError=journal
LimitNOFILE=65536

[Install]
WantedBy=multi-user.target
EOF
sudo systemctl daemon-reload
ok "/etc/systemd/system/siphon-bot.service written + daemon-reloaded"

# ─── 5. Update siphon-ai daemon's [bridge].ws_url (optional) ──────────────

step "Section 5: Daemon [bridge].ws_url"

bot_ws_url="ws://${BOT_BIND}/"
if [[ ! -e "$SIPHON_AI_TOML" ]]; then
  warn "$SIPHON_AI_TOML not found — skipping ws_url update."
  warn "Set [bridge].ws_url = \"$bot_ws_url\" manually once the daemon is installed."
else
  current=$(sudo grep -E '^ws_url *=' "$SIPHON_AI_TOML" | head -1 | sed -E 's/.*= *//;s/[ "\047]+//g' || true)
  if [[ -z "$current" ]]; then
    warn "Couldn't read existing ws_url from $SIPHON_AI_TOML."
    warn "Set [bridge].ws_url = \"$bot_ws_url\" by hand."
  elif [[ "$current" == "$bot_ws_url" ]]; then
    ok "Daemon's [bridge].ws_url already points at $bot_ws_url."
  else
    do_update=""
    case "$UPDATE_DAEMON_WS_URL" in
      yes) do_update=1 ;;
      no)  do_update=0 ;;
      ask|*)
        if [[ "$NONINTERACTIVE" == "1" ]]; then
          warn "UPDATE_DAEMON_WS_URL=ask in NONINTERACTIVE mode → skipping."
          do_update=0
        else
          read -rp "  Repoint daemon ws_url from $current to $bot_ws_url? [y/N]: " ans
          [[ "$ans" =~ ^[Yy]$ ]] && do_update=1 || do_update=0
        fi
        ;;
    esac
    if [[ "$do_update" == "1" ]]; then
      backup_if_exists "$SIPHON_AI_TOML"
      # Replace the first ws_url = "..." line. Anchor on `ws_url` to
      # avoid clobbering any other URL-shaped fields.
      sudo sed -i -E "s|^(ws_url *= *).*$|\1\"$bot_ws_url\"|" "$SIPHON_AI_TOML"
      ok "Updated $SIPHON_AI_TOML ws_url → $bot_ws_url"
      if systemctl is-active --quiet siphon-ai; then
        sudo systemctl restart siphon-ai
        ok "Restarted siphon-ai to pick up the new ws_url."
      fi
    else
      warn "Left daemon ws_url at $current. Update by hand if you want it on the bot."
    fi
  fi
fi

# ─── 6. Enable + start ────────────────────────────────────────────────────

step "Section 6: Enable + start siphon-bot"

sudo systemctl enable --now siphon-bot >/dev/null 2>&1
sleep 2

if ! systemctl is-active --quiet siphon-bot; then
  sudo systemctl status siphon-bot --no-pager -n 30 >&2 || true
  fail "siphon-bot.service is not active. See journalctl output above."
fi
ok "siphon-bot is active ($(systemctl is-active siphon-bot))"

# Surface the startup line so the operator can see which LLM the bot picked.
startup_line=$(sudo journalctl -u siphon-bot -n 50 --no-pager 2>/dev/null \
               | grep -E '\[llm\] model=' | tail -1 || true)
[[ -n "$startup_line" ]] && ok "${startup_line#*]: }"

# ─── Done ─────────────────────────────────────────────────────────────────

cat <<EOF

${C_OK}━━━ Bot install complete${C_OFF}

Bot listens on ws://${BOT_BIND}/  (running as user '${BOT_USER}')

Tail logs:        sudo journalctl -u siphon-bot -f
Restart:          sudo systemctl restart siphon-bot
Edit env file:    sudo $EDITOR /etc/siphon-bot/env  (then restart)

Next:
  1. If the script didn't repoint it for you: set
     [bridge].ws_url = "ws://${BOT_BIND}/" in
     /etc/siphon-ai/siphon-ai.toml and restart siphon-ai.
  2. Place a test call. Bot logs will show metric lines per turn
     (\`metric turn_summary user_to_audio_ms=…\`).
  3. For LLM provider tuning (Groq, Anthropic, OpenRouter, Ollama),
     append BOT_LLM_BASE_URL/MODEL/etc. to /etc/siphon-bot/env and
     restart — see docs/BOT_LOCALHOST_SETUP.md §3 "Choosing the LLM".

EOF
