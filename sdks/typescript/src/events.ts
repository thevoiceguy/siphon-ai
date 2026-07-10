/**
 * Typed events for the SiphonAI WS protocol v1 (daemon → server).
 *
 * A discriminated union mirroring `schemas/siphon-ai.v1.json`
 * `$defs/BridgeOut` exactly — the SDK test suite validates this module
 * against the schema and every example in `docs/PROTOCOL.md`.
 *
 * Parsing is tolerant by contract: unknown JSON fields pass through and an
 * unknown `type` becomes `{ type: "unknown" }` (the protocol is additive
 * within v1).
 */

export interface AudioFormat {
  encoding: string;
  sample_rate: number;
  channels: number;
  frame_ms: number;
}

export interface SipMeta {
  call_id: string;
  headers: Record<string, string>;
}

export interface TraceContext {
  traceparent: string;
  tracestate?: string;
}

interface Base {
  call_id: string;
  seq: number;
}

export interface Start extends Base {
  type: "start";
  version: string;
  from: string;
  to: string;
  direction: "inbound" | "outbound";
  audio: AudioFormat;
  sip: SipMeta;
  srtp?: Record<string, unknown>;
  verstat?: Record<string, unknown>;
  trace_context?: TraceContext;
  retrieved?: boolean;
  reconnected?: boolean;
}

export interface SpeechStarted extends Base {
  type: "speech_started";
  ts_ms: number;
}

export interface SpeechStopped extends Base {
  type: "speech_stopped";
  ts_ms: number;
  duration_ms: number;
}

/** Wire `type: hold` (daemon→server): the far end held the call. */
export interface FarEndHold extends Base {
  type: "hold";
  direction: string;
}

/** Wire `type: resume` (daemon→server). */
export interface FarEndResume extends Base {
  type: "resume";
}

export interface SilenceDetected extends Base {
  type: "silence_detected";
  duration_ms: number;
}

export interface DeadAirDetected extends Base {
  type: "dead_air_detected";
  duration_ms: number;
}

export interface RtpStats extends Base {
  type: "rtp_stats";
  jitter_ms?: number | null;
  packet_loss_ratio?: number | null;
  rtcp_rtt_ms?: number | null;
}

export interface Dtmf extends Base {
  type: "dtmf";
  digit: string;
  duration_ms: number;
  method: "rfc2833" | "inband";
}

/** Playout-position echo of a `mark` the server sent. */
export interface MarkEvent extends Base {
  type: "mark";
  name: string;
}

export interface RecordingStarted extends Base {
  type: "recording_started";
  recording_id: string;
}

export interface RecordingStopped extends Base {
  type: "recording_stopped";
  recording_id: string;
}

export interface RecordingFailed extends Base {
  type: "recording_failed";
  recording_id: string;
  reason: string;
}

export interface ConferenceJoined extends Base {
  type: "conference_joined";
  room_id: string;
  participants: number;
}

export interface ConferenceLeft extends Base {
  type: "conference_left";
  room_id: string;
  reason: string;
}

export interface ParticipantJoined extends Base {
  type: "participant_joined";
  room_id: string;
  participant_call_id: string;
}

export interface ParticipantLeft extends Base {
  type: "participant_left";
  room_id: string;
  participant_call_id: string;
}

/** Ack: the bot-requested hold is active. */
export interface Held extends Base {
  type: "held";
}

/** Ack: the bot-requested hold ended. */
export interface Resumed extends Base {
  type: "resumed";
}

/** Last message of a session; the daemon closes after sending it. */
export interface StopEvent extends Base {
  type: "stop";
  reason: string;
}

export interface ErrorEvent extends Base {
  type: "error";
  code: string;
  message: string;
}

/** A `type` this SDK version doesn't know — additive within v1. */
export interface UnknownEvent {
  type: "unknown";
  wireType: string;
  raw: Record<string, unknown>;
}

export type BridgeEvent =
  | Start
  | SpeechStarted
  | SpeechStopped
  | FarEndHold
  | FarEndResume
  | SilenceDetected
  | DeadAirDetected
  | RtpStats
  | Dtmf
  | MarkEvent
  | RecordingStarted
  | RecordingStopped
  | RecordingFailed
  | ConferenceJoined
  | ConferenceLeft
  | ParticipantJoined
  | ParticipantLeft
  | Held
  | Resumed
  | StopEvent
  | ErrorEvent
  | UnknownEvent;

export const KNOWN_EVENT_TYPES: ReadonlySet<string> = new Set([
  "start",
  "speech_started",
  "speech_stopped",
  "hold",
  "resume",
  "silence_detected",
  "dead_air_detected",
  "rtp_stats",
  "dtmf",
  "mark",
  "recording_started",
  "recording_stopped",
  "recording_failed",
  "conference_joined",
  "conference_left",
  "participant_joined",
  "participant_left",
  "held",
  "resumed",
  "stop",
  "error",
]);

/**
 * Parse one JSON text frame into a typed event.
 *
 * Throws for malformed JSON or a non-object payload; returns
 * `UnknownEvent` for a well-formed message whose `type` this SDK doesn't
 * know. Unknown *fields* on known types pass through untouched.
 */
export function parseEvent(text: string | Buffer): BridgeEvent {
  let payload: unknown;
  try {
    payload = JSON.parse(text.toString());
  } catch (e) {
    throw new Error(`malformed protocol JSON: ${(e as Error).message}`);
  }
  if (
    typeof payload !== "object" ||
    payload === null ||
    typeof (payload as { type?: unknown }).type !== "string"
  ) {
    throw new Error("protocol message must be an object with a string `type`");
  }
  const message = payload as Record<string, unknown> & { type: string };
  if (!KNOWN_EVENT_TYPES.has(message.type)) {
    return { type: "unknown", wireType: message.type, raw: message };
  }
  // The wire shape IS the type shape (schema-validated in tests) — no
  // per-field mapping layer to drift.
  return message as unknown as BridgeEvent;
}
