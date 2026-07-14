/** One bridged call: typed event stream + command senders. */

import type { WebSocket } from "ws";

import { AudioSender } from "./audio.js";
import { parseEvent, type BridgeEvent, type Start } from "./events.js";

/** One 20 ms caller-audio frame (raw PCM16-LE mono). */
export interface AudioFrame {
  type: "audio";
  pcm: Buffer;
}

export type CallItem = AudioFrame | BridgeEvent;

/**
 * A live SiphonAI bridge session.
 *
 * Iterate it (`for await (const item of call)`) to receive caller audio
 * frames interleaved with typed protocol events; the iterator ends when
 * the daemon closes the session (normally right after `stop`).
 *
 * Command methods mirror `docs/PROTOCOL.md` §4. End a call with
 * `hangup()` — per §5.7 a bare WS close is an unexpected drop, not a
 * hangup.
 */
export class Call implements AsyncIterable<CallItem> {
  readonly callId: string;
  readonly audioOut: AudioSender;

  constructor(
    private readonly ws: WebSocket,
    readonly start: Start,
  ) {
    this.callId = start.call_id;
    this.audioOut = new AudioSender(
      (frame) => this.ws.send(frame),
      start.audio.sample_rate,
    );
  }

  // ─── receiving ─────────────────────────────────────────────────

  [Symbol.asyncIterator](): AsyncIterator<CallItem> {
    const queue: CallItem[] = [];
    let notify: (() => void) | null = null;
    let done = false;

    const wake = () => {
      if (notify !== null) {
        const n = notify;
        notify = null;
        n();
      }
    };
    this.ws.on("message", (data: Buffer, isBinary: boolean) => {
      if (isBinary) {
        queue.push({ type: "audio", pcm: data });
      } else {
        try {
          queue.push(parseEvent(data));
        } catch {
          // The daemon never sends malformed JSON; a robust server
          // drops the frame rather than the call.
        }
      }
      wake();
    });
    const finish = () => {
      done = true;
      this.audioOut.close();
      wake();
    };
    this.ws.on("close", finish);
    this.ws.on("error", finish);

    return {
      next: async (): Promise<IteratorResult<CallItem>> => {
        for (;;) {
          const item = queue.shift();
          if (item !== undefined) return { value: item, done: false };
          if (done) return { value: undefined, done: true };
          await new Promise<void>((resolve) => {
            notify = resolve;
          });
        }
      },
    };
  }

  // ─── audio ─────────────────────────────────────────────────────

  /** Queue PCM16-LE mono bytes (any length) — re-framed to exact 20 ms
   * frames and paced at real time. */
  sendAudio(pcm: Buffer): void {
    this.audioOut.push(pcm);
  }

  /** Send one already-exact 20 ms frame immediately (no pacing) — for
   * echo/replay servers mirroring the daemon's own cadence. */
  sendAudioFrame(frame: Buffer): void {
    this.ws.send(frame);
  }

  /** Barge-in: drop locally buffered audio AND tell the daemon to flush
   * everything already queued on its side. */
  clear(): void {
    this.audioOut.clear();
    this.command({ type: "clear" });
  }

  // ─── commands (PROTOCOL.md §4) ─────────────────────────────────

  mark(name: string): void {
    this.command({ type: "mark", name });
  }

  hangup(cause = "normal"): void {
    this.command({ type: "hangup", cause });
  }

  transfer(target?: string, opts?: { replacesCallId?: string }): void {
    const msg: Record<string, unknown> = { type: "transfer" };
    if (target !== undefined) msg.target = target;
    if (opts?.replacesCallId !== undefined)
      msg.replaces_call_id = opts.replacesCallId;
    this.command(msg);
  }

  sendDtmf(digit: string, durationMs = 160): void {
    this.command({ type: "send_dtmf", digit, duration_ms: durationMs });
  }

  /** Verdict on a pending pause-mode barge-in arbitration (0.32.0): the
   * speech was a real interruption — the daemon drops the retained
   * playout tail. A no-op when no arbitration is pending. */
  bargeInConfirm(): void {
    this.command({ type: "barge_in_confirm" });
  }

  /** Verdict on a pending pause-mode barge-in arbitration (0.32.0):
   * false positive — playout resumes where it stopped. A no-op when no
   * arbitration is pending. */
  bargeInReject(): void {
    this.command({ type: "barge_in_reject" });
  }

  mute(): void {
    this.command({ type: "mute" });
  }

  unmute(): void {
    this.command({ type: "unmute" });
  }

  startRecording(): void {
    this.command({ type: "start_recording" });
  }

  stopRecording(): void {
    this.command({ type: "stop_recording" });
  }

  pauseRecording(): void {
    this.command({ type: "pause_recording" });
  }

  resumeRecording(): void {
    this.command({ type: "resume_recording" });
  }

  setRecordingConsent(note?: string): void {
    const msg: Record<string, unknown> = { type: "set_recording_consent" };
    if (note !== undefined) msg.note = note;
    this.command(msg);
  }

  park(slot?: string): void {
    const msg: Record<string, unknown> = { type: "park" };
    if (slot !== undefined) msg.slot = slot;
    this.command(msg);
  }

  conferenceJoin(roomId: string): void {
    this.command({ type: "conference_join", room_id: roomId });
  }

  conferenceLeave(): void {
    this.command({ type: "conference_leave" });
  }

  hold(): void {
    this.command({ type: "hold" });
  }

  resume(): void {
    this.command({ type: "resume" });
  }

  // ─── lifecycle ─────────────────────────────────────────────────

  /** Hard-drop the socket without a hangup — the daemon treats it as an
   * unexpected WS drop. Test harnesses use this; bots want `hangup()`. */
  abort(): void {
    this.audioOut.close();
    this.ws.close(1011, "aborted");
  }

  private command(msg: Record<string, unknown>): void {
    if (this.ws.readyState !== this.ws.OPEN) return; // best-effort near teardown
    this.ws.send(JSON.stringify({ call_id: this.callId, ...msg }));
  }
}
