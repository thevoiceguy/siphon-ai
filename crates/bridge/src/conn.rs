//! WebSocket bridge connection lifecycle.
//!
//! Per `docs/PROTOCOL.md` §1, this owns ONE WebSocket connection
//! corresponding to ONE call. It opens the connection, sends `start`,
//! then runs a `tokio::select!` loop relaying audio (binary) and control
//! (text) frames between the WS server and the controller via channels.
//!
//! ## Concurrency model
//!
//! `connect_and_run` is a single async function the controller spawns on
//! a per-call task. It returns when the connection ends. Three sources
//! drive the loop:
//!
//! - `audio_out_rx` — already-encoded PCM16-LE bytes from media-glue;
//!   sent as binary WS frames (one frame per message, exactly 20 ms).
//! - `control_out_rx` — high-level [`OutgoingEvent`]s the controller
//!   wants to ship to the server. The conn stamps each with the next
//!   `seq` and serializes to [`BridgeOut`].
//! - the WebSocket itself — incoming text/binary frames are demuxed:
//!   binary → `audio_in_tx`, text → parsed [`BridgeIn`] →
//!   `control_in_tx`, ping → pong, close → graceful exit.
//!
//! `seq` starts at 0 on the [`BridgeOut::Start`] message and increments
//! by 1 with every subsequent SiphonAI→server message (PROTOCOL.md §3).
//!
//! ## Hot path
//!
//! Per CLAUDE.md §4.3, the binary audio path:
//! - allocates one `Vec<u8>` per inbound frame (tungstenite owns the
//!   buffer; we hand it through);
//! - allocates zero buffers per outbound frame beyond what tungstenite
//!   needs to frame the WS message;
//! - never blocks (channel send/recv yield instead).
//!
//! ## Not yet implemented
//!
//! - WS keepalive (PROTOCOL.md §5.6: ping every 15 s, 10 s pong
//!   deadline). Tracked as a follow-up; the underlying WS lib will
//!   surface a hard error if the TCP connection dies, so v0.0.0 still
//!   detects total disconnects.

use std::time::Duration;

use futures::{SinkExt, StreamExt};
use thiserror::Error;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::{
    client::IntoClientRequest,
    http::{HeaderValue, Request},
    protocol::{frame::coding::CloseCode, CloseFrame, Message, WebSocketConfig},
    Error as WsError,
};
use tokio_tungstenite::{connect_async_with_config, MaybeTlsStream, WebSocketStream};
use tracing::{debug, info, instrument, warn};

use crate::protocol::{
    BridgeIn, BridgeOut, CallId, DtmfMethod, ErrorCode, Seq, StartMsg, StopReason, WS_SUBPROTOCOL,
};

/// Maximum size of any single text frame, per PROTOCOL.md §2.1.
const MAX_TEXT_BYTES: usize = 256 * 1024;

/// Connection-level configuration. All fields come from siphon-ai's TOML
/// config (see `crates/config`); the bridge crate doesn't read TOML
/// itself.
#[derive(Debug, Clone)]
pub struct BridgeConfig {
    /// Full WebSocket URL: `ws://host:port/path` or `wss://...`.
    pub ws_url: String,
    /// Full `Authorization` header value (including the scheme).
    /// Sent verbatim on the WS upgrade. Bare tokens get normalised
    /// to `Bearer <token>` upstream in `core::acceptor`; values
    /// that already contain a scheme (`Bearer xxx`, `Basic abc`,
    /// `Digest …`) pass through untouched.
    pub auth_header: Option<String>,
    /// How long to wait for the WS handshake before giving up.
    pub connect_timeout: Duration,
}

impl Default for BridgeConfig {
    fn default() -> Self {
        Self {
            ws_url: String::new(),
            auth_header: None,
            connect_timeout: Duration::from_secs(5),
        }
    }
}

