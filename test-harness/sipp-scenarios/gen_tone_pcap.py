#!/usr/bin/env python3
"""Generate a libpcap capture of a G.711 mu-law RTP tone stream.

Used by run-all.sh's barge_in_pause phase: SIPp's `play_pcap_audio`
replays the capture as the caller's media, giving forge-vad real
speech-shaped energy to fire on — the one thing a signaling-only
scenario can't provide. Generated at run time (stdlib only) so no
binary audio file lives in the repo (CLAUDE.md §6.4).

Usage: gen_tone_pcap.py OUT.pcap [SECONDS] [FREQ_HZ]
"""

import math
import struct
import sys

SAMPLE_RATE = 8000
SAMPLES_PER_FRAME = 160  # 20 ms
AMPLITUDE = 12000  # loud enough to trip an energy VAD, no clipping


def ulaw_encode(sample: int) -> int:
    """ITU-T G.711 mu-law encode one 16-bit PCM sample."""
    BIAS, CLIP = 0x84, 32635
    sign = 0x80 if sample < 0 else 0x00
    magnitude = min(abs(sample), CLIP) + BIAS
    exponent = 7
    mask = 0x4000
    while exponent > 0 and not magnitude & mask:
        exponent -= 1
        mask >>= 1
    mantissa = (magnitude >> (exponent + 3)) & 0x0F
    return ~(sign | (exponent << 4) | mantissa) & 0xFF


def main() -> None:
    path = sys.argv[1]
    seconds = float(sys.argv[2]) if len(sys.argv) > 2 else 3.0
    freq = float(sys.argv[3]) if len(sys.argv) > 3 else 440.0

    frames = int(seconds * SAMPLE_RATE / SAMPLES_PER_FRAME)
    out = bytearray()
    # libpcap global header, little-endian, linktype 1 (Ethernet) —
    # the framing sipp's pcapplay parser expects.
    out += struct.pack("<IHHiIII", 0xA1B2C3D4, 2, 4, 0, 0, 65535, 1)

    ssrc = 0x51500BAD
    for i in range(frames):
        first = i * SAMPLES_PER_FRAME
        payload = bytes(
            ulaw_encode(int(AMPLITUDE * math.sin(2 * math.pi * freq * ((first + n) / SAMPLE_RATE))))
            for n in range(SAMPLES_PER_FRAME)
        )
        # RTP: V=2, PT=0 (PCMU), 20 ms timestamp cadence.
        rtp = struct.pack("!BBHII", 0x80, 0x00, i & 0xFFFF, first, ssrc) + payload
        # sipp rewrites the destination; ports/addrs here are placeholders.
        udp = struct.pack("!HHHH", 6000, 6001, 8 + len(rtp), 0) + rtp
        ip = struct.pack(
            "!BBHHHBBH4s4s",
            0x45, 0, 20 + len(udp), i & 0xFFFF, 0, 64, 17, 0,
            bytes([127, 0, 0, 1]), bytes([127, 0, 0, 1]),
        )
        eth = b"\x00" * 12 + struct.pack("!H", 0x0800)
        pkt = eth + ip + udp
        ts_us = i * 20_000
        out += struct.pack("<IIII", ts_us // 1_000_000, ts_us % 1_000_000, len(pkt), len(pkt))
        out += pkt

    with open(path, "wb") as f:
        f.write(out)


if __name__ == "__main__":
    main()
