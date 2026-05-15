# Fail2Ban policy for SiphonAI

An internet-facing SIP endpoint sees constant scan traffic —
botnets walking IP space looking for open trunks to abuse for
toll fraud. The `[[trunk]]` allowlist already 403s every one of
them at the SIP layer (`docs/CONFIG.md` §`[[trunk]]`), so they
can't *do* anything, but they keep retrying forever and fill
your journal. Fail2ban watches the journal for the 403 log line
and drops the offender at the firewall after N strikes.

This is defense in depth on top of `[[trunk]]`. The allowlist is
the trust gate; fail2ban is the noise filter.

---

## What's in this repo

```
contrib/fail2ban/
├── filter.d/siphon-ai.conf      # regex that matches our 403 log line
└── jail.d/siphon-ai.local       # ban policy (5 strikes / 10 min / 24 h)
```

Both files are short and operator-tunable. Read them — they
explain the trade-offs inline.

---

## Install

Assumes you've followed `docs/INSTALL_DEBIAN13.md` and have
SiphonAI logging to journald via systemd.

```bash
sudo apt install -y fail2ban
sudo cp /opt/siphon-ai-src/contrib/fail2ban/filter.d/siphon-ai.conf  /etc/fail2ban/filter.d/
sudo cp /opt/siphon-ai-src/contrib/fail2ban/jail.d/siphon-ai.local   /etc/fail2ban/jail.d/
sudo systemctl enable --now fail2ban
sudo systemctl status fail2ban --no-pager
```

Replace `/opt/siphon-ai-src` with your clone path.

The default jail bans after **5 failed INVITEs from the same IP
in 10 minutes** for **24 hours**. Tune `maxretry`/`findtime`/`bantime`
in `siphon-ai.local` to taste. For a busy internet endpoint hit
by carrier-grade scanner farms you might drop to `maxretry = 3`.

---

## Verify

After a few minutes (or after a scanner has tried 5+ times),
list the active bans:

```bash
sudo fail2ban-client status siphon-ai
# Status for the jail: siphon-ai
# |- Filter
# |  |- Currently failed: 7
# |  |- Total failed:     142
# |  `- Journal matches:  _SYSTEMD_UNIT=siphon-ai.service
# `- Actions
#    |- Currently banned: 4
#    |- Total banned:     17
#    `- Banned IP list:   31.70.75.115 31.70.66.9 5.196.63.60 87.98.242.75
```

The banned IPs are dropped at the kernel level — no more
INVITEs from them reach SiphonAI, so the journal stops growing
from those sources.

Confirm with `sudo nft list ruleset | grep -A5 f2b-siphon-ai`:
the chain is populated with `ip saddr <BANNED> drop` entries.

---

## Unban / inspect

```bash
# Pull one IP out of jail
sudo fail2ban-client set siphon-ai unbanip 1.2.3.4

# See exactly which log lines triggered the most recent bans
sudo fail2ban-client status siphon-ai
sudo journalctl -u fail2ban -n 50 --no-pager

# Re-test the filter regex against a sample log line
sudo fail2ban-regex \
    'INVITE rejected: no trunk matched (403 Forbidden) peer=1.2.3.4:5060' \
    /etc/fail2ban/filter.d/siphon-ai.conf
```

---

## Escalation: permanent bans for repeat offenders

The default 24 h ban resets after the offender's been quiet for
the ban duration. For abuse-heavy networks you probably want
repeat offenders to get progressively longer bans, eventually
permanent. Enable in `/etc/fail2ban/fail2ban.conf`:

```ini
[DEFAULT]
bantime.increment  = true
bantime.factor     = 24
bantime.maxtime    = 31536000   # 1 year
```

`bantime.factor = 24` with the jail's `bantime = 86400` means
the second offense lands at 24 days, the third at ~576 days
(capped to `maxtime`). A botnet IP that comes back tomorrow gets
shut out for a month.

---

## What this DOESN'T do

- **Doesn't replace the `[[trunk]]` allowlist.** Without trunks,
  every INVITE is accepted at the SIP layer; fail2ban would have
  nothing to ban on (since the log line it keys off only fires
  on the 403 path). Trunks + fail2ban is the combo; fail2ban
  alone isn't enough.
- **Doesn't help with credentialed-trunk abuse.** If a real
  carrier credential leaks and an attacker reaches you from an
  allowlisted IP, the 403 never fires and fail2ban stays quiet.
  Rotate creds, prefer per-trunk IPs to wide CIDRs.
- **Doesn't help with on-path attackers.** Anyone who can
  arrange traffic to appear to come from a trusted source IP
  bypasses the allowlist and the ban filter. TLS transport
  (`transports = ["tls"]` + pinned certs) is the answer there;
  fail2ban is for the abusive-but-not-on-path noise.
- **Doesn't help with UDP floods.** A DDoS at line rate
  saturates the kernel UDP path before SiphonAI sees the
  packets; fail2ban only reacts to logged events. For DoS
  resilience use a connection-tracked rate-limit at the
  firewall:

  ```nft
  # Rate-limit per source IP to 50 packets/sec on the SIP port.
  # Adjust by deployment.
  add rule inet siphon_ai input udp dport 5060 \
      meter sip-rate { ip saddr limit rate 50/second } accept
  ```

---

## What's documented separately

- `docs/CONFIG.md` §`[[trunk]]` — the primary trust gate.
- `docs/INSTALL_DEBIAN13.md` §7 — host-firewall layer.
- `docs/OPERATIONS.md` — the §11.8 ten-questions audit for
  diagnostic visibility.

The recommended production stance: `[[trunk]]` allowlist (you
already have this), fail2ban (this doc), host firewall (install
guide §7), and TLS where the peer supports it. Each layer
covers a different threat.