/// Channels the controller hands to the conn task.
///
/// The conn task owns these halves for the lifetime of the call. When
/// the conn returns (clean close or error), it drops the channels —
/// senders observe `SendError`, receivers observe `None`.
pub struct BridgeChannels {
    /// PCM16-LE audio bytes from media-glue, framed as exactly one WS
    /// binary message each.
    pub audio_out_rx: mpsc::Receiver<Vec<u8>>,
    /// Control events the controller wants to ship; the conn stamps
    /// `seq` and serializes.
    pub control_out_rx: mpsc::Receiver<OutgoingEvent>,
    /// PCM16-LE audio bytes received from the server.
    pub audio_in_tx: mpsc::Sender<Vec<u8>>,
    /// Parsed control messages from the server. `call_id` is already
    /// validated against the connection's call.
    pub control_in_tx: mpsc::Sender<BridgeIn>,
}

/// High-level outgoing control events. Distinct from [`BridgeOut`] so
/// callers don't have to know `seq` or `call_id` — the conn stamps
/// them.
#[derive(Debug, Clone, PartialEq)]
pub enum OutgoingEvent {
    SpeechStarted {
        ts_ms: u64,
    },
    SpeechStopped {
        ts_ms: u64,
        duration_ms: u64,
    },
    Dtmf {
        digit: char,
        duration_ms: u32,
        method: DtmfMethod,
    },
    Mark {
        name: String,
    },
    /// Mid-dialog re-INVITE flipped peer audio direction to
    /// something other than `sendrecv`. The conn stamps `seq` and
    /// emits [`BridgeOut::Hold`].
    Hold {
        /// `"sendonly"`, `"recvonly"`, or `"inactive"` per RFC 3264.
        direction: String,
    },
    /// Direction returned to `sendrecv` after a [`Self::Hold`].
    Resume,
    Stop {
        reason: StopReason,
    },
    Error {
        code: ErrorCode,
        message: String,
    },
}

/// Why the connection task ended cleanly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisconnectReason {
    /// Controller sent [`OutgoingEvent::Stop`]; SiphonAI sent the WS
    /// close (1000) and the server acknowledged.
    StopSent,
    /// Server initiated the close (clean 1000 or otherwise). SiphonAI
    /// did NOT have a chance to send `stop` first — controller should
    /// emit a CDR with `stop_reason = "ws_disconnect"` (PROTOCOL.md §5.7).
    ServerClosed,
    /// Controller dropped its outgoing channels without sending `stop`.
    /// Conn synthesized a `stop { reason: error }` to keep the spec
    /// invariant ("`stop` is always the last message") and closed.
    ControllerHungUp,
}

#[derive(Debug, Error)]
pub enum BridgeError {
    /// Boxed because `tungstenite::Error` is large (~136 B); keeps `Result`
    /// sizes reasonable on the success path.
    #[error("websocket error: {0}")]
    WebSocket(Box<WsError>),

    #[error("invalid bridge configuration: {0}")]
    InvalidConfig(String),

    #[error("connect timed out after {0:?}")]
    ConnectTimeout(Duration),

    #[error("server returned malformed JSON: {0}")]
    BadJson(String),

    #[error("server message has wrong call_id (expected {expected}, got {got})")]
    CallIdMismatch { expected: String, got: String },

    #[error("internal: {0}")]
    Internal(String),
}

impl From<WsError> for BridgeError {
    fn from(e: WsError) -> Self {
        BridgeError::WebSocket(Box::new(e))
    }
}

/// Connect to the WS server, send `start`, and run the bidirectional
/// loop until the call ends.
///
/// The `start.seq` field is overwritten to `0` regardless of input
/// (PROTOCOL.md §3 mandates `seq` starts at 0 on `start`).
#[instrument(skip_all, fields(call_id = %start.call_id, ws_url = %config.ws_url))]
pub async fn connect_and_run(
    config: BridgeConfig,
    mut start: StartMsg,
    channels: BridgeChannels,
) -> Result<DisconnectReason, BridgeError> {
    start.seq = 0;
    let call_id = start.call_id.clone();

    let request = build_upgrade_request(&config, &call_id)?;

    let ws_config = WebSocketConfig {
        max_message_size: Some(MAX_TEXT_BYTES),
        max_frame_size: Some(MAX_TEXT_BYTES),
        ..Default::default()
    };

    let connect_fut = connect_async_with_config(request, Some(ws_config), false);
    let (ws, response) = match tokio::time::timeout(config.connect_timeout, connect_fut).await {
        Ok(result) => result?,
        Err(_) => return Err(BridgeError::ConnectTimeout(config.connect_timeout)),
    };

    if let Some(echoed) = response.headers().get("sec-websocket-protocol") {
        if echoed.as_bytes() != WS_SUBPROTOCOL.as_bytes() {
            warn!(echoed = ?echoed, "server echoed an unexpected subprotocol; proceeding");
        }
    } else {
        debug!("server did not echo a subprotocol; proceeding optimistically");
    }
    info!("bridge connected");

    run_loop(ws, start, channels, call_id).await
}

