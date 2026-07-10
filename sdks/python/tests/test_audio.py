"""AudioSender re-framing + pacing unit tests."""

from __future__ import annotations

import asyncio
import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))

from siphon_ai_server.audio import AudioSender, frame_bytes  # noqa: E402


class FrameBytesTest(unittest.TestCase):
    def test_spec_sizes(self) -> None:
        self.assertEqual(frame_bytes(8000), 320)
        self.assertEqual(frame_bytes(16000), 640)
        with self.assertRaises(ValueError):
            frame_bytes(44100)


class AudioSenderTest(unittest.IsolatedAsyncioTestCase):
    async def test_reframes_arbitrary_pushes_to_exact_frames(self) -> None:
        sent: list[bytes] = []

        async def send(frame: bytes) -> None:
            sent.append(frame)

        sender = AudioSender(send, 8000)
        # 700 bytes in awkward chunks = 2 whole frames + 60-byte tail.
        sender.push(b"\x01" * 100)
        sender.push(b"\x02" * 500)
        sender.push(b"\x03" * 100)
        await sender.flush()
        await sender.aclose()

        self.assertEqual(len(sent), 3)
        self.assertTrue(all(len(f) == 320 for f in sent), [len(f) for f in sent])
        # Tail frame is zero-padded, not dropped.
        self.assertTrue(sent[-1].endswith(b"\x00" * 260))

    async def test_pacing_is_real_time(self) -> None:
        stamps: list[float] = []
        loop = asyncio.get_running_loop()

        async def send(_: bytes) -> None:
            stamps.append(loop.time())

        sender = AudioSender(send, 8000)
        sender.push(b"\x00" * 320 * 5)  # exactly 5 frames
        await sender.flush()
        await sender.aclose()

        self.assertEqual(len(stamps), 5)
        elapsed = stamps[-1] - stamps[0]
        # 4 inter-frame gaps at 20 ms = 80 ms nominal; generous CI bounds.
        self.assertGreaterEqual(elapsed, 0.06, f"sent too fast: {elapsed:.3f}s")
        self.assertLess(elapsed, 0.5, f"sent too slow: {elapsed:.3f}s")

    async def test_clear_drops_buffer(self) -> None:
        sent: list[bytes] = []

        async def send(frame: bytes) -> None:
            sent.append(frame)

        sender = AudioSender(send, 8000)
        sender.push(b"\x00" * 320)  # one frame gets sent
        await asyncio.sleep(0.03)
        sender.push(b"\x00" * 3200)  # ten more…
        dropped = sender.clear()  # …dropped before pacing reaches them
        await sender.aclose()

        self.assertGreater(dropped, 0)
        self.assertLessEqual(len(sent), 2)


if __name__ == "__main__":
    unittest.main()
