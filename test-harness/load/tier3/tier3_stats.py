#!/usr/bin/env python3
"""Summarise a tier-3 run from the daemon's CDRs and lifecycle webhooks.

Usage: tier3_stats.py <cdr.jsonl> [lifecycle.jsonl]
                     [--since-line=N] [--since-lc-line=N]

Both files are appended to by the running daemon and carry every call it
has ever handled, so a run is identified by the line count each file had
when it started. Filtering only the CDRs silently mixes earlier calls into
the webhook-derived figures — which is how the round-trip sample count came
out larger than the call count on the first run of this script.

Everything here is per-call, which is the point: the sequential form of
§10.3 trades concurrency for a distribution, so the output is percentiles
over calls rather than a single loaded-system number.
"""
import json
import sys
from datetime import datetime


def ts(v):
    return datetime.fromisoformat(v.replace("Z", "+00:00")) if v else None


def pct(xs, p):
    """Nearest-rank percentile. With n=60 the p95 is the 57th of 60 — say so
    next to the number rather than implying a smooth tail."""
    if not xs:
        return None
    s = sorted(xs)
    k = max(0, min(len(s) - 1, int(round(p / 100 * len(s) + 0.5)) - 1))
    return s[k]


def summarise(name, xs, unit="", nd=1):
    if not xs:
        print(f"  {name:24} (no samples)")
        return
    print(
        f"  {name:24} n={len(xs):<4} min={min(xs):.{nd}f} p50={pct(xs,50):.{nd}f} "
        f"p95={pct(xs,95):.{nd}f} max={max(xs):.{nd}f} {unit}"
    )


def main():
    args = [a for a in sys.argv[1:] if not a.startswith("--")]
    since = since_lc = 0
    for a in sys.argv[1:]:
        if a.startswith("--since-line"):
            since = int(a.split("=", 1)[1])
        elif a.startswith("--since-lc-line"):
            since_lc = int(a.split("=", 1)[1])
    cdr_path = args[0]
    lc_path = args[1] if len(args) > 1 else None

    rows = []
    with open(cdr_path) as fh:
        for n, line in enumerate(fh, 1):
            if n <= since:
                continue
            try:
                rows.append(json.loads(line))
            except json.JSONDecodeError:
                continue

    out = [r for r in rows if r.get("direction") == "outbound"]
    inb = [r for r in rows if r.get("direction") == "inbound"]

    setup, dur = [], []
    for r in out:
        a, s = ts(r.get("answered_at")), ts(r.get("started_at"))
        if a and s:
            setup.append((a - s).total_seconds() * 1000)
        if r.get("duration_ms"):
            dur.append(r["duration_ms"] / 1000)

    print(f"outbound legs: {len(out)}   inbound (returned) legs: {len(inb)}")
    if len(out) != len(inb):
        print(f"  !! {abs(len(out)-len(inb))} leg(s) unmatched — a trombone that "
              f"did not close is a failed sample, not a short one")
    print("\nCarrier setup latency (outbound INVITE -> 200 OK):")
    summarise("setup_ms", setup, "ms", 0)
    print("\nCall duration (sanity — should cluster at the hold):")
    summarise("seconds", dur, "s", 1)

    for label, rs in (("outbound", out), ("inbound (returned)", inb)):
        q = [r.get("quality") or {} for r in rs]
        print(f"\nQuality — {label} leg:")
        summarise("mos_estimate_avg", [x["mos_estimate_avg"] for x in q
                                       if x.get("mos_estimate_avg") is not None], "", 3)
        summarise("avg_jitter_ms", [x["avg_jitter_ms"] for x in q
                                    if x.get("avg_jitter_ms") is not None], "ms", 2)
        lost = [x.get("rx_packets_lost", 0) or 0 for x in q]
        recv = [x.get("rx_packets_received", 0) or 0 for x in q]
        if recv:
            tot_l, tot_r = sum(lost), sum(recv)
            print(f"  {'packet loss':24} {tot_l}/{tot_r+tot_l} packets "
                  f"= {100*tot_l/max(1,tot_r+tot_l):.3f}%   "
                  f"(calls with any loss: {sum(1 for x in lost if x)}/{len(lost)})")

    causes = {}
    for r in rows:
        c = (r.get("termination") or {}).get("cause", "?")
        causes[c] = causes.get(c, 0) + 1
    print("\nTermination causes:", ", ".join(f"{k}={v}" for k, v in sorted(causes.items())))

    if lc_path:
        # The carrier's own out-and-back: our INVITE goes out, and the call
        # arriving BACK is a `call_start`. That isolates the SBC from our
        # own answer, which the CDR's setup figure folds together.
        events = []
        with open(lc_path) as fh:
            for n, line in enumerate(fh, 1):
                if n <= since_lc:
                    continue
                try:
                    b = json.loads(line).get("body", {})
                except json.JSONDecodeError:
                    continue
                if b.get("type") in ("outbound_initiated", "call_start"):
                    events.append((ts(b["timestamp"]), b["type"]))
        events.sort()
        rt = []
        pending = None
        for t, kind in events:
            if kind == "outbound_initiated":
                pending = t
            elif kind == "call_start" and pending:
                d = (t - pending).total_seconds() * 1000
                if 0 < d < 10000:
                    rt.append(d)
                pending = None
        print("\nCarrier round trip (our INVITE out -> the call arriving back):")
        summarise("round_trip_ms", rt, "ms", 0)


if __name__ == "__main__":
    main()
