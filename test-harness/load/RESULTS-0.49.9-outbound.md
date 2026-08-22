# §12 outbound origination under load — 0.49.9

Run 2026-08-21 against the **shipped 0.49.9 `.deb`** (`/usr/bin/siphon-ai`
sha256 `ca7a6af19…`, extracted from `siphon-ai_0.49.9-1_amd64.deb`), with the
**shipped 0.49.8 deb** (`44d1d3e3c…`) as the control.

Scope: `LOAD_TEST_PLAN.md` **§12** — the direction nothing in this document
had ever loaded. Every phase in §§3–6 and every tier in §10 drives calls
*into* the daemon; the `originate` commands in §10.2 are FreeSWITCH
originating **at** us. Nothing exercised `POST /admin/v1/calls`.

**Result: pass on 0.49.9, and the phase fails the binary it was written
for.** Three concurrency steps and both teardown directions returned every
resource to idle, including `dialogs_active`. The same rig against 0.49.8
leaves that gauge pinned at **exactly the cumulative call count** — the
signature of issue #548, which survived every other phase in this plan
including the 8-hour convergence soak.

---

## Environment

| | |
|---|---|
| Box | 4 vCPU, 7947 MB, Debian 13 (trixie), Linux 6.12.95+deb13-amd64 |
| Daemon | `siphon-ai 0.49.9` (shipped deb) on `127.0.0.1:5070` |
| Control | `siphon-ai 0.49.8` (shipped deb), same config |
| Callee | SIPp v3.7.7 on `:5075` — `uas_hold.xml` / `uas_hold_remote_bye.xml` |
| WS sink | `paced_sink.mjs` on `:8770` (**not** `ws_sink.mjs` — §8) |
| `ulimit -n` | 524288 |
| Posture | PCMU, plaintext, `[cdr.file]` on, **no** HEP, webhooks, recording or VAD |
| Pacing | 5 cps ramp, 120 s hold (45 s for the remote-BYE and control runs) |

Single box: SIPp *answers* here rather than placing calls, which costs far
less than generating. §10.1 still stands for any ceiling claim — **this
phase is not one.** It measures what the outbound path costs and what it
fails to release, not where it knees.

---

## 1. Concurrency steps — 0.49.9, we hang up

`HOLD=120 CPS=5 ./phase-outbound.sh <deb-binary> 0.49.9-local 25 50 100`

| concurrent | CPU (% of one core) | CPU per call | RSS under load | fds | RTP ports | originates refused |
|---|---|---|---|---|---|---|
| idle | 0.0 | — | 16.0 MB | 13 | 0 | — |
| 25 | 8.4 | **0.336 %** | 26.0 MB | 88 | 50 | 0 |
| 50 | 14.5 | **0.290 %** | 33–36 MB | 163 | 100 | 0 |
| 100 | 27.1 | **0.271 %** | 49.9 MB | 313 | 200 | 0 |

All 175 calls ended `answered`; `outbound_calls_total{result}` carried no
other label. **Zero WARN or ERROR** in the daemon log for the whole run.

🔑 **fds are exactly `13 + 3N`** at every step (88 / 163 / 313). Tier 1 and
tier 2 both published `12 + 3N` for inbound; the extra descriptor here is
the CDR file, which those postures did not have open. The *slope* is
identical — an originated leg costs the same three descriptors as an
accepted one.

🔑 **Two RTP ports per originated leg**, exactly as for an accepted one. §1.1's
range is therefore a ceiling on the **sum** of both directions, not on
inbound alone — worth stating explicitly in any capacity figure, because a
node that both answers and originates halves its headroom twice over.

🔑 **Per-call CPU improves with concurrency** (0.336 → 0.290 → 0.271 %),
the same flat-to-improving shape tier 2 found for inbound. Fixed per-call
overhead amortises; nothing knees inside this range.

⚠️ **Do not compare these percentages to the inbound figures.** Tier 1's
0.53 %/call and tier 2's 0.650 %/call were measured at 200 concurrent, over a
NIC in tier 2's case, and with a different feature posture. The comparable
claim this run supports is narrower and still useful: *originating a call
costs the same order as answering one, and scales the same way.*

## 2. Both teardown directions

Who hangs up decides which branch of the outbound teardown runs, and #548
sat on one of them.

