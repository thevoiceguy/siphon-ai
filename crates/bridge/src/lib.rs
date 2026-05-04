//! WebSocket bridge: protocol types and connection management.
//!
//! The protocol shape is a public API — see `docs/PROTOCOL.md` and
//! CLAUDE.md §4.2. Audio frames are 20ms PCM16-LE mono; never break this.
