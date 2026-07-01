#!/usr/bin/env python3
"""Anti-drift guard for the shipped observability artifacts.

Every `siphon_ai_*` metric name referenced in the reference Prometheus rules
and Grafana dashboards under `examples/observability/` must actually be a
metric the daemon emits. A metric rename that forgets to update the
rules/dashboards would otherwise ship silently-broken artifacts; this check
(run in CI, same spirit as the version-consistency gate) fails loud instead.

The "emitted" set is every `"siphon_ai_..."` string literal across the Rust
workspace — not just the constants in `crates/telemetry/src/metrics.rs`,
because several metrics (e.g. `siphon_ai_sip_auth_total`,
`siphon_ai_invite_admission_total`, `siphon_ai_rtp_packet_loss_ratio`) are
emitted with inline string literals from other crates. Scanning the whole
workspace keeps the set a safe superset (a stray non-metric literal like a
log target only makes the check more lenient, never wrongly failing).

What it does NOT catch: PromQL/JSON semantics, label typos, or renames that
update both sides wrongly. It only asserts referenced base metric names are
real. Recorded-rule series (`siphon_ai:...`, with a colon) are defined by the
rules themselves and are intentionally ignored.
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
RUST_DIRS = [REPO / "crates", REPO / "bins"]
OBS_DIR = REPO / "examples" / "observability"

# Metric-name string literals anywhere in the Rust source: `"siphon_ai_foo"`.
DEFINED_RE = re.compile(r'"(siphon_ai_[a-z0-9_]+)"')
# References in YAML/JSON: underscore names only (colon recorded-series and
# Prometheus built-ins like `up` are excluded by construction).
REFERENCED_RE = re.compile(r"\bsiphon_ai_[a-z0-9_]+\b")

# metrics-exporter-prometheus renders a histogram as three series; strip the
# suffix back to the base name registered in metrics.rs.
HISTOGRAM_SUFFIXES = ("_bucket", "_sum", "_count")


def defined_metrics() -> set[str]:
    names: set[str] = set()
    for root in RUST_DIRS:
        for path in root.rglob("*.rs"):
            names.update(DEFINED_RE.findall(path.read_text()))
    return names


def base_name(name: str) -> str:
    for suffix in HISTOGRAM_SUFFIXES:
        if name.endswith(suffix):
            return name[: -len(suffix)]
    return name


def referenced_metrics() -> dict[str, list[str]]:
    """Map metric base-name -> list of files referencing it."""
    refs: dict[str, set[str]] = {}
    for path in sorted(OBS_DIR.rglob("*")):
        if path.suffix.lower() not in {".yml", ".yaml", ".json"}:
            continue
        rel = path.relative_to(REPO).as_posix()
        for raw in REFERENCED_RE.findall(path.read_text()):
            refs.setdefault(base_name(raw), set()).add(rel)
    return {name: sorted(files) for name, files in refs.items()}


def main() -> int:
    if not OBS_DIR.exists():
        print(f"error: {OBS_DIR} not found", file=sys.stderr)
        return 2

    defined = defined_metrics()
    if not defined:
        print("error: no siphon_ai_ metric literals found in the workspace", file=sys.stderr)
        return 2

    referenced = referenced_metrics()
    unknown = {name: files for name, files in referenced.items() if name not in defined}

    if unknown:
        print("Observability artifacts reference unknown metric(s):", file=sys.stderr)
        for name, files in sorted(unknown.items()):
            print(f"  - {name}  (in {', '.join(files)})", file=sys.stderr)
        print(
            "\nEither the metric was renamed/removed in the Rust source, or the "
            "reference is a typo. Update examples/observability/ to match.",
            file=sys.stderr,
        )
        return 1

    print(
        f"Observability metrics consistent: "
        f"{len(referenced)} referenced, all emitted "
        f"({len(defined)} siphon_ai_ literals in the workspace)."
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
