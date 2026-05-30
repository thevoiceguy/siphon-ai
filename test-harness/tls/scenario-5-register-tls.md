# Scenario 5 — REGISTER over `sip:host;transport=tls`

Validates the outbound TLS UAC path: `[[register]] transport =
"tls"` actually does TLS (no silent UDP fall-back), the
registration completes with the registrar's authentication
challenge, and the daemon-wide webpki trust store accepts a
real-world cert chain. The §10-item-5 acceptance line.

## Pick one of these registrar setups

### Option A — Asterisk with TLS (recommended; lightest)

```bash
# Asterisk in a container with TLS-only PJSIP and digest auth
docker run --rm --name ast-tls \
  -p 5061:5061/tcp \
  -v $PWD/asterisk-tls/:/etc/asterisk \
  asterisk:21
```

Minimal `pjsip.conf`:

```ini
[transport-tls]
type = transport
protocol = tls
bind = 0.0.0.0:5061
cert_file = /etc/asterisk/keys/asterisk.crt
priv_key_file = /etc/asterisk/keys/asterisk.key
method = tlsv1_2

[siphon-ai-auth]
type = auth
auth_type = userpass
username = siphon-ai
password = test123

[siphon-ai-aor]
type = aor
max_contacts = 1

[siphon-ai]
type = endpoint
transport = transport-tls
context = from-siphon-ai
disallow = all
allow = ulaw,alaw
auth = siphon-ai-auth
aors = siphon-ai
```

Self-signed cert in `keys/` is fine for this test — siphon-ai
honours `[[register]].tls.insecure_skip_verify = true` only
if explicitly configured. For a clean PASS, use a cert that
chains to a public CA (Let's Encrypt) or a private root
that's been imported into the daemon's trust store.

### Option B — Kamailio with TLS

If you already have a Kamailio instance with TLS, point
SiphonAI at it. The `kamailio.cfg` `listen` directive should
include `tls:0.0.0.0:5061` with `enable_tls=yes`.

### Option C — Real TLS-only PBX you already have

If you have a 3CX / FreeSWITCH / Switchvox already exposed
over TLS, register against that. Easiest if so.

## Procedure

1. **Add a `[[register]]` block** to your daemon config:

   ```toml
   [[register]]
   name             = "tls-test"
   server           = "192.0.2.1"       # the registrar's IP
   port             = 5061              # or wherever it listens
   transport        = "tls"
   username         = "siphon-ai"
   auth_username    = "siphon-ai"
   password         = "test123"
   realm            = "asterisk"        # or whatever the registrar challenges with
   expires_secs     = 600
   register_on_startup = true
   ```

   Hostname `server =` values are **not** supported in 0.3.0
   (carry-forward to 0.3.1) — use the IP address directly.

2. **Restart the daemon** (or `systemctl reload` won't pick up a
   new `[[register]]` block — registrations are start-up state).

   ```bash
   sudo systemctl restart siphon-ai
   ```

3. **Watch the daemon log** for the registration lifecycle:

   ```bash
   sudo journalctl -u siphon-ai -f \
     | grep -iE 'register|tls|auth|challenge'
   ```

4. **Expect to see**:

   ```
   INFO ... REGISTER attempt name=tls-test transport=tls server=192.0.2.1:5061
   INFO ... TLS handshake complete server=192.0.2.1:5061
   INFO ... REGISTER 401 challenged, retrying with auth
   INFO ... REGISTER 200 OK expires=600
   INFO ... refresh scheduled in 540s
   ```

   Critical line is the `transport=tls` AND the `TLS handshake
   complete`. If the daemon falls back to UDP silently, that's
   the bug 0.3.0 closes — log a fail.

5. **Verify the registration counters**:

   ```bash
   curl -s http://127.0.0.1:9091/metrics \
     | grep -E 'siphon_ai_register_(attempts|state)_total'
   ```

   Expected:
   - `siphon_ai_register_attempts_total{name="tls-test",outcome="ok"} 1` (or higher)
   - `siphon_ai_register_state{name="tls-test"} 1`

6. **Confirm on the registrar side** that the registration landed:

   - Asterisk: `asterisk -rx 'pjsip show contacts'` — should show
     a contact ending in `;transport=tls`
   - Kamailio: `kamctl ul show` — same shape

## PASS criteria

- Daemon log shows `transport=tls` AND `TLS handshake complete`
- Daemon log shows `REGISTER 200 OK` after auth challenge
- `siphon_ai_register_state{name="tls-test"}` = 1
- Registrar's contact list shows our binding with `transport=tls`

## Common failure modes

| Symptom | Likely cause |
|---|---|
| Log shows `transport=udp` instead of tls | The bug 0.3.0 was supposed to fix. File a bug — this is the regression we're guarding against. |
| `TLS handshake failed: certificate verify failed` | Registrar's cert isn't trusted by webpki / Mozilla CA bundle. Either get a Let's Encrypt cert on the registrar, or import the registrar's root into siphon-ai's trust store (post-v1 feature; not in 0.3.0) |
| `Address resolution failed` | You used a hostname in `server =`; hostnames not supported in 0.3.0 (0.3.1 carry-forward). Use an IP. |
| `407 Proxy Authentication Required` not followed by retry | Auth handling regression — siphon-rs PR territory |
| 200 OK but `Contact:` in subsequent INVITEs is wrong | Auto-fill `[node].public_address` isn't set; daemon advertises its bind address. Set `[node].public_address` |

## Bonus check — observe the actual TLS

Confirm the registrar handshake from the host while it's running:

```bash
openssl s_client -connect <registrar-ip>:5061 -servername <registrar-fqdn> -showcerts < /dev/null \
  | head -30
```

If openssl can't TLS-handshake the registrar but siphon-ai
reports "TLS handshake complete," siphon-ai's TLS stack is
being more permissive than openssl — worth a closer look.
