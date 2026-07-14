#!/usr/bin/env node
/**
 * Reference echo WS server for the SiphonAI bridge protocol v1.
 *
 * Echoes every audio frame back to the caller — point a softphone at
 * SiphonAI and you hear yourself. Built on the **`siphon-ai-server` SDK**
 * (`sdks/typescript/`), so this file is the canonical example of writing
 * a SiphonAI bot server in Node with typed events instead of hand-rolled
 * wire code. The Python twin (`examples/echo-ws-server-python`) carries
 * the SIPp test-harness knobs; this one stays minimal.
 *
 * Run:
 *   (cd ../../sdks/typescript && npm install)   # one-time: build the SDK
 *   npm install
 *   node server.mjs --bind 0.0.0.0:8080
 */

import { parseArgs } from "node:util";

import { SiphonServer, SUBPROTOCOL } from "siphon-ai-server";

const { values: args } = parseArgs({
  options: {
    bind: { type: "string", default: "0.0.0.0:8080" },
    "auth-token": { type: "string" },
    "echo-marks": { type: "boolean", default: false },
    "delay-ms": { type: "string", default: "0" },
    help: { type: "boolean", short: "h", default: false },
  },
});

if (args.help) {
  console.log(`usage: node server.mjs [options]

  --bind HOST:PORT   address to listen on (default: 0.0.0.0:8080)
  --auth-token TOK   require Authorization: Bearer <TOK> on the upgrade
  --echo-marks       send a \`mark\` event back after \`start\`
                     (used by protocol smoke tests)
  --delay-ms MS      echo each audio frame back after this many ms`);
  process.exit(0);
}

const [host, port] = args.bind.split(":");
if (!port) {
  console.error("--bind must be HOST:PORT");
  process.exit(2);
}
const delayMs = Number(args["delay-ms"]);
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

const server = new SiphonServer(
  async (call) => {
    const start = call.start;
    console.log(
      `start call_id=${start.call_id} version=${start.version}` +
        ` from=${start.from} to=${start.to}` +
        ` rate=${start.audio.sample_rate} ch=${start.audio.channels}` +
        ` frame_ms=${start.audio.frame_ms} sip_call_id=${start.sip.call_id}`,
    );
    if (start.reconnected) {
      console.log(`start call_id=${start.call_id} is a reconnected call`);
    }
    if (args["echo-marks"]) call.mark("echo_ready");

    let framesEchoed = 0;
    for await (const item of call) {
      if (item.type === "audio") {
        if (delayMs > 0) await sleep(delayMs);
        // Echo 1:1, mirroring the daemon's own 20 ms cadence — for
        // generated audio use call.sendAudio, the SDK's paced re-framer.
        call.sendAudioFrame(item.pcm);
        framesEchoed += 1;
      } else if (item.type === "speech_started" && item.decision_pending) {
        // Pause-mode barge-in arbitration (0.32.0). An echo server never
        // wants to stop echoing, so reject the barge-in — unless the
        // harness asks for a confirm via SIPHON_ECHO_BARGE_IN_VERDICT=confirm.
        const verdict =
          process.env.SIPHON_ECHO_BARGE_IN_VERDICT === "confirm"
            ? "confirm"
            : "reject";
        if (verdict === "confirm") call.bargeInConfirm();
        else call.bargeInReject();
        console.log(
          `speech_started call_id=${start.call_id} decision_pending` +
            ` deadline_ms=${item.decision_deadline_ms} verdict=${verdict}`,
        );
      } else if (item.type === "barge_in_resolved") {
        console.log(
          `barge_in_resolved call_id=${start.call_id} outcome=${item.outcome}`,
        );
      } else if (item.type === "stop") {
        console.log(`stop call_id=${start.call_id} reason=${item.reason}`);
        break;
      } else if (item.type === "unknown") {
        console.warn(`unknown text message type=${item.raw?.type}`);
      } else {
        console.log(`${item.type}:`, item);
      }
    }
    console.log(
      `done call_id=${start.call_id} frames_echoed=${framesEchoed}`,
    );
  },
  { host, port: Number(port), authToken: args["auth-token"] },
);

await server.listen();
console.log(
  `listening on ws://${host}:${port}  (subprotocol=${SUBPROTOCOL},` +
    ` auth=${args["auth-token"] ? "on" : "off"}, delay_ms=${delayMs})`,
);

for (const sig of ["SIGINT", "SIGTERM"]) {
  process.on(sig, async () => {
    console.log(`received ${sig}, shutting down`);
    await server.close();
    process.exit(0);
  });
}
