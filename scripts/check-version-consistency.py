#!/usr/bin/env python3
"""Fail the build when the project's version is stated inconsistently.

The workspace `Cargo.toml` `[workspace.package].version` is the single
source of truth. Everything else must agree with it:

  * `README.md` carries a `**Current release: vX.Y.Z**` marker that must
    equal the workspace version. (Other version mentions in the README —
    e.g. "Since v0.10.0 …" feature notes, or the protocol `version: "1"`
    — are deliberately NOT matched; only the canonical marker is.)
  * `CHANGELOG.md` must have a *dated* `## [X.Y.Z] - YYYY-MM-DD` heading
    for the workspace version (not just `## [Unreleased]`).

The version only moves in the `chore(release)` commit, which updates all
three together — so in steady-state development (Cargo pinned to the last
released version, entries accruing under `[Unreleased]`) this gate stays
green, and it goes red the moment the three drift (this repo has drifted
twice; README sat at v0.12.2 while the tag was v0.15.0).

Usage:
  check-version-consistency.py            # gate the working tree
  check-version-consistency.py --tag vX.Y.Z   # also assert tag == version
  check-version-consistency.py --selftest     # run embedded fixtures

A final `--tag vX.Y.Z` must equal the workspace version. A pre-release
tag (`vX.Y.Z-rc.N`) only needs base >= workspace, so the release pipeline
can be validated for an upcoming version before the version bump.

Exits 0 when consistent, 1 (listing each mismatch on stderr) otherwise.
Stdlib-only, mirroring scripts/check-doc-links.py.
"""

import argparse
import os
import re
import sys

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))

SEMVER = r"\d+\.\d+\.\d+"
# `version = "X.Y.Z"`, taken from inside the [workspace.package] table.
README_MARKER_RE = re.compile(r"Current release:\s*v(" + SEMVER + r")")
# A *released* (dated) changelog heading: `## [X.Y.Z] - 2026-06-24`.
CHANGELOG_HEADING_RE = re.compile(
    r"^##\s*\[(" + SEMVER + r")\]\s*-\s*\d{4}-\d{2}-\d{2}\s*$", re.MULTILINE
)


def workspace_version(cargo_text):
    """Pull `version` from the `[workspace.package]` table.

    Returns the version string, or None if the table/key is absent.
    """
    in_table = False
    for line in cargo_text.splitlines():
        stripped = line.strip()
        if stripped.startswith("[") and stripped.endswith("]"):
            in_table = stripped == "[workspace.package]"
            continue
        if in_table:
            m = re.match(r'version\s*=\s*"(' + SEMVER + r')"', stripped)
            if m:
                return m.group(1)
    return None


def readme_version(readme_text):
    """The version in the `Current release: vX.Y.Z` marker, or None."""
    m = README_MARKER_RE.search(readme_text)
    return m.group(1) if m else None


def changelog_has_release(changelog_text, version):
    """True if a dated `## [version] - DATE` heading exists."""
    return any(
        m.group(1) == version
        for m in CHANGELOG_HEADING_RE.finditer(changelog_text)
    )


def check(cargo_text, readme_text, changelog_text, tag=None):
    """Return a list of human-readable mismatch strings (empty == OK)."""
    errors = []

    version = workspace_version(cargo_text)
    if version is None:
        return ["Cargo.toml: no [workspace.package].version found"]

    rv = readme_version(readme_text)
    if rv is None:
        errors.append(
            "README.md: no 'Current release: vX.Y.Z' marker found"
        )
    elif rv != version:
        errors.append(
            f"README.md: 'Current release: v{rv}' != workspace "
            f"version {version}"
        )

    if not changelog_has_release(changelog_text, version):
        errors.append(
            f"CHANGELOG.md: no dated '## [{version}] - YYYY-MM-DD' "
            f"heading for workspace version {version}"
        )

    if tag is not None:
        errors.extend(_check_tag(tag, version))

    return errors


def _ver_tuple(semver):
    return tuple(int(p) for p in semver.split("."))


def _check_tag(tag, version):
    """Validate a release tag against the workspace version.

    A *final* tag (`vX.Y.Z`) must equal the workspace version exactly —
    you can't tag a version the tree doesn't claim to be. A *pre-release*
    tag (`vX.Y.Z-rc.N`) is allowed to target the current or a future
    version (base >= workspace) so the release pipeline can be exercised
    for an upcoming version without a premature version bump; the release
    workflow marks these as pre-releases (never latest).
    """
    raw = tag[1:] if tag.startswith("v") else tag
    base = raw.split("-", 1)[0]
    is_prerelease = "-" in raw

    if not re.fullmatch(SEMVER, base):
        return [f"tag {tag}: '{base}' is not an X.Y.Z version"]
    if is_prerelease:
        if _ver_tuple(base) < _ver_tuple(version):
            return [
                f"tag {tag}: pre-release base {base} is older than "
                f"workspace version {version}"
            ]
        return []
    if base != version:
        return [
            f"tag {tag}: version {base} != workspace version {version}"
        ]
    return []


