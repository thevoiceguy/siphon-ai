#!/usr/bin/env python3
"""Print the CHANGELOG.md section body for one released version.

Given `X.Y.Z`, emits everything between that version's heading
(`## [X.Y.Z] - YYYY-MM-DD`) and the next `## [` heading, with
surrounding blank lines trimmed. Used by the release workflow to feed
`gh release create --notes-file` from the hand-curated changelog (the
notes are extracted, never invented).

Usage:
  extract-changelog.py 0.16.0                 # section -> stdout
  extract-changelog.py 0.16.0 --selftest      # run embedded fixtures

Exits 1 if the version has no dated section (the version-consistency
gate should already have caught that on the tag build).
"""

import argparse
import os
import re
import sys

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
SEMVER = r"\d+\.\d+\.\d+"


def extract(changelog_text, version):
    """Return the trimmed section body, or None if absent.

    Matches a *dated* heading (`## [X.Y.Z] - DATE`) so `[Unreleased]`
    is never returned as a release section.
    """
    heading = re.compile(
        r"^##\s*\[" + re.escape(version) + r"\]\s*-\s*\d{4}-\d{2}-\d{2}\s*$"
    )
    next_heading = re.compile(r"^##\s*\[")
    lines = changelog_text.splitlines()
    start = None
    for i, line in enumerate(lines):
        if heading.match(line):
            start = i + 1
            break
    if start is None:
        return None
    end = len(lines)
    for j in range(start, len(lines)):
        if next_heading.match(lines[j]):
            end = j
            break
    body = "\n".join(lines[start:end]).strip("\n")
    return body.strip()


def selftest():
    sample = (
        "# Changelog\n\n"
        "## [Unreleased]\n\n"
        "## [0.16.0] - 2026-07-01\n\n"
        "### Added\n\n- A thing.\n- Another.\n\n"
        "## [0.15.0] - 2026-06-24\n\n"
        "### Added\n\n- Old thing.\n"
    )
    cases = [
        ("0.16.0", "### Added\n\n- A thing.\n- Another."),
        ("0.15.0", "### Added\n\n- Old thing."),
        ("9.9.9", None),
    ]
    failures = 0
    for version, expect in cases:
        got = extract(sample, version)
        ok = got == expect
        if not ok:
            failures += 1
        print(f"  [{'ok' if ok else 'FAIL'}] {version}: {got!r}")
    # `[Unreleased]` must never be returned as a release section.
    if extract(sample, "Unreleased") is not None:
        failures += 1
        print("  [FAIL] Unreleased leaked as a section")
    if failures:
        print(f"selftest: {failures} case(s) failed", file=sys.stderr)
        return 1
    print("selftest: all cases passed.")
    return 0


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("version", nargs="?", help="version, e.g. 0.16.0")
    parser.add_argument("--selftest", action="store_true")
    args = parser.parse_args()

    if args.selftest:
        return selftest()
    if not args.version:
        parser.error("version is required (or pass --selftest)")

    with open(os.path.join(REPO, "CHANGELOG.md"), encoding="utf-8") as fh:
        body = extract(fh.read(), args.version)
    if not body:
        print(
            f"CHANGELOG.md: no dated section for {args.version}",
            file=sys.stderr,
        )
        return 1
    print(body)
    return 0


if __name__ == "__main__":
    sys.exit(main())
