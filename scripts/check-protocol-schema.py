#!/usr/bin/env python3
"""Protocol-schema anti-drift gate (0.27.0, DESIGN_PROTOCOL_SDKS §2).

Two checks, same spirit as check-observability-metrics.py:

1. **Schema ↔ Rust drift** — regenerate the schema from the Rust types
   (`cargo run -p siphon-ai-bridge --example gen_schema --features
   json-schema`) and diff against the committed
   `schemas/siphon-ai.v1.json`. A protocol change without a regenerated
   schema fails here. Skipped with --no-regen (for environments without
   cargo).

2. **Schema ↔ docs corpus** — extract every fenced ```json block from
   `docs/PROTOCOL.md`, and validate each JSON object that carries a
   protocol `type` discriminator against the schema's top-level `oneOf`.
   A documented example the schema rejects (or a schema the examples
   outgrew) fails here. This is the third leg of the CLAUDE.md §4.2
   anti-drift loop: Rust types ↔ round-trip tests ↔ PROTOCOL.md ↔ schema.

Requires `jsonschema` (pip). Exit 0 = consistent.
"""

from __future__ import annotations

import json
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SCHEMA_PATH = ROOT / "schemas" / "siphon-ai.v1.json"
PROTOCOL_MD = ROOT / "docs" / "PROTOCOL.md"

GEN_CMD = [
    "cargo",
    "run",
    "--quiet",
    "-p",
    "siphon-ai-bridge",
    "--example",
    "gen_schema",
    "--features",
    "json-schema",
]


def fail(msg: str) -> None:
    print(f"ERROR: {msg}", file=sys.stderr)
    sys.exit(1)


def check_regen() -> None:
    result = subprocess.run(GEN_CMD, cwd=ROOT, capture_output=True, text=True)
    if result.returncode != 0:
        fail(f"schema generator failed:\n{result.stderr[-2000:]}")
    generated = json.loads(result.stdout)
    committed = json.loads(SCHEMA_PATH.read_text())
    if generated != committed:
        fail(
            "schemas/siphon-ai.v1.json is stale — the Rust protocol types "
            "changed. Regenerate it:\n  cargo run -p siphon-ai-bridge "
            "--example gen_schema --features json-schema "
            "> schemas/siphon-ai.v1.json"
        )
    print("schema ↔ Rust: consistent (regenerated output matches committed file)")


def extract_json_objects(md: str) -> list[tuple[int, dict]]:
    """Every JSON object in ```json fences, with its line number.

    A fence may hold one object or several (one per line / pretty-printed
    back-to-back); parse greedily with raw_decode.
    """
    objects: list[tuple[int, dict]] = []
    for match in re.finditer(r"```json\n(.*?)```", md, re.S):
        text = match.group(1)
        base_line = md[: match.start()].count("\n") + 2
        decoder = json.JSONDecoder()
        idx = 0
        while idx < len(text):
            # Skip to the next possible object start.
            brace = text.find("{", idx)
            if brace == -1:
                break
            try:
                obj, end = decoder.raw_decode(text, brace)
            except json.JSONDecodeError:
                idx = brace + 1
                continue
            if isinstance(obj, dict):
                objects.append((base_line + text[:brace].count("\n"), obj))
            idx = end
    return objects


def check_corpus() -> None:
    try:
        import jsonschema
    except ImportError:
        fail("the `jsonschema` package is required: pip install jsonschema")

    schema = json.loads(SCHEMA_PATH.read_text())
    validator = jsonschema.Draft202012Validator(schema)

    # The discriminators the schema knows about — only objects claiming
    # one of these are protocol messages (PROTOCOL.md also shows CDR
    # records, webhook payloads, admin bodies… those aren't in scope).
    known_types: set[str] = set()
    for union in ("BridgeOut", "BridgeIn"):
        for variant in schema["$defs"][union]["oneOf"]:
            known_types.add(variant["properties"]["type"]["const"])

    md = PROTOCOL_MD.read_text()
    checked = 0
    errors: list[str] = []
    for line, obj in extract_json_objects(md):
        if obj.get("type") not in known_types:
            continue
        checked += 1
        problems = sorted(validator.iter_errors(obj), key=lambda e: e.json_path)
        if problems:
            # oneOf failures nest; surface the variant-specific context.
            best = jsonschema.exceptions.best_match(problems)
            errors.append(
                f"PROTOCOL.md:{line} `type: {obj['type']}` fails the schema: "
                f"{best.message}"
            )
    if errors:
        fail(
            f"{len(errors)} documented example(s) do not validate:\n  "
            + "\n  ".join(errors)
        )
    if checked < 20:
        fail(
            f"only {checked} protocol examples found in PROTOCOL.md — the "
            "extractor or the docs regressed (expected 20+)"
        )
    print(
        f"schema ↔ docs: consistent ({checked} PROTOCOL.md examples validate, "
        f"{len(known_types)} known message types)"
    )


def main() -> None:
    if not SCHEMA_PATH.exists():
        fail(f"{SCHEMA_PATH} missing")
    if "--no-regen" not in sys.argv:
        check_regen()
    check_corpus()


if __name__ == "__main__":
    main()
