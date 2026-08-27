#!/usr/bin/env node
// The AI-side of the browser interop harness (DEV_PLAN_WebRTC.md §5,
// item 2): a siphon-ai.v1 WebSocket server that *measures* what the
// browser sent and *plays* something recognisably different back.
//
// An echo server proves audio moved; it cannot tell you which
// direction moved it. So this one is asymmetric by design:
//
//   browser --450 Hz--> siphon-ai --> here   (we assert 450 Hz arrived)
//   browser <--900 Hz-- siphon-ai <-- here   (the page asserts 900 Hz)
//
// Both frequencies are multiples of 50 Hz on purpose: a 20 ms window
// is exactly rate/50 samples, so the Goertzel bin spacing is 50 Hz and
// these land dead on a bin at both 8 kHz and 16 kHz. Off-bin tones
// (440/880) smear across neighbours and cost about a third of the
// discrimination for nothing.
//
// Both ends check *frequency*, not just level, so a stuck buffer, a
// half-duplex path, or a loopback that quietly echoes cannot pass.
//
// Usage:
//   node tone-ws-server.mjs --port 8769 --report /tmp/tone-report.json
//
// The report is written on SIGTERM/SIGINT and at exit; the runner
// reads it after the call.

import { writeFileSync } from "node:fs";
import { createRequire } from "node:module";

const require = createRequire(
  process.env.TONE_WS_REQUIRE_BASE ?? `${process.cwd()}/node_modules/`,
);
const { WebSocketServer } = require("ws");

const argv = process.argv.slice(2);
const arg = (name, fallback) => {
  const i = argv.indexOf(`--${name}`);
  return i >= 0 && argv[i + 1] ? argv[i + 1] : fallback;
};

const PORT = Number(arg("port", 8769));
const REPORT = arg("report", "");
/** What we play toward the browser. Distinct from the browser's tone. */
const TX_HZ = Number(arg("tx-hz", 900));
/** What we expect to hear from the browser. */
const RX_HZ = Number(arg("rx-hz", 450));

/** Per-call measurement, reported at exit. */
const calls = [];

/** Candidate tones, on the 50 Hz grid a 20 ms window resolves exactly. */
const GRID = Array.from({ length: 39 }, (_, i) => (i + 2) * 50); // 100–2000 Hz

/**
 * Goertzel: energy at one frequency, normalised against the frame's
 * total energy. Cheaper than an FFT and all we need — "is the tone we
 * expect the thing that is actually here?"
 */
function toneRatio(samples, hz, rate) {
  const n = samples.length;
  if (n === 0) return 0;
  const k = Math.round((n * hz) / rate);
  const w = (2 * Math.PI * k) / n;
  const cosine = Math.cos(w);
  const coeff = 2 * cosine;
  let s0 = 0;
  let s1 = 0;
  let s2 = 0;
  let energy = 0;
  for (let i = 0; i < n; i++) {
    const x = samples[i] / 32768;
    s0 = coeff * s1 - s2 + x;
    s2 = s1;
    s1 = s0;
    energy += x * x;
  }
  const power = s1 * s1 + s2 * s2 - coeff * s1 * s2;
  if (energy <= 1e-9) return 0;
  // Goertzel power is summed over the window; normalise the same way.
  return power / (energy * n * 0.5);
}

function toSamples(buf) {
  const out = new Int16Array(buf.length / 2);
  for (let i = 0; i < out.length; i++) out[i] = buf.readInt16LE(i * 2);
  return out;
}

/**
 * The frequency carrying most of a frame's energy, and how much.
 *
 * Reported rather than merely checked because the browser gets to
 * decide what it sends: an engine that cannot run WebAudio falls back
 * to its fake capture device, whose tone is whatever that engine
 * picked. Measuring rather than assuming keeps every engine on the
 * same assertion — "the thing the page says it is sending is the thing
 * that arrived".
 */
function dominant(samples, rate) {
  let bestHz = 0;
  let bestRatio = 0;
  for (const hz of GRID) {
    if (hz >= rate / 2) break;
    const r = toneRatio(samples, hz, rate);
    if (r > bestRatio) {
      bestRatio = r;
      bestHz = hz;
    }
  }
  return { hz: bestHz, ratio: bestRatio };
}

function peak(samples) {
  let p = 0;
  for (const s of samples) p = Math.max(p, Math.abs(s));
  return p;
}

