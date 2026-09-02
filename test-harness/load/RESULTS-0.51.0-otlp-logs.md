# OTLP log export under load — 200 concurrent with the collector killed

Run 2026-09-02 against **PR #591** (`feat/otlp-log-export`, `0d5f3f1`,
workspace `0.50.1`), built `--release` from the PR head. Closes the one
acceptance criterion issue #589 left unrun, and the one the PR's own
"Not done" section names:

> A stopped collector produces no call-path latency and no unbounded memory
> growth (soak it in `test-harness/load/` at ~200 concurrent with the
> collector killed)

**Result: pass.** 46 minutes at 200 concurrent with the collector killed
cost the call path nothing measurable — setup-latency p95 stayed at
**0.5 ms on all 185 samples**, the same value the export-off control arm
produced — and memory stayed bounded, proven by re-load rather than by
eyeballing a curve. fds and threads did not move. **19,348 calls, zero
rejections, zero SIPp-side errors.**

Two findings that are not defects but belong in the operator docs are in
§6: the drop counter is **not** the collector-down signal, and whether an
outage is visible at all depends on the log filter in use.

---

## Environment

| | |
|---|---|
| Box | 4 vCPU, 7947 MB, Debian 13, Linux 6.12.95+deb13-amd64 |
| Daemon | PR #591 `--release`, `127.0.0.1:5070`, metrics `:9591`, RTP `30000–30800` |
| Collector | `otel/opentelemetry-collector-contrib:0.159.0`, host network, OTLP/gRPC `:4317`, `debug` + `file` exporters |
| Generator | SIPp 3.7.7, `-l 200`, 60 s hold ⇒ ~3.3 calls/s churn |
| Sink | `paced_sink.mjs` on `:8769` (50.001 Hz measured) |
| Filter | `--log siphon_ai=info,siphon=info,forge=info`; export `level = "info"` |
| Posture | PCMA/PCMU, plaintext, no HEP / webhooks / recording / CDR / VAD |

The RTP range sits **below** the 32768 ephemeral floor
(`net.ipv4.ip_local_port_range = 32768 60999`) so #504 could not confound
the run, and clear of the live daemon on this box (5060/5061, `:9091`,
40000–41000). That daemon was idle throughout and untouched.

**One-way media.** The generator streams no RTP (no pcap in-tree, per
README §Prerequisites); the sink feeds the daemon, so the daemon transmits
50 fps per call and the encode/send path is exercised, but the receive path
is not. `inactivity_timeout_secs = 0` keeps those calls up. Quality SLOs
(MOS, jitter) are therefore not assertable here and are not claimed — this
run is about latency, memory, and descriptors.

---

## 1. The phases

One daemon spans B→E, so the collector dies **mid-soak** under live load
rather than between runs. A is a separate daemon with log export off.

| Phase | Wall | Export | Collector | Calls |
|---|---|---|---|---|
| **A** control | 12 min | **off** | up | 2,747 |
| **B** baseline | 15 min | on | up | 3,370 |
| **C** *the criterion* | **46 min** | on | **killed** | 9,231 |
| **D** recovery | 10 min | on | restarted | ~1,800 |
| **E** re-load | 11 min | on | up | ~2,200 |

| Phase | RSS | fds | threads | CPU¹ | setup p95 | `calls_active` | rejected | drops |
|---|---|---|---|---|---|---|---|---|
| A control | 57.1 → 62.4 MB | 609–612 | 6 | 56.5 % | **0.5 ms** | 199–200 | 0 | 0 |
| B collector up | 44.7 → 64.6 MB | 463–613 | 8 | 54.7 % | 0.5 ms² | 199–200 | 0 | 0 |
| **C collector killed** | **65.5 → 76.2 MB** | **608–611** | **8** | **53.3 %** | **0.5 ms** | 199–200 | **0** | **0** |
| D restarted | 76.2 → 76.5 MB | 610–613 | 8 | 53.0 % | 0.5 ms | 199–200 | 0 | 0 |
| E re-load | 76.5 → 76.9 MB | 13 → 613 | 8 | 47.7 % | 0.5 ms | 164–200 | 0 | 0 |

¹ % of **one** core. ² one 1.0 ms sample during the initial ramp.

---

## 2. No call-path latency

🔑 **`sdp_negotiate_seconds` p95 was 0.5 ms in every sample of every phase**
— 185 consecutive samples across the 46 collector-down minutes, identical
to the export-off control arm. p50 likewise. 0.5 ms is the second bucket
edge, so the honest statement is *"p95 never left the sub-millisecond
bucket"*, not a claim of 0.5 ms precision (§4's note on bucket edges).

CPU is the corroborating measure, and it is flat across arms: **56.5 %** of
one core with export off, **53.3 %** with export on and the collector dead.
A dead collector does not make the daemon work harder; if anything the
sample means drift the other way, within noise.

Setup success was total: **16,601 INVITEs accepted on daemon B, 16,601
calls completed, `invites_total{rejected}` = 0** in every sample, and SIPp
wrote **no error file at all** in any phase.

---

## 3. No unbounded memory growth

RSS over the 46 collector-down minutes, 5-minute stride:

```
18:41  67.1 MB   ← collector killed
18:46  68.7      18:51  69.4      18:56  70.8
19:01  75.4      19:06  75.7      19:11  76.1
19:16  77.5      19:21  77.5      19:26  77.7 MB
```

+10.7 MB over 46 min, and **decelerating**: the last 15 minutes added
0.16 MB. But §6.3 is explicit that a curve is the *worst* evidence here, so
the claim rests on the re-load test instead:

