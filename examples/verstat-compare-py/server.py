#!/usr/bin/env python3
"""Cross-check SiphonAI's STIR/SHAKEN verdict against Twilio's claim.

A diagnostic WebSocket server: it accepts a SiphonAI bridge connection,
reads the ``start`` message, and compares two independent signals about the
same call —

  * **ours** — ``start.verstat`` (SiphonAI verified the PASSporT itself:
    fetched the x5u cert, validated the chain to the STI-PA anchor, checked
    the ES256 signature, the orig/dest TN binding, and ``iat`` freshness),
  * **Twilio's** — the ``X-Twilio-VerStat`` SIP header Twilio sets on
    inbound calls, surfaced here via ``[bridge].forward_headers``.

It logs AGREE / DIVERGE per call. Useful during initial deployment to
confirm our verification matches what the carrier already told us, and to
catch misconfiguration (e.g. a stale trust anchor that fails every call
Twilio says passed). It does not play any audio — it just drains the
stream after the comparison.

Run:  python3 server.py --bind 127.0.0.1:8765
Needs: websockets  (pip install websockets)
"""

from __future__ import annotations

import argparse
import asyncio
import json
import logging

from websockets.asyncio.server import ServerConnection, serve

LOG = logging.getLogger("verstat-compare")
SUBPROTOCOL = "siphon-ai.v1"


def parse_twilio_verstat(value: str | None):
    """Parse an ``X-Twilio-VerStat`` value into (passed, attest).

    Twilio sets one of: ``TN-Validation-Passed-{A,B,C}``,
    ``TN-Validation-Failed-{A,B,C}``, or ``No-TN-Validation``. Returns
    ``(None, None)`` when the header is absent or unrecognised.
    """
    if not value:
        return (None, None)
    v = value.strip()
    if v == "No-TN-Validation":
        return (False, None)
    if v.startswith("TN-Validation-Passed-"):
        return (True, v.rsplit("-", 1)[-1])
    if v.startswith("TN-Validation-Failed-"):
        return (False, v.rsplit("-", 1)[-1])
    return (None, None)


def our_verdict(verstat: dict | None):
    """Reduce ``start.verstat`` to (passed, trusted_attest).

    ``passed`` mirrors SiphonAI's composite: every check must hold. The
    attestation is trustworthy only when ``passed``.
    """
    if not verstat:
        return (None, None)
    passed = all(
        verstat.get(k)
        for k in (
            "signature_valid",
            "cert_chain_valid",
            "orig_passed",
            "dest_passed",
            "iat_passed",
        )
    )
    return (passed, verstat.get("attest") if passed else None)


def compare(call_id: str, verstat: dict | None, twilio_header: str | None) -> None:
    ours_passed, ours_attest = our_verdict(verstat)
    tw_passed, tw_attest = parse_twilio_verstat(twilio_header)

    if verstat is None:
        LOG.warning(
            "call_id=%s no SiphonAI verstat (is [security.stir_shaken] enabled?); "
            "Twilio says passed=%s attest=%s",
            call_id, tw_passed, tw_attest,
        )
        return
    if twilio_header is None:
        LOG.warning(
            "call_id=%s no X-Twilio-VerStat (not a Twilio call, or not in "
            "[bridge].forward_headers); ours passed=%s attest=%s",
            call_id, ours_passed, ours_attest,
        )
        return

    agree = (ours_passed == tw_passed) and (
        not ours_passed or ours_attest == tw_attest
    )
    level = LOG.info if agree else LOG.warning
    level(
        "call_id=%s %s — ours(passed=%s attest=%s) vs twilio(passed=%s attest=%s) [%r]",
        call_id,
        "AGREE" if agree else "DIVERGE",
        ours_passed, ours_attest, tw_passed, tw_attest, twilio_header,
    )


async def handle(connection: ServerConnection) -> None:
    try:
        async for message in connection:
            if isinstance(message, (bytes, bytearray)):
                continue  # audio frame — drain, we don't play anything back
            try:
                msg = json.loads(message)
            except json.JSONDecodeError:
                continue
            if msg.get("type") == "start":
                sip_headers = msg.get("sip", {}).get("headers", {})
                compare(
                    msg.get("call_id"),
                    msg.get("verstat"),
                    sip_headers.get("X-Twilio-VerStat"),
                )
    except Exception as e:  # noqa: BLE001 — diagnostic server, never crash a call
        LOG.debug("connection ended: %s", e)


async def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--bind", default="127.0.0.1:8765")
    ap.add_argument("--log-level", default="INFO")
    args = ap.parse_args()
    logging.basicConfig(level=args.log_level, format="%(asctime)s %(levelname)s %(message)s")
    host, _, port = args.bind.partition(":")
    async with serve(handle, host, int(port), subprotocols=[SUBPROTOCOL]):
        LOG.info("verstat-compare listening on %s (subprotocol %s)", args.bind, SUBPROTOCOL)
        await asyncio.get_event_loop().create_future()  # run forever


if __name__ == "__main__":
    asyncio.run(main())
