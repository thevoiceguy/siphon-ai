/**
 * Outbound audio framing: arbitrary PCM byte pushes → exact 20 ms frames,
 * paced at real time (one frame per 20 ms) so a fast TTS engine can't
 * flood the daemon's bounded playout queue and blunt barge-in `clear`.
 */

export const FRAME_MS = 20;
export const SUPPORTED_RATES = [8000, 16000] as const;

/** Bytes per 20 ms PCM16-LE mono frame at `sampleRate`. */
export function frameBytes(sampleRate: number): number {
  if (!SUPPORTED_RATES.includes(sampleRate as 8000 | 16000)) {
    throw new Error(`unsupported sample rate ${sampleRate} (8000 or 16000)`);
  }
  return (sampleRate / 1000) * FRAME_MS * 2;
}

export type SendFrame = (frame: Buffer) => void;

/**
 * Paced 20 ms re-framer over a raw frame sender.
 *
 * `push()` never blocks; a timer drains the buffer one frame per 20 ms.
 * `clear()` drops everything buffered (barge-in); `flush()` resolves when
 * the buffer has fully played out, zero-padding the final partial frame.
 */
export class AudioSender {
  private readonly frameSize: number;
  private buffer: Buffer = Buffer.alloc(0);
  private timer: NodeJS.Timeout | null = null;
  private closed = false;

  constructor(
    private readonly send: SendFrame,
    sampleRate: number,
  ) {
    this.frameSize = frameBytes(sampleRate);
  }

  /** Queue PCM16-LE mono bytes of any length for paced sending. */
  push(pcm: Buffer): void {
    if (this.closed) return;
    this.buffer = Buffer.concat([this.buffer, pcm]);
    this.arm();
  }

  /** Drop all buffered audio (the local half of barge-in). Returns the
   * byte count dropped — pair with sending the daemon a `clear`. */
  clear(): number {
    const dropped = this.buffer.length;
    this.buffer = Buffer.alloc(0);
    return dropped;
  }

  /** Resolve once everything pushed so far has been sent; the final
   * partial frame (if any) is zero-padded to spec size. */
  async flush(): Promise<void> {
    const tail = this.buffer.length % this.frameSize;
    if (tail !== 0) {
      this.buffer = Buffer.concat([
        this.buffer,
        Buffer.alloc(this.frameSize - tail),
      ]);
      this.arm();
    }
    while (this.buffer.length > 0 && !this.closed) {
      await new Promise((resolve) => setTimeout(resolve, FRAME_MS));
    }
  }

  close(): void {
    this.closed = true;
    this.buffer = Buffer.alloc(0);
    if (this.timer !== null) {
      clearInterval(this.timer);
      this.timer = null;
    }
  }

  private arm(): void {
    if (this.timer !== null || this.closed) return;
    this.timer = setInterval(() => {
      if (this.closed || this.buffer.length < this.frameSize) {
        // Idle: disarm; the next push re-arms (no catch-up bursts).
        if (this.timer !== null) {
          clearInterval(this.timer);
          this.timer = null;
        }
        return;
      }
      const frame = this.buffer.subarray(0, this.frameSize);
      this.buffer = this.buffer.subarray(this.frameSize);
      try {
        this.send(Buffer.from(frame));
      } catch {
        this.close(); // dead socket just stops the pacer
      }
    }, FRAME_MS);
  }
}
