# Tier 2 rig — FreeSWITCH generator on a separate box

Everything needed to reproduce `../RESULTS-tier2.md` (`LOAD_TEST_PLAN.md` §10.2).
Two hosts: **Box A** runs the daemon under test, **Box B** runs FreeSWITCH as the
generator. §10.1 is the whole point — the generator must not live on the box
under test, and must be proven not to be the constraint.

```
Box B  FreeSWITCH ──SIP + RTP over the NIC──► Box A  siphon-ai  ──WS──► paced_sink.mjs
```

## Files

| | where it runs | what it is |
|---|---|---|
| `ramp.sh` | Box B | originate N calls at C cps, plaintext |
| `ramp-tls.sh` | Box B | same over `;transport=tls` with SRTP mandatory |
| `phase1.sh` | Box A | 50/100/200 ramp, 5 min per step, plaintext |
| `phase2.sh` | Box A | same ramp, TLS + SRTP |
| `phase3.sh` | Box A | clean vs `netem`-impaired quality runs |
| `tier2-lab.toml.example` | Box A | daemon config, plaintext |
| `tier2-tls.toml.example` | Box A | daemon config, TLS listener + `srtp = "required"` |

The phase drivers require `SP` — a working directory for logs, CDRs and metric
snapshots:

```sh
export SP=/var/tmp/tier2 && mkdir -p $SP
```

Replace `/PATH/TO/WORKDIR` in the `.toml.example` files with the same directory,
and put the daemon's pid in `$SP/lab.pid`. The drivers assume metrics on
`127.0.0.1:9191` and the WS sink on `127.0.0.1:8767`.

## Setup that is easy to get wrong

**The sink.** Use `../paced_sink.mjs`, not `ws_sink.mjs` — the latter paces with
`setInterval(20)`, which under-runs realtime and lands directly on any tx-rate or
clock-drift measurement (§8). Check its reported `tick_hz` is 50.00 before
trusting a run. It needs the `ws` module; point `SINK_REQUIRE_BASE` at a
`node_modules` that has it.

**`[node].public_address` is required** once `[sip].listen` binds `0.0.0.0` — the
SDP answer's `c=` line cannot be `0.0.0.0`, and the validator refuses to start
without it.

**Size the RTP range for the target**, per §1.1: `2 × concurrency` plus teardown
headroom. The examples use `[41000, 45000]` for up to 1000 calls.

**Restrict the listener.** Both boxes here have public IPs and `:5060` already
sees scanner INVITEs, so the configs carry a `[[trunk]]` allowlist naming Box B.

### TLS + SRTP (phase 2)

**On Box A** — `srtp = "required"`, **not** `"preferred"`. Stock FreeSWITCH
rejects the preferred-mode `RTP/AVP` + `a=crypto` offer with a `488`
(`docs/CONFIG.md`, `docs/FREESWITCH_INTEGRATION.md`). If the production
`privkey.pem` is not readable by the account running the test, generate a lab
cert — FreeSWITCH's default `tls-verify-policy` is `none`, so self-signed is
accepted:

```sh
openssl ecparam -genkey -name prime256v1 -out lab-key.pem
openssl req -new -x509 -key lab-key.pem -out lab-cert.pem -days 30 \
  -subj "/CN=sip-lab.example" -addext "subjectAltName=IP:<BOX_A_IP>"
```

**On Box B** — the stock external profile ships `external_ssl_enable=false` and
binds udp/tcp only. Set it true in `vars.xml`, and create `agent.pem` (only
`dtls-srtp.pem` and `wss.pem` ship), or the profile will not offer TLS:

```sh
openssl req -x509 -newkey rsa:2048 -nodes -days 30 -subj "/CN=fs-lab.local" \
  -keyout k.pem -out c.pem && cat k.pem c.pem > /etc/freeswitch/tls/agent.pem
cp c.pem /etc/freeswitch/tls/cafile.pem
chown freeswitch:freeswitch /etc/freeswitch/tls/{agent,cafile}.pem
fs_cli -x "reloadxml" && fs_cli -x "sofia profile external restart"
fs_cli -x "sofia status profile external" | grep TLS-BIND-URL   # expect :5081
```

Confirm SRTP actually covered every call rather than assuming it:
`forge_srtp_packets_decrypted_total` should equal
`forge_rtp_packets_received_total`.

### FreeSWITCH limits

The stock `max-sessions 1000` / `sessions-per-second 30` already cover 200
concurrent at 10 cps, so §10.2's raise to 3000/200 is only needed past ~500 —
or past ~400 with crypto, where FreeSWITCH's own CPU starts to threaten §10.1.

## Calls end themselves

`ramp.sh` sets `execute_on_answer='sched_hangup +<hold> NORMAL_CLEARING'` on
every originate, so a run terminates on its own schedule instead of depending on
a closing `hupall`. If the driving session dies, the calls still end. Keep this
property in anything derived from these scripts — it is what makes an unattended
ramp safe.

## Measuring

Use `/proc/<pid>/stat` fields 14+15 over a fixed window. **`ps -o %cpu` is a
lifetime average**, not an instantaneous rate: it makes a flat run look like it
is ramping, and is meaningless for a long-lived FreeSWITCH process. `bc` may not
be installed — the drivers do their arithmetic in `awk`.

Sample the generator's CPU in every row too. If Box B is above ~70 % while the
daemon is below it, **the run is void** (§10.1).

## netem

`phase3.sh` applies impairment to **Box B's egress only** — the
caller→SiphonAI direction, which is what `siphon_ai_rtp_rx_jitter_ms` measures.
The return path stays clean; say so when publishing.

It also arms a detached auto-removal:

```sh
setsid nohup bash -c 'sleep 1800; tc qdisc del dev eth0 root' >/dev/null 2>&1 &
```

Keep that. `netem` impairs your own ssh to the box, so a dropped session must
not strand the qdisc.