def _read(path):
    with open(path, encoding="utf-8") as fh:
        return fh.read()


def run_repo(tag):
    errors = check(
        _read(os.path.join(REPO, "Cargo.toml")),
        _read(os.path.join(REPO, "README.md")),
        _read(os.path.join(REPO, "CHANGELOG.md")),
        tag=tag,
    )
    if errors:
        print("Version consistency check failed:", file=sys.stderr)
        for e in errors:
            print(f"  - {e}", file=sys.stderr)
        return 1
    version = workspace_version(_read(os.path.join(REPO, "Cargo.toml")))
    suffix = f" (tag {tag})" if tag else ""
    print(f"Version consistent: {version}{suffix}.")
    return 0


def selftest():
    """Embedded fixtures exercising each match/mismatch path."""
    cargo = '[workspace.package]\nversion = "0.15.0"\nedition = "2021"\n'
    readme = "x\n**Current release: v0.15.0.** y\nSince v0.10.0 z\n"
    changelog = "## [Unreleased]\n\n## [0.15.0] - 2026-06-24\n\n### Added\n"

    cases = [
        ("all consistent", cargo, readme, changelog, None, []),
        (
            "tag matches",
            cargo,
            readme,
            changelog,
            "v0.15.0",
            [],
        ),
        (
            "readme stale",
            cargo,
            readme.replace("v0.15.0", "v0.12.2"),
            changelog,
            None,
            ["README.md"],
        ),
        (
            "readme marker missing",
            cargo,
            "no marker here\n",
            changelog,
            None,
            ["README.md"],
        ),
        (
            "changelog only unreleased",
            cargo,
            readme,
            "## [Unreleased]\n\n### Added\n",
            None,
            ["CHANGELOG.md"],
        ),
        (
            "changelog heading undated",
            cargo,
            readme,
            "## [0.15.0]\n",
            None,
            ["CHANGELOG.md"],
        ),
        (
            "final tag mismatches",
            cargo,
            readme,
            changelog,
            "v0.14.0",
            ["tag v0.14.0"],
        ),
        (
            # final tag of a future version still requires the bump
            "final future tag rejected",
            cargo,
            readme,
            changelog,
            "v0.16.0",
            ["tag v0.16.0"],
        ),
        (
            # pre-release of a future version: allowed pre-bump
            "prerelease future tag allowed",
            cargo,
            readme,
            changelog,
            "v0.16.0-rc.1",
            [],
        ),
        (
            # pre-release of the current version: allowed
            "prerelease current tag allowed",
            cargo,
            readme,
            changelog,
            "v0.15.0-rc.2",
            [],
        ),
        (
            # pre-release of an older version: rejected
            "prerelease older tag rejected",
            cargo,
            readme,
            changelog,
            "v0.14.0-rc.1",
            ["older than workspace"],
        ),
        (
            "cargo version missing",
            "[workspace]\nmembers = []\n",
            readme,
            changelog,
            None,
            ["Cargo.toml"],
        ),
        (
            # protocol `version: "1"` and feature mentions must not be
            # mistaken for the release marker.
            "ignores non-marker version mentions",
            cargo,
            'version: "1"\nSince v0.9.0 …\n'
            "**Current release: v0.15.0.**\n",
            changelog,
            None,
            [],
        ),
    ]

    failures = 0
    for name, c, r, cl, tag, expect in cases:
        errors = check(c, r, cl, tag=tag)
        if not expect:
            ok = errors == []
        else:
            ok = len(errors) == len(expect) and all(
                tok in err for tok, err in zip(expect, errors)
            )
        status = "ok" if ok else "FAIL"
        if not ok:
            failures += 1
        print(f"  [{status}] {name}: {errors}")

    if failures:
        print(f"selftest: {failures} case(s) failed", file=sys.stderr)
        return 1
    print(f"selftest: all {len(cases)} cases passed.")
    return 0


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--tag",
        help="release tag (e.g. v0.16.0); asserts it equals the "
        "workspace version",
    )
    parser.add_argument(
        "--selftest",
        action="store_true",
        help="run embedded fixtures instead of checking the repo",
    )
    args = parser.parse_args()
    if args.selftest:
        return selftest()
    return run_repo(args.tag)


if __name__ == "__main__":
    sys.exit(main())
