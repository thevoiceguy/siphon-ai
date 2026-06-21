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
//! ## Liveness
//!
//! Two timers guard against a non-responsive WS server (both
//! configurable under `[bridge]`; a zero duration disables each):
//!
//! - **Keepalive** (PROTOCOL.md §5.6): a WS Ping every
//!   `ws_ping_interval_secs` (default 15 s); if no Pong lands within
//!   `ws_pong_timeout_secs` (default 10 s) the connection is half-open
//!   and the session drops with [`BridgeError::KeepaliveTimeout`]
//!   (reconnect-eligible — see `core`'s 0.7.3 reconnect path).
//! - **Start-deadline** (PROTOCOL.md §3.1): the server must send its
//!   first audio frame (or `hangup`) within `server_start_deadline_secs`
//!   (default 5 s) of `start`, else the call is torn down with
//!   `error { code: "server_too_slow" }`
//!   ([`DisconnectReason::ServerTooSlow`], a definitive teardown).

use std::time::Duration;

use futures::{SinkExt, StreamExt};
use thiserror::Error;
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::{
    client::IntoClientRequest,
    http::{HeaderValue, Request},
    protocol::{frame::coding::CloseCode, CloseFrame, Message, WebSocketConfig},
    Error as WsError,
};
use tokio_tungstenite::{connect_async_with_config, MaybeTlsStream, WebSocketStream};
use tracing::{debug, info, instrument, warn};

use crate::protocol::{
    BridgeIn, BridgeOut, CallId, ConferenceLeftReason, DtmfMethod, ErrorCode, Seq, StartMsg,
    StopReason, WS_SUBPROTOCOL,
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
    /// mTLS settings for the bridge connection. `None` = use the
    /// existing plaintext / webpki-validated path. `Some(_)` = a
    /// rustls `ClientConfig` is built carrying the operator's client
    /// cert + optional SPKI pin and handed to tokio-tungstenite's
    /// `Connector::Rustls`. See [`crate::tls`] for the verifier shape.
    pub tls: Option<crate::tls::BridgeTlsConfig>,
    /// WS keepalive ping cadence (PROTOCOL.md §5.6). The conn sends a WS
    /// Ping every interval and tears the session down if no Pong arrives
    /// within [`Self::pong_timeout`]. `Duration::ZERO` on either disables
    /// keepalive. Sourced from `[bridge].ws_ping_interval_secs`
    /// (default 15 s).
    pub ping_interval: Duration,
    /// Pong deadline for [`Self::ping_interval`] (default 10 s);
    /// `[bridge].ws_pong_timeout_secs`. `Duration::ZERO` disables.
    pub pong_timeout: Duration,
    /// `server_too_slow` start-deadline (PROTOCOL.md §3.1): the server
    /// must send its first audio frame (or `hangup`) within this window
    /// of `start`, else the call is torn down with
    /// `error { code: "server_too_slow" }`. `Duration::ZERO` disables.
    /// Sourced from `[bridge].server_start_deadline_secs` (default 5 s).
    pub start_deadline: Duration,
}

