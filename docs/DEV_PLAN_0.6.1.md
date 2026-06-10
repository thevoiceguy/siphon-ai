# SiphonAI 0.6.1 Development Plan

> **Status: decisions LOCKED** (user, 2026-06-09): consult leg = **originate
> API reuse** (no new creation protocol, §9.1); Refer-To **derived from the
> consult dialog** (`target` optional with `replaces_call_id`, §9.2);
> **outbound legs transferable** in 0.6.1 (§9.3). WS protocol stays
> `version: "1"` (one additive field). Ready to execute chunk-by-chunk.

Theme: **attended transfer** — the fast-follow 0.6.0 unlocked (plan §9.7).
The bot consults a human before handing the caller off: SiphonAI places the
consult leg as a normal outbound call, the bot talks to the agent over that
call's own WS session, and completion is one REFER with Replaces on the
original call. Blind transfer (0.2.0) covered "send the caller somewhere";
attended transfer covers "make sure someone is there first."

```
caller A ↔ SiphonAI ↔ WS session A (bot)
                │
                ├── POST /admin/v1/calls ──► agent C ↔ WS session C (bot)
                │        (consult leg = a 0.6.0 outbound call)
                │
                └── BridgeIn::Transfer{replaces_call_id} on session A
                         → REFER A (Refer-To: C's contact + Replaces)
                         → A INVITEs C with Replaces; both SiphonAI legs end
```

## 0. Why this is buildable now (grounded, not assumed)

Upstream feasibility verified against the pinned siphon-rs (Explore pass,
2026-06-09):

- **`create_refer_with_replaces` EXISTS** — `sip-uac/src/lib.rs:2756`:
  `(dialog: &Dialog, refer_to_uri: &SipUri, target_dialog: &Dialog) ->
  Request`, building the `Refer-To` with the URL-encoded `Replaces=`
  parameter from `target_dialog.id()` (call-id / to-tag / from-tag).
- **High-level wrapper EXISTS** — `IntegratedUAC::refer(dialog, refer_to,
  target_dialog: Option<&Dialog>)` (`integrated.rs:2138`): `None` = blind
  (what `core/transfer.rs` uses today), `Some` = attended. Same send path,
  same 202 handling, returns the implicit `Subscription`.
- **Dialog identifiers accessible** — `Dialog::id()` →
  `DialogId::{call_id, local_tag, remote_tag}`; `Dialog::remote_target()`
  is the consult leg's Contact (our Refer-To URI source, §9.2). The consult
  leg's `Dialog` is already held by `OutboundCall` (0.6.0 chunk 2).
- **Working examples upstream** — `sip-uac/examples/attended_transfer.rs` +
  `call_transfer_scenario.rs` show the intended usage end-to-end.

**No upstream work required.** 0.6.1 is siphon-ai glue: a consult-dialog
lookup, one additive protocol field, and the attended branch in the
transfer task.

## 1. Already shipped (context)

- Blind transfer (0.2.0): `BridgeIn::Transfer{target}` → per-call
  `TransferContext{sip_call_id, uac, dialog_manager}` → spawn task resolves
  the dialog by Call-ID → `uac.refer(…, None)` → 202 = `StopReason::
  Transfer`; non-2xx = `BridgeOut::Error{TransferFailed}`, call continues.
- Outbound origination (0.6.0): the consult leg's entire lifecycle —
  originate API, gateways, guardrails, its own WS session, webhooks, CDR,
  metrics — needs **zero new work**. A consult call IS an outbound call.

## 2. Scope (must-have)

### 2.1 Consult-dialog lookup (the one new cross-call touch)

REFER-with-Replaces needs the consult leg's `Dialog` while running on the
original call's transfer task. CLAUDE.md §4.4 forbids generalized cross-call
state, so this is a **narrow, explicit map**: outbound calls register
`bridge_call_id → Dialog handle` (Arc clone of what `OutboundCall` already
holds) in a daemon-wide `ConsultRegistry` owned alongside the
`CallRegistry`, inserted in `run_call` and removed on teardown (same
hygiene as the SIP-side registry). Lookup is read-only and by exact id —
no enumeration, no peeking at controller state.

### 2.2 Protocol: `transfer.replaces_call_id` (additive, version stays "1")

```jsonc
{ "type": "transfer", "call_id": "A", "replaces_call_id": "siphon-C…" }
```

- New optional field on the existing `transfer` message. Absent = blind
  transfer, byte-for-byte today's behavior.
- With `replaces_call_id`: `target` becomes **optional** — default is the
  consult dialog's `remote_target()` (its Contact, the URI the transferee
  can actually reach the agent at). If `target` IS sent, it overrides
  (escape hatch for SBCs that need a different reachable URI).
- Errors (all `BridgeOut::Error{TransferFailed}`, call keeps running):
  unknown `replaces_call_id`, consult call already ended, REFER rejected.
- PROTOCOL.md §transfer updated in the same PR (CLAUDE.md §4.2).

### 2.3 Attended branch in the transfer task

`core/transfer.rs` grows the attended arm: resolve consult dialog from the
`ConsultRegistry` → `uac.refer(&mut dialog_a, &refer_to, Some(&consult_
dialog))` → same `TransferOutcome` plumbing as blind. After 202: the
transferee INVITEs the agent with Replaces; the agent's PBX/endpoint
replaces the consult leg (we get BYEd on both legs through the existing
paths — outbound teardown for the consult leg, SIP BYE handling for leg A).
We do **not** proactively hang up the consult leg on 202; the far end
drives replacement (and if it never does, the consult call ends normally
via hangup/BYE like any call).

