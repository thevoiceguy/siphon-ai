# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **`crates/webrtc-glue` + `[webrtc]` config** — the foundation of
  Phase 2 in `docs/design/DEV_PLAN_WebRTC.md`, and the answer to that
  plan's top risk. The new crate holds the §4.1 leg-selection rule
  (**transport selects eligibility, SDP shape selects the leg**) and
  the `[webrtc]` → forge-webrtc `PeerConfig` mapping. Everything is
  checked against the **real Chrome offer captured off the wire**
  during the Phase 1 browser run — kept verbatim as a fixture — and
  forge-webrtc answers it correctly: right DTLS role for
  `a=setup:actpass`, mirrored `mid`/BUNDLE/`rtcp-mux`, our own
  fingerprint, and exactly one pinned codec (Opus by default; PCMU
  with `prefer_g711`, which makes the whole call 8 kHz end-to-end and
  skips transcoding). Two findings the plan did not anticipate are
  pinned by tests: Chrome's host candidate is an **mDNS `.local`
  name** (a server without an mDNS resolver needs the peer's srflx
  candidate — an ICE failure from this otherwise looks like a network
  problem), and a DTLS-SRTP offer *without* ICE (some SBCs) must stay
  on the classic path, which is why ICE — not the profile — is the
  discriminator. Behind a `webrtc` cargo feature: a plain
  `cargo build` links neither forge-webrtc nor forge-ice, while a
  config that enables it on a binary built without it now fails
  **both at startup and at `siphon-ai check`** (the pre-upgrade gate
  previously only compiled the config, so it could pass where boot
  would fail). No media yet — offer/answer lifecycle and the audio
  path are next.

- **`examples/browser-sip/headless-check.sh` — the Phase 1 exit check,
  self-driving on a browserless box**, and the check itself **passed**
  (recorded in `DEV_PLAN_WebRTC.md` §3.3): headless Chromium + SIP.js
  digest-REGISTERed over WSS with Origin enforcement, the binding
  expired via connection-loss grace when the browser died, the
  exchange appeared in Homer with `Via: SIP/2.0/WSS`, and the
  browser's test INVITE authenticated, routed, and drew the precise
  `488 offer profile UDP/TLS/RTP/SAVPF rejected` that marks Phase 2's
  seam. The script needs no root: system `chromium` if present, else
  Playwright's download with the two missing NSS libraries fetched via
  `apt-get download` into a local dir. The page loads SIP.js as
  jsdelivr's ESM build (npm ships no UMD bundle — the unpkg `dist/`
  path 404s) and gains `?auto=1` / `?auto=1&call=1` self-driving
  modes.

- **`examples/browser-sip/`** — the hands-on Phase 1 exit check for
  `docs/design/DEV_PLAN_WebRTC.md`: a SIP.js test page, a lab config
  (`[sip.wss]` + `[sip.auth]` + `[registrar]` with an Origin
  allow-list), and a mkcert-or-openssl certificate script. Five
  minutes from checkout to a browser tab REGISTERed over WSS, binding
  visible in `siphon_ai_registrar_bindings` and expiring when the tab
  closes; optional Homer leg via `examples/homer-stack/`. The whole
  path short of the browser itself is machine-verified: a WSS probe
  (self-signed TLS, page Origin, digest 401→200→unregister, wrong
  origin refused 403) runs against the exact lab config.
  `ws_sip_call.py` gained a `__main__` guard so the probe logic is
  importable.

- **`[registrar]` — the daemon serves REGISTER** (Phase 1 of
  `docs/design/DEV_PLAN_WebRTC.md` §3.2; distinct from `[[register]]`,
  which is client-side). Thin adapter over upstream `sip-registrar`
  (`BasicRegistrar` does header validation, digest auth with the
  AOR-to-identity authorization step — user A cannot register user
  B's AOR — expires clamping, and the location store); what siphon-ai
  adds is connection awareness: a registration arriving over a stream
  transport (WS/WSS/TCP/TLS) is bound to its connection — the only
  route back to a browser — and expired ~32 s after that connection
  dies (a reload re-REGISTERs inside the window). Off by default;
  enabling it without `[sip.auth]` is a load error unless the
  explicit `allow_unauthenticated` lab flag is set. Credentials are
  `[sip.auth]`'s users, so INVITE and REGISTER share one set. New
  metrics: `siphon_ai_registrar_registers_total{result}` (alert on
  `forbidden` — someone's credentials claiming someone else's
  identity) and `siphon_ai_registrar_bindings`. The suite gains a
  `ws_registrar` phase driving the SIP.js-shaped flow over WS:
  REGISTER → 401 digest challenge → authenticated REGISTER → 200 →
  `Expires: 0` unregister. siphon-rs pinned to `v2026.08.25`
  (sip-uas 0.5.0: `on_register` now receives the `TransportContext`,
  [siphon-rs #127](https://github.com/thevoiceguy/siphon-rs/pull/127)).

- **SIP over WebSocket listeners (RFC 7118)** — Phase 1 of
  `docs/design/DEV_PLAN_WebRTC.md`. `[sip].transports` accepts `"ws"`
  and `"wss"`, each with its own listener block: `[sip.ws]` /
  `[sip.wss]` (`listen`, `cert`/`key` for WSS, `allowed_origins`).
  Upgrades must offer the `sip` subprotocol; a non-empty
  `allowed_origins` refuses unlisted (or absent) `Origin`s `403` at
  the upgrade, pinning which pages may open signalling (browsers
  always send it; `[sip.auth]` remains the real authentication).
  Replies and in-dialog requests to a WS client route down the
  connection the client established — browser Via/Contacts are
  unroutable, so there is deliberately no dial-back fallback. Off
  unless configured; the WSS cert is deliberately independent of
  `[sip.tls]` (browsers must trust it) and rotating it wants a
  restart. The signalling suite gains a `ws_signaling` phase
  (`ws_sip_call.py` — SIPp can't speak RFC 7118): a full
  INVITE/ACK/BYE call over one WS connection, origin-refusal
  assertions, and the settle-to-zero check. Verified end-to-end:
  OPTIONS and a full call answered over WS, wrong/absent origin
  refused, teardown clean. siphon-rs pinned to `v2026.08.24.1`
  (sip-transport 0.5.0: `WsAcceptPolicy` + WS idle timeout,
  [siphon-rs #125](https://github.com/thevoiceguy/siphon-rs/pull/125));
  the `ws` feature joins `tls` on the sip-transport dependency.

- **RTP port-pool gauges + a teardown-soak harness phase** (Phase 0 of
  `docs/design/DEV_PLAN_WebRTC.md`). Two new gauges,
  `siphon_ai_rtp_port_pairs_allocated` and `_capacity`, published every
  2 s by a sampler that reads the pool itself — deliberately not
  incremented at alloc/free sites, since a site-updated gauge
  under-counts under exactly the leak it exists to catch. Allocated
  diverging from `siphon_ai_calls_active` is a leaked media session;
  alert on it. The SIPp suite gains a `teardown_soak` phase: twelve
  calls across clean-BYE / CANCEL / abrupt-vanish (established dialog,
  peer exits with no BYE — the browser-shaped teardown) over UDP and
  TCP, then an assertion that live calls, the dialog store, and
  allocated port pairs all return to zero. Lab result on this baseline:
  eleven distinct teardown shapes (including delayed-offer and no-ACK
  vanishes, and with the inactivity watchdog disabled) all settle to
  zero — teardown is currently defense-in-depth (media watchdog,
  forge's own session sweep, dialog reaper, BYE-failure-tolerant
  teardown), and the phase exists to keep it that way once a WebRTC
  leg multiplies the abrupt-teardown rate.

### Changed

- **forge-media pinned by tag: `v2026.08.24`** (was `rev = "c277860cd123"`) —
  forge-media's first tagged release under its new
  [RELEASING.md](https://github.com/thevoiceguy/forge-media/blob/main/RELEASING.md)
  (the siphon-rs model, adopted in
  [forge-media #131](https://github.com/thevoiceguy/forge-media/pull/131)).
  Both upstream pins now read as tags, and this pair is self-consistent by
  construction: forge-media `v2026.08.24` embeds siphon-rs `v2026.08.24`,
  the same tag `[workspace.dependencies]` pins directly. Content over the
  old rev: forge-webrtc 0.4.0's G.711 negotiation
  ([forge-media #130](https://github.com/thevoiceguy/forge-media/pull/130))
  and crate-version stamps (forge-ice 0.3.0, forge-rtp 0.3.0) — none of
  which siphon-ai compiles today (forge-webrtc is not yet a dependency;
  the version bumps are manifest-only for the crates we use), so no
  behaviour change.

## [0.50.1] - 2026-08-24

**Two observability blind-spot fixes and routine upstream pin rolls.**

Both fixes came out of validating the 0.50.0 getting-started flow: the
`[[trunk]]` gate's 403s — the exact scanner traffic the gate exists to
shed — were invisible on `/metrics`, and four 0.49.0 admin endpoints
reported as `endpoint="unknown"`. No protocol change (still `1`), CDR
still v8, no new config keys. Drop-in upgrade; alerting can now key on
`siphon_ai_invites_total{result="rejected_trunk"}`.

### Changed

- **siphon-rs pinned to `v2026.08.24`** (from `v2026.08.21`) — picks up
  [sip-identity 0.3.0](https://github.com/thevoiceguy/siphon-rs/pull/124):
  PASSporT **signing** behind a new `sign` feature
  ([siphon-rs #123](https://github.com/thevoiceguy/siphon-rs/pull/123)),
  the provider-side complement to the ES256 verification SiphonAI already
  uses. SiphonAI does not enable the feature, so verification behaviour is
  unchanged — this makes outbound STIR/SHAKEN signing (long demand-gated)
  buildable against the current pin when it's picked up.
- **forge-media pinned to `c277860cd123`** (from `1fe94a502efa`) — one
  commit: aligns forge-media's own siphon-rs submodule to `v2026.08.24`,
  keeping both pins on the same upstream revision (the media path sees
  siphon-rs through that submodule).

### Fixed

- **`[[trunk]]`-gate 403s now count in `siphon_ai_invites_total` as
  `result="rejected_trunk"`**
  ([#564](https://github.com/thevoiceguy/siphon-ai/issues/564)). The allowlist gate runs in the SIP
  dispatch layer before the acceptor, whose paths owned every increment of
  that counter — so the INVITEs the gate sheds (scanner traffic hammering
  the SIP port from non-allowlisted sources) were invisible on `/metrics`
  and unalertable without log-pipeline tooling. Per-peer detail stays in
  the audit stream and SIP ring; the new label is alertable directly.
  Digest-auth 401s are deliberately still not in this counter (a challenged
  INVITE retries with credentials and is counted then) — that gate's
  brute-force signal remains `siphon_ai_sip_auth_total{result="failed"}`.

- **Four admin endpoints no longer report as `endpoint="unknown"` in
  `siphon_ai_admin_requests_total`**
  ([#565](https://github.com/thevoiceguy/siphon-ai/issues/565)):
  `GET /admin/v1/status`, `GET /admin/v1/errors`,
  `GET /admin/v1/cdrs/recent`, and `POST /admin/v1/drain` — the 0.49.0
  sightglass endpoints — got authorization arms but no metric-label
  arms, so their traffic lost per-endpoint visibility and polluted the
  `unknown` probing signal dashboards key on. The static route table is
  now a single shared source (`STATIC_ROUTES`) answering both
  `min_role` and `route_label`, so a route can no longer be known to
  authorization yet invisible to metrics.

## [0.50.0] - 2026-08-23

**One new config key and one dependency bump that reaches the wire.**

`[media].reserved_outbound_calls` closes the last gap the load plan found:
inbound and outbound calls draw RTP ports from one pool, first-come-first-served,
so on a node that both answers and originates an inbound surge could starve
origination *completely* with no inbound-side symptom. Set it and the inbound
allocator stops at a floor, leaving the rest to origination. Default `0` is the
old unreserved behaviour, so **upgrading changes nothing until you set it**.

⚠️ **DTLS-SRTP deployments read the forge-media bump below before upgrading.**
The certificate moves from RSA-2048 to ECDSA P-256, which is live wherever
`[media].srtp_offer = "dtls"` is set. Almost every peer prefers P-256 already —
it is what browsers present — but a peer that can only do RSA key exchange
would newly fail.

**No protocol change** (still `1`), CDR still v8. Adds one config key, two
metrics, and a `--inspect-config` line. Wants a restart to take effect.

### Changed

- **forge-media pinned to `1fe94a502efa`** — nine commits, three of them
  filed from this project. [#120](https://github.com/thevoiceguy/forge-media/pull/120)
  adds `PortPool::allocate_reserving(min_free)` and
  `MediaSessionConfig::min_free_port_pairs`, which is what makes
  `[media].reserved_outbound_calls` above an *exact* floor: the check now
  happens inside the allocator's own critical section instead of as a
  free-pair read taken just before it, so concurrent INVITE setup can no
  longer dip below the reservation. Both land in the same release, so no
  published build ever had the softer version.
  [#122](https://github.com/thevoiceguy/forge-media/pull/122) fixed
  forge's broken-link CI step (it grepped for a word rustdoc never
  prints, so it had passed unconditionally while eleven unresolved links
  accumulated) and [#123](https://github.com/thevoiceguy/forge-media/pull/123)
  gave `forge-vad` a types-only `forge-core` dependency so two of them
  could be live links. Neither changes anything we compile.

  **One upstream change reaches our wire without us asking for it:**
  forge-media #117 + #118 move the DTLS-SRTP certificate from **RSA-2048
  to ECDSA P-256** and offer ECDHE-ECDSA first. We build `forge-engine`
  with `dtls`, so this is live wherever `[media].srtp_offer = "dtls"`
  (0.9.4) is set. The SDP `a=fingerprint` stays `sha-256` — only the key
  type, and therefore the hash value, change — and P-256 is what browsers
  and webrtc-rs present, so it is the more interoperable default as well
  as ~100× faster to generate. A peer that can only do RSA key exchange
  would now fail where it previously worked. The `delayed_offer_dtls`
  SIPp scenario covers the handshake.

  The remaining commits (#116, #119, #121) are WebRTC/ICE work in
  `forge-webrtc` / `forge-ice`, neither of which we depend on. #115 bumps
  forge-media's own siphon-rs submodule to v2026.08.19; we pin siphon-rs
  directly at v2026.08.21 and the two stay separate package sources, but
  the version skew closed — forge's vendored `sip-sdp` goes 0.3.0 → 0.3.1,
  matching what our tag carries.

### Added

- **`[media].reserved_outbound_calls`: hold RTP ports back for
  origination** (#556). Inbound and outbound draw port pairs from one
  pool, first-come-first-served, with no reservation between them, so on
  a node that both answers and originates an inbound surge starves
  origination *completely* — and there is no inbound-side symptom while
  it happens. Measured on 0.49.9 with the pool shrunk to 60 calls and 50
  inbound + 20 outbound asked for: inbound established **50/50** and
  stayed healthy for the whole window while **10 of 20 originates
  failed** (`test-harness/load/RESULTS-0.49.9-mixed-and-soak.md` §2). The
  starved direction is usually the one with a deadline attached — a
  scheduled callback or a notification, versus a caller who will redial —
  so the default allocation is the opposite of how an operator would
  prioritise it, and the only lever was `[sip.admission]`: a global
  inbound concurrency cap standing in for a pool reservation, with the
  two numbers drifting apart the moment `rtp_port_range` changed.

  Set `reserved_outbound_calls = N` and the **inbound** allocator refuses
  once the pool's free pairs reach `N`, leaving the rest to origination,
  which is not gated. Counted in calls, not ports (one call is one pair).
  The refusal reuses the exhausted-pool path from #554/#555 unchanged —
  `503 Service Unavailable` + `Retry-After`, counted as
  `siphon_ai_invites_total{result="rejected_capacity"}` — deliberately:
  a caller must not be able to tell "we are empty" from "the rest is
  spoken for", since both mean try another node. The split is
  operator-side, on the new **`siphon_ai_rtp_reserve_blocks_total`**
  (published as a zero baseline, so "the reserve has never had to shed"
  reads as `0` rather than as a missing series) alongside
  **`siphon_ai_rtp_reserved_outbound_calls`**, the configured threshold as
  a gauge. Startup logs the split (`pool_calls` / `reserved_outbound_calls`
  / `inbound_calls`) and `--inspect-config` prints it.

  Default `0` — unreserved, byte-for-byte the pre-0.50 behaviour.
  Restart-required. A value at or above the pool's capacity fails at load
  with the capacity in the message: reserving the whole pool would answer
  `503` to every INVITE forever, which is a concurrency cap wearing a
  media knob's clothes. **The floor is exact** — it is evaluated inside
  the RTP pool allocator's own critical section, so no amount of
  concurrent INVITE setup can dip below it and `N` needs no slack (that
  took an upstream change; see the forge-media bump below). The one
  caveat, in `docs/CONFIG.md`: the knob decides *who loses* when the pool
  is too small, it does not make it bigger. `rtp_port_range` must still
  be sized for the sum of both directions; `docs/DEPLOY.md` now says so
  under its own heading, with the measurement and the reason an operator
  watching HTTP status codes sees none of it (`POST /admin/v1/calls`
  answers `202` and the port failure arrives later on the
  `outbound_failed` webhook).

- **The SIPp harness asserts the outbound delayed-offer counter, not just
  "answered"** (#558). The `outbound_delayed_uas` phase drives a full
  RFC 3264 delayed offer — offerless INVITE out, the peer's offer in its
  2xx, our answer in the ACK — but asserted only
  `siphon_ai_outbound_calls_total{result="answered"}`, which *any*
  originate satisfies. `siphon_ai_outbound_delayed_offer_total` (#406) is
  the counter that says the negotiation itself succeeded, and nothing in
  CI asserted it, so its documented result set could rot unnoticed. The
  scenario's own `check_it` proves the ACK carried an m-line; this adds
  the daemon's side of the same claim, and catches an originate that
  answers while the negotiation lands on `missing_sdp_offer`,
  `invalid_remote_media`, `media_activate` or an `srtp_*` label.

## [0.49.10] - 2026-08-22

One fix to the **inbound** call path: a full RTP port pool is a capacity
condition, and it now answers like one. Relevant to any node that can run
out of ports — most sharply to a node that also originates, since both
directions draw from the same pool.

**No config change, no protocol change** (still `1`), CDR still v8. Adds
one metric label. Wants a restart to take effect.

### Fixed

- **An exhausted RTP port pool now answers `503 Service Unavailable` +
  `Retry-After`, not `500 Server Internal Error`** (#554). A full pool is a
  capacity condition — the daemon is working exactly as configured and is
  simply out of ports — but `500` tells the peer we have a defect. RFC 3261
  §21.5.4 reserves `503` for temporary overload, a proxy that receives one
  SHOULD try the next server in its target set, and carrier SBCs treat the
  two very differently. It was also inconsistent with this daemon's own
  overload signalling: `[sip.admission]` and the drain path already answer
  `503` + `Retry-After`, so alerting built on "503 means siphon-ai is full"
  got an internal-error page instead.
  * `forge-core` has always distinguished `ForgeError::ResourceLimit`;
    `media-glue` was flattening every forge error into one stringly
    `SetupError::Session`, so the acceptor could not tell capacity from a
    fault. The type now survives the conversion.
  * The delayed-offer path hardcoded `500` and the flat `rejected` label
    instead of consulting the shared status table, so a delayed-offer
    caller got a different answer to the same condition. It routes through
    the table now.
  * New metric label **`siphon_ai_invites_total{result="rejected_capacity"}`**,
    separately alertable from `rejected`. On a node that also originates the
    pool is shared with outbound, so this counter is the only inbound-side
    signal that a surge in the other direction is refusing calls — see
    `test-harness/load/RESULTS-0.49.9-mixed-and-soak.md` §2.1. That pool
    has no reservation between the two directions, which is tracked
    separately as #556; this release changes only the answer given, not
    who gets a port.

### Documentation

- **The load test plan is complete — outbound origination, a live-carrier
  tier, both directions at once, and a churning soak** (#553). Everything
  through §11 loaded *inbound* only, which is how a per-originated-call
  dialog leak (#548) reached production unnoticed; the plan now has a §12
  outbound phase, §13 mixed-direction phase, and the tier-3 live run its
  three tiers always promised. Three results documents land with it:
  - `RESULTS-0.49.9-outbound.md` — origination under load. `fds = 13 + 3N`
    and two RTP ports per originated leg; a 60-minute churning soak
    (2,995 placed / 2,995 completed / 0 failed / 0 WARN) whose RSS growth
    is **arena, not a leak** — the same drained process absorbed 750 more
    calls at 1.9 KB each where a leak needed ~16 MB.
  - `RESULTS-0.49.9-tier3.md` — 60 sequential calls over a live Twilio
    trunk (TLS + SRTP), out through the carrier and back in to the same
    node. MOS p50 **4.435**, jitter p50 **1.94 ms**, loss **0.001 %**,
    setup p95 **460 ms**. It also proves the tier-2 netem model
    *pessimistic*: live jitter 1.94 ms against netem's 3.35, live loss
    0.001 % against 0.495 %.
  - `RESULTS-0.49.9-mixed-and-soak.md` — the two directions compose
    linearly (`fds = 13 + 3N`, `udp_sockets = 2N + 1`, N the *total*), and
    the shared RTP pool is first-come-first-served with **no reservation
    between them** (#556). Running the exhaustion test in the reverse
    order is what found #554, fixed above.

## [0.49.9] - 2026-08-21

Two fixes to the **outbound** call path, both found while verifying the
0.49.8 deploy and both older than it. Neither affects a node that only
accepts inbound calls; both matter to one that originates.

**No config change, no protocol change** (still `1`), CDR still v8. Wants
a restart to take effect.

### Fixed

- **In-dialog requests on outbound legs stop swapping their `From` URI
  mid-dialog** (#549), via siphon-rs `v2026.08.20.1` → `v2026.08.21`
  (its #121, opened from here). A UAC dialog's local URI now comes from
  the dialog-forming request's `From` rather than the client's configured
  identity, per RFC 3261 §12.2.1.1. Every outbound leg sets a per-call
  `From` — a `[[gateway]]`'s `from`, or its `[[register]]` AOR, is the
  caller-ID — so our BYE went out as `sip:siphon@<public_address>` on a
  dialog the INVITE had opened as the AOR, same tag. FreeSWITCH tolerated
  it (it matches on Call-ID + tags); a URI-validating peer answers `481`
  and the leg then hangs to the far end's media timeout, and From-based
  CDR correlation splits one call across two identities. The same release
  stamps `User-Agent` on the 15 of 27 upstream request builders that never
  did — our ACK and BYE carried none, so 0.49.8's product token covered
  only part of a call's traffic. Tag bump only; no API change, no config
  change, sip-uac 0.7.0 → 0.7.1.

- **Outbound calls no longer leak a dialog apiece** (#548). Gateway UACs
  are built with the UAS's `DialogManager` (#324), so an originated
  call's confirmed dialog lands in the shared store — but only the
  inbound teardown retired one, so the store grew by one entry per
  originated call for the life of the process and
  `siphon_ai_dialogs_active` never came back down. Past `sip-dialog`'s
  `MAX_CONFIRMED_DIALOGS` (10,000) `insert` fails silently and in-dialog
  requests stop matching — for inbound calls too, since the store is
  shared. The outbound teardown now retires its dialog after the BYE
  exchange, on the same deferred grace window the inbound path uses
  (#458), whichever side hung up. Present since #458 first shipped in
  0.48.13; a node that only accepts inbound calls was never affected.

## [0.49.8] - 2026-08-21

`[sip].user_agent` finally does what this repo has documented it doing
since it was added, a `[[register]]` block can no longer be silently
dead, and the SIPp harness stops reporting its own port collisions as
product regressions. Three siphon-rs releases are absorbed along the
way, two of them cut for fixes this project filed.

**Upgrade note: this changes what your node says on the wire**, so it
wants a restart to take effect. No config change is required (the new
validation only refuses a combination that never worked), no protocol
change (WS stays version `"1"`), and no CDR change (still v8).

### On the wire: one change to `User-Agent` / `Server`

Four entries below touch these two headers, which reads like four
changes. For anyone who deploys **releases**, it is exactly one:

| | 0.49.7 and every release before it | 0.49.8 |
|---|---|---|
| `Server` on responses | `siphon-rs/0.1.0` | **`siphon-ai/0.49.8`** |
| `User-Agent` on requests | `siphon-rs/0.1.0` | **`siphon-ai/0.49.8`** |
| with `[sip].user_agent` set | *ignored* — still `siphon-rs/0.1.0` | **the configured value**, both headers |

`siphon-rs/0.1.0` was a version number that never corresponded to
anything: it was hardcoded upstream before any release existed, and
this daemon took it because `[sip].user_agent` was wired to nothing.
The intermediate values you will see in the entries below —
`sip-uas/0.3.0`, `sip-uac/0.5.0`, `sip-uac/0.6.0` — existed only in
unreleased builds between the dependency bumps; no tagged siphon-ai
ever shipped them.

**What to check before upgrading:** anything that matches on either
header — a carrier-side rule, a Homer search, a log or capture filter,
an SBC's UA-based routing. If you would rather keep a stable string
across this transition, set `[sip].user_agent` to whatever you were
matching on; it is honoured now.

### Fixed

- **A `[[register]]` block on an ephemeral SIP port is now refused at config load, instead of dying quietly at runtime.** The Contact a registrar routes calls back through is built from the *configured* `[sip].listen` port — `[node].public_address` replaces only the host — so `listen = "0.0.0.0:0"` produced `sip:user@0.0.0.0:0`, which is not a valid SIP URI. Until siphon-rs v2026.08.20.1 that panicked the registration's drive task; after it, the task ends with a `warn!` and that registration is simply dead on a daemon whose `/ready`, metrics and other registrations all look fine. CLAUDE.md §4.6 puts this class of check at load time, so `siphon-ai check` and startup both refuse it now, naming the block and quoting the listen. An ephemeral bind with no `[[register]]` blocks is untouched — nothing needs advertising.

- **The other half of `[sip].user_agent`: requests now name this product too** — closes [#539](https://github.com/thevoiceguy/siphon-ai/issues/539). The response half shipped in the entry below; requests kept saying `sip-uac/<version>` because every request builder in `sip-uac`'s `UserAgentClient` pushed a crate constant and `UACConfig::user_agent` was consumed only as an SDP session name — no configuration from here could reach the header. [siphon-rs#116](https://github.com/thevoiceguy/siphon-rs/pull/116), opened from this project, threads the configured token into those builders; the pin bump below picks it up. A REGISTER now carries `User-Agent: siphon-ai/<version>` (or whatever `[sip].user_agent` says), asserted against a fake registrar in `bins/siphon-ai/tests/startup.rs` — the test that shipped `#[ignore]`d with the first half, now live.
  - **Operator note:** requests move from `sip-uac/0.5.0` to `siphon-ai/<version>` on a node that sets nothing. With the `Server` change in the previous release and the two token changes before it, anything matching on either header wants checking once, now, rather than release by release.

- **`[sip].user_agent` was documented to brand the `User-Agent` and `Server` headers and was wired to nothing** ([#539](https://github.com/thevoiceguy/siphon-ai/issues/539)). The compiled value reached exactly one call site — `sip_local_uri()`, where a helper ignored its argument and returned the literal `"siphon"` — so the key parsed, validated, and did nothing, with no warning to say so. Every response this daemon sent carried the SIP stack's own default instead: `siphon-rs/0.1.0` historically, `sip-uas/<version>` since the v2026.08.19 bump above.
  - **Responses are fixed.** `[sip].user_agent` now reaches the `Server` header, and an unset key means **`siphon-ai/<version>`** — this product and its own version, rather than whichever stack crate emitted the message. It applies to responses the UAS synthesizes (100 Trying, 405, 481, 501, the OPTIONS 200) and to the ones the routing handler builds and fills itself (the trunk 403, the no-route 404 / 488).
  - **Requests are not, and cannot be from here.** `sip-uac`'s request builders — `create_register`, `create_options`, `create_invite_with_from`, `create_invite_with_body`, `create_reinvite`, `create_update`, `create_publish`, `create_subscribe` — each push the crate constant `DEFAULT_USER_AGENT` onto `User-Agent` directly. `UACConfig.user_agent` exists but is consumed only as an SDP session name, so REGISTER, INVITE, REFER and BYE still say `sip-uac/<version>` no matter what is configured here. That is the same shape of bug one layer down, and it needs a siphon-rs change; #539 stays open for it, with an `#[ignore]`d test in `bins/siphon-ai/tests/startup.rs` stating the intended behaviour.
  - The dead `extract_user_part()` helper is gone. `sip_local_uri()` uses the `siphon` user-part directly, which is what it always produced — a `User-Agent` is product info, not a user, and routing one through the other is what disguised the dead code as a live path.
  - **Operator note:** a node that never set this key moves from `sip-uas/0.3.0` to `siphon-ai/<version>` on responses. Combined with the bump above, that is two changes to the same header in one release for anyone matching on it.

- **The SIPp harness reported a daemon that never started as six failing scenarios** ([#541](https://github.com/thevoiceguy/siphon-ai/issues/541)). #533 made every port the harness binds overridable and added a preflight — but it probed only what the *auxiliary* phases bind. The **main** phase runs `$SIPHON_AI_CONFIG` (`configs/local-dev.toml` by default), whose `[observability] http_listen` is a literal the preflight never read. So on a box with a daemon already on `:9091`, the documented `AUX_OBS_PORT=9591 ./run-all.sh` moved the auxiliary phases off the collision and left the main phase sitting on it: its daemon exited with "Address already in use" and its first six scenarios — `basic_call_then_bye` through `reinvite_unsupported_codec_488` — reported as scenario failures while every auxiliary phase passed. That is the exact failure mode #533 set out to end, one port short of ending it; it cost a red 7-of-40 run while verifying the siphon-rs bump below.
  - **The main phase now gets what every other phase already had:** its config is copied with the `[observability]` and `[admin]` listeners rewritten onto the harness's own ports before the daemon sees it. One `AUX_OBS_PORT` override covers the whole run, and `SIPHON_AI_CONFIG` goes back to meaning "which config to run" rather than "how to dodge a port". Only lines that already exist are rewritten, so a config that declares no listener still declares none.
  - **A daemon that dies during startup now aborts the run**, printing the config path, the log path and the log's last 15 lines — and keeping the rewritten copy on disk to inspect. Whatever the cause (a port taken, a config the daemon rejects, a bad build), the answer is in that log, not in sipp output. Verified by pointing the harness at a config with an invalid `[sip].listen`: exit 2, no scenario run, cause printed.
  - Re-run on the box that produced the red run, with the production daemon still holding `:9091`: **39 of 40**, the one failure being `barge_in_pause`, which needs a `setcap` this host does not grant.

### Changed

- **Bumped siphon-rs `v2026.08.20` → [`v2026.08.20.1`](https://github.com/thevoiceguy/siphon-rs/releases/tag/v2026.08.20.1)** — sip-uac 0.6.0 → 0.7.0, sip-uas 0.3.0 → 0.4.0, every other crate unchanged. Carries [siphon-rs#119](https://github.com/thevoiceguy/siphon-rs/pull/119) for its [#118](https://github.com/thevoiceguy/siphon-rs/issues/118), both filed from this project while bumping onto v2026.08.20: the integrated builders' `local_uri` / `contact_uri` parsed with `.ok()` and discarded the failure, so a rejected URI was indistinguishable from an unset one. `build()` then either reported `local_uri is required` for a URI that *was* supplied, or synthesized a default Contact and panicked unwrapping that parse.
  - **It was reachable here.** The registration UAC's Contact is built from the *configured* `[sip].listen`, not the bound address, so `listen = "0.0.0.0:0"` plus a `[[register]]` block made `sip:user@0.0.0.0:0` — unparseable, silently dropped, and then a panic in the default-Contact path that killed that registration's task. It now surfaces as an error naming the block.
  - **Breaking upstream, so the four builder sites here take a `?`**: the UAS, the transfer UAC, each gateway UAC, and the per-`[[register]]` UAC. Each names itself in the error (`UAS contact_uri: …`, `[[register]] cucm local_uri: …`) rather than passing the bare upstream message up.

- **Bumped siphon-rs `v2026.08.19` → [`v2026.08.20`](https://github.com/thevoiceguy/siphon-rs/releases/tag/v2026.08.20)** — sip-uac 0.5.0 → 0.6.0, every other crate unchanged. Carries [siphon-rs#116](https://github.com/thevoiceguy/siphon-rs/pull/116) (see above). Upstream's `UacError` gains two variants, which is breaking there; nothing here matches on that type, so no code moved. `cargo check --workspace --all-targets` needed no glue changes.
  - **The pins are now tags, not revs.** siphon-rs's `RELEASING.md` asks consumers to pin tags, and it answers the objection that kept us on revs — *"Tags are immutable. Never move or delete a pushed tag; if a tagged release is bad, cut a new one."* A tag also names what the release notes and the changelog name, where a twelve-hex-digit rev names nothing, and `Cargo.lock` still records the exact commit either way. All thirteen `sip-*` entries move together, as they must: different refs of one repo are different sources to Cargo, which duplicates shared crates and produces impossible type mismatches.

- **Bumped siphon-rs `99da91f599e2` → `9b7b238fd11f`** — [v2026.08.19](https://github.com/thevoiceguy/siphon-rs/releases/tag/v2026.08.19), upstream's **first tagged release**, carrying [siphon-rs#111](https://github.com/thevoiceguy/siphon-rs/pull/111), [#112](https://github.com/thevoiceguy/siphon-rs/pull/112) and [#113](https://github.com/thevoiceguy/siphon-rs/pull/113). Upstream `main` is two commits further on, both docs-only (a changelog entry and a `RELEASING.md`), so the tag is the clean point to sit on. We still pin the **rev**, not the tag: a tag is a movable ref, and reproducibility is the whole reason these are pinned.
  - **#111** gives the crates real version numbers for the first time — `sip-uac` 0.5.0, `sip-uas` 0.3.0, `sip-core` 0.7.7, and so on, where everything had read 0.1.x since the repo began. No code change; the numbers are what #113 consumes.
  - **#112** is README prose. No code.
  - **#113 changes what this daemon puts on the wire.** It derives the default `User-Agent` / `Server` product tokens from those crate versions instead of the hardcoded `"siphon-rs/0.1.0"` frozen there before any release. Upstream reasons that embedders setting `user_agent` themselves are unaffected — **we are not such an embedder**: neither the `IntegratedUAS` nor either `IntegratedUAC` in `runtime.rs` passes a config, so all three take the stack default. Live-probed on the bumped build, a `Server` header that read `siphon-rs/0.1.0` now reads **`sip-uas/0.3.0`**, and UAC requests carry **`User-Agent: sip-uac/0.5.0`**. Accurate at last, but still the crate rather than the product — `[sip].user_agent` is documented to brand these headers and is in fact dead code ([#539](https://github.com/thevoiceguy/siphon-ai/issues/539)). Fixing that is what will make this token read `siphon-ai/<version>`; this bump only moves it off a number that was never true.
  - **Operator note:** anything matching on that token — a carrier-side rule, a Homer search, a log grep — wants updating before this ships.
  - Verified per CLAUDE.md §7.5: `cargo check --workspace --all-targets` needed **no glue changes**; workspace suite **1,283 passed / 0 failed**; clippy `-D warnings` and `cargo fmt` clean; SIPp signalling **39 of 40**, the single failure (`barge_in_pause`) failing **identically on `main` before the bump** in a control run on the same box with the same config, so it is this host, not the bump; and HEP3 emission re-checked end to end against a stub collector — request and response both captured, payload chunks intact — because `sip-hep` moves with the bump and `test-harness/hep-collector-stub/` is still an empty scaffold. Lockfile movement is confined to the siphon-rs crates: **zero crates.io entries touched**.

### Documentation

- **`test-harness/load/RESULTS-0.49.7-ring-ab.md` — the SIP-ladder ring's memory cost, measured.** `RESULTS-0.49.5-sip-ring.md` recorded it as unquantified because a call-based comparison could not resolve it: the expected signal is 1–2 MB against run-to-run RSS variance larger than that, and at 203 concurrent that rig loses 19 % of its calls to the WS server, so the two arms would not carry the same load. Isolating the ring instead — synthetic SIP from loopback, no media, no WS, no call setup, identical flood in both arms so transaction memory cancels — puts it at **~1.9 MB at a realistic 200-concurrent shape** (about 2.5 % of that node's 77 MB), **~17 MB with the pending bound saturated at its per-call cap**, and a computed **~55 MB** ceiling at the bounds that needs 512 concurrent calls each having exchanged 64+ messages. Per-message cost is roughly `1.34 × payload + 285 B`, the slope being allocator size-class rounding. Carries `abrun.sh`, `ringflood.py` and the two configs into the harness.

- **The SIP-ladder ring's documented worst case was an order of magnitude low.** `DESIGN_SIP_LADDER.md` §3.2 published `50 calls × 64 messages × ~1.5 KB ≈ 4.8 MB` — arithmetic over the *completed* window only, dropping the `MAX_PENDING` (256) and `MAX_LIVE` (512) populations the same section introduces thirteen lines earlier. The bound the code actually guarantees, and states correctly in `sip_ring.rs`, is `(256 + 512 + cap_calls) × cap_messages` entries: **~55 MB**, not 4.8. Corrected, and the measured figures now sit next to the knobs an operator turns rather than only in a load-results file — `docs/CONFIG.md` gains **~1.9 MB at a realistic 200-concurrent shape** (~2.5 % of daemon RSS) on `sip_ring_size` and the per-message cost on `sip_ring_max_messages`, which is the knob that sizes the ring. No behaviour change; the bounds themselves are unmoved.


## [0.49.7] - 2026-08-19

One sightglass fix. **Sightglass-only** — the daemon is behaviourally
identical to 0.49.6, so there is no reason to restart a node for this.
Sightglass ships in the release **tarball**, never the `.deb`; take it
from there, or the TUI stays on whatever version was last installed by
hand while the daemon moves on without it.

### Fixed

- **A trunk's peer list could be clipped mid-CIDR in sightglass.** `35.156.191.128/30` rendered as `35.156.191.128/3` — not merely ugly, because `/3` is itself a valid prefix length, so the table stated a range the config does not contain. Reported from a live session on a node with Twilio's eight CIDRs. The column now packs whole addresses only and appends an honest count of the rest (`54.172.60.0/30, 54.244.51.0/30, +6 more`), scaling with terminal width; where not even one address and its count fit, it degrades to a bare `8 peers` rather than clipping the count itself. The peers column also takes an exact width instead of a `Min`, so the budget is the real column rather than a guess at how leftover space gets divided. DESIGN_SIGHTGLASS.md §7: nothing truncates silently.


## [0.49.6] - 2026-08-19

Everything a single load run turned up. The ring's own bounds held at
203 concurrent — nothing dropped, no warning logged — but the run
surfaced an eviction flaw waiting for a busier node, a trunk an operator
could not see, and a test harness that reported port collisions as
product regressions.

**Upgrade note:** this is the first release since 0.49.2 that changes
daemon behaviour, so unlike 0.49.5 it wants a restart to take effect.
No config change, no protocol change (WS stays version `"1"`), no CDR
change (still v8), and no dependency moved.

### Fixed

- **The SIPp harness now fails fast on a port collision instead of reporting one as 17 scenario failures.** Thirteen auxiliary phases hard-coded `9091` for their `[observability]` listener — the same port `configs/local-dev.toml` uses, and therefore the same port a daemon already running on the box is using. The daemon exited at startup with `Address already in use` and every affected phase reported as a *scenario* failure, which reads like a signalling regression: a local run scored 17 of 40 red on 2026-08-19, none of it real. `SIPHON_AI_CONFIG` only ever fixed the main phase, because the auxiliary phases generate their own configs. Every port the harness binds (`SIPP_PORT`, `DAEMON_PORT`, `ADMIN_API_PORT`, the new shared `AUX_OBS_PORT`, `ECHO_WS_PORT`) now reads from the environment, and a preflight check refuses to start on a busy TCP listener, naming the variable and a free port to use — the same treatment the missing-echo-server case already got, and for the same reason.


### Fixed

- **A live call's SIP ladder could be evicted by scanner noise.** The ring bounds its not-yet-completed traces least-recently-touched, but **an established call is SIP-silent between its ACK and its BYE** — its `last_touched` never advances, while rejected scanner INVITEs and REGISTER refreshes keep arriving with fresh ones. The live call was therefore the *oldest* entry and would be evicted **before** the transient noise the bound exists to contain, discarding the ladder of the call an operator is most likely to be looking at. Live calls are now a separate population, promoted when the control registry accepts the call, bounded separately at `MAX_LIVE = 512`, and never evicted to make room for noise. Found by the 0.49.5 load run (`test-harness/load/RESULTS-0.49.5-sip-ring.md`), which did not trip the bound at 203 concurrent but showed the trace-growth curve that makes it inevitable on a busier or longer-lived node.

### Added

- **`GET /admin/v1/trunks` and `[[trunk]]` rows in sightglass's trunks tab.** The tab showed only `[[register]]` bindings, so on a node with a registered PBX *and* an IP-authenticated carrier trunk, the carrier was simply absent — indistinguishable from a config mistake, and reported as exactly that confusion from a live session. Trunks have no credentials and no registration, hence no live state to poll, which is why DESIGN_SIGHTGLASS.md §6.6 deferred them; but "nothing to poll" is not a reason to render nothing. Trunk rows now list their peer CIDRs with a dim `ip-auth` marker and dashes where a registration would show state, so they never read as "up". Readonly role; active OPTIONS probing toward gateways remains the deferred part.

- **`test-harness/load/RESULTS-0.49.5-sip-ring.md`** — the ring shipped enabled-by-default in 0.49.3 and had never been load-tested. At 203 concurrent it dropped nothing at either bound, and the run also caught **47 of 250 calls dying on `ws_disconnect`**: the generator's WS server, not the daemon, and §1.4 of the load plan happening as written. Carries `ringstat.sh` into the harness.

## [0.49.5] - 2026-08-19

A one-line UX fix with a two-line consequence: the SIP ladder's key is
now visible in sightglass. **Sightglass-only** — the daemon binary is
byte-for-byte 0.49.4 in behaviour, so there is no reason to restart a
node for this. Sightglass ships in the release **tarball**, never the
`.deb`; take it from there.

### Fixed

- **The SIP ladder key was bound but never advertised, so the feature was undiscoverable.** The calls tab's footer listed `j/k`, `x`, `p`, `u`, `c` and `o` but not `s` — reported by an operator who had the pane working only because they had been told the key existed. A keymap without a hint is half a feature. The footer now lists `s sip`, greyed with the `✗` marker for a `readonly` token like every other gated key, because a key nobody can see is a key nobody uses.
  - While the ladder is **open**, the footer swaps to its own keys (`j/k scroll`, `⏎ expand`, `y copy`, `s close sip`). The overlay owns `j/k` while it is up, so continuing to advertise "select" described a key that no longer did that.
  - Adding one hint pushed the footer past 110 columns and silently truncated `originate`, trading one discoverability bug for another. The two navigation labels are now terse (`⇥ tabs`, `n nodes`) — both are already visible in the tab bar and the header chip, whereas the action keys have no other source. A render test pins the fit at 110 columns.

## [0.49.4] - 2026-08-19

### Fixed

- **The SIP ladder's `direction` field was `"unknown"` for every message on a wildcard-bound node — i.e. on nearly every real deployment** (shipped in 0.49.3; found by running the 0.49.3 release artifact against a production-shaped node rather than in a test). siphon-rs stamps a HEP packet's local end with the **socket's** address, so on the usual `listen = "0.0.0.0:5060"` our own end is literally `0.0.0.0`. The derivation matched only `[node].public_address` and a non-wildcard bind IP, so neither end matched and the field fell through to `"unknown"` — sightglass drew `·` for every message and the ladder's arrows never rendered. Confirmed from a production node's own Homer capture: inbound `srcIp <peer> / dstIp 0.0.0.0`, outbound the reverse.
  - The **loopback** case failed the opposite way: both ends are `127.0.0.1`, both match, the `src` arm wins, and every message read `"out"` — including inbound INVITEs. That is how it surfaced, driving a real call through the shipped binary in a lab second instance.
  - **Fix:** an unspecified IP (`0.0.0.0` / `::`) counts as one of ours, which is exactly what siphon-rs means by it; and when *both* ends look local, the SIP bind port breaks the tie. Port is consulted **only after** IP has failed, never before — a port-first test mislabels inbound traffic, because SIP peers overwhelmingly send *from* 5060 too, and that earlier regression test still passes unchanged.
  - No API, config or schema change: `direction` already documented `in`/`out`/`unknown`, and `src`/`dst` were correct throughout, so this is a label fix rather than data recovery. Nodes on 0.49.3 lose nothing by upgrading; their captured messages were always accurate.

### Changed

- **The SIPp CI job no longer loses its whole budget to a slow apt mirror.** The install step cancelled the entire 15-minute job four times in a row on 2026-08-19 without running a single scenario: the runner's `azure.archive.ubuntu.com` mirror returned `Ign:` for every index and the `archive.ubuntu.com` fallback hung inside `apt-get update`. The step now tries the install against the indices the runner image already ships with — skipping the refresh entirely in the common case — and only falls back to `apt-get update` when a package is genuinely unresolvable, with retries, per-request timeouts, and its **own 6-minute budget** so a mirror outage is reported as itself instead of silently consuming the scenarios' time.

## [0.49.3] - 2026-08-19

Two additions, both operator-facing: a per-call SIP ladder you can read
without leaving the terminal, and advisory scanning in CI so the next
RUSTSEC finding arrives as a failing check rather than a hand run. **No
protocol change** (WS stays `version: "1"`), **no CDR change** (still
v8), and no upstream dependency movement — siphon-rs and forge-media are
pinned exactly where 0.49.2 left them.

Upgrade note: `[observability].sip_ring_size` defaults to `50`, so SIP
capture is **on** after this upgrade. It holds recent messages
unredacted in memory and the endpoint serving them is `operator`-gated —
see *Admin auth & RBAC* in `docs/DEPLOY.md`, and set it to `0` to opt
out.

### Added

- **Per-call SIP ladder: `GET /admin/v1/calls/{id}/sip` and sightglass's `s` pane** (DESIGN_SIP_LADDER.md). Select a call on sightglass's calls tab, press `s`, and see its signaling — the SIP messages the daemon captured for it, oldest first, timestamped relative to the call's first message because the gaps are what a ladder is read for. `⏎` expands one message to full raw text, `y` copies it via OSC 52 (works over SSH; no clipboard dependency added).
  - **`operator` role, deliberately unredacted.** The endpoint returns messages verbatim, `Authorization` and `Proxy-Authorization` headers included — a redacted ladder invites the wrong conclusion about what actually went over the wire, so the access boundary is the token role rather than the content. This is **the first `GET` on the admin API gated above `readonly`**, and `DEPLOY.md` now states plainly that on a default install an `operator` token can read recent credentials. `--read-only` mode does not block it: the ladder changes nothing on the node.
  - **Capture is a new sink leg, not a new capture mechanism.** siphon-rs's `sip-hep` owns no transport — it emits into the `HepSink` handle siphon-ai constructs — so every SIP message already passed through the process with the raw bytes, the `Call-ID` as correlation id, `src`/`dst` and a timestamp. No siphon-rs change, no second parse of the wire, no new dependency.
  - **Works with HEP shipping off.** `HepTelemetry::build` now takes an optional collector plus extra in-process sinks; with `[observability.hep]` absent it opens no socket and spawns no worker, and a single sink leg is used directly rather than wrapped, so a HEP-only node keeps its exact previous call path. Teeing off the UDP sink instead would have left the pane silently *empty* on a node that ships nothing to Homer — "no messages" rather than "not enabled", the worst failure mode for an observability feature.
  - **Bounded three ways, because most SIP dialogs are not calls.** Per call: 64 messages (`[observability].sip_ring_max_messages`), oldest-within-the-call dropped and the response flagged `truncated`. Completed calls: 50 (`sip_ring_size`, defaulting to `cdr_ring_size` so the recent-calls pane and the ladder never disagree about what is inspectable). And a third bound the design note missed — REGISTER refreshes, OPTIONS and, on a public-IP node, scanner INVITEs rejected with 403 all carry a `Call-ID`, all reach the sink, and none ever becomes a call, so none would ever be evicted by a completed-call window; the reference node alone would feed it ~1,440 REGISTER cycles a day. Pending traces are capped at 256 and evicted least-recently-touched.
  - **Enabled by default** (`sip_ring_size = 50`); `0` disables capture entirely and the endpoint answers `501`. Sightglass renders `501` (capture off), `403` (token too low) and `404` (unknown call, or a pre-0.49.3 daemon) as notes *inside the pane* — none of them marks the node down. The pane is polled only while open: it is the one payload sightglass fetches measured in kilobytes.
  - New metrics `siphon_ai_sip_ring_messages_total{result}` and `siphon_ai_sip_ring_traces` (named *traces*, not *calls*, for the same reason the third bound exists). No protocol change, no CDR change, nothing written to disk. **Not a Homer replacement** — minutes of history, one node, no search; `docs/HEP.md` remains the answer for anything in depth.

- **`cargo audit` in CI** (`.github/workflows/audit.yml`) — this repo had no advisory scanning at all, which is why [RUSTSEC-2026-0258](https://rustsec.org/advisories/RUSTSEC-2026-0258) sat in the lockfile for two days and then surfaced from a hand run rather than a check (see 0.49.2). The sibling forge-media repo caught the same advisory the day it published, because it has this job.
  - The trigger set is deliberately **not** test.yml's. A **daily schedule** is the case that actually matters — an advisory lands against code nobody touched, so no push and no PR will ever fire a check, only a clock. **`push` to main** keeps the default branch's status honest between nightly runs. **Pull requests are gated on dependency files only** (`Cargo.lock`, `Cargo.toml`, the workflow itself): gating every PR means a newly published advisory turns unrelated work red through no fault of its own, which is exactly what happened to [forge-media#107](https://github.com/thevoiceguy/forge-media/pull/107) — a redis bump left sitting on an h2 failure it had nothing to do with.
  - Fails on **vulnerabilities only**. The informational "unmaintained"/"unsound" advisories (currently 6: audiopus_sys, paste, rustls-pemfile, anyhow, lru ×2) are reported without failing the run — a gate that is chronically red gates nothing.
  - `cargo audit` reads `Cargo.lock` and never builds, so the job needs no system libraries and no workspace compile.

## [0.49.2] - 2026-08-19

Housekeeping: a security advisory in our own lockfile, and two upstream log
levels that stop a healthy node from writing noise. **No behaviour change, no
config change, no protocol change** — the WS protocol stays `version: "1"` and
the CDR schema stays v8.

### Fixed

- **Two `cargo audit` vulnerabilities in this workspace's lockfile** ([#522](https://github.com/thevoiceguy/siphon-ai/pull/522)). [RUSTSEC-2026-0258](https://rustsec.org/advisories/RUSTSEC-2026-0258) — *h2 unbounded empty DATA frames*, published 2026-08-17 — hit the h2 0.4.14 under hyper 1.x, which serves **every HTTP listener the daemon exposes**: the admin API, `/metrics`, `/healthz`, and the outbound webhook/CDR sink client. Bumped to 0.4.16. Alongside it, [RUSTSEC-2026-0204](https://rustsec.org/advisories/RUSTSEC-2026-0204) (invalid pointer dereference in crossbeam-epoch's `fmt::Pointer` impl, reached via forge-vad → tract-onnx → rayon) goes 0.9.18 → 0.9.20. Lockfile-only, no `Cargo.toml` change.
  - The diff is deliberately **4 lines**. `cargo update -p h2 --precise 0.4.16` also rewrote 11 unrelated lines — `socket2` 0.6.3 → **0.5.10** under hyper, quinn and quinn-udp, and `windows-sys` 0.61.2 → **0.52.0** under errno, rustix, tempfile and winapi-util. Those are *downgrades*, two of them on the network path, and nothing in either advisory asks for them. This starts from the previous lockfile and edits only the two version + checksum pairs; `cargo metadata --locked` accepts the result, so cargo considers it an exact, valid resolution.
  - **No `cargo audit` job exists in this repo's CI** (only `test.yml` and `release.yml`), so this surfaced from a hand run rather than a check. Worth adding — forge-media's Security Audit job caught the same advisory the day it published.

### Changed

- **Bumped siphon-rs `15303bf847e6` → `99da91f599e2`** — [siphon-rs#109](https://github.com/thevoiceguy/siphon-rs/pull/109) and [siphon-rs#110](https://github.com/thevoiceguy/siphon-rs/pull/110) (its issues #107 and #108, both filed from this project). **Two log levels; no behaviour change and nothing to configure.** Operators running at `info` get a materially quieter journal; anything already at `debug` is unaffected.
  - **#109 — the expected first digest challenge drops from `warn!` to `debug!`.** `IntegratedUAC` warned whenever a 401/407 arrived while `auto_retry_auth` was set. That is RFC 3261 §22 working exactly as specified: the registrar is supposed to challenge, the stack answers it immediately, the request succeeds. Nothing had happened an operator could act on. **Measured on this project's reference node at a 120s granted expiry: ~1,440 WARNs/day on an idle, healthy box, 361 of them in a single 6-hour window** — a permanent non-zero warn baseline, which is what makes warn-level alerting useless. The two genuinely abnormal auth outcomes in the same function — "auth still rejected; retrying with refreshed credentials" and "auth retry limit reached; returning last challenge to caller" — keep their `warn!`.
  - **#110 — four per-transaction `info!` sites drop to `debug!`**: the transaction start, the authenticated-retry start, and `on_final` in both `InviteTransactionUser` and `SimpleTransactionUser`. The precedent was already in the file — `on_provisional`, the sibling method handling the same class of event, has always been `debug!`, so a 180 was debug and a 200 was info for the same "a response arrived on the transaction I started" mechanic. At `info` the volume tracked request rate rather than operator intent: **~13,100 client transactions produced 52,568 lines from those four statements alone** — ~750 lines/s at a sustained 566 transactions/s, and a 20 MB log in 70s of driving.
  - No code change in this workspace; both are log-level changes inside siphon-rs, and `cargo check --workspace` needed no glue edits.

- **Bumped forge-media `1d7bbaba0c22` → `b161adedee5d`** — six commits, **all dependency bumps**: [#106](https://github.com/thevoiceguy/forge-media/pull/106) bytes 1.12.1, [#109](https://github.com/thevoiceguy/forge-media/pull/109) base64 0.23.1, [#110](https://github.com/thevoiceguy/forge-media/pull/110) validator 0.21.0, [#113](https://github.com/thevoiceguy/forge-media/pull/113) a 7-crate patch group, [#114](https://github.com/thevoiceguy/forge-media/pull/114) h2 0.4.16 for RUSTSEC-2026-0258 plus the deletion of `forge-ai-stream`'s unused reqwest 0.11, and [#107](https://github.com/thevoiceguy/forge-media/pull/107) redis 1.2.0 with the two call sites its 1.x API removed. **Taken to keep the pin current, not to collect a fix.**
  - **No forge-media source change touches code we compile.** #114's h2 bump is in forge-media's own lockfile, which does not apply to a git dependency, and its only source edit is to `forge-ai-stream` — the crate CLAUDE.md §4.1 keeps us off. #107's redis sits behind `forge-engine`'s `persistence-redis` feature, which implies `ai`, which we disable; `redis` appears nowhere in this workspace's lockfile.
  - One change does reach our build: #109 moves **forge-sdp to base64 0.23.1**, which now builds alongside the 0.22.1 we use ourselves. A duplicate version, harmless — dedupe when we move our own base64. Same situation as the sha1/hmac/sha2 duplicates noted at the 2026-08-07 pin.

## [0.49.1] - 2026-08-18

### Changed

- **Bumped siphon-rs `9ad33983b0e9` → `15303bf847e6`** — [siphon-rs#104](https://github.com/thevoiceguy/siphon-rs/pull/104) (its issue #103, filed from this project): **a client SIP transaction that completed normally never left the transaction manager's table.** The terminal wait timers — K for non-INVITE (RFC 3261 §17.1.2.2), D for INVITE (§17.1.1.2) — set the FSM `Terminated` but emitted only `Cancel`, while the manager removed the entry solely on `Terminate`. `Terminate` is reserved for abnormal endings because it notifies the transaction user, so the normal path had no removal at all: **a transaction that failed was reclaimed, and one that succeeded was not.** With no periodic sweep, the only thing that ever removed a completed client transaction was the 10,000-entry eviction.
  - **Found on this project's reference node**, which registers to a registrar every 60s. Each refresh costs two client transactions (the 401-challenged REGISTER plus the authenticated retry), so the table filled in **~3.5 days** and then evicted on every subsequent refresh, logging `Client transaction limit reached, evicting oldest transaction ... method: Register ... current_count=10000 limit=10000` once a minute.
  - **Why it is a fix and not just tidiness:** the eviction picks its victim by `start_time` alone, so once the table is full the oldest entry can be a *live* transaction rather than a dead one. Everything before that point is bounded and harmless; past it, a long-running transaction is at risk.
  - Scope is wider than REGISTER — every normally-completing client transaction (OPTIONS, INFO, MESSAGE, NOTIFY, SUBSCRIBE, UAC BYE via Timer K; INVITE after a non-2xx final via Timer D) — and both transports: on TCP/TLS the wait timer is zero, so those leaked immediately rather than after T4. Client-side only; the server's Timer J/I already emitted `Terminate`.
  - Upstream fixes it by reaping on **FSM state** rather than on the action, so the transaction-user contract is unchanged — notably no extra `on_terminated` call, and therefore no new `WARN`, on a path that runs once per registration refresh and once per request on a busy proxy.
  - **No code change here and nothing to configure.** The reap is entirely inside `TransactionManager`; nothing in this workspace changed. Operators get the fix by upgrading, and a restart clears an already-saturated table.
  - Also carries [siphon-rs#105](https://github.com/thevoiceguy/siphon-rs/pull/105) (clears ~180 clippy warnings across 12 crates) and [siphon-rs#106](https://github.com/thevoiceguy/siphon-rs/pull/106) (a fmt + clippy CI gate on a pinned toolchain). Neither changes behaviour.
  - Verified per CLAUDE.md §7.5: workspace suite green, fmt, clippy `-D warnings`, all check scripts, and the SIPp signalling suite re-run. No glue changes.

## [0.49.0] - 2026-08-17

### Added

- **`POST /admin/v1/drain` and sightglass's System tab — the plan's final chunk** (DESIGN_SIGHTGLASS.md §6.5, PR 6 of 6). The daemon gains an admin-role **programmatic drain**: the endpoint fires a `Notify` the runtime's `run()` selects beside the SIGTERM future, entering the exact graceful path (503 new INVITEs, active calls finish, force-terminate at the deadline, exit) — `202 {drain_signalled, already_draining}`, idempotent, and deliberately not wired to the "second signal forces teardown" escape hatch, so repeating the POST can never kill calls. `GET`/`PUT /admin/v1/log` responses move to typed wire structs (byte-identical JSON). Sightglass gains the **System tab** (`6`): one row per node with live log filter, drain state, HEP, and version — `L` sets the tracing filter on the selected node (live, no restart), `H` emits a HEP probe, `D` starts the drain behind a deliberately loud node-named confirm; all admin-gated per node. This completes the six-PR sightglass plan.

- **Release tarballs now carry two binaries: `siphon-ai` and `sightglass`.** The release workflow builds and packages the operator console alongside the daemon for both musl targets. The `.deb` stays daemon-only on purpose — sightglass is a client tool that usually runs on an operator's workstation, not the service host; take it from the tarball.

- **A recent-CDRs ring behind `GET /admin/v1/cdrs/recent`, richer live stats, and sightglass's call history** (DESIGN_SIGHTGLASS.md §6.3/§6.4, PR 5 of 6). A ring-capturing sink joins the runtime's CDR fanout and keeps the last `[observability].cdr_ring_size` (default 50) completed calls' **CDR records verbatim** in memory — the CDR schema is the one schema, served newest-first on the readonly endpoint, working with zero `[cdr]` sinks configured and surviving SIGHUP sink rebuilds. The live-stats endpoint `GET /admin/v1/calls/:id/stats` gains **static call facts**, registered once per call beside the quality feed: `direction`, `from`, `to`, `sip_call_id`, `sample_rate`, `srtp_profile` (absent = plaintext RTP), `verstat_attest` — all additive JSON. (The design's dynamic fields — live VAD state, last DTMF, WS reconnect count — are deferred; each needs tap→registry streaming that isn't worth building unasked. The design note records the split.) Sightglass's calls tab grows a **recent pane** under the active table (ended time, from → to, hangup cause, duration, MOS — read defensively off the versioned CDR records) and the detail pane renders the new static facts. Pre-0.49 daemons degrade to an absent pane, never a down node.

- **`GET /admin/v1/status`, a completed sightglass overview, and the Rooms tab** (DESIGN_SIGHTGLASS.md §6.2, PR 4 of 6). The daemon gains a one-request (readonly-role) JSON summary — `{version, uptime_secs, active_calls, registrations: {registered, total}, draining, hep_enabled}` — the live snapshot a dashboard or deploy script wants without parsing Prometheus text; cumulative counters stay on `/metrics` (the design's `total_calls`/`hep_collector_up` fields were dropped — neither has readable state behind its metric, and duplicating counters into core wasn't worth it; the design note records the deviation). Sightglass's overview grid gains **version and uptime columns** (dash on pre-0.49 daemons), and the new **Rooms tab** (`4`; errors moves to `5`) lists conference rooms with members inline plus parked calls across the fleet — `x` ends the focused room / kicks the focused member / hangs up the focused parked call via node-named confirms (operator role), `u` retrieves the focused parked call with the optional new-`ws_url` form. Conferences/parked polls tolerate `501` (feature off ⇒ empty), and status tolerates pre-0.49 daemons — neither can mark a node down.

- **A recent-errors ring behind `GET /admin/v1/errors`, and sightglass's Errors tab reading it** (DESIGN_SIGHTGLASS.md §6.1, PR 3 of 6). A `tracing` layer captures every `warn!`/`error!` event into a bounded in-memory ring — timestamp, level, target, message with structured fields appended, and the **`call_id` of the nearest enclosing per-call span**, so entries join against `/admin/v1/calls`, the CDR, and Homer. The ring answers newest-first on the (readonly-role) endpoint; capacity is `[observability].error_ring_size` (default 256, `0` disables, >65536 fails at load — it's an operator tail, not log storage). The layer installs before config loads so config-load warnings are themselves captured, sits behind a WARN per-layer filter (zero cost on the info/debug firehose), and capture is a short mutex push on the logging thread — nothing on the audio path emits warns in steady state, and HEP-style queueing would be overkill for events this rare. One caveat documented in CONFIG.md: the reloadable global log filter gates all layers, so narrowing it below `warn` via `PUT /admin/v1/log` also narrows the ring. New metric `siphon_ai_error_ring_captured_total{level}` — a rate spike is a health signal even when nobody is watching the ring. Sightglass grows the **Errors tab** (`4`): the fleet's merged tail, newest first, node-tagged and level-colored, scoped by the node filter like every tab; a pre-0.49 daemon (no endpoint) degrades to an "endpoint unavailable" note rather than an error — and never marks the node down.

- **Sightglass grows its operator actions** (DESIGN_SIGHTGLASS.md, PR 2 of 6). The calls tab now acts, not just watches: `x` hangs up the focused call, `p` parks it, `u` retrieves (with an optional new `ws_url` — move a live call to a different WS server), `c` adds it to a conference room, and `o` opens a dial form for outbound origination — each against the existing `/admin/v1/*` endpoints, each targeting exactly one node, with the confirm modal naming that node ("hangup abc-123 **on prod-2**?") so a fleet operator never acts on the wrong box. Results surface as transient toasts carrying the daemon's own error text. Keybinds are **RBAC-aware per node**: at startup sightglass learns each token's role with two side-effect-free probes of the RBAC gate (a hangup on a sentinel call id and an empty-body originate — the gate runs before dispatch, so 403-vs-404/400 separates the roles without touching a call or dialing anything) and greys out what the role can't do; a real 403 later toasts and teaches the ceiling. `--read-only` now hard-disables all actions client-side (and skips the probes). New user guide: `docs/SIGHTGLASS.md`. Daemon untouched — this is all client-side, on endpoints that have existed since 0.6.0–0.43.0.

- **`sightglass`: a terminal operator console for one or more siphon-ai nodes** (DESIGN_SIGHTGLASS.md, PR 1 of 6). A new `bins/sightglass` binary (crate `siphon-ai-sightglass`, ratatui) that fans out to each configured node's `[admin]` listener and renders a tabbed live view — **overview** (fleet health grid + active-call sparkline), **trunks** (registration state across nodes), **calls** (fleet-unified table with both id namespaces and direction, plus a per-focused-call quality pane fed by `GET /admin/v1/calls/:id/stats` with a client-side MOS trend). Multi-node is first-class from this first cut: every record is keyed `(node, id)`, one staggered poller set per node, a down node degrades to its own dimmed row without stalling the rest, and the Node column auto-hides on single-node fleets. Fleet config is `~/.config/sightglass/config.toml` (`[[node]]` name/url/token_file/ca) or `--target` for an ad-hoc single node; `--read-only` and `--ascii` flags per the design note. Read-only in this PR — operator actions (hangup/park/retrieve/originate, node-named confirm modals, per-node RBAC-aware keybinds) are PR 2. The daemon binary is untouched: ratatui/crossterm live only in the new crate.

- **`crates/admin-api-types`: the admin API's request/response wire shapes as a shared crate**. The `/admin/v1/*` JSON shapes (`AdminCallRow`, `RegistrationRow`, `ConferenceRow`, `ParkedRow`, `DrainStatus`, the list envelopes, and the POST bodies) moved out of `siphon-ai-telemetry` into a serde-only crate consumed by both the daemon (serializer) and sightglass (deserializer), so the two cannot drift. Wire-preserving by construction: telemetry re-exports the same names at the same paths, the handlers serialize the same JSON byte-for-byte (all 79 existing admin dispatch tests pass unchanged), and new wire snapshot tests in the crate pin the exact JSON as the contract. One internal type change: `AdminCallRow.direction` is `String` rather than `&'static str` (identical on the wire; required for the deserialize direction).

## [0.48.19] - 2026-08-14

### Changed

- **Bumped forge-media `b4f8df5c8f09` → `1d7bbaba0c22`** — [forge-media#112](https://github.com/thevoiceguy/forge-media/pull/112) (its issue #111, filed from this project's #504): a port bind that hits `AddrInUse` now **retries the next pair instead of failing the session**. `PortPool` allocates from its own bookkeeping and binds later, so a port it believes free can be held by a socket outside the process — `[media].rtp_port_range` sits inside the kernel's ephemeral range (`net.ipv4.ip_local_port_range`, commonly 32768–60999), and a DNS lookup by anything on the host is enough to take one. Here that surfaced as a `500 Server Internal Error` on an inbound INVITE, measured at **one call in 399** during the 0.48.18 soak. Upstream splits `ForgeError::AddrInUse(SocketAddr)` out of `Network(String)` — so the retry keys off a type rather than message text — and draws up to five pairs, logging a `warn!` with the address each time it steps past one.
  - **No code change here and nothing to configure.** Nothing in this workspace matches on `ForgeError`, so the new variant is purely additive. Operators get the fix by upgrading.
  - **The `net.ipv4.ip_local_reserved_ports` guidance in `docs/DEPLOY.md` stays correct and is still worth applying.** This removes the *requirement* to reserve the range, not the value of doing so: a reserved range means the retry never has to fire, and a `warn!` that does fire is still telling you something real about the host. The docs are left as they are for that reason.
  - Verified per CLAUDE.md §7.5: workspace suite 1,161 passed / 0 failed, fmt, clippy `-D warnings`, all three check scripts, and the SIPp signalling suite re-run. No glue changes.

## [0.48.18] - 2026-08-14

### Changed

- **Bumped siphon-rs `9f70b83011f2` → `9ad33983b0e9`** — [siphon-rs#102](https://github.com/thevoiceguy/siphon-rs/pull/102) (its issue #101, filed from this project): `TransactionManager::ack_received` now returns whether it **absorbed** the ACK. It used to return `()` and no-op on a miss, hiding the one distinction RFC 3261 §17.2.1 draws — an ACK for a non-2xx final belongs to the completed INVITE server transaction alone, while an ACK for a 2xx is end-to-end and only the transaction user can handle it. The FSM already knew which was which (`send_final` terminates the server transaction on a 2xx and leaves it `Completed` on a non-2xx, so "matched an entry" is exactly "ACK to a non-2xx") and simply did not say so. Source-compatible and not `#[must_use]`, so the signal is opt-in; this release opts in — see the `Fixed` entry below. Verified per CLAUDE.md §7.5: workspace suite green, SIPp signalling suite re-run, no glue changes.

### Fixed

- **Absorbed ACKs no longer reach the UAS, so a rejected INVITE stops producing a stray `WARN`.** 0.48.17 fixed `missing_sdp_answer` (#497) by dispatching *every* ACK, because the pump had no way to keep the delayed-offer ACK it needed while dropping the ones it did not. The side effect shipped with it: an ACK acknowledging a non-2xx final — which the transaction has already absorbed, and which is hop-by-hop by §17.2.1 — was handed to `IntegratedUAS`, resolved to no dialog, and logged `Received ACK for unknown dialog`. On a public-facing listener that is one per scanner INVITE rejected with a `403` and then ACKed: **measured at ~0.5/min (~660/day) on the reference node**, against zero in the week before, plus a dialog-manager lock and a task spawn per packet.
  - Nothing was ever broken by it — `ack_received` is called before dispatch either way, so non-2xx absorption and retransmission-timer clearing were untouched throughout. The cost was operator noise in a `WARN` stream that is otherwise worth reading, and a little attacker-triggerable work per unsolicited datagram.
  - **The fix belongs upstream, not in a log level.** With `ack_received` now reporting absorption, the pump returns on `true` and dispatches only what the transaction layer left for it. That is the RFC's own layering rather than a filter bolted on top: `warn!`-to-`debug!` would have hidden the symptom while still doing the lookup and the spawn.
  - The delayed-offer path is unchanged and still covered by the `delayed_offer_no_answer` harness phase: a 2xx terminates the server transaction as it is sent, so its ACK is never absorbed and still reaches `on_ack` — body or not.

## [0.48.17] - 2026-08-13

### Changed

- **Bumped siphon-rs `b9f5a3bf66f2` → `9f70b83011f2`** — [siphon-rs#100](https://github.com/thevoiceguy/siphon-rs/pull/100) (its issue #99, filed from this project's #490): the UAC session refresh reserves the CSeq it consumes *before* the request leaves, instead of committing it after the response. Upstream's window was the one #491 closed here — for a whole round trip the shared dialog still read the pre-request number, so an owner request racing the refresh (teardown BYE, hold re-INVITE, REFER) reused it. `start_session_timer` had documented that as unclosable at that layer, on the reasoning that the alternative was serialising behind the dialog's write lock across a ~32 s round trip; reserving holds it for two clones and an increment instead. **Nothing changes on the wire here**: nothing in this daemon calls `CallHandle::start_session_timer`, and our own outbound legs are covered by `DialogSource::reserve_cseq` on the `shared_dialog` upstream never touches. What it buys is removing the last reason not to adopt the upstream timer — the gate [#484](https://github.com/thevoiceguy/siphon-ai/issues/484) now names. Verified per CLAUDE.md §7.5: workspace suite 1,161 passed / 0 failed, SIPp signalling 37/38 locally (only `barge_in_pause`, which needs a `setcap` this box lacks) and green in CI, no glue changes, and the #490 repro re-run unchanged against the bumped build.

### Fixed

- **`missing_sdp_answer` could never happen** (issue #497). The daemon's packet pump dropped every body-less ACK before dispatch, on the reasoning that an early-offer ACK carries no SDP and needs no application handling. True of that population — and the premise fails on the one the outcome exists for: a **delayed-offer** ACK with no SDP is the peer declining to answer our offer (RFC 3261 §13.2.2.4 requires it there). That call was never classified; it sat half-established, holding its forge media session and RTP ports, until Timer H expired **32 s** later and recorded `ack_timeout` — a cause that is supposed to mean no ACK arrived at all. The CDR variant, the `siphon_ai_delayed_offer_total{result="missing_sdp_answer"}` label and the `docs/DEPLOY.md` row all described an outcome the daemon could not produce, and a trunk that consistently declines offers looked like a network fault: exactly the confusion #425 added the variant to prevent.
  - Both downstream layers were already correct and both were unreachable — `sip-glue`'s `on_ack` documents forwarding body-less ACKs for this precise reason, and `BridgingAcceptor::on_ack` handles both populations (an ACK for a dialog we are not holding matches nothing and is ignored). The #425 tests call `on_ack` directly, which is why a green suite never caught it. **The fix is one layer up**, in the pump: dispatch every ACK. The cost is one dialog-map probe per early-offer ACK, once per call.
  - Live before/after on the same lab: the declining ACK now reaps at once — CDR `termination.cause = "missing_sdp_answer"`, `duration_ms` 0, `answered_at` null, and the metric label finally moves — where before it was `ack_timeout` at `duration_ms` 32001.
  - Covered from now on by a new harness phase, `delayed_offer_no_answer`, which asserts the metric label *and* the CDR cause. The CDR grep doubles as the timing assertion: a regression to the Timer-H path writes no CDR for another 32 s, so it fails rather than passing slowly. Verified to fail against the previous binary.

- **The outbound session-timer arming log no longer describes a cadence it does not use.** It read *"we refresh at half the interval"*, which #490 made wrong in the same release that shipped it — refreshes run a five-second guard ahead of the half-way point. It now logs `refresh_period_secs`, computed by the same function the loop schedules from, so the line cannot drift from the code again; the prose says "we refresh on that period" and the two branches (we-refresh vs callee-refreshes) are split so only the relevant one carries the field. `docs/CONFIG.md` and `docs/DEPLOY.md` were already corrected in #490 — this was the one place left saying the old thing, and the tense of `reserve_cseq`'s doc comment is fixed with it.

## [0.48.16] - 2026-08-13

### Fixed

- **An outbound session-refresh loop that gives up now says so** (issue #484). The refresh added in 0.48.14 works — live-verified, CSeq advancing 2→3→4 against a callee imposing `Session-Expires: 90;refresher=uac` — but it had no failure signal whatsoever: a rejected or timed-out refresh logged a `warn!` and the loop ticked on at the same cadence forever, with no metric and nothing to alert on. The call still ended correctly at the armed expiry, so the *outcome* was right while the *reason* was invisible — "the callee is refusing our refreshes" and "the callee hung up" looked identical until the deadline passed. Ported from upstream's fix for the same gap in its own timer (siphon-rs #98 / #93):
  - **A non-2xx answer is a failure, not health.** `422 Session Interval Too Small` and `503` come back as `Ok(response)`, so they were previously indistinguishable from a successful refresh while the session was quietly left to expire.
  - **`408`/`481` is terminal on the first occurrence**, whatever the threshold — the peer is saying the dialog does not exist and no number of retries brings it back (RFC 3261 §12.2.1.2); continuing just burns CSeqs against a dead dialog.
  - **Two new counters.** `siphon_ai_session_refresh_total{result=ok|rejected|failed}` scores every attempt, and `siphon_ai_session_refresh_stopped_total{reason=dialog_gone|exhausted|unresolvable}` fires when the loop stops while the call is still up — **that one is the alertable signal**, since every increment means nothing is keeping the session alive. Like upstream, giving up does not BYE the call: RFC 4028 §10 suggests the refresher tear it down, but that is the deployment's decision, so the loop stops and reports while the armed expiry ends the call at its deadline.
  - **Dead dialogs are identified by the dialog's state, not the response code.** `408`/`481` never arrive as a code — `apply_in_dialog_response` maps them to `Err` and terminates the dialog on the way through — so the first cut of this change matched `Some(408) | Some(481)` and was dead code that live traffic never hit: a real `481` came back as `Err("Received 481 for in-dialog …")`, scored an ordinary failure, and the loop retried a dialog the peer had already declared dead. Reading the state (as upstream does, for the same reason) also catches an owner that hung up while a refresh was in flight. Verified live: a callee answering the refresh `481` now stops after exactly one attempt instead of two.
  - **The give-up threshold is 2, not upstream's default of 3, because 3 can never be reached.** Refreshes run at `Session-Expires/2` while the expiry sits a full `Session-Expires` past the last success, so exactly two attempts fit inside a dying session regardless of the negotiated value — a third would mean the loop never announces giving up, which is the silence this change exists to remove. A test pins the arithmetic so a future "let's be more robust" bump fails loudly instead of quietly restoring it. When this was written the second attempt landed *on* the deadline, so `exhausted` lost the race to the expiry and never fired — but #490, in this same release, moves every tick a five-second guard earlier, which puts the last attempt ten seconds clear and makes `exhausted` reachable on a 90 s timer. `dialog_gone` remains the branch that fires deterministically (terminal on the first `408`/`481`), and any non-`ok` `siphon_ai_session_refresh_total` is still the earliest signal; `docs/DEPLOY.md` carries the corrected version of this.
  - **Not** switched to upstream's `CallHandle::start_session_timer`, which is what #484 originally proposed on an incorrect reading of siphon-rs #96. That fix stopped the upstream timer refreshing a private clone, but it still refreshes `CallHandle::dialog` — and this daemon does not use that dialog on an outbound leg. `OutboundCall::dialog` is a snapshot taken at answer time and moved into the leg's own `shared_dialog`, which every in-dialog request (REFER, hold/resume, park, teardown BYE) resolves from and commits back to. Switching would put refreshes on one dialog and the BYE on another: the duplicate-CSeq BYE a record-routing carrier answers `408`, which is issue #353 again. Unifying them is a real refactor of `DialogSource::Direct` — a synchronous `parking_lot::Mutex` becoming the upstream async lock, across hold, resume, REFER, park and teardown — and stays tracked in #484.

- **The session-refresh re-INVITE and the teardown BYE no longer go out on the same CSeq** (issue #490). On an outbound leg whose refreshes are being rejected, the two were measured 564 µs apart carrying `CSeq: 3 INVITE` and `CSeq: 3 BYE` in one dialog — two different requests, one sequence number, which RFC 3261 §12.2.1.1 forbids. A peer entitled to reject that BYE leaves the leg (and its billing) up at the far end: the leak #480 armed the expiry to close, reopened at the moment the expiry fires. Reproduced 3/3 against the shipped 0.48.15 binary; SIPp answers the colliding BYE anyway (it matches on Call-ID), which is why the 0.48.14 verification did not see it. Two independent halves:
  - **The consumed CSeq is now published before the request leaves, not after it is answered.** `DialogControl::commit` writes the advance back post-response, so during the round-trip the shared dialog still read the *pre-request* number and anything else on the leg picked it up — the teardown BYE did. `DialogSource::reserve_cseq` now takes the number on the shared dialog up front; it never runs the sequence backwards, so two in-flight requests get two distinct numbers. Applied to the refresh loop, to hold/resume (inside `send_reinvite`), and to REFER, all of which had the same window — the session timer just made it deterministic rather than a coincidence. Inbound legs never had it: upstream re-inserts the advanced dialog into the shared `DialogManager` before the send, so the number is published already.
  - **No refresh tick lands on the deadline it protects.** Ticks recurred at exactly `Session-Expires/2` while the armed expiry sat at `Session-Expires` past the last *successful* refresh, so the second tick after any failure coincided with the teardown by construction. Refreshes now run a five-second guard ahead of the half-way point, which puts twice the guard between the last attempt and the deadline. Refreshing early is always safe (RFC 4028 §10 — the peer restarts its timer from whenever the refresh lands), and the gap is also the window in which a rejected refresh gets to be logged before the call ends: `exhausted` is now reachable on a 90 s timer instead of losing the photo finish to the expiry. `docs/DEPLOY.md`'s metric row said so and has been corrected.
  - Every refresh attempt now logs its `cseq`, so this class of defect is legible in the field rather than only in a packet capture.

## [0.48.15] - 2026-08-12

### Fixed

- **A registrar's unsolicited MWI no longer reads as a protocol fault** (issue #486). A PBX we register to pushes `NOTIFY Event: message-summary` (RFC 3842) at the account immediately after every successful REGISTER — the default for FreeSWITCH and Asterisk when the account has a mailbox — and SiphonAI answered every one with `489 Bad Event`, because `dispatch_notify` supported exactly one package (`refer`, for post-REFER progress). RFC-defensible in isolation, but it meant `siphon_ai_notify_total{result="bad_event"}` climbed at the registration refresh rate on a perfectly healthy daemon: measured on the production node at **one per minute, ~1,440/day, indefinitely** (100% of NOTIFYs received — 38 of 38 in the sampled window — were this one shape from the registrar). The counter therefore could not distinguish a healthy node from a broken one, and a genuinely unexpected event package landed in the same bucket, invisible. Same defect class as #474 in 0.48.14: there a counter that was always absent, here one that is always climbing.
  - MWI is now absorbed — `200 OK`, discarded, no WS surfacing — which is what every hard phone on that PBX already does, and what a bridge with no mailbox to display should do. Scored **`result="ignored"`**, deliberately *not* folded into `accepted`, so absorbed MWI stays distinguishable from post-REFER transfer progress (the two now share a `200`, so the label is the only thing that separates them — there is a test pinning exactly that). `bad_event` goes back to meaning what its name says and should sit at zero.
  - **Not conditioned on `Subscription-State: terminated`.** SiphonAI never sends a `message-summary` SUBSCRIBE, so every MWI NOTIFY it can receive is unsolicited by construction; gating on `terminated` (the flavour FreeSWITCH stamps) would re-open the same noise against a PBX that uses `active`.
  - **`message-summary` is deliberately absent from `Allow-Events`**, which still advertises `refer` alone. That header states what we would accept a SUBSCRIBE for, and there is no subscription state machine here — absorbing an unsolicited push is the narrower claim, and conflating the two would invite the registrar to establish MWI subscription state we cannot honour.
  - The `result` label now travels with the response out of `dispatch_notify` instead of being recovered from the status code, which had become ambiguous once two distinct outcomes both answered `200`. Documented in `docs/DEPLOY.md` (metric row) and `docs/REGISTRATION.md` (new "MWI pushes from the registrar" section, a FreeSWITCH vendor note, and a troubleshooting row).

## [0.48.14] - 2026-08-12

### Changed

- **Bumped siphon-rs `9ae6ce2cee5a` → `b71a75f1eacf`**, picking up two fixes to issues this project filed: **#91** keys the ingress rate-limit warning throttle by source IP (a process-wide static meant a multi-source flood named one peer and hid the rest, siphon-ai's siphon-rs#90), and **#94** schedules UAC session refreshes at `Session-Expires/2` instead of `max(90, se/2)` with an immediate first tick (siphon-rs#92). No glue changes were needed. Verified with the full workspace suite plus the SIPp signalling regression: 35 of 38 scenarios pass, and the same three — `outbound_uas_answer`, `attended_transfer`, `barge_in_pause` — fail identically on `main` before the bump, so they are pre-existing on this host (two need capabilities/services the box doesn't grant; the harness also collides with a running daemon on `:9091` until pointed elsewhere). **Two of those three were the harness, not the host** — see the bump below.

- **Bumped siphon-rs `b71a75f1eacf` → `b9f5a3bf66f2`**, closing out the session-timer defects this project's #477 work surfaced upstream: **siphon-rs #96** makes `CallHandle::start_session_timer` refresh the caller's *own* dialog instead of a private clone (siphon-rs#95 — the clone advanced a CSeq that never reached the owner, so a later BYE reused a consumed one and drew a 408 from a record-routing carrier), and **siphon-rs #98** stops a doomed refresh loop instead of retrying forever, reporting `Idle / Healthy / Failing{consecutive} / Stopped{reason}` on a new `session_timer_state()` watch channel, treating 408/481 as terminal on first occurrence and finally counting non-2xx responses (422, 503) as failures rather than health (siphon-rs#93).
  - **Neither changes what this daemon does on the wire.** Nothing here calls `start_session_timer`: the outbound refresh added in this same release drives `DialogControl` directly, precisely because of the clone bug #96 has now fixed. Both land as a correctness floor under a future switch to the upstream timer — worth making for #98's failure signal, which the hand-rolled task lacks entirely, but a behaviour change to every outbound leg and so deliberately not folded into a pin bump. Tracked in **#484**.
  - Verified with the full workspace suite (**1,145 tests, 0 failures** across 57 binaries), `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings` (clean), and the SIPp signalling regression: **37 of 38 scenarios pass**, with `barge_in_pause` the only failure and failing identically on `main` before the bump (the call hangs with `frames_echoed=0` and is force-terminated at the 30 s drain timeout).
  - **Correction to the entry above:** `outbound_uas_answer` and `attended_transfer` were never host-capability failures. Every auxiliary phase spawns its own echo WS server from `examples/echo-ws-server-python/.venv`, falls back to system `python3` when that venv is absent, and that interpreter has no `websockets` module — so the server died on import, the daemon logged `cause=WsDisconnect`, and the scenario failed for reasons having nothing to do with SIP. Creating the venv the runner documents takes the suite from 35/38 to 37/38. The `:9091` collision with a running daemon is real, and is in `configs/local-dev.toml`'s `[observability].http_listen` as well as the runner's own port constants; `SIPHON_AI_CONFIG` overrides the former.

### Fixed

- **Outbound legs now honour an RFC 4028 timer the callee imposes** (issue #477). Inbound calls have had full session-timer handling for releases — a negotiation policy, and a `SessionExpired` event that reclaims the call. Outbound legs had none of it: `outbound.rs` and `outbound_service.rs` contained no session-timer code at all, so a callee that answered with `Session-Expires` and then stopped refreshing left SiphonAI holding the leg, and its billing, open indefinitely. Reproduced against a UAS that answers `Session-Expires: 90;refresher=uac`: before, the call was still up 140 s later with nothing sent; now it is torn down at the 90 s deadline (`RFC 4028 session expired; tearing down call`, CDR cause `local_shutdown`).
  - **SiphonAI now refreshes when the callee nominates it** (`refresher=uac`), at half the interval, with the armed expiry left underneath as the backstop for a refresh that fails — a rejected or failed refresh therefore ends the call at the deadline instead of silently pretending the session is alive. Verified live: a callee imposing `Session-Expires: 90;refresher=uac` gets refreshes at 45/90/135 s with the CSeq advancing 2→3→4, and the call outlives the deadline that previously killed it. Refreshes are driven through `DialogControl` (resolve → send → commit) rather than upstream's `CallHandle::start_session_timer`, which refreshes a *clone* of the dialog: the CSeq it advances never reaches ours, so the teardown BYE would reuse a consumed CSeq and get 408'd by a record-routing carrier (the issue #353 failure).
  - **Still no `Supported: timer` on outbound INVITEs.** That is deliberate: advertising support would let a *compliant* callee legally nominate us as refresher (RFC 4028 §7.1), which is a behavioural change to every outbound call rather than only the SBC-shaped ones that nominate us today — and it needs an upstream API that can set the caller-ID and extra headers on the same INVITE, which `invite_with_from`/`invite_with_headers` cannot do. Reading the callee's `Session-Expires`, arming an expiry against it, and refreshing when we are nominated anyway gets the protection without that change.
  - The expiry is the *same* `SessionTimerManager` the inbound path uses, shared via a new `SessionTimers` handle rather than a second manager — `subscribe()` is one-shot upstream, so a second manager would mean a second fan-out task and one of them silently receiving nothing. Parsing is upstream's `SessionExpires`, which enforces the 90 s floor, so a bogus tiny value cannot arm a timer that tears down healthy calls.

### Documentation

- **Documented the RFC 4028 session-timer knobs and what SiphonAI does with them** (issue #478). `[sip].min_session_expires_secs` and `[sip].preferred_session_expires_secs` have been accepted, validated and wired into the negotiation policy for several releases while appearing nowhere in `docs/CONFIG.md` — an operator had no way to learn they exist. Both now have rows, including the parts that only show up in production: an offer below Min-SE is rejected `422 Session Interval Too Small`, a *refresh* below Min-SE is also 422 but leaves the existing dialog and timer running so the peer can retry, and a configured value under the RFC floor of 90 fails startup.
  - The behaviour around them is now written down too. **On an inbound call SiphonAI always nominates the caller as refresher**, overriding `refresher=uas` if the peer asks for it, because it does not refresh an inbound leg itself — there is no setting that changes this. (Outbound legs are a separate matter: per the entry above, a callee that nominates *us* gets refreshed.) The operator-facing consequence is that a peer which negotiates a timer and then stops refreshing **loses the call at the deadline by design**, showing up as a clean teardown with one log line (`RFC 4028 session expired; tearing down call`), a CDR cause of `local_shutdown`, and a duration suspiciously close to the negotiated `Session-Expires`. "Calls die at exactly N seconds" is now searchable.

### Fixed

- **`siphon_ai_outbound_audio_frames_dropped_total` is now published at zero from startup** (issue #474). `describe_counter!` registers the `# HELP` text, not the series — a counter does not exist in `/metrics` until something increments it. For a counter whose *healthy* value is zero and whose every movement is the alert, that makes "nothing was dropped" and "this build has no instrumentation" render identically, and the transition an operator watches for is the one they cannot see. Measured on 0.48.13: a clean one-call run exported 59 `siphon_ai_*` series and the playout counter was **absent**; a WS server bursting 100 frames on connect (so the 200 ms playout window must evict, PROTOCOL.md §5.5) made it appear at **85** alongside the "faster than realtime" warning. The counter was always correct — it was just invisible while the news was good. This is why the Playout SLO in `test-harness/load/LOAD_TEST_PLAN.md` §4 has been recorded as *unverified, not passed* on both the 0.48.10 and 0.48.13 tier-1 load runs, while every other §4 SLO was measured.
  - Published via a new `metrics::publish_zero_baselines()` at recorder install, the same treatment `siphon_ai_sip_rate_limited_total` already gets (#464). Deliberately limited to **unlabeled** counters: publishing a labeled series means inventing label values before anything has happened — `siphon_ai_room_frames_dropped_total` would need a `stage` × `side` matrix — which trades one wrong signal for another. Labeled drop counters stay with the code that owns their label space, as the HEP sampler does.
  - **Metric names at the emit sites now come from the constants that declare them.** `media-glue` and `sip-glue` were spelling 12 metric names as string literals — including the playout counter above — because they depended on the `metrics` facade but not on `siphon-ai-telemetry`, so the constants were out of reach. `room.rs` had gone further and re-declared four names locally under a comment asking the reader to keep them in step by hand. A rename in `telemetry` would have left these crates emitting the old name with nothing failing to compile — the same class of silent drift as the missing series. Both crates now take a **`siphon-ai-telemetry` dependency for the name constants**: the edge `core` already has for `DIALOGS_ACTIVE`, with `telemetry` depending on neither, so there is no cycle and no new external dependency.

### Documentation

- **Corrected the neural-VAD memory cost in `docs/CONFIG.md`** (issue #472). The row told integrators to "budget ~3× the CPU per call and **~2× the memory per call**" against energy VAD, and then to size for a knee near 200 concurrent calls. The memory ratio was measured at **25** concurrent and does not survive being applied at 200: energy's per-call figure there is mostly fixed overhead divided by a small N (0.76 MB/call) rather than a marginal cost, while neural's is a genuine per-call model instantiation that barely moves with concurrency. Using the same formula on the 200-concurrent column of the original data gives **7.5×**, not 2×; re-measured on 0.48.13 it is 6.5× at 25 concurrent and **8.8× at 200**. Sizing a 200-call node off "2×" budgets ~120 MB where ~400 MB is needed. The row now quotes the marginal delta (**~1.9 MB/call**) and totals at a stated concurrency instead of a ratio, since a ratio between a cost that amortises and one that does not is meaningless without naming the N. The CPU half of the claim was re-measured and is correct (2.9×). `test-harness/load/RESULTS-0.48.10.md` §7.2, which CONFIG.md cites as its source and which contradicted itself on this point ("~6× the per-call memory" in the narrative against "~1.6 vs ~0.79" in the bullet), is corrected in place.

## [0.48.13] - 2026-08-10

### Fixed

- **Finished calls no longer leave their SIP dialog in the shared store forever** (issue #458). `sip-uas` inserts one confirmed dialog per accepted INVITE and siphon-ai never removed it, so `DialogManager` grew for the life of the process — the per-call RSS growth reported against 0.48.10. **The memory was the smaller half of the problem:** `sip-dialog` caps the store at `MAX_CONFIRMED_DIALOGS = 10_000` and `sip-uas` discards the resulting `insert` error (`let _ = ...`), so a daemon past ten thousand cumulative calls silently stops tracking new dialogs altogether — and in-dialog requests (BYE, re-INVITE, the post-REFER NOTIFY, transfer) resolve through exactly that store. At 10k calls/day that is a one-day fuse on a busy node, and nothing in the daemon reported it. Located with a `dhat` heap profile diffing a 100-call run against an 800-call run at constant concurrency: `DialogManager::insert` ← `sip_uas::accept_invite_with_session_timer` ← `BridgingAcceptor::on_matched`, ~976 B/call, with the retained dialog's contact URI and two transaction-metrics `String`s never freed alongside it. Ruled out on the way: metrics cardinality (series count flat at 60 across 4,000 calls), the call registry (empty after every batch), multi-arena glibc fragmentation (`MALLOC_ARENA_MAX=2` was marginally *worse*), untrimmed heap (aggressive `MALLOC_TRIM_THRESHOLD_` changed nothing), and the transaction map (genuinely bounded — it evicts oldest at `max_server_transactions`).
  - The fix is a **deferred reaper** (`crates/core/src/dialog_reaper.rs`) rather than a removal at teardown, because the dialog store is what in-dialog requests match against: dropping a dialog the moment its call ends would race the very BYE that ended it, and a peer retransmitting BYE after our `200 OK` must still get a `200`, not "unknown dialog". Teardown *retires* a `DialogId`; a sweeper removes it once a 32 s grace window (SIP Timer H/J — the BYE retransmission window) has passed. Within that window behaviour is exactly as before. The upstream `terminate()` + `cleanup_terminated()` pairing was the intended shape but is unusable from here — `DialogManager::get` returns a *clone*, so `Dialog::terminate()` marks a copy and leaves the stored dialog untouched.
  - New gauge **`siphon_ai_dialogs_active`**, sampled each sweep, because there was previously no way for an operator to watch this grow — the other half of why it went unnoticed. It should track `calls_active` lagging by the grace window.
  - Verified live over 4,000 calls at constant concurrency: `dialogs_active` returns to **0** after every batch (it previously grew 1:1 with call count), all 4,000 terminated `caller_hangup`, SIPp reported zero failures, and the daemon logged **no warnings and no `481`s** — the retransmitted-BYE path the deferral protects. Steady-state RSS growth after pool warm-up fell from ~2.1 KB/call to ~0.78 KB/call; the residue is allocator overhead and the bounded transaction map, not retained dialogs.

## [0.48.12] - 2026-08-10

### Fixed

- **Spooled webhook/audit deliveries are no longer destroyed after 8 hours** (issue #467). A delivery that exhausted its in-memory `retry_max` was persisted to `spool_dir` and re-attempted by the drain worker — but a hard-coded `MAX_DRAIN_ATTEMPTS = 100`, combined with the drain's capped backoff (`min(10·2^min(a,5), 300)`), meant the entry was deleted after **exactly 8.00 hours** of a receiver being unreachable. The limit was undocumented, unconfigurable, and expressed in a unit no operator could reason about without summing the backoff series by hand, while `DEPLOY.md` described the spool as making delivery "durable" and said only "a poison entry that never succeeds" was discarded. Nothing on the drain path distinguishes a poison payload from a down receiver — both are `Attempt::Transient` — and genuine poison is already handled separately and immediately as a `4xx` (`Attempt::Rejected`), so the attempt cap was defending a case that was already covered while silently destroying good records. Found on a production box where the audit receiver had been down: the spool sat at a flat 126 entries spanning exactly 7.9 hours (steady state *at* the horizon, entries ageing out as fast as they arrived) with **270 audit events permanently lost in a week**, the only signal being a warning that named a limit appearing nowhere in the config. The horizon is now wall-clock and per-sink: **`spool_max_age_secs`**, default **259200 (72 h)** so an outage starting on a Friday evening is still recoverable on Monday, and **`0` disables the age check entirely** — the correct setting for an audit stream that must not lose records, leaving the 10,000-entry file cap as the only bound. Available on all four spooling sinks (`[webhooks]`, `[cdr.webhook]`, `[audit.webhook]`, `[quality.webhook]`). The drop log line now names the limit that fired (`spooled delivery exceeded spool_max_age_secs`, with `age_secs` and `spool_max_age_secs`) rather than saying "max attempts", which read as the configured `retry_max` — a different counter at a different layer, and the confusion that led to finding this. Both the 8-hour horizon and the previously-undocumented 10,000-entry file cap are now in `CONFIG.md` and `DEPLOY.md`.

- **`siphon_ai_webhook_spool_depth` and `..._deliveries_total` HELP text listed only two of four sinks.** Both said "(lifecycle, cdr)" while `audit` and `quality` also use them — the audit sink being the one that mattered above.

## [0.48.11] - 2026-08-10

### Added

- **The per-source SIP ingress rate limit is now configurable and counted** (issue #459, upstream siphon-rs #89). A hard-coded `UDP_RATE_LIMIT_PPS = 200` in `sip-transport` dropped inbound SIP per source IP with **no metric, no config knob, and a warning that carried no count** — and disabling `[sip.admission]` did not disable it, because it sits a layer below. New `[sip].udp_rate_limit_pps` (inbound datagrams/sec) and `[sip].stream_rate_limit_fps` (post-framing SIP messages/sec over TCP/TLS/WS), both defaulting to **200 to preserve existing behaviour exactly**, both accepting `0` to disable, and both validated at config load — the upstream token bucket rejects out-of-range values only when it is built, which is after the listeners are up. Drops are exported as `siphon_ai_sip_rate_limited_total{transport}`, pre-registered at zero for every transport so an alert never has to tolerate an absent series. The source IP is deliberately not a label (unbounded, and spoofable on UDP); the throttled `sip_transport` WARN names the peer and now carries a cumulative `dropped_total`. Applied once at startup, so **a config reload cannot change either limit** — and if a packet somehow beat the setter, the daemon warns that the configured value did not take effect rather than running silently at a different cap. Verified live: at the default, 120 cps from one source dropped **2279 packets, all counted**, where the same load previously vanished into a bare warning; raising the limit to 5000 took the identical load to **zero** drops; `0` disables. Note the calls still completed in all three cases — SIP retransmission masks the drops as added setup latency rather than failures, which is why this went unnoticed for so long.
  - Wiring this up required siphon-ai to implement `sip_observe::TransportMetrics` (new `crates/telemetry/src/transport.rs`) and install it with `set_transport_metrics`. The upstream hook is **defaulted to a no-op**, so an embedder that installs nothing — as siphon-ai did until now — gets the `NoopTransportMetrics` fallback and sees nothing at all. Only `on_rate_limited` is implemented; the other hooks are deliberate no-ops, because siphon-ai already derives that signal at the call and dialog layers with better attribution, and counting raw packets twice would add cardinality without answering anything in DEV_PLAN §11.8.

- **HEP shipping now exports metrics, and a dead collector is no longer silent** (issue #460). `siphon_ai_hep_packets_sent_total` and `siphon_ai_hep_packets_dropped_total{reason="queue_full"}` were documented in five places — including `crates/telemetry/src/hep.rs`'s own module doc, which claimed "the drop counter is surfaced via metrics" — and existed nowhere: `/metrics` carried 53 `siphon_ai_*` series and not one HEP series, so HEP.md's numbered troubleshooting runbook sent operators to two metrics that could never appear. Neither needed an upstream change; `hep_rs::UdpHepSink` already exposes `sent()` and `drops()`, and siphon-ai simply never read them. A sampler mirrors both into the registry every 10 s (`absolute`, not `increment` — the upstream values are monotonic totals, so mirroring them cannot drift the way a locally-computed delta would across a missed tick), publishes once at build so **both series exist from startup rather than registering lazily on first use**, and publishes a final time *after* the shutdown drain so end-of-life CDR chunks and close-race drops land in the last scrape. The counters carry no `{type}` label because SIP chunks are emitted by `sip-hep` and RTCP/QoS by `forge-hep` — neither passes through siphon-ai, so there is no local call site to attribute them at.

### Fixed

- **The default log filter no longer silently discards whole crates' warnings** (found while fixing #460). `DEFAULT` in `bins/siphon-ai/src/main.rs` was an allowlist with no global level — `siphon_ai=info,...,forge=warn` — and `EnvFilter` drops every *unlisted* target outright. `hep_rs` was not listed, so upstream's throttled "HEP UDP send failed ... Connection refused" warning, which is the daemon's only signal that a Homer collector is unreachable, never reached a sink; **(Corrected after release — the original entry overstated this.** It claimed the same omission muted 38 `warn!`/`error!` sites across seven first-party crates. That was wrong: `EnvFilter` matches targets by *prefix*, so `siphon_ai=info` already covered every `siphon_ai_*` crate and none of them were ever muted. The targets actually dropped were those matching no listed prefix — `hep_rs`, the siphon-rs crates other than `sip_uas`/`sip_transaction`/`sip_transport`, and every third-party dependency. Verified against production on 0.48.11: `hep_rs` and `sip_uac` both went from **zero** lines in three days to logging normally once the floor was in place, while `siphon_ai_http` was emitting 663 lines throughout.) The directive now leads with a bare `warn` floor and lifts the first-party crates to `info` on top of it, which makes the filter fail-safe: a new crate, or any dependency, can no longer be muted by omission — only turned *up* by naming it. The upstream `sip_*` / `forge*` entries are gone as redundant, since the floor puts them exactly where their old `=warn` directives did. Two tests drive a real subscriber and assert on what it emitted, rather than pattern-matching the directive string — the string is what regressed, so it can't be the oracle. Note the precedence is replace, not merge: a `RUST_LOG` narrowing to one target drops the floor with it.

- **Corrected what the HEP docs promise, in `HEP.md`, `CONFIG.md`, `DEPLOY.md` and `DEV_PLAN.md`** (issue #460). `siphon_ai_hep_collector_up` is **not** implemented and is no longer claimed anywhere: `sent()` counts wire successes and `drops()` counts queue-full, so a *refused* send is counted in neither, and there is nothing to derive the gauge from without a send-failure counter in `hep-rs`. Also struck: the `{type}` labels, the `collector_down` / `encode_error` drop reasons, and `siphon_ai_hep_send_duration_seconds`, all specified in DEV_PLAN and never built. HEP.md's runbook is rewritten to start from the startup line and the `hep_rs::udp` warning before reaching for metrics, and DEV_PLAN's R12 mitigation now describes what exists instead of what was planned. One correction is measured rather than reasoned: **`siphon_ai_hep_packets_sent_total` does not go flat when the collector dies.** The sink writes to a *connected* UDP socket, so the refusal surfaces as `ECONNREFUSED` on the *following* send — one send consumes the queued ICMP error and the next finds the queue empty and succeeds. Against a dead collector the counter kept climbing at almost exactly half rate (35 of ~70 attempted), which no threshold can distinguish from a quiet period; the docs now say to alert on the log line and explain why.

- **`docs/CONFIG.md` no longer under-prices neural VAD by ~6×** (issue #461). The `vad = "neural"` row quoted "~60–80 µs per 32 ms audio window per call" — 0.19–0.25% of a core — as the only sizing figure available to an integrator. Measured on 4 vCPU at 8 kHz, away from saturation so contention couldn't inflate it, the cost is **+1.47% of a core per call**, about 470 µs per window. The row now carries that figure, the previously undocumented **~1.6 MB of RSS per call** (against energy's ~0.79 — and it is per call, not a one-time model load), the fact that the model loads on the **first call** rather than at startup so an idle daemon's RSS says nothing about its footprint, and a sizing rule: on 4 vCPU the knee lands near 200 concurrent rather than the 250+ energy sustains. Absolute numbers are CPU-dependent, so the row says to re-measure on your own hardware.

- **The 491-glare backoff for hold/resume re-INVITEs now follows RFC 3261 §14.1's role split** (issue #454). Both call directions retried after a fixed 250 ms, which sits inside the *non-owner's* 0–2 s band — correct for inbound legs (SiphonAI doesn't own the Call-ID there, verified live on 0.48.10), wrong for outbound legs, where SiphonAI **is** the Call-ID owner and the RFC prescribes 2.1–4.0 s. The two bands are disjoint on purpose — after a collision one side is guaranteed to re-offer first, so the glare cannot repeat — and on the outbound path that guarantee was forfeited: a peer retrying fast could collide a second time and spend the once-only retry, failing the hold (`hold_failed`; the call stays in its prior media state, so the blast radius was one unheld call — low severity, but the collision-avoidance guarantee is the whole point of the scheme). The backoff is now chosen from the role's band — outbound 2.1–4.0 s, inbound 0–2 s — and **randomised within it**, also per the RFC: the previous fixed value meant two SiphonAI instances glaring at each other would retry in permanent lockstep. Jitter comes from std's OS-seeded `RandomState` (no new dependency; desynchronisation is all §14.1's randomness buys). A unit test pins both bands, their disjointness, and that the value actually varies. The functional test plan's HOLD-04 row, which expected 2.1–4.0 s unconditionally, now states the per-role bands.

### Documentation

- **Published tier-1 load & capacity results for 0.48.10, including §7.2's feature-cost deltas** (`test-harness/load/RESULTS-0.48.10.md`, follow-up to #456). 4 vCPU, G.711, plaintext loopback: **≥250 concurrent calls** and **≥75 cps** with every quality SLO held — both floors rather than measured ceilings, since the ramp ended where the plan said to stop with two-thirds of the CPU idle and the rate ramp ended on the *generator's* headroom, not the bridge's. Per-call CPU **falls** with concurrency (1.10% → 0.52% of a core), fds are exactly `12 + 3 × calls`, threads never leave 5, and 375 calls offered against a 250 cap shed exactly 125 with `503` and no brown-out for admitted traffic. §7.2 prices the features one at a time at a fixed 200-concurrent reference point: **HEP is free** (+1%), **recording costs +81%** CPU/call and is the only variant to lose packets (0.169%, MOS floor 3.530 — inside SLO, but "recording is free" is not supportable), and **neural VAD costs +191%** CPU/call and ~6× the memory, moving the knee from ≥250 to below 200 on this hardware. Two findings came out of the run and are filed but unfixed: **#458** (RSS grows ~6–10 KB per *completed call* — tracks cumulative calls, not concurrency and not frames; ~60–100 MB/day at 10k calls/day) and **#459** (an undocumented hard-coded 200 pps per-source SIP cap that drops silently, and that disabling `[sip.admission]` does not disable). The run also corrected the plan where it was unfalsifiable or wrong: the §4/§5 setup-latency SLO moved 250 ms → 200 ms (`SDP_NEGOTIATE_BUCKETS` tops out at a finite `0.2`, so a 250 ms bar could be satisfied but never falsified), §6.3's "RSS within 10% of idle" criterion was replaced with the two tests that actually separate pooling from a leak, three metric names were corrected, and `rtp_packet_loss_ratio`, `ws_connect_seconds` and `outbound_audio_frames_dropped_total` are recorded as **not exported** — the last of which leaves §4's Playout SLO with no data source, so it is published as unverified rather than passed. §7.2 itself no longer demands a full re-ramp per feature, and now says to take the marginal cost at low concurrency: neural VAD reads +1.03 %/core per call at 200 concurrent but +1.47 at 25, and its first-audio p95 of 198 ms at 200 concurrent is saturation (18 ms at 25), not an inherent cost of the model. Both references to HEP drop metrics are struck — see #460.

- **The load-harness README now runs against `configs/soak.toml`, which is sized to its own plan** (follow-up to #456). The README's run commands still started the daemon with `configs/local-dev.toml`, which fails the harness three ways: it listens on **5070** while every SIPp command targets **5060** (the documented soak could not even connect), its 100-port RTP range caps at 50 concurrent calls, and its default 60 s inactivity watchdog reaps the burst's signalling-only calls at the one-minute mark. The run commands now use `configs/soak.toml` — and `soak.toml`'s RTP range widens `[40000, 41100]` → `[40000, 42000]` to match LOAD_TEST_PLAN.md §1.1's own sizing rule (2 × target concurrency plus teardown headroom; §11's human reference call is call number 501). Prerequisites gain the `ulimit -n 65536` step (each call ≈ 4 fds — the 500-burst blows through the default 1024), and the plan's §1.1 table and §1.3 now name `soak.toml` as the shipped baseline. Verified live: daemon on `soak.toml`, echo server, one SIPp call through 5060, clean CDR.

## [0.48.10] - 2026-08-08

### Fixed

- **A consent announcement cut short by a hold or park now fails closed** (issue #445). Since 0.26.0, a `Park`/`Hold` arriving mid-prompt resolved the announcement with its partial play time, and the controller treated any completion as consent: `RecControl::Start` fired, audio was captured, and the CDR affirmatively stamped `consent { announced: true }` with the partial duration — possibly the incoherent `announced: true, announcement_ms: 0` when the cut landed inside the first 20 ms tick (an announce arriving while already held was "skipped" the same way). Reachable in production shapes: the `on_ws_failure` auto-park (the WS connects in parallel with the prompt), an early server `hold`, or an operator park landing during the announcement. A partially heard prompt is not consent — maintainer policy call on the issue's three options was **fail-closed**: capture does not start, `consent { announced: false }` is stamped, and the outcome follows the 0.48.9 rules — `recording_result = "blocked"` when the call was actually going to record (`mode = "always"`, or an on-demand `start_recording`), the #446 deferred escalation otherwise. The WARN log carries the partial `played_ms`. Operational corollary, now documented in RECORDING.md: a bot that holds or parks callers immediately after answer will fail-close its own recordings — let the prompt finish first. Wire shape: `AnnounceEnd::CutShort` (0.48.9) was built as the seam for exactly this decision; no protocol or schema change.

## [0.48.9] - 2026-08-08

### Fixed

- **A call that ends during its consent announcement now stamps a truthful CDR** (issue #444). The 0.48.8 stamping machinery had four teardown-edge gaps, all in the same resolution path. (1) The common one: a `mode = "always"` caller hanging up mid-prompt broke the controller's biased select before the announce-completion arm could run, and nothing after the loop read it — the CDR serialized with no consent block, no `recording_result`, and no metric, indistinguishable from a call never subject to recording, which is precisely the gap #440 set out to close. The consent stamp is now driven by *an announcement was configured* rather than *the prompt played or failed*, and teardown drains the completion channel — so an abandoned-mid-prompt call reads `consent { announced: false }` with no `recording_result`: recording correctly never started, and the consent block alone tells the story. (2) A WS-failure prompt (`on_ws_failure = "play_prompt"`) preempting an in-flight consent announcement dropped the old completion sender, which the controller read as a play failure and stamped `blocked` — so every call in a WS outage window claimed a broken prompt file and sent the on-call chasing config. The tap now resolves a replaced announcement as `Preempted` (a typed `AnnounceEnd` replaces the raw milliseconds on the completion channel), which is not `blocked`: the outage calls stamp `announced: false` and no result. A dropped completion channel (tap death) likewise no longer claims `blocked`. (3) An idle recording writer that missed the 500 ms finalize deadline or panicked fabricated a `Failed` summary whose `recording_path` named a file that was never created — routing a never-started recording into the `failed`-means-disk alert bucket; the fabrication is now gated on capture having actually been requested. (4) A controller crash (sub-task panic) after the announcement had fail-closed recording lost the `blocked` signal entirely on the error-path CDR; `CallError::TaskJoin` now carries the fail-close flag and consent story out, so the abnormal-teardown population — where the audit trail matters most — keeps it.

- **`mode = "on_demand"`: a broken consent prompt no longer stamps `blocked` on every call** (issue #446). A per-call announcement failure unconditionally fail-closed recording — correct — but also stamped `recording_result = "blocked"` and ticked `siphon_ai_recordings_total{result="blocked"}` on **every** call, including the majority whose server would never have requested recording: a fleet recording ~5% of calls on demand with a bad prompt pushed reported a ~20× phantom recording shortfall. The failure is now remembered per-call and escalates to `blocked` only if the server actually sends `start_recording` (which is refused, keeping the fail-close); calls nobody asked to record log the WARN, stamp `consent { announced: false }`, and report no `recording_result` — there is no recording outcome to report. `mode = "always"` is unchanged. Documented in DEPLOY.md and RECORDING.md.

### Changed

- **`recording_result` is a typed enum end to end** (issue #447). The CDR schema field was a stringly `Option<String>` with a closed four-value vocabulary enforced nowhere; it is now `Option<RecordingResult>`, a serde enum with pinned snake_case wire strings — the same mechanism as `TerminationCause` — so the vocabulary is enforced at the schema layer and a new variant is a schema-version conversation instead of a silently absorbed string. Wire output (JSON and CSV) is byte-identical. Alongside it, the file-wins-over-blocked precedence rule now lives in exactly one place (`CallOutcome::recording_result`, resolved once into the termination view) instead of two byte-identical copies of which only the uncalled one was tested.

## [0.48.8] - 2026-08-08

### Fixed

- **A recording that fail-closes on its consent announcement now says so on the CDR** (issue #440). `[recording.announcement]` is a fail-closed control: if the prompt can't play, the call doesn't record. That part worked on the mainline path (edge caveats found by post-merge review are tracked in #444/#445/#446). What didn't was the record of it — `docs/CONFIG.md` promised the call "shows up as `consent.announced = false`" and the code comment beside the fail-close said the same, but the consent block was gated on the two things that hadn't happened (an announcement having played, or the server having reported consent), so it was omitted entirely. A call whose consent prompt failed serialized identically to a call recording was never turned on for, which is exactly the distinction a recording-consent audit needs; the only trace was a WARN line. The block is now stamped `{ announced: false, announcement_ms: 0 }` whenever an announcement was configured and couldn't be played. The path also incremented **no metric at all** — the recording never finished because it never started — so a bad prompt file pushed to a fleet would silently stop it recording with nothing to alert on; `siphon_ai_recordings_total` gains a `result="blocked"` value, deliberately distinct from `failed` so a bad prompt and a bad disk stay separately alertable.

- **The CDR can now express whether a recording actually landed** (issue #441). `recording_path` was stamped whenever a recording was attempted — including a `failed` one where the file is incomplete or, as reproduced with an unwritable directory, never created at all — while the `ok`/`degraded`/`failed` outcome existed only in `siphon_ai_recordings_total`, a process-wide counter that cannot be attributed to a call. So a downstream uploader, retention job, or compliance export reading CDRs had no way to tell a path naming a good recording from one naming a file that does not exist. Records now carry `recording_result` (`ok` / `degraded` / `failed` / `blocked`), the same vocabulary as the metric. Same shape as #369: an outcome the daemon knew precisely and observed correctly, that the per-call record couldn't say.

  **CDR schema goes to version 8.** The JSON addition is additive-optional on its own, but the field also gives the CSV sink a **50th column** — appended at the end per its documented append-only rule, so ingestors keyed by column *position* survive, while one asserting an exact column *count* does not, which is CLAUDE.md §7.7's "could break parsers" bar. The bump also follows the v4 (`quality`) precedent of letting consumers gate on `version >= 8` rather than probe for the field — the natural thing for a compliance export reconciling recordings against calls. **Upgrade note:** a CSV ingestor pinned to 49 columns needs updating before this release; a JSON consumer tolerant of unknown fields needs nothing.

## [0.48.7] - 2026-08-07

### Fixed

- **The forge histograms actually render buckets now** (issue #437). The 0.48.6 note below claimed the forge-media bump gave "every forge histogram explicit buckets"; that was wrong for this daemon's own `/metrics` — a 0.48.6 instance still rendered `forge_vad_neural_inference_seconds` as a Prometheus summary (quantiles), unaggregatable across instances. `describe_*!` HELP text travels through the `metrics` facade to whatever recorder is installed, but bucket registration is **exporter-side**: forge-media #102 could only export suggested-bucket consts, and `prometheus_builder()` never applied them. It now registers forge-engine's exported buckets for the only two forge histogram families the consumed forge crates emit — `forge_vad_neural_inference_seconds` and `forge_transcoding_duration_seconds` (the conference/webrtc/sdp histograms live in forge-conference and forge-api, which SiphonAI deliberately doesn't consume) — referencing the upstream consts directly so name and buckets track the pin by construction, with a rendered-output test alongside the #431 suite. DEPLOY.md's forge row now points at forge-media's `docs/METRICS.md` and names the two bucketed families.

## [0.48.6] - 2026-08-07

### Changed

- **forge-media bumped `aeae479b391d` → `b4f8df5c8f09`** (PR #435 — forge-media [#102](https://github.com/thevoiceguy/forge-media/pull/102), [#104](https://github.com/thevoiceguy/forge-media/pull/104), [#105](https://github.com/thevoiceguy/forge-media/pull/105), plus dependabot bumps #88/#89/#91/#99). Two of these are visible on this daemon's own `/metrics`, since the embedded forge crates' `forge_*` families export through our recorder: forge #102 gives every forge facade metric a `# HELP` line and every forge histogram explicit buckets (forge's sibling of our #431/#432 sweep — `forge_vad_neural_inference_seconds` previously rendered as an unaggregatable summary), and forge #105 makes `forge_rtcp_sender_{packets,bytes}_total` count per-SSRC deltas from received sender reports instead of summing running totals, so they now grow linearly as wire counts do rather than superlinearly. forge #102 also renamed six unprefixed forge-api metrics — forge-api is not a crate SiphonAI consumes, and nothing here referenced the old names. forge #104 moves forge to the digest-0.11 stack (sha1 0.11 / hmac 0.13 / sha2 0.11); SiphonAI's own hmac 0.12 / sha2 0.10 now build alongside as duplicate versions — harmless, to be deduped with our own digest move. The `metrics` facade stays 0.24 on both sides (exporter lockstep preserved), and forge's `external/siphon-rs` submodule is unchanged. siphon-rs (`daee496e1a17`) and hep-rs (`91e689b`) were audited in the same pass and were already at their latest upstream commits. No SiphonAI behavior change; full workspace tests plus all 38 SIPp integration scenarios pass on the new pin.

## [0.48.5] - 2026-08-05

### Fixed

- **An honest peer reusing a still-valid nonce no longer reads as a credential attack** (issue #430). Upstream `sip-auth` rejects a correct digest over a known, unexpired nonce as soon as more than 10 s (its `max_request_age`) has passed since that nonce's last successful authentication — *before* the response digest is ever computed — and that rejection collapsed into the same `Ok(false)` as a wrong password. SiphonAI therefore answered `401 stale=false` and scored `siphon_ai_sip_auth_total{result="failed"}` plus a `sip_auth{result:"failed"}` audit event, byte-identical to a bad-credential attempt, for any UAC that caches credentials and authenticates pre-emptively (PJSIP-based UAs by default) whenever its calls are more than 10 s apart. Any SIEM rule counting `failed` events fired on honest traffic. Via a siphon-rs bump ([thevoiceguy/siphon-rs#87](https://github.com/thevoiceguy/siphon-rs/pull/87) — a `nonce_reuse_expired` discriminator in `nonce_is_stale`'s post-hoc-query style), the rejection is now mapped where RFC 7616 §3.5 points: a `401 stale=true` re-challenge (nonce unacceptable, credentials not implicated — the peer retries silently against the fresh nonce), scored and audited `stale`. `failed` now means exactly a credential mismatch. The two nonce windows also become configurable — `[sip.auth].nonce_ttl_secs` (default 300, must be ≥ 1) and `[sip.auth].nonce_reuse_window_secs` (default 10; `0` disables the reuse window so nonce reuse is bounded only by the TTL, for operators who'd rather skip the extra 401 round-trip entirely) — previously both were hardwired upstream defaults. Documented in CONFIG.md (`[sip.auth]`) and DEPLOY.md (metric semantics).

- **`[route.media].dtmf` and `[route.media].srtp` typos now fail startup instead of silently inheriting the global.** Both route overrides were accepted unvalidated: `dtmf = "of"` (or any unknown token) loaded fine and behaved as "inherit the global RFC-2833 payload type" — only an exact `"off"` disabled telephone-events — and an unknown `srtp` token loaded and warn-fell-back to the daemon default on every call. Config load now checks both against the same token sets as the global fields (`"rfc2833" | "off"`, `"off" | "preferred" | "required"`) and rejects anything else, matching the existing load-time validation for route `vad`, `recording.mode`, `min_attestation`, and `on_ws_failure`. **Upgrade note:** a config that today carries a misspelled value in either field will stop loading — run `siphon-ai check` before upgrading production, same drill as the 0.46.1 strict route keys (#384). Documented in CONFIG.md.

- **Every metric now scrapes with a `# HELP` line, and every histogram renders buckets** (issue #431). Thirteen metric families emitted from sip-glue, media-glue, quality, and the daemon binary had a DEPLOY.md row apiece but no `describe_*!` registration, so they exported as a bare `# TYPE` with no description — found live when `siphon_ai_sip_auth_total` exported four `result` series and explained none of them. Four `rtp_*` quality histograms were also missing from the bucket registrations, so they rendered as Prometheus summaries (quantiles), which cannot be aggregated across instances — while their sibling `rtp_rtt_ms`, recorded three lines away in the same match arm, had both. Buckets: jitter / RX-jitter 1 ms–500 ms (one frame time is 20 ms), packet loss across the full 0.0–1.0 fraction, MOS cut on the conventional quality bands (2.6/3.1/3.6/4.0). The gap was silent because coverage was a hand-written list of seven metrics; it is now self-detecting — `ALL_COUNTERS` / `ALL_GAUGES` / `ALL_HISTOGRAMS` name every metric the telemetry module declares, one test renders each and requires a HELP line, one requires every histogram to render buckets rather than quantiles, and one scans the module's own source so a new metric const that isn't listed fails the build instead of quietly escaping coverage.

### Documentation

- **Stale "deferred" notes swept.** A code-review pass found doc comments still describing shipped work as pending: the acceptor's module header claimed BYE/CANCEL plumbing, CDR/webhook emission, and config-driven `forward_headers` were unwired (all landed with the daemon runtime); the runtime's deferred list still included outbound REGISTER, HEP/Homer, and the admin endpoints; the call controller claimed `clear`/`mark`/`transfer`/`send_dtmf` were logged-only; the webhook schema claimed HMAC signing was a follow-up (`X-SiphonAI-Signature` ships since 0.11.0); the raw config called `recording.mode = "on_demand"` "a later chunk" (shipped 0.5.0); and PROTOCOL.md §3.8 + the WS schema still said `rtp_stats.rtcp_rtt_ms` is `null` "until forge originates its own RTCP SRs (deferred to 0.3.1)" — it populates since 0.3.2. Comments and docs now describe what's actually built; no behavior change.

## [0.48.4] - 2026-08-04

### Fixed

- **A BYE or body-less ACK during the delayed-offer pending window now ends the call immediately** (issue #425). Between our 200-with-offer and the peer's ACK answer, an inbound delayed-offer call has a confirmed dialog but no controller yet — and both teardown signals fell into that gap: the routing handler dropped ACKs without a body before the acceptor could see them, and `dispatch_bye` consulted only the controller registry, so the peer's BYE got its RFC-mandated 200 while the parked call sat — forge session and ports held — until the Timer-H watchdog expired 32 s later and wrote a CDR claiming `ack_timeout` with a 32 s duration for a call the peer had ended in milliseconds (live shape: FreeSWITCH's gateway leg failing its own media setup sends exactly this empty-ACK + BYE pair 2 ms after our 200). Now every in-dialog ACK reaches the acceptor — a body-less one on a held dialog is the peer failing to answer our offer (RFC 3261 §13.2.2.4) and reaps the call as `missing_sdp_answer`, the outcome that was documented in the metric description and CDR schema since 0.9.5 but that no code path ever emitted — and the daemon installs the acceptor as the dialog terminator, so a BYE that misses the controller registry also checks the pending window and reaps the call with the ordinary `caller_hangup` cause and its real duration (CANCEL deliberately does not: RFC 3261 §9.2, no effect after a final response). `siphon_ai_delayed_offer_total` gains the `caller_hangup` result label; documented in DEPLOY.md.
- **Every metric on `/metrics` now carries a `# HELP` line, and every histogram explicit buckets** (issue #431). Thirteen documented metric families — `siphon_ai_sip_auth_total`, `invite_admission_total`, `invite_admission_sources`, `notify_total`, `quality_records_total`, `silence_events_total`, `dead_air_events_total`, the four `rtp_*` quality histograms, and the two `*_tls_reload_attempts_total` counters — were emitted from `sip-glue` / `media-glue` / `quality` / the daemon binary with a DEPLOY.md row apiece but no `describe_*!` call, so they scraped as a bare `# TYPE` with no description (found live while verifying inbound digest auth: `siphon_ai_sip_auth_total` exports four `result` series and explains none of them). The same four `rtp_*` histograms were also missing from the bucket registrations, so they rendered as **summaries** rather than histograms — quantiles that can't be aggregated across instances — while their sibling `siphon_ai_rtp_rtt_ms`, recorded three lines away in the same match arm, had both. Buckets: jitter and rx-jitter 1 ms – 500 ms (one frame time is 20 ms), packet loss over the full 0.0–1.0 fraction, MOS cut on the conventional quality bands (2.6 / 3.1 / 3.6 / 4.0). The gap was silent because coverage was a hand-written list of seven metrics, so this also makes it self-detecting: `ALL_COUNTERS` / `ALL_GAUGES` / `ALL_HISTOGRAMS` name every metric the telemetry crate declares, one test renders each and requires a `# HELP` line, another requires every histogram to render buckets rather than quantiles, and a third scans the module's own source so a new metric const that isn't listed fails the build instead of quietly escaping coverage. No metric names, labels, or values change.

## [0.48.3] - 2026-08-03

### Fixed

- **`srtp = "preferred"` now offers `RTP/AVP` + `a=crypto` instead of `RTP/SAVP`** (issue #422). RFC 3264 answers echo the offered transport profile, so an `RTP/SAVP` offer gives a compliant plaintext-only peer no legal way to answer — it must reject the stream — which made "preferred" behave as "required, against peers polite enough to violate the RFC." Live shape: a FreeSWITCH `originate loopback/9000` makes its gateway leg send an offerless INVITE, our delayed-offer 200 OK offered SAVP, and FS (no SRTP on that leg) correctly gave up — ACK then BYE with `cause=500 "Internal media error"` — while the same peer on the early-offer path (where we merely mirror its `RTP/AVP`) worked fine. The generated SDES offer now follows the policy: `required` keeps `RTP/SAVP` (encryption non-negotiable, unchanged); `preferred` offers plain `RTP/AVP` carrying `a=crypto` — the long-standing optional-SRTP convention — so capable peers answer with their own key and both sides encrypt, and everyone else answers plaintext and proceeds in the clear. The answer path already keyed off the crypto attribute rather than the transport profile, so both outcomes flow through the existing install/downgrade logic; new tests pin the offer shape per mode and both answer outcomes (AVP+crypto → keys installed + `start.srtp.profile`; plaintext → clean downgrade, no profile claimed). Applies to both users of the shared offer builder: outbound origination (`[[gateway]].srtp`) and the inbound delayed-offer 200 OK (`[media].srtp` / route override). Documented in CONFIG.md (`[media].srtp_offer`, `[[gateway]].srtp`).

## [0.48.2] - 2026-07-31

### Fixed

- **SiphonAI stops transmitting RTP while a peer hold forbids it** (issue #417). Answering a peer's `a=sendonly`/`a=inactive` re-INVITE with `recvonly`/`inactive` commits us to silence per RFC 3264 §6.1 — but nothing enforced it: the tap kept pushing WS playout to forge at an unbroken 50 pps for the whole held span (live-proven against FreeSWITCH `uuid_hold`, which honors its own `recvonly` in the mirrored case). The `negotiated_direction` doc comment's "(eventually) pause forge's outbound RTP" was that never-landed enforcement. Now the acceptor recomputes a shared tx-suppression flag on **every** accepted direction change — not just the hold/resume event boundary, since a held→held flavor change (`sendonly` → `recvonly`) crosses the may-send line without emitting anything — and the tap drops every caller-leg push while it is set: WS playout (drain-and-drop, same stance as mute — the WS server is never back-pressured and held-era audio is discarded, not queued), barge-in re-queues, the room mix for a peer-held conference member (only that leg's wire — its server audio still mixes for the other members, whose wires are not held), parked MOH, announcements (which stall to be heard on resume rather than play into a deaf leg), and queued outbound DTMF (telephone-events are RTP too). The recording fork is skipped with it — the caller heard nothing. A peer `recvonly` hold (we answered `sendonly`) correctly leaves transmission flowing. New `siphon_ai_peer_hold_tx_suppressed_frames_total` counter + engage/release log lines with per-hold totals; documented in PROTOCOL.md §3.3 and DEPLOY.md. Scope: inbound legs (the same scope as the §3.3 events and the #402 watchdog park — outbound-leg peer re-INVITEs don't reach this handler today).

## [0.48.1] - 2026-07-31

### Fixed

- **Outbound delayed-offer calls now carry media** (issue #414). The path's forge session was never transitioned `Initializing → Active` — `DelayedOfferAnswerer::generate_answer` answers via `accept_inbound`, which allocates/negotiates/attaches but deliberately does not activate, and unlike the inbound early-offer path nothing downstream compensated with an explicit `start_session`. The call looked healthy at every other layer (200 OK → ACK with a valid answer, WS `start` delivered, `result="answered"` counted) while zero RTP flowed in either direction, since the forwarding task was never spawned. Present since the feature's introduction (0.9.0, #191), masked until 0.48.0 by the #406 SDP-negotiation failures that killed these calls earlier. The generator now activates the session after DTLS/SDES key install and before the answer rides out in the ACK — the peer only learns our RTP address from the ACK, so the session is Active before the first packet can arrive; a new `result="media_activate"` label on `siphon_ai_outbound_delayed_offer_total` counts the (previously impossible-to-see) activation failure, and a regression test pins `SessionState::Active` on this path, mirroring the one that already pinned it for `apply_answer`. Also fixed while there: the generator's post-`accept_inbound` SRTP failure arms (DTLS/SDES post-processing, `enable_dtls`) now roll the session back instead of leaking it and its port pair, and `apply_answer`'s activation comment no longer claims to cover this path — the mis-statement that kept the gap invisible.

## [0.48.0] - 2026-07-30

### Added

- **`siphon_ai_outbound_delayed_offer_total{result=…}`** (issue #406 §3) — the outbound delayed-offer path (offerless INVITE we sent; peer offers in its 2xx, we answer in the ACK) previously emitted no delayed-offer metric at all: every increment of `siphon_ai_delayed_offer_total` was on the *inbound* path. Results: `answered`, `srtp_policy` (the gateway's `srtp` mode refused every offered audio alternative), `srtp_setup` (selected secure alternative failed to negotiate/install), `invalid_remote_media`, `missing_sdp_offer`. Both live failure modes in #406 would have been one-line diagnoses with it. Documented in `docs/DEPLOY.md`.

### Documentation

- **Conference enter/exit tones already exist — `[conference].join_tones` (0.7.0) — and are now discoverable** (issue #404). The live test that motivated the issue ran with the knob at its default (`false`), and nothing outside the CONFIG.md table mentioned it. PROTOCOL.md §4.8 now documents the built-in knob (distinct join/leave pitches, fires on hang-up departures too, mixed room-wide so recordings capture it) alongside the server-side recipe (play audio on `participant_joined`/`participant_left` — a member's WS audio reaches the whole room). A new regression test pins the exact live-observed case: the remaining member hears the leave chime when the other member hangs up (teardown departure, not an explicit `conference_leave`). The issue's proposed `join_tone`/`leave_tone` split was not adopted — the existing combined knob covers the use case with distinct pitches per direction.

### Fixed

- **`hold` on a conference member is now rejected with `error { hold_failed }` instead of silently evicting the leg from its room** (issue #403). Holding a conferenced call dropped its room membership — every *other* member got `participant_left` and the admin roster updated, but the held leg itself never received a `conference_left`, leaving its server with a stale room model in violation of PROTOCOL.md §3.12's every-exit-is-reported contract. Ruling: hold does not stack on room membership (option (a) from the issue — matching the existing no-stacking-on-peer-hold policy, and no protocol shape change): the tap, which owns membership state, refuses the hold; the call stays in the room completely untouched (no fan-out, no roster change), and a server that wants hold semantics sends `conference_leave` first, explicitly. `resume`'s "restores the direct caller↔server pair" contract is unaffected since a held call can now never be in a room. Documented in PROTOCOL.md §4.10.

- **A hold longer than `[media].inactivity_timeout_secs` no longer kills the held call** (issue #402). The inbound-RTP watchdog kept running while the call was held, and a bot-hold's `a=sendonly` re-INVITE tells the caller to stop sending — RFC 3264-compliant silence — so at exactly held+60 s (default) the daemon emitted `error { rtp_timeout }` + `stop { error }` and tore the call down mid-hold, making §4.10's "bot holds → bot resumes" primitive unusable for real-world holds. The watchdog is now parked while the call is held — bot-initiated hold *or* peer-hold (an `a=inactive` peer-hold stops inbound RTP too, same kill) — checked at watchdog-fire time so no hold↔resume transition can race an expired deadline into a spurious teardown, and re-armed with a fresh full window on resume. Genuine RTP loss outside a hold still tears down exactly as before; a resumed call that goes silent gets one fresh window, then dies. Documented in PROTOCOL.md §3.10/§4.10 and CONFIG.md.

- **SRTP policy is now evaluated against the *set* of offered audio m-lines, and the SDES/DTLS answer wiring patches the *selected* m-line instead of the first** (issue #406 §1–2). FreeSWITCH's late-negotiation offer advertises secure and plaintext audio alternatives (`RTP/SAVP` + `RTP/AVP` on one port); the gate, the offer tweaks, and the answer post-processing all read only the **first** audio m-line. Consequences, both measured live: with `srtp` unset (`off`) the whole call was refused with 488 even though the peer offered plaintext (taking an explicitly offered option is not a downgrade); with `srtp = "preferred"` the call answered but crypto was patched onto whichever m-line came first rather than the one the negotiator accepted. The gate now refuses only when *no* offered alternative satisfies the mode; the tweaks select the first satisfying alternative (peer's declared preference order — plaintext-first under `preferred` yields a plaintext call), neutralize plaintext alternatives under `required` so the negotiator can't accept them ahead of the secure line, and the post-processors patch exactly the selected (accepted, nonzero-port) m-line, restoring the offered protocol on rejected echoes per RFC 3264 §6. `media-glue`'s `negotiate_answer` and `audio_remote_addr` likewise now locate the *accepted* audio stream instead of assuming the first m-line is it (the first may legitimately be a port-0 rejection post siphon-rs #85). Builds on the upstream one-m-line-per-type negotiator fix in this release's pin bump. FreeSWITCH profile prerequisites for delayed offer (`enable-3pcc`, `inbound-late-negotiation`) and their distinct failure signatures (instant `480` vs `488`) documented in `docs/FREESWITCH_INTEGRATION.md` (#406 §4).
- **Registration attribution now runs before the `[[trunk]]` allowlist walk, so `register_source = "<register name>"` routes are reachable in configs that also declare trunks** (issue #405). With any `[[trunk]]` block present, an inbound INVITE's `register_source` was computed exclusively by the trunk walk: a registrar whose IP also matched a trunk CIDR got the *trunk* name (silently killing every route matching the registration name), and a registrar outside every trunk allowlist got `403 Forbidden` — making registered-phone mode effectively unusable in production configs, contradicting `docs/REGISTRATION.md`. The gate now consults the registration manager's exact `ip:port` lookup first, then the trunk walk, then 403. A peer we register *to* is operator-declared (`[[register]].server`/`port`) and implicitly trusted. The exact-port match preserves the useful split: a PBX's other profiles (same IP, different source port, e.g. FreeSWITCH's external profile on :5080) keep identifying via the trunk walk. Unregistered peers and no-match 403 behavior are unchanged; precedence documented in `docs/CONFIG.md`.
- **Outbound calls through a digest-authenticated gateway no longer fail instantly as `rejected`** (siphon-rs #83, fixed upstream in siphon-rs #86; pin bump). The UAC never auto-retried an INVITE on a 401/407 challenge — the flag existed but was dead code on the INVITE path — so a perfectly credentialed call through FreeSWITCH/Asterisk-style PBXes that digest-challenge INVITE surfaced the challenge as a terminal rejection ~1 ms after dial (`outbound_failed{cause:"rejected"}`). IP-ACL trunks (e.g. Twilio) masked it. The UAC now builds the authenticated re-INVITE (same Call-ID, CSeq+1, fresh branch) transparently, honoring `max_auth_retries`. `CallHandle::invite_request()` became async upstream (the live attempt is replaced on retry); the one call site adapted.
- **An offer carrying secure and plaintext audio alternatives (e.g. stock FreeSWITCH late negotiation offering `RTP/SAVP` + `RTP/AVP` on one port) no longer gets both m-lines accepted on the same local port** (siphon-rs #84, fixed upstream in siphon-rs #85; reaches the media path via forge-media #100's submodule bump). Both alternatives were accepted in the answer — indistinguishable on the wire, one encrypted and one not — so the dialog established and then no media flowed in either direction for the life of the call (siphon-ai #406 §2's root). The negotiator now accepts the first acceptable alternative per media type and rejects the rest with port 0, per RFC 3264 §6. The remaining siphon-ai-side hardening for delayed-offer alternatives (SRTP policy gate over the m-line *set*, patching the *selected* m-line) is tracked in #406.

## [0.47.2] - 2026-07-29

### Fixed

- **`silence_detected` no longer fires mid-utterance, and the silence stretch is measured from the caller's last speech *end*, not its onset** (issue #399). The idle detector anchored its silence timer at `speech_started` and never checked whether speech was still active, so any caller utterance longer than `silence_threshold_ms` (default 3 s — routine in real speech) emitted `silence_detected` while the caller was still talking, and the once-per-stretch suppression flag then swallowed the event for the *real* silence that followed — exactly inverted behavior. Short utterances fired early, with `duration_ms` inflated by the utterance's own length. The detector now tracks speech-active state (fed from every forge-vad transition, including debounce-suppressed provisional pairs, so a suppressed pair can't wedge the gate), holds both idle events while the caller is speaking, and anchors silence *and* dead-air at speech end — which also fixes the latent sibling: an utterance longer than `dead_air_threshold_ms` would have fired `dead_air_detected` mid-audio. `duration_ms` now reports the true silence length, matching what PROTOCOL.md §3.6/§3.7 always promised; speechless-call behavior (measured from call start) is unchanged. Live-found on 0.47.1: the new `offset_ms` field made the wrong anchor arithmetically visible on the wire (`silence.offset_ms − duration_ms == speech_started.offset_ms`, three-for-three).

## [0.47.1] - 2026-07-29

### Added

- **`offset_ms` extended to `dtmf`, `silence_detected`, and `dead_air_detected`** (completes issue #394 — 0.47.0 added it to the speech events). Same semantics and anchor: monotonic milliseconds between `start` being written to the socket and the event's trigger — the digit's *end* detection for `dtmf`, the detector poll that crossed the threshold for the idle pair (so the idle events' offset carries the same up-to-500 ms poll quantization their `duration_ms` already has). Stamped at detection, saturating to `0` for triggers that predate `start` on the wire. Additive within protocol v1; absent from older daemons; documented in PROTOCOL.md §3.4/§3.6/§3.7, typed in both server SDKs.

## [0.47.0] - 2026-07-29

### Added

- **`offset_ms` on `speech_started` / `speech_stopped` — monotonic milliseconds between `start` being sent and the VAD transition** (issue #394). `ts_ms` carries wall-clock epoch milliseconds (#389/#392), so placing a speech event on the media timeline meant subtracting wall-clock values across two hosts — exposed to clock skew, WS transit jitter on `start`, and NTP steps. The daemon now also stamps the transition against its own monotonic clock, anchored to the instant `start` was written to the socket (the same instant already used as the CDR `first_audio_out_ms` epoch). The stamp is taken when the transition comes off the detector — before any barge-in debounce or pause hold — so a held `speech_started` keeps its true timeline position; a transition detected before `start` hit the wire saturates to `0`. Additive within protocol v1 (optional field, no version bump), absent from older daemons; documented in PROTOCOL.md §3.2, typed in both server SDKs.

### Fixed

- **Sink changes downgraded to restart-required because their durable spool is active are now reported in the `config_reload` audit event's `restart_required` field and the summarizing reload warning** (issue #390). All three sink arms (`[webhooks]`, `[cdr]`, `[audit]`) logged their own specific journal warning but never added the section to the reload's `restart_required` list, so the machine-readable audit trail claimed a plain `"applied"` while deliveries kept going to the old target until a restart — and since a repeat SIGHUP with an unchanged file short-circuits as `no_change`, that one journal line was the only trace a restart was owed. The sections now surface as `"[webhooks] delivery"` / `"[cdr] delivery"` / `"[audit] delivery"` — distinct from the existing `"[audit]"` entry, which means "audit was disabled at startup and enabling it needs the process-global facade installed". Verified live on 0.46.1: an `[audit.webhook].url` edit under an active spool produced a bare-`applied` audit event while probe events and the drained spool kept POSTing to the old url. Sink fingerprints still advance only on a successful swap, so reverting a downgraded edit converges silently with no restart debt (covered by new tests).

### Documentation

- **PROTOCOL.md §3.2 now states that `ts_ms` on speech events is wall-clock Unix-epoch milliseconds, not a monotonic offset from `start`** (issue #389) — the ambiguity that motivated `offset_ms` above. Doc-comment and schema descriptions were corrected in the same pass.
- **The `speech_started` doc comment and schema description no longer claim the event is gated on a nonexistent `bridge.vad = true` flag** — speech events are always emitted; `[media].vad` selects the detection backend only.

## [0.46.1] - 2026-07-28

### Fixed

- **A confirmed dialog's route set no longer changes after dialog creation — teardown BYE after hold/resume on a re-dialed connection is answered immediately instead of timing out** (fixed upstream by [siphon-rs#82](https://github.com/thevoiceguy/siphon-rs/pull/82), pin bumped `376d0e9a5c5e` → `065c2c65a0f7`). `Dialog::update_from_response` refreshed the remote target on a 2xx — correct, RFC 3261 §12.2.1.2 — but **also** overwrote the dialog's route set from the response's `Record-Route`, which §12.1 fixes at dialog creation. Invisible when the response returns over the original connection (same `Record-Route` echoes back), it bit on the 0.45.3/0.46.0 idle-close fallback path: a 2xx returned over a connection *we* re-dialed carries Twilio's transaction-scoped `twnat=sip:<ip>:<port>` NAT hint in `Record-Route`; adopting it as dialog state sent the subsequent BYE to a target only valid for the dead connection — `408` after 32 s, teardown stalled, and the CDR's `ended_at` inflated by the same 30 s (billing-grade). Upstream now recomputes the route set only in the one case RFC 3261 §13.2.2.4 requires it (a 2xx confirming an *early* dialog); in-dialog responses never touch it. Wire-isolated upstream on live Twilio Secure Trunking (hold → resume → BYE now `200` in ~40 ms on a re-dialed connection). Pin bump only — no siphon-ai code change.

## [0.46.0] - 2026-07-28

### Fixed

- **In-dialog requests on an inbound TLS leg re-dialed after a carrier idle-close now reach the right hop and pass certificate verification** (fixed upstream by [siphon-rs#78](https://github.com/thevoiceguy/siphon-rs/pull/78) and [siphon-rs#79](https://github.com/thevoiceguy/siphon-rs/pull/79), pin bumped `4e7d5d7d0bd7` → `376d0e9a5c5e`). Two defects sat under 0.45.3's teardown-on-FIN failover. First, `sip-dialog` unconditionally reversed the Record-Route set — correct only for the UAC side (RFC 3261 §12.1.2); a UAS takes the set as received (§12.1.1) — so the fallback dial targeted the *far end* of the proxy chain (often a portless bottom entry resolving to `:5060`) instead of the top hop's `:5061;transport=tls`. Second, a fresh TLS dial to a hop the carrier Record-Routed **by IP** used that IP literal as the SNI/reference identity, and carrier certs carry no IP SANs, so verification failed. Upstream added `UACConfig.tls_server_name` to carry the trunk hostname as the reference identity — applied only where the SNI would otherwise be an IP literal, never overriding hostname targets or non-TLS transports.
- **The `[sip].tcp_keepalive_interval_secs` keepalive now sends the RFC 5626 §3.5.1 double-CRLF ping instead of a bare CRLF** (fixed upstream by [siphon-rs#80](https://github.com/thevoiceguy/siphon-rs/pull/80)). The single CRLF is the *pong*, and emitting it unsolicited toward the server made Twilio Secure Trunking reap idle connections **sooner** (~82–88 s vs ~127 s with the knob off) — the 0.45.3 feature pushed traffic onto the recovery path more often instead of keeping connections open. CONFIG.md now also carries the honest caveat: a carrier whose idle timer counts *SIP messages* rather than bytes cannot be held open at this layer — measure the connection lifetime with the knob on before relying on it.

### Added

- **`[sip].tls_server_name` — TLS reference identity for client-side dials whose SNI would otherwise be an IP literal** (the siphon-ai half of siphon-rs#79). The daemon-wide transfer UAC drives every in-dialog request an inbound leg sends (hold re-INVITE, REFER, BYE) and has no trunk config to derive a hostname from, so the scenario that actually hits the IP-SNI failure — an inbound Twilio TLS leg re-dialed after an idle-close — needs this knob set to the trunk's hostname (e.g. `example.pstn.twilio.com`). Validated at load (must be a DNS name — an IP literal or empty value refuses to start), logged at startup when set. **Gateways and registrations need no configuration:** a `[[gateway]]`'s `proxy` hostname and a `[[register]]`'s server hostname are passed through as the reference identity automatically (an IP-literal `proxy` has no name to offer and is left unset). Documented in CONFIG.md.

### Changed

- **BREAKING (config): an unknown key inside `[[route]]`, `[route.match]`, or any `[route.*]` override block is now a startup error instead of being silently ignored** (issue #383). Every section in `crates/config` already carried `#[serde(deny_unknown_fields)]` — its doc comment calls that "the right strictness — it catches typos like `auido_sample_rate`" — but no struct in `crates/routes` did, leaving the dialplan the laxest part of the config. Two ways that bit: a misspelled match key was **dropped**, so a route written as "`from_user` **and** `to`" matched on `from_user` alone and, since routes are first-match-wins, quietly stole calls meant for later routes; and a misspelled override key (`ws_uri` for `ws_url`) was indistinguishable from "not overridden", so the route silently bridged to the global `[bridge].ws_url` — a production misroute with a clean `config OK`. The same shape applied to `[route.media]`/`[route.security]`/`[route.recording]`, where a dropped override silently inherits a *less* restrictive global. Unknown keys now produce the same "unknown field `x`, expected one of …" error the rest of the config gives, which for `to` → `to_user` is self-explaining. Unknown **top-level** tables stay tolerated as before (`RawRouteFile` is deliberately lenient — `load_from_toml` is handed whole config files and picks the routes out), and `[route.match.header]` names are map keys, so arbitrary header names are unaffected. **Upgrade note:** a deployed config carrying a stray key in a route block will now refuse to start; run `siphon-ai --config <file> check` before upgrading — the error names the offending key. The shipped `examples/twilio-trunk/siphon-ai.toml` carried exactly this bug (`register_source = "twilio"` at route level, where it did nothing — and `"twilio"` was the `[[trunk]]` name, not the `"trunk"` literal a UAS-mode call actually carries); it has been corrected.

## [0.45.3] - 2026-07-28

### Fixed

- **In-dialog requests on an inbound TCP/TLS leg no longer die at Timer B after a carrier idle-closes the connection** (fixed upstream by [siphon-rs#74](https://github.com/thevoiceguy/siphon-rs/pull/74), pin bumped `ead16f9bf9b7` → `4e7d5d7d0bd7`). An inbound session's writer task was stopped only by sender-drop, but the `Flow` clones held by dialogs outlive the session — so after a carrier idle-closed a quiet signaling connection (~2 min observed on Twilio Secure Trunking) the socket sat in CLOSE-WAIT forever (one leaked fd per connection) and its writer channel kept accepting: every in-dialog request on the leg — hold re-INVITE, REFER, the teardown BYE — was written into the half-closed socket and died at Timer B ~32 s later, leaving the caller in dead air until the carrier's own cleanup. Upstream now shuts the writer down when the read side ends (flow sends fail fast, socket properly closed), and `bye`/`refer`/`reinvite_via_flow` hand the transaction a pool-resolved fallback target so the existing RFC 3263 §4.3 dispatch failover re-sends via the connection pool when the flow is dead. No siphon-ai code change needed for the failover; the keepalive below closes the gap end-to-end.

### Added

- **`[sip].tcp_keepalive_interval_secs` — opt-in CRLF keepalive on established inbound SIP-over-TCP/TLS connections.** A call that goes SIP-quiet (RTP is out-of-band) can outlive a carrier's connection-idle window; a periodic CRLF keeps the signaling path open so in-dialog requests never race a dead socket in the first place. Default `0` (off), mirroring `[sip].tcp_idle_timeout_secs`'s wiring; keepalive frames are transport noise — never HEP-captured. Documented in CONFIG.md.

## [0.45.2] - 2026-07-27

### Fixed

- **Inbound legs no longer reuse `CSeq 1` for every in-dialog request — bot-initiated hold can be resumed again** (issue #377). The daemon-wide transfer UAC — which drives an inbound call's hold/resume re-INVITEs as well as its REFER — was built without a `DialogManager`, so it got a private one. Each in-dialog send advances the dialog's local CSeq and re-inserts the dialog into *that* UAC's store, while the per-call lookup (`DialogSource::Managed`) reads the **UAS's** store, which never saw the advance: every request re-resolved the answer-time snapshot and went out as `CSeq 1`. The first request was fine, so plain hold and plain teardown always worked; the second collided. In practice `hold` → `resume` left the caller **stuck on hold** — the carrier read the repeated CSeq as a retransmission and never sent a final response, the transaction died at Timer B (~32 s) with `error { code: "hold_failed" }`, and the call ran on until the RTP-inactivity watchdog killed it (~60 s, CDR `tap_ended`); `hold` → teardown drew a **`408` thirty seconds later**, stalling teardown and inflating the CDR. This is the inbound half of #353's frozen-snapshot bug reached by a different route — that fix gave outbound legs a shared advancing dialog and left this side alone, because the shared-store assumption encoded in `DialogSource::commit`'s doc comment looked sound. `build_transfer_uac` now shares the UAS's store, exactly as `build_outbound_uac` has since #324, and `BridgingAcceptor::install_transfer` **asserts** the UAC it is handed was built over the store it is handed — a silent mis-wiring that only surfaces as a wrong CSeq on a second request now fails loudly at daemon start. Verified live over a Twilio trunk before and after.

### Documentation

- **PROTOCOL.md §4.3 no longer promises a `hangup` `cause` → SIP response mapping that has never existed** (issue #376). The spec documented `rejected` → `603 Decline`, `busy` → `486 Busy Here`, `not_acceptable` → `488 Not Acceptable Here`, and `normal` → "BYE, or 487 on an early dialog". None of it is implemented, and two independent things stand in the way: the controller's `Hangup` arm reads `cause` only to log it and always tears down with BYE, and — more fundamentally — **a WS session only ever exists on an answered call**, since the SIP `200 OK` is sent before the WebSocket is opened and `start` is delivered, so there is no moment at which a server could decline a call that hasn't been answered. Confirmed on a live trunk at the theoretical limit: a server sending `hangup { cause: "rejected" }` **1 ms after `start`** still produced INVITE → 200 OK → BYE with no 603 anywhere, a 90 ms CDR, and a call the carrier billed as connected. The mapping appears to have been written for a deferred-answer design (`BridgeIn::Answer`, still noted in CONFIG.md as targeted for 0.2.1/0.3.0) that never shipped. §4.3 now states plainly that every `hangup` is a BYE, carries an explicit warning for call-screening servers (with the routing/trunk/admission alternatives that *do* work), and points at #376 for pre-answer rejection. The `HangupCause` doc comments and the generated schema descriptions say the same, so SDK authors reading the machine-readable contract get the truth too. No wire behavior changed and the cause values stay accepted, so adding real pre-answer rejection later remains additive.

## [0.45.1] - 2026-07-27

### Fixed

- **Outbound calls now count in the shared call instruments: `siphon_ai_calls_total{cause}`, `siphon_ai_calls_active`, and `siphon_ai_call_duration_seconds`** (issue #373). All three were updated only on the inbound teardown path, and `siphon_ai_outbound_calls_total{result}` classifies call **setup** only (`answered`/`declined`/…) — so once an outbound call was answered, its termination cause reached no metric at all, and `siphon_ai_calls_active` ignored the leg entirely (live-confirmed: 2 active calls, gauge reading 1). The practical bite: 0.45.0's "alert on `cause="ws_disconnect"`" never fired for outbound legs — origination-heavy deployments, exactly the ones most exposed to their own WS server crashing, got no signal. Both teardown paths now end calls through one shared helper (gauge down, cause counted, wall-clock duration recorded — the same block the acceptor always ran), and an outbound leg joins `calls_active` at answer, mirroring inbound's join-at-accept. Additive to the time series: no labels changed, existing inbound-only dashboards simply start seeing the outbound legs they were silently missing. DEPLOY.md metric table now states the direction coverage for all four metrics.

## [0.45.0] - 2026-07-27

### Fixed

- **CDR now records an unexpected mid-call WS drop as `termination.cause = "ws_disconnect"`** (issue #369; **CDR schema v6 → v7**). PROTOCOL.md §5.7 has always promised that a WS connection closing before any `stop` exchange produces a CDR recording `ws_disconnect` — but the CDR had no such cause: a crashed WS server, a network cut, a keepalive timeout, a bare socket close (even a clean 1000 with no `hangup`), and a reconnect window that elapsed without recovering all collapsed into `bridge_ended`, the same value an orderly SiphonAI-side session ending produces. The only trace of the difference was substring-matching the free-text `termination.bridge_disconnect` diagnostic, which has no stability contract. Same WS-vs-CDR divergence class as #356 (`transfer`) and #332 (`caller_hangup`) — the durable, billing/ops-grade side was the lossy one. New `WsDisconnect` variant on `CallTermination` / the CDR `TerminationCause` (serialises `"ws_disconnect"`, matching the WS `stop` reason); the controller classifies every bridge-first ending through one helper whose drop set deliberately equals reconnect eligibility (`ServerClosed` or any connection error → `ws_disconnect`; `stop`-sent, controller-hung-up, `server_too_slow`, `protocol_error` stay `bridge_ended`), so give-up reconnects land on `ws_disconnect` too. `siphon_ai_calls_total` gains the `cause="ws_disconnect"` label — alert on it; it means the WS server crashed or the network failed, not that a call ended. PROTOCOL.md §5.7 step 4 also named a `stop_reason` CDR field that never existed under any option — corrected to `termination.cause`.

- **Outbound requests no longer reuse one From tag for the process lifetime** (fixed upstream by [siphon-rs#72](https://github.com/thevoiceguy/siphon-rs/pull/72), pin bumped `5c7cf20beb50` → `ead16f9bf9b7`). `UserAgentClient` minted a single `local_tag` at construction and stamped it on every out-of-dialog request until restart, so all outbound INVITEs (and OPTIONS, SUBSCRIBE, MESSAGE, PUBLISH, unsolicited NOTIFY) from a node shared one From tag — RFC 3261 §8.1.1.3 requires a fresh tag per request and §19.3 fresh randomness per tag; a live trunk capture showed 7 unrelated calls with 7 Call-IDs and a single tag over 86 minutes. Upstream now mints a tag per request, with REGISTER as the deliberate exception: a stable per-registrar tag (capped map, like the reused Call-ID) so a refresh series presents one `(Call-ID, From tag)` identity per §10.2. No siphon-ai code change.

## [0.44.0] - 2026-07-26

### Fixed

- **`clear` now drops pending `mark`s and rebases the playout clock — post-clear marks fire on time** (issue #365; PROTOCOL.md §4.1 violation). Marks were detached wall-clock timers (`tokio::spawn` + `sleep_until(first_push + frames × 20 ms)`) that nothing could cancel or re-time: a mark pending at clear-time **fired anyway** (telling the server the caller heard audio that was discarded), and because `clear` never reset the frame-count anchor, every mark sent **after** a clear fired **late by the entire flushed duration** — cumulative across repeated clears, breaking turn-taking in exactly the barge-in-driven calls that use marks most. Marks are now **queue-riders owned by the tap** (PROTOCOL.md §4.1's model): they hold their position in the outbound queue, arm against the playout clock when the audio ahead of them has been handed to the media engine, and die with the queue on `clear` / `auto_clear` / mute / park — every flush path now also rebases the playout clock (`clear` previously skipped it; the pause-arbitration paths already did this). Integration tests mirror the issue's timeline (burst → mark → clear → mark) and are CI-proven red on the pre-fix tap.

- **PROTOCOL.md §5.5 outbound-audio backpressure is now implemented — server bursts no longer queue unbounded** (issue #366). §5.5 promised a 200 ms outbound buffer with oldest-frame dropping and a `siphon_ai_outbound_audio_frames_dropped_total` metric; none of it existed — every frame a WS server sent was forwarded into forge's playout queue immediately, so a buggy or hostile server streaming faster than realtime grew per-call memory without bound while the caller heard ever-staler audio, with no metric, no error, and no log. The tap now owns a bounded outbound queue fed **just-in-time** into forge via the playout clock (≤ 100 ms in flight + the documented 10-frame/200 ms window, oldest-dropped beyond it, metric + one warn per call). Well-behaved paced senders (both server SDKs) never engage the holdback, and ≤ 200 ms TTS chunks are absorbed without loss; outbound DTMF rides the same queue so a burst-then-digit server keeps its ordering. Sustained over-rate streaming now degrades to "caller hears the newest audio" instead of "caller falls minutes behind". §5.5 rewritten to state the exact window; metric documented in DEPLOY.md. The queue is also what makes #365's marks droppable — the two fixes share the mechanism, and the burst-bounding test is CI-proven red on the pre-fix tap.

## [0.43.0] - 2026-07-25

### Added

- **All admin endpoints are now served under `/admin/v1/` — the five pre-0.6.0 routes gained v1 forms, and their original unversioned paths remain as deprecated aliases** (issue #362). The admin API had grown in two generations: the original endpoints (`GET /admin/calls`, `POST /admin/calls/:id/hangup`, `GET /admin/registrations`, `GET|PUT /admin/log`, `POST /admin/hep/test`) were unversioned, while everything since 0.6.0 lived under `/admin/v1/`. Closely related verbs on the same collection sat in different namespaces — originate at `POST /admin/v1/calls`, then the obvious `GET /admin/v1/calls` 404'd because the list was at `GET /admin/calls`. The five legacy routes are now also served at `/admin/v1/calls`, `/admin/v1/registrations`, `/admin/v1/log`, and `/admin/v1/hep/test` (same handlers, same roles, same responses); the unversioned paths keep working indefinitely as deprecated aliases (earliest removal 1.0), and `siphon_ai_admin_requests_total{endpoint=…}` labels alias and v1 traffic separately so a dashboard can watch legacy usage drain before anyone retires them. The new **`POST /admin/v1/calls/:id/hangup`** takes the **bridge `call_id`** — the same id its `/park`, `/retrieve`, and `/stats` siblings take — so the rule is now uniform: everything under `/admin/v1/calls/:id/…` takes the bridge id. The legacy alias keeps its SIP Call-ID semantics unchanged; neither endpoint guesses namespaces (a wrong-namespace id is a plain 404), and `GET …/calls` returns both ids per row. Also fixed in the docs: `POST /admin/calls/:id/hangup` was described as "inbound calls only" — stale since the 0.41.x outbound-BYE fixes (#342/#353); it force-releases outbound calls too (live-verified on 0.42.0).

## [0.42.0] - 2026-07-25

### Fixed

- **SRTP calls no longer log false "SRTCP replay attack detected" warnings when the peer's RTCP SSRC changes** (fixed upstream by [forge-media#98](https://github.com/thevoiceguy/forge-media/pull/98), pin bumped `6e3fcd2e7c6f` → `003b5e1be563`). The SRTP receive context kept a **single** SRTCP replay window keyed only by the SRTCP index, but RFC 3711 §3.4 replay protection is **per-SSRC** — each SSRC has its own index space. When the inbound RTCP SSRC changed mid-call (Twilio does this transitioning from ringback/early media to the answered stream), the new SSRC's fresh low indices were rejected by the shared window as replays until they climbed past the old SSRC's high-water mark: spurious `SRTCP unprotect failed … replay attack detected` WARNs on normal secure-trunk calls, false positives in `forge_srtcp_replay_attacks_blocked_total`, and dropped RTCP reports (feeding the RTT/quality stats) during the changeover. The SRTCP sibling of the per-SSRC RTP-stats fix (#94, shipped in 0.40.0's pin). Upstream replaces the shared window with a per-SSRC `ReplayWindow` map, creates/advances a window **only after successful authentication** (forged packets can't grow the map or advance any window), and keeps genuine per-SSRC replays blocked — regression-tested upstream (`test_srtcp_replay_window_is_per_ssrc`, red before the fix). No siphon-ai code change; no API change.

- **CDR now records a transfer as `termination.cause = "transfer"`** (issue #356; **CDR schema v5 → v6**). When the WS server transferred a call away (`transfer` → the peer accepted the REFER), the WS emitted `stop { reason: "transfer" }` but the CDR recorded `termination.cause = "local_shutdown"` — so the billing-grade, durable record couldn't tell a hand-off from an admin force-hangup, a CANCEL, or an RFC 4028 session-expiry. This is the same WS-vs-CDR divergence that #332 fixed for `caller_hangup`. New `Transfer` variant on `CallTermination` / the CDR `TerminationCause` (serialises `"transfer"`, matching the WS `stop` reason); the controller now sets it on a successful REFER instead of `LocalShutdown`. Additive for parsers that treat `termination.cause` as an open string, but the schema `version` bumps to 6 so a consumer that exhaustively matches the v5 cause set can gate on it. Verified live: the transfer that recorded `local_shutdown` before now records `transfer`.

- **Post-REFER NOTIFYs are now accepted with `200 OK` instead of `405 Method Not Allowed`** (issue #357). After a transfer REFER is accepted, the peer reports transfer progress on the implicit refer subscription (`NOTIFY Event: refer`, RFC 3515 §2.4.4). SiphonAI answered it `405` — contradicting `docs/PROTOCOL.md` §4.4, which promised those NOTIFYs are "accepted". The transfer itself was unaffected (the REFER+BYE pattern never consumes the subscription), but a progress-tracking peer saw a spurious `405`, and the doc was wrong about the wire behavior. `RoutingHandler` now answers NOTIFY itself: `Event: refer` → `200 OK` (dropped, not surfaced over the WS; deliberately dialog-blind, since the NOTIFY normally arrives after our BYE tore the dialog down); an unsupported event package → `489 Bad Event` + `Allow-Events: refer` (RFC 6665); no `Event` header → `400`. NOTIFY is now advertised in `Allow` (405/OPTIONS responses), keeping RFC 3261 §20.5 honest. New counter `siphon_ai_notify_total{result=accepted|bad_event|bad_request}`. This supersedes #358's interim doc-only resolution of the same issue (which documented the 405): PROTOCOL.md §4.4 now promises — and the daemon delivers — the `200 OK`. No siphon-rs change — the `on_notify` hook already existed.

## [0.41.2] - 2026-07-24

### Fixed

- **Outbound in-dialog requests now carry monotonically increasing CSeqs (hold → resume → BYE no longer collide)** (issue #353). On an **outbound** call, every in-dialog request after the initial INVITE — a bot `hold`, then `resume`, and the closing BYE on local teardown — went out with the **same `CSeq: 2`** instead of `2, 3, 4, …`. The carrier rejected the malformed sequence: the duplicate-CSeq **BYE drew `408 Request Timeout`** (callee stranded in dead air until the carrier's ~60 s session timer — the #342 symptom, resurfacing for the re-INVITEd-call case), and the duplicate-CSeq **resume re-INVITE** was indistinguishable from a retransmission of the hold, so a resume could never apply `a=sendrecv` and the call stayed one-way/held. Only observable **after** 0.41.1, because before it none of these outbound in-dialog requests reached the wire (#342). Root cause: the outbound leg resolved each in-dialog request's dialog from a **frozen snapshot taken at answer** (`DialogSource::Direct(Box<Dialog>)`) — so `send_reinvite` advanced the CSeq on a throwaway per-request clone (and re-inserted into the gateway UAC's *private* `DialogManager`, which the snapshot never reads), and every `resolve()` restarted from `local_cseq == 1`. Inbound legs were immune: they resolve from the shared `DialogManager` the UAC re-inserts into. The fix holds the outbound leg's confirmed dialog behind a single shared, advancing handle (`Arc<Mutex<Dialog>>`) that the transfer REFER, hold/resume re-INVITE, and teardown BYE all resolve from and **commit back to**, so the local CSeq increases monotonically across the call. No siphon-rs change. Load-bearing regression test (`direct_source_commit_advances_the_shared_cseq`), CI-proven red without the fix (it reproduces the `2, 2, 2` collision).

## [0.41.1] - 2026-07-24

### Fixed

- **Locally terminated *outbound* calls now BYE the carrier; hold works on outbound legs** (completes issue #342 — the outbound half; fixed upstream by [siphon-rs#70](https://github.com/thevoiceguy/siphon-rs/pull/70), pin bumped `fbdf132a11d6` → `5c7cf20beb50`). #342/0.41.0 fixed the *inbound* teardown BYE (route set), but the same stranding persisted on **outbound** calls: hanging one up locally (admin hangup, WS `server_hangup`, drain, controller exit) sent **no BYE**, leaving the callee in dead air until the carrier's ~60 s session timer, and a bot-initiated `hold`/transfer re-INVITE silently failed. Found black-box testing 0.41.0 on the live Twilio trunk. Root cause was in siphon-rs: `TlsPool::send_tls` keyed connection reuse by `(peer_addr, SNI)`. An outbound call's INVITE opens its connection with the request-URI **hostname** SNI, but an in-dialog request (BYE / re-INVITE / REFER) derives its SNI from the peer's `Record-Route` — an **IP literal** for a carrier edge — so it missed the hostname-keyed connection and dialed a **fresh** connect, which the edge refuses and whose IP-SNI handshake fails the edge's hostname cert (`outbound TLS connect … transport error`); the request never reached the wire. Inbound legs were unaffected because they reuse their inbound connection explicitly via a `flow`. The fix makes `send_tls` fall back to reusing any live connection to the same peer `addr` on an exact-key miss (RFC 5923 connection reuse); the TCP pool already keyed by `addr` alone. No siphon-ai code change. Load-bearing regression test upstream (`send_tls_reuses_connection_to_same_addr_across_sni`), CI-proven red without the fix.

## [0.41.0] - 2026-07-24

### Fixed

- **Force-hangup on a call answered through a record-routing trunk now releases the caller** (issue #342; fixed upstream by [siphon-rs#69](https://github.com/thevoiceguy/siphon-rs/pull/69), pin bumped `50866b1599a7` → `fbdf132a11d6`). A local teardown — `POST /admin/calls/:id/hangup`, a WS `hangup` (`server_hangup`), or a bridge-ended stop — tore down siphon's media and wrote the CDR but left the caller in **dead air** until the carrier's session timer fired (~60 s on Twilio). Only a far-end-initiated BYE (`caller_hangup`) was unaffected. Root cause was in siphon-rs: `IntegratedUAC::bye` and `bye_via_flow` were the only in-dialog senders that skipped `prepare_in_dialog_request`, so the closing BYE went out with **no `Route` headers** and the carrier's private media-gateway address as Request-URI. A record-routing edge can't correlate that and answers `481 Call/Transaction Does Not Exist`, so the BYE never reached the PSTN. Measured live on the Twilio trunk: the daemon's real BYE drew a 481, while a hand-crafted BYE carrying the two `Record-Route` hops + the dialog local URI drew a 200 and released the stranded leg immediately. This looked TLS-specific only because the local TCP test peer doesn't Record-Route — the real condition is a record-routing peer. No siphon-ai code change; the fix routes both BYE methods through the dialog route set exactly as `reinvite`/`send_update`/`send_refer_via_flow` already do (empty route set → byte-identical, so direct peers are unaffected), and sources the BYE `From` from the dialog's local URI (RFC 3261 §12.2.1.1). Load-bearing regression test upstream (`bye_via_flow_carries_route_set_and_dialog_local_uri`), CI-proven red without the fix.

- **Outbound-originated calls now emit a SIP HEP ladder** (issue #341; fixed upstream by [siphon-rs#68](https://github.com/thevoiceguy/siphon-rs/pull/68), pin bumped `6fd432270bb8` → `50866b1599a7`). With `[hep]` enabled, calls the box *placed* via `POST /admin/v1/calls` produced RTCP/QoS/CDR chunks but no SIP ladder in Homer — while calls it *received* rendered the full INVITE→200→ACK→BYE. Root cause was in siphon-rs: the client connection pool (`ConnectionPool` / `TlsPool`), used for every outbound SIP connection, had zero HEP emission, so the UAC's INVITE/ACK/BYE and the responses it read all bypassed the hooks that the inbound accept loops and the one-shot senders already had. No siphon-ai code change — the emitter this daemon already installs simply now sees the outbound leg. Registration (`[[register]]`, also UAC over the pool) gains its SIP ladder the same way. Verified live: the outbound INVITE that previously produced zero HEP now emits.

- **Graceful drain now refuses outbound origination** (issue #343). During drain (SIGTERM → draining), `POST /admin/v1/calls` was not gated: it returned `202` and actually placed an INVITE, dialing the PSTN even late in the drain window. The new call wasn't tracked by the drain and was orphaned when the process exited — no clean teardown, no CDR — and since origination is the one admin action that spends money, a deploy overlapping an operator- or automation-driven originate would strand a real call. New inbound INVITEs were already 503'd during drain; outbound was the gap. `originate` now consults the same `DrainFlag` the inbound routing handler uses and refuses with `503 Service Unavailable` (new `OriginateRejection::Draining`), checked first — before gateway validation or a concurrency permit — so a draining daemon returns `Draining`, not `UnknownGateway`, and sends no INVITE.

- **Drain-forced calls now keep their CDR — file record *and* HEP chunk** (issue #344; needs the hep-rs bump to `91e689b`). A call force-terminated at the drain deadline computed `termination = DrainForced` but its CDR reached neither sink, so the one cause the record exists to capture was unobservable and metrics (`siphon_ai_calls_drain_forced_total`) disagreed with the CDRs about those calls' existence. Two independent races, both fixed:

  - **File record.** The post-controller cleanup task deregistered the call from the registry *before* emitting its CDR — but `drain_wait` polls that registry's length to decide the drain is complete. So a drain-forced call's slot could empty while its `FileSink` write was still in flight, letting teardown proceed and cancel the detached cleanup task mid-write. Deregistration now happens **last**, after `cdr_sink.emit(...)` has awaited the flush, in both the inbound acceptor and the outbound service, so the drain blocks until the CDR is on disk. A BYE retransmit racing this window still finds the entry and gets a 200 — the slot just lives a few ms longer.

  - **HEP chunk.** `HepWorkerHandle::shutdown` aborted the HEP UDP worker, discarding anything still queued — and the CDR HEP chunk is only *queued* by `HepCdrSink::emit`, not sent inline. It now signals [`UdpHepSink::shutdown`](https://github.com/thevoiceguy/hep-rs/pull/2) (new upstream) and awaits the worker within a 2 s grace: the worker closes its receiver and flushes the backlog before exiting, falling back to abort only if a wedged collector blows the grace. The global SIP/forge emitters hold sender clones forever, so the channel never closes on its own — closing the receiver is what makes the drain reliable. Because `MultiSink::emit` awaits both sinks, the file-record reorder above also guarantees the HEP chunk is queued before the drain completes, so the worker drain that follows always has it.

  The hep-rs drain is proven load-bearing by its own `shutdown_flushes_the_queued_backlog` test (a 100-packet backlog against a deliberately-slow collector, which `abort` would discard). Note that on a fast loopback neither race reproduces — the incidental teardown latency lets both writes win — so these are correctness/determinism fixes verified by construction and by the upstream unit test, not by a failing end-to-end here.

## [0.40.0] - 2026-07-22

### Added

- **CDR `answered_at` — connected duration is now derivable** (issue #331; **CDR schema v4 → v5**). A record carried `started_at`, `ended_at` and `duration_ms` but no answer timestamp, and for outbound calls `started_at` is stamped when the origination request is accepted — *before the INVITE goes out*. So `duration_ms` silently measured INVITE-to-end including ring time, and connected duration was not derivable from a v4 record in any form. The overstatement is exactly the ring duration, so it is unbounded and varies per call: ~1.4 s on an instant pickup, ~12.4 s on a call that rang that long — more than half again on a 21 s conversation. Carriers bill from answer, so anyone rating or reconciling against an invoice was systematically over by an amount correlated with how long each call rang, with nothing in the record signalling it. Unanswered calls were also indistinguishable from very short answered ones on duration alone.

  `answered_at` is stamped where `place()` / `place_delayed()` return on the 2xx for outbound; for inbound it coincides with `started_at`, because that is stamped in `run_call`, which is only reached once the INVITE has been accepted. `None` when the call never connected. `duration_ms` keeps its existing meaning — wall-clock including setup — now documented as such rather than left ambiguous; billable duration is `ended_at - answered_at`. CSV gains an appended `answered_at` column (49 total).

### Fixed

- **CDR `termination.cause` is `caller_hangup` when the far end hangs up** (issue #332; **CDR schema v4 → v5**, shared with the entry above). A remote BYE recorded `local_shutdown` — the same value as an admin force-hangup, a CANCEL, and an RFC 4028 session expiry — so the single most common way a call ends had no distinct value. The daemon was not missing the information: it computes the correct attribution two lines earlier in the same match arm, sends the WS server `caller_hangup`, and then did not consult `remote_bye_received()` when setting the CDR cause. The WS protocol and the CDR therefore disagreed about the same event, and the CDR — the billing-grade, durable side — was the lossy one. Anyone building "who hung up first" analytics (abandonment rates, agent-vs-customer disconnect, short-call diagnosis) could not do it from CDRs at all.

  New `CallTermination::CallerHangup` / `TerminationCause::CallerHangup`, serialising as `caller_hangup` to match the WS `stop` message's existing `reason` for the same event, gated on the `remote_bye_received()` predicate already in scope. `local_shutdown` now means what its name says: admin force-hangup, CANCEL, session-timer expiry. The `siphon_ai_calls_total{cause}` metric gains the same value, so a dashboard split by cause will show remote hangups moving out of `local_shutdown` rather than the total changing.

  Two integration tests in `registry_bye.rs` had codified the old behaviour (asserting a BYE yields `LocalShutdown`); they now assert the corrected attribution.

  **CDR version bumped once for both changes.** `answered_at` alone is additive-optional and would not have required it (the `verstat_attest` / `recording_id` precedent), but a new `TerminationCause` value can break a strict exhaustive matcher — the same reasoning that drove v2 and v3. Consumers should gate on `version >= 5`.

- **A sink enabled under a disabled master switch now warns at startup instead of failing silently** (issue #333). `[cdr.file].enabled = true` without `[cdr].enabled = true` produced no CDRs, no warning, and no log line of any kind — the master switch installs a no-op sink regardless of the sub-blocks, and nothing surfaced the contradiction. A production box ran in exactly this state for the lifetime of a test engagement: zero CDRs written, `/var/log/siphon-ai/` present and writable and empty, `journalctl | grep -i cdr` returning nothing at all. It was found only by going looking for a record that should have existed, and it silently invalidated everything downstream — any test assuming CDRs exist passes vacuously, so the CSV format added in 0.36.0 and the `tx_*` quality fields had never once been written on that box.

  Config load now logs one `warn!` per affected sub-sink, naming it: `[cdr.file].enabled = true but [cdr].enabled = false — this sink is inert and nothing will be written`. A warning rather than a hard error, because silencing a whole subsystem with one flag while leaving the sub-blocks configured is legitimate; CLAUDE.md §4.6's fail-loud rule is about config that *cannot* work, not config that deliberately does nothing.

  The issue asked whether other master-switch/sub-block pairs share the shape. `[conference]`, `[park]` and `[hep]` do not — they have no sub-blocks with their own `enabled`. **`[audit]` and `[quality]` do**, each with `.file` and `.webhook` under a master switch, and both had the identical silent failure; all three are covered. Documented in `docs/CONFIG.md`, including the trap that omitting the `[cdr]` block entirely leaves the master off (`enabled` defaults to `false`), so a lone `[cdr.file]` block writes nothing.

- **`rx_packets_lost` no longer reports the ring duration as packet loss** (issue #330; fixed upstream by [forge-media#94](https://github.com/thevoiceguy/forge-media/pull/94), pin bumped `f6151edf2724` → `6e3fcd2e7c6f` — #94's code fix at `24e20ae1e8ac`, with [forge-media#96](https://github.com/thevoiceguy/forge-media/pull/96) a docs/test-only correction of the diagnosis on top, no behaviour change). On a trunk that sends ringback as early media, `rx_packets_lost` was set once at answer to roughly `ring_seconds × 50` and never moved again — 621 on a 12.42 s ring, 127 on a 2.5 s ring — which pinned the transport `mos_estimate` to **1.0, the floor of the scale**, for the whole call. The effect was inverted from reality (the longer a call rang, the worse its reported quality, so the calls that rang longest looked worst to anyone triaging complaints) and it propagated into the CDR `quality` block and Homer's HEP QoS, contaminating historical quality data and any MOS-based alerting at the source. `avg/max_packet_loss_ratio` stayed correctly at `0.0` throughout, so the two loss signals in the same record actively disagreed.

  Root cause was in forge's `RxStreamStats`, which carried a single sequence baseline with **no SSRC field at all** — it compared sequence numbers across sources that each have their own independent sequence space (RFC 3550 §8). When the trunk switches SSRC at answer, a baseline anchored in the ringback era carried into the answered stream, and any sequence discontinuity between the two was charged as loss. A production capture established that the loss was fabricated at the source: 4 SSRCs on one 5-tuple, seq 773→3333, 2561 packets, **zero wire loss**. The exact discontinuity on a *failing* call was never captured (that call's own packets were not recorded, and the capture in hand shows continuous sequence across its SSRC changes, so that particular call was healthy); the leading hypothesis is that the answered stream's sequence starts about `ring × 50` ahead of where the ringback stream ended — a media server deriving sequence from a clock running since call setup — which the SSRC-blind baseline reads as loss with every packet still counted. forge-media#94 re-baselines on the SSRC change, so any such discontinuity is absorbed regardless of the precise trigger; its regression test models the capture's real sizes (749 ringback packets, all counted, then 1812 answered packets starting a full ring ahead) and reproduces 749 phantom lost with the fix disabled, 0 with it. No siphon-ai code change; the counters this daemon already reads simply stop being wrong.

  A related suspicion — that early-media RTP was *received but never counted*, leaving `rx_packets_received` undercounting the ringback burst — was filed as [forge-media#95](https://github.com/thevoiceguy/forge-media/issues/95) and **closed as invalid**: a controlled call with 7 s of ringback and no media after answer counted all 222 early-media packets (`recv=222, lost=0`), not one. Early media is received and counted; there is no uncounted burst. One narrow case remains outside #94's reach — a trunk that keeps a *single* SSRC across answer while jumping its sequence number — but that is malformed RTP under RFC 3550 §8 and would read as genuine loss to any receiver.

## [0.39.0] - 2026-07-22

### Fixed

- **`GET /admin/v1/drain` is now labelled in `siphon_ai_admin_requests_total`** (issue #319). The route is dispatched and served correctly but had no arm in `route_label()`, so it fell through to `endpoint="unknown"` — the only served admin route missing from the table. Two consequences: successful drain polls were indistinguishable from unrecognised paths (both `endpoint="unknown"`, separated only by `result`), polluting the reasonable "someone is probing the admin API" signal a dashboard keys on `endpoint="unknown"`; and drain — the endpoint a deploy script polls hardest during a rollout, likely the highest-rate admin request on the box — was the one route with no per-endpoint visibility. Metric-label only; no functional, auth, RBAC, or response-body change.

  While fixing it, `min_role()` turned out to be missing the same route. That table's doc requires it to mirror `admin::dispatch`, and without an arm the drain path only reached `ReadOnly` by falling through to `authorize`'s unknown-path default — the right answer (and the one `docs/DEPLOY.md` already documents) for the wrong reason, and silently wrong had that default ever been tightened. Both tables now carry the route, so behaviour is unchanged and no longer accidental.

  New `every_served_route_has_a_bounded_label` test walks all ten static dispatcher routes plus the nine dynamic templates and asserts each has a non-`"unknown"` label, that a static route's label is exactly `"METHOD /path"` (catching a copy-pasted arm pointing at the wrong template), that a dynamic route's label is a template rather than a concrete path and leaks no id into the metric label, and that every served route has an explicit `min_role` arm. The prior `route_label` coverage was only the two `:name` registration templates.

- **`PROTOCOL.md` §3.10 cited a config key that does not exist** (issue #325). The `rtp_timeout` row named `media.rtp_idle_timeout_ms` as the governing setting — a key that appears nowhere else in the repo, in no `.rs`, `.toml`, or other doc. The real control is `[media].inactivity_timeout_secs`: different name, different unit. This is the worst failure mode for a config reference — an operator reading the protocol spec adds the key, sees no effect, and concludes the timeout is unconfigurable or that they've disabled it when they haven't. Per CLAUDE.md §9 `PROTOCOL.md` is the highest-stakes document in the repo, and §3.10 is the section a WS-server author consults when calls die on a silent leg. The row now names the real key and adds what a server author actually needs: default `60` s, per-route overridable via `[route.media].inactivity_timeout_secs`, `0` disables the watchdog, and the error is fatal with `stop` (`reason: "error"`) following. The §3.10 example `message` was also fictional — `"no RTP for 30s on leg A"` implies a 30 s default the daemon doesn't have and a per-leg attribution it never produces, with "leg A" appearing nowhere else in the spec; it now carries the emitted text, `"no inbound RTP within the inactivity timeout"`. Docs only — no behaviour change. Found while tracing the teardown in #324.

- **A far-end BYE now tears down an outbound call** (issue #324; requires the siphon-rs pin bump to `6fd432270bb8` = [siphon-rs#67](https://github.com/thevoiceguy/siphon-rs/pull/67)). Previously a BYE from the remote party on an **outbound** call was answered `481 Call/Transaction Does Not Exist` and never reached the call controller. The call then lingered until the 60 s media-inactivity watchdog, producing a CDR duration inflated by a full minute (billing-grade: ~144 s recorded against a carrier-billed 84 s in the reported case), a `tap_ended` termination cause that misclassifies a normal hangup as a media failure, a trunk channel held for the extra minute, and a doomed teardown BYE that burned another ~32 s on Timer F. It fired on every outbound call where the remote party hung up first — the common case.

  There were **two independent causes**, and the issue diagnosed only the first:

  1. **The dialog store wasn't shared.** Each gateway UAC owned a private `DialogManager`, so `IntegratedUAS::dispatch` resolved the inbound in-dialog request against a different store, missed, and short-circuited to 481 without ever invoking `on_bye`. Fixed by threading `uas.dialog_manager()` into every gateway UAC via the new upstream `IntegratedUACBuilder::dialog_manager` setter. (The inbound path has always shared this store; the outbound path is catching up.)

  2. **Outbound calls were never in the registry the BYE handler resolves against.** Even once `on_bye` ran, `terminate_from_bye(sip_call_id)` missed: `dispatch_bye` consults `CallRegistry` (keyed by SIP Call-ID), while the outbound service only ever inserted into `CallControlRegistry` (keyed by bridge id). The issue asserted this half was already fine because the call "is listed in `GET /admin/calls`" — but that endpoint reads the *control* registry, a different table. Outbound legs now join `CallRegistry` for their lifetime, and the teardown BYE is gated on `remote_bye_received()` the way the inbound acceptor has always gated its own, so we no longer BYE into a dialog the peer has already discarded.

  **Secondary behaviour change:** `CallRegistry` is also the daemon's active-call count for graceful shutdown, so `/admin/v1/drain`'s `active_calls` now includes outbound calls and a drain waits for them to finish. Previously a deploy walked straight past in-flight outbound calls. This is a deliberate fix, but it changes a number operators may alert on.

  A peer re-INVITE on an outbound leg is now refused `501` (the documented "no stored answer" path) rather than the `481` an unregistered dialog produced — renegotiating an outbound leg is genuinely unimplemented, so 501 is the honest response.

  New SIPp scenario `outbound_remote_bye` closes the coverage gap the issue flagged: roles inverted, SIPp answers the originated call and then sends the BYE. It asserts the `200` (a regressed daemon answers `481` there), that the daemon logged `BYE → controller shutdown` (proving the lookup resolved), and that no Timer-F teardown failure followed. Verified to fail with *either* half of the fix reverted — checking only that the call left the registry is too weak, because the bridge notices the peer is gone and ends the call anyway within seconds, which is precisely how the second cause hid behind the first. This is the suite's first UAS-side in-dialog request; the harness suite is now 38 scenarios.

## [0.38.0] - 2026-07-21

### Added

- **TX-side packet counters across the quality telemetry** (issue #320;
  requires the forge-media pin bump to `f6151edf2724` =
  [forge-media#93](https://github.com/thevoiceguy/forge-media/pull/93),
  which publishes the underlying numbers on the `ForgeEvent` bus). Every
  quality surface was `rx_*` only, so an operator could see what SiphonAI
  *received* but never what it *sent* — "the outbound leg was clean" was a
  ratio with no denominator behind it. Three fields close the gap:
  `tx_packets_sent` and `tx_octets_sent` (locally measured on the
  SiphonAI→caller stream, cumulative since call start; octets are RTP
  payload only, the same basis as an RTCP SR's sender octet count), and
  `tx_packets_lost_reported` (the far end's own **absolute** count of
  packets it lost on that stream, from the latest RR's cumulative-lost
  field). Together they express the sentence operators actually ask for
  after a bad call: *"we sent 1,914 packets; the far end reported 12
  lost."* `tx_packets_lost_reported` is **signed** — RFC 3550 §6.4.1
  defines it that way because duplicates can push the peer's
  packets-received past packets-expected, so consumers must parse it as a
  signed integer and not clamp; a negative value is real information (a
  duplicating path). Surfaces: the `rtp_stats` WS event (additive optional
  fields, **protocol stays v1** as with the 0.30.0 `rx_*` addition), the
  CDR `quality` block, `/admin/v1/calls/:id/stats`, and the `[quality]`
  history records. CDR schema **stays at version 4** — additive optional
  fields within an existing block, per CLAUDE.md §7.7 and the
  `verstat_attest` / `recording_id` precedent (the v4 bump was for
  introducing the block itself). CSV gains three append-only columns
  (`quality_tx_packets_sent`, `quality_tx_octets_sent`,
  `quality_tx_packets_lost_reported`) at the end of the header, so
  position-keyed ingestors are unaffected. Motivated by a real 0.37.3
  outbound call over a Twilio Secure trunk where `rx_packets_lost` 115 /
  `rx_packets_received` 1914 was visible but the clean TX direction could
  not be quantified in packets. Docs: `PROTOCOL.md` §3.8, `DEPLOY.md`;
  schema regenerated; both server SDKs updated in lockstep.

### Changed

- **SIPp harness**: `run-all.sh` now preflights the echo WS server the
  same way it already preflights `sipp` and the daemon binary, exiting
  `2` with start-up instructions instead of running the suite without
  it. Previously a missing server produced eight scenario failures
  scattered across five phases (`basic_call_then_bye`,
  `session_timer_echo`, `reinvite_hold_resume`,
  `reinvite_unsupported_codec_488`, `session_progress_then_answer`,
  `stir_shaken_attestation_pass`, `digest_auth_caller`,
  `recording_writes_valid_wav`) — every scenario whose call must reach
  ACTIVE aborts on an unexpected BYE when the daemon tears the call down
  after the bridge can't connect, while scenarios that reject before
  bridging (488 / 428 / 403 / 503, CANCEL) still pass. The split reads
  like a signalling regression rather than a missing prerequisite, and a
  baseline-vs-branch comparison reproduces it identically on both sides,
  which makes the wrong conclusion look confirmed. CI was already
  immune — `.github/workflows/test.yml` starts the server and waits for
  the bind before invoking the script — so this closes the gap for local
  runs only; the check is a no-op in CI. The shared echo-server port is
  now a single `ECHO_WS_PORT` constant interpolated into the six
  generated configs that use it, matching the `*_WS_PORT` convention the
  private-echo-server phases already follow (it stays pinned to 8765 by
  `configs/local-dev.toml`, which the main phase runs against).

### Fixed

- **`packet_loss_ratio` is documented correctly: it is a per-interval
  figure, not a cumulative one** (issue #320, secondary item). The CDR
  `quality` block's `avg/max_packet_loss_ratio` (`crates/cdr/src/schema.rs`),
  `PROTOCOL.md` §3.8, and `DEPLOY.md` all described these as the
  "RR-reported **cumulative**-loss ratio". They are derived from the RR's
  `fraction_lost` field, which measures loss over the interval since the
  *previous* report (RFC 3550 §6.4.1) — so `avg_packet_loss_ratio` is a
  mean of interval fractions, and an operator reconciling it against a
  carrier's cumulative figure would never get matching numbers. **The
  emitted values are unchanged**; only the descriptions were wrong. The
  field was deliberately *not* recomputed from the newly available
  cumulative counter — that would silently change a published number for
  existing consumers. Use `tx_packets_lost_reported / tx_packets_sent`
  for a true whole-call loss rate.

## [0.37.3] - 2026-07-21

### Fixed

- **Outbound INVITE now carries the configured caller-ID in its From
  header** (issue #316; siphon-rs pin bump for `IntegratedUAC::
  invite_with_from`). Both `[[gateway]].from` and the per-originate
  `from` were honored for the WS `start` message and the CDR but **never
  reached the INVITE** — the UAC stamped its own local identity, so every
  outbound INVITE went out as `sip:siphon@<public_address>`. Any trunk
  that validates caller-ID (Twilio Secure Trunking, essentially every
  commercial provider) declined the call. The resolved caller-ID is now
  parsed and threaded through `OutboundOriginator::place` /
  `place_delayed` into a new per-call UAC From override, for both early
  and delayed offer. A malformed per-request `from` is rejected `400`
  (`BadFrom`) before a concurrency permit is taken; the gateway `from`
  stays validated at config load. This bug was **masked by #312** — before
  0.37.2 outbound TLS never completed the handshake, so the provider never
  parsed the From; with TLS fixed, this was the next wall. No protocol,
  config-schema, or CDR change (the WS/CDR `from` was already correct).

## [0.37.2] - 2026-07-20

### Changed

- **`GET /admin/calls` now returns an object per call, not a bare SIP
  Call-ID string** (issue #311). Each element is
  `{call_id, sip_call_id, direction}`: the **bridge** `call_id` (the id
  `/admin/v1/conferences/*`, `/park`, `/retrieve`, and `/stats` all take,
  and the value on the WS `start` message + CDR), the **SIP** Call-ID
  (what `POST /admin/calls/:id/hangup` takes), and `"inbound"` /
  `"outbound"`. Before this, the only endpoint that enumerated calls
  returned SIP Call-IDs while every conference endpoint required the
  bridge id, and **no admin endpoint exposed the mapping** — operator
  conferencing was undriveable without correlating daemon logs, and the
  resulting `404 "no active call"` (while `GET /admin/calls` listed the
  call) read as a liveness bug. The listing is now sourced from the
  bridge-id-keyed control registry, so it also covers **outbound** calls,
  which the old inbound-only listing omitted. **This is a breaking
  response-shape change** to `GET /admin/calls` (array of strings → array
  of objects); scripts parsing it must be updated. The `404` bodies from
  the conference/park handlers now name the expected id namespace.

### Fixed

- **Outbound TLS to a hostname trunk now completes — SNI is the URI
  hostname, not the resolved IP** (issue #312; siphon-rs #64, pin
  `36c3ac4f3c0c` → `3a4fc312ade3`). On a hostname trunk with no SRV
  records (Twilio Secure Trunking, `*.pstn.twilio.com`), RFC 3263
  resolution replaced the URI host with the resolved A-record IP, and the
  UAC then handed *that IP* to rustls as the TLS `ServerName` — so the
  handshake presented `sni=<ip>` and cert-verified against the IP. Any
  trunk serving a hostname-scoped certificate and keying on SNI rejected
  it, so outbound TLS calls never connected (`result="unreachable"`);
  combined with a secure trunk rejecting UDP (`488`), there was **no
  working outbound transport**. Fixed upstream in siphon-rs `sip-dns` /
  `sip-uac`: a `DnsTarget` now carries the pre-resolution hostname as its
  TLS reference identity (RFC 5922 §4) and TLS uses it for SNI and
  certificate-name verification, while the connection still targets the
  resolved IP — so RFC 3263 address selection is unchanged. This bump is
  the only siphon-ai change; no siphon-ai API, config, protocol, or CDR
  change. IP-literal and SRV-addressed trunks are unaffected.

- **`siphon_ai_admin_requests_total` no longer labels failed admin
  requests `result="ok"`** (issue #310). The counter derived `result`
  from the auth outcome only, so any authorized request whose handler
  then returned a non-2xx status was still counted `ok` — a `404` from
  operating on a stale `call_id`, a `409`, a `503` at a cap. `result` now
  follows the response status: `not_found` for `404` (a normal
  conference/park race), `error` for every other handler failure (400 /
  409 / 429 / 501 / 503), `ok` only for 2xx. The auth layer's
  `unauthenticated` / `forbidden` are unchanged. Alerting on
  `result != "ok"` is now a faithful failure signal. `# HELP` text,
  `docs/DEPLOY.md`, and the audit-stream `result` field updated to match.

## [0.37.1] - 2026-07-20

### Fixed

- **Feature guides no longer document the pre-0.10.0 admin API.**
  `docs/OUTBOUND.md`, `docs/CONFERENCE.md`, `docs/PARK.md`,
  `docs/OPERATIONS.md`, `docs/INSTALL_DEBIAN13.md`, and one
  `docs/CONFIG.md` entry still told operators to call
  `http://localhost:9091/admin/v1/…` unauthenticated, and OUTBOUND §3 +
  CONFIG's `[outbound].max_concurrent` entry both stated that the
  originate API "has no built-in authentication." That has been wrong
  since **0.10.0**, which moved `/admin/*` off `[observability]
  .http_listen` (it returns `404` there) onto the dedicated `[admin]`
  listener behind a bearer token + RBAC. Following those guides on
  0.37.0 produced `404`s and read as a broken feature; `docs/DEPLOY.md`
  and the README were already correct, so the docs contradicted each
  other. Every example now targets the `[admin]` listener with an
  `Authorization: Bearer …` header and names the **minimum role** —
  verified against `crates/telemetry/src/auth.rs`: origination is
  `admin` (billable), conference create/end/add/remove and park/retrieve
  are `operator`, and the list/`GET` routes are `readonly`. OUTBOUND §3
  is rewritten around the token as the primary control (separate
  `admin`-role token for dialing, `[admin.tls]` on a routable bind,
  rotation needs a restart — SIGHUP doesn't reload the token table) with
  the superseded 0.6.0 reverse-proxy posture marked as history.
  Additionally, `INSTALL_DEBIAN13.md`'s sample config had **no `[admin]`
  block at all**, so its admin commands pointed at a closed port even
  with the URL corrected — it now ships a loopback `[admin]` listener
  with `readonly` + `operator` tokens drawn from the existing
  `/etc/siphon-ai/env` `EnvironmentFile`, plus a note on why the admin
  port needs no firewall rule while it stays on loopback. Docs only — no
  code, config-schema, protocol, or CDR change.

- **Idle TLS trunk disconnects no longer log at `ERROR`** (#306,
  siphon-rs #63). Peers that drop an idle SIP/TLS connection without a
  TLS `close_notify` — Twilio, and anything behind an AWS NLB — surface
  in rustls as an `UnexpectedEof` read error. The TLS session loop had
  logged every read error at `error!` and bumped the transport
  Read-stage error metric, so a routine post-call disconnect produced a
  spurious `tls read error … peer closed connection without sending TLS
  close_notify` line after essentially every call on a TLS trunk —
  noise that trains operators to ignore error-level logs and trips
  log-based alerting. `UnexpectedEof` is now treated like a clean EOF
  (`info!` "closed by peer", no error metric); genuine read failures
  keep the `error!` log (now with the `peer` field) and the metric.
  siphon-rs pin bumped `f3454c7` → `36c3ac4`; no API change, log quality
  only.

## [0.37.0] - 2026-07-16

### Added

- **Neural (Silero) VAD backend — `[media].vad = "energy" | "neural"`** with a per-route `[route.media].vad` override (strict replace, both directions, validated at config load). `"neural"` runs forge-vad's new Silero backend (forge-media #86: local tract-onnx inference, no network, ~60–80 µs per 32 ms window) for materially fewer acoustic false positives — coughs, keyboard clatter, music-on-hold bleed — before pause-mode barge-in arbitration even arms. Sessions are allocated before codec negotiation, so a neural detector is created at 16 kHz and re-aligned to the **negotiated** bridge rate at setup time (fixing the latent default-16 kHz-on-8 kHz mismatch; the delayed-offer and outbound paths re-align at `apply_answer`). The default `"energy"` keeps pre-0.37 detection byte-identical, including no per-session engine config. **WS protocol, `speech_started`/`speech_stopped` events, and CDR are unchanged** — the backend changes detection quality only. forge-media pin bumped to `1c996ae5fb4f` with `features = ["neural-vad"]` (tract is pure Rust; the static-musl multi-arch release build is the acceptance gate). Closes the siphon-ai half of the ROADMAP P2 "Neural VAD upgrade" item; rollout gate stays real-call false-barge-in rates under `mode = "pause"` + `debounce_ms` via the existing barge-in metrics. See `docs/CONFIG.md` `[media].vad`.

## [0.36.0] - 2026-07-16

### Added

- **CSV CDR file output** — `[cdr.file].format = "jsonl" | "csv"`
  (#297). Default `jsonl`, unchanged. `"csv"` writes a fixed 45-column
  flat view of the CDR record: nested optional blocks (`audio`,
  `termination`, `consent`, `park`, `hold`, `reconnect`, `quality`)
  become prefixed columns; absent/unmeasured values are **empty
  cells**, not zeros; enums use the same snake_case wire strings as
  JSON; RFC 4180 quoting. A header row is written when the file starts
  empty (never repeated on restart-append) and columns are append-only
  across releases. The webhook sink is unaffected (always JSON). When
  switching an existing file's format, point at a new `path`. See
  `docs/DEPLOY.md` → *CDR consumers* → *CSV format*.
- **`print-config --format json`** (#296). Renders the effective
  compiled config as pretty-printed JSON for tooling (`jq`, deploy
  diffing) — same sections and redaction semantics as the text output
  (unset → `null`, hidden secrets → `"<redacted>"`, `--show-secrets`
  reveals; per-route keys appear only when the route overrides them).
  Default format stays `text`, byte-identical to before. An inspection
  view, not a loadable config.

## [0.35.0] - 2026-07-15

### Added

- **Optional `/metrics` bearer auth** —
  `[observability].metrics_token`
  (`docs/design/DESIGN_METRICS_AUTH.md`; #294). Recon-hardening for
  deployments that expose the observability port beyond loopback:
  when set, `GET /metrics` requires `Authorization: Bearer <token>`
  (SHA-256 + constant-time compare, the admin listener's scheme; only
  the hash is retained in memory). Failures answer `401` +
  `WWW-Authenticate: Bearer`. **Unset = open**, the default —
  existing deployments are unchanged. `/health` and `/ready` are
  never gated (probes must not need secrets). Empty-after-expansion
  tokens fail at load; use `${file:…}` / `${cred:…}`.
  Prometheus-side `authorization.credentials_file` snippet documented
  in `docs/DEPLOY.md` and `examples/observability/prometheus.yml`.
- **Metric**: `siphon_ai_metrics_requests_total{result=ok|unauthenticated}`
  — emitted only when the gate is configured (an open endpoint counts
  nothing); rejected scrapes also log a rate-limited warning.

This closes the last locally-buildable P2 roadmap item. No WS-protocol,
CDR, or webhook changes.

## [0.34.0] - 2026-07-15

### Added

- **WS-failure prompt playback** — `[bridge].on_ws_failure =
  "play_prompt"` + `ws_failure_prompt_file`, both per-route
  overridable (`docs/design/DESIGN_WS_FAILURE_PROMPT.md`; #292).
  Finishes the switch reserved since v1: when the WS becomes
  **unusable** — unexpected drop, connect failure at answer, keepalive
  timeout, `protocol_error`, `server_too_slow`, or an exhausted
  0.7.3 reconnect window — the caller hears a configurable WAV
  (*"we're experiencing difficulties…"*) before the normal BYE,
  instead of an unexplained disconnect. Details:
  - Never fires when the ending was intended (server `hangup` / clean
    `stop`), on caller actions, on `rtp_timeout`, or during drain.
  - **Fail-open**: an unusable prompt (rate mismatch, file vanished)
    degrades to today's immediate teardown; playback is capped at a
    fixed 30 s. CDR termination causes are unchanged (`duration_ms`
    grows by the prompt).
  - **Announce-over-park**: a prompt started while the call is parked
    on MOH now plays (MOH → prompt → BYE after a failed reconnect);
    a park arriving mid-announcement still cuts it short, so the
    0.26.0 consent semantics are unchanged.
  - Prompt file is required + existence-checked at load when any
    effective policy is `play_prompt`; WAVs longer than the 30 s cap
    warn at load.
- **Metric**: `siphon_ai_ws_failure_prompts_total{result}` with
  `played | cut_short | unusable | timeout`.
- **SIPp harness**: `ws_failure_prompt` phase (echo server drops the
  WS mid-call; asserts the prompt played on a real call). Suite is
  now 38 scenarios.

### Changed

- `MediaTap::with_ws_reconnect` renamed to `with_survive_ws_drop`
  (internal API) — the tap's survive-WS-drop mode now serves both
  reconnect and prompt calls.

No WS-protocol, CDR, or webhook-schema changes.

## [0.33.0] - 2026-07-15

### Added

- **Registration management (admin API)** — operators can force a
  `[[register]]` binding back **without bouncing the daemon** (which
  tears down every active call). Two write actions on the
  authenticated `[admin]` listener, operator role, audit-logged, no
  new config (`docs/design/DESIGN_REGISTRATION_ADMIN.md`; #289):
  - **`POST /admin/v1/registrations/{name}/refresh`** — immediate
    off-cycle REGISTER; during a failure backoff the kick also resets
    the backoff to its initial value.
  - **`POST /admin/v1/registrations/{name}/restart`** — full cycle:
    REGISTER `Expires: 0` to clear the registrar-side binding, then a
    fresh REGISTER (stale server state, contact rebinding). A failed
    unregister warns and proceeds — only the final attempt drives
    status/metrics/webhook.
  Both return `202` with the accept-time row; the outcome is
  asynchronous and observable via `GET /admin/registrations`,
  `siphon_ai_register_attempts_total`, and the
  `registration_state_changed` webhook. `404` unknown name, `409`
  while draining. Per-binding only (no "refresh all" in v1).
- **Parked bindings**: `register_on_startup = false` now runs the
  ordinary drive task **parked under operator control** — no REGISTER
  until the first `refresh`/`restart` arrives (the "tell to register"
  RPC the `disabled` status had reserved). Ship the config dark, kick
  the binding when the maintenance window opens. No "re-disable"
  action in v1.
- **Metric**: `siphon_ai_register_admin_triggers_total{name,action}` —
  accepted operator triggers; the resulting REGISTER lands on
  `register_attempts_total` as usual.
- **SIPp harness**: `registration_admin` phase — the suite's first
  registrar-side scenario (SIPp answers REGISTER and asserts the
  restart's `Expires: 0` on the wire). Suite is now 37 scenarios.

No WS-protocol, CDR, config-schema, or webhook changes.

## [0.32.0] - 2026-07-14

### Added

- **Reversible (server-arbitrated) barge-in** —
  `[bridge.barge_in].mode = "pause"`
  (`docs/design/DESIGN_REVERSIBLE_BARGE_IN.md`; #285/#286). Today a
  cough, laugh, or backchannel ("uh-huh") that trips VAD irreversibly
  kills the bot's playout. Pause mode reacts instantly but
  *reversibly*: playout is flushed within one frame — exactly like
  `auto_clear` — but the unplayed tail is retained, and the WS server
  (the only layer with STT) rules on intent:
  - `speech_started` carries `decision_pending: true` +
    `decision_deadline_ms` when an arbitration arms;
  - the server answers **`barge_in_confirm`** (real interruption —
    tail dropped) or **`barge_in_reject`** (false positive — playout
    resumes mid-utterance); a `clear` during the window acts as
    confirm, and late verdicts are harmless no-ops;
  - every resolution is acknowledged with
    **`barge_in_resolved { outcome: confirmed | rejected | timeout }`**;
  - no verdict within `decision_ms` (default 500) applies
    `on_timeout` (default `confirm` — a server that never rules
    degrades safely to "auto_clear delayed by the window");
  - `resume_max_secs` (default 30) caps the retained audio;
  - `start.barge_in_mode` announces the call's resolved policy;
  - per-route overrides via `[route.bridge.barge_in]`, field-wise;
  - the existing `debounce_ms` echo gate composes in front (acoustic
    filter first, semantic arbitration second).
  Arbitration only arms while the bot is playing, is suspended in
  conference rooms, and resolves as confirm when preempted by
  mute/hold/park/announce/room-join or a WS drop. Off by default;
  protocol stays **v1** (all additions additive), CDR stays **v4**
  (additive optional `quality` counters).
- **Server SDKs**: typed `BargeInResolved` / extended `SpeechStarted`
  and `Start`, plus `barge_in_confirm()` / `barge_in_reject()` (Python)
  and `bargeInConfirm()` / `bargeInReject()` (TypeScript). The echo
  reference servers answer arbitration requests with a reject by
  default (`SIPHON_ECHO_BARGE_IN_VERDICT=confirm` flips it).
- **Conformance**: new bundled `barge-in-pause` testkit scenario
  (verdict within the deadline + timeout-outcome tolerance) and a
  `session.barge_in_mode` scenario option.
- **Metrics**: `siphon_ai_barge_in_decisions_total{outcome}` and
  `siphon_ai_barge_in_decision_seconds` (explicit 50 ms–5 s buckets).
- **SIPp harness**: `barge_in_pause` phase with *real caller media* —
  a run-time-generated G.711 tone pcap (`gen_tone_pcap.py`, stdlib
  only) replayed via `play_pcap_audio`, driving VAD → pause →
  reject → resume end-to-end. The suite is now 36 scenarios.

### Fixed

- Route-level `[route.bridge.barge_in]` strings are now **validated at
  config load** — previously a typo'd route `mode` was silently inert
  (the runtime merge skipped unparseable values).

## [0.31.1] - 2026-07-14

### Security

- **siphon-rs bumped to `f3454c7`**, picking up upstream **#61**: a
  registration-hijack authentication-bypass fix plus remote parser
  panic fixes. Deployments using `[[register]]` / digest auth should
  update.

### Changed

- **forge-media bumped to `3c59b5f`** (lockstep with siphon-rs):
  dependency migrations — rand 0.10, openssl 0.10.81, and SRTP moving
  to aes 0.9 / aes-gcm 0.11 (cipher 0.5). Wire behavior is unchanged;
  the SIPp SRTP-SDES and DTLS scenarios pass against the new cipher
  stack.
- **`metrics` facade 0.23 → 0.24** (+ `metrics-exporter-prometheus`
  0.15 → 0.18) so forge and the daemon share one metrics recorder —
  forge moved to metrics 0.24, and a version-split facade would have
  silently dropped every `forge_*` series from `/metrics`. No metric
  names or labels changed.

No protocol, CDR, config, or API changes — a dependency-only patch.

## [0.31.0] - 2026-07-14

### Added

- **`[quality]` per-call quality history records** (P1 "Per-call
  quality telemetry", release 2 of 2 — the theme is complete). One JSON
  record per call per `interval_secs` (default 30) plus a **final
  end-of-call summary**, in exactly the CDR `quality` block's shape
  flattened with framing (`version`/`kind`/`call_id`/`ts`/`seq`) — one
  shape feeds the CDR, the records, and the live endpoint, so they can
  never drift. Ships to an append-only JSONL file and/or an HMAC-signed
  webhook over the shared delivery transport (signing,
  `X-SiphonAI-Event-Id` idempotency, and durable spool exactly as
  `[cdr.webhook]`; delivery metrics under `sink="quality"`). Off by
  default; restart-required; fail-loud when enabled with no sink.
  Records with nothing measured are skipped.
- **`GET /admin/v1/calls/{id}/stats`** (readonly role): live quality
  snapshot for one active call — the "what is this call doing *right
  now*" probe, same field shape as the CDR block. `404` when no active
  call has that bridge `call_id`.
- **Quality-history ingestion pipeline** in `examples/observability`:
  Loki + Vector services (webhook intake or JSONL file tailing; only
  `kind` becomes a Loki label — `call_id` stays a JSON field per the
  cardinality rule) and a **Per-Call Quality History** Grafana
  dashboard (MOS, RX loss, RR loss ratio, first-audio latency,
  end-of-call summary table). End-to-end ingestion guide in
  `docs/OPERATIONS.md` (live / history / CDR — three layers, one
  shape).
- **Metric**: `siphon_ai_quality_records_total{kind=interval|final}`.

The WS protocol stays **v1** and the CDR stays **v4** — this release
adds delivery surfaces, not wire changes.

## [0.30.0] - 2026-07-13

### Added

- **Local receive-side RTP stats on `rtp_stats`** (P1 "Per-call quality
  telemetry", release 1 of 2). The `rtp_stats` WS event was
  remote-reported only — RTCP Receiver Reports describing how the far
  end hears the stream SiphonAI *sends*. It now also carries the side
  SiphonAI *receives*, measured locally by forge-media
  (`MediaStatsSnapshot`, forge-media#81; pin bumped to `5fa76fb38675`):
  additive optional `rx_jitter_ms` (RFC 3550 §6.4.1 interarrival
  jitter at the negotiated RTP clock), and cumulative
  `rx_packets_received` / `rx_packets_lost` (sequence-gap transit
  loss; late arrivals repair it) / `rx_packets_out_of_order` /
  `rx_packets_duplicate`. A congested path is often asymmetric —
  the two viewpoints on one event tell "they hear us badly" from
  "we hear them badly". The WS protocol stays **v1**; schema
  regenerated and both server SDKs updated in lockstep.
- **`mos_estimate`** on `rtp_stats`: transport-only MOS-CQE in
  `[1.0, 5.0]` via the simplified E-model over local RX jitter/loss
  plus RTCP RTT — the same math heplify-server applies to SiphonAI's
  HEP QoS chunks, so Homer-side and WS-side scores agree. `null`
  until RX data exists.
- **CDR `quality` block** — **CDR `version` 3 → 4** (additive-optional
  block; bumped per the 0.9.5 new-block precedent). Per-call summary
  in the record operators already ingest: `first_audio_out_ms` (WS
  `start` on the wire → first server audio frame reaching playout —
  the STT/LLM/TTS first-token latency; closes OPERATIONS.md Q5),
  `barge_in_count` (`auto_clear` firings + server `clear` commands;
  closes Q8), `avg/max_jitter_ms`, `avg/max_packet_loss_ratio`,
  `avg_rtcp_rtt_ms` (RTCP-RR aggregates), end-of-call `rx_packets_*`
  totals, and `mos_estimate_min/avg`. Unmeasured fields are omitted,
  not zeroed; the block is omitted entirely for calls that never went
  active.
- **Metrics**: `siphon_ai_rtp_rx_jitter_ms` and
  `siphon_ai_rtp_mos_estimate` histograms, recorded on every
  `rtp_stats` emission once RX data exists.

### Changed

- The daemon now configures forge-media to publish local media-stats
  snapshots at a fixed 5 s cadence (RTCP-conventional). They feed both
  the `rtp_stats` `rx_*` fields and the CDR `quality` block, so the
  CDR populates even on routes with WS `rtp_stats` emission disabled.
  Cost: one broadcast event per receiving leg per 5 s.

## [0.29.0] - 2026-07-10

### Added

- **Protocol conformance testkit — `siphon-ai-testkit`** (P1 "Protocol
  SDKs & machine-readable schemas", final release — the theme is
  complete). A new `crates/protocol-testkit` binary that plays the
  *daemon's* side of WS protocol v1 against any candidate server — no
  SIP, no RTP, no daemon needed. Scripted calls from TOML scenarios
  (five bundled: `basic-echo`, `dtmf`, `recording-controls`,
  `hangup-semantics`, `keepalive`; `--scenario-dir` adds your own) with
  every server message validated against `schemas/siphon-ai.v1.json`
  **and** the daemon's real wire types, exact 20 ms frame sizing and
  real-time pacing asserted, §5.7 close semantics enforced (bare close
  mid-call is a violation; server `hangup` is honored daemon-style),
  unknown-event tolerance probed, and WS keepalive checked. Exit code 0
  iff conformant plus a JSON report (`--report`) — *"conformant with
  protocol v1"* is now a claim any third-party server's CI can gate on.
  See `docs/CONFORMANCE.md`.
- **`conformance` CI job** — every PR now runs the full scenario set
  against **both** SDK echo servers (`echo-ws-server-python`,
  `echo-ws-server-node`) — the first CI coverage for the Node echo
  server, closing the theme's last verification gap.

The WS protocol stays **v1**; the daemon binary is unchanged (the
testkit's one new dependency, the `jsonschema` validator, is
test-tooling only).

## [0.28.0] - 2026-07-10

### Added

- **Server SDKs — `sdks/python` + `sdks/typescript`** (P1 "Protocol SDKs
  & machine-readable schemas", second release). Two dependency-light
  packages (`siphon-ai-server`; `websockets` / `ws` respectively) that
  implement the WS bridge protocol so a bot author writes handlers, not
  wire code: WS accept with `siphon-ai.v1` subprotocol echo, typed events
  for all 21 daemon→server messages, one `Call` method per server→daemon
  command (all 17), a **paced 20 ms audio re-framer** (arbitrary byte
  pushes → exact 320/640 B frames at real time — the code every example
  hand-rolled), §5.7 close semantics (`hangup` vs bare-close drop), and
  `start.reconnected` surfaced. Zero AI dependencies. Types are
  hand-written and **validated against `schemas/siphon-ai.v1.json` plus
  every `docs/PROTOCOL.md` example** in each SDK's test suite, with full
  union coverage asserted — a new `sdk-tests` CI job runs both suites on
  every PR. Vendorable (`pip install ./sdks/python`,
  `npm install ./sdks/typescript`); registry publishing deferred.
- **`examples/echo-ws-server-node`** — new minimal echo server on the
  TypeScript SDK.

### Changed

- **`examples/echo-ws-server-python` is rewritten on the Python SDK**
  (566 → 408 lines, same CLI and behavior, every `--auto-*` test-harness
  knob kept). It remains the SIPp CI fixture, so every daemon PR now
  exercises the Python SDK end-to-end against real calls.

The WS protocol stays **v1**; the daemon binary is unchanged.

## [0.27.0] - 2026-07-09

### Added

- **Machine-readable protocol schema — `schemas/siphon-ai.v1.json`** (P1
  "Protocol SDKs & machine-readable schemas", first release; design note
  `docs/design/DESIGN_PROTOCOL_SDKS.md`). The complete WS protocol
  contract as JSON Schema draft 2020-12, **generated from the Rust wire
  types** in `crates/bridge`: `$defs/BridgeOut` (21 daemon→server
  messages) + `$defs/BridgeIn` (17 server→daemon), doc comments as
  descriptions, and an `x-binary-frames` annotation describing the audio
  half (raw PCM16-LE, 320 B @ 8 kHz / 640 B @ 16 kHz, 20 ms). Point your
  editor, validator, or code generator at it. The top level is `anyOf`
  (not `oneOf`): `hold`/`resume`/`mark` exist in both directions, so
  validate against the direction-specific union when you know who sent
  the frame. A new CI gate regenerates the schema and diffs it on every
  PR, **and validates every JSON example in `docs/PROTOCOL.md` against
  it** (39 today) — the protocol docs, Rust types, and schema can no
  longer drift apart silently. Generation is behind a dev-only
  `json-schema` cargo feature (`schemars`); the daemon binary is
  unchanged. Protocol stays **v1**.

## [0.26.0] - 2026-07-09

### Added

- **Recording consent announcement — `[recording.announcement]`** (P1
  "Recording compliance & storage", final release — the theme is
  complete). Point `file` at a "this call may be recorded" WAV and the
  daemon plays it to the caller right after answer; **capture starts only
  when the prompt finishes**. The WS session connects in parallel
  (announce-then-bridge); the bot can't talk over the prompt, and nothing
  the caller says during it reaches the recording *or* the server. With
  `mode = "on_demand"`, a `start_recording` arriving mid-prompt is
  deferred to prompt completion. **Fail-closed**: if the prompt can't play
  (missing file, wrong sample rate), the call is *not* recorded — and the
  CDR shows `consent.announced = false`. Applies to inbound and outbound
  legs. **Off by default.**
- **Consent audit trail on the CDR** — additive
  `consent { announced, announcement_ms, server }` object (schema version
  unchanged). `announced`/`announcement_ms` come from the daemon-played
  prompt; `server` from the new **`set_recording_consent`** WS control
  message (`{ "type": "set_recording_consent", "call_id", "note"? }`) —
  a stamp for consent your server captured itself (DTMF press-1, verbal
  yes). A stamp, not a gate: capture gating stays `on_demand` +
  `start_recording`. Protocol stays **v1**.
- **Outbound-leg recording.** Originated calls (`POST /admin/v1/calls`)
  can record exactly like inbound ones — same `[recording]`
  dir/encryption/format, same on-demand WS controls, same object-storage
  upload spool. Per-gateway default (`[[gateway]].recording = "off"
  (default) | "always" | "on_demand"`, validated at load) plus a
  per-originate `"recording"` override (`400` for bad values, rejected
  before a toll-fraud concurrency permit is consumed). Recording an
  outbound leg is config/API opt-in, never implied. **Off by default.**

## [0.25.0] - 2026-07-08

### Added

- **Object-storage upload — `[recording.storage]`** (P1 "Recording
  compliance & storage", second release). Finalized recordings upload to
  any S3-compatible bucket (AWS, MinIO, Cloudflare R2, Backblaze B2 —
  path-style, hand-rolled SigV4, **no AWS SDK**). Durable by design: a
  small job file lands in `spool_dir` at call teardown (atomic, survives
  restarts) and a background worker uploads with retries; a job that keeps
  failing is dropped with a metric rather than wedging the spool, and the
  local file is deleted only after a durable upload (opt-in
  `delete_local_after_upload`). `key_template` names objects with
  `{call_id}` / `{date}` / `{route}` / `{direction}`. The CDR gains an
  additive `recording_url` (`s3://bucket/key`, stamped at enqueue) and a
  new **`recording_uploaded`** lifecycle webhook (after `call_end`)
  confirms arrival with `size_bytes`. New metrics:
  `siphon_ai_recording_uploads_total{result}`,
  `siphon_ai_recording_upload_spool_depth`,
  `siphon_ai_recording_upload_seconds`. Retention/TTL stays the bucket
  lifecycle policy's job (worked recipe in `docs/RECORDING.md` §9). Pair
  with `[recording.encryption]` so the bucket only ever holds ciphertext.
  **Off by default.**
- **AWS KMS as the recording KEK — `[recording.encryption.kms]`**. The KMS
  hook the 0.24.0 envelope design reserved: each recording's data key is
  wrapped by KMS `Encrypt` (the KEK never exists outside KMS; every unwrap
  is IAM-auditable), on the same SigV4 client — still no AWS SDK. Exactly
  one of `kek` / `kms`; `endpoint` override supports KMS-compatible
  emulators. `siphon-ai decrypt-recording` gains `--kms-region` /
  `--kms-endpoint` (credentials via `AWS_ACCESS_KEY_ID` /
  `AWS_SECRET_ACCESS_KEY`); symmetric-KMS blobs name their own key, so no
  key ARN is needed to decrypt. **Off by default.**
- **Ogg-Opus recording format — `[recording].format = "opus"`**. ~10×
  smaller than WAV for voice, encoded with the same libopus the media path
  already uses and playable by ffmpeg/VLC/browsers. Streaming-native
  (RFC 7845), so nothing needs a finalize back-patch — including inside an
  encrypted envelope. Extensions: `.opus` plaintext, `.opusa` sealed.
  Adds the `ogg` crate (the theme's one new small dependency). **Default
  stays WAV.**

## [0.24.0] - 2026-07-08

### Added

- **Recording encryption at rest — `[recording.encryption]`** (P1 "Recording
  compliance & storage", first sub-item; design note
  `docs/design/DESIGN_RECORDING_COMPLIANCE.md`). With `enabled = true`, a
  `kek` (64 hex chars, referenced via `${file:}`/`${cred:}`) and a `key_id`,
  recordings are written as encrypted **`.wava` envelopes** instead of
  plaintext WAV — nothing plaintext ever touches disk. Envelope encryption:
  a fresh random 256-bit data key per recording seals the audio in
  independent AES-256-GCM chunks; the data key travels in the file header,
  wrapped by your KEK. The header names the `key_id` that wrapped it, so
  **rotating the KEK never re-encrypts audio**. Config is validated
  fail-loud at startup; a runtime wrap failure fails the *recording*
  (`recording_failed`), never the call. The CDR gains an additive
  `recording_encrypted` flag (schema version unchanged). Decrypt offline
  with the new **`siphon-ai decrypt-recording <file> --kek-file <hex>`**
  subcommand — needs no daemon config; a wrong key names the `key_id` the
  recording requires; `--allow-unfinalized` recovers a crashed capture. The
  `SAIWAVA1` container format is documented in `docs/RECORDING.md` §8 for
  third-party implementations. **Off by default.** Deps: `aes-gcm` +
  `zeroize` promoted from transitive to direct (RustCrypto; no new vendor).

### Changed

- **Recordings now appear as `<name>.part` while in progress** and are
  renamed to their final `.wav`/`.wava` name only when finalized — for
  *plaintext* recordings too. A bare `.wav` on disk is now always a
  complete file (safe for a watcher/uploader to pick up), and a daemon
  crash leaves only a `.part` instead of a WAV with placeholder header
  sizes. **If you watch the recording directory, match the final names and
  ignore `*.part`.**

## [0.23.0] - 2026-07-08

### Added

- **W3C trace-context propagation to the WS server** (P1 "Observability
  completeness"; final sub-item — the theme is complete). When
  `[observability.otlp]` is enabled, the WS upgrade request now carries
  [`traceparent`](https://www.w3.org/TR/trace-context/) (+ `tracestate` when
  non-empty), and the `start` message carries the same values in a new
  additive `trace_context` field for servers whose WS library hides upgrade
  headers. A WS server that continues the trace from either place appears in
  the **same waterfall** as the daemon's SIP/media spans — one distributed
  trace per call across both services. The span-id propagated is the daemon's
  call-root span; park-retrieve and WS-reconnect sessions stay in the same
  trace. **The protocol stays v1**: the field is absent whenever OTLP is
  disabled (the default), so existing servers see an unchanged `start` shape.
  No new knob — OTLP on ⇒ headers + field, off ⇒ neither. The reference echo
  and OpenAI-Realtime example servers show the continuation pattern. See
  `docs/PROTOCOL.md` §3.1 and `docs/CONFIG.md` → `[observability.otlp]`.

### Fixed

- **OTel span-context extraction now reaches the OTLP layer.** The 0.22.0
  init installed the OTLP tracing layer behind `tracing_subscriber::reload`,
  whose downcast barrier made the layer invisible to
  `OpenTelemetrySpanExt::context()` — span *export* worked, but anything
  asking a live span for its trace context got nothing (this would have
  silently disabled 0.23.0's propagation). The layer is now installed
  concrete with a reloadable per-layer filter (`OFF` until `[observability.otlp]`
  activates it), preserving the zero-cost-when-disabled property.

## [0.22.0] - 2026-07-03

### Added

- **OpenTelemetry / OTLP distributed tracing — `[observability.otlp]`** (P1
  "Observability completeness"; second sub-item of the theme). Export
  per-call traces over OTLP/gRPC to a collector (Tempo / Jaeger / an
  OpenTelemetry Collector). Each call is **one trace** — `on_invite →
  on_matched → accept_inbound → run → { WS bridge, media }` — with the SIP
  `Call-ID`, direction, and from/to on the root span, so an operator can see
  where a call spent its time across the daemon. Config knobs: `endpoint`
  (default `http://localhost:4317`), parent-based `sample_ratio`,
  `timeout_ms`, `service_name`, and extra resource `attributes`; independent
  of the metrics HTTP listener (traces without metrics scraping is a valid
  setup). **Off by default** and **best-effort** (CLAUDE.md §4.7): spans batch
  on a background worker and drop on overflow, so a slow or unreachable
  collector never blocks a call; a bad endpoint fails loud at startup, a
  collector that's merely down does not. When disabled the tracing layer is a
  zero-cost no-op. Pending spans flush on shutdown. See `docs/CONFIG.md` →
  `[observability.otlp]`. W3C trace-context propagation to the WS server is a
  follow-up (v0.23.0).

## [0.21.1] - 2026-07-01

### Fixed

- **SIP-over-TCP/TLS trunks no longer wedge after ~60s of a call**
  (CUCM and any persistent-connection trunk). The SIP stack closed an
  inbound TCP/TLS connection after 60s with no inbound SIP — but a trunk
  keeps its signaling connection open for a call's whole life while
  sending **no SIP at all** (RTP is out-of-band), so 60s idle was hit by
  essentially every call. The reaped connection then dropped mid-call
  re-INVITEs and BYEs (they got no response — the socket was gone before
  the transaction layer saw them), leaving the peer's dialogs stuck and
  its trunk health-check failing → `503` on new calls. The idle timeout
  is now two-phase: a short Slowloris window until a connection completes
  its first SIP message, then a long, configurable **established** timeout
  (new `[sip].tcp_idle_timeout_secs`, default `1800`; `0` disables). UDP
  is connectionless and was never affected. Requires the paired siphon-rs
  transport fix (bumped here). See `docs/CONFIG.md` → `[sip]`.

## [0.21.0] - 2026-07-01

### Added

- **Dashboards & alerts as code** (P1 "Observability completeness"; first
  sub-item of the theme). A runnable Prometheus + Grafana stack under
  [`examples/observability/`](examples/observability/) — the consumer
  artifacts for the metrics the daemon already emits, no daemon code. Ships
  a reference scrape config, **16 recording rules** (per-route call rates,
  INVITE reject ratio, latency percentiles for WS-connect / SDP-negotiate /
  call-duration / RTP-RTT / packet-loss / room-tick-lag, webhook delivery
  success ratio, registration state), **12 alerting rules** (target/
  registration down, high reject rate, dead air, slow WS connect, high RTP
  RTT / packet loss, spool backlog, delivery failing, admission flooding,
  sip-auth brute force, drain forced), and **two provisioned Grafana
  dashboards** (Fleet Overview + Call Quality). `docker compose -f
  examples/observability/compose.yaml up` stands the whole stack up.
- **Observability anti-drift CI check.** `scripts/check-observability-metrics.py`
  (new `observability artifacts` CI job) asserts every `siphon_ai_*` metric
  referenced in the shipped rules/dashboards is actually emitted by the
  daemon, and `promtool check config` validates the PromQL — so a metric
  rename can't ship silently-broken artifacts (same spirit as the version
  gate).

### Changed

- **`docs/OPERATIONS.md` made concrete.** The §11.8 "ten questions" now carry
  the worked PromQL and the covering dashboard/alert for each metrics-
  answerable one, plus a symptom → dashboard table. `docs/DEPLOY.md`'s metrics
  section points to the shipped stack. (Prometheus/Grafana for the aggregate;
  Homer for the individual call.)

## [0.20.0] - 2026-07-01

### Added

- **Signed audit-event stream — `[audit]`** (P1 "Security & abuse
  hardening"; the last sub-item of the theme). A tamper-evident trail of
  admin and security decisions for SIEM ingestion — *who did what* on the
  `[admin]` surface and *what the daemon refused* on the SIP surface —
  distinct from `[webhooks]` (ops automation) and `[cdr]` (billing).
  Ships to an append-only JSONL **file** (`[audit.file]`, for a log
  shipper) and/or an HMAC-signed **webhook** (`[audit.webhook]`, for a
  SIEM collector); enable either or both. The webhook reuses the 0.11.0
  delivery transport, so the `X-SiphonAI-Signature` HMAC (the
  tamper-evidence), `X-SiphonAI-Event-Id` idempotency, durable spool, and
  the `siphon_ai_webhook_*` delivery metrics (label `sink="audit"`) all
  behave identically. Six event types — `admin_request`, `sip_auth`,
  `invite_rejected`, `attestation_rejected`, `config_reload`,
  `cert_reload` — with an `events` allowlist. Emission is deliberately
  signal-first: `invite_rejected` records admission `rate_limited` /
  `no_trunk` / `draining` but **not** the per-packet silent flood-drop
  (auditing that DoS-shedding fast path would amplify the attack), and
  `sip_auth` records `failed` / `stale` but **not** the normal per-call
  `challenged` / `ok`. Off by default; hot-reloadable on `SIGHUP` when
  enabled at startup (enabling from off is restart-required). Best-effort
  and off the call path — a slow SIEM never blocks an admin request or a
  SIP transaction. New `docs/AUDIT.md`; see also `docs/CONFIG.md` →
  `[audit]`. Completes the P1 security & abuse hardening theme.

## [0.19.0] - 2026-06-27

### Added

- **Inbound INVITE admission control — `[sip.admission]`** (P1 "Security &
  abuse hardening"; second chunk of v0.19.0). A DoS posture beyond the
  `[[trunk]]` allowlist: shed abusive inbound INVITEs **before** any
  trunk / auth / route work. A **per-source token bucket** keyed on the
  source IP (`max_per_sec` + `burst`) answers an over-rate source `503` +
  `Retry-After`, and after `drop_after` consecutive rejects **silently
  drops** further INVITEs from it (an obvious flood doesn't earn a
  response). An optional **global `max_concurrent`** cap (read from the
  live call registry) answers `503` once the node is at capacity. Source
  buckets live in a size-capped table (`max_sources`) with idle/oldest
  eviction, so the limiter can't leak memory under a spoofed-source
  flood. New metrics
  `siphon_ai_invite_admission_total{result=accepted|rate_limited|dropped}`
  + `siphon_ai_invite_admission_sources` gauge. Off by default;
  restart-required on `SIGHUP` (part of `[sip]`). See `docs/CONFIG.md` →
  `[sip.admission]`.

- **Inbound digest authentication — `[sip.auth]`** (P1 "Security & abuse
  hardening"; first chunk of v0.19.0). Challenge inbound INVITEs with RFC 3261
  §22 / RFC 7616 digest auth, so trust no longer rests on a spoofable network
  identity (source IP / `From:` host). A new out-of-dialog INVITE that needs
  auth and arrives without a valid `Authorization` is answered `401
  Unauthorized` + `WWW-Authenticate` (nonce/realm/qop); the peer re-sends with
  a digest `response` verified against the configured credentials. Replay is
  bounded by a server nonce TTL (an expired nonce gets a `stale=true`
  re-challenge). Configured by `[sip.auth]` (`enabled`, `realm`, `algorithm` =
  MD5/SHA-256/SHA-512, `qop`, and `[[sip.auth.user]]` credentials) — passwords
  resolve via `${file:…}`/`${cred:…}` (v0.18.0). Digest is an **AND-gate with
  the `[[trunk]]` allowlist**, opt-in per trunk via `auth_required = true`, so
  a static-IP carrier that doesn't send credentials stays allowlist-only and
  isn't broken by enabling auth; with no trunks (legacy mode) every INVITE is
  challenged. New metric `siphon_ai_sip_auth_total{result=ok|challenged|failed|stale}`.
  Uses the upstream `sip-auth` server-side verifier (no siphon-rs change).
  Off by default; no protocol/CDR/schema break. See `docs/CONFIG.md` →
  `[sip.auth]`.

## [0.18.0] - 2026-06-26

### Added

- **Admin listener TLS — `[admin.tls]`** (P1 "Security & abuse hardening";
  second chunk of v0.18.0). The authenticated `[admin]` listener can now serve
  **HTTPS** directly, so the bearer token is encrypted on the wire on a
  routable bind without a TLS-terminating proxy. Set `[admin.tls].cert` +
  `.key` (both required when the table is present; missing/empty → fatal at
  load). The cert is loaded at startup (fail-loud) and **hot-reloaded on
  `SIGHUP`** alongside `[sip.tls]` — the next connection picks up the new cert,
  in-flight ones keep theirs, and a broken PEM keeps the previous cert
  (nginx-style). New metric `siphon_ai_admin_tls_reload_attempts_total`
  `{outcome=ok|failed}`. Without `[admin.tls]` a non-loopback bind still works
  but logs a sharpened startup warning (the token travels in the clear). See
  `docs/CONFIG.md` → `[admin.tls]`.

- **Secret resolution from files & systemd credentials** (P1 "Security &
  abuse hardening"; first chunk of v0.18.0). Config `${...}` references can now
  pull a secret from outside the process environment, so plaintext secrets
  needn't sit in env vars (visible in `/proc/<pid>/environ`, dumps, unit
  files). Two new source prefixes, usable anywhere `${VAR}` works:
  `${file:/path/to/secret}` (trimmed file contents — Docker/Kubernetes
  secrets, Vault-Agent templated files) and `${cred:NAME}`
  (`$CREDENTIALS_DIRECTORY/NAME` — systemd `LoadCredential=`). Same fail-loud
  pass as `${VAR}`: a missing file, unset `$CREDENTIALS_DIRECTORY`, or path
  traversal in a credential name fails the load. `${VAR}`/`${VAR:-default}`
  behaviour is unchanged (the `:-` default operator still wins, so
  `${file:-x}` stays an env reference). See `docs/CONFIG.md` → *Secrets &
  variable expansion*.

## [0.17.0] - 2026-06-25

### Added

- **Graceful shutdown & connection draining** (P0 "Production operability").
  On `SIGTERM`/`SIGINT` the daemon now **drains** instead of dropping calls
  mid-conversation: it flips `/ready` to not-ready, rejects new inbound
  INVITEs with `503 Service Unavailable` + `Retry-After` (so an upstream
  proxy/LB routes elsewhere), lets in-flight calls finish — bounded by
  `[shutdown].drain_timeout_secs` (default `30`; `0` = pre-0.17.0 immediate
  exit) — then **force-terminates any stragglers at the deadline with a real
  `BYE` + WS `hangup`** rather than a silent RTP stop. In-dialog requests
  (re-INVITE/ACK/BYE) for calls already up keep flowing so the drained calls
  aren't broken. A **second** shutdown signal during the drain forces an
  immediate exit (operator escape hatch). This is what makes zero-drop
  rolling deploys possible — pair `drain_timeout_secs` with the supervisor's
  kill grace (`terminationGracePeriodSeconds` / `TimeoutStopSec`). See
  `docs/design/DESIGN_GRACEFUL_SHUTDOWN.md` and `docs/DEPLOY.md` →
  *Graceful shutdown & rolling deploys*.
- **`[shutdown]` config table** with `drain_timeout_secs` (`docs/CONFIG.md`).
  Restart-required on SIGHUP (read once at startup).
- **`GET /admin/v1/drain`** — live drain status
  `{draining, active_calls, drain_timeout_secs, remaining_secs}` for deploy
  scripts to confirm a pod entered drain and watch the countdown (readonly
  role).
- **Drain observability:** `siphon_ai_draining` gauge (1 while draining),
  `siphon_ai_drain_seconds` histogram (how long the drain took), and
  `siphon_ai_calls_drain_forced_total` counter (calls force-ended at the
  deadline). Drain lifecycle logs throughout.
- **SIPp coverage:** a graceful-drain phase in `test-harness/sipp-scenarios`
  (`drain_graceful_bye.xml` + `drain_invite_503.xml`) asserts end-to-end that
  a deadline straggler gets a real BYE, a new INVITE mid-drain is 503'd, and
  the daemon exits within the window.

### Changed

- **CDR schema → version 3.** Adds the `drain_forced` `termination.cause`
  value (calls force-ended at the drain deadline), distinct from
  `local_shutdown`, so a deploy's forced terminations are attributable
  per-call. Also surfaced on `siphon_ai_calls_total{cause="drain_forced"}`.
  A new value in an existing enum field — no field added or removed.
- The systemd unit sketch (`docs/DEPLOY.md`) gains `TimeoutStopSec=40` so the
  default 30 s drain window fits inside systemd's stop timeout.

## [0.16.0] - 2026-06-24

### Added

- **Docs: installing from a release + a releasing runbook.**
  `docs/DEPLOY.md` gains an *Install from a release* section (verify
  checksums + cosign signature, then install the binary, the `.deb`, or the
  signed container), and a new top-level `RELEASING.md` documents the
  "bump, then tag and push" flow the workflow automates. Final chunk of the
  P0 "Release & packaging" theme.
- **Automated release workflow** (`.github/workflows/release.yml`). Pushing
  a `v*` tag now builds multi-arch static-musl binaries (`x86_64` +
  `aarch64`, cross-compiled with cargo-zigbuild), packages them as
  per-arch `.tar.gz`, emits a `SHA256SUMS`, and creates the GitHub release
  with notes extracted from `CHANGELOG.md` (pre-release tags like
  `v0.16.0-rc.1` are marked accordingly, never latest). A `preflight` job
  re-asserts tag == workspace version before anything is built. Second
  chunk of the P0 "Release & packaging" theme.
- **Debian packages** (`.deb` for `amd64` + `arm64`, via cargo-deb). Each
  release now ships installable packages built from the same prebuilt
  static binaries: they drop the binary at `/usr/bin/siphon-ai`, a default
  conffile at `/etc/siphon-ai/config.toml`, and a hardened systemd unit
  (enabled but **not** started — the default config has a placeholder
  `ws_url`), and create the `siphon-ai` service user + `/var/{lib,log}`
  dirs in the maintainer scripts. `apt install ./siphon-ai_*_amd64.deb`.
  Fourth chunk of the P0 "Release & packaging" theme.
- **Release supply chain: SBOM, signatures, and a published container.**
  Each release now ships a CycloneDX SBOM (syft), a cosign **keyless**
  signature over `SHA256SUMS` (`SHA256SUMS.cosign.bundle`, verifiable
  against the workflow's GitHub OIDC identity), and a multi-arch
  (`linux/amd64` + `linux/arm64`) container at
  `ghcr.io/thevoiceguy/siphon-ai:<tag>` (also cosign-signed; `:latest`
  only for final releases). The image is assembled from the same prebuilt
  static binaries that ship on the release — byte-identical, no recompile.
  Third chunk of the P0 "Release & packaging" theme.

### Changed

- **Docker dev image tracks the toolchain.** `docker/Dockerfile` now uses
  `rust:1.95-alpine` (matching `rust-toolchain.toml`) instead of the stale
  `rust:1.85` base, which sat below the workspace MSRV and only built
  because `rust-toolchain.toml` forced a 1.95.0 download on top of it.

- **CI: version-consistency gate.** A new `version consistency` job
  (`scripts/check-version-consistency.py`) fails the build if the
  workspace `Cargo.toml` version, the README "Current release" marker, and
  the `CHANGELOG.md` dated heading disagree — closing the drift that left
  the README at v0.12.2 while the latest tag was v0.15.0 (README corrected
  to v0.15.0). First chunk of the P0 "Release & packaging" theme
  (`docs/design/DESIGN_RELEASE_PACKAGING.md`).

## [0.15.0] - 2026-06-24

### Added

- **Per-route `[route.bridge.tls]` override** — a route can now carry its
  own mTLS client config for the WS leg (client cert/key + optional SPKI
  pin), e.g. a pinned internal handler alongside a publicly-trusted shared
  one. When present it **fully replaces** the global `[bridge.tls]` for
  matching calls; routes without it inherit the global. Compiled (cert/key
  loaded, pin parsed) at config load — a bad path fails at startup, not on
  the first matching call — and lives on `CompiledRoute`, so it swaps
  atomically with the route table on `SIGHUP` reload like the rest of
  `[route.bridge]`. The `routes` crate gains an internal `siphon-ai-bridge`
  edge (no new external crate, no cycle). `print-config` / `route-test`
  show whether a route's bridge mTLS is on. See `docs/DIALPLAN.md` §5.5.

## [0.14.1] - 2026-06-22

### Fixed

- **Delayed-offer and outbound calls never bridged audio** (no RTP in
  either direction). Every offer/answer media path — inbound delayed offer
  (offerless INVITE → offer in 200 OK → answer in ACK) and outbound
  origination — funnels through `MediaSetup::apply_answer`, which bound the
  codec + remote address and attached the tap but **never activated the
  forge session**. The session stayed in `Initializing`, so forge's RTP
  forwarding task was never spawned: nothing was decoded inbound or sent
  outbound. The tap still attached (its timers fired `rtp_stats` /
  `silence_detected`), which masked the dead media — and on inbound calls
  the v0.13.0 start-deadline then tore the call down with `server_too_slow`.
  `apply_answer` now activates the session (`Initializing → Active`, starting
  forwarding), mirroring what the early-offer inbound path already did via
  `start_session` before its 200 OK. Only the early-offer inbound path
  (INVITE-with-SDP) was unaffected, which is why forcing CUCM Early Offer /
  an MTP appeared to "fix" it. Regression test asserts the session reaches
  `Active` after `apply_answer`.

## [0.14.0] - 2026-06-20

### Added

- **Error-signaling: `rtp_timeout`, `audio_format`, `protocol_error`**
  (PROTOCOL.md §2.2 / §3.10) — the last three documented `error` codes the
  daemon detected (or could trivially detect) but never emitted. Closes the
  protocol doc↔impl drift (bug #4).
  - **`rtp_timeout`** — when the media inactivity watchdog fires (no inbound
    RTP for `[media].inactivity_timeout_secs`), the WS server is now told
    *why* (`error{rtp_timeout}` + `stop`) before the socket closes, instead
    of seeing a bare close.
  - **`audio_format`** — inbound binary frames are validated against the
    negotiated frame size (320 B @ 8 kHz, 640 B @ 16 kHz). A wrong-size frame
    is **dropped** (non-fatal) and reported via `error{audio_format}`,
    **rate-limited** to the first bad frame + at most one/sec. The call stays
    up — one malformed frame can't kill it; persistent failure is still
    caught by the dead-air / rtp watchdog.
  - **`protocol_error`** — malformed JSON, an unknown message `type`, or a
    `call_id` that doesn't match the connection now emits
    `error{protocol_error}` + `stop` before closing. A **definitive**
    teardown (new `DisconnectReason::ProtocolError`, not reconnect-eligible —
    a buggy server would just repeat the violation). Previously these
    conditions tore down silently and *were* reconnect-eligible.

## [0.13.0] - 2026-06-20

### Added

- **WS liveness: keepalive + start-deadline** (PROTOCOL.md §5.6 / §3.1) —
  two documented MUSTs that were never implemented, so a non-responsive
  WS server could wedge a live call indefinitely. Now:
  - **Keepalive** — SiphonAI sends a WS Ping every
    `[bridge].ws_ping_interval_secs` (default 15 s) and, if no Pong lands
    within `[bridge].ws_pong_timeout_secs` (default 10 s), treats the
    connection as half-open and drops the session
    (`error { code: "internal", message: "ws keepalive timeout" }`,
    best-effort). A keepalive timeout is reconnect-eligible when
    `[bridge].ws_reconnect_enabled` (0.7.3), else it tears the call down.
    Previously only a *total* TCP disconnect was detected — a hung server
    on a live socket was invisible.
  - **Start-deadline** — the WS server must send its first audio frame
    (or `hangup`) within `[bridge].server_start_deadline_secs` (default
    5 s) of `start`, else the call is torn down with
    `error { code: "server_too_slow" }` + `stop`. A definitive teardown
    (not reconnect-eligible — redialing a slow server wouldn't help).
  - All three knobs default to the spec values; `0` disables the
    corresponding guard. Daemon-wide `[bridge]` settings; applies to
    inbound, outbound, and reconnect/retrieve WS sessions alike.

### Changed

- Bumped the forge-media pin to `049a19983a95` (forge-media PR #76):
  `forge-core`'s `EventBus::publish` no longer logs a spurious `WARN` when
  it has no subscribers. Drops log noise on the per-call event path;
  logging-only, no API or behavior change.

## [0.12.2] - 2026-06-20

### Fixed

- **`siphon-ai check` silently swallowed config load-time warnings**
  (security-relevant). The read-only subcommands (`check` / `print-config`
  / `route-test`) installed no tracing subscriber, so compile-time
  `warn!`s — notably the **SRTP-master-key-in-cleartext** footgun (a
  gateway with `srtp != off` but a non-TLS transport) — were dropped,
  making the documented pre-deploy preflight strictly *less* informative
  than a real boot. Tracing is now installed before the read-only
  subcommands, so `check` surfaces exactly what the daemon prints at
  startup (then still reports `config OK` + exit 0, since these are
  warnings, not errors).
- **SIGHUP webhook/CDR-spool reload warning over-fired.** With a
  `spool_dir` configured, every reload logged "delivery changes require a
  restart" even when `[webhooks]` / `[cdr]` hadn't changed. The warning now
  fires only when the sink's own config actually changed (and the reload no
  longer needlessly rebuilds an unchanged sink).
- **`[media]` restart-required check missed codec / DTMF changes** (silent
  config drift). The reload's restart-required fingerprint hashed only
  `rtp_port_range` / `moh_file` / `srtp`, so changing `[media].codecs` (or
  any `[bridge]` default) and reloading was silently swallowed — not
  hot-applied and with no restart-required warning. The fingerprint now
  covers the full `[media]` block plus the bridge/codec defaults, so any
  such change surfaces as restart-required.

## [0.12.1] - 2026-06-19

### Added

- **SIGHUP outbound gateway hot-reload.** `systemctl reload siphon-ai` now
  also rebuilds and swaps the `[[gateway]]` set — add / remove / modify
  trunks without a restart. In-flight outbound calls keep the trunk
  (`OutboundOriginator`) they're on; new originations use the new set. The
  gateway table moved behind an `ArcSwap` in the outbound service; each
  reload mints fresh per-gateway UACs (stateless senders over the shared
  transaction manager). Requires outbound enabled and the `[outbound]`
  limits unchanged — `max_concurrent` / `rate_limit_per_sec` resize the live
  admission semaphore and stay restart-required (a reload that changes them
  warns and applies only the safe sections). Completes the `SIGHUP` reload
  surface started in 0.12.0.

## [0.12.0] - 2026-06-19

### Added

- **Config CLI subcommands.** The daemon gains read-only subcommands;
  running the daemon is unchanged (`siphon-ai --config X`, no subcommand).
  - `siphon-ai check --config X` — validate + compile and exit (no sockets,
    no runtime). Exit `0` + a one-screen summary if valid, `1` + the error
    on stderr otherwise. The CI / pre-deploy / pre-`systemctl reload`
    preflight. (Also fixes the documented-but-nonexistent `--check` flag in
    `contrib/README.md`.)
  - `siphon-ai print-config --config X [--show-secrets]` — render the
    effective compiled config (post-`${VAR}`, post per-route merge); secrets
    redacted by default.
  - `siphon-ai route-test --config X --to N [...]` — run the dialplan against
    a synthetic call (first-match-wins) and report the winning route (or
    `NO MATCH → 404`) + its effective bridge config.
- **`SIGHUP` config hot-reload.** `systemctl reload siphon-ai` re-reads the
  `--config` file and hot-applies the reload-safe sections **without dropping
  calls**: the **route table** (new INVITEs use the new dialplan; in-flight
  calls keep their match) and the **`[webhooks]` / `[cdr]` sinks** (rebuilt +
  swapped, unless a durable `spool_dir` is active for that sink — its drain
  worker can't be hot-swapped). The `[sip.tls]` cert reload (0.3.0) is folded
  into the same handler.
  - **Fail-safe:** a config that doesn't load/compile is logged + counted and
    the running config is kept — a bad edit can't take the daemon down.
  - **Restart-required sections** (`[sip]` listen/transports, `[node]`,
    `[media]`, `[observability]`, `[admin]`, `[hep]`,
    `[security.stir_shaken]`, and `[[gateway]]` — gateway hot-reload is a
    planned follow-up) are applied-by-restart; a reload that changes one logs
    a warning naming it and still applies the safe sections.
  - New metric `siphon_ai_config_reloads_total{result=applied|no_change|failed}`.

## [0.11.0] - 2026-06-19

### Added

- **Webhook & CDR delivery trust + durability.** The shared outbound HTTP
  transport (lifecycle webhooks **and** the CDR webhook) gains, all
  additively — bodies are unchanged, so the webhook and CDR schema
  `version`s are **not** bumped:
  - **Idempotency.** Every delivery carries `X-SiphonAI-Event-Id` (+ an
    `Idempotency-Key` alias) — a UUIDv4 stable across retries and any spool
    replay. Delivery is at-least-once; receivers dedupe on this id.
  - **Authenticity (opt-in `secret`).** When `[webhooks].secret` /
    `[cdr.webhook].secret` is set, deliveries carry
    `X-SiphonAI-Signature: t=<unix>,v1=<hex>` — HMAC-SHA256 over
    `"<unix>.<raw-body>"`. The timestamp is inside the signed string, giving
    the receiver replay protection from a freshness window. The secret is
    `${VAR}`-expanded and never logged.
  - **Durability (opt-in `spool_dir`).** When `[webhooks].spool_dir` /
    `[cdr.webhook].spool_dir` is set, a delivery that exhausts the in-memory
    retry budget is persisted to disk and re-attempted by a background
    worker that **resumes after a daemon restart** (spool-on-failure: the
    happy path stays zero-disk-I/O). Oldest-first with capped backoff; a
    `4xx` or poison entry is eventually discarded; a per-sink file cap bounds
    disk (dropping the newest, never evicting an already-persisted entry).
    The directory is created + write-probed at startup, so a bad path fails
    the daemon loudly (CLAUDE.md §4.6). Unset ⇒ today's best-effort behavior,
    unchanged.
  - **Delivery metrics.** `siphon_ai_webhook_deliveries_total{sink,result}`,
    `siphon_ai_webhook_delivery_attempts_total{sink,outcome}`,
    `siphon_ai_webhook_spool_depth{sink}` (gauge), and
    `siphon_ai_webhook_delivery_seconds{sink}` (histogram). `sink` ∈
    `lifecycle` | `cdr`.

  See `docs/DEPLOY.md` → *Webhook delivery: signing, idempotency, durability*
  (incl. a receiver verification snippet) and the `[webhooks]` /
  `[cdr.webhook]` config reference.

## [0.10.0] - 2026-06-19

### Added

- **Native admin authentication + RBAC.** `/admin/*` is now gated by
  bearer tokens with three nested roles — `readonly` ⊂ `operator` ⊂
  `admin`. Tokens are declared under a new `[admin]` config block
  (`[[admin.token]] { name, token, role }`), hashed (SHA-256) at load and
  compared in constant time; the secret is never logged. The
  endpoint→minimum-role map: `readonly` = all GET/list routes; `operator`
  = hangup, park/retrieve, conference create/end/add/remove; `admin` =
  **billable** origination (`POST /admin/v1/calls`), `PUT /admin/log`, and
  `POST /admin/hep/test`. Missing/invalid token → `401` (+
  `WWW-Authenticate: Bearer`); role below the minimum → `403`. Config is
  validated at load (CLAUDE.md §4.6): an `[admin]` block with no tokens, an
  empty/duplicate name, an empty secret, an unknown role, or an
  unparseable `listen` fails the daemon at startup.
- **Admin request audit + metric.** Every admin request emits a structured
  log line (actor = token **name**, role, endpoint template, result, peer
  — never the secret) and ticks
  `siphon_ai_admin_requests_total{endpoint, role, result}` (`result` ∈
  `ok` | `unauthenticated` | `forbidden` | `not_found`; `endpoint` is a
  bounded route template with ids collapsed to `:id`).

### Changed

- **BREAKING: `/admin/*` moved off the metrics listener.** Admin endpoints
  are now served **only** on the dedicated `[admin].listen`; the
  `[observability].http_listen` port serves just `/metrics`, `/health`,
  `/ready` and returns `404` for `/admin/*`. **Migration:** add an
  `[admin]` block with at least one token, repoint admin tooling at the new
  port with an `Authorization: Bearer …` header, and remove any `/admin/*`
  allow rules (or front-proxy auth) from the metrics port. If `[admin]` is
  omitted, `/admin/*` is **not served at all** (secure default) — the
  daemon still starts and serves metrics/health. The admin listener is
  plain HTTP for now (`[admin].tls` is a planned follow-up); bind it on
  loopback or front it with TLS termination. A non-loopback bind logs a
  warning at startup.

## [0.9.5] - 2026-06-19

### Fixed

- **Inbound delayed offer never bridged** (regression latent since 0.9.0).
  The daemon's packet pump special-cased ACK — it cleared the 200-OK
  retransmit timer and returned *without* dispatching the request to the
  UAS — so `on_ack` never fired and the delayed-offer call was never
  finalized from the ACK's SDP answer. Early-offer calls were unaffected
  (their ACK carries no body and needs no handling). The 200 OK with our
  offer was sent and the dialog looked up (so a BYE got a 200), which is
  why the SIPp tests — which only asserted the 200 OK content — missed
  it. Now a **body-carrying ACK is dispatched to the UAS** (`on_ack` →
  `finalize_delayed_offer` → bridge); body-less ACKs keep the
  timer-only fast path. The `delayed_offer` SIPp phase now also asserts
  the bridge actually connected.

### Added

- **Per-call CDR for delayed-offer negotiations that fail before going
  active.** A delayed-offer call whose ACK answer never arrives or is
  unusable (the 200-with-offer was sent but the call never reached a
  controller) now writes a CDR, not just a metric + log. Five new
  `TerminationCause` variants — `ack_timeout`, `missing_sdp_answer`,
  `invalid_sdp_answer`, `no_compatible_codec`, `invalid_remote_media` —
  carry the reason; `audio` is empty (no codec was negotiated) and the
  disconnect detail strings are blank. **CDR schema `version` → 2**: a
  strict consumer that exhaustively matched the v1 cause set won't
  recognise the new values, so the version is bumped per CLAUDE.md §7.7
  (the record shape is otherwise unchanged).

## [0.9.4] - 2026-06-18

### Added

- **DTLS-SRTP on the inbound delayed-offer offer (RFC 5763)** — the
  second DTLS-on-delayed follow-up; SiphonAI can now both answer (0.9.3)
  *and* offer DTLS on a delayed call. On an inbound delayed offer
  SiphonAI is the *offerer*, so with `[media].srtp` `preferred`/`required`
  and the new **`[media].srtp_offer = "dtls"`** it offers DTLS-SRTP in the
  200 OK (`UDP/TLS/RTP/SAVPF` + `a=fingerprint` + `a=setup:actpass`); the
  peer's answered fingerprint + setup arrive in the ACK, where SiphonAI
  derives its role (RFC 5763 §5) and enables the handshake. Surfaces on
  `start.srtp` (`exchange: "dtls"`). `[media].srtp_offer` defaults to
  `"sdes"` (the 0.9.2 behaviour); it only affects the delayed-offer path
  (inbound early offer always *answers* the peer's choice). SIPp
  `delayed_offer_dtls` phase added. **This completes SRTP for delayed
  offer** — SDES + DTLS, both directions. *Remaining delayed-offer
  follow-up: a per-call CDR for negotiations that fail before going
  active (today a metric + warn).*

## [0.9.3] - 2026-06-18

### Added

- **DTLS-SRTP on the outbound delayed-offer answer (RFC 5763)** — the
  first of the two DTLS-on-delayed follow-ups. When SiphonAI dials an
  **offerless** outbound INVITE and the peer's 2xx offers DTLS-SRTP
  (`UDP/TLS/RTP/SAVPF` + `a=fingerprint` + `a=setup:actpass`), SiphonAI
  now **answers** it: the gateway UAC's delayed-offer answer generator
  runs the inbound early-offer DTLS path (rewrite the offer for codec
  matching, patch the answer back to the SAVPF profile with our
  `a=fingerprint` + opposite `a=setup`, and `enable_dtls` as the
  handshake server). The generator gained a per-process DTLS certificate;
  the negotiated exchange (`dtls` vs `sdes`) is now carried on
  `OutboundAccepted` so `start.srtp.exchange` reports it correctly.
  Governed by `[[gateway]].srtp` like the SDES answer (0.9.1). SIPp
  `outbound_delayed_dtls` phase added. *DTLS on the **inbound**
  delayed-offer (where we'd offer DTLS) is the next follow-up.*

## [0.9.2] - 2026-06-18

### Added

- **SRTP on the inbound delayed-offer offer (SDES, RFC 4568)** — the
  mirror of the 0.9.1 outbound follow-up. On an inbound delayed offer
  SiphonAI is the *offerer*, so when `[media].srtp` (or a `[route.media]`
  override) is `preferred`/`required` the 200 OK now offers SDES
  (`RTP/SAVP` + `a=crypto`); the peer's answered key is installed from the
  ACK (`apply_answer`), and `required` fails the call if the peer answers
  plaintext. Surfaces on `start.srtp`. This reuses the existing
  `originate_offer`/`apply_answer` SDES path the delayed-offer accept
  already runs — it just stops hardcoding plaintext. SIPp
  `delayed_offer_srtp` phase added. *DTLS-SRTP on a delayed offer isn't
  produced (the SDES offer path only) — a remaining follow-up.*

## [0.9.1] - 2026-06-18

### Added

- **SRTP on the outbound delayed-offer answer (SDES, RFC 4568)** — the
  deferred 0.9.0 follow-up. An offerless outbound INVITE can't *offer*
  SRTP, but when the peer's 2xx carries an SDES offer (`RTP/SAVP` +
  `a=crypto`) SiphonAI now **answers** it: the gateway UAC's delayed-offer
  answer generator runs the same SDES negotiation the inbound early-offer
  path uses (rewrite the peer offer for codec matching, patch the answer
  back to `RTP/SAVP` with our `a=crypto`, install the keys on the leg).
  Governed by `[[gateway]].srtp` — `preferred` answers SRTP when offered
  (else plaintext), `required` fails the call on a plaintext peer offer.
  Surfaces on `start.srtp` and `siphon_ai_outbound_srtp_total{result}`
  like early-offer outbound SRTP. SIPp `outbound_delayed_srtp` phase
  added. *DTLS-SRTP on a delayed answer is not handled (no per-call cert
  in the generator), and SRTP on the **inbound** delayed-offer (where we
  offer) remains a separate follow-up.*

## [0.9.0] - 2026-06-18

Theme: **SIP delayed offer (offerless INVITE).** SiphonAI previously
**required** an inbound INVITE to carry an SDP offer and rejected an
offerless one, forcing interop partners (notably **Cisco CUCM**) to
insert a Media Termination Point. Delayed offer is now supported in both
directions, so the MTP can be removed and media flows directly. Protocol
stays `version: "1"` (no WS message change — `start` is just deferred by
one SIP round-trip until the codec is known from the answer); CDR stays
at its current `version` (the new outcomes surface as a metric, not a CDR
reason — a call that fails negotiation never went active).

### Added

- **SIP delayed offer (offerless INVITE), RFC 3264 — inbound and
  outbound.** Removes the forced **MTP** on CUCM (and similar)
  trunks/phones so media flows directly. Early-offer calls are unchanged.
  - **Inbound** (chunk 1): an inbound INVITE with no SDP is accepted —
    SiphonAI allocates media, puts **its own offer** in the 200 OK, and
    reads the peer's **answer from the ACK** before bridging. On by
    default; gate with `[sip].allow_delayed_offer = false` to force strict
    early offer (offerless INVITE then rejected `488`). The ACK-answer
    wait is bounded by SIP Timer H (~32 s); the call is active only after
    the answer is parsed. Metric `siphon_ai_delayed_offer_total{result}`.
  - **Outbound** (chunk 2): `POST /admin/v1/calls` with `"delayed_offer":
    true` dials an **offerless INVITE**, takes the peer's offer from the
    2xx, and answers in the **ACK** (via the gateway UAC's RFC-3264 answer
    generator). Delayed-outbound legs get transfer/hold like early-offer
    ones. (SRTP on the delayed-offer answer is a follow-up — the offerless
    INVITE can't carry an SDES offer.)
  - SIPp `delayed_offer` (inbound) and `outbound_delayed` phases added.

## [0.8.2] - 2026-06-17

### Added

- **Opus SDP `fmtp` (RFC 7587)** — the deferred 0.8.0 Opus follow-up.
  SiphonAI now advertises `a=fmtp:<pt> maxplaybackrate=16000;
  sprop-maxcapturerate=16000; stereo=0; sprop-stereo=0; useinbandfec=1;
  usedtx=0` for Opus — telling the peer we want **mono at ≤16 kHz** (the
  16 kHz bridge rate forge runs Opus at) and asking for in-band FEC. On
  the outbound **offer** it's keyed to our PT (111); on the **answer** it
  is re-keyed onto the *negotiated* payload type, so it survives a peer
  offering Opus at a different dynamic PT (the upstream negotiator carries
  fmtp forward by the offered PT, which would otherwise drop our tuning).
  Opus was already functionally correct without this — forge decodes mono
  at 16 kHz regardless — so these are quality/bandwidth hints, additive,
  no protocol/CDR change. Other codecs (G.711/G.722) remain fmtp-free.
  The SIPp `opus` phase now also `check_it`-asserts the answer `a=fmtp`.

## [0.8.1] - 2026-06-17

### Fixed

- **Outbound REGISTER advertised `0.0.0.0` in `Via` and `Contact` when
  `[sip].listen` used a wildcard bind** (`0.0.0.0:5060` / `[::]:5060`).
  The registration drive task used the socket *bind* address for the
  `Via` sent-by and `Contact` host, so a wildcard bind leaked into the
  outbound REGISTER — `0.0.0.0` is not a routable Contact, so registrars
  (e.g. CUCM) could not send INVITEs back, breaking inbound calls and
  registrar classification. REGISTER now advertises
  `[node].public_address` combined with the listen port — the same
  reachable address the inbound UAS already uses for its `Contact`. The
  socket still binds the configured (possibly wildcard) address; only
  the advertised SIP headers changed. A concrete, non-wildcard
  `[sip].listen` is unaffected. (`[node].public_address` is required
  whenever the bind is unspecified, so the advertised address is always
  routable.)

## [0.8.0] - 2026-06-17

Theme: **Opus codec support.** SiphonAI advertised only G.711/G.722 and
**rejected Opus at config load**; the v1 plan deferred it (DEV_PLAN §15.1)
as blocked on resampling. Opus is now negotiable. It runs at a **16 kHz
bridge rate** — `opus/48000/2` on the wire, but the WS path sees a 16 kHz
session (`start.audio.sample_rate = 16000`) — so the fixed 8/16 kHz PCM16
audio contract (CLAUDE.md §4.2) is unchanged and the WS protocol stays
`version: "1"`. **Off by default** (add `"opus"` to `[media].codecs`).
Minor-version bump because it adds SiphonAI's first **native build
dependency** (libopus). Delivered across three chunks (forge-media PR →
siphon-ai enablement → harness/docs/release).

### Added

- **Opus in `[media].codecs`.** A peer that offers `opus/48000/2` (or a
  route that lists `"opus"` for outbound) now negotiates Opus. The media
  engine (forge) runs the codec at 16 kHz mono — libopus decodes any
  encoded stream to 16 kHz and downmixes stereo→mono internally, and the
  encoder takes 16 kHz mono PCM (RFC 7587 — the RTP clock stays 48 kHz).
  RTP timestamps step at the 48 kHz clock; only the WS-facing PCM is
  16 kHz. The dynamic Opus payload type is preserved on the answer
  (RFC 3264). `docs/CONFIG.md`.
- **SIPp `opus` regression phase** (`opus_caller.xml`): offers Opus, asserts
  the 200 OK answers Opus and the daemon brings the call up at 16 kHz
  (`negotiated=opus sample_rate=16000`). Signalling only — the Opus
  encode/decode round-trip is forge unit-tested.

### Changed

- **Upstream forge-media pin `e95a31a959a6` → `3c82c2e5d175`** — adds the
  Opus 16 kHz bridge rate (forge-media#75, mirroring G.722's
  wire-clock-vs-PCM-rate split) and enables forge-engine's `opus` feature.
  Also picks up an unrelated SDES mid-call re-key API (forge-media#72),
  unused here.
- **New native build dependency: libopus** (via `audiopus`/`audiopus_sys`,
  built from source). Building `siphon-ai` now needs a C toolchain + CMake;
  the shipped Dockerfile already has them. `docs/DEPLOY.md` gains a build-
  prerequisites note. The runtime image is unaffected (statically linked).

### Notes

- **SDP `fmtp` (`stereo=0` / `useinbandfec` / `maxplaybackrate`) is a
  follow-up.** Opus is correct without it (the `/2` rtpmap is emitted and
  forge decodes mono regardless); the params interact with the answer's
  dynamic PT and want validation against a real softphone/carrier
  (`docs/design/DESIGN_OPUS.md` §7.5).

## [0.7.5] - 2026-06-17

Follow-up to 0.7.2: **bot-initiated hold on outbound legs.** The hold/resume
drive shipped in 0.7.2 was inbound-only — it built the hold/resume re-INVITE
offers from the inbound side's cached answer SDP, which the outbound originate
path didn't retain. This closes that gap, so a call placed via
`POST /admin/v1/calls` can be held/resumed by the WS server exactly like an
inbound call. No protocol or CDR change; hold remains always-available (no
flag).

### Changed

- **Outbound originated calls now support `hold` / `resume`.** `apply_answer`
  retains the SDP **offer** we sent (`OutboundAccepted.offer_sdp`), and the
  outbound `run_call` builds a `HoldContext` from it (direction-flipped to
  `sendonly` / `sendrecv`) with the same `DialogControl` it uses for outbound
  transfer (the directly-held dialog, re-INVITE via the gateway UAC). The gap
  music reuses the shared `[media].moh_file`.
- SIPp **outbound_bot_hold** regression phase (`outbound_bot_hold_uas.xml`):
  SiphonAI dials out, the echo-ws (`--auto-hold`) drives `hold`/`resume`, and
  the callee asserts it receives the sendonly/sendrecv re-INVITEs —
  `holds_total{result="ok"}` reads 2.

With this, both bot-hold and WS reconnect now work on inbound **and** outbound
legs.

## [0.7.4] - 2026-06-17

Follow-up to 0.7.3: **WS reconnect on outbound legs.** The reconnect drive
shipped in 0.7.3 was inbound-only — the controller logic is bridge-generic,
but the `[bridge].ws_reconnect_*` settings weren't threaded into the
outbound originate path. This closes that gap, so a call placed via
`POST /admin/v1/calls` reconnects the same way an inbound call does when
its WS drops. Still gated by `[bridge].ws_reconnect_enabled` (off by
default); no protocol or CDR change.

### Changed

- **Outbound originated calls now honour `[bridge].ws_reconnect_enabled`.**
  The originate path threads the daemon's reconnect settings (enabled,
  `ws_reconnect_max_secs`, and the shared `[media].moh_file` for the gap)
  through to the call controller and puts the leg's tap in survive-WS-drop
  mode — identical behaviour and code path to inbound. A new
  `OutboundService::with_moh_file` carries the hold-music file.
- SIPp **outbound_reconnect** regression phase: SiphonAI dials out, SIPp
  answers, the echo-ws (`--drop-after-ms`) drops, SiphonAI re-dials and
  resumes (`reconnected: true`), and the call ends cleanly — asserting
  `ws_reconnects_total{result="recovered"}`.

## [0.7.3] - 2026-06-17

Theme: **WS reconnect mid-call** — opt-in resilience. Until now, any
unexpected drop of the WebSocket to the developer's server killed the
call (fallback prompt → BYE → CDR `ws_disconnect`), so a server deploy /
restart / network blip took out every in-flight call. With
`[bridge].ws_reconnect_enabled = true`, SiphonAI instead keeps the SIP
call up on hold music and re-dials the **same** `ws_url`, resuming on a
fresh session keyed by the same `call_id` — falling back to teardown only
if no redial succeeds within a bounded window. **Off by default**; the WS
protocol stays `version: "1"` (additive) and the CDR schema stays at
version 1. Delivered across three chunks (config + protocol surface →
reconnect drive → observability/docs/harness/release).

### Added

- **Automatic WS reconnect (`[bridge].ws_reconnect_enabled`).** On an
  **unexpected** drop (server closed the socket without a `hangup`, an
  IO/TLS error, or a keepalive timeout) SiphonAI parks the caller on hold
  music and re-dials the same `ws_url` with exponential backoff
  (250 ms → ×2 → cap 5 s), resuming on a fresh session. Bounded by
  **`[bridge].ws_reconnect_max_secs`** (default 30) — how long the caller
  hears hold music before reconnect gives up and the §5.7 teardown runs.
  Both knobs take a per-route `[route.bridge]` override; enabling with a
  zero window fails loud at load. `docs/CONFIG.md`.
- **`start.reconnected`** — a new additive boolean on the `start` message
  (omitted-when-false, like `retrieved`), `true` on the session that
  resumes a call after a drop. The server should drop any handler it still
  holds for that `call_id` and treat the new socket as the live one; `seq`
  restarts at 0 and there is **no replay** of pre-drop audio/events.
  `docs/PROTOCOL.md` §3.1, §5.7.
- **Metric `siphon_ai_ws_reconnects_total{result=recovered|exhausted}`**
  and **CDR `reconnect { count, total_gap_ms }`** (additive, schema stays
  v1) — per-call reconnect-episode accounting. `docs/DEPLOY.md`.
- **SIPp `ws_reconnect` regression phase** — an echo-ws started with
  `--drop-after-ms` closes the socket mid-call; the daemon reconnects, the
  redial's `start` carries `reconnected: true`, and the call resumes and
  ends cleanly (asserts `ws_reconnects_total{result="recovered"}`).

### Changed

- **PROTOCOL.md §5.7 rewritten.** Reconnect is now supported (opt-in).
  With it on, **a call is ended by the `hangup` control message** — a bare
  WS socket close (even a clean `1000`) is treated as an unexpected drop
  and reconnected. With reconnect **off**, the v1 behaviour is unchanged.
- **`MediaTap` survive-WS-drop mode.** Internally, a reconnect-enabled
  call's tap treats a closed WS-facing channel as non-fatal (it holds for
  the redial) rather than tearing down — park parks *before* closing, but
  reconnect reacts *to* the close, so the tap had to learn to outlive it.

### Notes

- Inbound legs only this release. Outbound bot-hold and outbound reconnect
  remain follow-ups (the reconnect drive is bridge-generic, but the
  settings aren't threaded into the originate path yet).

## [0.7.2] - 2026-06-16

Theme: **bot-initiated hold/resume** — the WS server can now put its own
caller on hold and bring them back, with SiphonAI driving a true SIP
re-INVITE. Until now `hold`/`resume` existed only as inbound *events* (the
far end held *us*, §3.3); the bot could drive every other call-control
primitive (transfer, hangup, park, record, mute, DTMF, conference) but not
hold. This closes that gap. **No config flag** — hold is always available on
inbound legs; `[media].moh_file` only chooses what the held caller hears.
The WS protocol stays `version: "1"` (additive) and the CDR schema stays at
version 1. Delivered across three chunks (protocol surface → re-INVITE drive
→ observability/docs/SIPp/release).

### Added

- **`hold` / `resume` (server → SiphonAI).** The WS server puts *its own*
  caller on hold (`{ "type": "hold", "call_id": … }`) and resumes them
  (`resume`). SiphonAI becomes the re-INVITE **offerer** (`a=sendonly` to
  hold, `a=sendrecv` to resume), plays hold music to the caller, and stops
  forwarding caller audio to the server while held (no barge-in during
  hold). On success it replies `held` / `resumed` (§3.13) — past-tense acks,
  deliberately distinct from the §3.3 peer-hold events. `docs/PROTOCOL.md`
  §4.10, §3.13, §3.10.
- **`error { code: "hold_failed" }`.** A re-INVITE that's rejected, times
  out, can't resolve glare (RFC 3261 §14.1 — backoff + retry-once), or is
  refused because the far end already holds us (no hold-stacking in this
  first cut) fails the hold without dropping the call — it stays in its
  prior media state.
- **`[media].moh_file`.** Hold music for bot-initiated hold (shared shape
  with `[park].moh_file`): a WAV at the call's negotiated rate, validated to
  exist at load. Unset → generated comfort silence. `docs/CONFIG.md`.
- **CDR `hold { count, total_ms }`.** Per-call bot-hold accounting, mirroring
  `park`. Present only when the bot held the call at least once; omitted
  otherwise, so the CDR schema stays at version 1. Counts bot-initiated
  holds only — a far-end hold isn't tallied. `docs/DEPLOY.md`.
- **Metric `siphon_ai_holds_total{result=ok|failed}`.** Covers both
  directions (hold and resume). `docs/DEPLOY.md`.
- **SIPp `bot_hold` regression phase.** The inverse of
  `reinvite_hold_resume.xml`: an echo-ws started with `--auto-hold` drives
  `hold` → `resume` → `hangup`, and `bot_hold_caller.xml` asserts it
  *receives* a sendonly re-INVITE then a sendrecv one (SiphonAI is the
  offerer), with `siphon_ai_holds_total{result="ok"}` reading 2.
- **Playout-gated barge-in debounce (`[bridge.barge_in].debounce_ms`)**
  (#173 — merged between 0.7.1 and this release, so 0.7.2 is its first
  tagged release). While the bot is playing out, a `speech_started` is held
  for `debounce_ms` and only flushes if speech *sustains* past it — an
  echo / brief-background-noise gate that stops the bot cutting itself off
  on its own echo. Barge-in stays **immediate while the bot is silent**, so
  a caller interrupting between phrases is unaffected. `0`/unset = off
  (original immediate-flush behaviour); only affects `auto_clear`. Per-route
  override via `[route.bridge.barge_in].debounce_ms`. `docs/CONFIG.md`.

### Changed

- **Upstream siphon-rs pin `db45e42` → `8f3fd80`.** Adds
  `IntegratedUAC::send_reinvite_via_flow` — the flow-aware counterpart of
  `send_reinvite`, mirroring `send_refer_via_flow` over the INVITE
  transaction. Bot-hold needs it: on a TCP/TLS inbound dialog (e.g. Twilio
  TLS trunking) the peer's `Contact` names an ephemeral port nothing listens
  on, so the re-INVITE must reuse the inbound connection — the same fix
  `#157`/`#159` applied to BYE and REFER.
- **`TransferContext` refactored to embed a shared `DialogControl`**
  (`{ uac, source, flow }`), so hold and transfer share one dialog-resolution
  + connection-reuse path instead of duplicating it.

### Notes

- Inbound legs only this release. Outbound bot-hold needs the originated
  offer SDP plumbed through `apply_answer` to build the hold/resume offers;
  it's a follow-up.

## [0.7.1] - 2026-06-15

Theme: **outbound SRTP** — SiphonAI could *answer* an inbound SRTP offer but
only ever *offered* plaintext `RTP/AVP`, so outbound calls couldn't carry
audio on secure trunks (e.g. Twilio secure trunking). This closes that
inbound↔outbound asymmetry via SDES (RFC 4568) on the offer. **Off by
default**; the WS protocol stays `version: "1"` and the CDR schema is
unchanged. Self-contained in SiphonAI — no upstream forge-media change (the
crypto primitives are public at the pinned rev). Delivered across three
chunks (media-glue core → config/protocol/observability → SIPp/release).

### Added

- **Outbound SRTP via SDES (`[[gateway]].srtp`).** A call placed through a
  gateway with `srtp = "preferred" | "required"` now *offers* SRTP: SiphonAI
  mints an `AES_CM_128_HMAC_SHA1_80` master key, sends the INVITE as
  `RTP/SAVP` with an `a=crypto:` line, and on a 2xx that accepts it installs
  the send/recv keys onto the trunk leg (`session.srtp_a()` —
  `install_srtp_keys`), so the media is encrypted.
  * `[[gateway]].srtp` — `"off"` (default) | `"preferred"` | `"required"`,
    the outbound mirror of `[media].srtp`. `required` fails the call if the
    trunk answers plaintext; `preferred` continues unencrypted (downgrade).
    A per-gateway load-time warning fires when `srtp` is set but
    `transport != "tls"` (the SDES key would travel in cleartext on the
    signalling plane). `docs/CONFIG.md`, `docs/OUTBOUND.md`.
  * `start.srtp` (`{ exchange: "sdes", profile }`) is now populated on
    **outbound** calls too, the same shape inbound uses (this also corrects
    the stale "SDES not yet produced" note in `docs/PROTOCOL.md` — inbound
    SDES was already produced; only the outbound offer side was missing).
  * Metric `siphon_ai_outbound_srtp_total{result=encrypted|downgraded}`
    (`docs/DEPLOY.md`). A SIPp **outbound_srtp** regression phase exercises
    the full negotiation: a `required` gateway, SIPp answering `RTP/SAVP` +
    `a=crypto`, asserting the `encrypted` metric.
  * Implemented entirely in SiphonAI using public forge-sdp / forge-engine
    APIs at the current pin — no upstream PR, no pin bump.

## [0.7.0] - 2026-06-15

Theme: **conferencing + media-only call park** — two operator-controllable
multi-leg features, both **off by default** (fail-closed like `[outbound]`,
so a 0.6.x config upgrades with zero behaviour change). Conferencing mixes
N calls into one room where *every* leg keeps its own WS session (no single
"host" bot); call park shelves a call on hold music with **no** WS session,
to be retrieved later onto a fresh session by an operator. Delivered across
five chunks (room core → WS surface → conference admin → park → docs/SIPp/
release). The WS protocol version stays `"1"` — every addition is a new
message, event, or error code.

### Added

- **Conference admin CRUD (0.7.0 chunk 3 of 5).** Operators can compose and
  inspect rooms over the admin HTTP API; webhooks announce room lifecycle.
  All endpoints `501` when `[conference].enabled = false`. Same private-bind /
  no-native-auth posture as the originate API.
  * `GET /admin/v1/conferences` — list live rooms + their member call-ids.
  * `POST /admin/v1/conferences` — pre-create an (initially empty) room
    (`{room_id?, sample_rate?}`; `201 {room_id}`, generated id when omitted).
  * `DELETE /admin/v1/conferences/:id` — force-end a room; every member
    reverts to its direct pair (`conference_left { room_closed }`).
  * `POST /admin/v1/conferences/:id/participants` `{call_id}` — add **any**
    active call (inbound or outbound) to a room; `DELETE …/:call_id` removes
    one. Both `202` (dispatched): the daemon signals the target call, which
    joins/leaves on its own WS session — the outcome surfaces there
    (`conference_joined` / `conference_left` / `error`), not in the HTTP reply.
  * Cross-call add/remove respects CLAUDE.md §4.4 — it pushes a
    `ConferenceCommand` onto the target call's `CallHandle` (via a new
    daemon-wide bridge-id → handle `CallControlRegistry` populated by both the
    acceptor and the outbound service); that call's own controller runs the
    same join/leave path a WS `conference_join` would. No reaching into
    another call's state.
  * Webhooks `conference_created` (first join / pre-create) and
    `conference_ended { duration_ms, peak_participants }` (last leave /
    force-end), via a room-lifecycle observer. `docs/DEPLOY.md`, `docs/CONFIG.md`.

- **Conference-room core (0.7.0 chunk 1 of 5 — internal API only; the WS
  protocol + admin surfaces land in later chunks).** A room is one daemon
  task owning a `forge-mixer` `AudioMixer` and a 20 ms tick; joined calls
  contribute their SIP leg *and* their WS session as two mixer participants
  (DEV_PLAN_0.7.0.md §9.1), and every sink hears the room minus its own
  input — the caller never hears themselves, each bot still hears its own
  caller. Pieces:
  * `[conference]` config block (`enabled` — **off by default**, fail-closed
    like `[outbound]`; `max_rooms` 16; `max_participants_per_room` 8 calls;
    `join_tones`), validated at load. A 0.6.x config upgrades with zero
    behaviour change. `docs/CONFIG.md`.
  * `ConferenceRegistry` (core): exact-id `room_id → RoomHandle` map in the
    `CallRegistry`/`ConsultRegistry` §4.4 shape — rooms spawn on first join
    (locked to the first joiner's sample rate; mismatched joins rejected, no
    resampling in 0.7.0) and end on last leave.
  * Tap re-plumbing (`TapCommand::JoinRoom`/`LeaveRoom`): joining swaps the
    direct caller↔WS pair for room routing inside the tap task (single
    owner, no locks — the mute/flush pattern); leaving or the room dying
    always restores the direct pair. `clear`/`mute`/barge-in `auto_clear`
    also flush the bot's audio buffered in the room. Per-leg recording keeps
    working (right channel = the room mix the caller actually heard).
  * Mixing is drain-once + subtract-self: upstream's `mix_excluding` drains
    per call, so per-sink mix-minus-self is computed from one
    `get_all_participant_audio` pass per tick with upstream's own
    auto-gain/clamp semantics (a `mix_all_excluding` upstream API would
    replace this).
  * Metrics: `siphon_ai_conferences_active`,
    `siphon_ai_conference_participants`,
    `siphon_ai_conference_joins_total{result}`,
    `siphon_ai_room_tick_lag_seconds`,
    `siphon_ai_room_frames_dropped_total{stage,side}` (`docs/DEPLOY.md`).
  * New upstream deps: `forge-mixer`, `forge-injection` (same pinned rev as
    the rest of forge-media). Deliberately **not** `forge-conference` — its
    DTMF-IVR/PIN/host-control layer is out of scope per §9.4.

- **Conference WS protocol surface (0.7.0 chunk 2 of 5).** The WS server can
  now drive conferencing for its own call (self-scoped, §9.2); the protocol
  version stays `"1"` (all additions are new messages / a new error code).
  * Server → SiphonAI: `conference_join { room_id }` (creates the room if
    absent, subject to caps) and `conference_leave`. `docs/PROTOCOL.md` §4.8.
  * SiphonAI → server: `conference_joined { room_id, participants }`,
    `conference_left { room_id, reason }` (`reason` = `left` |
    `room_closed`), and the fan-out events `participant_joined` /
    `participant_left { room_id, participant_call_id }` to every *other*
    member when the room's composition changes. `docs/PROTOCOL.md` §3.12.
  * New `error` code `conference_failed` — a refused join (disabled, cap
    reached, sample-rate mismatch, already joined); the call continues on its
    direct pair.
  * Wired into both inbound (`BridgingAcceptor::with_conference`) and
    outbound (`OutboundService::with_conference`) calls; the daemon builds
    one shared `ConferenceRegistry` from `[conference]` when enabled. The
    async join runs off the controller's control loop (spawned, like REFER).
  * Reference echo server (`examples/echo-ws-server-python`) gains
    `--auto-conference-join ROOM` and logs the new events — the harness hook
    for the chunk-5 two-caller SIPp scenario.

- **Media-only call park + retrieve (0.7.0 chunk 4 of 5).** Park shelves a
  call **without** a WS session: the caller hears hold music, the SIP dialog
  + RTP stay up, and the call is later **retrieved** onto a *fresh* WS session
  (or times out / hangs up). The one chunk that reworks the per-call
  controller lifecycle — the media tap becomes the durable owner and the WS
  bridge becomes swappable. `docs/PARK.md`, `docs/design/DESIGN_0.7.0_PARK.md`.
  * `[park]` config block (`enabled` — **off by default**; `moh_file`
    optional, validated + decoded at load, comfort noise when unset or on a
    sample-rate mismatch; `timeout_secs` 300 / `0` = indefinite;
    `timeout_action` `hangup`|`keep`; `max_parked` 32). Global only.
    `docs/CONFIG.md`.
  * WS protocol (version stays `"1"`): `park { call_id, slot? }` (server parks
    its own call, self-scoped), `stop { reason: "park" }`, `start.retrieved`
    on a retrieved session, and `error` code `park_failed`. `docs/PROTOCOL.md`
    §3.1 / §3.9 / §3.10 / §4.9.
  * MOH on a 20 ms monotonic tick into forge playout (looping `FileSource` at
    the call's rate, else `forge-injection` comfort noise); a parked call's
    `MediaTap` task stays alive (it owns the forge media handle), while its WS
    bridge detaches and is re-spawned fresh on retrieve.
  * Admin API: `GET /admin/v1/parked`, `POST /admin/v1/calls/:id/park`
    `{slot?}`, `POST /admin/v1/calls/:id/retrieve` `{ws_url?}` (both `202`
    dispatched; retrieve is operator-only — there is no WS retrieve message).
    `501` when park is off, `404` unknown call, `409` retrieve of a non-parked
    call. `docs/DEPLOY.md`.
  * Observability: webhooks `call_parked` / `call_retrieved` / `park_timeout`;
    metrics `siphon_ai_parks_total{result}`,
    `siphon_ai_retrieves_total{result}`, `siphon_ai_parked_calls_active`; CDR
    `park { count, total_ms }` (additive, schema stays v1). Recording in
    progress at park keeps writing (records the MOH the caller hears).
  * Applies to inbound **and** outbound calls (any call in the
    `CallControlRegistry`). Reference echo server gains `--auto-park[=SLOT]`.

- **0.7.0 docs, SIPp coverage, and release (chunk 5 of 5).** Feature guides
  `docs/CONFERENCE.md` and `docs/PARK.md` (joining flow, admin control,
  limits, testing); doc-drift fixes in `CLAUDE.md` §8 and `docs/DEV_PLAN.md`
  (recording / outbound / conferencing / park are delivered, not "out of
  scope"; `forge-mixer` + `forge-injection` are now used). SIPp signaling
  regression gains three live phases — conference two-caller mix, park →
  retrieve → hangup, and park → timeout → hangup — each cross-checking the
  feature's metric.

## [0.6.2] - 2026-06-12

Theme: **TLS trunk hardening** — the fixes found by running v0.6.1 against a
production TLS trunk (Twilio secure trunking), plus the dispatcher growing
outbound TCP/TLS so gateways and registrations can dial secure trunks, not
just answer them. Everything new is off by default; the WS protocol stays at
`version: "1"` and the CDR schema is unchanged. A 0.6.1 deployment upgrades
with zero config changes.

### Added

- **Outbound dialing over TCP/TLS (`[[gateway]].transport`).** The transport
  dispatcher was inbound-only: any request needing a fresh TCP/TLS connection
  (an originated INVITE to a TLS trunk, a REGISTER to a TLS registrar) died
  with `outbound … without an existing stream is not supported in v1`. The
  dispatcher now owns client connection pools (`sip-transport`'s
  `ConnectionPool`/`TlsPool`, the pattern proven in siphond): outbound TCP/TLS
  with no established stream dials out through the pool, reuses the connection
  on subsequent requests, and the pool's reader feeds responses back into the
  same inbound packet pipeline the listeners use. TLS verifies the peer against
  the bundled webpki (Mozilla CA) roots — sufficient for public trunks like
  Twilio — plus an optional `[sip.tls_client].extra_ca` PEM bundle for
  private-CA deployments and self-signed test rigs (path validated at load).
  SNI is the gateway's proxy host, threaded through the existing
  `TransportContext::server_name`.
  * `[[gateway]]` gains `transport = "udp" | "tcp" | "tls"` (default udp).
    Non-UDP appends `;transport=…` to the Request-URI so RFC 3263 resolution
    selects the right transport; `tls` flips the default proxy port to 5061.
    With `register` set the transport is inherited from the register block and
    an explicit `transport` is rejected at load. `[[register]]` blocks with
    `transport = "tls"` — documented since 0.3.0 but broken by the same
    dispatcher gap — now actually go out over TLS.
  * Note: media on outbound legs is still plain RTP. Trunks that require SRTP
    (e.g. Twilio secure trunking) need the follow-up SDES change before
    outbound calls carry audio — this change is signaling-transport only.

- **Deepgram/LLM example bot: human-handoff transfer triggers**
  (`examples/deepgram-llm-bot-node/`). With `BOT_TRANSFER_TARGET` (a SIP URI)
  set, the bot hands the caller off via the protocol's `transfer` frame
  (PROTOCOL.md §4.4) through two routes sharing one announce-then-REFER path:
  a deterministic keyword fast-path over final utterances
  (`BOT_TRANSFER_PHRASE`, e.g. "transfer me" / "speak to a human"), and a
  `transfer_call` tool offered to the LLM so natural phrasings the regex
  misses still trigger the handoff. The tool only signals intent — the
  destination is always `BOT_TRANSFER_TARGET`; the model never chooses a URI.
  Example-only; no daemon changes.

### Fixed

- **TLS trunks: call transfer (REFER) failed with `transfer_failed` (#159).** The known
  gap left by the cleanup-BYE fix below: a `transfer` requested by the WS server on a
  call that arrived over TCP/TLS died with `send_refer: … transport error`, because
  upstream `send_refer` resolves the dialog's remote target and dials a fresh
  connection the inbound-only dispatcher refuses to open (and the peer's Contact names
  an ephemeral source port nothing listens on anyway). The transfer task now reuses the
  inbound connection captured at INVITE time: `TransferContext` carries the same
  `DialogFlow` that `TeardownContext` got in the BYE fix (attached in `run_call`, once
  the accepted session's transport is known), and `run_transfer_inner` sends both the
  REFER and the post-REFER BYE through the new upstream `send_refer_via_flow` /
  `bye_via_flow` (siphon-rs#58). `DialogFlow` additionally captures the receiving
  listener's local address so the auto-filled `Via` on flow-routed requests advertises
  the TLS listener's port instead of the UDP listener's (the cosmetic nit observed in
  the #157 verification). UDP dialogs and outbound (gateway-originated) legs keep the
  existing resolve-and-send path. Pin bumped to siphon-rs `db45e42251c3`, which also
  changes the `*_via_flow` call convention to the new `Flow` struct.

- **TLS trunks: daemon-initiated BYE never reached the peer (caller heard dead air
  after the bot hung up).** The companion to the Contact-port fix below, in the other
  direction. When the WS server ended the call (`hangup`), or a session timer / admin
  force-hangup drove teardown, the cleanup BYE was sent via `IntegratedUAC::bye`,
  which resolves the dialog's remote target and builds a fresh transport context —
  but the dispatcher is inbound-only and refuses to open a new TCP/TLS connection,
  so the BYE died with `outbound BYE failed … transport error` and the peer held the
  call until its own timeout. The acceptor now captures the inbound connection's
  writer channel at INVITE time (`DialogFlow`) and sends the cleanup BYE through
  `IntegratedUAC::bye_via_flow` over that same connection (RFC 5626 flow semantics).
  UDP dialogs keep the existing path. (The matching REFER gap this fix left open
  is also closed in this release — see the transfer entry above.)

- **TLS trunks: in-dialog ACK/BYE were lost (silent-tail recordings, wrong CDR cause).**
  When the daemon ran both a UDP and a TLS listener (`[sip].transports = ["udp", "tls"]`),
  the `Contact` on responses advertised the UDP listener's port with `transport=tls`
  (e.g. `<sip:siphon@<ip>:5060;transport=tls>`) regardless of which listener received
  the INVITE. A secure trunk (e.g. Twilio over TLS) honoured that Contact and dialed
  TLS to the UDP port, where nothing listens — so the caller's ACK and BYE never
  arrived and the call only ended when the RTP inactivity watchdog fired ~60 s later.
  Symptoms: call recordings padded with a ~60 s silent tail, CDR `cause = tap_ended`
  instead of a clean hangup, and `outbound BYE failed` warnings. Fixed upstream in
  siphon-rs (the auto-filled Contact port now follows the listener that received the
  request); this release threads the receiving listener's local address through the
  packet pump (`TransportContext::with_local_addr`) and bumps the siphon-rs pin.
  UDP-only deployments were never affected and their Contacts are unchanged.

- **SIPp suite portability to dual-stack hosts** (`test-harness/
  sipp-scenarios/run-all.sh`): sipp invocations now pin `-i 127.0.0.1`.
  Without it, sipp's `[local_ip]` can expand to `::1`, so UAS scenarios
  advertise an IPv6 Contact the IPv4-bound daemon can't reach — the
  in-dialog BYE fails with a transport error and the outbound /
  attended-transfer phases hang. The blind-transfer phase also gains
  the same venv-then-system-python3 fallback the other phases already
  had, instead of hard-requiring the CI-prepped venv. Harness-only;
  no daemon changes.

## [0.6.1] - 2026-06-10

Theme: **attended transfer** — the 0.6.0 fast-follow. The bot consults a
human before handing the caller off: SiphonAI places the consult leg as a
plain 0.6.0 outbound call (its own WS session), and completion is one
REFER-with-Replaces on the original call. The WS protocol stays at
`version: "1"` (one additive field) and the CDR schema is unchanged.

### Added

- **Attended transfer** — `transfer.replaces_call_id` names an answered
  outbound call (the consult leg, placed via `POST /admin/v1/calls` and
  identified by the `call_id` that endpoint returned). SiphonAI sends a
  REFER whose `Refer-To` embeds a `Replaces` built from the consult
  dialog, so the transferee connects directly to the consulted party
  (RFC 5589 §7). `target` becomes optional — the default Refer-To is the
  consult dialog's remote target (its 200 OK Contact); send `target` only
  to override the reachable URI. The consult leg is **not** torn down at
  REFER time (the transferee's INVITE-with-Replaces takes it over); to
  cancel a consultation, just hang up the consult call. Unknown / not-yet-
  answered / already-ended `replaces_call_id` → `error
  { code: "transfer_failed" }` and the call continues. `docs/PROTOCOL.md`
  §4.4.
- **Outbound legs are transferable** (blind or attended) — an outbound
  bot can hand its callee off the same way. The REFER goes out through
  the gateway's own UAC, so its digest credentials answer any 401/407
  challenge on the REFER.
- **Metric** — `siphon_ai_transfers_total{mode="blind"|"attended",
  result="accepted"|"rejected"|"local_error"}`; also back-fills blind
  transfers, which were previously unmetered.
- **SIPp coverage** — `attended_transfer_a.xml` + an always-on
  three-party regression phase (SIPp on both far ends: inbound transferee
  + consult callee; pass requires the REFER's `Refer-To` to carry
  `Replaces=` *and* the metric reading attended/accepted), driven by a
  new `--auto-transfer-replaces` test-harness knob on the echo WS
  example server.

### Fixed

- **Duplicate BYE after an accepted transfer** on inbound legs: the
  transfer task sends the post-REFER BYE ("REFER + BYE", RFC 5589 §6.1),
  but the acceptor's cleanup task then sent a *second* BYE from a fresh
  CSeq space — a protocol violation that strict peers reject. Affected
  blind transfer too (latent since 0.2.0; exposed by the new attended
  SIPp scenario's stricter tail).

## [0.6.0] - 2026-06-09

Theme: **outbound call origination.** SiphonAI inverts its inbound-only
model — `POST /admin/v1/calls` places a SIP call through a configured
gateway and bridges the answered call to a WS server over the same
protocol v1 session inbound calls use. **Off by default** (fail-closed on
`[outbound].max_concurrent = 0`) — a 0.5.0 deployment upgrades with zero
behaviour change. The WS protocol stays at `version: "1"` (the new
`start.direction` field is additive) and the CDR schema stays at version 1
(`direction` was reserved for outbound since v1).

### Added

- **Outbound origination** — `[outbound]` (`max_concurrent`,
  `rate_limit_per_sec`) + `[[gateway]]` blocks: standalone trunks
  (`proxy` / `from` / optional digest `auth_username` + `auth_password`)
  or `register = "<name>"` to dial through an existing `[[register]]`,
  inheriting its server, credentials, and AOR. Validated at config load.
  See `docs/OUTBOUND.md`.
- **Originate API** — `POST /admin/v1/calls` `{to, gateway, ws_url?,
  from?}` → `202 {call_id}`. **No built-in auth by design** (reverse-proxy
  posture, plan §9.5): bind the admin API private and front it yourself.
  The cap + rate limit are the native toll-fraud guardrails; the
  `503`/`429` rejections are fail-closed.
- **WS protocol** — `start.direction: "inbound" | "outbound"` (additive;
  servers that ignore it keep working). Outbound sessions start at answer
  and carry the dialed `to` and the caller-ID `from`.
- **Call-progress webhooks** — `outbound_initiated` `{to, gateway}`,
  `outbound_answered` `{sip_call_id}`, terminal `outbound_failed`
  `{cause}`; answered calls finish with the existing `call_end`. `cause`
  mirrors the metric's `result` labels.
- **CDR** — `direction: "outbound"` for answered originated calls;
  `route` carries the gateway name. Unanswered calls get no CDR (webhook +
  metric cover them), mirroring inbound where CDRs cover bridged calls.
- **Metrics** — `siphon_ai_outbound_calls_total{result="answered"|"busy"|
  "declined"|"no_answer"|"rejected"|"unreachable"|"failed"}` and the
  `siphon_ai_outbound_calls_active` gauge.
- **SIPp coverage** — `outbound_uas_answer.xml` + an always-on roles-
  inverted regression phase (SIPp answers SiphonAI's INVITE; pass requires
  the full INVITE → ACK → BYE flow *and* the answered-counter reading 1),
  driven by a new `--auto-hangup-after-ms` test-harness knob on the echo
  WS example server.
- **`docs/OUTBOUND.md`** — the outbound guide (enabling, originate API,
  the toll-fraud security posture, lifecycle, observability, testing
  without spending money, limitations).

### Notes

- Outbound calls **spend money**. The security model is deliberate:
  no native API auth, so the documented posture (private bind +
  authenticating reverse proxy + `max_concurrent` + `rate_limit_per_sec`
  + trunk-side destination allowlists) is mandatory reading —
  `docs/OUTBOUND.md` §3.
- Not in 0.6.0: early media (WS session starts at answer), attended
  transfer (the 0.6.1 fast-follow), outbound recording, outbound
  STIR/SHAKEN signing, built-in AMD (the WS server's job, by design).

## [0.5.0] - 2026-06-08

Theme: **call recording.** Each call's audio can be captured to a stereo WAV
(caller on the left channel, bot/WS on the right) for compliance and QA.
**Off by default** — a 0.4.x deployment upgrades with zero behaviour change
until `[recording].mode` is set. The WS protocol stays at `version: "1"`
(the new recording messages are additive) and the CDR schema stays at
version 1 (the new fields are additive optionals).

### Added

- **Call recording** (`[recording]`) — writes `<dir>/<call_id>.wav`, stereo
  PCM16 at the call's negotiated rate. `mode = "off"` (default) / `"always"`
  (whole call) / `"on_demand"` (WS-server-driven). The recorder runs off the
  audio hot path (CLAUDE.md §4.3): the media tap only does a non-blocking
  copy onto a bounded channel, and a per-call writer task does the file I/O —
  a backed-up writer drops frames (flagged `degraded`) rather than ever
  stalling or gapping the live call. See `docs/RECORDING.md`.
- **Per-route override** — `[route.recording].mode` strictly overrides the
  global mode for matched calls (mirrors `[route.security]`). The output
  `dir` is the global one, so `[recording].dir` is required (and created at
  load) whenever any route enables recording, even with the global mode
  `off`.
- **On-demand control (WS protocol).** New `BridgeIn`: `start_recording` /
  `stop_recording` / `pause_recording` / `resume_recording`. New
  `BridgeOut`: `recording_started` / `recording_stopped` /
  `recording_failed` (each with `recording_id`). `pause_recording` **omits**
  the paused span from the file (dropped, not silenced) — the PCI
  "pause while the caller reads a card number" primitive. PROTOCOL.md §3.11 /
  §4.7.
- **CDR pointer** — `recording_id` / `recording_path` on the CDR (additive
  optionals, omitted when the call wasn't recorded → schema stays at v1).
- **Metric** — `siphon_ai_recordings_total{result="ok"|"degraded"|"failed"}`.
- **`docs/RECORDING.md`** — the recording guide (enabling, output format,
  on-demand control, observability, the hot-path/degraded story, disk
  sizing, retention, consent, and limitations), plus an always-on recording
  phase in the SIPp regression suite that asserts a valid stereo WAV.

### Notes

- Recordings are written **decrypted** — even for SRTP-encrypted calls, the
  WAV on disk is plaintext PCM (forge decrypts the media to bridge it; the
  recorder taps the decoded audio). The recording directory is sensitive
  data; protect it at rest (disk encryption, permissions) and manage
  retention yourself — the daemon never deletes recordings. Consent and any
  "this call is recorded" announcement are the operator's responsibility.
- **SRTP re-key on a timer** was planned to ride along but was **deferred**:
  forge-media has no coordinated mid-call re-key (DTLS renegotiation is
  blocked; a unilateral key swap would break media), so it needs upstream
  work first. See `docs/design/DEV_PLAN_0.5.0.md` §3.2 / §6.

## [0.4.1] - 2026-06-07

Completes the 0.4.0 STIR/SHAKEN theme — the four items deferred from that
release, plus the small feature that makes the passing path testable. Still
**off by default**; protocol stays at `version: "1"` (the one new `verstat`
field is additive).

### Added

- **PASSporT `iat` freshness check (replay protection).** With verification
  enabled, a PASSporT whose `iat` is outside `[security.stir_shaken]
  .iat_freshness_secs` of now (past **or** future), or missing, now fails —
  surfaced as the new `verstat.iat_passed` boolean and folded into the
  composite pass. Default window 60 s (ATIS-1000074); `0` disables the check
  for upstreams with broken clocks.
- **`[security.stir_shaken].x5u_tls_extra_ca`** — optional supplemental CA
  bundle trusted **for the `x5u` HTTPS fetch only** (added to the public
  web-PKI roots), for operators hosting `x5u` behind a private/lab CA.
  Validated at load when enabled. Does not affect the SHAKEN chain, which
  always validates against `trust_anchors`.
- **`docs/SECURITY_STIR_SHAKEN.md`** — the STIR/SHAKEN security model:
  attestation is a signal not a verdict, the two trust domains, the
  `verstat` trust rule, replay/freshness, observe-first gate rollout,
  monitoring, and limitations.
- **Twilio Caller Identity cross-check recipe** — a `docs/INTEGRATIONS_TWILIO.md`
  section and a runnable `examples/verstat-compare-py` server that compares
  SiphonAI's independent `verstat` against Twilio's `X-Twilio-VerStat`
  header (forwarded via `[bridge].forward_headers`), logging AGREE/DIVERGE.
- **Passing-attestation SIPp regression** (`stir_shaken_attestation_pass.xml`)
  plus the `gen_test_passport` example (a `siphon-ai-stir-shaken` example
  that mints a CA + leaf + x5u TLS cert + fresh signed PASSporT, doubling as
  an operator lab tool). The first *green* verstat path under CI — a
  fully-verifiable call is admitted, alongside the 0.4.0 428/403 rejects.

### Changed

- **`verstat.iat_passed` is part of the composite `passed()`** — a
  deployment that already opted into `stir_shaken` will now reject a
  previously-passing call that carries a stale `iat`. This is the
  spec-correct outcome; tune or disable it via `iat_freshness_secs`.

## [0.4.0] - 2026-06-07

Theme: **STIR/SHAKEN call authentication.** Inbound INVITEs carrying an
RFC 8224 `Identity` header are now verified end-to-end — PASSporT decode
(RFC 8225), ES256 signature, X.509 chain validation to a configured STI-PA
trust anchor (via the `x5u` certificate, fetched and TTL-cached), and the
SHAKEN `orig`/`dest` ↔ SIP `From`/`To` claim checks — yielding a per-call
*verstat* verdict. Operators can gate on it (`min_attestation` 4xx,
`require_identity` 428, with per-route overrides), and the verdict is
surfaced everywhere observability already reaches: the WS `start` message,
the CDR, a structured log line, and a new HEP3 chunk for Homer.

Everything is **off by default** — a 0.3.x deployment upgrades with zero
behaviour change until `[security.stir_shaken].enabled = true`. Protocol
stays at `version: "1"`: `start.verstat` is an additive optional field, so
v1 WS servers built against earlier releases keep working unchanged. The
cryptographic core lives in two new building blocks — siphon-rs's
`sip-identity` crate (parsing + ES256 + chain validation) and this repo's
`siphon-ai-stir-shaken` crate (the `x5u` fetch, cert cache, and verdict
orchestration the stack crate deliberately leaves to the application).

### Added

- **`siphon-ai-security` crate — the verstat vocabulary.** `AttestationLevel`
  (SHAKEN A/B/C with an explicit trust rank), `VerificationResult` (the
  verdict, with a `trusted_attestation()` accessor that only trusts a claim
  when verification fully passed), and the `MinAttestation` policy gate
  (strict per-route `resolve` + the §4 `permits` matrix). Dependency-light
  so every layer can depend on it cheaply.

- **`[security]` / `[security.stir_shaken]` config surface.** `enabled`,
  `trust_anchors` (PEM bundle path, validated at load), `cert_cache_ttl_secs`
  (default 1 h), `require_identity`, plus the gate knobs `min_attestation`
  (`none`/`A`/`B`/`C`) and `min_attestation_response` (403/488/606). Fully
  inert by default; misconfiguration fails loud at startup. See
  [`docs/CONFIG.md`](docs/CONFIG.md).

- **`siphon-ai-stir-shaken` crate — the verifier service.** The
  application-layer half of verification: HTTPS `x5u` certificate fetch
  (https-only, redirect-free, size/time-capped), a process-wide TTL cert
  cache keyed by URL, trust-anchor loading, and verdict orchestration
  (`Verifier::verify(identity, from, to) → VerificationResult`). The
  cryptographic core (ES256 + X.509 chain validation) is siphon-rs
  `sip-identity`; this crate wires it to the network and the cache.

- **Accept-path verification + the verstat surface.** Each inbound INVITE
  is verified before route/media bring-up; the verdict rides
  `BridgeOut::Start` as the optional `verstat` object and lands on the CDR
  as `verstat_attest` / `verstat_passed` (additive — CDR schema stays at
  version 1; emitted only when verification is enabled). One `info!` line
  per call carries the verstat fields. See [`docs/PROTOCOL.md`](docs/PROTOCOL.md).

- **Attestation policy gate.** After verification, before route matching,
  the daemon can reject calls that don't meet policy — `require_identity`
  → **428 Use Identity Header** (RFC 8224 §6.2.2) for an INVITE with no
  `Identity` header, and a `min_attestation` floor → the configured
  **403/488/606** with a `Reason: Q.850;cause=21` header. The gate runs
  before media is allocated, so a rejected call never opens an RTP port or
  WS bridge. Per-route override via `[route.security].min_attestation`
  (strict override, like `[route.media].srtp`). See
  [`docs/CONFIG.md`](docs/CONFIG.md) and [`docs/DIALPLAN.md`](docs/DIALPLAN.md).

- **HEP3 verstat chunk for Homer.** When HEP is enabled, the verdict ships
  as a `HepProtocol::Verstat` (chunk type `0x66`) packet correlated by SIP
  `Call-ID`, threading onto the same call view as the SIP / RTCP / CDR
  chunks. JSON payload, same shape as `start.verstat`. Requires the
  upstream `hep-rs` `Verstat = 102` protocol type
  ([thevoiceguy/hep-rs#1](https://github.com/thevoiceguy/hep-rs/pull/1)).
  See [`docs/HEP.md`](docs/HEP.md).

- **New metric `siphon_ai_verstat_total{result=passed|failed|unsigned}`** and
  a `rejected_attestation` label on `siphon_ai_invites_total` so
  STIR/SHAKEN policy rejections are separately alertable from ordinary
  routing/media rejects. See [`docs/DEPLOY.md`](docs/DEPLOY.md).

- **`contrib/sti-pa-roots.pem` trust-anchor template + `contrib/README.md`.**
  A documented placeholder (not a baked-in root — a stale or wrong trust
  anchor is a security defect): the operator populates it with the
  authentic STI-PA root(s), verified by fingerprint. Using it unpopulated
  fails loud at startup by design.

- **STIR/SHAKEN SIPp regressions.** `stir_shaken_no_identity_428.xml` and
  `stir_shaken_attestation_403.xml` exercise the accept-path gate end-to-end
  over real SIP (both reject before media), run in a new always-on
  `stir_shaken` phase of the regression suite.

### Changed

- **`siphon_ai_rtp_rtt_ms` now renders as a bucketed Prometheus histogram instead of a summary.** The metric had no explicit buckets registered, so `metrics-exporter-prometheus` fell back to a summary (quantiles) — inconsistent with the other `_seconds` histograms and awkward to aggregate across instances. It now has explicit ms buckets (10 ms–1 s) via `set_buckets_for_metric`, matching the 0.3.0 housekeeping rule ("histograms get sensible buckets defined explicitly"). `/metrics` now emits `siphon_ai_rtp_rtt_ms_bucket{le="…"}` lines; anything scraping the old `{quantile="…"}` series should switch to `histogram_quantile()` over the buckets.

## [0.3.2] - 2026-06-05

Closes the last open 0.3.0 Definition-of-Done item: `rtcp_rtt_ms` now
populates on live calls.

### Fixed

- **`rtp_stats.rtcp_rtt_ms` is now populated instead of always `null`** — picked up via a forge-media bump (`5c30c03e17f4` → `e95a31a959a6`, [thevoiceguy/forge-media#69](https://github.com/thevoiceguy/forge-media/pull/69)). The `rtcp_rtt_ms` field has shipped since 0.3.0 but always emitted `null`: forge-engine's terminator mode generates an RTP stream toward the carrier (its own SSRC) yet never originated RTCP **Sender Reports** for it, so the carrier's Receiver Reports came back with `last_sr = 0` and the `RttTracker` (RFC 3550 §A.7) had nothing to match against. 0.3.0 plan §9 decision 10 deferred the SR emitter as a follow-up; this is it. forge-engine now sends an SR per generated stream every 5 s (RFC 3550 §6.2 minimum), SRTCP-protected, and resolves the echoed `last_sr`/`delay_since_last_sr` from incoming RRs into the RTT sample carried on `RtcpReportReceived`. SiphonAI already consumed the field (`media-glue` populates `rtcp_rtt_ms` on the `rtp_stats` WS event and records the `siphon_ai_rtp_rtt_ms` histogram), so no SiphonAI-side code change — the value simply starts flowing. Expect a sample on each RR (~every 5 s) once both directions of RTCP are live.

## [0.3.1] - 2026-06-05

Carrier-interop hardening for the 0.3.0 encryption stack. 0.3.0 shipped
TLS, mTLS, and DTLS-SRTP, but its SRTP coverage was self-paired — so a
cluster of spec-conformance bugs stayed invisible until a spec-correct
carrier (Twilio Secure Trunking) was on the wire: AES-CM IV byte offsets,
SRTCP KDF labels, RTCP SR/RR report-count parsing, and an always-set RTP
marker bit — all fixed here via forge-media bumps. It also brings forward
the 0.3.0 §6 carry-forward — SDES SRTP outbound (`RTP/SAVP`) — to unblock
carriers whose all-or-nothing "Secure Trunking" toggle mandates TLS
signaling and SRTP together. Rounded out with RFC 3261 §12 / §13 / §20
response polish and journald/observability fixes.

Note: 0.3.0 was prepared (version bump + changelog) but never tagged; its
encryption features ship to users for the first time here, hardened.

Protocol stays at `version: "1"` — every addition is additive, so v1 WS
servers built against 0.1.0 / 0.2.0 keep working unchanged.

### Fixed

- **SRTP audio now decrypts cleanly against spec-correct peers** — picked up via a forge-media bump (`48ff87be0a85` → `33443589ce2e`, [thevoiceguy/forge-media#67](https://github.com/thevoiceguy/forge-media/pull/67)). The four AES-CM IV construction sites in `forge-rtp` placed the packet index in the wrong bytes of the 128-bit IV (RTP 48-bit index at `iv[6..12]` instead of `iv[8..14]`; SRTCP 32-bit index at `iv[8..12]` instead of `iv[10..14]`, both per RFC 3711 §4.1.1 / §4.1.2). Symmetric protect/unprotect round-trip tests passed because both ends used the same wrong offsets and AES-CTR cancelled — bug stayed invisible until a spec-correct peer (Twilio Secure Trunking) was on the wire. Concrete production symptom on the first SDES SRTP Twilio call: caller heard white noise instead of the bot's greeting (our outbound was unrecoverable garbage to Twilio), and the bot's STT received PCMU-shaped bytes that didn't decode to recognisable speech (Twilio's inbound was unrecoverable garbage to us, so no LLM turn ever fired). DTLS-SRTP runs through the same code path; existing DTLS callers were silently affected the same way against any spec-correct peer — the 0.3.0 DTLS-SRTP coverage was self-paired and didn't surface it. No SiphonAI-side code change.

- **SRTCP packets from spec-correct peers now authenticate successfully** — picked up via a forge-media bump (`f599ebd6cd39` → `48ff87be0a85`, [thevoiceguy/forge-media#66](https://github.com/thevoiceguy/forge-media/pull/66)). `forge-rtp`'s `derive_session_keys` always derived with the SRTP labels (`0x00` / `0x01` / `0x02`) regardless of which protocol was calling it; SRTCP requires labels `0x03` / `0x04` / `0x05` per RFC 3711 §4.3.3. Result was that every SRTCP packet from Twilio / FreeSWITCH / any spec-correct peer was discarded with "SRTCP authentication failed" — visible in the journal every ~5 s (the RTCP send interval). Surfaced immediately once SDES SRTP shipped on the siphon-ai side and real carrier RTCP started landing; DTLS-SRTP 0.3.0 coverage was hand-driven and audio-focused, so SRTCP didn't get exercised end-to-end. SRTP path is unchanged. No SiphonAI-side code change.

- **Outbound RTP no longer sets the marker bit on every packet** — picked up via a forge-media bump (`33443589ce2e` → `5c30c03e17f4`, [thevoiceguy/forge-media#68](https://github.com/thevoiceguy/forge-media/pull/68)). `forge-engine`'s playout scheduler set the RTP marker on the first frame of each *append call*, but SiphonAI streams one 20 ms frame per call — so every outbound packet carried `M=1` instead of only the first packet of each talkspurt (RFC 3551 §4.1). Confirmed against Twilio Secure Trunking: 100 % of outbound packets were marked, while Twilio's inbound correctly marked only talkspurt starts. Not audible (the static was the separate AES-CM IV bug above), but an interop wart — stricter jitter buffers can treat every marked packet as a fresh talkspurt and needlessly re-adjust playout. The fix keys the marker off a persistent wall-clock talkspurt detector (audio resuming after a >60 ms silence gap, or a barge-in `Replace`); verified on the wire post-deploy as 2 of 317 outbound packets marked, both at talkspurt starts. No SiphonAI-side code change.

### Added

- **SDES SRTP outbound — inbound `RTP/SAVP` offers now negotiate end-to-end** (the 0.3.0 plan §6 carry-forward, brought forward to unblock production deployments where the carrier ships an all-or-nothing "Secure Trunking" toggle that requires TLS signaling AND SRTP — most notably Twilio Elastic SIP Trunk). When `[media].srtp = "preferred"` or `"required"` and the offer's audio m-line is `RTP/SAVP` (or `RTP/SAVPF` without TLS), the daemon now:
  1. Parses the offer's `a=crypto:` attributes via `forge_sdp::sdes`,
  2. Selects the strongest mutually-supported crypto suite (default preference `AES_CM_128_HMAC_SHA1_80`),
  3. Calls `forge_sdp::sdes::answer_sdes()` to derive the inbound and outbound SRTP master keys plus a freshly-generated local `a=crypto:` line,
  4. Patches the SDP answer with `RTP/SAVP` profile + the local crypto attribute,
  5. Installs the derived keys into the per-call `SrtpContext` via the new `forge_engine::srtp_install::install_srtp_keys` (forge-media PR #65), at which point the ordinary `protect_rtp` / `unprotect_rtp` path takes over.

  `start.srtp` on the WS protocol populates as `{exchange: "sdes", profile: "AES_CM_128_HMAC_SHA1_80"}` so the bridge server knows the call is SDES-protected (distinct from the existing `exchange: "dtls"` value for the DTLS-SRTP path).

  Policy matrix is now complete:

  | Mode | Plain RTP | DTLS-SRTP | SDES |
  |---|---|---|---|
  | `off` | ✓ | 488 | 488 |
  | `preferred` | ✓ | ✓ | ✓ |
  | `required` | 488 | ✓ | ✓ |

  Malformed SDES offers (no `a=crypto:`, no acceptable crypto suite, malformed inline key) surface as 488 the same way DTLS-SRTP fingerprint-mismatches do — no silent downgrade. Seven new unit tests cover the negotiation, profile rewrite, missing-crypto rejection, and SAVP-vs-SAVPF disambiguation against the existing DTLS-SRTP helper.

### Fixed

- **Log output no longer emits ANSI colour escape sequences when stdout isn't a terminal.** `bins/siphon-ai/src/main.rs` builds the tracing subscriber from a hand-composed `fmt::layer()` rather than the higher-level `fmt::Subscriber::builder()` (to get a reload handle for the EnvFilter). The layer form defaults to ANSI on regardless of tty status — so every line under systemd was landing in journald with embedded `\x1b[..m` sequences. Harmless to human readers (journalctl strips them on display), but it silently broke every downstream consumer that does string matching against the journal — most notably the fail2ban `<HOST>` extractor in our trunk-rejection filter, which saw `peer=\x1b[3m185.9.19.90:61792\x1b[0m` and never matched. The fmt layer now calls `.with_ansi(std::io::IsTerminal::is_terminal(&std::io::stdout()))` so ANSI is enabled for interactive `cargo run` but disabled under systemd. After upgrading, restart fail2ban (`sudo systemctl restart fail2ban`) so its journal cursor advances past the polluted entries; subsequent 403s will match the filter.

- **RTP QoS metrics are no longer garbage for any real SIP peer** — picked up via a forge-media bump (`f7cd7f074d7c` → `47cf68aa0f0a`, [thevoiceguy/forge-media#63](https://github.com/thevoiceguy/forge-media/pull/63)). `forge-rtp`'s SR/RR parsers were ignoring the `RC` field in the RTCP common header and greedily consuming 24-byte chunks until the input buffer ran out — treating the trailing SDES bytes of every compound RTCP packet (which RFC 3550 §6.1 makes mandatory) as phantom reception report blocks. The wrong bytes landed in `jitter`, `cumulative_lost`, `last_sr`, etc., silently corrupting every downstream metric and `RtpStats` WS event. Observed pre-fix: `siphon_ai_rtp_jitter_ms` averaged ~113 M ms per RR against a real Twilio inbound (formula was decoding ASCII SDES CNAME bytes as the jitter field). Post-fix: `jitter_ms` / `packet_loss_ratio` / `rtt_ms` reflect actual call quality; the `rtp_stats` WS events your bot can use for adaptive logic are now trustworthy. No SiphonAI-side code change; the fix is entirely in `forge-rtp::rtcp::{SenderReport,ReceiverReport}::parse`, which now take an explicit `report_count: u8` argument wired through from the RTCP header.

- **Responses now emit `Server:` instead of `User-Agent:`, advertise `Allow:` on 2xx to INVITE, and omit empty `Supported:` on OPTIONS 200 OK** — picked up via a siphon-rs bump (`47cd5d39c7d6` → `a4f8521561d6`, [thevoiceguy/siphon-rs#52](https://github.com/thevoiceguy/siphon-rs/pull/52)). Three independent RFC 3261 §13/§20 polish items: (1) §20.41 / §20.50 — responses identify the UAS via `Server:`, requests use `User-Agent:` (we were emitting the latter on responses; carriers tolerated it but it confused header-name-strict SIP analysers); (2) §13.2.1 — 2xx to INVITE SHOULD advertise the methods the UAS supports so the peer learns what mid-dialog requests (re-INVITE / UPDATE / REFER / INFO) are legal without an OPTIONS probe; (3) §20.37 — an empty `Supported:` value implies nothing useful and some peers treat the blank as a parse oddity. No SiphonAI-side code change.

- **`200 OK` to INVITE now carries the request's `Record-Route` headers** — picked up via a siphon-rs bump (`d0d3691244de` → `47cd5d39c7d6`, [thevoiceguy/siphon-rs#51](https://github.com/thevoiceguy/siphon-rs/pull/51)). The UAS response builder previously dropped every `Record-Route` from the INVITE, in violation of RFC 3261 §12.1.1. Subsequent in-dialog requests (ACK / BYE / re-INVITE / REFER) routed straight to the UAS's `Contact` instead of traversing the proxy chain — silent under carriers like Twilio (whose edge tolerates direct-to-Contact in-dialog routing), but a latent dialog-killer behind stricter SBCs or multi-proxy topologies. No SiphonAI-side code change; the fix is entirely in the upstream UAS builder.

## [0.3.0] - 2026-05-26

Third release. Theme: **trust and encryption** — every transport
the daemon touches can now run encrypted. SIP/TLS gets hot cert
reload (no in-flight call drops on renewal). The WebSocket bridge
gets mTLS with optional SPKI cert pinning. Inbound calls offering
DTLS-SRTP get a SAVPF answer end-to-end (forge handles the
handshake, derives SRTP keys, decrypts media). RTP-quality events
(`jitter_ms`, `packet_loss_ratio`, and an `rtcp_rtt_ms` field
reserved for 0.3.1) now actually populate.

Protocol stays at `version: "1"` — every new variant is additive,
so v1 WS servers built against 0.1.0 / 0.2.0 keep working
unchanged. The wire-shape additions land *behind* the new config
defaults: out of the box, 0.3.0 behaves like 0.2.0.

### Added

#### Encryption

- **DTLS-SRTP for inbound calls** (PROTOCOL §3.1 `start.srtp`,
  DEV_PLAN_0.3.0.md §4.1). When the offer's audio m-line is
  `UDP/TLS/RTP/SAVPF` and `[media].srtp` is `"preferred"` or
  `"required"`, the daemon:
  1. extracts the remote `a=fingerprint:` + `a=setup:` from the
     offer,
  2. answers `UDP/TLS/RTP/SAVPF` with our own SHA-256 fingerprint
     and `a=setup:passive` (RFC 5763 §5),
  3. provisions the DTLS leg on the per-call `MediaSession`,
     forge-engine's recv loop demuxes the inbound DTLS handshake
     (RFC 5764 §5.1.2 first-byte demux),
  4. on handshake completion, the derived SRTP master keys
     install into the existing `SrtpContext` and subsequent SRTP
     packets decode through the ordinary unprotect path.

  `start.srtp` is populated with `{exchange: "dtls", profile:
  "AES_CM_128_HMAC_SHA1_80"}` — the profile is best-guess
  pre-handshake (RFC 5764 mandates that suite as baseline; the
  actual negotiation may select a stronger AES-GCM suite).

  Long-lived per-process DTLS cert generated at daemon startup
  (rcgen). Same cert presented to every DTLS handshake; rotation
  is via daemon restart (or `systemctl reload` on the SIP/TLS
  side — DTLS-SRTP cert rotation is intentionally NOT exposed,
  since rotating it mid-call would invalidate in-flight handshakes).

  SDES (`RTP/SAVP` / `RTP/SAVPF`) offers are rejected with 488 —
  forge-sdp ships the `a=crypto:` parser but the forge-engine
  producer wiring isn't done. 0.3.1.

- **`[media].srtp` config + policy gate**. New
  `[media].srtp = "off" | "preferred" | "required"` (default
  `"off"`, matches 0.2.0). Per-route override via
  `[route.media].srtp`. The policy matrix is enforced before any
  media bring-up — incompatible offers fail fast with 488:

  | Mode | Plaintext (`RTP/AVP`) | DTLS-SRTP | SDES |
  |---|---|---|---|
  | `off` | ✓ | 488 | 488 |
  | `preferred` | ✓ | ✓ | 488 |
  | `required` | 488 | ✓ | 488 |

  Resolution via `resolve_srtp_mode(defaults, route)` mirrors the
  other `resolve_*` helpers; unknown route-level values warn and
  fall back to defaults.

- **mTLS for the bridge WebSocket leg** (`[bridge.tls]` block,
  DEV_PLAN_0.3.0.md §4.2 Part A, `docs/DEPLOY.md` §3a). New
  config:

  ```toml
  [bridge.tls]
  client_cert    = "/etc/siphon-ai/bridge/client.pem"
  client_key     = "/etc/siphon-ai/bridge/client.key"
  pinned_sha256  = "..."   # optional 64-hex-char SPKI SHA-256
  ```

  Builds a custom `rustls::ClientConfig` and hands it to
  `tokio-tungstenite`'s `Connector::Rustls`. The optional SPKI
  pin (SHA-256 of the server's `SubjectPublicKeyInfo` per
  RFC 7469 §3) replaces default CA verification with exact-match,
  appropriate for carrier-pinned PBX deployments. Cert / key /
  pin validation happens at config compile so issues surface at
  daemon startup, not at first call.

- **Outbound TLS UAC for REGISTER** (DEV_PLAN_0.3.0.md §4.5,
  `docs/REGISTRATION.md` "TLS registration"). `transport = "tls"`
  on a `[[register]]` block now actually goes out over TLS — no
  silent fallback to UDP. Uses the daemon-wide webpki trust
  store (Mozilla CA bundle). Twilio Elastic SIP Trunk recipe in
  `REGISTRATION.md`. The stale "Inbound UAS only" disclaimer in
  `CONFIG.md` is removed.

- **SIGHUP hot cert reload for SIP/TLS** (DEV_PLAN_0.3.0.md
  §4.3). `systemctl reload siphon-ai` rotates `[sip.tls].cert` +
  `.key` without dropping in-flight TLS sessions. In-flight
  dialogs keep using the cert they handshook with
  (RFC 5746-compliant); new connections pick up the fresh cert.
  Broken PEM on reload doesn't kill the daemon — `error!`
  logged, previous cert keeps serving. New metric
  `siphon_ai_sip_tls_reload_attempts_total{outcome}`. systemd
  `ExecReload=/bin/kill -HUP $MAINPID`. Builds on siphon-rs's
  `run_tls_with_swappable_config` (#49).

#### Observability

- **`rtp_stats` event fields populate** (PROTOCOL §3.8,
  DEV_PLAN_0.3.0.md §4.4). `jitter_ms` and `packet_loss_ratio`
  are now driven by a new `ForgeEvent::RtcpReportReceived` event
  forge-engine emits on every received RR (forge-media#57 +
  #60). Closes the pre-existing 0.2.0 gap where both fields were
  always `null`. New `siphon_ai_rtp_rtt_ms` histogram alongside
  the existing jitter / loss histograms.

- **`rtcp_rtt_ms` field reserved + sticky semantics** in
  PROTOCOL §3.8. The field is documented and the wire shape is
  pinned, but stays `null` in 0.3.0 — populating it needs
  forge-engine to originate its own RTCP SRs (the
  `forge_rtp::RttTracker` primitive is ready and tested in
  forge-media#57). When a real value does arrive in a future
  release, it'll be "sticky": once populated, a later window
  with no fresh RR doesn't wipe it.

### Changed

- **`forge-media` rev pinned to `f7cd7f0`**, picking up DTLS-SRTP
  scaffolding (#61), recv-loop demux (#62), RtcpReportReceived
  event + emitter (#57 + #60), SDES primitives (#56), tarpaulin
  coverage fix (#59).

- **`siphon-rs` rev pinned to `d0d3691`**, picking up swappable
  TLS `ServerConfig` (#49) and CI-on-PR gating (#50).

- **`[sip.tls]` callout in `docs/CONFIG.md`** — old "Inbound UAS
  only" warning replaced with a precise statement: inbound UAS
  still terminates TLS here; outbound TLS works for
  `[[register]]` as of 0.3.0; originated INVITEs are still
  post-v1.

### Fixed

- **forge-rtp DTLS verify-callback** (forge-media#61). The
  existing `DtlsContext::new` installed OpenSSL's default
  chain-verify mode, which fails closed on self-signed certs —
  which is what every DTLS-SRTP peer presents (RFC 5763 §5).
  Replaced with a `set_verify_callback` that accepts any chain;
  fingerprint verification runs post-handshake as before. Makes
  the entire DTLS path actually usable for the first time.

- **forge-media Code Coverage** (forge-media#59). Tarpaulin
  failures on every PR since 2026-05-11 fixed: one missing
  feature gate (`test_codec_config_stored` needed
  `#[cfg(feature = "opus")]`) + one timing-tight assertion in
  `test_jitter_buffer_timing` that fell over under ptrace
  instrumentation. Three pre-existing dead-code `opus` tests in
  `forge-api` now actually run thanks to a new
  `forge-api/opus` feature.

### Known limitations (0.3.1 carry-forwards)

These are documented in `DEV_PLAN_0.3.0.md` §11 slip-mitigation,
`PROTOCOL.md`, and `REGISTRATION.md`:

- **`rtcp_rtt_ms` not populated end-to-end.** The field is
  reserved and the consumer wiring works, but forge-engine
  doesn't yet originate its own RTCP SRs. The `RttTracker`
  primitive is ready upstream; what's missing is the periodic
  SR send loop with RFC 3550 §6.2 bandwidth budget tracking.

- **SDES (`RTP/SAVP`) not produced.** forge-sdp ships the
  `a=crypto:` parser (forge-media#56); forge-engine doesn't
  consume it yet. SAVP / non-DTLS SAVPF offers are 488'd under
  any `srtp_mode`.

- **Per-route `[route.bridge.tls]` override.** mTLS for the
  bridge is global only in 0.3.0; every accepted call shares
  the same client cert.

- **Hostname `[[register]].server`.** Static-IP validation in
  `compile_registers` still rejects hostnames; lifting it needs
  a `RegisterConfig.server_addr: SocketAddr` refactor.

- **Per-registration cert pinning** (`[[register]].tls.pinned_sha256`).
  siphon-rs's UAC takes a daemon-wide TLS client config and
  doesn't yet expose a per-target `ClientConfig` API.

- **Attended transfer (REFER with Replaces)** carried over from
  0.2.0 — depends on a siphon-rs UAC capability that's still
  pending.

### Stats

- 8 PRs merged on siphon-ai for 0.3.0: #83, #85, #86, #87, #88,
  #89, #90, #91, #92.
- 6 upstream PRs merged on forge-media: #56, #57, #59, #60, #61,
  #62.
- 2 upstream PRs merged on siphon-rs: #49, #50.
- Workspace test count: 429 → 466 (+37 new tests across the
  sprint; every PR landed with `fmt --check` + `clippy
  --workspace --all-targets -- -D warnings` clean).

## [0.2.0] - 2026-05-25

Second release. Theme: **operator primitives** — the WS server can
now react to silence and dead-air with built-in events instead of
running its own VAD timers, observe RTP quality without scraping
RTCP, mute the AI's playout independently of `clear`, and pick
between three call-progress modes per deployment. Plus an
end-to-end Twilio recipe, a Deepgram transcription reference
server, a CI gate on every PR, and the operator-facing TLS
deployment recipe.

Protocol stays at `version: "1"` — every new variant is additive,
so v1 WS servers built against 0.1.0 keep working unchanged.

### Added

- **Transcription reference WS server** (`examples/transcription-server-py/`). Streaming Python WS server that pipes every call's audio to Deepgram and emits one JSON-line transcript per result on stdout. Demonstrates the non-agent (observer) use case — real-time transcription, compliance recording, supervisor assist. README documents the swap pattern for AssemblyAI / Whisper / OpenAI; points at `openai-realtime-bridge-py` for the multi-provider abstraction. Single dep (`websockets>=13`); ~390 LoC including comments.

- **CI workflow** (`.github/workflows/test.yml`). Gates every PR and every push to main on `fmt + clippy -D warnings + cargo test --workspace` and a follow-up `SIPp signaling regression` job that builds the daemon, brings up the echo-ws-server, and runs `test-harness/sipp-scenarios/run-all.sh`. SIPp depends on lint-and-test so a broken build doesn't burn a SIPp spin-up. Cargo cache via `Swatinem/rust-cache@v2`; toolchain comes from `rust-toolchain.toml`. `run-all.sh` is now `DAEMON_BIN`-env-overridable so CI / operators can point at a release build or a custom path without editing the script.

- **Twilio Elastic SIP Trunking integration recipe**. `docs/INTEGRATIONS_TWILIO.md` walks the trunk-side setup (Origination URI, signalling-IP allowlist, TLS) and the siphon-ai-side config end-to-end; the Programmable Voice `<Dial><Sip>` flow gets a brief alternative section with a TwiML snippet. Runnable starter config at `examples/twilio-trunk/`.

- **`rtp_stats` WS event + RTP-quality histograms** (PROTOCOL §3.8). Periodic snapshot of RTP-quality state cached from forge `QualityDegraded` / `QualityRestored` events. Cadence configurable via `[bridge].rtp_stats_interval_ms` (default `5000`, mirroring RTCP §6.2; per-route override; `0` disables). Fields `jitter_ms` / `packet_loss_ratio` are `null` until forge reports a first assessment; `QualityRestored` resets them to `0.0` (distinct from `null`). Two histograms — `siphon_ai_rtp_jitter_ms`, `siphon_ai_rtp_packet_loss_ratio` — record values on every emission. HEP RTCP chunks (forge-hep) already ship to the configured collector — no extra wiring needed. `rtcp_rtt_ms` is not yet exposed (forge upstream gap; deferred to 0.2.1 / 0.3.0). New `RtpStatsTracker` helper with 7 unit tests.

- **`silence_detected` / `dead_air_detected` WS events** (PROTOCOL §3.6 / §3.7). Timer-derived primitives the WS server can use for "are you still there?" prompts and hung-call teardown. `silence_detected` is one-sided (caller has been VAD-silent past `[bridge].silence_threshold_ms`, default 3 s); fires once per silence stretch. `dead_air_detected` is two-sided (neither caller speech nor outbound WS audio past `[bridge].dead_air_threshold_ms`, default 10 s); re-fires on every elapsed threshold. Both thresholds are per-route overridable; `0` disables. Detection cadence is 500 ms. Underlying state machine factored into `IdleDetector` (8 unit tests). Counters: `siphon_ai_silence_events_total`, `siphon_ai_dead_air_events_total`.

- **`BridgeIn::Mute` / `BridgeIn::Unmute`** (WS protocol §4.6). Sustained AI-side mute primitive — distinct from `clear` (one-shot flush). On `mute`: subsequent audio bytes from the WS server are dropped (channel still drained so the WS server isn't back-pressured) and forge's playout queue is flushed for immediate silence. `unmute` releases the gate. Protocol-version unchanged; existing servers ignore the new variants.

- **Configurable SIP call progress** (`[sip.call_progress]`). New `mode` field selects what — if any — provisional response the UAS sends before the `200 OK`:
  - `instant_answer` (default; v0.1.0 behaviour): skip extra provisionals.
  - `ringing`: send `180 Ringing` (no body) before the 2xx.
  - `session_progress`: send `183 Session Progress` with the negotiated answer SDP before the 2xx (Flavour B per `docs/design/DEV_PLAN_0.2.0.md` §9.1 — best-effort, no `100rel`). Peers that include `Require: 100rel` in the INVITE fall back to `instant_answer` with a `warn!` log; reliable provisionals are deferred to 0.2.1 / 0.3.0.

  Backwards-compatible: existing configs without the `[sip.call_progress]` block keep v0.1.0 behaviour.

- **TLS deployment recipe** (`docs/DEPLOY.md` § TLS deployment). End-to-end walkthrough for a TLS-secured deployment using the SIP/TLS + WSS mechanics that already shipped in 0.1.0: cert provisioning options, `[sip.tls]` configuration, the file-permission pattern for cert/key under the systemd `siphon` user, Let's Encrypt deploy-hook for renewal, and an `openssl s_client` + SIPp `-t l1` smoke test. WSS works out-of-the-box against any publicly-signed cert because the WS client is built with `rustls-tls-webpki-roots` — no host-CA-store dependency.

### Changed

- **Rust toolchain pinned to `1.95.0`** (`rust-toolchain.toml`). Previously `channel = "stable"`, which let local dev clippy drift from CI clippy — a drift PR #78 surfaced when CI's clippy 1.95.0 caught a `result_large_err` lint that the older local clippy was silent on. Future-stable bumps are now an explicit edit to this file.

- **CI failure diagnostics for SIPp** (`.github/workflows/test.yml`). The SIPp regression job now cats every `*_errors.log` (in the scenarios dir; `run-all.sh` pins its CWD there so paths are predictable) and every daemon log on failure. The first real failure under the new pipeline — a `session_timer_echo` SIPp scenario using `[auto_media_port]` (added in SIPp 3.7; CI's ubuntu-latest apt sip-tester is 3.6.0) — was diagnosed and fixed in the same hour the dump was added.

### Known limitations

These are documented because they're DoD adjacent and worth setting expectation around.

- **`rtp_stats.rtcp_rtt_ms` is not populated.** The `rtp_stats` event has the field reserved in PROTOCOL §3.8, but jitter and packet-loss are the only quality dimensions the daemon currently exposes (forge-media doesn't surface RTT in the `QualityDegraded` / `QualityRestored` events the snapshot is derived from). RTT exposure is targeted at 0.2.1 / 0.3.0 alongside the forge-media work.
- **Reliable provisionals (RFC 3262 `100rel`) for `session_progress` mode** are not implemented. INVITEs that include `Require: 100rel` fall back to `instant_answer` for that call with a `warn!` log rather than sending a non-compliant unreliable 183. The reliable path is paired with `BridgeIn::Answer` (the "AI plays during the 183 phase" flow) for 0.2.1 / 0.3.0.
- **Hot reload of the SIP/TLS cert is not implemented.** Cert rotation requires a daemon restart; pair with an L4 load balancer if your traffic pattern can't tolerate that. The renewal recipe in `docs/DEPLOY.md` § TLS deployment uses a Let's Encrypt deploy-hook + `systemctl restart`.

### Deferred to 0.2.1 (Sprint 1 §5 stretch slip)

`docs/design/DEV_PLAN_0.2.0.md` §5 listed three stretch items that slip to 0.2.1 per the plan's own policy ("Stretch items slot into spare time, in §5 order. If stretch eats more than Week 5, bump them to 0.2.1."). For clarity:

- **mTLS for the bridge WebSocket connection** and wire-format validation against the WS server's cert. The 0.2.0 TLS recipe in `docs/DEPLOY.md` covers SIP/TLS + server-auth WSS + cert rotation; client-cert auth on the WS leg would need a `[bridge.tls.client_cert]` / `[bridge.tls.client_key]` config surface and the matching rustls connector wiring — not in 0.2.0.
- **Attended transfer (REFER with Replaces)** — depends on siphon-rs UAC capability that wasn't ready in time.
- **`examples/provider-toolkit-py/`** — a pluggable Deepgram/Whisper STT + OpenAI/Anthropic/Groq LLM + ElevenLabs/Cartesia TTS reference example. The 0.2.0 reference servers (`echo-ws-server-python`, `openai-realtime-bridge-py`, `transcription-server-py`) cover the canonical shapes; the multi-provider toolkit is a 0.2.1 cleanup item.

## [0.1.0] - 2026-05-22

First public release. SiphonAI is a provider-neutral SIP-to-WebSocket
media bridge: it terminates SIP calls, streams the call audio over a
WebSocket to a developer-supplied server, and plays audio received back
over that WebSocket into the call. It contains no AI code — the AI is
the WebSocket server's job.

### Added

#### SIP signaling

- Inbound trunk mode (UAS): accept calls from a SIP trunk or PBX, gated
  by an optional per-trunk source-IP / From-host allowlist.
- Registered-phone mode (UAC + REGISTER): register to a PBX (e.g. Cisco
  CUCM, Asterisk, FreeSWITCH) as a phone, with periodic re-REGISTER,
  retry/backoff, and digest authentication.
- Call lifecycle: INVITE / ACK / BYE / CANCEL, 100 Trying, provisional
  and final responses, re-INVITE for hold / resume.
- Blind transfer initiated from the WebSocket server (REFER).
- RFC 3261 / RFC 3581 response compliance: Via `received=` / `rport=`,
  rich Contact, and an honest `Allow` header on 405 / OPTIONS.

#### Media

- RTP / RTCP bridging via forge-media, with jitter buffering.
- Codecs: G.711 PCMU / PCMA (8 kHz) and G.722 (16 kHz).
- DTMF via RFC 2833 (telephone-event), surfaced to the WebSocket server.
- Barge-in: VAD-driven `speech_started` events for interruption handling.

#### WebSocket bridge protocol v1

- Bidirectional audio as 20 ms PCM16 little-endian mono frames
  (160 samples @ 8 kHz, 320 @ 16 kHz).
- Control and event messages with monotonic per-call `seq` numbering.
- Canonical protocol specification in `docs/PROTOCOL.md`.

#### Routing

- TOML dialplan: ordered, first-match-wins routes matched on the inbound
  INVITE (request URI, To, From, Call-ID, custom headers).
- Optional per-route regex matching and per-route overrides of global
  media / bridge settings.

#### Configuration

- Single TOML configuration file with load-time validation (invalid
  regex, dangling references, unset env vars fail loud at startup).
- Environment-variable expansion in config values.

#### Observability

- Structured `tracing` logs with `call_id` correlation.
- Prometheus metrics with bounded-cardinality labels.
- Distributed tracing spans for long-running per-call operations.
- HEP/EEP emission to Homer for SIP, RTCP, and application events.
- Call Detail Records (CDR) as JSON, to a file sink and/or webhook sink.
- Out-of-band lifecycle webhooks (call start / end, registration state).
- `/health` and `/ready` endpoints with k8s-correct semantics.
- Runtime per-target log-level adjustment via the admin API.

#### Packaging

- Multi-stage Docker image and `docker compose` quickstart stack.
- Idempotent Debian 13 install scripts with systemd units.
- Reference WebSocket servers in `examples/`: echo (Python / Node),
  an OpenAI Realtime bridge, and a Deepgram + LLM voice bot.

[Unreleased]: https://github.com/thevoiceguy/siphon-ai/compare/v0.41.2...HEAD
[0.41.2]: https://github.com/thevoiceguy/siphon-ai/compare/v0.41.1...v0.41.2
[0.41.1]: https://github.com/thevoiceguy/siphon-ai/compare/v0.41.0...v0.41.1
[0.41.0]: https://github.com/thevoiceguy/siphon-ai/compare/v0.40.0...v0.41.0
[0.14.1]: https://github.com/thevoiceguy/siphon-ai/compare/v0.14.0...v0.14.1
[0.14.0]: https://github.com/thevoiceguy/siphon-ai/compare/v0.13.0...v0.14.0
[0.13.0]: https://github.com/thevoiceguy/siphon-ai/compare/v0.12.2...v0.13.0
[0.6.2]: https://github.com/thevoiceguy/siphon-ai/compare/v0.6.1...v0.6.2
[0.6.1]: https://github.com/thevoiceguy/siphon-ai/compare/v0.6.0...v0.6.1
[0.6.0]: https://github.com/thevoiceguy/siphon-ai/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/thevoiceguy/siphon-ai/compare/v0.4.1...v0.5.0
[0.4.1]: https://github.com/thevoiceguy/siphon-ai/compare/v0.4.0...v0.4.1
[0.4.0]: https://github.com/thevoiceguy/siphon-ai/compare/v0.3.2...v0.4.0
[0.3.2]: https://github.com/thevoiceguy/siphon-ai/compare/v0.3.1...v0.3.2
[0.3.1]: https://github.com/thevoiceguy/siphon-ai/compare/v0.2.0...v0.3.1
[0.2.0]: https://github.com/thevoiceguy/siphon-ai/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/thevoiceguy/siphon-ai/releases/tag/v0.1.0