fn build_upgrade_request(
    config: &BridgeConfig,
    call_id: &CallId,
) -> Result<Request<()>, BridgeError> {
    let mut request = config.ws_url.as_str().into_client_request()?;

    let headers = request.headers_mut();
    headers.insert(
        "Sec-WebSocket-Protocol",
        HeaderValue::from_static(WS_SUBPROTOCOL),
    );
    headers.insert(
        "User-Agent",
        HeaderValue::from_static(concat!("siphon-ai/", env!("CARGO_PKG_VERSION"))),
    );
    headers.insert(
        "X-Siphon-Call-Id",
        HeaderValue::from_str(call_id.as_str()).map_err(|e| {
            BridgeError::InvalidConfig(format!("call_id is not a valid HTTP header value: {e}"))
        })?,
    );
    if let Some(value) = &config.auth_header {
        // Sent verbatim — `normalize_auth_header` already prepended
        // `Bearer ` for the bare-token case, and any pre-existing
        // scheme (`Bearer`, `Basic`, `Digest`, custom) survives
        // untouched. Reformatting here would double-prefix
        // non-Bearer schemes.
        headers.insert(
            "Authorization",
            HeaderValue::from_str(value).map_err(|e| {
                BridgeError::InvalidConfig(format!(
                    "auth_header is not a valid HTTP header value: {e}"
                ))
            })?,
        );
    }

    Ok(request)
}

/// Normalize a configured WS auth header into the full `Authorization`
/// value the bridge sends verbatim on the WS handshake (see
/// [`build_request`]).
///
/// - `"Bearer xxx"`, `"Basic abc"`, `"Digest …"` → returned as-is.
///   Any RFC 9110 scheme works; the conn layer sends what it is
///   handed without reformatting.
/// - `"xxx"` (a bare token with no whitespace) → `"Bearer xxx"`.
///   Preserves the historic UX where operators wrote just the token
///   for Bearer-auth WS servers.
///
/// This lives in `siphon-ai-bridge` — the crate that owns the
/// wire-header behavior — so `siphon-ai-config` (which reads the
/// `ws_auth_header` TOML key) and `siphon-ai-core` (which merges
/// route overrides onto the default) produce identical bytes from a
/// single implementation rather than two copies kept in sync by hand.
pub fn normalize_auth_header(value: &str) -> String {
    let trimmed = value.trim();
    // A bare token has no inner whitespace; an `Authorization`
    // header always has a scheme keyword followed by a space.
    if trimmed.contains(char::is_whitespace) {
        trimmed.to_string()
    } else {
        format!("Bearer {trimmed}")
    }
}

type WsStream = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

