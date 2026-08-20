# SIP-ladder ring memory — an on/off A/B

Run 2026-08-20 against the **shipped 0.49.7 tarball binary**
(`siphon-ai-0.49.7-x86_64-unknown-linux-musl`), on a lab second instance.

This closes the item `RESULTS-0.49.5-sip-ring.md` left open:

> **A ring-on/ring-off A/B.** The RSS delta above is not attributable; the
> ring's own cost is still unquantified.

It is now quantified. **~1.9 MB at a realistic 200-concurrent shape**, and
**~17 MB with the pending bound saturated at its per-call cap** — the
configuration an operator would have to work to reach.

## Why not a call-based run

The obvious A/B — same load through the tier-2 generator, ring on then off —
cannot answer this. The expected signal is 1–2 MB, and the 0.49.5 run's own
RSS varied by more than that between samples at fixed concurrency. Worse, at
203 concurrent that rig loses 19 % of its calls to the WS server (§1.4), so
the two arms would not even carry the same load.

So the ring is isolated instead: synthetic SIP dialogs from loopback, no
media, no WS server, no call setup. Both arms take the identical flood, so
transaction-layer memory — which is much larger than the ring's — is present
in both and cancels in the difference. What remains is the ring.

## Method

Fresh daemon per arm (never reused: a drained process's free pool absorbs the
next arm's allocations invisibly, the trap `RESULTS-0.48.10.md` fell into),
idle RSS recorded after a 3 s settle, flood, 8 s settle, loaded RSS. Arms
alternate `on, off, on, off …` so any drift lands on both. Configs are
byte-identical but for `[observability].sip_ring_size` — `50` vs `0`.

Two things had to be fixed before the numbers meant anything, both recorded
here because each silently produced a plausible wrong answer first:

- **`[sip].udp_rate_limit_pps` defaults to 200 per source**, so the first
  flood delivered **3,162 of 16,384 messages** and filled 60 of 256 traces.
  It measured a quarter of the ring and looked fine. Set to `0` in *both*
  arms. `docs/CONFIG.md` warns about exactly this for load generators.
- **INVITE was the wrong probe.** Unanswered INVITEs leave server
  transactions retransmitting their 403 for ~32 s; that storm's timing
  dominated, and the ring-on arm swung **61 → 125 MB** while ring-off held
  ±1.3 MB. `OPTIONS` is one request, one 200, no retransmission — after the
  switch, ring-on repeated to **±0.2 MB** across four runs.

## Results

Medians of the alternating pairs, RSS delta from idle, in kB.

| shape | messages | ring on | ring off | **ring cost** |
|---|---|---|---|---|
| 256 traces × 64 msg, small (~287 B avg) | 16,384 | 38,562 | 27,840 | **10.7 MB** |
| 256 traces × 64 msg, realistic (~591 B avg) | 16,384 | 44,116 | 26,900 | **16.8 MB** |
| 282 traces × 6 msg, realistic | 1,692 | 5,520 | 3,616 | **1.9 MB** |

Two payload sizes give the shape of the per-message cost:

```
  ~287 B avg payload →   670 B/message
  ~591 B avg payload → 1,076 B/message
  ⇒ roughly  1.34 × payload + ~285 B
```

The slope above 1.0 is allocator size-class rounding, not bookkeeping: an
882-byte string does not occupy 882 bytes. The ~285 B intercept is the entry
itself — timestamp, `src`/`dst`, direction, and the `String`/`VecDeque`/map
overhead around them.

The realistic row is the one to quote. **A prod-shaped node at ~200
concurrent pays about 1.9 MB for the ring** — against the 77 MB that node's
whole daemon occupied at that load, i.e. **~2.5 %**, and comfortably inside
the run-to-run RSS variance that made the 0.49.5 figure unattributable.

## The ceiling, computed not measured

The bounds permit `MAX_PENDING (256) + MAX_LIVE (512) + cap_calls (50)` =
818 traces, each up to `sip_ring_max_messages` (64) messages. At the measured
~1.05 kB/message that is **~55 MB**.

**This is arithmetic on a measured rate, not an observed number**, and it is
not reachable by accident: it needs 512 concurrent calls *each* having
exchanged 64+ SIP messages. Real calls in these runs carry 4–8. Treat it as
the bound the design guarantees, not a figure any node is likely to approach
— and note it scales linearly with `sip_ring_max_messages`, which is the knob
to reach for if it ever matters.

## Not measured

- **The live population's cost specifically.** All 282 traces in the
  realistic row are noise (the pending bound capped them at 256, visible in
  the run). Live traces cost the same per message — same struct, same
  storage — but a run with 512 genuinely live calls would confirm rather than
  assume it.
- **Whether freed ring memory returns to the OS.** Every arm here starts
  fresh; nothing tests `sip_ring_size = 0` on a *running* daemon that had a
  full ring.
- **Real message-size distribution.** `591 B` average is a padded synthetic.
  A node carrying large SDP or many headers will sit above the realistic row;
  the formula above is how to re-estimate it.

## Reproducing

`ringflood.py` and `abrun.sh` are alongside this file.

```bash
for i in 1 2 3; do
  PAD=600 ./abrun.sh on  282 3
  PAD=600 ./abrun.sh off 282 3
done
```
