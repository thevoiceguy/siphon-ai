/**
 * Corpus + schema conformance (design D2): every PROTOCOL.md
 * daemon→server example parses typed; every command shape the SDK emits
 * validates against `schemas/siphon-ai.v1.json` `$defs/BridgeIn`.
 */

import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import path from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";

import { Ajv2020 } from "ajv/dist/2020.js";

import { KNOWN_EVENT_TYPES, parseEvent } from "../src/events.js";

const repo = path.resolve(
  path.dirname(fileURLToPath(import.meta.url)),
  "../../../..",
);
const schema = JSON.parse(
  readFileSync(path.join(repo, "schemas/siphon-ai.v1.json"), "utf8"),
);
const protocolMd = readFileSync(path.join(repo, "docs/PROTOCOL.md"), "utf8");

function knownTypes(union: "BridgeOut" | "BridgeIn"): Set<string> {
  return new Set(
    schema.$defs[union].oneOf.map(
      (v: { properties: { type: { const: string } } }) =>
        v.properties.type.const,
    ),
  );
}

const bridgeOutTypes = knownTypes("BridgeOut");
const bridgeInTypes = knownTypes("BridgeIn");
const ambiguous = new Set(
  [...bridgeOutTypes].filter((t) => bridgeInTypes.has(t)),
);

function corpus(): Record<string, unknown>[] {
  const objects: Record<string, unknown>[] = [];
  for (const match of protocolMd.matchAll(/```json\n([\s\S]*?)```/g)) {
    const text = match[1];
    let idx = 0;
    while (idx < text.length) {
      const brace = text.indexOf("{", idx);
      if (brace === -1) break;
      // Greedy parse: find the shortest prefix that is valid JSON.
      let end = brace;
      let depth = 0;
      let inString = false;
      let escaped = false;
      for (; end < text.length; end++) {
        const ch = text[end];
        if (inString) {
          if (escaped) escaped = false;
          else if (ch === "\\") escaped = true;
          else if (ch === '"') inString = false;
          continue;
        }
        if (ch === '"') inString = true;
        else if (ch === "{") depth++;
        else if (ch === "}" && --depth === 0) break;
      }
      if (depth !== 0) break;
      try {
        objects.push(JSON.parse(text.slice(brace, end + 1)));
      } catch {
        // not a standalone JSON object (fragment) — skip
      }
      idx = end + 1;
    }
  }
  return objects;
}

test("SDK knows every BridgeOut discriminator", () => {
  assert.deepEqual(new Set(KNOWN_EVENT_TYPES), bridgeOutTypes);
});

test("every documented daemon→server example parses typed", () => {
  const seen = new Set<string>();
  for (const obj of corpus()) {
    const t = obj.type as string | undefined;
    if (t === undefined || !bridgeOutTypes.has(t)) continue;
    if (ambiguous.has(t) && !("seq" in obj)) continue; // BridgeIn form
    const event = parseEvent(JSON.stringify(obj));
    assert.notEqual(event.type, "unknown", `documented \`${t}\` must parse typed`);
    seen.add(t);
  }
  assert.ok(seen.size >= 15, `corpus only covered ${[...seen].sort()}`);
});

test("every command the SDK can emit validates against $defs/BridgeIn", () => {
  const ajv = new Ajv2020({ strict: false });
  const validate = ajv.compile({
    $ref: "#/$defs/BridgeIn",
    $defs: schema.$defs,
  });
  const commands: Record<string, unknown>[] = [
    { type: "clear" },
    { type: "mark", name: "greeting_done" },
    { type: "hangup", cause: "normal" },
    { type: "transfer", target: "sip:agent@pbx.example.com" },
    { type: "transfer", replaces_call_id: "siphon-abc" },
    { type: "send_dtmf", digit: "5", duration_ms: 160 },
    { type: "mute" },
    { type: "unmute" },
    { type: "start_recording" },
    { type: "stop_recording" },
    { type: "pause_recording" },
    { type: "resume_recording" },
    { type: "set_recording_consent", note: "dtmf-1" },
    { type: "park", slot: "vip" },
    { type: "conference_join", room_id: "support-7" },
    { type: "conference_leave" },
    { type: "hold" },
    { type: "resume" },
    { type: "barge_in_confirm" },
    { type: "barge_in_reject" },
  ];
  const emitted = new Set<string>();
  for (const msg of commands) {
    const wire = { call_id: "siphon-test", ...msg };
    assert.ok(
      validate(wire),
      `${msg.type} fails schema: ${JSON.stringify(validate.errors?.[0])}`,
    );
    emitted.add(msg.type as string);
  }
  assert.deepEqual(emitted, bridgeInTypes, "must exercise every BridgeIn command");
});

test("pause-mode barge-in shapes parse typed (0.32.0)", () => {
  // Pre-0.32.0 speech_started shape: decision fields simply absent.
  const plain = parseEvent(
    '{"type":"speech_started","call_id":"c","seq":4,"ts_ms":1234}',
  );
  assert.equal(plain.type, "speech_started");
  assert.ok(plain.type === "speech_started" && !plain.decision_pending);

  const armed = parseEvent(
    '{"type":"speech_started","call_id":"c","seq":5,"ts_ms":1300,"decision_pending":true,"decision_deadline_ms":500}',
  );
  assert.ok(armed.type === "speech_started");
  assert.equal(armed.decision_pending, true);
  assert.equal(armed.decision_deadline_ms, 500);

  for (const outcome of ["confirmed", "rejected", "timeout"]) {
    const resolved = parseEvent(
      `{"type":"barge_in_resolved","call_id":"c","seq":6,"outcome":"${outcome}"}`,
    );
    assert.ok(resolved.type === "barge_in_resolved");
    assert.equal(resolved.outcome, outcome);
  }
});

test("unknown type wraps, unknown fields pass through, malformed throws", () => {
  const unknown = parseEvent('{"type": "yodel", "call_id": "c"}');
  assert.equal(unknown.type, "unknown");

  const dtmf = parseEvent(
    '{"type":"dtmf","call_id":"c","seq":9,"digit":"1","duration_ms":160,"method":"rfc2833","novel":true}',
  );
  assert.equal(dtmf.type, "dtmf");

  assert.throws(() => parseEvent("{nope"));
  assert.throws(() => parseEvent('{"no_type": 1}'));
});
