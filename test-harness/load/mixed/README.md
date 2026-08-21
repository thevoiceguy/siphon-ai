# Mixed-direction rig

`../LOAD_TEST_PLAN.md` §13. Reproduces `../RESULTS-0.49.9-mixed-and-soak.md`.

§§3–6 load inbound. §12 loads outbound. Neither says anything about what
they do to each other — and they share **one RTP port pool, one dialog
store, one fd table**.

```
SIPp (uac_hold.xml) ──INVITE──► siphon-ai ──INVITE──► SIPp (../outbound/uas_hold.xml)
        inbound legs                 │                        outbound legs
                                     └──WS──► ../paced_sink.mjs
```

Reuses `../outbound/`'s ramp, sampler and UAS scenario; only the inbound
caller and the configs are new.

## Running it

```sh
cp mixed-lab.toml.example mixed-lab.toml
cp mixed-exhaust.toml.example mixed-exhaust.toml
export SP=/var/tmp/mixed-load

# steady state: 50 in + 50 out on a pool big enough for both
HOLD=120 CPS=5 ./phase-mixed.sh /usr/bin/siphon-ai run-label 50 50

# exhaustion, BOTH orders — they are not symmetric
EXHAUST=1 HOLD=60 ./phase-mixed.sh /usr/bin/siphon-ai exhaust-in 50 20
ORDER=outbound-first EXHAUST=1 HOLD=60 ./phase-mixed.sh /usr/bin/siphon-ai exhaust-out 20 50
```

`ORDER` decides which direction gets the pool first. **Run both.** When
inbound holds it, outbound fails through the originate API's webhook and
metric; when outbound holds it, inbound must be refused on the wire, and
that path answers a different code (see #554). A run in one order tests
half the behaviour.

## Reading the output

Four JSON samples per run: `idle`, the direction that went first
(`inbound-only` / `outbound-only`), `mixed`, and `drained`.

Expect `fds = 13 + 3N` and `udp_sockets = 2N + 1` with **N the total across
both directions** — that shared-pool arithmetic is the whole point. The
`drained` line must show fds back to the idle count, `udp_sockets` back to
1, and `dialogs_active` 0.

The phase also prints what the inbound caller was told, parsed out of
SIPp's error file, which is the only place an on-the-wire refusal shows up.

## Traps

- **The exhaust config is deliberately too small** (120 ports = 60 calls).
  A run against it that reports failures is working; a run that reports
  none did not actually exhaust anything — check the arithmetic against
  your step sizes.
- **`inactivity_timeout_secs = 0`.** Inbound legs here stream no RTP, so
  the watchdog would reap them mid-hold (§1.3).
- **A trunk must cover loopback** or every inbound INVITE is 403'd before
  it can allocate a thing, and the run looks like a routing failure rather
  than a pool test.
- `../outbound/`'s traps all still apply — fail-closed `[outbound]`, the
  paced sink rather than `--auto-hangup-after-ms`, and the two-step
  `dialogs_active` read.
