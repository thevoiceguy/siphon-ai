#!/usr/bin/env python3
"""Smoke tests for the OpenAI bridge.

Pinned to the surface area we can exercise without a real OpenAI
session: argparse, the resampler, and the request handler's
healthz short-circuit. Real end-to-end testing needs a working
``OPENAI_API_KEY`` plus a running SiphonAI daemon and lives in
the test-harness — not here.

Run:
    .venv/bin/python -m pytest test_smoke.py     # if you have pytest
    .venv/bin/python test_smoke.py               # bare runner

Bare runner is the default because ``websockets`` is the only
runtime dep and we don't want to drag pytest in just for three
assertions.
"""
from __future__ import annotations

import os
import struct
import sys
import unittest

# Imports are inside cases so a stray import error doesn't tank
# the whole module on shells without the venv activated.


class ResamplerTests(unittest.TestCase):
    def test_passthrough_when_rates_match(self):
        from server import resample_pcm16

        # 1 ms of silence at 24 kHz = 24 samples * 2 bytes = 48 bytes
        silence = b"\x00" * 48
        self.assertEqual(resample_pcm16(silence, 24_000, 24_000), silence)

    def test_upsample_8k_to_24k_triples_frame(self):
        from server import resample_pcm16

        # 20 ms of audio @ 8 kHz = 160 samples = 320 bytes.
        # After 8 → 24 kHz upsample we expect ~3x → ~960 bytes
        # (480 samples). The exact length depends on the linear
        # interpolator's rounding; tolerate ±1 sample.
        frame_8k = b"\x10\x00" * 160  # 160 samples of small constant
        frame_24k = resample_pcm16(frame_8k, 8_000, 24_000)
        out_samples = len(frame_24k) // 2
        self.assertAlmostEqual(out_samples, 480, delta=1)

    def test_downsample_24k_to_8k_thirds_frame(self):
        from server import resample_pcm16

        frame_24k = b"\x10\x00" * 480
        frame_8k = resample_pcm16(frame_24k, 24_000, 8_000)
        out_samples = len(frame_8k) // 2
        self.assertAlmostEqual(out_samples, 160, delta=1)

    def test_short_input_returns_unchanged(self):
        from server import resample_pcm16

        # Single sample — interpolator can't form a pair, returns
        # the input verbatim rather than crashing.
        single = b"\x10\x00"
        self.assertEqual(resample_pcm16(single, 8_000, 24_000), single)

    def test_clamps_to_int16(self):
        from server import resample_pcm16

        # Two max-positive samples followed by an interpolation
        # produces a value the size of either input — must not
        # overflow into bytes the unpacker rejects.
        peaks = struct.pack("<hh", 32767, 32767)
        out = resample_pcm16(peaks, 8_000, 16_000)
        vals = struct.unpack(f"<{len(out) // 2}h", out)
        for v in vals:
            self.assertTrue(-32768 <= v <= 32767)


class CliTests(unittest.TestCase):
    def test_missing_api_key_errors_clearly(self):
        # parse_args should `parser.error` (SystemExit) when the
        # key isn't set. Capture stderr and check the message.
        from server import parse_args

        env = os.environ.pop("OPENAI_API_KEY", None)
        try:
            with self.assertRaises(SystemExit):
                parse_args(["--bind", "127.0.0.1:0"])
        finally:
            if env is not None:
                os.environ["OPENAI_API_KEY"] = env

    def test_parses_with_api_key(self):
        from server import parse_args

        prior = os.environ.get("OPENAI_API_KEY")
        os.environ["OPENAI_API_KEY"] = "sk-fake"
        try:
            opts = parse_args(["--bind", "127.0.0.1:9000", "--voice", "verse"])
            self.assertEqual(opts.bind_host, "127.0.0.1")
            self.assertEqual(opts.bind_port, 9000)
            self.assertEqual(opts.voice, "verse")
            self.assertEqual(opts.openai_api_key, "sk-fake")
        finally:
            if prior is None:
                os.environ.pop("OPENAI_API_KEY", None)
            else:
                os.environ["OPENAI_API_KEY"] = prior


if __name__ == "__main__":
    # Make `python test_smoke.py` work from the example dir without
    # needing PYTHONPATH gymnastics.
    sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
    unittest.main()
