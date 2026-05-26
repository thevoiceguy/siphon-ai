//! WebSocket bridge: protocol types and connection management.
//!
//! The protocol shape is a public API — see `docs/PROTOCOL.md` and
//! CLAUDE.md §4.2. Audio frames are 20ms PCM16-LE mono; never break this.

pub mod audio;
pub mod conn;
pub mod protocol;
pub mod tls;

pub use audio::{
    pack_pcm16_le, samples_per_frame, unpack_pcm16_le, AudioError, Reframer, BYTES_PER_SAMPLE,
    FRAME_DURATION_MS,
};
pub use conn::{
    connect_and_run, normalize_auth_header, BridgeChannels, BridgeConfig, BridgeError,
    DisconnectReason, OutgoingEvent,
};
pub use protocol::{
    AudioEncoding, AudioFormat, BridgeIn, BridgeOut, CallId, Direction, DtmfMethod, ErrorCode,
    HangupCause, Seq, SipMeta, StartMsg, StopReason, PROTOCOL_VERSION, WS_SUBPROTOCOL,
};
