#!/usr/bin/env python3
"""Hold half the RTP pairs in a range, the way an ephemeral socket does.

Reproduces siphon-ai#504 deliberately instead of waiting for the 1-in-399
chance: bind the RTP (even) port of every other pair, so roughly half of
the pool's draws collide. On 0.48.18 that fails calls outright; on 0.48.19
forge steps past to another pair (up to five draws).

Point `[media].rtp_port_range` at a range OUTSIDE any
net.ipv4.ip_local_reserved_ports reservation, or there is nothing to squat.
Results of the 0.48.18-vs-0.48.19 A/B run: RESULTS-0.48.19.md.

Usage: squat.py <min_port> <max_port> [--fraction 0.5]
Prints how many it holds, then sleeps until killed.
"""
import socket
import sys
import time

lo, hi = int(sys.argv[1]), int(sys.argv[2])
frac = 0.5
if "--fraction" in sys.argv:
    frac = float(sys.argv[sys.argv.index("--fraction") + 1])

held = []
# Pairs are (even, even+1). Take the even port of every Nth pair.
step = int(round(2 / frac)) if frac > 0 else 2
if step % 2:
    step += 1
for p in range(lo, hi, step):
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    try:
        s.bind(("0.0.0.0", p))
        held.append(s)
    except OSError:
        s.close()

total_pairs = len(range(lo, hi, 2))
print(f"holding {len(held)} of {total_pairs} RTP ports in {lo}-{hi} "
      f"({100 * len(held) / total_pairs:.0f}%)", flush=True)
while True:
    time.sleep(3600)
