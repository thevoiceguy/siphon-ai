/**
 * Server SDK for the SiphonAI WebSocket bridge protocol v1.
 *
 * ```ts
 * import { SiphonServer } from "siphon-ai-server";
 *
 * const server = new SiphonServer(async (call) => {
 *   console.log("call from", call.start.from);
 *   for await (const item of call) {
 *     if (item.type === "audio") call.sendAudioFrame(item.pcm); // echo
 *     else if (item.type === "dtmf" && item.digit === "0") call.hangup();
 *   }
 * });
 * await server.listen();
 * ```
 *
 * The canonical protocol spec is `docs/PROTOCOL.md`; the machine-readable
 * contract is `schemas/siphon-ai.v1.json` (this SDK's test suite validates
 * against both). **This SDK contains no AI code** — STT/LLM/TTS are your
 * handler's business.
 */

export { AudioSender, FRAME_MS, frameBytes } from "./audio.js";
export { Call, type AudioFrame, type CallItem } from "./call.js";
export {
  KNOWN_EVENT_TYPES,
  parseEvent,
  type AudioFormat,
  type BargeInResolved,
  type BridgeEvent,
  type ConferenceJoined,
  type ConferenceLeft,
  type DeadAirDetected,
  type Dtmf,
  type ErrorEvent,
  type FarEndHold,
  type FarEndResume,
  type Held,
  type MarkEvent,
  type ParticipantJoined,
  type ParticipantLeft,
  type RecordingFailed,
  type RecordingStarted,
  type RecordingStopped,
  type Resumed,
  type RtpStats,
  type SilenceDetected,
  type SipMeta,
  type SpeechStarted,
  type SpeechStopped,
  type Start,
  type StopEvent,
  type TraceContext,
  type UnknownEvent,
} from "./events.js";
export { SiphonServer, SUBPROTOCOL, type CallHandler } from "./server.js";
