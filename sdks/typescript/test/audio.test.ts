/** AudioSender re-framing + pacing. */

import assert from "node:assert/strict";
import test from "node:test";

import { AudioSender, frameBytes } from "../src/audio.js";

test("frameBytes matches the spec", () => {
  assert.equal(frameBytes(8000), 320);
  assert.equal(frameBytes(16000), 640);
  assert.throws(() => frameBytes(44100));
});

test("re-frames arbitrary pushes into exact zero-padded frames", async () => {
  const sent: Buffer[] = [];
  const sender = new AudioSender((f) => sent.push(f), 8000);
  sender.push(Buffer.alloc(100, 1));
  sender.push(Buffer.alloc(500, 2));
  sender.push(Buffer.alloc(100, 3));
  await sender.flush();
  sender.close();

  assert.equal(sent.length, 3);
  for (const f of sent) assert.equal(f.length, 320);
  // Tail zero-padded, not dropped.
  assert.ok(sent[2].subarray(60).every((b) => b === 0));
});

test("pacing is real time", async () => {
  const stamps: number[] = [];
  const sender = new AudioSender(() => stamps.push(Date.now()), 8000);
  sender.push(Buffer.alloc(320 * 5)); // exactly 5 frames
  await sender.flush();
  sender.close();

  assert.equal(stamps.length, 5);
  const elapsed = stamps[4] - stamps[0];
  assert.ok(elapsed >= 60, `sent too fast: ${elapsed}ms`);
  assert.ok(elapsed < 500, `sent too slow: ${elapsed}ms`);
});

test("clear drops buffered audio", async () => {
  const sent: Buffer[] = [];
  const sender = new AudioSender((f) => sent.push(f), 8000);
  sender.push(Buffer.alloc(3200)); // ten frames queued
  await new Promise((r) => setTimeout(r, 30));
  const dropped = sender.clear();
  sender.close();

  assert.ok(dropped > 0);
  assert.ok(sent.length <= 2);
});