| run | who BYEs | calls | `dialogs_active` after drain |
|---|---|---|---|
| `0.49.9-local` (25/50/100) | **we** do, via `POST /admin/v1/calls/:id/hangup` | 175 | 0 at t=38 s / 33 s / 37 s |
| `0.49.9-remote` (50) | **SIPp** does | 50 | 0 at t=38 s |

Reclamation lands 33–38 s after teardown in every case: the reaper's 32 s
grace window (SIP Timer H/J, so a retransmitted BYE still matches) plus up
to one 5 s sweep. That is the designed behaviour, not latency to fix.

## 3. The leak audit — §6.3, with `dialogs_active` in it

After every step, with all calls ended:

```
calls_active        0     ✓
dialogs_active      0     ✓  (after the grace window — see below)
fds                 13    ✓  exactly the idle count
RTP ports bound     0     ✓
threads             5     ✓  never moved at any concurrency
outbound_calls_total{result} — answered only, no other label   ✓
```

RSS after drain rises with the **peak concurrency** the process has seen —
26.7 MB after the 25-step, 36.0 after 50, 50.5 after 100 — and does not come
back. That is §6.3's documented pool-sizing-plus-arena behaviour, not a
leak; the decisive re-load test is in `RESULTS-0.48.13.md` and nothing here
contradicts it.

## 4. The control — the same rig against 0.49.8

`HOLD=45 CPS=5 ./phase-outbound.sh <0.49.8-binary> 0.49.8-control 50 50`

Two consecutive 50-call steps on the unfixed binary:

| | 0.49.9 | 0.49.8 |
|---|---|---|
| calls_active after drain | 0 | 0 |
| fds after drain | 13 (idle) | 13 (idle) |
| RTP ports after drain | 0 | 0 |
| **`dialogs_active` after drain** | **0** | **100** |

The leak audit's poll never printed a reclamation line for either 0.49.8
step — it waited out the full 50 s twice and gave up. The final gauge is
**exactly the cumulative call count**, 50 + 50, which is what makes it a
per-call leak rather than a fixed overhead: every other resource returned to
idle, and only the shared dialog store did not.

🔑 **This is the assertion the plan was missing.** Past `sip-dialog`'s
`MAX_CONFIRMED_DIALOGS` (10,000) `insert` fails silently and in-dialog
requests stop matching — for inbound calls too, since the store is shared.
At the 100-call steps above, a node would reach that in a day of moderate
origination.

---

## Traps this run cost us

🪤 **A single `dialogs_active == 0` read passes against a leaking daemon.**
The gauge is republished only on the reaper's 5 s sweep, and `sip-uac`
inserts the dialog into the store when it *sends the BYE* — so for a few
seconds after teardown the gauge still holds its pre-BYE value, which is
`0`. The first cut of this check read once, immediately, and went green
against 0.49.8. The audit now waits for the gauge to publish **non-zero**
first, then waits for it to return to 0. §6.3 carries the same warning.

🪤 **`[last_To:]` / `[last_From:]` expand to the whole header line**, field
name included. Hand-writing the far end's BYE with them emits
`From: To: <...>`; SIPp then retransmits the malformed BYE until it aborts
the call, and 50 legs sat up until the run ended. It reads exactly like a
daemon that ignores BYE. `outbound_remote_bye.xml` already documented this
and captures the values with `ereg` instead — `uas_hold_remote_bye.xml` is
derived from it rather than written fresh. **A red run here is the rig
until proven otherwise.**

🪤 **`Failed to delete FD from epoll, errno = 1` in SIPp's error log is
benign** — it appears at teardown of a healthy run. Judge the run by the
abort lines, not by the file being non-empty.

🪤 **`[outbound]` is fail-closed.** No `max_concurrent`, no outbound, and
every originate returns `501` — which reads like a broken rig rather than a
config gap.

---

## What this does not cover

- **A ceiling.** Steps stop at 100 and the generator shares the box (§10.1).
- **Outbound over a real trunk.** §10.3 tier 3 is still unrun; these are
  loopback legs to SIPp, so nothing here says anything about a carrier SBC.
- **Mixed direction.** Every call in this run is outbound. A node doing both
  at once shares one RTP port range and one dialog store, and that
  interaction is untested.
- **Long-hold outbound.** Holds are 45–120 s. §6.2's hour-long soak has no
  outbound equivalent yet.