/** 20 ms of `hz` at `rate`, PCM16LE, as the protocol requires. */
function toneFrame(hz, rate, seq) {
  const n = rate / 50;
  const buf = Buffer.alloc(n * 2);
  for (let i = 0; i < n; i++) {
    const t = (seq * n + i) / rate;
    buf.writeInt16LE(Math.round(Math.sin(2 * Math.PI * hz * t) * 12000), i * 2);
  }
  return buf;
}

const wss = new WebSocketServer({
  host: "127.0.0.1",
  port: PORT,
  handleProtocols: () => "siphon-ai.v1",
});

wss.on("connection", (ws) => {
  const call = {
    call_id: null,
    sample_rate: null,
    rx_frames: 0,
    rx_bad_size: 0,
    rx_peak: 0,
    rx_tone_frames: 0,
    rx_tonal_frames: 0,
    rx_dominant_hz: 0,
    tx_frames: 0,
    started_at: new Date().toISOString(),
  };
  calls.push(call);
  let timer = null;
  let seq = 0;
  /** Dominant-frequency tally, so the report names one number. */
  const histogram = new Map();

  const startTx = (rate) => {
    if (timer) return;
    const t0 = process.hrtime.bigint();
    let n = 0;
    const tick = () => {
      if (ws.readyState !== 1) return;
      // Back-pressure guard: never queue more than ~200 ms.
      if (ws.bufferedAmount <= (rate / 50) * 2 * 10) {
        ws.send(toneFrame(TX_HZ, rate, seq++));
        call.tx_frames++;
      }
      n++;
      const targetMs = n * 20;
      const elapsedMs = Number(process.hrtime.bigint() - t0) / 1e6;
      timer = setTimeout(tick, Math.max(0, targetMs - elapsedMs));
    };
    timer = setTimeout(tick, 20);
  };

  ws.on("message", (data, isBinary) => {
    if (!isBinary) {
      let msg;
      try {
        msg = JSON.parse(data.toString());
      } catch {
        return;
      }
      if (msg.type === "start") {
        call.call_id = msg.call_id ?? null;
        call.sample_rate = msg.audio?.sample_rate ?? 16000;
        // Start playing immediately: PROTOCOL.md §3.1 gives the server
        // `server_start_deadline_secs` to produce its first frame.
        startTx(call.sample_rate);
      }
      if (msg.type === "stop") {
        try {
          ws.close();
        } catch {
          /* already closing */
        }
      }
      return;
    }

    const rate = call.sample_rate ?? 16000;
    const expected = (rate / 50) * 2;
    if (data.length !== expected) {
      call.rx_bad_size++;
      return;
    }
    const samples = toSamples(data);
    call.rx_frames++;
    const framePeak = peak(samples);
    call.rx_peak = Math.max(call.rx_peak, framePeak);
    if (framePeak <= 1000) return; // near-silence carries no tone
    // A frame counts as "the browser's tone" when most of its energy
    // sits at the frequency the page says it is generating…
    if (toneRatio(samples, RX_HZ, rate) > 0.5) call.rx_tone_frames++;
    // …and as *tonal* when its energy is concentrated anywhere at all,
    // which is what an engine using its fake capture device produces.
    const d = dominant(samples, rate);
    if (d.ratio > 0.5) {
      call.rx_tonal_frames++;
      histogram.set(d.hz, (histogram.get(d.hz) ?? 0) + 1);
      let bestHz = 0;
      let bestN = 0;
      for (const [hz, n] of histogram) {
        if (n > bestN) {
          bestN = n;
          bestHz = hz;
        }
      }
      call.rx_dominant_hz = bestHz;
    }
  });

  const finish = () => {
    if (timer) clearTimeout(timer);
    timer = null;
    call.ended_at = new Date().toISOString();
  };
  ws.on("close", finish);
  ws.on("error", finish);
});

function report() {
  return JSON.stringify({ tx_hz: TX_HZ, rx_hz: RX_HZ, calls }, null, 2);
}

// Written continuously, not just at exit: the test asserts on it while
// the call is still up, and a harness that has to kill the server to
// read its findings cannot tell "the call was fine" from "the server
// died early".
if (REPORT) {
  // Written once up front so a reader never races the first interval.
  writeFileSync(REPORT, report());
  setInterval(() => writeFileSync(REPORT, report()), 1000).unref();
}

for (const sig of ["SIGTERM", "SIGINT"]) {
  process.on(sig, () => {
    if (REPORT) writeFileSync(REPORT, report());
    console.log(report());
    process.exit(0);
  });
}

wss.on("listening", () =>
  console.log(
    `tone-ws-server on 127.0.0.1:${PORT} (tx ${TX_HZ} Hz, expecting ${RX_HZ} Hz)`,
  ),
);