**Consult-cancel needs no new primitive**: the bot hangs up the consult
call (`hangup` on session C or admin force-hangup) and call A continues.

### 2.4 Transfer on outbound legs

Outbound `CallControllerConfig` currently hardcodes `transfer: None`. Wire
a context so the headline use case works: bot dials a customer (0.6.0),
customer asks for a human, bot consults an agent, completes — the REFER
lands on the *customer* leg. Implementation note: the outbound leg's
`Dialog` is held directly by `OutboundCall` (no `DialogManager` lookup
needed) — either verify the daemon `DialogManager` tracks UAC dialogs and
reuse `TransferContext` as-is, or add a dialog-direct variant to
`TransferContext`. **Verify at chunk time; both paths are local glue.**

### 2.5 Observability (same PR as the feature, CLAUDE.md §4.5)

- **Metric** — `siphon_ai_transfers_total{mode="blind"|"attended",
  result="accepted"|"rejected"|"local_error"}`. New (blind transfer
  currently has NO metric — this back-fills it).
- **Logs** — attended attempt/outcome at `info` with both call_ids in the
  fields; consult-lookup failure at `warn`.
- **CDR/webhooks** — no schema change: leg A ends with the existing
  transfer termination; the consult leg already emits the full 0.6.0
  outbound set. (Re-point: `call_end.termination_cause` distinguishes
  transfer already via `StopReason::Transfer` → verify the cause string
  mapping at chunk time and document it.)
- **HEP** — REFER/NOTIFY ship via the existing siphon-rs SIP emission;
  nothing new.

## 3. Out of scope — the AI line (unchanged)

No AI, no "warm transfer with AI summary," no whisper/announce (that's the
conferencing theme). The bot's consult conversation is the WS server's
business on session C; SiphonAI just bridges it.

## 4. Out of scope (explicit non-goals for 0.6.1)

- **Receiving** INVITE-with-Replaces (SiphonAI as transfer *target*) —
  siphon-rs has `ReplacesHeader::parse` but UAS integration is manual;
  defer until a use case demands it.
- Media mixing of the two legs inside SiphonAI (three-way) — conferencing
  theme.
- Transfer progress NOTIFYs surfaced to the WS server — blind transfer
  ignores them today; attended keeps parity (202 = success). Revisit if
  operators ask.
- New consult WS messages (`consult`/`consult_answered`/…) — rejected in
  §9.1; the originate API is the consult-creation surface.

## 5. Chunk plan (proposed)

1. **ConsultRegistry + outbound registration** (core): the narrow map,
   insert/remove in `run_call`, unit tests. No behavior change.
2. **Protocol + attended REFER** (bridge, core): `replaces_call_id` field +
   PROTOCOL.md; attended arm in `transfer.rs` + `call.rs`; target
   derivation/override; round-trip + error-path tests; metric (back-fills
   blind). The feature lands here.
3. **Outbound-leg transfer** (core, bin): transfer context on outbound
   `CallControllerConfig` (§2.4 verification + glue); tests.
4. **Docs + SIPp + release**: PROTOCOL.md polish, OUTBOUND.md cross-link,
   DEPLOY.md metric row, attended-transfer SIPp scenario (SIPp UAC calls
   in; runner originates a consult leg to a SIPp UAS; transfer with
   `replaces_call_id`; assert the REFER's Refer-To carries `Replaces=` with
   the consult dialog's ids), CHANGELOG, version 0.6.0 → 0.6.1, tag.

Each chunk: branch → PR → CI green → squash-merge, per CLAUDE.md.

## 6. Definition of Done — v0.6.1

- A live attended transfer works end-to-end in the SIPp harness: inbound
  call + consult leg + `transfer{replaces_call_id}` → REFER with a
  correctly-formed Replaces (verified against the consult dialog's ids) →
  202 → both legs torn down cleanly.
- Same flow on an *outbound* leg A (the §2.4 case) — covered at minimum by
  unit tests on the context plumbing; SIPp coverage if cheap.
- Blind transfer behavior is byte-for-byte unchanged (regression: existing
  blind_transfer.xml still passes).
- `siphon_ai_transfers_total` emits for both modes; documented.
- PROTOCOL.md documents the field, the derivation rule, the error cases;
  protocol stays `version: "1"`.

## 7. Risks

- **Replaces interop** — some PBXes (and some trunks) reject or mishandle
  REFER-with-Replaces from a B2BUA-ish endpoint. Mitigation: the failure
  mode is already graceful (non-2xx → `TransferFailed`, call continues),
  and the `target` override (§2.2) gives operators a knob. Document tested
  targets in the interop notes as they accumulate.
- **Consult-leg race** — the consult call ends (agent hangs up) between
  lookup and REFER. Window is small; the far end then rejects the Replaces
  INVITE and leg A's transfer fails gracefully. Accept; document.
- **Outbound dialog manager question** (§2.4) — unverified whether UAC
  dialogs are resolvable via the shared `DialogManager`. Both outcomes are
  cheap glue; flagged for chunk-3 verification, not a plan blocker.

## 8. Decisions (LOCKED 2026-06-09, via AskUserQuestion)

- **§9.1 Consult-leg creation = originate API reuse.** The consult leg is
  a plain `POST /admin/v1/calls` outbound call with its own WS session. No
  new WS messages for creation; the WS server needs admin-API reachability
  (it is the operator's infra; the reverse-proxy posture from 0.6.0 §9.5
  applies unchanged).
- **§9.2 Refer-To = derived from the consult dialog** (`remote_target()`
  Contact); explicit `target` optional and overriding when present.
- **§9.3 Outbound legs transferable in 0.6.1** — the outbound-bot → human
  handoff is in scope, not deferred.
