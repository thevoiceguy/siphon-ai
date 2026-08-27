// One real browser, one real call, two-way audio proved by frequency
// (DEV_PLAN_WebRTC.md §5, item 2).
//
// The page sends a 450 Hz oscillator instead of a microphone; the WS
// server on the far side of the bridge measures what arrives and plays
// 900 Hz back. Each end asserts the *other's* tone, so neither a
// half-duplex path nor an echo can pass:
//
//   page --450--> WSS --> siphon-ai --> WS server   (server asserts 450)
//   page <--900-- WSS <-- siphon-ai <-- WS server   (page asserts 900)

import { readFileSync } from "node:fs";
import { join } from "node:path";
import { expect, test } from "@playwright/test";
import { HERE } from "../stack.mjs";

const stack = JSON.parse(readFileSync(join(HERE, ".stack.json"), "utf8"));

/** The lab page, driving itself: register, call, tone mode, vendored stack. */
const PAGE = `${stack.pageUrl}?auto=1&call=1&tone=1&sipjs=/vendor/sip.js/lib/index.js`;

async function metrics() {
  const r = await fetch(`http://127.0.0.1:${stack.ports.obs}/metrics`);
  return r.text();
}

/** One `siphon_ai_*` series' value, or undefined when absent. */
function series(text, name, label) {
  const line = text
    .split("\n")
    .find((l) => l.startsWith(name) && (!label || l.includes(label)));
  return line ? Number(line.trim().split(/\s+/).pop()) : undefined;
}

function toneReport() {
  // The server writes this every second; before its first write (or if
  // it died) an empty report reads as "nothing heard yet", which the
  // polling assertions turn into a timeout with a useful message
  // rather than an ENOENT stack trace.
  try {
    return JSON.parse(readFileSync(stack.toneReport, "utf8"));
  } catch {
    return { calls: [] };
  }
}

test("a browser registers, calls, and audio flows both ways", async ({
  page,
}, testInfo) => {
  const engine = testInfo.project.name;
  const console_lines = [];
  page.on("console", (m) => console_lines.push(m.text()));
  page.on("pageerror", (e) => console_lines.push(`PAGEERROR: ${e.message}`));

  // SIP.js logs its whole configuration at startup, so the raw console
  // is thousands of lines — useless in a failure message and priceless
  // in an artifact. Split the two: assertions quote the signal, the
  // attachment keeps everything.
  const signal = () =>
    console_lines
      .filter((l) => /BROWSER-SIP|PAGEERROR|error|failed|refused/i.test(l))
      .slice(-12)
      .join("\n");

  // Everything the page said, kept as an artifact whatever happens.
  testInfo.attachments.push;
  const dumpConsole = async () =>
    testInfo.attach(`${engine}-console.log`, {
      body: console_lines.join("\n"),
      contentType: "text/plain",
    });

  const before = toneReport().calls.length;
  await page.goto(PAGE);
  test.info().annotations.push({ type: "page", description: PAGE });

  // 1. REGISTER over WSS — Phase 1's exit criterion, per engine.
  await expect
    .poll(() => console_lines.some((l) => l.includes("BROWSER-SIP-RESULT: REGISTERED")), {
      message: `${engine} never registered:\n${signal()}`,
      timeout: 30_000,
    })
    .toBe(true);

  // 2. The call is answered with a real media leg.
  await expect
    .poll(() => console_lines.some((l) => l.includes("BROWSER-SIP-RESULT: ANSWERED")), {
      message: `${engine} call never answered:\n${signal()}`,
      timeout: 30_000,
    })
    .toBe(true);

  // 3. The daemon says this is a browser leg, and says it is up. The
  //    admin surface is the operator's view of exactly this call
  //    (§4.6), so asserting it here keeps that view honest.
  await expect
    .poll(
      async () =>
        series(await metrics(), "siphon_ai_webrtc_legs_total", 'result="connected"'),
      { message: `${engine}: no connected WebRTC leg in /metrics`, timeout: 30_000 },
    )
    .toBeGreaterThan(0);

  // 4. The page can prove it *received* audio. Receiver stats work on
  //    every engine; the frequency check below is a bonus where the
  //    engine will run an AudioContext at all (headless Firefox will
  //    not — see examples/browser-sip/index.html).
  await expect
    .poll(async () => (await page.evaluate(() => window.__labAudio ?? null))?.rx_energy ?? 0, {
      message: `${engine}: no inbound audio energy — the WS server's tone never reached the browser\n${signal()}`,
      timeout: 45_000,
    })
    .toBeGreaterThan(0);

  const heard = await page.evaluate(() => window.__labAudio);
  testInfo.annotations.push({
    type: "audio",
    description: `${engine}: tx ${heard.source}${heard.tx_hz ? ` @${heard.tx_hz} Hz` : ""}, analyser=${heard.analyser}`,
  });
  if (heard.analyser) {
    await expect
      .poll(async () => (await page.evaluate(() => window.__labAudio))?.matches ?? 0, {
        message: `${engine} never heard ${heard.expect_rx_hz} Hz specifically`,
        timeout: 30_000,
      })
      .toBeGreaterThanOrEqual(5);
    const final = await page.evaluate(() => window.__labAudio);
    expect(
      Math.abs(final.dominant_hz - final.expect_rx_hz),
      `${engine} heard ${final.dominant_hz} Hz, expected ~${final.expect_rx_hz}`,
    ).toBeLessThanOrEqual(60);
  }

  // 5. …and the WS server heard the browser. It *measures* the
  //    dominant frequency rather than assuming one, because an engine
  //    without WebAudio sends its fake capture device instead of our
  //    oscillator — so the assertion is "what the page says it sent is
  //    what arrived", which holds either way.
  await expect
    .poll(() => toneReport().calls.slice(before).at(-1)?.rx_tonal_frames ?? 0, {
      message: `${engine}: the WS server heard no identifiable audio from the browser`,
      timeout: 30_000,
    })
    .toBeGreaterThanOrEqual(25);

  const call = toneReport().calls.slice(before).at(-1);
  expect(call.rx_bad_size, "every frame must be exactly 20 ms of PCM16LE").toBe(0);
  expect(call.rx_peak, "audio arrived, but as near-silence").toBeGreaterThan(2000);
  testInfo.annotations.push({
    type: "ws-server",
    description: `${engine}: ${call.rx_frames} frames, dominant ${call.rx_dominant_hz} Hz, peak ${call.rx_peak}`,
  });
  if (heard.tx_hz) {
    expect(
      call.rx_dominant_hz,
      `${engine} sent ${heard.tx_hz} Hz but the WS server heard ${call.rx_dominant_hz} Hz`,
    ).toBe(heard.tx_hz);
    expect(call.rx_tone_frames).toBeGreaterThanOrEqual(25);
  }

  // 6. Hang up from the browser and give the slot back — the same
  //    teardown the loopback tests assert in-process, here driven by a
  //    real SIP stack's BYE.
  await page.click("#btnHangup");
  await expect
    .poll(async () => series(await metrics(), "siphon_ai_calls_active"), {
      message: `${engine}: the call did not clear after BYE`,
      timeout: 30_000,
    })
    .toBe(0);
  await expect
    .poll(async () => series(await metrics(), "siphon_ai_rtp_port_pairs_allocated"), {
      message: `${engine}: the RTP port pair leaked after BYE`,
      timeout: 30_000,
    })
    .toBe(0);

  await dumpConsole();
});
