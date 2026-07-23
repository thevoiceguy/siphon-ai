# SiphonAI — Full Functional Test Plan

**Target build:** 0.40.0 (single-node test box; host/IP kept in private engagement notes, not this repo)
**Author:** testing engagement (maintainer = `thevoiceguy`)
**Status:** v1 complete (§0–§6). Synthesized from a full codebase surface-inventory (WS protocol, admin/config/CLI, call features, observability). Living doc — update ✅/🟡/⬜ as cases run.
**Purpose:** exercise every SiphonAI subsystem to flush out bugs, prioritized, with each case marked for what setup it needs.

---

## §0. Environment, method, and conventions

### 0.1 Box & access
- Installed via `.deb`, systemd unit `siphon-ai`, binary `/usr/bin/siphon-ai`, config `/etc/siphon-ai/config.toml` (`root:siphon-ai 0640`).
- The `siphon` login user is **not** in the `siphon-ai` group → config, `/var/log/siphon-ai` (CDRs), `/var/lib/siphon-ai` (recordings) and `journalctl -u siphon-ai` all need **sudo**, and sudo needs a password → **the tester (human) runs sudo commands in their own SSH terminal**; the agent cannot (no TTY).
- **Paste-safety:** long single-line commands get split by the terminal on paste (observed break ~78 cols). Keep each command short and on its own line; prefer `printf | sudo tee -a` over in-place multi-line edits. To glob a path under `/var/lib/siphon-ai` (which `siphon` can't traverse), let root expand it: `sudo bash -c 'cp …/*hex*.wav /tmp/r.wav'`.
- **Config preflight** (reproduces daemon env, incl. `${SIPHON_ADMIN_RO}` from `EnvironmentFile=-/etc/siphon-ai/env`): `sudo bash -c 'set -a; . /etc/siphon-ai/env; siphon-ai --config /etc/siphon-ai/config.toml check'`.

### 0.2 Endpoints & auth
- **Observability listener** `127.0.0.1:9091` — open: `/health`, `/ready`, `/metrics`.
- **Admin API** `127.0.0.1:9092` — bearer auth, RBAC. Tokens: `~/.siphon-op-token` (operator), `~/.siphon-admin-token` (admin); a `${SIPHON_ADMIN_RO}` read-only token exists via env (no file). Roles nest: `ReadOnly < Operator < Admin`.
- **Bot / WS server** `ws://127.0.0.1:8080/` — deepgram-llm-bot-node (returns 426 to plain GET = healthy).
- **Lifecycle webhooks** currently **enabled** → `http://127.0.0.1:8899/hook` (temporary test receiver `scratchpad/webhook_receiver.py`, logs to `webhook_hits.log`; allowlist currently `conference_created,conference_ended` — widen to all events for full webhook coverage).

### 0.3 Driving calls (single-endpoint reality)
- **Outbound** is the primary lever: `POST /admin/v1/calls` (admin role) `{"to":"<TESTER_CELL>","gateway":"twilio-out","ws_url":"ws://127.0.0.1:8080/"[,"recording":"always"][,"delayed_offer":true][,"from":"…"]}`. `<TESTER_CELL>` = the tester's own E.164 number (kept out of this repo); `twilio-out` = Twilio Secure Trunking (TLS + SRTP required).
- **Inbound** and **2-endpoint** scenarios (conference mixing with 2 callers, blind/attended transfer between two live legs, registration against a PBX) need setup the box doesn't have by default — flagged 🔒 per case.

### 0.4 How each test observes correctness
| Channel | Where | Access |
|---|---|---|
| CDR (JSONL) | `/var/log/siphon-ai/cdr.jsonl` | sudo |
| CDR (CSV) | switch `[cdr.file].format="csv"` + new path | sudo |
| Logs | `journalctl -u siphon-ai` (spans carry `call_id`/`room_id`) | sudo |
| Metrics | `curl :9091/metrics` | open |
| Webhooks | test receiver on :8899 | agent-readable |
| Recordings | `/var/lib/siphon-ai/recordings/<call_id>.wav[a]` | sudo to copy out |
| HEP → Homer | Homer UI / hep-collector-stub | needs Homer |

### 0.5 Legend
- **Status:** ✅ verified this engagement · 🟡 partially verified · ⬜ not yet tested
- **Setup:** 🟢 outbound-only (doable now) · 🔵 needs inbound path · 🟣 needs 2nd endpoint/PBX · 🟠 needs object storage/Homer/other
- **Priority:** **P0** core call path & billing-grade data · **P1** major features · **P2** edges/hardening

### 0.6 Already verified this engagement (don't re-run unless regressing)
- Outbound origination + far-end BYE attribution (`caller_hangup`), teardown timing (#324).
- CDR v5: `answered_at` present, billable derivable (#331); `termination.cause=caller_hangup` (#332); quality block incl. `tx_*`.
- `rx_packets_lost` no longer counts ring as loss (#330 / forge-media #94); MOS not floored.
- CDR sink master-switch warning (#333); `[cdr] enabled=true` required.
- Conference: auth/RBAC, join, mixing, participant-remove, auto-cleanup, **DELETE force-end** (call survives, reverts to direct bridge), metrics.
- **`conference_created` / `conference_ended` webhooks** (payloads, `peak_participants` 0 and 1, `duration_ms`, idempotency header, delivery metrics).
- **Recording** during a conference (continuity across join/force-end, stereo L=caller/R=room-mix, `recording_*` CDR fields), plaintext WAV plays in a standard player.
- **CDR CSV**: 49-column header exact match, row alignment, `answered_at` 49th column, empty-optional cells.

---

## §1. WebSocket protocol (v1)

SiphonAI is the WS **client**; the dev server is the WS **server**; one WS per call. Canonical: `docs/PROTOCOL.md`, wire types `crates/bridge/src/protocol.rs`, schema `schemas/siphon-ai.v1.json`.

### 1.0 Method — two harnesses
- **protocol-testkit (no phone needed).** `crates/protocol-testkit` *plays the daemon* against a candidate WS server and validates every frame (schema + typed parse + exact frame-byte size + pacing). Use for the bulk of §1. **P0 baseline:** run bundled scenarios first — `basic-echo, dtmf, recording-controls, hangup-semantics, keepalive, barge-in-pause` — then add gap scenarios below.
- **Live outbound call** (`ws://127.0.0.1:8080/`) for end-to-end reality: the deepgram bot is the WS server, so BridgeIn coverage is limited to what that bot sends. To exercise arbitrary BridgeIn commands, point the originate at a **custom test WS server** we control (script that sends specific control messages on cue). 🔵 flag = needs a driver server or inbound path.

### 1.1 Server→SiphonAI control messages (`BridgeIn`) — P1 unless noted
All must carry `call_id`, must not carry `seq`. **Invariant to assert on every one:** a schema-valid but state-invalid command is a no-op, never tears down the call.

| ID | Message | Steps | Expected / observe | Setup |
|---|---|---|---|---|
| WS-IN-01 | `clear` | send mid-playout | queued audio dropped, pending `mark`s behind it dropped w/o firing | 🟢/🔵 |
| WS-IN-02 | `mark{name}` | send while streaming audio | `BridgeOut::mark{name}` fires when prior audio drained | 🔵 |
| WS-IN-03 | `hangup{cause}` | send each cause normal/rejected/busy/not_acceptable | `stop{server_hangup}`+close; SIP 200-BYE/603/486/488 respectively | 🔵 |
| WS-IN-04 | `send_dtmf{digit,duration_ms}` | send `0-9*#ABCD`; duration clamps `[40,2000]` | RFC2833 injected toward caller; `tx_packets_sent` rises | 🟢/🔵 |
| WS-IN-05 | `mute` / `unmute` | mute, confirm caller silence, unmute | bot audio dropped+flushed while muted; other controls still flow; unmute no-op if not muted | 🔵 |
| WS-IN-06 **P0** | `conference_join{room_id}` / `conference_leave` | join then leave | `conference_joined{room_id,participants}` / `conference_left{left}`; leave-when-not-in-room = silent no-op | `[conference].enabled` 🔵 |
| WS-IN-07 | `hold` / `resume` | hold, resume; also hold-when-peer-held | `held`/`resumed`; idempotent re-ack; peer-held → `error{hold_failed}`, no stacking, call survives | `HoldContext` (inbound) 🔵 |
| WS-IN-08 | `park` | send | `stop{park}`+close; SIP/RTP stay up (hold music) | `[park].enabled` 🔵 |
| WS-IN-09 | `transfer{target}` / attended `{replaces_call_id}` | blind then attended | 2xx → `stop{transfer}`+close; failure → `error{transfer_failed}`, call continues; **unconfigured → `error{transfer_failed}` "not configured"** | `IntegratedUAC`/`TransferContext` 🟣 |
| WS-IN-10 | recording ctl: `start/stop/pause/resume_recording` | full cycle | `recording_started/stopped`; pause drops (not silences) span; ignored if `rec_blocked` | `[recording].mode="on_demand"` 🔵 |
| WS-IN-11 | `set_recording_consent{note}` | send | **no reply**; lands on CDR `consent.server`; not a gate | 🔵 |
| WS-IN-12 | `barge_in_confirm` / `barge_in_reject` | send during pause arbitration | retained tail dropped(confirm)/re-queued(reject); `barge_in_resolved{confirmed|rejected}`; no-op if none pending | `[bridge.barge_in].mode="pause"` 🟢(testkit) |

### 1.2 SiphonAI→server events (`BridgeOut`) — P1
Every event carries `seq` (u64) + `call_id`. Assert presence/shape and trigger:
- **Speech/VAD:** `speech_started{ts_ms,decision_pending?,decision_deadline_ms?}`, `speech_stopped{ts_ms,duration_ms}` — always emitted. **P1**
- **Peer media:** `hold{direction}` / `resume` (peer re-INVITE) — distinct from your `held`/`resumed`. **P1** 🔵
- **Silence/dead-air:** `silence_detected{duration_ms}` (`[bridge].silence_threshold_ms` def 3s), `dead_air_detected{duration_ms}` (def 10s, re-fires). **P2**
- **`rtp_stats`** every `rtp_stats_interval_ms` (def 5s): assert `Option` fields omitted-not-null until data; **assert `tx_packets_lost_reported` is signed and may be negative** (0.38.0). **P1**
- **`dtmf{digit,duration_ms,method=rfc2833|inband}`** caller keypress. **P1** 🔵
- **`mark{name}`** ack. **Conference fan-out:** `participant_joined/left{participant_call_id}` on OTHER sessions. **P0** 🟣(needs 2nd leg)
- **`barge_in_resolved{confirmed|rejected|timeout}`** — emitted for EVERY resolution incl. timeout + preempting commands. **P1**
- **`stop{reason}`** always last + close 1000; reasons `caller_hangup|server_hangup|transfer|ws_disconnect|park|error`. **P0**
- **`error{code,message}`** — fatal→`stop{error}` follows; non-fatal (`audio_format|conference_failed|park_failed|hold_failed`) → no stop. **P0**

### 1.3 Audio framing invariants — **P0**
- WS-AUD-01: every binary frame **exactly** 320 B @8k / 640 B @16k (20 ms, PCM16-LE mono). Testkit asserts per-frame.
- WS-AUD-02: **wrong-size frame** → dropped, non-fatal `error{audio_format}` (rate-limited ≤1/s), call continues.
- WS-AUD-03: binary=audio, text=JSON control, interleave freely; text >256 KiB rejected.
- WS-AUD-04: outbound backpressure — buffer 200 ms (10 frames); overflow drops oldest + `siphon_ai_outbound_audio_frames_dropped_total`.
- WS-AUD-05: pacing — server audio faster than real-time flagged by testkit.

### 1.4 `start` message & `version` — **P0**
- WS-ST-01: `start` is first msg, `seq=0`, carries `from/to/direction/audio{encoding,sample_rate,channels,frame_ms}/sip{call_id,headers}`. Assert `direction=outbound` on originated calls. Never assume any `sip.headers` present (allowlist may be empty).
- WS-ST-02: optional fields present only when applicable — `srtp{exchange,profile}` (encrypted leg), `verstat` (STIR/SHAKEN), `retrieved:true` (park-retrieve), `reconnected:true` (WS reconnect), `trace_context` (OTLP on), `barge_in_mode`. Assert **absent-not-false** normally.
- WS-ST-03: `version` is string `"1"`; additive changes don't bump. Server unwilling to speak v1 → `hangup`/close 1003.
- WS-ST-04: **start-deadline** — server must send first audio (or `hangup`) within `server_start_deadline_secs` (def 5s) or `error{server_too_slow}`+`stop`. A control msg alone does NOT satisfy it.

### 1.5 `seq` & ordering — **P0**
- WS-SEQ-01: `seq` strictly +1 across all messages on one socket, starts 0 at `start`, **never resets within a call/session**.
- WS-SEQ-02: park-retrieve and reconnect open **new sessions** → seq=0, `retrieved/reconnected:true`, **no replay** of prior events.
- WS-SEQ-03: `start` first, `stop` last, controls+binary interleave between.

### 1.6 Errors & edge cases — **P0/P1**
- WS-ERR-01 **P0**: malformed JSON / unknown `type` / mismatched `call_id` → fatal `error{protocol_error}`+`stop{error}`+close; **not** reconnected even if reconnect on.
- WS-ERR-02: non-fatal errors continue call (`audio_format`,`conference_failed`,`park_failed`,`hold_failed`) — assert no `stop` follows.
- WS-ERR-03 **P2 (bug-hunt)**: **`codec_unsupported` must NEVER appear on the wire** — enum variant exists (`protocol.rs:649`) but PROTOCOL.md §3.10 says codec failure is SIP 488 before WS opens. Assert never emitted.
- WS-ERR-04: liveness — SiphonAI pings every 15s; no pong in 10s → `error{internal,"ws keepalive timeout"}`+drop (or reconnect). Testkit `keepalive` covers baseline.
- WS-ERR-05: reconnect (opt-in `ws_reconnect_enabled`) — unexpected drop → hold music + redial same `ws_url` → `start{reconnected:true}` seq=0 no replay; give up after `ws_reconnect_max_secs` (def 30) → `ws_disconnect`. `hangup`/already-`stop` never reconnected. Default off → fallback prompt + BYE + CDR `ws_disconnect`.
- WS-ERR-06 **P2**: reserved namespaces — `type` starting `_`, fields `x-`, close 4000-4099 must be unused by v1.

### 1.7 Testkit coverage (done) vs gaps (to add)
- **Covered by bundled scenarios:** basic-echo (audio + unknown-msg tolerance), dtmf, recording-controls, hangup-semantics (+reconnect), keepalive, barge-in-pause (reject path).
- **Gap scenarios to author (P1):** transfer/REFER, mute/unmute, mark round-trip, conference join/leave+fan-out, park/retrieve, bot-hold/resume, **barge_in_confirm** path (bundled only rejects), silence/dead-air, rtp_stats field shapes incl. negative `tx_packets_lost_reported`, protocol_error triggers, audio_format wrong-size, server_too_slow, set_recording_consent no-reply, optional start fields (srtp/verstat/retrieved/reconnected/trace_context).

## §2. Admin API & RBAC

Admin API on `127.0.0.1:9092` only (the observability port 404s `/admin/*` since 0.10.0). Flow: authenticate bearer → `min_role(method,path)` → dispatch. Bodies JSON. Handler-layer `503` = subsystem `Option` is `None`.

### 2.1 RBAC matrix — **P0** (security-critical)
Roles nest `ReadOnly < Operator < Admin`. Wire strings lowercase, case-**sensitive** (`"Admin"`→config load fail). For **each** endpoint, test all three roles + no-token + bad-token:
| ID | Case | Expect |
|---|---|---|
| ADM-RBAC-01 | no `Authorization` header | `401` + `WWW-Authenticate: Bearer` |
| ADM-RBAC-02 | bad/revoked token | `401` (constant-time compare — no timing leak of which token) |
| ADM-RBAC-03 | readonly token → GET list routes | `200`; → hangup/park/originate | `403 {required,have}` |
| ADM-RBAC-04 | operator token → hangup/park/conference | `200/202`; → originate/PUT log/hep test | `403` |
| ADM-RBAC-05 | admin token → originate/PUT log/hep | `200/202`; also can do all operator/readonly | ✅ |
| ADM-RBAC-06 **P1** | **rotated/revoked token still works until process restart** (`[admin]` not SIGHUP-reloaded) | document + verify — operational footgun |
| ADM-RBAC-07 | unknown `/admin/*` path with valid token | `404 {error:"unknown admin route"}` (route map not leaked to anon → 401 first) |

### 2.2 Endpoint contract tests
Legend: role in brackets. ✅ = already verified this engagement.
| ID | Endpoint | Pri | Setup | Key assertions (success + errors) |
|---|---|---|---|---|
| ADM-01 | GET `/admin/calls` [ro] | P0 | 🔵 | `{count,calls:[{call_id,sip_call_id,direction}]}` — **both id namespaces + direction present** (#311 regression: NOT a bare SIP-ID array); empty→`count:0` |
| ADM-02 | GET `/admin/registrations` [ro] | P1 | 🟣 | rows `{name,server_addr,status,last_attempt_at,expires_at,last_error}` |
| ADM-03 | GET/PUT `/admin/log` [ro/admin] | P1 | 🟢 | GET `{filter}`; PUT raw directive or `{"filter":…}`→`{filter,previous}`; **400** empty/whitespace/non-UTF8/invalid directive; `previous` enables clean revert |
| ADM-04 | GET `/admin/v1/conferences` [ro] | P1 | 🟢 | ✅ list; **501** when `[conference].enabled=false` |
| ADM-05 | GET `/admin/v1/parked` [ro] | P1 | 🟢 | `{count,parked:[{call_id,slot?,parked_secs}]}`; **501** park off |
| ADM-06 | GET `/admin/v1/drain` [ro] | P1 | 🟢 | `{draining,active_calls,drain_timeout_secs,remaining_secs}`; poll during SIGTERM |
| ADM-07 | GET `/admin/v1/calls/:id/stats` [ro] | P1 | 🔵 | flattened quality block; **400** empty id; **404** no active call w/ that **bridge** id (ended calls → CDR/quality, not here) |
| ADM-08 | POST `/admin/calls/:id/hangup` [op] | P1 | 🔵 | `:id`=**SIP** Call-ID; `200 {shutdown_signalled:true}`; **404 {shutdown_signalled:false}** on no-match; **passing bridge call_id → 404** (common mistake — test it) |
| ADM-09 | POST `/admin/v1/registrations/:name/{refresh,restart}` [op] | P1 | 🟣 | `202 {accepted,action,registration}`; **404** unknown name; **409** draining |
| ADM-10 | POST `/admin/v1/conferences` [op] | P1 | 🟢 | ✅ `201 {room_id}`; empty body→generated id; **400** bad sample_rate; **409** RoomExists; **503** max_rooms |
| ADM-11 | DELETE `/admin/v1/conferences/:id` [op] | P1 | 🟢 | ✅ `200 {ended:true}`; **404** RoomNotFound; **501** disabled |
| ADM-12 | POST/DELETE `/admin/v1/conferences/:id/participants[/:cid]` [op] | P1 | 🟢/🟣 | ✅ add/remove `202` (dispatched); **404** UnknownCall (msg reminds bridge-id vs sip); fan-out on other legs needs 🟣 |
| ADM-13 | POST `/admin/v1/calls/:id/park` [op] | P1 | 🔵 | `202 {call_id}`; **404** unknown; **max_parked NOT enforced here** → surfaces as WS `park_failed`, call continues (test this asymmetry) |
| ADM-14 | POST `/admin/v1/calls/:id/retrieve` [op] | P1 | 🔵 | `202`; **409 NotParked**; **404** unknown; **501** park off |
| ADM-15 | POST `/admin/hep/test` [admin] | P2 | 🟠 | `200 {emitted:true,correlation_id:"admin-probe"}`; **503** `[hep]` off; verify chunk-100 in Homer |
| ADM-16 | POST `/admin/v1/calls` (originate) [admin] | P0 | 🟢 | ✅ `202 {call_id}`; full rejection→status matrix: **400** NoWsUrl/BadTarget/BadRecording/BadFrom/bad JSON, **404** UnknownGateway, **503** AtCapacity, **429** RateLimited, **501** outbound disabled |

### 2.3 Metric & audit side-effects — **P1**
- ADM-MET-01: every admin call increments `siphon_ai_admin_requests_total{endpoint,role,result}`; `result` reflects status (matched-handler 404→`not_found`; 400/409/429/501/503→`error`); `endpoint` always a **bounded template** (`:id` collapsed), `"unknown"` for unrecognized → dashboards key `endpoint="unknown"` as a probe signal.
- ADM-MET-02 **P1 invariant**: every served route must have a non-`"unknown"` label AND an explicit `min_role` arm (`every_served_route_has_a_bounded_label` test) — a missing arm silently defaults ReadOnly. Worth a live probe: hit each route, assert no `endpoint="unknown"` and correct role enforcement.
- ADM-AUD-01: with `[audit]` on, every admin call emits an `admin_request` audit event (401/403/2xx/404) with peer/actor/role/endpoint/status/result (+`required_role` on 403). See §4.5.

## §3. Call features & lifecycle

**Cross-cutting:** conference/park/retrieve/hold APIs use the **bridge** `call_id` (`siphon-…`), not `sip_call_id` (rejected). Fail-closed features needing explicit enable: outbound, conference, park, recording(≠off). Hold has **no** enable flag. Dialplan + registration always active.

### 3.1 Route matching / dialplan — **P1**, mostly 🔵 (needs inbound) + `route-test` CLI (🟢)
Much of this is testable **offline** via `siphon-ai route-test` (no call). Match keys (AND within route): `request_uri_user/host`, `to_user/host`, `from_user/host`, `register_source` (or `"trunk"`), `header.<NAME>`, `any`.
| ID | Case | Expect |
|---|---|---|
| RT-01 | `route-test` known number | winning route + effective ws_url/codecs; override vs `[bridge]` default |
| RT-02 | unmatched | `NO MATCH → SIP 404` |
| RT-03 | first-match-wins ordering | earlier route wins even if later also matches |
| RT-04 | `regex=true` per-route | substring (unanchored) match on every value; literal+regex can't mix |
| RT-05 | `header.X` match; absent header | absent matches empty-string |
| RT-06 | AND semantics | all keys in a route must match |
| RT-07 **P1** | config-load negatives | empty match block, `any`+keys, bad regex, dup header, dup name, bad register_source ref → **exit 1** |
| RT-08 | missing/non-last default route | startup **warning**; routes below unreachable |
| RT-09 | live inbound routing 🔵 | matched route's ws_url gets the session; `route` in CDR/metrics/logs |

### 3.2 Inbound registration — **P1**, 🟣 (needs PBX)
| ID | Case | Expect |
|---|---|---|
| REG-01 | initial REGISTER + digest retry | reaches `registered`; `register_state{name,state}=1`, `register_attempts_total{outcome=registered}` |
| REG-02 | refresh at expires−60s | periodic re-REGISTER |
| REG-03 | failure backoff 5→10→20…cap300 | `failed` state, `register_state_changed` webhook w/ `last_error` |
| REG-04 | `POST …/refresh` (op) | off-cycle REGISTER, backoff reset, `202`; **404** unknown; **409** draining |
| REG-05 | `POST …/restart` (op) | Expires:0 then fresh REGISTER |
| REG-06 | `register_on_startup=false` | parked/`disabled`; only startable via refresh |
| REG-07 | wildcard `[sip].listen` w/o `public_address` | config-load reject |
| REG-08 | hostname in `server` | rejected at load (IP-only) |

### 3.3 Outbound origination — **P0**, 🟢 (our main lever) except SRTP/2P
| ID | Case | Pri | Setup | Expect |
|---|---|---|---|---|
| OB-01 ✅ | basic originate → answer → BYE | P0 | 🟢 | `202`; `outbound_initiated`→`outbound_answered`→`call_end`+CDR; `caller_hangup` |
| OB-02 | `delayed_offer:true` (RFC 3264) | P1 | 🟢 | INVITE w/o SDP, answer peer offer in ACK; `delayed_offer_total{result}`; failure causes on CDR (`ack_timeout`/`missing_sdp_answer`/…) |
| OB-03 | `from` caller-ID override | P1 | 🟢 | INVITE From set; **400** malformed URI |
| OB-04 | SRTP `required` vs plaintext answer | P1 | 🟠 | call **fails** (`outbound_calls_total{failed}`, `outbound_srtp_total{downgraded}`); `required`+encrypted→`start.srtp{exchange:sdes}`, `outbound_srtp_total{encrypted}` |
| OB-05 | unanswered outcomes | P1 | 🟢 | `outbound_failed{cause}` busy/declined/no_answer/rejected/unreachable — **terminal, NO CDR** (assert no CDR line) |
| OB-06 | limits | P1 | 🟢 | `503` at `max_concurrent`; `429` at `rate_limit_per_sec` |
| OB-07 | no early media | P1 | 🟢 | WS `start` only at answer; bot must speak first |

### 3.4 Transfer (REFER) + Hold — **P1**, 🟣 (needs 2nd endpoint)
| ID | Case | Setup | Expect |
|---|---|---|---|
| XF-01 | blind `transfer{target}` | 🟣 | 2xx→BYE→`stop{transfer}`+close; `transfers_total{blind,accepted}` |
| XF-02 | blind bad URI / dialog gone | 🔵 | `error{transfer_failed}`, call continues |
| XF-03 | attended `transfer{replaces_call_id}` | 🟣+outbound | consult leg via originate; REFER w/ Replaces; consult taken over; `stop{transfer}` |
| XF-04 | attended bad replaces_call_id | 🟢 | `transfer_failed`, call continues |
| XF-05 | transfer unconfigured | 🟢 | `error{transfer_failed}` "not configured" |
| HOLD-01 | `hold`/`resume` (bot-initiated) | 🔵 | `held`/`resumed` after re-INVITE 2xx; MOH to caller; `holds_total{ok}`; CDR `hold{count,total_ms}` |
| HOLD-02 | double-hold / resume-not-held | 🔵 | idempotent no-op, call survives |
| HOLD-03 | hold while in conference / peer-held | 🔵 | rejected `hold_failed`, no stacking |
| HOLD-04 | 491 glare | 🟣 | backoff 2.1–4.0s, retry once, else `hold_failed` stays sendrecv |
| HOLD-05 | peer-initiated hold | 🔵 | `BridgeOut::hold{direction}` / `resume` (distinct from `held`/`resumed`) |

### 3.5 Park / retrieve — **P1**, 🔵 (needs inbound + a WS endpoint; no 2nd phone)
`[park].enabled` required. Config: `moh_file` (load-validated), `timeout_secs`, `timeout_action` hangup/keep, `max_parked`.
| ID | Case | Expect |
|---|---|---|
| PK-01 | WS `park` | `stop{park}`+close, call lives (SIP/RTP up, MOH); `parks_total{ok}`, `parked_calls_active`+1, `call_parked{slot?}` webhook, CDR `park{count,total_ms}` |
| PK-02 | operator `POST …/park` | `202`; **404** unknown |
| PK-03 | `max_parked` exceeded | WS `park_failed`, call continues (NOT admin 503) |
| PK-04 | retrieve `POST …/retrieve` | fresh WS `start{retrieved:true}`, seq 0, **no replay**; `retrieves_total{ok}`, `call_retrieved{ws_url}` |
| PK-05 | retrieve not-parked / unknown | `409 NotParked` / `404` |
| PK-06 | park timeout → hangup | `park_timeout{action:hangup}` webhook then teardown+CDR |
| PK-07 | park timeout → keep | stays parked, timer doesn't re-arm |
| PK-08 | repeated park/retrieve | works; recording keeps writing MOH |

### 3.6 Conferencing — **P1**, mostly ✅ single-member; 🟣 for 2+ members
| ID | Case | Setup | Status |
|---|---|---|---|
| CF-01 | join/leave (WS self-scoped) | 🔵 | ⬜ `conference_joined`/`conference_left{left}`; direct pair restored |
| CF-02 | admin add/remove/create/force-end | 🟢 | ✅ (DELETE room-end verified) |
| CF-03 | **participant fan-out** `participant_joined/left` | 🟣 | ⬜ needs 2 legs |
| CF-04 | **mix-minus-self** correctness (hear others not self) | 🟣 | ⬜ needs 2 real callers |
| CF-05 | sample-rate lock / `rate_mismatch` | 🟣 | ⬜ two legs at 8k vs 16k → 2nd rejected |
| CF-06 | caps: `max_rooms`(503), `room_full`, `already_joined` | 🟢/🟣 | ⬜ join-result metric variants |
| CF-07 | recording = room mix on bot channel | 🟢 | ✅ (single-member continuity verified) |

### 3.7 Recording — **P1**, ✅ core; encryption/storage need setup
| ID | Case | Setup | Status |
|---|---|---|---|
| RC-01 | `mode=always` inbound / per-originate `recording:always` outbound | 🟢 | ✅ stereo WAV L=caller R=bot, `recording_*` CDR |
| RC-02 | on-demand WS `start/stop/pause/resume_recording` | 🔵 | ⬜ pause **drops** span (PCI); events; no-op on already-recording |
| RC-03 | override precedence route > gateway > originate | 🟢 | ⬜ confirm strict replace |
| RC-04 | `format="opus"` | 🟢 | ⬜ Ogg-Opus, ~10× smaller, plays in ffmpeg/VLC |
| RC-05 | encryption `.wava` + `decrypt-recording` round-trip | 🟠 | ⬜ KEK or KMS(LocalStack); `recording_encrypted:true`; wrong key_id errors |
| RC-06 | consent announcement (fail-closed) | 🟢 | ⬜ prompt plays, capture starts after; CDR `consent{announced,announcement_ms}`; unplayable→not recorded |
| RC-07 | `set_recording_consent` | 🔵 | ⬜ CDR `consent.server` |
| RC-08 | object storage upload | 🟠 | ⬜ MinIO/LocalStack; `recording_url` s3://, `recording_uploaded{url,size_bytes}` after call_end; spool durable across restart |
| RC-09 | writer backpressure | 🟢 | ⬜ `recordings_total{degraded}` on channel-full, call never stalls |
| RC-10 | `.part`→final atomic finalize | 🟢 | ✅ observed (finalize race) |

### 3.8 Graceful shutdown / drain — **P1**, 🔵 (needs a live call during SIGTERM)
| ID | Case | Expect |
|---|---|---|
| DR-01 | SIGTERM with active call, finishes in window | clean drain, no forced teardown; `draining=1`, `/ready`→503 |
| DR-02 | new out-of-dialog INVITE during drain | `503 + Retry-After`; `invite_admission`/audit `invite_rejected{draining}` |
| DR-03 | straggler past deadline | `mark_drain_forced`→WS `stop`+outbound BYE; CDR `drain_forced`; `calls_drain_forced_total`+1; `drain_seconds` histogram |
| DR-04 | second SIGTERM during drain | immediate teardown |
| DR-05 | `GET /admin/v1/drain` polling | `remaining_secs` counts down; `active_calls` decreases |
| DR-06 | admin registration refresh during drain | `409` |

## §4. Observability (CDR / webhooks / metrics / HEP / audit / quality)

### 4.1 CDR — **P0** (billing-grade). Current `version=5`.
| ID | Case | Setup | Status |
|---|---|---|---|
| CDR-01 | JSONL: all field groups populate correctly | 🟢 | ✅ identity/routing/audio/termination/quality + `answered_at`, `recording_*` |
| CDR-02 | CSV: 49-col header + row alignment | 🟢 | ✅ verified |
| CDR-03 | `answered_at` None when never connected (unanswered outbound writes **no** CDR though — cross-check) | 🟢 | ⬜ inbound-rejected vs outbound-failed distinction |
| CDR-04 | `termination.cause` full enum coverage | mixed | 🟡 `caller_hangup`✅; ⬜ `server_hangup`, `local_shutdown` (CANCEL/session-timer), `drain_forced`, `bridge_ended`, `tap_ended`, delayed-offer failures (`ack_timeout` etc.) |
| CDR-05 | quality block: omitted-not-zeroed for unmeasured | 🟢 | ⬜ assert absent fields vs 0; `tx_packets_lost_reported` signed (may be negative) |
| CDR-06 | `park`/`hold`/`reconnect` blocks present only when episode occurred | 🔵 | ⬜ |
| CDR-07 | verstat fields only when STIR/SHAKEN on | 🔵 | ⬜ |
| CDR-08 | CSV vs JSONL field parity | 🟢 | ⬜ same record both formats, diff |
| CDR-09 | HEP `Cdr` chunk (0x65) mirrors CDR at call end | 🟠 | ⬜ Homer |

### 4.2 Webhooks — **P1**. `version=1`, 12 event types. **Widen box allowlist to all (or empty=all) for full coverage.**
| ID | Event | Setup | Status |
|---|---|---|---|
| WH-01 | `call_start` / `call_end` (pair 1:1, `duration_ms`,`termination_cause`) | 🔵 | ⬜ inbound |
| WH-02 | `outbound_initiated`→`outbound_answered`→`call_end` | 🟢 | ⬜ |
| WH-03 | `outbound_failed{cause}` — terminal, no call_end | 🟢 | ⬜ |
| WH-04 | `registration_state_changed` (non-call-scoped, `previous_status`) | 🟣 | ⬜ |
| WH-05 | `conference_created`/`conference_ended{duration_ms,peak_participants}` | 🟢 | ✅ |
| WH-06 | `call_parked`/`call_retrieved`/`park_timeout{action}` | 🔵 | ⬜ |
| WH-07 | `recording_uploaded{url,size_bytes}` (after call_end) | 🟠 | ⬜ |
| WH-08 | delivery mechanics: `X-SiphonAI-Event-Id` idempotency, HMAC `X-SiphonAI-Signature` (with `secret`), `spool_dir` durability across restart, `retry_max`/`timeout` | 🟢 | 🟡 idempotency+unsigned verified; ⬜ HMAC + spool |
| WH-09 | allowlist filter: empty=all; named subset; **all 12 names valid** (not just call_*) | 🟢 | 🟡 conf names verified; **doc says only call_start/call_end — stale (see §5 bug list)** |
| WH-10 | delivery metrics `webhook_deliveries_total{sink,result}`, `webhook_delivery_seconds` | 🟢 | ✅ lifecycle |

### 4.3 Metrics — **P1**. Scrape `GET :9091/metrics` (Prometheus v0.0.4).
| ID | Case | Assert |
|---|---|---|
| MET-01 | counters increment on labeled paths | `invites_total`, `calls_total{cause}`, `outbound_calls_total{result}`, `route_match_total{route}`, `conference_joins_total{result}`, `transfers_total`, `parks_total`, `holds_total`, `config_reloads_total{result}`, `webhook_*`, `barge_in_decisions_total` |
| MET-02 | gauges track live state | `calls_active`, `outbound_calls_active`, `conferences_active`, `conference_participants` (2/member), `parked_calls_active`, `register_state{name,state}`, `draining` |
| MET-03 | histograms render as `_bucket{le}` w/ explicit buckets | `ws_connect_seconds`, `call_duration_seconds`, `rtp_rtt_ms` (ms!), `sdp_negotiate_seconds`, `drain_seconds`, `webhook_delivery_seconds`, `room_tick_lag_seconds`, `barge_in_decision_seconds` |
| MET-04 **P1** | **cardinality**: no `call_id` label anywhere; `route`/causes/results bounded | grep `/metrics` for any unbounded label |
| MET-05 | `metrics_token` gate (0.35.0) | with token set: `/metrics` needs bearer (401+WWW-Authenticate), `/health`+`/ready` never gated |

### 4.4 HEP / Homer — **P2**, 🟠 (needs Homer; `examples/homer-stack/`)
| ID | Chunk | Assert in Homer |
|---|---|---|
| HEP-01 | `Sip` (0x01) from siphon-rs | SIP ladder keyed by Call-ID |
| HEP-02 | `Rtcp` (0x05) + `RtpQos` (vendor 0x20) from forge | QoS panel: jitter/loss/ssrc when RTP flows |
| HEP-03 | `Log` (0x64) app lifecycle | timeline call_started/ended |
| HEP-04 | `Cdr` (0x65) at call end | full CDR JSON inspectable |
| HEP-05 | `Verstat` (0x66) per inbound call when verify on | verdict threaded on call view |
| HEP-06 | best-effort | `siphon_ai_hep_collector_up=0` when unreachable, call unaffected; `/admin/hep/test` probe |

### 4.5 Audit `[audit]` — **P1**, 🟢 (admin_request testable now). OFF by default; enabling from off = **restart-required**.
| ID | Event | Trigger |
|---|---|---|
| AUD-01 | `admin_request` (peer,actor,role,endpoint,status,result,required_role on 403) | every admin call incl. 401/403/404 |
| AUD-02 | `sip_auth` (failed/stale only) | 🔵/🟣 bad digest |
| AUD-03 | `invite_rejected` (rate_limited/no_trunk/draining) | 🔵 |
| AUD-04 | `attestation_rejected` | 🔵 STIR/SHAKEN |
| AUD-05 | `config_reload` (applied/failed, restart_required[]) | SIGHUP |
| AUD-06 | `cert_reload` (admin_tls/sip_tls) | 🟠 TLS cert swap |
| AUD-07 | sink HMAC signature `X-SiphonAI-Signature: t=…,v1=…`, spool | 🟢 |

### 4.6 Quality history `[quality]` — **P1**, 🟢. OFF default, **restart-required**.
| ID | Case | Assert |
|---|---|---|
| QH-01 | `interval` records every `interval_secs` (cumulative-since-start) | tail `[quality.file]` JSONL; `quality_records_total{kind=interval}` |
| QH-02 | `final` record == CDR quality block field-for-field | diff QH final vs CDR |
| QH-03 | per-call `seq` monotonic from 0 | assert |
| QH-04 | live probe `GET /admin/v1/calls/:id/stats` matches tracker | 🔵 during call |

### 4.7 Health / readiness — **P0**, 🟢
| ID | Case | Assert |
|---|---|---|
| RDY-01 | `/health` always `200 ok` | ✅ |
| RDY-02 | `/ready` `200 ready` when SIP bound; `503 not ready` before/ during drain | ⬜ check 503 during SIGTERM |
| RDY-03 | probes never gated even with `metrics_token` set | ⬜ |

## §5. Config & CLI

### 5.1 CLI subcommands — **P1**, 🟢 (all offline, need only config read via sudo)
| ID | Subcommand | Assert |
|---|---|---|
| CLI-01 | `check` | valid→exit 0 + summary (node/listen/routes/default-route/subsystems); invalid→exit 1 + `config INVALID:` stderr; **missing default route → exit 0 + warning** (not error). Preflight: `sudo bash -c 'set -a; . /etc/siphon-ai/env; siphon-ai --config … check'` |
| CLI-02 | `print-config` | secrets `<redacted>` by default; `--show-secrets` reveals; `--format json` parses w/ `jq`; per-route keys only when overridden; NOT reloadable as config |
| CLI-03 | `route-test` | see RT-01..06; `--ruri-user` defaults to `--to`; malformed `-H` → `bad --header` |
| CLI-04 | `decrypt-recording` | 🟠 KEK round-trip; wrong key_id error names it; out==in error; `.wava.part` needs `--allow-unfinalized`; both/neither key modes error; KMS via `--kms-endpoint` (LocalStack); missing AWS creds error. **No daemon config needed.** |

### 5.2 Config validation (load-time, fail-loud) — **P1**, 🟢
Every "required/fatal/must-be/unknown→fatal" is a **negative test**: edit config to the bad state, run `check`, assert exit 1 + specific stderr. High-value cases (from the config inventory):
- `${VAR}` expansion: unset-no-default fails; `${file:/abs}` trims exactly trailing newline; `${file:-x}` parses as env-with-default not file; missing `${cred:NAME}` / `$CREDENTIALS_DIRECTORY` fails.
- `[sip]` wildcard listen without `[node].public_address` → fail.
- `[sip.tls]` key group/world-readable → refuse; TLS transport without `[sip.tls]` → fail.
- routes: empty match / `any`+keys / bad regex / dup header / dup name / bad register_source ref → fail (RT-07).
- `[[trunk]]` with neither peer_addrs nor from_hosts → fail; 0 trunks = accept-any (dev footgun — verify warning).
- `[outbound]` enabled but no gateway / bad gateway (`from` missing `sip:`, half-set creds, unknown register ref) → fail.
- `[security].min_attestation != none` without `stir_shaken.enabled` → fail.
- `[recording]` mode≠off without `dir` → fail; encryption enabled without exactly one of kek/kms → fail; announcement file missing → fail.
- `[audit]`/`[quality]` enabled with no sub-sink → fail. `[cdr]` sub-sink enabled under master off → **warn** (0.40.0, ✅ verified).
- `[shutdown].drain_timeout_secs` must be ≤ systemd `TimeoutStopSec` (operational, not load-checked — document).

### 5.3 Config reload (SIGHUP) — **P1**, 🟢/🔵
| ID | Case | Assert |
|---|---|---|
| CFG-01 | hot-reloadable sections | routes, `[webhooks]`/`[cdr]` sinks (no active spool), `[audit]` (if on at boot), `[[gateway]]` (limits unchanged), `[sip.tls]`/`[admin.tls]` cert → apply live, no call drop; `config_reloads_total{applied}`; `cert_reload`/`config_reload` audit |
| CFG-02 | restart-required sections | edit + SIGHUP → `warn` "require a restart" naming section; running config unchanged; `config_reloads_total` |
| CFG-03 | spool_dir active downgrades sink to restart | assert warn |
| CFG-04 | bad reload keeps running config | `config_reloads_total{failed}`, no crash |
| CFG-05 | `[admin]` token table NOT reloaded | rotated token still works until restart (ADM-RBAC-06) |

## §6. Prioritization, bug-hunt candidates & setup gaps

### 6.1 Priority tiers (recommended execution order)
- **P0 — do first (core call path + billing data + security):** §1.3/1.4/1.5/1.6 (audio framing, start, seq, protocol_error), §2.1 RBAC matrix, §2 ADM-01/ADM-16, §3.3 OB-01/05/06, §4.1 CDR (esp. CDR-04 termination coverage, CDR-05 quality shapes), §4.7 health/ready.
- **P1 — major features:** everything in §1.1/1.2 gap scenarios, §3.4–3.8 (transfer/hold/park/conference/recording/drain), §4.2–4.6 (webhooks/metrics/audit/quality), §5 (CLI/config validation/reload).
- **P2 — edges/hardening:** WS-ERR-03/06, HEP (§4.4), admission/DoS gates, STIR/SHAKEN paths, KMS/object-storage.

### 6.2 Bug-hunt candidates surfaced during surface mapping (verify these specifically)
1. **`error{codec_unsupported}` must never appear on the wire** — enum variant exists (`protocol.rs:649`) but PROTOCOL.md §3.10 says codec failure is SIP 488 pre-WS. (WS-ERR-03)
2. **`[webhooks].events` doc is stale** — `docs/CONFIG.md` + `raw.rs` doc comment say "valid values today: call_start, call_end", but the filter matches all 12 `type_str()` values generically (verified live for conference events). Doc fix candidate.
3. **`admin hangup` id-namespace footgun** — takes `sip_call_id`; passing the bridge `call_id` silently 404s. Conversely conference/park take bridge `call_id` and reject `sip_call_id`. Assert error messages guide correctly.
4. **`max_parked` asymmetry** — enforced in the call controller (WS `park_failed`), NOT at `POST …/park` (no 503). Confirm and document.
5. **Rotated/revoked admin token works until restart** — `[admin]` not SIGHUP-reloaded. Security-relevant; verify + document.
6. **`route_label`/`min_role`/`dispatch` must stay in lockstep** — a served route missing a `min_role` arm silently defaults ReadOnly. Probe every route for `endpoint="unknown"` and correct role.
7. **`tx_packets_lost_reported` is signed** (i64/CDR, may be negative from duplicates) — assert consumers/CSV don't clamp. (RtpStats + CDR-05)
8. **HEP diagnostic metrics** (`siphon_ai_hep_collector_up/packets_sent/dropped`) are emitted by upstream crates, not this repo's `metrics.rs` — confirm they actually appear on `/metrics` when `[hep]` is on (doc vs reality).
9. **`quality_records_total`** emitted from `crates/quality/facade.rs`, not declared in `metrics.rs` — confirm it registers.
10. **`pacing_slack_frames`** default 25 in code vs "500 ms" doc text — cosmetic, confirm consistent.

### 6.3 Setup gaps — what's blocked and what unblocks it
| Capability | Unblocks | How to provide |
|---|---|---|
| 🔵 **Inbound call path** | most CDR/webhook lifecycle, hold, park, on-demand recording, live routing, audit sip_auth/invite_rejected | a softphone (Linphone) registering/calling in, or SIPp UAC against `:5060`; simplest big unlock |
| 🟣 **2nd endpoint / SIPp legs** | conference mixing + fan-out, blind/attended transfer, glare, rate-mismatch | SIPp scenarios in `test-harness/sipp-scenarios/` (`outbound_uas_answer.xml` etc.) — no telco cost |
| 🟣 **PBX/registrar** | registration lifecycle, register_source routing | Asterisk/PJSIP or FreeSWITCH container (interop lab notes in `test-harness/interop/`) |
| 🟠 **Object storage** | recording upload, `recording_uploaded`, spool | MinIO or LocalStack container |
| 🟠 **Homer stack** | all HEP/QoS verification | `examples/homer-stack/` docker-compose |
| 🟠 **TLS/SRTP peer, KMS** | SRTP required/downgrade, `.wava` KMS, cert reload | Twilio (have) for SRTP; LocalStack for KMS |
| 🟢 **Custom driver WS server** | arbitrary BridgeIn commands (transfer/hold/park/recording ctl/conference from the server side), start-deadline, malformed-msg | small Python/Node WS server we script — **high leverage, cheap**; or use **protocol-testkit** which drives the daemon directly |

### 6.4 Highest-leverage next moves
1. **Run protocol-testkit** bundled scenarios against the echo server (unlocks most of §1 with zero telco cost), then author the gap scenarios (§1.7).
2. **Stand up a SIPp inbound + UAS-answer harness** (`test-harness/sipp-scenarios/`) — unlocks 🔵 and much of 🟣 without a second phone or money.
3. **Widen the webhook allowlist to all events** + point at a durable receiver — unlocks §4.2 fully.
4. **Write a scriptable driver WS server** — unlocks server→bridge command coverage (transfer/hold/park/recording/conference) end-to-end.

---

_End of plan. Status legend in §0.5. Update the ✅/🟡/⬜ marks as cases are executed._
