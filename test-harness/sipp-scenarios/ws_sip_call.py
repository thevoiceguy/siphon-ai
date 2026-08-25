#!/usr/bin/env python3
"""SIP-over-WebSocket (RFC 7118) signalling probe for run-all.sh.

SIPp cannot speak RFC 7118, so this is the suite's WS client
(DEV_PLAN_WebRTC.md Phase 1 §3.3). Uses the `websockets` package the
echo-ws-server venv already carries.

Modes:
  call     INVITE -> 100/200 -> ACK -> hold 1s -> BYE -> 200, all over
           one WS connection with the `sip` subprotocol and an allowed
           Origin. Exits 0 and prints CALL-OK on success. The SDP
           offers classic RTP/AVP PCMU; no RTP is actually sent, so
           run the daemon with the media watchdog generous or off.
  refused  Assert the upgrade is REFUSED for (a) a wrong Origin and
           (b) no Origin, when the daemon has an allow-list. Exits 0
           and prints REFUSED-OK when both are rejected.

Env: WS_URL (default ws://127.0.0.1:5082), WS_ORIGIN (the allowed
origin, default https://ops.example.com).
"""

import asyncio
import os
import sys

import websockets

WS_URL = os.environ.get("WS_URL", "ws://127.0.0.1:5082")
ORIGIN = os.environ.get("WS_ORIGIN", "https://ops.example.com")
CALL_ID = "ws-call-1@browser.invalid"


def sdp(port=6000):
    return (
        "v=0\r\no=browser 1 1 IN IP4 127.0.0.1\r\ns=-\r\n"
        "c=IN IP4 127.0.0.1\r\nt=0 0\r\n"
        f"m=audio {port} RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\n"
    )


def msg(method, cseq, extra="", body=""):
    m = (
        f"{method} sip:1000@127.0.0.1 SIP/2.0\r\n"
        f"Via: SIP/2.0/WS browser.invalid;branch=z9hG4bK-{method.lower()}-{cseq}\r\n"
        "Max-Forwards: 70\r\n"
        "From: <sip:browser@browser.invalid>;tag=bt1\r\n"
        f"To: <sip:1000@127.0.0.1>{extra}\r\n"
        f"Call-ID: {CALL_ID}\r\n"
        f"CSeq: {cseq} {method}\r\n"
        "Contact: <sip:browser@browser.invalid;transport=ws>\r\n"
    )
    if body:
        m += f"Content-Type: application/sdp\r\nContent-Length: {len(body)}\r\n\r\n{body}"
    else:
        m += "Content-Length: 0\r\n\r\n"
    return m.encode()


async def recv_status(ws, want, timeout=8):
    while True:
        r = await asyncio.wait_for(ws.recv(), timeout=timeout)
        if isinstance(r, bytes):
            r = r.decode(errors="replace")
        line = r.splitlines()[0]
        print("<", line)
        code = line.split(" ", 2)[1]
        if code == want:
            return r
        if not code.startswith("1"):
            raise SystemExit(f"expected {want}, got {line}")


async def call():
    async with websockets.connect(
        WS_URL, subprotocols=["sip"], additional_headers={"Origin": ORIGIN}
    ) as ws:
        assert ws.subprotocol == "sip", f"subprotocol {ws.subprotocol!r}"
        await ws.send(msg("INVITE", 1, body=sdp()))
        ok = await recv_status(ws, "200")
        to_line = next(l for l in ok.splitlines() if l.lower().startswith("to:"))
        tag = to_line.split("tag=", 1)[1].split(";")[0].strip()
        await ws.send(msg("ACK", 1, extra=f";tag={tag}"))
        await asyncio.sleep(1)
        await ws.send(msg("BYE", 2, extra=f";tag={tag}"))
        await recv_status(ws, "200")
        print("CALL-OK")


async def assert_refused(origin):
    kw = {"subprotocols": ["sip"]}
    if origin is not None:
        kw["additional_headers"] = {"Origin": origin}
    try:
        async with websockets.connect(WS_URL, **kw):
            raise SystemExit(f"upgrade with origin={origin!r} was NOT refused")
    except websockets.exceptions.InvalidStatus as e:
        code = e.response.status_code
        if code != 403:
            raise SystemExit(f"origin={origin!r}: expected 403, got {code}")
        print(f"refused as expected (403) origin={origin!r}")


async def refused():
    await assert_refused("https://evil.example.net")
    await assert_refused(None)
    print("REFUSED-OK")


mode = sys.argv[1] if len(sys.argv) > 1 else "call"
asyncio.run({"call": call, "refused": refused}[mode]())
