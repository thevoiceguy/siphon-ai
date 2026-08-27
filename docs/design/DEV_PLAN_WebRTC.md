# WebRTC Support for SiphonAI — Development Plan

**Status:** Draft for review
**Baseline:** siphon-ai 0.50.1, siphon-rs v2026.08.24, forge-media v2026.08.24 (forge's first tagged release; includes the G.711 browser-leg work below and embeds the same siphon-rs tag)
**Goal:** A browser can place and receive calls through SiphonAI with no SIP client installed — signaling over SIP-over-WebSocket (RFC 7118), media over ICE/DTLS-SRTP/Opus — and the WebSocket AI side sees exactly what it sees today: PCM16LE, fixed 20ms frames.

---

## 0. What already exists (verified against the repos)

This plan is shorter than it would have been six months ago because most of the media plane is already built upstream.

**forge-webrtc (0.4.0)** is an endpoint-shaped `PeerConnection`:

- ICE (RFC 8445) with trickle (RFC 8838), both roles, checks/nomination/keepalives on the single media socket
- DTLS-SRTP (RFC 5764) key exchange, both `a=setup` roles, fingerprint bound to signalled SDP
- SRTP/SRTCP (RFC 3711/7714) keyed directly from the DTLS export
- SDP offer *and* answer (RFC 3264/8829), BUNDLE + rtcp-mux, renegotiation with rollback
- **One audio section — Opus and G.711 (PCMU/PCMA), preference-ordered via `PeerConfig::codecs`** ([forge-media #130](https://github.com/thevoiceguy/forge-media/pull/130); this plan originally said "Opus only" and filed that PR). ICE restart deliberately unsupported.
- TURN client support in forge-ice (RFC 8489 long-term credentials)

**forge-codecs / forge-transcoder / forge-resampler** cover the Opus↔PCM16LE path (`audiopus`, sample-rate conversion, `RtpTranscoder` with payload-type mapping).

**siphon-ai 0.50.0** already runs DTLS-SRTP in production via `[media].srtp_offer = "dtls"`, and the cert just moved to ECDSA P-256 — the browser-preferred key type. The SIPp `delayed_offer_dtls` scenario covers that handshake.

**What does not exist:**

- siphon-rs has UDP/TCP/TLS transports only. **No WebSocket transport (RFC 7118).** This is the largest single piece of new code.
- siphon-ai does not depend on forge-webrtc or forge-ice at all.
- No browser-facing registration story (SIP.js clients REGISTER over WSS).
- No e2e test path for WebRTC media — SIPp cannot exercise it.

---

## 1. Architecture decision: the browser is a SIP endpoint

**Decision: signaling stays SIP.** A browser running SIP.js connects over WSS, REGISTERs, and places calls with ordinary INVITE/BYE. SiphonAI grows one new *transport* and one new *media leg type* — not a second signaling protocol.

Why this and not WHIP/WHEP or a custom HTTP offer/answer API:

- WHIP/WHEP are ingest/egress-shaped (unidirectional session setup, no mid-call control, no hold/transfer/REFER semantics). Calls are dialog-shaped.
- SIP.js and JsSIP are mature, maintained, and speak RFC 7118 out of the box. Zero client-side protocol work.
- Everything downstream of the transaction layer — dialogs, CDR, routing, admission, STIR/SHAKEN, HEP capture — works unchanged, because it's just SIP.
- It preserves the project identity: SiphonAI is a neutral bridge, and "the browser is another SIP peer" is a one-sentence explanation.

**Rejected alternative worth recording:** terminating WebRTC media with a custom REST signaling API. It would ship faster for a demo but forks the call model — two signaling paths through routing, admission, and CDR forever. Not worth it.

### Codec decision: Opus by default, transcode at the bridge; G.711 available upstream

Two options were on the table:

1. **Transcode Opus↔PCM16LE in media-glue** using forge-transcoder + forge-resampler. Works today, no upstream changes. Opus at 48kHz decoded and resampled to the bridge's PCM16LE rate. CPU cost is real but small (libopus decode is ~1–2% of a core per call).
2. Extend forge-webrtc to also offer PCMU/PCMA (G.711 is mandatory-to-implement in WebRTC, all browsers accept it). Skips transcoding entirely and matches the SIP-side codec.

**Decision: ship (1) as the default; (2) is built** — it was implemented upstream ahead of the phases rather than just filed ([forge-media #130](https://github.com/thevoiceguy/forge-media/pull/130), merged 2026-08-24). forge-webrtc now negotiates a preference-ordered codec list (`PeerConfig::codecs`, default Opus-first), answers pin exactly one codec at the remote's payload type, and `negotiated_codec()`/`AudioSender::samples_per_20ms()` tell the media layer what to encode and how to frame it. Opus stays the default because it's the *better* codec for the primary use case — a wideband 48kHz browser leg feeding a voice AI pipeline beats 8kHz G.711 for ASR quality. What (2) buys Phase 2: a per-route codec preference can put PCMU/PCMA first on the browser leg to match a G.711 SIP leg and skip the transcode entirely — worth exposing in `[webrtc]`/per-route config when the leg lands, but the transcode path remains the default and must work regardless (a browser answering an Opus-preferring offer picks Opus).

### ICE role

forge-webrtc supports both ICE roles, so no ICE-lite shortcut is needed — use it as shipped, controlled role on the answering side. Configure `stun_servers` and optional TURN in `[webrtc]` config. TURN is the operator's problem (coturn), not ours; we just plumb credentials.

---

## 2. Phase 0 — Prerequisite: dialog teardown hardening

**The CUCM hung-dialog/teardown leak (503-before-INVITE after several sessions) ships before any WebRTC code.**

This is not just sequencing hygiene. Browser peers are the worst-case client for dialog lifecycle: tab closes, page refreshes, laptop lids, and network handoffs all produce abrupt teardowns with no BYE. A WebRTC leg multiplies the exact failure path the CUCM bug lives in. Shipping WebRTC on top of a known teardown leak means shipping a repro machine for it.

Deliverables:

- Root-cause and fix the bridge-side hung-dialog leak (BYE exchange handling)
- Add a dialog-lifecycle soak scenario to the test harness: N sessions with mixed clean/abrupt teardown, assert zero residual dialogs and zero leaked RTP port pairs afterward (the port pool metrics from 0.50.0 make the leak observable now)
- **Define transport-loss teardown semantics while in there:** when a connection-oriented transport drops mid-dialog, the dialog must be reaped after a grace timer. This lands in Phase 0 as generic TCP/TLS behavior and is exactly what WSS disconnect will hook into in Phase 1 — a browser tab closing *is* a transport drop.

Exit criteria: soak passes at 10× the CUCM repro count; CUCM deployment confirms fix in the field.

---

## 3. Phase 1 — siphon-rs: WebSocket transport (RFC 7118)

New work in `sip-transport`, plus touches in `sip-core` (Via transport tokens) and registrar.

### 3.1 Transport implementation

- WS and WSS listeners (tokio-tungstenite; TLS via the existing rustls plumbing — browsers require secure contexts, so WSS is the only mode that matters in practice, but plain WS stays for lab use)
- SIP messages as complete WebSocket *text or binary messages* (RFC 7118 §4: one SIP message per WS message, no stream framing — simpler than the TCP path)
- `Via: SIP/2.0/WS` / `WSS` transport tokens, `;transport=ws` URI parameter, RFC 3263 resolution additions
- HTTP upgrade handling: `Sec-WebSocket-Protocol: sip` negotiation (reject upgrades without it), configurable `Origin` allow-list
- HEP3 capture on the new transport, same as every other transport — Homer sees browser signaling on day one
- Per-source ingress rate limiting via the existing `sip-ratelimit` token bucket

### 3.2 Browser client realities

Browsers sit behind NAT with unroutable Contact URIs. Handle this the way every WebSocket SIP server does:

- **Connection-reuse routing:** responses and in-dialog requests to a WS client go down the connection the client established, ignoring Contact/Via addresses. This is RFC 7118 §5.2 behavior and effectively RFC 5626 outbound-lite. Full RFC 5626 (reg-id/instance-id, flow failover) is explicitly out of scope for v1 — one connection per client, reconnect means re-REGISTER.
- Registrar: bind registrations to the WS connection; expire on disconnect after the Phase 0 grace timer.
- Digest auth over WS works unchanged (SIP.js supports it); credentials come from the existing auth config.

### 3.3 Testing

- Unit tests against tungstenite loopback for framing, fragmentation edge cases, oversized messages
- SIPp cannot speak RFC 7118; use a small Rust test client (sip-testkit addition) or SIP.js under Node for transport-level integration tests
- Add WS to the transport matrix in siphon-rs CI

**Exit criteria:** SIP.js in Chrome REGISTERs over WSS, places a call that routes to a plain SIP UAS, and signaling is fully visible in Homer. Media at this phase: none negotiated or dummy — this phase is signaling only.

> **Validated 2026-08-25** (`examples/browser-sip/`, headless Chromium + SIP.js 0.21 driving the real daemon): digest REGISTER over WSS with Origin enforcement → `siphon_ai_registrar_bindings 1`, binding expired via the connection-loss grace when the browser died; the full REGISTER/401/200 exchange captured in Homer with `Via: SIP/2.0/WSS`; and the browser's test INVITE was digest-challenged, re-authenticated by SIP.js, **routed** (`route=default`), then rejected by the media layer with the precise `488 offer profile UDP/TLS/RTP/SAVPF rejected under srtp_mode = Off` — which is the exact seam where Phase 2's §4.1 detection rule instantiates the WebRTC leg instead. `headless-check.sh` re-runs the whole check unattended and seeds Phase 3's nightly harness.

**Estimated scope:** the largest and least parallelizable phase. The RFC 7118 framing itself is small; connection-reuse routing and registration-to-connection binding are where the time goes.

---

## 4. Phase 2 — siphon-ai: the WebRTC media leg

New crate: `crates/webrtc-glue`, mirroring the `sip-glue`/`media-glue` split. Depends on forge-webrtc, forge-ice, forge-transcoder. Gated behind a `webrtc` cargo feature so the default build is byte-identical to today's.

### 4.1 Leg selection

When an INVITE arrives over WS/WSS transport, the offer SDP is WebRTC-shaped (`a=fingerprint`, ICE attributes, BUNDLE group). Detect this in sip-glue and instantiate a `PeerConnection` from forge-webrtc as the media leg instead of a plain forge-engine RTP session. Same dialog machinery, different media backend. Outbound to a registered browser client is the mirror image: originate with a forge-webrtc offer.

Detection rule: **transport type selects eligibility, SDP shape selects the leg.** A WS client sending a plain RTP offer (possible from a non-browser RFC 7118 client) still gets a plain RTP leg. No magic.

### 4.2 SDP plumbing

- forge-webrtc owns offer/answer generation for its leg; sip-glue passes SDP bodies through rather than synthesizing them via sip-sdp for this leg type
- Trickle ICE: v1 uses **complete-gathering-before-answer** (no trickle in signaling). SIP trickle (RFC 8840, INFO-based) is messy and SIP.js works fine without it when the server's candidates are host candidates on a public address — gathering completes in milliseconds. Revisit only if TURN-relayed server candidates become a supported deployment.
- Renegotiation (hold, re-INVITE) maps onto forge-webrtc's re-offer support. ICE restart is unsupported upstream — document that a mid-call network change on the browser side means the call drops and the client redials. This is an acceptable v1 limitation; browsers on wifi→cellular handoff are an edge case for the target use cases (agent testing, click-to-call).

### 4.3 Audio path

- Decode Opus 48kHz → forge-resampler → the bridge's internal PCM16LE rate → existing 20ms framing to the AI WebSocket. Encode path mirrors it. When the leg negotiated G.711 instead (per-route preference, forge-media #130), the "transcode" collapses to the same G.711 codec path classic legs use — no resampler, no libopus; `AudioSender::samples_per_20ms()` (160 vs 960) drives the framing either way.
- The `fixed_audio_packet_size` contract on the AI WebSocket is **unchanged and non-negotiable** — the transcode happens entirely inside media-glue, and a wrong-sized frame still tears down the call. Add a transcoder-output assertion in debug builds.
- Opus PLC/FEC: enable decoder FEC; browser networks are lossier than SIP trunks and it's a free quality win.

### 4.4 Ports, admission, and the pool

- ~~A WebRTC leg uses **one socket** (BUNDLE + rtcp-mux) versus the RTP/RTCP pair a classic leg allocates. Draw it from the same pool as one pair so `[media].reserved_outbound_calls` and the admission math stay coherent; a config note explains the accounting.~~ **Done.** `MediaSetup::reserve_port_pair` hands the leg a `PortReservation` under the same floor an inbound SIP call uses, and forge-webrtc binds its socket to that pair's RTP port (forge-media [#134](https://github.com/thevoiceguy/forge-media/pull/134) added `TransportConfig::local_port` and `SessionManager::reserve_port_pair`).

  Implementing it turned up a **fourth** reason beyond the three the plan anticipated, and it is the one an operator would hit first: a socket bound outside `rtp_port_range` is **unreachable through the firewall they configured for RTP**. Accounting can be reconciled after the fact; a blocked media path is a broken call in a deployment that did everything right.

  Two decisions worth recording. The leg holds a whole *pair* despite using one port, because a pair is the unit the pool, the gauge, and the reserved band all count in — charging a browser call half a slot would make `reserved_outbound_calls` mean different things depending on who called. And the reservation releases on **`Drop`**, not on an explicit teardown call: forge's release is `async` so `Drop` spawns it, which is slightly awkward but makes forgetting impossible. An explicit `release().await` is what a leak looks like the first time someone adds an early `return` above it — and Phase 0 of this plan exists precisely because that bug class is easy to write and hard to see.

  Because `siphon_ai_rtp_port_pairs_allocated` is *sampled from pool truth* rather than incremented at call sites, browser calls appear in the capacity gauge with no new metric — and the Phase 0 soak assertion that the gauge returns to zero now covers WebRTC legs for free.
- ICE consent freshness and DTLS handshake timeouts feed the same watchdog that tears down `server_too_slow` calls, so a browser that connects signaling but never completes media doesn't hold a slot. New timeout: `[webrtc].setup_timeout` (default 15s from answer to DTLS-complete).

### 4.5 Config surface

```toml
[webrtc]
enabled = false                  # default off, feature-gated build
stun_servers = []                # e.g. ["stun:stun.l.google.com:19302"]
turn_servers = []                # optional, with credential refs via EnvironmentFile
setup_timeout_secs = 15

[sip.transport.wss]
listen = "0.0.0.0:8443"
cert = "..."                     # reuse existing TLS config shape
allowed_origins = ["https://ops.example.com"]
```

Everything appears in `--inspect-config`; changing any of it wants a restart (consistent with existing EnvironmentFile/restart semantics).

### 4.6 Observability and CDR

- ~~New metrics: ICE connection state transitions, DTLS handshake duration, transcode CPU time per call, consent-freshness failures, per-leg codec label on existing audio metrics~~ **Done.** Five metrics, all WebRTC-leg-only: `siphon_ai_webrtc_legs_total{codec,result}`, `siphon_ai_webrtc_legs_ended_total{reason}`, `siphon_ai_webrtc_ice_seconds`, `siphon_ai_webrtc_dtls_seconds`, `siphon_ai_webrtc_transcode_seconds{direction}` (all in `docs/DEPLOY.md`).

  Three notes worth keeping. **(1)** Splitting `ice_timeout` from `dtls_timeout` — no path at all versus a path whose handshake failed, two different faults with two different fixes — required making the setup wait *event-driven* (`SetupOutcome`, `await_setup`) instead of polling the connection state. Polling collapses ICE nomination and DTLS completion into one "connected"; the transition is the diagnosis, so it has to be observed as it happens. **(2)** *Consent-freshness failures do not exist as an upstream event.* forge-webrtc sends RFC 7675 keepalives but never fails a transport when the replies stop, so the real detector for a browser that vanished is the inactivity watchdog — which is also what gives the slot back. It is counted honestly as `legs_ended_total{reason="inactivity"}` rather than a metric named after a mechanism we do not have; if forge-webrtc ever emits a consent failure, that becomes its own label. **(3)** Transcode cost is recorded once per leg, not per frame: 50 histogram observations a second per call would cost more than the work being measured. The per-leg `codec` label is what makes it readable — Opus and G.711 differ by an order of magnitude, which is the case for `[webrtc].prefer_g711` on a busy node. A codec label was *not* added to the classic path's RTP quality histograms: those come from RTCP on a leg type where the codec is already on the CDR and the `start` message, and widening them would cost cardinality on the hot path for nothing.
- ~~CDR: add leg-transport (`udp|tcp|tls|ws|wss`) and media-type (`rtp|srtp|webrtc`) fields — this is a CDR schema bump (v8 → v9), so it rides the existing CDR versioning process and gets called out loudly in the changelog~~ **Done.** v9, both fields, 50 → 52 CSV columns, documented in `docs/DEPLOY.md` and called out at the top of the changelog.

  Three decisions worth recording. **(1)** They are typed `Option` and **omitted where genuinely unknown** rather than defaulted: a pre-v9 record parses with both `None`, and a delayed-offer call that failed negotiation before any media existed gets `leg_transport` with no `media_type` — claiming `rtp` there would assert that cleartext audio flowed when none did. Same reasoning for siphon-rs's SCTP transport kinds, which no siphon-ai listener produces: no CDR name, so nothing recorded. **(2)** `media_type` is derived from `start.srtp` — the same fact the WS server was told — so the record and the bridge cannot disagree about whether a call was encrypted; and a browser leg is checked *first*, because it deliberately carries no `srtp` block (DTLS-SRTP is intrinsic to it) and reading that absence as "cleartext" is exactly the bug the ordering prevents. **(3)** Outbound records what the peer *answered*, not what the gateway offered: a `preferred` trunk that downgraded says `rtp`.

  Verified end to end: `examples/browser-sip/headless-check.sh` now enables a CDR file sink in `lab.toml` and asserts a real headless-Chrome call lands as `version 9, leg_transport=wss, media_type=webrtc`.
- sightglass: surface ICE/DTLS state per call

---

## 5. Phase 3 — Testing and interop

SIPp covers none of the media plane here, so the harness grows two new legs:

1. **forge-webrtc loopback in-process:** a test PeerConnection dialing the bridge — covers ICE, DTLS, SRTP, Opus transcode, teardown, and runs in plain CI. This is the workhorse; it's cheap enough for the soak/load harness, so wire it into the same mixed-traffic runs that produced `RESULTS-0.49.9-mixed-and-soak.md`.
2. **Real-browser e2e:** Playwright driving headless Chromium + SIP.js against a containerized bridge, asserting two-way audio (inject a tone, assert level on the AI WebSocket, and vice versa). Runs nightly, not per-commit.

Interop matrix, in priority order: Chrome, Firefox, Safari (its DTLS and Opus behaviors diverge most) × SIP.js and JsSIP. Safari is where the surprises will be — budget time for it.

Abrupt-teardown scenarios from Phase 0 get a WebRTC variant: kill the browser page mid-call N hundred times, assert zero dialog/port leaks. This is the acceptance test that ties the whole plan together.

---

## 6. Phase 4 — Ship

- Package: the `webrtc` feature compiles into the release `.deb` and container (same artifacts, feature on at build, `enabled = false` at runtime by default — no second package to maintain)
- Docs: a quickstart that goes from `docker compose up` to a talking browser tab in under five minutes, including a minimal bundled SIP.js example page served from the observability port. The example page is the demo *and* the smoke test.
- Blog post for Phones Still Exist: "your browser is a SIP phone now" — the RFC 7118 + forge-webrtc story, with the real numbers from the soak runs. Natural sequel to the launch post.
- README/CHANGELOG discipline: WebRTC lands in the CHANGELOG with the same rigor as 0.50.0, and the README feature list updates in the same PR. (The README lag problem does not get a new feature to lag on.)

---

## 7. Sequencing, risk, and effort

| Phase | Depends on | Risk | Relative effort |
|---|---|---|---|
| 0 — Teardown hardening | — | Low (known bug) | S–M |
| 1 — RFC 7118 in siphon-rs | 0 (grace-timer semantics) | Medium | **L** |
| 2 — WebRTC leg in siphon-ai | 1, forge-webrtc as-is | Medium | M–L |
| 3 — Test harness + interop | 2 | Safari unknowns | M |
| 4 — Ship | 3 | Low | S |

**Top risks:**

1. **forge-webrtc maturity under churn.** It looks complete, but it hasn't been hammered by real browsers at volume. Mitigation: Phase 3 item 1 goes into the load harness early, and issues get filed/fixed upstream the way the port-pool work was (#120 precedent).
2. **Registration/connection lifecycle bugs** — the same class of bug as the CUCM leak, on a new transport. Mitigation: Phase 0's grace-timer semantics are designed for this before the transport exists.
3. **Safari.** Always Safari.

**Out of scope for v1 (recorded so they're decisions, not omissions):** video, data channels, full RFC 5626 outbound with flow failover, SIP trickle-ICE (RFC 8840), ICE restart / mid-call mobility, acting as a TURN server. (G.711 on the browser leg was on this list as "filed upstream instead" — it has since shipped upstream in [forge-media #130](https://github.com/thevoiceguy/forge-media/pull/130); wiring a per-route preference for it into siphon-ai is a Phase 2 config detail, not a separate scope item.)