🔑 **A drained daemon absorbed an entire second 200-concurrent load for
+0.37 MB.** After a graceful drain (76.5 MB, fds back to 13), phase E ran
another ~2,200 calls to 200 concurrent and finished at **76.9 MB**. The
first load's growth was allocator arena fill, not retention — the same
signature `RESULTS-0.48.13.md` established, and the reason the phase-C
curve is not a leak.

🔑 **fds and threads never moved.** 608–611 under load with the collector
dead — indistinguishable from the control arm's 609–612 — and **13 after
drain**, the idle count. Thread count held at 8 for the whole B→E daemon
(6 in the control arm; the +2 are the log worker and the SDK's batch
thread, both created once). A dead gRPC endpoint reconnecting for 46
minutes leaked neither.

**Leak audit after graceful drain** (SIPp `SIGUSR1`, so calls end with real
BYEs): `calls_active` 0, `dialogs_active` 0, RTP ports bound 0, fds 13.

---

## 4. The feature works, and recovers

Verified against the running collector before the load:

- **19/19 records carried a populated `trace_id` and `span_id`.**
- **Resource attributes match the span pipeline**: `service.name=siphon-ai`,
  `service.instance.id=siphon-ai-otlp-soak` (the node id), and
  `deployment.environment=otlp-soak` from `[observability.otlp.attributes]`.
- 24 MB of log records exported over phase B's 15 minutes.

🔑 **Recovery needs no restart.** `docker start` on the collector and export
resumed within seconds at ~190 records/batch — the *steady* rate, with no
backlog burst, which independently confirms the outage's records were
dropped rather than buffered (§3's memory result says the same thing from
the other side).

🔑 **SIGTERM flushes.** `OTLP logger flushed + shut down` printed, and the
collector received 5 further records *after* the signal — the shutdown
lines themselves made the trip.

---

## 5. Warnings

Five `WARN` lines in 16,601 calls, all identical and all at teardown:

```
siphon_ai_media_glue::tap: events_tx full or closed; dropping rtp_stats event
```

A pre-existing teardown race in the media-glue tap, unrelated to log
export. Recorded rather than attributed: the control arm logged 0 in 2,747
calls, but at a rate of 5/14,053 the expected count in 2,747 calls is ~1, so
the two arms are not distinguishable at this sample size.

---

## 6. Two things for the operator docs

Neither is a defect. Both are things an operator would otherwise learn
during an incident.

🔑 **`siphon_ai_otlp_log_records_dropped_total` is not the collector-down
signal — it stayed at 0 for all 46 minutes with the collector dead.** That
is correct by design: the counter guards *our* bounded queue (1024), and
the worker kept draining it into the SDK at full speed. The records died
one layer further in, inside `BatchLogProcessor`'s own queue, which does not
feed this counter. So the counter answers "is the producer outrunning the
queue?" and never "is the collector gone?" — the loss during an outage is
silent as far as this metric is concerned. `docs/CONFIG.md`'s "watch that
counter, because movement there means the console has lines the collector
never received" is true, but its converse is not, and that is the reading an
operator is likely to make. The `reason` label was deliberately left open
for a `collector_down` value; this run is the argument for filling it in.

🔑 **Whether an outage is visible depends on the log filter.** This soak
logged nothing about the dead collector — 0 export-error lines in 46
minutes — because it ran a narrow `--log siphon_ai=info,siphon=info,forge=info`,
which admits no `opentelemetry` target. Re-run with the **built-in default
filter**, whose `warn` floor covers every target, the same outage is loud
within seconds:

```
ERROR opentelemetry_sdk: name="BatchLogProcessor.ExportError"
  error="… gRPC code: Unavailable: transport error: tcp connect error:
         Connection refused (os error 111)"
```

So the PR's claim holds as written — but only under a filter with the warn
floor. A `RUST_LOG` that names targets explicitly (a common enough
production habit) silences the only signal an outage produces today.

---

## 7. What this run does not cover

- **Receive-path media.** One-way only, per §Environment — no MOS or jitter
  claim is made.
- **Export at `debug`.** Run at `level = "info"`, the documented default.
  A debug firehose is the case that would actually exercise the 1024-entry
  queue and move the drop counter; it is untested.
- **A slow collector**, as distinct from a dead one. A collector that
  accepts connections and then stalls is the harder case for a batch
  pipeline, and it is not what `docker kill` produces.
- **Multi-hour duration.** 46 minutes down is enough to falsify a leak
  given the re-load result; it is not an 8-hour convergence run
  (`RESULTS-convergence-8h.md`).

---

## Reproducing

```sh
# collector
docker run -d --name otlp-soak-collector --network host \
  -v $PWD/collector.yaml:/etc/otelcol-contrib/config.yaml \
  otel/opentelemetry-collector-contrib:latest

# daemon: configs/soak.toml + [observability.otlp]{,.logs} enabled,
# SIP :5070, metrics :9591, rtp_port_range [30000, 30800]
cargo run -p siphon-ai --release -- --config soak-otlp.toml

# load: 200 concurrent, 60 s hold
sipp -sf churn_200_60s.xml -i 127.0.0.1 -p 5080 \
     -l 200 -r 10 -rp 1000 -s 1000 -nostdin 127.0.0.1:5070

# then, mid-soak
docker kill otlp-soak-collector

# graceful drain for the leak audit — SIGUSR1, not SIGKILL, or the calls
# never send a BYE and the watchdog is off
kill -USR1 $(pgrep -f 'sipp -sf churn_200_60s')
```

`churn_200_60s.xml` is `concurrent_burst_500.xml` with the pause cut from
600 s to 60 s, so calls churn instead of sitting.
