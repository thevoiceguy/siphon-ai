# Outbound origination load rig

Reproduces `../RESULTS-0.49.9-outbound.md` (`../LOAD_TEST_PLAN.md` §12).

Every other rig in this tree drives calls **into** the daemon. This one
drives them **out**, through `POST /admin/v1/calls`, which is the direction
nothing had ever loaded — and the direction issue #548 leaked a dialog in,
undetected, through every phase in the plan including the 8-hour soak.

```
ob_ramp.sh ──POST /admin/v1/calls──► siphon-ai ──INVITE──► SIPp (uas_hold.xml)
                                         │
                                         └──WS──► ../paced_sink.mjs
```

Single box: unlike §10.2, the "generator" here is SIPp answering calls, which
costs far less than placing them. §10.1 still applies to anything you intend
to publish as a ceiling — this phase is about *what the outbound path leaks
and costs*, not about finding the knee.

## Files

| | what it is |
|---|---|
| `phase-outbound.sh` | driver: steps, sampling, drain, leak audit |
| `ob_ramp.sh` | originate N at C cps, hold, tear down, wait for drain |
| `obstat.sh` | one-shot JSON sample — CPU as a rate, RSS, fds, gauges |
| `uas_hold.xml` | SIPp answers, holds until **we** BYE |
| `uas_hold_remote_bye.xml` | SIPp answers, holds, then **it** BYEs |
| `outbound-lab.toml.example` | daemon config, ports clear of a prod install |

## Running it

```sh
cp outbound-lab.toml.example outbound-lab.toml
export SP=/var/tmp/outbound-load && mkdir -p $SP

# both teardown directions — a fix that retires the dialog in only one
# branch passes half of this
HOLD=120 CPS=5 ./phase-outbound.sh /usr/bin/siphon-ai 0.49.9-local  25 50 100
TEARDOWN=remote HOLD=120 CPS=5 ./phase-outbound.sh /usr/bin/siphon-ai 0.49.9-remote 50
```

Needs `sipp`, `node` (for `../paced_sink.mjs`), and a daemon binary. Every
knob has a default: `SP`, `CONFIG`, `HOLD`, `CPS`, `OBS_PORT`, `TEARDOWN`,
and `ADMIN`/`TOKEN`/`GATEWAY`/`TO` for `ob_ramp.sh` alone.

## Reading the output

One JSON line per sample, four per step: `idle`, `early` (ramp landed),
`late` (mid-hold), `drained` (post-teardown, past the grace window).

The line that matters is `drained`. **`dialogs_active` must be 0 there** —
and the driver polls for it rather than reading once, because the gauge is
republished only on the reaper's 5 s sweep and the UAC inserts the dialog
when it sends the BYE. A single read taken straight after teardown returns
the *pre-BYE* value, which is `0`, which passes against a daemon that
reclaims nothing. That is not hypothetical: it is how the first cut of this
check went green against the unfixed binary.

## Traps

- `[outbound]` is **fail-closed** — no `max_concurrent`, no outbound, and
  every originate is a `501` that reads like a broken rig.
- `inactivity_timeout_secs = 0`, or the watchdog reaps legs mid-hold: they
  are held open by the driver, not by media (§1.3).
- Use `../paced_sink.mjs`, **not** the echo server's
  `--auto-hangup-after-ms` — that ends the call from the WS side after a
  beat, which is right for a conformance scenario and fatal for a hold.
- An originated leg holds two RTP ports like any other, so §1.1's range is a
  ceiling on the *sum* of both directions.
- `ob_ramp.sh` counts refused originates and prints them. A step that ran at
  half its target because of `max_concurrent`/`rate_limit_per_sec` must not
  be read as a step at its target.
