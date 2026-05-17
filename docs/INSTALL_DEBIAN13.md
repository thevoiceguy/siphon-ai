# Installing SiphonAI on Debian 13 ("Trixie")

End-to-end walkthrough for a fresh Debian 13 server. Produces a
running `siphon-ai` daemon under `systemd`, listening on a SIP
port, exposing `/metrics` and `/admin/*` on a private port, and
ready to bridge calls to a WebSocket server. About 15 minutes
end-to-end on a small VM.

Assumes a non-root user with `sudo`. Replace `siphon@host` with
your shell prompt; replace IPs with your own.

---

## 1. System prerequisites

### Packages

```bash
sudo apt update
sudo apt install -y \
    build-essential \
    pkg-config \
    libssl-dev \
    ca-certificates \
    curl \
    git \
    libsystemd-dev \
    sip-tester        # SIPp — used to smoke-test the install
```

`build-essential`, `pkg-config`, `libssl-dev` cover the C
toolchain bits the Rust crates' build scripts dlopen. `sip-tester`
ships SIPp v3.7 which is plenty new for the bundled scenarios.

### Rust toolchain

**Use `rustup` — not Debian's `rustc` package.** Debian 13 ships
`rustc 1.85`, which is too old: transitive deps from
`forge-media` / `siphon-rs` (the `icu_*`, `smol_str`, `time`,
etc. families) require `rustc 1.89` or newer. The workspace's
`rust-toolchain.toml` pins the channel to `stable`, so rustup
picks up the right version automatically; `apt install rustc`
gets you a hard `error: rustc 1.85 is not supported` mid-build.

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable
source "$HOME/.cargo/env"
rustc --version    # 1.89+ for the workspace
```

If `rustup` is already installed from an earlier setup, make
sure stable is current:

```bash
rustup update stable
```

---

## 2. Get the source

```bash
sudo install -d -o "$USER" -g "$USER" /opt/siphon-ai-src
git clone https://github.com/thevoiceguy/siphon-ai.git /opt/siphon-ai-src
cd /opt/siphon-ai-src
```

The first build pulls `siphon-rs`, `forge-media`, and `hep-rs` from
git — make sure outbound HTTPS to `github.com` is allowed.

---

## 3. Build

```bash
cargo build --release -p siphon-ai
```

Cold builds take ~3–4 minutes on a 4 vCPU box. The release binary
lands at `target/release/siphon-ai` (~31 MB static-ish ELF).
`cargo test --workspace` is optional but a good sanity step.

---

## 4. Install layout

The daemon is one binary plus a TOML config and (optionally) a
state directory for CDR files. The convention this guide uses:

| Path                                  | What lives there |
|---------------------------------------|------------------|
| `/usr/local/bin/siphon-ai`            | The compiled binary. |
| `/etc/siphon-ai/siphon-ai.toml`       | Daemon config. |
| `/etc/siphon-ai/env`                  | Secrets in `KEY=VALUE` form for systemd `EnvironmentFile`. |
| `/var/log/siphon-ai/`                 | CDR JSONL files (optional). |
| `/run/siphon-ai/`                     | Sockets / pidfiles (systemd manages). |

Create the dirs and the dedicated service user. The user is
intentionally distinct from any operator-side `siphon` /
admin login: it has no login shell, no home outside its state
dir, and the daemon runs as it under systemd. Splitting the
two means a breach of the daemon doesn't hand the attacker a
sudo-capable account.

```bash
sudo useradd --system --home-dir /var/lib/siphon-ai --shell /usr/sbin/nologin siphon-ai
sudo install -d -o root      -g root      -m 0755 /etc/siphon-ai
sudo install -d -o siphon-ai -g siphon-ai -m 0750 /etc/siphon-ai/env.d
sudo install -d -o siphon-ai -g siphon-ai -m 0750 /var/log/siphon-ai
sudo install -m 0755 target/release/siphon-ai /usr/local/bin/siphon-ai
```

---

## 5. Configure

A working trunk-mode config — accept inbound INVITEs on UDP 5060,
bridge every call to a WebSocket server, expose Prometheus +
admin on 127.0.0.1:9091. Adjust IPs and the WS URL.

> **Replace `<YOUR_PUBLIC_IP>` below with this server's actual
> reachable address.** It's pasted into `c=IN IP4 …` in every SDP
> answer. Get it with `ip -4 addr show | grep "inet "` — pick the
> address your trunk peer (FreeSWITCH, ITSP) can reach. **Leaving
> the placeholder produces a call that signals fine but plays no
> audio**, because RTP from the peer is sent into a black hole
> and a tcpdump on this host sees nothing inbound.

```bash
sudo tee /etc/siphon-ai/siphon-ai.toml >/dev/null <<'EOF'
[node]
id             = "siphon-prod-1"
# Required when [sip].listen binds the wildcard. Use the daemon's
# routable IP — this is what the SDP answer's c= line advertises.
# Do NOT leave the placeholder; see the WARNING above.
public_address = "<YOUR_PUBLIC_IP>"

