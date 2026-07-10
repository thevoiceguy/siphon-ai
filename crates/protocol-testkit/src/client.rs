//! Thin WS client playing the daemon's side of the bridge protocol.
//!
//! Deliberately NOT `siphon_ai_bridge::conn` — that module is entangled
//! with the daemon's call machinery (reconnect state, media channels).
//! The testkit needs a bare socket it can drive step-by-step.

use anyhow::{bail, Context, Result};
use futures::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use siphon_ai_bridge::WS_SUBPROTOCOL;

/// One inbound frame, as the step runner sees it.
#[derive(Debug)]
pub enum Incoming {
    Text(String),
    Binary(Vec<u8>),
    Pong,
    /// Server closed (code, reason) — `None` when the socket dropped
    /// without a close handshake.
    Closed(Option<(u16, String)>),
}

pub struct WsClient {
    ws: WebSocketStream<MaybeTlsStream<TcpStream>>,
}

impl WsClient {
    /// Connect and require the server to echo the `siphon-ai.v1`
    /// subprotocol, exactly as the daemon does.
    pub async fn connect(url: &str) -> Result<Self> {
        let mut request = url
            .into_client_request()
            .with_context(|| format!("bad WebSocket URL `{url}`"))?;
        request.headers_mut().insert(
            "Sec-WebSocket-Protocol",
            HeaderValue::from_static(WS_SUBPROTOCOL),
        );
        let (ws, response) = tokio_tungstenite::connect_async(request)
            .await
            .with_context(|| format!("WebSocket connect to `{url}` failed"))?;
        let negotiated = response
            .headers()
            .get("Sec-WebSocket-Protocol")
            .and_then(|v| v.to_str().ok());
        if negotiated != Some(WS_SUBPROTOCOL) {
            bail!(
                "server did not negotiate subprotocol `{WS_SUBPROTOCOL}` \
                 (got {negotiated:?}) — PROTOCOL.md §2.1 requires echoing it"
            );
        }
        Ok(Self { ws })
    }

    pub async fn send_text(&mut self, text: String) -> Result<()> {
        self.ws
            .send(Message::Text(text))
            .await
            .context("WS text send failed")
    }

    pub async fn send_binary(&mut self, frame: Vec<u8>) -> Result<()> {
        self.ws
            .send(Message::Binary(frame))
            .await
            .context("WS binary send failed")
    }

    pub async fn send_ping(&mut self) -> Result<()> {
        self.ws
            .send(Message::Ping(Vec::new()))
            .await
            .context("WS ping send failed")
    }

    /// Next protocol-relevant frame. Never blocks past the caller's
    /// surrounding timeout (use `tokio::time::timeout`).
    pub async fn recv(&mut self) -> Result<Incoming> {
        loop {
            match self.ws.next().await {
                None => return Ok(Incoming::Closed(None)),
                Some(Err(e)) => {
                    // A peer that vanishes mid-read is a drop, not an error
                    // the runner needs to distinguish further.
                    tracing::debug!("ws read error treated as drop: {e}");
                    return Ok(Incoming::Closed(None));
                }
                Some(Ok(Message::Text(t))) => return Ok(Incoming::Text(t.to_string())),
                Some(Ok(Message::Binary(b))) => return Ok(Incoming::Binary(b.to_vec())),
                Some(Ok(Message::Pong(_))) => return Ok(Incoming::Pong),
                Some(Ok(Message::Ping(_))) => continue, // auto-ponged by tungstenite
                Some(Ok(Message::Close(frame))) => {
                    return Ok(Incoming::Closed(
                        frame.map(|f| (f.code.into(), f.reason.to_string())),
                    ));
                }
                Some(Ok(Message::Frame(_))) => continue,
            }
        }
    }

    /// Clean close (1000), waiting briefly for the server's close reply.
    pub async fn close(&mut self) -> Result<()> {
        let _ = self
            .ws
            .close(Some(CloseFrame {
                code: CloseCode::Normal,
                reason: "".into(),
            }))
            .await;
        // Drain until the close handshake completes or the peer drops.
        let drain = async {
            while let Some(msg) = self.ws.next().await {
                if msg.is_err() || matches!(msg, Ok(Message::Close(_))) {
                    break;
                }
            }
        };
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), drain).await;
        Ok(())
    }

    /// Abrupt drop — no close handshake. What an unexpected WS drop
    /// looks like from the server's side (PROTOCOL.md §5.7).
    pub fn abort(self) {
        drop(self.ws);
    }
}
