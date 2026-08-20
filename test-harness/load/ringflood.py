#!/usr/bin/env python3
"""Flood a siphon-ai node with N distinct SIP dialogs x M messages each.

Isolates the SIP-ladder ring's memory cost: no media, no WS server, no
call setup — just SIP messages carrying Call-IDs the ring will file.
Every message is a fresh transaction (unique branch + CSeq) so nothing
is absorbed as a retransmission before it reaches the HEP sink.
"""
import argparse, socket, sys, time

ap = argparse.ArgumentParser()
ap.add_argument("--host", default="127.0.0.1")
ap.add_argument("--port", type=int, default=5070)
ap.add_argument("--traces", type=int, default=256)
ap.add_argument("--messages", type=int, default=64, help="per trace")
ap.add_argument("--pad", type=int, default=0, help="extra header bytes per message")
a = ap.parse_args()

s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
s.setblocking(False)
pad = ("X-Pad: " + "p" * a.pad + "\r\n") if a.pad else ""
sent = 0
for t in range(a.traces):
    cid = f"ringflood-{t}"
    for m in range(a.messages):
        body = (
            # OPTIONS, not INVITE: an unACKed INVITE leaves a server
            # transaction retransmitting its 403 for ~32 s, and that
            # storm's timing swamped the very thing being measured
            # (the ring-on arm swung 61-125 MB while ring-off held
            # +/-1.3). OPTIONS is one request, one 200, done.
            f"OPTIONS sip:911@{a.host}:{a.port} SIP/2.0\r\n"
            f"Via: SIP/2.0/UDP 127.0.0.1:9;branch=z9hG4bKrf{t}x{m};rport\r\n"
            f"From: <sip:flood@example.net>;tag=f{t}x{m}\r\n"
            f"To: <sip:911@{a.host}>\r\n"
            f"Call-ID: {cid}\r\n"
            f"CSeq: {m + 1} OPTIONS\r\n"
            f"Contact: <sip:flood@127.0.0.1:9>\r\n"
            f"{pad}"
            f"Max-Forwards: 70\r\nContent-Length: 0\r\n\r\n"
        )
        try:
            s.sendto(body.encode(), (a.host, a.port))
            sent += 1
        except OSError:
            time.sleep(0.01)
        # Pace to stay under the kernel's UDP receive buffer. Without
        # this the socket, not the ring, decides what gets measured.
        if sent % 100 == 0:
            time.sleep(0.02)
print(f"sent {sent} messages across {a.traces} traces", flush=True)
