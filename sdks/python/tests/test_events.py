"""Corpus + schema conformance for the typed events (design D2).

Three legs:
1. every `docs/PROTOCOL.md` daemon→server example parses into the right
   typed event (no UnknownEvent, no ValueError);
2. every command the SDK can send validates against
   `schemas/siphon-ai.v1.json` `$defs/BridgeIn`;
3. tolerance invariants — unknown fields ignored, unknown types wrapped.

Run from the repo root: `python -m unittest discover sdks/python/tests`.
"""

from __future__ import annotations

import json
import re
import sys
import unittest
from pathlib import Path

REPO = Path(__file__).resolve().parents[3]
sys.path.insert(0, str(REPO / "sdks" / "python" / "src"))

from siphon_ai_server import events  # noqa: E402
from siphon_ai_server.events import Start, UnknownEvent, parse_event  # noqa: E402

SCHEMA = json.loads((REPO / "schemas" / "siphon-ai.v1.json").read_text())
PROTOCOL_MD = (REPO / "docs" / "PROTOCOL.md").read_text()


def _known_types(union: str) -> set[str]:
    return {
        v["properties"]["type"]["const"] for v in SCHEMA["$defs"][union]["oneOf"]
    }


BRIDGE_OUT_TYPES = _known_types("BridgeOut")
BRIDGE_IN_TYPES = _known_types("BridgeIn")
# Direction-ambiguous discriminators (hold/resume/mark): PROTOCOL.md
# examples can't be classified by name alone; classify by shape instead —
# BridgeOut messages always carry `seq`, BridgeIn commands never do.
AMBIGUOUS = BRIDGE_OUT_TYPES & BRIDGE_IN_TYPES


def corpus() -> list[dict]:
    objects: list[dict] = []
    for match in re.finditer(r"```json\n(.*?)```", PROTOCOL_MD, re.S):
        text = match.group(1)
        decoder = json.JSONDecoder()
        idx = 0
        while idx < len(text):
            brace = text.find("{", idx)
            if brace == -1:
                break
            try:
                obj, end = decoder.raw_decode(text, brace)
            except json.JSONDecodeError:
                idx = brace + 1
                continue
            if isinstance(obj, dict):
                objects.append(obj)
            idx = end
    return objects


class CorpusTest(unittest.TestCase):
    def test_every_documented_event_parses_typed(self) -> None:
        seen: set[str] = set()
        for obj in corpus():
            t = obj.get("type")
            if t not in BRIDGE_OUT_TYPES:
                continue
            if t in AMBIGUOUS and "seq" not in obj:
                continue  # it's the BridgeIn command form
            event = parse_event(json.dumps(obj))
            self.assertNotIsInstance(
                event, UnknownEvent, f"documented `{t}` must parse typed"
            )
            self.assertEqual(event.type, t)
            seen.add(t)
        # The corpus must exercise a healthy majority of the surface.
        self.assertGreaterEqual(
            len(seen), 15, f"corpus only covered {sorted(seen)}"
        )

    def test_sdk_knows_every_bridge_out_type(self) -> None:
        self.assertEqual(set(events._EVENT_TYPES), BRIDGE_OUT_TYPES)


class CommandSchemaTest(unittest.TestCase):
    """Every command the SDK can emit validates against $defs/BridgeIn."""

    def test_commands_validate(self) -> None:
        try:
            import jsonschema
        except ImportError:
            self.skipTest("jsonschema not installed")
        validator = jsonschema.Draft202012Validator(
            {"$ref": "#/$defs/BridgeIn", "$defs": SCHEMA["$defs"]}
        )
        commands = [
            {"type": "clear"},
            {"type": "mark", "name": "greeting_done"},
            {"type": "hangup", "cause": "normal"},
            {"type": "hangup", "cause": "busy"},
            {"type": "transfer", "target": "sip:agent@pbx.example.com"},
            {"type": "transfer", "replaces_call_id": "siphon-abc"},
            {"type": "send_dtmf", "digit": "5", "duration_ms": 160},
            {"type": "mute"},
            {"type": "unmute"},
            {"type": "start_recording"},
            {"type": "stop_recording"},
            {"type": "pause_recording"},
            {"type": "resume_recording"},
            {"type": "set_recording_consent", "note": "dtmf-1"},
            {"type": "set_recording_consent"},
            {"type": "park", "slot": "vip"},
            {"type": "park"},
            {"type": "conference_join", "room_id": "support-7"},
            {"type": "conference_leave"},
            {"type": "hold"},
            {"type": "resume"},
        ]
        emitted_types = set()
        for msg in commands:
            msg = {**msg, "call_id": "siphon-test"}
            problems = list(validator.iter_errors(msg))
            self.assertFalse(
                problems, f"{msg['type']} fails schema: {problems[:1]}"
            )
            emitted_types.add(msg["type"])
        self.assertEqual(
            emitted_types,
            BRIDGE_IN_TYPES,
            "the SDK must exercise every BridgeIn command",
        )


class ToleranceTest(unittest.TestCase):
    def test_unknown_type_wraps_not_raises(self) -> None:
        event = parse_event('{"type": "yodel", "call_id": "c", "seq": 1}')
        self.assertIsInstance(event, UnknownEvent)
        self.assertEqual(event.raw["seq"], 1)

    def test_unknown_fields_ignored(self) -> None:
        event = parse_event(
            '{"type": "dtmf", "call_id": "c", "seq": 9, "digit": "1",'
            ' "duration_ms": 160, "method": "rfc2833", "novel_field": true}'
        )
        self.assertEqual(event.digit, "1")

    def test_start_from_keyword_rename(self) -> None:
        raw = {
            "type": "start",
            "version": "1",
            "call_id": "c",
            "seq": 0,
            "from": "+13125551212",
            "to": "5000",
            "direction": "inbound",
            "audio": {
                "encoding": "pcm16le",
                "sample_rate": 8000,
                "channels": 1,
                "frame_ms": 20,
            },
            "sip": {"call_id": "x@y", "headers": {}},
        }
        event = parse_event(json.dumps(raw))
        self.assertIsInstance(event, Start)
        self.assertEqual(event.from_, "+13125551212")
        self.assertFalse(event.reconnected)

    def test_malformed_json_raises(self) -> None:
        with self.assertRaises(ValueError):
            parse_event("{nope")
        with self.assertRaises(ValueError):
            parse_event('{"no_type": 1}')


if __name__ == "__main__":
    unittest.main()