impl Default for BridgeConfig {
    fn default() -> Self {
        Self {
            ws_url: String::new(),
            auth_header: None,
            connect_timeout: Duration::from_secs(5),
            tls: None,
            // Disabled by default so unit/integration fixtures that build a
            // bare `BridgeConfig` keep their prior behaviour. The daemon
            // path (`acceptor::build_bridge_config`) always populates these
            // from `[bridge]` (defaults 15 s / 10 s / 5 s).
            ping_interval: Duration::ZERO,
            pong_timeout: Duration::ZERO,
            start_deadline: Duration::ZERO,
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
    /// A bot-initiated [`BridgeIn::Hold`] re-INVITE succeeded (0.7.2).
    /// The conn stamps `seq` and emits [`BridgeOut::Held`]. Distinct
    /// from [`Self::Hold`], which reports that the *far end* held us.
    Held,
    /// A bot-initiated [`BridgeIn::Resume`] re-INVITE restored two-way
    /// audio (0.7.2) → [`BridgeOut::Resumed`].
    Resumed,
    /// Caller has been silent (no VAD speech) for at least
    /// `duration_ms`. Configurable via `[bridge].silence_threshold_ms`;
    /// `0` disables. Fires once per silence stretch — the next event
    /// only after a speech-then-silence cycle.
    SilenceDetected {
        duration_ms: u64,
    },
    /// No audio in EITHER direction (no caller VAD speech AND no
    /// outbound playout from the WS server) for at least
    /// `duration_ms`. Suspect connectivity / hung call. Configurable
    /// via `[bridge].dead_air_threshold_ms`; `0` disables. Fires
    /// every time the threshold is crossed without either side
    /// producing audio.
    DeadAirDetected {
        duration_ms: u64,
    },
    /// Periodic snapshot of RTP / RTCP quality, emitted every
    /// `[bridge].rtp_stats_interval_ms` (default 5 s). Fields are
    /// `None` when forge has not yet reported a value (e.g. early
    /// in the call before the first RTCP report arrives).
    RtpStats {
        /// Estimated inter-arrival jitter in milliseconds. `None`
        /// until forge has reported its first quality assessment.
        jitter_ms: Option<f32>,
        /// Packet loss as a ratio in `[0.0, 1.0]`. `None` until
        /// forge has reported its first quality assessment.
        packet_loss_ratio: Option<f32>,
        /// Mean RTT in milliseconds. `None` until forge originates
        /// its own SRs (0.3.1 follow-up).
        rtcp_rtt_ms: Option<f32>,
    },
    Stop {
        reason: StopReason,
    },
    Error {
        code: ErrorCode,
        message: String,
    },
    /// A recording began. The conn stamps `seq` and emits
    /// [`BridgeOut::RecordingStarted`].
    RecordingStarted {
        recording_id: String,
    },
    /// A recording finalized → [`BridgeOut::RecordingStopped`].
    RecordingStopped {
        recording_id: String,
    },
    /// A recording failed → [`BridgeOut::RecordingFailed`].
    RecordingFailed {
        recording_id: String,
        reason: String,
    },
    /// This call joined a conference room → [`BridgeOut::ConferenceJoined`].
    /// The conn stamps `call_id` + `seq`.
    ConferenceJoined {
        room_id: String,
        participants: usize,
    },
    /// This call left a conference room → [`BridgeOut::ConferenceLeft`].
    ConferenceLeft {
        room_id: String,
        reason: ConferenceLeftReason,
    },
    /// Another call joined this call's room → [`BridgeOut::ParticipantJoined`].
    ParticipantJoined {
        room_id: String,
        participant_call_id: String,
    },
    /// Another call left this call's room → [`BridgeOut::ParticipantLeft`].
    ParticipantLeft {
        room_id: String,
        participant_call_id: String,
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
    /// The server didn't send its first audio frame (or `hangup`) within
    /// the start-deadline (PROTOCOL.md §3.1). The conn emitted
    /// `error { code: "server_too_slow" }` + `stop` and closed. A
    /// definitive teardown — never reconnect-eligible (redialing the same
    /// slow server wouldn't help).
    ServerTooSlow,
    /// The server sent an invalid WS message — malformed JSON, an unknown
    /// `type`, or a `call_id` that doesn't match the connection
    /// (PROTOCOL.md §3.10 `protocol_error`). The conn emitted
    /// `error { code: "protocol_error" }` + `stop` and closed. A
    /// definitive teardown — never reconnect-eligible (a buggy server
    /// would just repeat the violation).
    ProtocolError,
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

    // Note: malformed JSON, an unknown `type`, and a mismatched `call_id`
    // are no longer surfaced as `BridgeError`s — since 0.14.0 they emit
    // `error { code: "protocol_error" }` + `stop` and return
    // `DisconnectReason::ProtocolError` (a definitive teardown) instead.
    #[error("internal: {0}")]
    Internal(String),

    /// No Pong arrived within the keepalive deadline (PROTOCOL.md §5.6) —
    /// a half-open connection. Treated as an unexpected drop, so it is
    /// reconnect-eligible when `[bridge].ws_reconnect_enabled`.
    #[error("ws keepalive timeout (no pong within {0:?})")]
    KeepaliveTimeout(Duration),
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
    start: StartMsg,
    channels: BridgeChannels,
) -> Result<DisconnectReason, BridgeError> {
    connect_and_run_with_ready(config, start, channels, None).await
}

/// [`connect_and_run`] plus an optional readiness signal: `ready_tx` (when
/// `Some`) fires **once** the WS handshake has succeeded and the `start`
/// message has been written to the socket. The reconnect drive (0.7.3)
/// uses it to keep the caller on hold music until a redial is actually
/// live, rather than dropping MOH optimistically on a socket that may
/// still fail. A dropped receiver is ignored (the send is best-effort).
pub async fn connect_and_run_with_ready(
    config: BridgeConfig,
    mut start: StartMsg,
    channels: BridgeChannels,
    ready_tx: Option<oneshot::Sender<()>>,
) -> Result<DisconnectReason, BridgeError> {
    start.seq = 0;
    let call_id = start.call_id.clone();

    let request = build_upgrade_request(&config, &call_id)?;

    let ws_config = WebSocketConfig {
        max_message_size: Some(MAX_TEXT_BYTES),
        max_frame_size: Some(MAX_TEXT_BYTES),
        ..Default::default()
    };

    // Pick the connector based on whether [bridge.tls] is configured.
    // No `tls` field → existing plaintext / webpki path
    // (`connect_async_with_config` uses tokio-tungstenite's default
    // connector, which itself does webpki validation for `wss://`).
    // `tls` set → custom `Connector::Rustls` carrying the operator's
    // client cert + optional SPKI pin.
    let (ws, response) = if let Some(tls_cfg) = &config.tls {
        use tokio_tungstenite::{connect_async_tls_with_config, Connector};
        let client_config = tls_cfg
            .to_rustls_config()
            .map_err(|e| BridgeError::InvalidConfig(format!("rustls config: {e}")))?;
        let connector = Some(Connector::Rustls(std::sync::Arc::new(client_config)));
        let connect_fut = connect_async_tls_with_config(request, Some(ws_config), false, connector);
        match tokio::time::timeout(config.connect_timeout, connect_fut).await {
            Ok(result) => result?,
            Err(_) => return Err(BridgeError::ConnectTimeout(config.connect_timeout)),
        }
    } else {
        let connect_fut = connect_async_with_config(request, Some(ws_config), false);
        match tokio::time::timeout(config.connect_timeout, connect_fut).await {
            Ok(result) => result?,
            Err(_) => return Err(BridgeError::ConnectTimeout(config.connect_timeout)),
        }
    };

    if let Some(echoed) = response.headers().get("sec-websocket-protocol") {
        if echoed.as_bytes() != WS_SUBPROTOCOL.as_bytes() {
            warn!(echoed = ?echoed, "server echoed an unexpected subprotocol; proceeding");
        }
    } else {
        debug!("server did not echo a subprotocol; proceeding optimistically");
    }
    info!("bridge connected");

    let liveness = Liveness {
        ping_interval: config.ping_interval,
        pong_timeout: config.pong_timeout,
        start_deadline: config.start_deadline,
    };
    run_loop(ws, start, channels, call_id, ready_tx, liveness).await
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

/// Per-session liveness timers resolved from [`BridgeConfig`]. A
/// `Duration::ZERO` disables the corresponding guard.
struct Liveness {
    ping_interval: Duration,
    pong_timeout: Duration,
    start_deadline: Duration,
}

async fn run_loop(
    ws: WsStream,
    start: StartMsg,
    channels: BridgeChannels,
    call_id: CallId,
    ready_tx: Option<oneshot::Sender<()>>,
    liveness: Liveness,
) -> Result<DisconnectReason, BridgeError> {
    let BridgeChannels {
        mut audio_out_rx,
        mut control_out_rx,
        audio_in_tx,
        control_in_tx,
    } = channels;

    let (mut sink, mut stream) = ws.split();

    // Expected inbound audio-frame size from the negotiated format, for the
    // §2.2 `audio_format` check. PCM16 = 2 bytes/sample; e.g. 8 kHz/20 ms/mono
    // = 320 B, 16 kHz = 640 B. `0` (an unset/odd format) disables the check.
    // Captured before `start` is moved into the `Start` message below.
    let expected_audio_bytes = (start.audio.sample_rate as usize / 1000)
        * start.audio.frame_ms as usize
        * start.audio.channels as usize
        * 2;

    // Send `start` as the first message. `seq = 0` already enforced.
    let start_json = serde_json::to_string(&BridgeOut::Start(start))
        .map_err(|e| BridgeError::Internal(format!("serialize start: {e}")))?;
    sink.send(Message::Text(start_json)).await?;

    // Handshake done and `start` is on the wire — signal readiness so a
    // reconnect drive can drop hold music now (0.7.3). Best-effort.
    if let Some(tx) = ready_tx {
        let _ = tx.send(());
    }

    // Subsequent SiphonAI→server messages use seq starting at 1.
    let mut seq: Seq = 1;

    // --- WS keepalive (PROTOCOL.md §5.6) -----------------------------
    // Send a Ping every `ping_interval`; if no Pong lands within
    // `pong_timeout` of an outstanding ping, the connection is half-open
    // → tear down (reconnect-eligible). Either duration zero disables it.
    let keepalive_on = !liveness.ping_interval.is_zero() && !liveness.pong_timeout.is_zero();
    let ping_period = if keepalive_on {
        liveness.ping_interval
    } else {
        // Parked far out; the branch is guarded off anyway. A non-zero
        // period is required — `interval*` panics on a zero period.
        Duration::from_secs(3600)
    };
    // `interval_at` so the FIRST ping fires one period out, not immediately.
    let mut ping_timer =
        tokio::time::interval_at(tokio::time::Instant::now() + ping_period, ping_period);
    ping_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // Absolute deadline an outstanding ping must be ponged by; `None` =
    // no ping in flight.
    let mut pong_deadline: Option<tokio::time::Instant> = None;

    // --- server_too_slow start-deadline (PROTOCOL.md §3.1) -----------
    // Armed now (`start` is on the wire); disarmed by the first inbound
    // audio frame or a `hangup`. `None` = disabled or already satisfied.
    let mut start_deadline: Option<tokio::time::Instant> = (!liveness.start_deadline.is_zero())
        .then(|| tokio::time::Instant::now() + liveness.start_deadline);

    // Rate-limit `audio_format` emits (§2.2): a server stuck sending
    // wrong-size frames shouldn't flood the WS with one error per frame
    // (~50/s). Emit on the first bad frame, then at most one per second.
    let mut last_audio_format_emit: Option<tokio::time::Instant> = None;

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
                        // §2.2 audio_format: a frame whose size doesn't
                        // match the negotiated format is a server bug.
                        // Drop it and tell the server (rate-limited,
                        // NON-fatal — no `stop`), but keep the call up:
                        // one bad frame mustn't kill a live call, and
                        // persistent failure is caught by the dead-air /
                        // rtp watchdog. A dropped frame does NOT satisfy
                        // the start-deadline.
                        if expected_audio_bytes > 0 && data.len() != expected_audio_bytes {
                            let now = tokio::time::Instant::now();
                            let emit = last_audio_format_emit
                                .map(|t| now.duration_since(t) >= Duration::from_secs(1))
                                .unwrap_or(true);
                            if emit {
                                last_audio_format_emit = Some(now);
                                warn!(call_id = %call_id, got = data.len(),
                                    expected = expected_audio_bytes,
                                    "dropping wrong-size audio frame (audio_format)");
                                let err = BridgeOut::Error {
                                    call_id: call_id.clone(),
                                    seq,
                                    code: ErrorCode::AudioFormat,
                                    message: format!(
                                        "expected {expected_audio_bytes}-byte PCM16 frame, got {}",
                                        data.len()
                                    ),
                                };
                                seq = seq.wrapping_add(1);
                                sink.send(Message::Text(serialize_or_drop(&err))).await?;
                            }
                            continue;
                        }
                        // First (valid) server audio satisfies the
                        // start-deadline (§3.1) — the strict "begin
                        // sending audio" rule.
                        start_deadline = None;
                        if audio_in_tx.send(data).await.is_err() {
                            return Ok(DisconnectReason::ControllerHungUp);
                        }
                    }
                    Some(Ok(Message::Text(text))) => {
                        // §3.10 protocol_error: malformed JSON or an
                        // unknown `type` (serde rejects both identically)
                        // is a fatal protocol violation. Tell the server
                        // (`error{protocol_error}` + `stop`) then close.
                        // Definitive teardown — a buggy server would just
                        // repeat it, so NOT reconnect-eligible.
                        let parsed: BridgeIn = match serde_json::from_str(&text) {
                            Ok(p) => p,
                            Err(e) => {
                                warn!(call_id = %call_id, error = %e,
                                    "malformed or unknown WS message from server");
                                let _ = emit_fatal(&mut sink, &call_id, &mut seq,
                                    ErrorCode::ProtocolError,
                                    "malformed or unknown message").await;
                                let _ = close_clean(&mut sink).await;
                                return Ok(DisconnectReason::ProtocolError);
                            }
                        };
                        let got = bridge_in_call_id(&parsed);
                        if got != call_id.as_str() {
                            warn!(call_id = %call_id, got,
                                "WS message call_id does not match the connection");
                            let _ = emit_fatal(&mut sink, &call_id, &mut seq,
                                ErrorCode::ProtocolError,
                                "call_id does not match the connection").await;
                            let _ = close_clean(&mut sink).await;
                            return Ok(DisconnectReason::ProtocolError);
                        }
                        // Debug-level: every received control message
                        // (Clear, Mark, Hangup, Transfer, SendDtmf).
                        // §11.8 Q9 in DEV_PLAN.md — operators bump
                        // `siphon_ai_bridge=debug` via /admin/log to
                        // see exactly what the WS server sent. Audio
                        // frames live one notch lower (trace).
                        tracing::debug!(?parsed, "ws inbound control");
                        // A `hangup` also satisfies the start-deadline:
                        // the server chose to end the call rather than
                        // speak (§3.1 — "or send `hangup`").
                        if matches!(parsed, BridgeIn::Hangup { .. }) {
                            start_deadline = None;
                        }
                        if control_in_tx.send(parsed).await.is_err() {
                            return Ok(DisconnectReason::ControllerHungUp);
                        }
                    }
                    Some(Ok(Message::Ping(payload))) => {
                        sink.send(Message::Pong(payload)).await?;
                    }
                    Some(Ok(Message::Pong(_))) => {
                        // Liveness ack — clear the outstanding-ping deadline.
                        pong_deadline = None;
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

            // --- keepalive: send a Ping, arm the pong deadline ---------
            _ = ping_timer.tick(), if keepalive_on => {
                // A send error means the socket is already gone — surface
                // it as a (reconnect-eligible) WS error.
                sink.send(Message::Ping(Vec::new())).await?;
                if pong_deadline.is_none() {
                    pong_deadline = Some(tokio::time::Instant::now() + liveness.pong_timeout);
                }
            }

            // --- keepalive: no Pong within the deadline → half-open ----
            _ = async { tokio::time::sleep_until(pong_deadline.unwrap()).await },
                if pong_deadline.is_some() =>
            {
                warn!(call_id = %call_id, timeout = ?liveness.pong_timeout,
                    "ws keepalive timeout — no pong; dropping session");
                // Best-effort fatal `error` + `stop` (§5.6 / §3.10). The
                // peer is unresponsive, so bound the emit with a short
                // timeout and ignore failures, then report the drop.
                let _ = tokio::time::timeout(
                    Duration::from_secs(1),
                    emit_fatal(&mut sink, &call_id, &mut seq,
                        ErrorCode::Internal, "ws keepalive timeout"),
                )
                .await;
                return Err(BridgeError::KeepaliveTimeout(liveness.pong_timeout));
            }

            // --- start-deadline: server never spoke → server_too_slow --
            _ = async { tokio::time::sleep_until(start_deadline.unwrap()).await },
                if start_deadline.is_some() =>
            {
                warn!(call_id = %call_id, deadline = ?liveness.start_deadline,
                    "server sent no audio within start-deadline — server_too_slow");
                // The connection is healthy here, so the `error` + `stop`
                // will actually reach the server before we close.
                let _ = emit_fatal(&mut sink, &call_id, &mut seq,
                    ErrorCode::ServerTooSlow, "no audio within start deadline").await;
                let _ = close_clean(&mut sink).await;
                return Ok(DisconnectReason::ServerTooSlow);
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

/// Emit a fatal `error` followed by `stop { reason: error }` — the §3.10
/// invariant that a fatal error is always the penultimate message and
/// `stop` the last. Used by the liveness timeouts; `seq` is advanced for
/// each. Returns the first send error so callers can decide whether the
/// peer is reachable (the keepalive path treats it as best-effort).
async fn emit_fatal<S>(
    sink: &mut S,
    call_id: &CallId,
    seq: &mut Seq,
    code: ErrorCode,
    message: &str,
) -> Result<(), BridgeError>
where
    S: SinkExt<Message, Error = WsError> + Unpin,
{
    let err = BridgeOut::Error {
        call_id: call_id.clone(),
        seq: *seq,
        code,
        message: message.to_string(),
    };
    *seq = seq.wrapping_add(1);
    sink.send(Message::Text(serialize_or_drop(&err))).await?;
    let stop = BridgeOut::Stop {
        call_id: call_id.clone(),
        seq: *seq,
        reason: StopReason::Error,
    };
    *seq = seq.wrapping_add(1);
    sink.send(Message::Text(serialize_or_drop(&stop))).await?;
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
        OutgoingEvent::Held => BridgeOut::Held { call_id, seq },
        OutgoingEvent::Resumed => BridgeOut::Resumed { call_id, seq },
        OutgoingEvent::SilenceDetected { duration_ms } => BridgeOut::SilenceDetected {
            call_id,
            seq,
            duration_ms,
        },
        OutgoingEvent::DeadAirDetected { duration_ms } => BridgeOut::DeadAirDetected {
            call_id,
            seq,
            duration_ms,
        },
        OutgoingEvent::RtpStats {
            jitter_ms,
            packet_loss_ratio,
            rtcp_rtt_ms,
        } => BridgeOut::RtpStats {
            call_id,
            seq,
            jitter_ms,
            packet_loss_ratio,
            rtcp_rtt_ms,
        },
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
        OutgoingEvent::RecordingStarted { recording_id } => BridgeOut::RecordingStarted {
            call_id,
            seq,
            recording_id,
        },
        OutgoingEvent::RecordingStopped { recording_id } => BridgeOut::RecordingStopped {
            call_id,
            seq,
            recording_id,
        },
        OutgoingEvent::RecordingFailed {
            recording_id,
            reason,
        } => BridgeOut::RecordingFailed {
            call_id,
            seq,
            recording_id,
            reason,
        },
        OutgoingEvent::ConferenceJoined {
            room_id,
            participants,
        } => BridgeOut::ConferenceJoined {
            call_id,
            seq,
            room_id,
            participants,
        },
        OutgoingEvent::ConferenceLeft { room_id, reason } => BridgeOut::ConferenceLeft {
            call_id,
            seq,
            room_id,
            reason,
        },
        OutgoingEvent::ParticipantJoined {
            room_id,
            participant_call_id,
        } => BridgeOut::ParticipantJoined {
            call_id,
            seq,
            room_id,
            participant_call_id,
        },
        OutgoingEvent::ParticipantLeft {
            room_id,
            participant_call_id,
        } => BridgeOut::ParticipantLeft {
            call_id,
            seq,
            room_id,
            participant_call_id,
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
        BridgeIn::StartRecording { call_id } => call_id.as_str(),
        BridgeIn::StopRecording { call_id } => call_id.as_str(),
        BridgeIn::PauseRecording { call_id } => call_id.as_str(),
        BridgeIn::ResumeRecording { call_id } => call_id.as_str(),
        BridgeIn::ConferenceJoin { call_id, .. } => call_id.as_str(),
        BridgeIn::ConferenceLeave { call_id } => call_id.as_str(),
        BridgeIn::Park { call_id, .. } => call_id.as_str(),
        BridgeIn::Hold { call_id } => call_id.as_str(),
        BridgeIn::Resume { call_id } => call_id.as_str(),
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
    fn build_bridge_out_maps_held_and_resumed() {
        // The bot-initiated hold acks (0.7.2) — distinct from the
        // peer-hold Hold/Resume events — stamp call_id + seq.
        let held = build_bridge_out(OutgoingEvent::Held, CallId::new("c"), 3);
        assert!(matches!(held, BridgeOut::Held { seq: 3, .. }));
        let resumed = build_bridge_out(OutgoingEvent::Resumed, CallId::new("c"), 4);
        assert!(matches!(resumed, BridgeOut::Resumed { seq: 4, .. }));
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
                target: Some("sip:x".into()),
                replaces_call_id: None,
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