async fn run_loop(
    ws: WsStream,
    start: StartMsg,
    channels: BridgeChannels,
    call_id: CallId,
) -> Result<DisconnectReason, BridgeError> {
    let BridgeChannels {
        mut audio_out_rx,
        mut control_out_rx,
        audio_in_tx,
        control_in_tx,
    } = channels;

    let (mut sink, mut stream) = ws.split();

    // Send `start` as the first message. `seq = 0` already enforced.
    let start_json = serde_json::to_string(&BridgeOut::Start(start))
        .map_err(|e| BridgeError::Internal(format!("serialize start: {e}")))?;
    sink.send(Message::Text(start_json)).await?;

    // Subsequent SiphonAI→server messages use seq starting at 1.
    let mut seq: Seq = 1;

    loop {
        tokio::select! {
            biased;

            maybe_event = control_out_rx.recv() => {
                let Some(event) = maybe_event else {
                    // Controller hung up. Synthesize stop+error to keep
                    // the spec invariant that `stop` is the last message.
                    let stop = BridgeOut::Stop {
                        call_id: call_id.clone(),
                        seq,
                        reason: StopReason::Error,
                    };
                    let _ = sink
                        .send(Message::Text(serialize_or_drop(&stop)))
                        .await;
                    let _ = close_clean(&mut sink).await;
                    return Ok(DisconnectReason::ControllerHungUp);
                };

                let bridge_out = build_bridge_out(event, call_id.clone(), seq);
                seq = seq.wrapping_add(1);
                let is_stop = matches!(bridge_out, BridgeOut::Stop { .. });
                let json = serde_json::to_string(&bridge_out)
                    .map_err(|e| BridgeError::Internal(format!("serialize: {e}")))?;
                sink.send(Message::Text(json)).await?;

                if is_stop {
                    close_clean(&mut sink).await?;
                    return Ok(DisconnectReason::StopSent);
                }
            }

            maybe_audio = audio_out_rx.recv() => {
                let Some(audio) = maybe_audio else {
                    // Audio channel closed but control channel may still
                    // have something to say. Continue the loop.
                    continue;
                };
                sink.send(Message::Binary(audio)).await?;
            }

            maybe_msg = stream.next() => {
                match maybe_msg {
                    Some(Ok(Message::Binary(data))) => {
                        // Audio frames at ~50/sec — trace-level so
                        // operators can opt into the per-frame stream
                        // when triaging WS payout issues without
                        // drowning their dashboards at info.
                        tracing::trace!(bytes = data.len(), "ws inbound audio");
                        if audio_in_tx.send(data).await.is_err() {
                            return Ok(DisconnectReason::ControllerHungUp);
                        }
                    }
                    Some(Ok(Message::Text(text))) => {
                        let parsed: BridgeIn = serde_json::from_str(&text)
                            .map_err(|e| BridgeError::BadJson(e.to_string()))?;
                        let got = bridge_in_call_id(&parsed);
                        if got != call_id.as_str() {
                            return Err(BridgeError::CallIdMismatch {
                                expected: call_id.0.clone(),
                                got: got.to_string(),
                            });
                        }
                        // Debug-level: every received control message
                        // (Clear, Mark, Hangup, Transfer, SendDtmf).
                        // §11.8 Q9 in DEV_PLAN.md — operators bump
                        // `siphon_ai_bridge=debug` via /admin/log to
                        // see exactly what the WS server sent. Audio
                        // frames live one notch lower (trace).
                        tracing::debug!(?parsed, "ws inbound control");
                        if control_in_tx.send(parsed).await.is_err() {
                            return Ok(DisconnectReason::ControllerHungUp);
                        }
                    }
                    Some(Ok(Message::Ping(payload))) => {
                        sink.send(Message::Pong(payload)).await?;
                    }
                    Some(Ok(Message::Pong(_))) => {
                        // Liveness ack; nothing to do until keepalive lands.
                    }
                    Some(Ok(Message::Close(frame))) => {
                        debug!(?frame, "server initiated close");
                        let _ = sink.send(Message::Close(frame)).await;
                        return Ok(DisconnectReason::ServerClosed);
                    }
                    Some(Ok(Message::Frame(_))) => {
                        // Raw-frame variant only surfaces with extensions; ignore.
                    }
                    Some(Err(e)) => return Err(BridgeError::from(e)),
                    None => return Ok(DisconnectReason::ServerClosed),
                }
            }
        }
    }
}

async fn close_clean<S>(sink: &mut S) -> Result<(), BridgeError>
where
    S: SinkExt<Message, Error = WsError> + Unpin,
{
    let frame = CloseFrame {
        code: CloseCode::Normal,
        reason: "".into(),
    };
    sink.send(Message::Close(Some(frame))).await?;
    let _ = sink.close().await;
    Ok(())
}

/// Serialize `out` to JSON, falling back to a minimal stop on serialization
/// failure (which can't really happen with our owned types but keeps the
/// best-effort path clean).
fn serialize_or_drop(out: &BridgeOut) -> String {
    serde_json::to_string(out).unwrap_or_else(|_| String::from("{\"type\":\"stop\"}"))
}

