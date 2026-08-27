// Real-browser interop for the WebRTC leg (DEV_PLAN_WebRTC.md §5,
// item 2 and the interop matrix).
//
// Nightly, not per-commit: the per-commit media coverage is the
// in-process loopback (`crates/webrtc-glue/tests/loopback.rs` and
// `crates/core/tests/webrtc_call.rs`). What only a real browser can
// tell us is whether *its* WebRTC stack and *its* SIP stack agree with
// ours — which is why this runs Chromium, Firefox and WebKit rather
// than one engine well.

import { defineConfig, devices } from "@playwright/test";

const only = process.env.BROWSERS?.split(",").map((s) => s.trim());
const engines = [
  { name: "chromium", use: devices["Desktop Chrome"] },
  {
    name: "firefox",
    use: {
      ...devices["Desktop Firefox"],
      launchOptions: {
        // Without these the page's AudioContext starts suspended (no
        // user gesture in a headless run), and a suspended context is
        // silent in *both* directions — the oscillator sends digital
        // zeroes and the analyser hears nothing. The page calls
        // `resume()` too; belt and braces, because a silent pass here
        // would look like a codec bug.
        firefoxUserPrefs: {
          // Headless Firefox has no audio output device, so its
          // AudioContext never leaves `suspended` no matter what these
          // say — the page detects that and falls back to the capture
          // device, which is why the fake stream matters more than the
          // autoplay prefs do.
          "media.autoplay.default": 0,
          "media.autoplay.blocking_policy": 0,
          "media.navigator.permission.disabled": true,
          "media.navigator.streams.fake": true,
        },
      },
    },
  },
  // WebKit is the closest thing to Safari that runs headless on Linux.
  // It is not Safari — the plan's real-Safari lane needs a macOS
  // runner — but it shares the engine whose DTLS and Opus behaviour
  // diverges most, so it is where surprises show up first.
  { name: "webkit", use: devices["Desktop Safari"] },
];

export default defineConfig({
  testDir: "./tests",
  // One stack, one AOR, one registration: engines take turns.
  fullyParallel: false,
  workers: 1,
  retries: 0,
  timeout: 120_000,
  expect: { timeout: 30_000 },
  reporter: process.env.CI ? [["list"], ["html", { open: "never" }]] : [["list"]],
  globalSetup: "./global-setup.mjs",
  globalTeardown: "./global-teardown.mjs",
  use: {
    // The lab's WSS cert is self-signed by `gen-cert.sh`.
    ignoreHTTPSErrors: true,
    trace: "retain-on-failure",
    video: "off",
  },
  projects: engines.filter((e) => !only || only.includes(e.name)),
});