[sip]
listen     = "0.0.0.0:5060"
transports = ["udp"]
user_agent = "SiphonAI/0.1.0"

[media]
codecs                  = ["pcmu", "pcma"]
dtmf                    = "rfc2833"
# RTP port pool. Forward this whole range on any firewall in
# front of the daemon. 50 calls × 2 ports per call ≥ 100 ports.
rtp_port_range          = [40000, 40500]
# RTP watchdog — tear the call down after N seconds of no inbound
# RTP. 60 s default keeps abandoned calls from holding ports.
inactivity_timeout_secs = 60

[bridge]
ws_url                = "ws://10.0.0.20:8080/"
ws_connect_timeout_ms = 3000

[observability]
enabled     = true
http_listen = "127.0.0.1:9091"

# ─── Trunk allowlist ─────────────────────────────────────────
# Required for production. Identifies inbound peers by source IP
# and/or From-URI host. Anything not matching a [[trunk]] block
# is rejected with 403 Forbidden at the SIP layer. Omit [[trunk]]
# entirely to accept INVITEs from ANY source — dev / behind-
# firewall only. See docs/CONFIG.md §"[[trunk]]" for the threat
# model and CIDR / from_hosts grammar.
[[trunk]]
name       = "freeswitch-main"
peer_addrs = ["10.0.0.10"]     # FreeSWITCH server

[[route]]
name = "fs-9000"
[route.match]
register_source = "freeswitch-main"
request_uri_user = "9000"

