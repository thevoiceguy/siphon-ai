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
├── jail.d/siphon-ai.local       # primary jail (5 strikes / 10 min / 24 h)
└── jail.d/recidive.local        # second-tier jail for repeat offenders
```

All three files are short and operator-tunable. Read them —
they explain the trade-offs inline.

---

## Install

Assumes you've followed `docs/INSTALL_DEBIAN13.md` and have
SiphonAI logging to journald via systemd.

### Scripted (recommended)

```bash
/opt/siphon-ai-src/scripts/install-fail2ban.sh
```

The script handles every step on this page — installs the package,
drops the filter + both jails, optionally enables `bantime.increment`
escalation (see §"Recidive: long bans for repeat offenders"),
validates the config with `fail2ban-client -t`, regex-tests the
filter against a canonical log line, and reports active-jail
status. Idempotent: re-running backs up existing configs to
`*.bak.<timestamp>` before overwriting.

Override `CONTRIB_DIR=` if your clone isn't at `/opt/siphon-ai-src`.
Set `BANTIME_INCREMENT=0` to skip the escalation drop-in.

### Manual (if you'd rather see every step)

```bash
sudo apt install -y fail2ban
sudo cp /opt/siphon-ai-src/contrib/fail2ban/filter.d/siphon-ai.conf  /etc/fail2ban/filter.d/
sudo cp /opt/siphon-ai-src/contrib/fail2ban/jail.d/siphon-ai.local   /etc/fail2ban/jail.d/
sudo cp /opt/siphon-ai-src/contrib/fail2ban/jail.d/recidive.local    /etc/fail2ban/jail.d/
sudo systemctl enable --now fail2ban
sudo systemctl status fail2ban --no-pager
```

Replace `/opt/siphon-ai-src` with your clone path.

The default jail bans after **5 failed INVITEs from the same IP
in 10 minutes** for **24 hours**. The recidive jail re-bans IPs
that have already been banned 3+ times in a week for **a week
each subsequent hit** — see §"Recidive: long bans for repeat
offenders" below. Tune `maxretry`/`findtime`/`bantime` in each
jail file to taste. For a busy internet endpoint hit by
carrier-grade scanner farms you might drop the primary jail to
`maxretry = 3`.

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

## Recidive: long bans for repeat offenders

The primary jail's 24 h ban resets after the offender has been
quiet for the ban duration. A bot that comes back the next day
starts fresh and only gets dropped after another 5 strikes.
That's noise you don't need.

`recidive.local` (shipped in this PR) is a second-tier jail
that watches fail2ban's OWN log for ban events. When an IP gets
banned by the `siphon-ai` jail 3 or more times in a week,
recidive picks it up and bans it for a *week*. Combined with
`bantime.increment` (below), the second recidive hit lands at
~24 weeks and the third at the year cap. A persistent botnet IP
is effectively gone after two appearances.

The recidive filter is bundled with the fail2ban package
(`/etc/fail2ban/filter.d/recidive.conf`); we only ship the jail
snippet.

### Stack the two

To get progressively longer recidive bans, enable
`bantime.increment` in `/etc/fail2ban/fail2ban.conf`:

```ini
[DEFAULT]
bantime.increment  = true
bantime.factor     = 24
bantime.maxtime    = 31536000   # 1 year
```

With `bantime.increment = true` applied to recidive's
`bantime = 604800` (1 week):

| Offense in recidive | Ban duration                 |
|---------------------|------------------------------|
| 1st                 | 1 week                       |
| 2nd                 | ~24 weeks                    |
| 3rd+                | 1 year (`maxtime` cap)       |

The same setting also escalates the primary `siphon-ai` jail's
24 h ban, but recidive matters more — it's the long-tail
filter.

### Inspect recidive

```bash
sudo fail2ban-client status recidive
# Status for the jail: recidive
# |- Filter
# |  |- Currently failed: 0
# |  |- Total failed:     12
# |  `- Journal matches:  _SYSTEMD_UNIT=fail2ban.service
# `- Actions
#    |- Currently banned: 2
#    |- Total banned:     4
#    `- Banned IP list:   31.70.66.9 31.70.75.115
```

Two banned IPs in the example output have shown up three or
more times across the past week, so they're locked out for a
week (or longer if `bantime.increment` is on).

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