fn build_bridge_out(event: OutgoingEvent, call_id: CallId, seq: Seq) -> BridgeOut {
    match event {
        OutgoingEvent::SpeechStarted { ts_ms } => BridgeOut::SpeechStarted {
            call_id,
            seq,
            ts_ms,
        },
        OutgoingEvent::SpeechStopped { ts_ms, duration_ms } => BridgeOut::SpeechStopped {
            call_id,
            seq,
            ts_ms,
            duration_ms,
        },
        OutgoingEvent::Dtmf {
            digit,
            duration_ms,
            method,
        } => BridgeOut::Dtmf {
            call_id,
            seq,
            digit,
            duration_ms,
            method,
        },
        OutgoingEvent::Mark { name } => BridgeOut::Mark { call_id, seq, name },
        OutgoingEvent::Hold { direction } => BridgeOut::Hold {
            call_id,
            seq,
            direction,
        },
        OutgoingEvent::Resume => BridgeOut::Resume { call_id, seq },
        OutgoingEvent::Stop { reason } => BridgeOut::Stop {
            call_id,
            seq,
            reason,
        },
        OutgoingEvent::Error { code, message } => BridgeOut::Error {
            call_id,
            seq,
            code,
            message,
        },
    }
}

fn bridge_in_call_id(msg: &BridgeIn) -> &str {
    match msg {
        BridgeIn::Clear { call_id } => call_id.as_str(),
        BridgeIn::Mark { call_id, .. } => call_id.as_str(),
        BridgeIn::Hangup { call_id, .. } => call_id.as_str(),
        BridgeIn::Transfer { call_id, .. } => call_id.as_str(),
        BridgeIn::SendDtmf { call_id, .. } => call_id.as_str(),
        BridgeIn::Mute { call_id } => call_id.as_str(),
        BridgeIn::Unmute { call_id } => call_id.as_str(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::HangupCause;

    #[test]
    fn build_bridge_out_stamps_call_id_and_seq() {
        let out = build_bridge_out(
            OutgoingEvent::Dtmf {
                digit: '5',
                duration_ms: 120,
                method: DtmfMethod::Rfc2833,
            },
            CallId::new("c"),
            7,
        );
        let BridgeOut::Dtmf {
            call_id,
            seq,
            digit,
            ..
        } = out
        else {
            panic!("expected Dtmf");
        };
        assert_eq!(call_id.as_str(), "c");
        assert_eq!(seq, 7);
        assert_eq!(digit, '5');
    }

    #[test]
    fn bridge_in_call_id_extracts_from_each_variant() {
        for msg in [
            BridgeIn::Clear {
                call_id: CallId::new("a"),
            },
            BridgeIn::Mark {
                call_id: CallId::new("b"),
                name: "x".into(),
            },
            BridgeIn::Hangup {
                call_id: CallId::new("c"),
                cause: HangupCause::Normal,
            },
            BridgeIn::Transfer {
                call_id: CallId::new("d"),
                target: "sip:x".into(),
            },
            BridgeIn::SendDtmf {
                call_id: CallId::new("e"),
                digit: '1',
                duration_ms: 80,
            },
            BridgeIn::Mute {
                call_id: CallId::new("f"),
            },
            BridgeIn::Unmute {
                call_id: CallId::new("g"),
            },
        ] {
            assert!(!bridge_in_call_id(&msg).is_empty());
        }
    }

    // ─── normalize_auth_header ─────────────────────────────────────

    #[test]
    fn normalize_auth_header_passes_bearer_through() {
        assert_eq!(normalize_auth_header("Bearer abc"), "Bearer abc");
    }

    #[test]
    fn normalize_auth_header_passes_basic_through() {
        assert_eq!(
            normalize_auth_header("Basic dXNlcjpwYXNz"),
            "Basic dXNlcjpwYXNz"
        );
    }

    #[test]
    fn normalize_auth_header_passes_digest_through() {
        assert_eq!(
            normalize_auth_header("Digest username=\"foo\""),
            "Digest username=\"foo\""
        );
    }

    #[test]
    fn normalize_auth_header_promotes_bare_token_to_bearer() {
        // Historic UX: operators wrote just the token for Bearer auth.
        assert_eq!(normalize_auth_header("xyz123"), "Bearer xyz123");
    }

    #[test]
    fn normalize_auth_header_trims_outer_whitespace() {
        assert_eq!(normalize_auth_header("  Bearer abc  "), "Bearer abc");
        assert_eq!(normalize_auth_header("  xyz123  "), "Bearer xyz123");
    }
}
