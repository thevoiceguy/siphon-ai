# Load results — 0.48.19 (RTP port bind retry, #504 / forge-media#112)

Run 2026-08-15 01:47 UTC. This is **not** a `LOAD_TEST_PLAN.md` phase — it is
a targeted A/B regression test for the one change in 0.48.19, using a
deliberately hostile port pool. The §4 ramp, §5 arrival-rate ceiling and
§6 soak were **not** re-run: 0.48.19 changes port allocation on the setup
path and nothing in media or steady state, so the 0.48.18 figures stand.

## What it tests

The 0.48.18 soak lost one call in 399 to a `500` when the kernel handed an
RTP port to some other socket before the daemon bound it (§1.1, issue #504).
0.48.19 bumps forge-media to `1d7bbaba0c22`, which retries the next pair on
`AddrInUse` instead of failing the session, drawing up to **five** pairs.

Waiting for a 1-in-399 event to reappear proves nothing in a 20-call run, so
`squat.py` manufactures the collision: it binds the **even (RTP) port of
every other pair** in the range, holding **25 of 50 pairs**, the way an
ephemeral socket does. That makes ~50 % of the pool's draws collide instead
of ~0.25 %.

## Environment

| | |
|---|---|
| Hardware | 4 vCPU, 7.9 GB RAM |
| OS / kernel | Debian 13, Linux 6.12.95+deb13-amd64 |
| Versions | `siphon-ai 0.48.18` sha256 `ecbc5e128…` vs `0.48.19` sha256 `6b7340df4…` |
| Provenance | both extracted from their shipped `.deb` with `dpkg-deb -x` (no sudo); the 0.48.19 binary is **byte-identical to production's `/usr/bin/siphon-ai`** |
| Posture | G.711 µ-law, recording off, HEP off, no webhooks, no audit |
| `rtp_port_range` | `[41000, 41100]` — 50 pairs, deliberately **outside** the reserved 40000–40500, so the squatter can work |
| Squatter | `squat.py 41000 41100 --fraction 0.5` → *holding 25 of 50 RTP ports (50 %)* |
| Generator | SIPp, `basic_call_then_bye.xml`, `-m 20 -r 2 -l 4`, same box |
| WS server | `examples/echo-ws-server-python` on `127.0.0.1:8090` |

Second unprivileged instance on `:5070` / metrics `:9191`, alongside an
untouched production daemon on `:5060`. Both versions ran back-to-back
against the same squatter, the same generator invocation and the same
config file.

## Headline

| | 0.48.18 | 0.48.19 |
|---|---|---|
| Successful calls | 13 / 20 | **19 / 20** |
| Failed calls | 7 | **1** |
| `rejecting INVITE … Address in use` | **7** | **1** |
| `RTP port is held outside the pool; drawing another pair` | 0 (no such path) | **26** |
| Distinct `accept_inbound` spans | **13** | **20** |

The span count is the structural half of the result, and it is worth more
than the pass rate. On 0.48.18, thirteen calls entered `accept_inbound` —
exactly the thirteen that succeeded; the other seven died before session
setup got that far. On 0.48.19 **all twenty** entered it, and nineteen came
out the other side. The retry is happening inside session setup, not in the
generator and not by luck.

The failures land on squatted ports and nowhere else. 0.48.18's seven bind
failures hit 41044, 41080 ×2, 41084 ×2, 41092, 41096 — all even, all in the
held set.

The rejection's *shape* changed too, which is the visible half of upstream
splitting `ForgeError::AddrInUse(SocketAddr)` out of `Network(String)`:

| 0.48.18 | `Network error: Failed to bind socket to 0.0.0.0:41096: Address in use (os error 98)` |
|---|---|
| **0.48.19** | `Address already in use: 0.0.0.0:41044` |

The retry keys off that type rather than message text, and the surviving
rejection still names the port — which is what makes a squatter identifiable
from the log at all.

## The one remaining failure is the retry cap, not a defect

```
WARN … forge_engine::session: RTP port is held outside the pool;
  drawing another pair addr=0.0.0.0:41096 attempt=4 max_attempts=5
WARN … acceptor: rejecting INVITE error=forge session error:
  Address already in use: 0.0.0.0:41044 code=500
```

Retries needed, per call:

| Retries | 1 | 2 | 3 | 4 | 5 (exhausted) |
|---|---|---|---|---|---|
| Calls | 5 | 1 | 2 | 2 | **1** |

Eleven of twenty calls hit at least one squatted draw — against ten expected
at a 50 % squat rate — and 26 collisions across 46 total draws is 57 %, so
the squatter did what it claims. Ten of those eleven recovered.

One call burned all five draws. At p = 0.5 per draw that is 0.5⁵ = **3.1 %
per call**, i.e. 0.6 expected failures in 20, and a **47 % chance of seeing
at least one** in a run this size. Observing exactly one is the cap behaving
as designed against an absurd collision rate, not a residual bug.

At the rate actually measured in the field — 1 in 399 draws, p ≈ 0.0025 —
exhausting five consecutive draws is p⁵ ≈ **10⁻¹³ per call**. The cap is
unreachable in practice and does not warrant an upstream issue. It is
hardcoded at five upstream; there is no knob for it in `[media]`, and this
result gives no reason to want one.

## Verdict

The fix works, in the only way that matters here: a call that would have
died on the first lost bind now steps past it. Failures fell 7 → 1 and the
`Address in use` rejections that motivated #504 fell to a single case that
is explained by the retry limit and quantified above.

**The `net.ipv4.ip_local_reserved_ports` guidance in §1.1 and `docs/DEPLOY.md`
is still worth applying**, and the CHANGELOG says so too. This removes the
*requirement* to reserve the range, not the value: reserving it means the
retry never fires, and a `warn!` that does fire is telling you something real
about the host.

## Not measured

- **No soak, no ramp, no arrival-rate run.** One 20-call sequence at 2 cps on
  loopback with PCMU. 0.48.13/0.48.18 figures stand for §4, §5 and §6.
- **The retry's cost under load is unmeasured.** Each retry is a bind attempt
  on the setup path; at 50 % squat it fired 26 times across 20 calls with no
  observable effect at this rate, but nothing here bounds its cost at 200
  concurrent calls. Setup latency was not instrumented in this run.
- **Only proven at a 50 % collision rate.** The natural rate is ~0.25 %; the
  behaviour in between is interpolated from the per-draw model above, not
  measured.
- **The squatter is not the field mechanism.** It holds ports for the whole
  run; a real ephemeral squatter holds one transiently. That makes this
  strictly harder than reality for the allocator, which is the point, but it
  does not reproduce a port freed mid-retry.

## Timeline note

The run completed at 01:47:42 UTC and 0.48.19 was deployed to production at
01:49:55 UTC, on these numbers. The harness shell never printed its summary
line — it stayed open on a backgrounded WS server — so the figures were read
off the raw logs then, and off the same logs again on 2026-08-17 when this was
written up. Nothing was re-run; every figure above comes from the original
01:47 artefacts, and the two readings agree.

**Production cannot demonstrate this fix.** `ip_local_reserved_ports` on the
production node covers its whole `rtp_port_range`, so the retry never fires
there and a quiet journal proves nothing about it. The squatted lab A/B is the
only honest evidence, which is why it ran against the shipped artefacts rather
than a dev build.