[[route]]
name = "default"
[route.match]
any = true
EOF
sudo chown root:siphon-ai /etc/siphon-ai/siphon-ai.toml
sudo chmod 0640 /etc/siphon-ai/siphon-ai.toml
```

The `WS server` is the box that runs the bot. For the FreeSWITCH
integration in `docs/FREESWITCH_INTEGRATION.md` we route inbound
`9000` from FreeSWITCH through this trunk and into the bot.

### Secrets

Put any `${VAR}` your TOML references in
`/etc/siphon-ai/env`:

```bash
sudo tee /etc/siphon-ai/env >/dev/null <<'EOF'
BRIDGE_TOKEN=replace-me
HEP_PASSWORD=replace-me
EOF
sudo chown root:siphon-ai /etc/siphon-ai/env
sudo chmod 0640 /etc/siphon-ai/env
```

---

## 6. systemd unit

```bash
sudo tee /etc/systemd/system/siphon-ai.service >/dev/null <<'EOF'
[Unit]
Description=SiphonAI — SIP-to-WebSocket bridge
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=siphon-ai
Group=siphon-ai
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
sudo systemctl status siphon-ai --no-pager
```

If the service comes up RED, `journalctl -u siphon-ai -n 80 --no-pager`
shows the load-time error. The config validates at startup so a
typo never makes it past `systemctl start`.

---

## 7. Firewall

Two layers of trust:

1. **SiphonAI's `[[trunk]]` block** (above) is the primary
   identity gate. Anything not matching a trunk gets `403
   Forbidden` at the SIP layer, before media setup.
2. **Host firewall** is defense in depth — it stops abuse (port
   scans, UDP floods) from reaching SiphonAI in the first place.
   Don't rely on the firewall alone for *identity* — flat ACLs
   drift, and SiphonAI's log line for a 403 is more useful than
   `nftables drop`.

```bash
# Replace 10.0.0.10 with your trunk peer's address.
sudo nft add table inet siphon_ai
sudo nft add chain inet siphon_ai input '{ type filter hook input priority 0; }'
sudo nft add rule inet siphon_ai input udp dport 5060           ip saddr 10.0.0.10 accept
sudo nft add rule inet siphon_ai input udp dport 40000-40500    ip saddr 10.0.0.10 accept
sudo nft add rule inet siphon_ai input tcp dport 9091           ip saddr 10.0.0.0/24 accept
```

(`ufw` / `iptables` / cloud SG equivalents work too; the
permission set is what matters.)

> **RTP source IP caveat:** the SIP `dport 5060` rule above always
> comes from the trunk peer (FreeSWITCH, ITSP, etc.) and is safe
> to lock by source IP. The **RTP** `dport 40000-40500` rule
> assumes RTP arrives from the same peer — which is true for
> default FreeSWITCH bridges. If you use `bypass_media=true` in
> the FS dialplan (recommended — see
> `docs/FREESWITCH_INTEGRATION.md`), RTP flows **directly between
> the original caller and SiphonAI**, bypassing FS. In that case
> the RTP source IP is the caller's IP — which can be anywhere
> — and you'll want either an open RTP source ACL (rely on
> conntrack + rate-limiting for safety) or a wider CIDR matching
> your expected caller pool. SIP identity is still gated by the
> `[[trunk]]` allowlist, so this doesn't loosen auth.

### Fail2ban for repeat offenders

The `[[trunk]]` allowlist 403s every scanner INVITE at the SIP
layer, but scanners retry forever and fill the journal. For
internet-facing deployments, set up fail2ban to drop repeat
offenders at the kernel after N strikes — see
`docs/SECURITY_FAIL2BAN.md` for the filter, jail config, and
walkthrough. ~5 minutes; recommended for any public IP.

---

## 8. Verify

### Health endpoints

```bash
curl -s http://127.0.0.1:9091/health     # → "ok"
curl -s http://127.0.0.1:9091/ready      # → "ready"
curl -s http://127.0.0.1:9091/admin/calls
# → {"calls":[],"count":0}
```

### SIPp smoke

Send one INVITE and verify the daemon answers + tears down clean.
This exercises the SIP path without bringing up a WS server.

```bash
# Tell the daemon to use a no-op WS for the smoke test by pointing
# the route at a non-existent URL; the call will 200-OK and then
# tear down on WS connect failure. That's enough to prove the SIP
# stack is alive. Or run the real WS server and watch metrics.

sipp -sn uac 127.0.0.1:5060 -m 1 -s 9000
# → SIP/2.0 200 OK
```

### Live metrics

```bash
curl -s http://127.0.0.1:9091/metrics | grep -E '^siphon_ai_(invites_total|calls_total|calls_active)'
```

A real call should bump `siphon_ai_invites_total{result="accepted"}`
and `siphon_ai_calls_total{cause="…"}` after teardown.

---

## 9. Operations

```bash
# Tail logs
sudo journalctl -u siphon-ai -f

# Bump bridge debug for an incident
prev=$(curl -s http://127.0.0.1:9091/admin/log)
curl -X PUT --data 'siphon_ai=info,siphon_ai_bridge=debug' \
    http://127.0.0.1:9091/admin/log
# (… reproduce, then revert …)
curl -X PUT --data "$prev" http://127.0.0.1:9091/admin/log

# Force-hangup a specific call
curl -s http://127.0.0.1:9091/admin/calls
curl -X POST http://127.0.0.1:9091/admin/calls/<sip-call-id>/hangup

# Restart cleanly
sudo systemctl restart siphon-ai
```

See `docs/OPERATIONS.md` for the diagnostic checklist tied to the
DEV_PLAN §11.8 ten-questions audit, and `docs/CONFIG.md` for every
TOML field.

---

## 10. Where to next

- **Trunk to FreeSWITCH:** see `docs/FREESWITCH_INTEGRATION.md` for a
  worked example that routes extension `9000` through this daemon
  into a Node bot.
- **HEP / Homer:** `docs/HEP.md` (and the local stack under
  `examples/homer-stack/`).
- **Soak the install:** `test-harness/load/` has SIPp scenarios for
  the 1-hour stability + 500-concurrent burst gates.
