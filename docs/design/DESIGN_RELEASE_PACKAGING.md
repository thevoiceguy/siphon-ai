# Design: release & packaging — automated, signed, multi-arch releases

Status: **DECISIONS LOCKED (2026-06-24) — ready to implement** (forks locked
in §4; now chunked PRs, same cadence as config-CLI → v0.12.0 and
per-route-bridge-tls → v0.15.0).

Theme: **P0 from `docs/ROADMAP.md`** ("Production operability → Release &
packaging") — *"the most-flagged drift. Releases are hand-cut and there are
no installable artifacts beyond from-source scripts + a Docker image."*

---

## 1. The gap today

Cutting a release is a **manual, multi-step ritual** (just exercised for
v0.15.0): bump `[workspace.package].version`, refresh `Cargo.lock`, move the
`CHANGELOG.md` `[Unreleased]` block under a dated heading, commit, annotated
tag, push, `gh release create --latest`. Nothing is automated, nothing is
verified, and the only artifact a consumer gets is "build it yourself" or an
**unpublished** Docker image. Concretely:

- **No prebuilt binaries.** Every operator compiles from source
  (`scripts/install-debian13.sh`) or builds the image locally. There is no
  `x86_64` binary — let alone `aarch64` — attached to a GitHub release.
- **No published container.** `docker/Dockerfile` produces a fine
  musl-static `x86_64` image, but it's built **by hand** and pushed nowhere
  (no GHCR). It's also `x86_64`-only and its toolchain has **already
  drifted**: it pins `FROM rust:1.85-alpine` while `rust-toolchain.toml` is
  `1.95.0` — a gap an automated, `--locked` release build would have caught.
- **No supply-chain artifacts.** No SBOM, no `SHA256SUMS`, no signatures.
  Nothing an operator (or their security team) can verify a download against.
- **No native packages.** No `.deb`/`.rpm`; the install scripts drop a
  systemd unit but there's no versioned, upgradeable package.
- **Version drift is unguarded — and live right now.** `README.md:45` reads
  *"Current release: v0.12.2"* while the actual latest tag is **v0.15.0**
  (three minor versions stale). The roadmap calls this out: *"this repo has
  already drifted twice."* Nothing fails the build when `Cargo.toml`,
  `CHANGELOG.md`, and `README.md` disagree.

The good news: the build itself is already reproducible (`--locked`, a pinned
`rust-toolchain.toml`, a working musl-static Dockerfile). This theme is mostly
**CI plumbing + a consistency gate**, not new daemon code. Zero changes to the
call path, protocol, or config schema.

## 2. Goals / non-goals

**Goals**
1. **Version-consistency CI gate** — fail every PR/push if the workspace
   version, `CHANGELOG.md`, and `README.md` disagree. The cheap quick-win;
   fixes the live README drift and prevents recurrence.
2. **Automated release workflow** — pushing a `v*` tag builds, packages,
   signs, and publishes everything: multi-arch binaries + checksums +
   signatures + SBOM attached to the GitHub release, and a multi-arch
   container pushed to GHCR. Cutting a release becomes "tag and push."
3. **Multi-arch prebuilt binaries** — `x86_64` **and** `aarch64`, musl-static,
   stripped, attached to each release with `SHA256SUMS` + signatures.
4. **Native package** — a `.deb` at minimum (carrying the binary + systemd
   unit + a default config), versioned and upgradeable.
5. **Published container** — multi-arch (`linux/amd64` + `linux/arm64`) image
   on GHCR, tagged by version + `latest`, with the runtime toolchain tracking
   `rust-toolchain.toml`.

**Non-goals (this theme)**
- **No daemon/protocol/config/CDR changes.** Pure build + CI + docs. Protocol
  stays v1.
- **No `.rpm` in the first cut** (roadmap marks it a stretch) — but the
  packaging step is structured so adding it later is a sibling job, not a
  rework.
- **No distroless/hardened image in the first cut** (roadmap stretch) — the
  Dockerfile runtime stage is already swappable (`alpine:3` → `distroless/
  static`); we make that a documented one-line follow-up, not blocking work.
- **No release-notes auto-generation.** The hand-curated `CHANGELOG.md` block
  remains the source of the GitHub release body (the workflow *extracts* it,
  doesn't invent it).
- **No crates.io publishing.** SiphonAI ships as a daemon/image, not a
  library; the workspace crates stay unpublished.

## 3. Design

### 3.1 Single source of truth + the consistency gate (chunk 1)

`[workspace.package].version` in the root `Cargo.toml` is **canonical**.
Everything else must agree with it:

- `CHANGELOG.md` must have a `## [<version>] - <date>` heading (not just
  `[Unreleased]`) for a tagged build, and a matching one for the workspace
  version on every push.
- `README.md`'s "Current release: vX.Y.Z" line must equal the workspace
  version.

A new `scripts/check-version-consistency.py` (stdlib-only, mirroring
`scripts/check-doc-links.py`) parses all three and exits non-zero on any
mismatch. It runs as a new fast job in `test.yml` (alongside `doc-links`), so
the README-vs-Cargo drift that exists *today* would turn the build red until
fixed. On a tag build (chunk 2) the same script additionally asserts the tag
equals the workspace version and that the CHANGELOG heading is dated (not
`[Unreleased]`).

This chunk also **fixes the current drift** (README → v0.15.0) so the gate
goes green on introduction.

### 3.2 Release workflow shape (chunk 2)

A new `.github/workflows/release.yml`, triggered on `push: tags: ['v*']`
(matching the existing hand-tag step — no new human action) with a
`workflow_dispatch` escape hatch for re-runs. Stages:

1. **Preflight** — `check-version-consistency.py --tag "$GITHUB_REF_NAME"`;
   reuse `test.yml`'s lint+test (or `needs:` a reusable workflow) so a release
   can't be cut from a red commit.
2. **Build matrix** — `{x86_64, aarch64}-unknown-linux-musl`, `--release
   --locked -p siphon-ai`, stripped. Cross-compilation approach is **decision
   3** below (`cross` vs `cargo-zigbuild` vs native arm64 runners).
3. **Package** — per arch: the raw binary, a `.tar.gz`, and (x86_64 + arm64)
   a `.deb` via `cargo-deb` (decision 4). Emit a combined `SHA256SUMS`.
4. **Sign + SBOM** — sign the checksums + artifacts and generate an SBOM
   (decision 5: cosign keyless + syft is the recommended default).
5. **Container** — `docker buildx` multi-arch (`linux/amd64,linux/arm64`)
   build of `docker/Dockerfile`, pushed to `ghcr.io/thevoiceguy/siphon-ai`
   tagged `:vX.Y.Z` + `:latest`, image signed with the same cosign identity.
   This is also where the Dockerfile's `rust:1.85-alpine` pin gets corrected
   to track `rust-toolchain.toml` (a build-arg or a CI check).
6. **Publish** — create/update the GitHub release for the tag, body extracted
   from the matching `CHANGELOG.md` section, all artifacts + `SHA256SUMS` +
   signatures + SBOM attached, marked `--latest`.

The daemon's manual tag step stays the trigger; everything after `git push
origin vX.Y.Z` becomes automated.

### 3.3 Multi-arch & the musl-static guarantee

Both targets are `*-musl` so the existing fully-static-binary property (runs
on any Linux ≥ 3.2, no glibc/musl ABI surprises — already relied on by the
Dockerfile) extends to `aarch64` and to the `.deb` payload unchanged. The
container's `buildx` arm64 layer uses the same target, so the published image
and the standalone binary are the *same* artifact per arch.

### 3.4 Native package (`.deb`)

`cargo-deb` driven by a `[package.metadata.deb]` block on the `siphon-ai`
bin crate: installs the binary to `/usr/bin`, the systemd unit (lifted from
`scripts/install-debian13.sh`) to `/lib/systemd/system`, and a default config
+ `siphon` system user via maintainer scripts. The existing from-source
install script stays for non-Debian hosts but the README points Debian/Ubuntu
operators at `apt install ./siphon-ai_X.Y.Z_*.deb`.

## 4. Decisions — LOCKED (2026-06-24)

1. **Release trigger = tag-push (`v*`) + `workflow_dispatch` fallback.**
   Pushing the annotated tag (today's manual final step) runs the full
   release; `workflow_dispatch` is the re-run escape hatch. No new human
   action versus the current flow.
2. **First-cut scope = full bundle.** Consistency gate + multi-arch binaries +
   `SHA256SUMS` + cosign signatures + SBOM + GHCR container + `.deb`.
   Everything in §2 Goals ships in this theme; only `.rpm` and the distroless
   variant are deferred.
3. **Cross-compile = `cargo-zigbuild`.** One x86 runner cross-builds both
   `x86_64-` and `aarch64-unknown-linux-musl` via the zig linker — no QEMU, no
   Docker-in-CI, fast matrix. (`cross` and native arm64 runners rejected:
   slower / costlier.)
4. **Packaging = `cargo-deb`** driven by `[package.metadata.deb]` on the
   `siphon-ai` bin crate. `.rpm` (`cargo-generate-rpm`) stays **deferred** to
   a follow-up; the packaging job is structured so `.rpm` is a sibling step,
   not a rework.
5. **Signing + SBOM = cosign keyless + syft.** Sigstore keyless signing via
   GitHub OIDC (no key to store/rotate) for both the `SHA256SUMS`/artifacts
   and the GHCR image; `syft` emits a CycloneDX SBOM attached to the release.
6. **Distroless runtime = deferred.** First cut keeps the `alpine:3` runtime
   stage; swapping to `distroless/static` stays a documented one-line
   follow-up (the Dockerfile already notes it).

(Defaults taken as locked: source of truth = workspace `Cargo.toml` version,
musl-static for all targets, GHCR registry, CHANGELOG-extracted release
notes.)

## 5. Observability / tests

No daemon code, so no new metrics. Verification is CI-side:

- `check-version-consistency.py` gets a unit/self-test over fixture
  Cargo/CHANGELOG/README triples (match → exit 0; each kind of mismatch →
  exit 1), mirroring how `check-doc-links.py` is exercised.
- The release workflow is validated on a **pre-release tag** (e.g.
  `v0.16.0-rc.1`) end-to-end before the real tag: confirm both-arch binaries
  run (`--version`), `SHA256SUMS` verifies, the signature verifies with
  `cosign verify`, the SBOM lists the expected crates, the GHCR manifest is
  multi-arch (`docker buildx imagetools inspect`), and the `.deb` installs +
  the unit starts in a throwaway container.

## 6. Chunks (target ~v0.16.0)

1. **Version-consistency gate.** `scripts/check-version-consistency.py` + new
   `test.yml` job; **fix the live README drift to v0.15.0**; self-tests. The
   cheap quick-win — lands independently and immediately stops recurrence.
2. **Release workflow — binaries.** `release.yml` on `v*`: multi-arch
   musl-static build matrix, `.tar.gz` + `SHA256SUMS`, attached to the GitHub
   release with CHANGELOG-extracted notes. Validated on an `-rc` tag.
3. **Supply chain + container.** cosign signing + syft SBOM on the artifacts;
   multi-arch `buildx` build pushed to GHCR (`:vX.Y.Z` + `:latest`, signed);
   correct the Dockerfile toolchain pin to track `rust-toolchain.toml`.
4. **Native package.** `cargo-deb` metadata + `.deb` built and attached;
   systemd unit + default config + `siphon` user wired through maintainer
   scripts.
5. **Docs + release.** `docs/DEPLOY.md` (install-from-release: binary, `.deb`,
   container with `cosign verify` + checksum steps), a `RELEASING.md` runbook
   describing the now-minimal "tag and push" flow, CHANGELOG, tag ~v0.16.0 —
   the first release cut *by* the new workflow.
