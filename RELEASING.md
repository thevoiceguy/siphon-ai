# Releasing SiphonAI

Cutting a release is **"bump, then tag and push."** The
[`release`](.github/workflows/release.yml) workflow does the rest: it builds
multi-arch static binaries, Debian packages, an SBOM, and a signed container,
and publishes a GitHub release. This runbook is the human half.

See [`docs/design/DESIGN_RELEASE_PACKAGING.md`](docs/design/DESIGN_RELEASE_PACKAGING.md)
for the design and the locked decisions.

## The single source of truth

The workspace `Cargo.toml` `[workspace.package].version` is canonical. The
`version consistency` CI gate (on every PR/push) fails the build unless:

- `README.md`'s `**Current release: vX.Y.Z**` marker equals it, and
- `CHANGELOG.md` has a dated `## [X.Y.Z] - YYYY-MM-DD` section for it.

So the three move together, in one commit, at release time — never drift.

## Cutting a final release

1. **Land all the work** for the release on `main` (entries accumulate under
   `## [Unreleased]` in `CHANGELOG.md`).

2. **Release-prep commit** (on a branch → PR → merge), conventionally
   `chore(release): X.Y.Z — <headline>`:
   - bump `[workspace.package].version` in `Cargo.toml` (and refresh
     `Cargo.lock` with a build);
   - in `CHANGELOG.md`, rename `## [Unreleased]` to `## [X.Y.Z] - <today>` and
     start a fresh empty `## [Unreleased]` above it;
   - update the `README.md` `**Current release: vX.Y.Z**` marker.

   Verify locally before pushing:
   ```sh
   python3 scripts/check-version-consistency.py
   python3 scripts/check-version-consistency.py --tag vX.Y.Z   # tag == version
   python3 scripts/extract-changelog.py X.Y.Z                  # notes exist
   ```

3. **Tag the merged release commit and push:**
   ```sh
   git checkout main && git pull
   git tag -a vX.Y.Z -m "vX.Y.Z — <headline>"
   git push origin vX.Y.Z
   ```

   That's the trigger. The `release` workflow then:
   - **preflight** — re-asserts `tag == workspace version` + a dated CHANGELOG
     section (a mistagged commit never builds);
   - **build** (`x86_64` + `aarch64` musl, cargo-zigbuild) → `.tar.gz`;
   - **`.deb`** (cargo-deb, `amd64` + `arm64`);
   - **publish** — CycloneDX SBOM (syft), `SHA256SUMS` over everything, a
     cosign keyless signature over the checksums, and a GitHub release marked
     **Latest**, with notes extracted from `CHANGELOG.md`;
   - **container** — multi-arch image to `ghcr.io/thevoiceguy/siphon-ai:vX.Y.Z`
     and `:latest`, cosign-signed.

4. **Verify the published release** (see `docs/DEPLOY.md` → *Install from a
   release*): `sha256sum -c SHA256SUMS`, `cosign verify-blob …`, and
   `cosign verify ghcr.io/thevoiceguy/siphon-ai:vX.Y.Z …`.

## Pre-releases (`-rc.N`)

A tag with a pre-release suffix — `vX.Y.Z-rc.1` — exercises the whole
pipeline **without** a version bump: preflight allows a pre-release whose base
version is the current or a future version, the release is marked
**pre-release** (never `:latest` / never Latest), and the notes fall back to
the base CHANGELOG section (or a generic line). Use it to validate workflow
changes; delete the throwaway tag/release/image afterward.

## Re-running

`release` also has a `workflow_dispatch` trigger taking an existing tag —
use it to re-run publishing for a tag without re-tagging (the release is
created if absent, or its assets/notes updated in place).

## What needs no manual step

Versions in crate `Cargo.toml`s (they inherit `version.workspace = true`),
the SBOM, signatures, checksums, and the container — all produced by the
workflow. Don't hand-build or hand-upload release artifacts.
