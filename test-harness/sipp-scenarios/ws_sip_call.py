#!/usr/bin/env python3
"""SIP-over-WebSocket (RFC 7118) signalling probe for run-all.sh.

SIPp cannot speak RFC 7118, so this is the suite's WS client
(DEV_PLAN_WebRTC.md Phase 1 §3.3). Uses the `websockets` package the
echo-ws-server venv already carries.

Modes:
  call      INVITE -> 100/200 -> ACK -> hold 1s -> BYE -> 200, all over
            one WS connection with the `sip` subprotocol and an allowed
            Origin. Exits 0 and prints CALL-OK on success. The SDP
            offers classic RTP/AVP PCMU; no RTP is actually sent, so
            run the daemon with the media watchdog generous or off.
  refused   Assert the upgrade is REFUSED for (a) a wrong Origin and
            (b) no Origin, when the daemon has an allow-list. Exits 0
            and prints REFUSED-OK when both are rejected.
  register  The SIP.js-shaped registration flow against [registrar]:
            REGISTER -> 401 (digest challenge) -> REGISTER with MD5
            digest credentials -> 200, then Expires: 0 -> 200 to
            unregister. Prints REGISTER-OK. Env: WS_AUTH_USER /
            WS_AUTH_PASS (default browser / s3cret-ws).

Env: WS_URL (default ws://127.0.0.1:5082), WS_ORIGIN (the allowed
origin, default https://ops.example.com).
"""

import asyncio
import hashlib
import os
import re
import secrets
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


AUTH_USER = os.environ.get("WS_AUTH_USER", "browser")
AUTH_PASS = os.environ.get("WS_AUTH_PASS", "s3cret-ws")


def register_msg(cseq, expires, authorization=None):
    m = (
        "REGISTER sip:127.0.0.1 SIP/2.0\r\n"
        f"Via: SIP/2.0/WS browser.invalid;branch=z9hG4bK-reg-{cseq}\r\n"
        "Max-Forwards: 70\r\n"
        f"From: <sip:{AUTH_USER}@127.0.0.1>;tag=rt1\r\n"
        f"To: <sip:{AUTH_USER}@127.0.0.1>\r\n"
        "Call-ID: ws-reg-1@browser.invalid\r\n"
        f"CSeq: {cseq} REGISTER\r\n"
        f"Contact: <sip:{AUTH_USER}@browser.invalid;transport=ws>\r\n"
        f"Expires: {expires}\r\n"
    )
    if authorization:
        m += f"Authorization: {authorization}\r\n"
    m += "Content-Length: 0\r\n\r\n"
    return m.encode()


def digest_response(challenge, method, uri):
    """RFC 7616 MD5 digest from a WWW-Authenticate header value."""
    params = {
        k: quoted or bare
        for k, quoted, bare in re.findall(r'(\w+)=(?:"([^"]*)"|([^",\s]+))', challenge)
    }
    realm, nonce = params["realm"], params["nonce"]
    qop = params.get("qop")
    ha1 = hashlib.md5(f"{AUTH_USER}:{realm}:{AUTH_PASS}".encode()).hexdigest()
    ha2 = hashlib.md5(f"{method}:{uri}".encode()).hexdigest()
    auth = (
        f'Digest username="{AUTH_USER}", realm="{realm}", nonce="{nonce}", '
        f'uri="{uri}", algorithm=MD5'
    )
    if qop and "auth" in qop:
        cnonce, nc = secrets.token_hex(8), "00000001"
        resp = hashlib.md5(
            f"{ha1}:{nonce}:{nc}:{cnonce}:auth:{ha2}".encode()
        ).hexdigest()
        auth += f', qop=auth, nc={nc}, cnonce="{cnonce}", response="{resp}"'
    else:
        resp = hashlib.md5(f"{ha1}:{nonce}:{ha2}".encode()).hexdigest()
        auth += f', response="{resp}"'
    if "opaque" in params:
        auth += f', opaque="{params["opaque"]}"'
    return auth


async def register():
    async with websockets.connect(
        WS_URL, subprotocols=["sip"], additional_headers={"Origin": ORIGIN}
    ) as ws:
        await ws.send(register_msg(1, 600))
        challenge_resp = await asyncio.wait_for(ws.recv(), timeout=5)
        if isinstance(challenge_resp, bytes):
            challenge_resp = challenge_resp.decode(errors="replace")
        first = challenge_resp.splitlines()[0]
        print("<", first)
        assert " 401 " in first, f"expected 401 challenge, got {first}"
        challenge = next(
            l.split(":", 1)[1].strip()
            for l in challenge_resp.splitlines()
            if l.lower().startswith("www-authenticate:")
        )
        auth = digest_response(challenge, "REGISTER", "sip:127.0.0.1")
        await ws.send(register_msg(2, 600, auth))
        ok = await recv_status(ws, "200")
        assert any(l.lower().startswith("contact:") for l in ok.splitlines()), \
            "200 must echo the registered Contact"
        print("registered")
        # And out again: Expires: 0 needs a fresh digest (new nonce may
        # be required; reuse works within the server's reuse window).
        await ws.send(register_msg(3, 0, auth))
        r = await asyncio.wait_for(ws.recv(), timeout=5)
        if isinstance(r, bytes):
            r = r.decode(errors="replace")
        code = r.splitlines()[0].split(" ", 2)[1]
        if code == "401":  # stale nonce — one re-auth round
            challenge = next(
                l.split(":", 1)[1].strip()
                for l in r.splitlines()
                if l.lower().startswith("www-authenticate:")
            )
            auth = digest_response(challenge, "REGISTER", "sip:127.0.0.1")
            await ws.send(register_msg(4, 0, auth))
            await recv_status(ws, "200")
        else:
            assert code == "200", f"unregister got {code}"
        print("REGISTER-OK")


if __name__ == "__main__":
    mode = sys.argv[1] if len(sys.argv) > 1 else "call"
    asyncio.run({"call": call, "refused": refused, "register": register}[mode]())
