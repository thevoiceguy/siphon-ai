#!/usr/bin/env node
// ws_sink.mjs paces with setInterval(20), which in Node fires at >=20 ms and
// so under-runs realtime. That is invisible for CPU/RSS work but it lands
// directly on any tx-rate or clock-drift measurement (LOAD_TEST_PLAN §6.2),
// because the daemon can only play out what it is fed. This sink corrects
// against a monotonic origin instead, the same way the UAC's send loop does.
import { createRequire } from "node:module";
const require = createRequire(process.env.SINK_REQUIRE_BASE ?? "/opt/siphon-ai-src/examples/deepgram-llm-bot-node/node_modules/");
const { WebSocketServer } = require("ws");
const PORT = Number(process.argv[2] || 8769);
const SILENCE = Buffer.alloc(320);
const conns = new Set();
let tx = 0, rx = 0;
const wss = new WebSocketServer({ host: "127.0.0.1", port: PORT, handleProtocols: () => "siphon-ai.v1" });
wss.on("connection", (ws) => {
  conns.add(ws);
  ws.on("message", (d, isBin) => { if (isBin) rx++; });
  ws.on("close", () => conns.delete(ws));
  ws.on("error", () => conns.delete(ws));
});
const t0 = process.hrtime.bigint();
let n = 0;
function tick() {
  for (const ws of conns) { if (ws.readyState === 1 && ws.bufferedAmount <= 3200) { ws.send(SILENCE); tx++; } }
  n++;
  const targetMs = n * 20;
  const elapsedMs = Number(process.hrtime.bigint() - t0) / 1e6;
  setTimeout(tick, Math.max(0, targetMs - elapsedMs));
}
setTimeout(tick, 20);
setInterval(() => {
  const el = Number(process.hrtime.bigint() - t0) / 1e9;
  console.log(JSON.stringify({ conns: conns.size, ticks: n, tick_hz: +(n / el).toFixed(3), tx, rx }));
}, 30000);
wss.on("listening", () => console.log(`paced_sink on 127.0.0.1:${PORT}`));
