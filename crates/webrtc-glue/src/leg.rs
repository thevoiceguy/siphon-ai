//! The browser's media leg: answer, connect, tear down.
//!
//! Wraps forge-webrtc's `PeerConnection` in the shape siphon-ai's call
//! machinery wants — an offer goes in, an answer comes out, and the
//! caller learns when media may flow (`DEV_PLAN_WebRTC.md` §4.2).
//!
//! # Complete gathering before the answer
//!
//! v1 does **no SIP trickle** (§4.2). Instead both ends gather fully
//! before signalling: SIP.js waits for its own gathering before
//! sending the INVITE — which is why the captured Chrome offer already
//! carries its candidates — and [`WebRtcLeg::answer`] waits for
//! `GatheringComplete` before building the answer, so the SDP the
//! browser receives is final. On a host-candidate-only server that is
//! milliseconds; with STUN it is one round trip, which is why the wait
//! is bounded rather than unconditional: a STUN server that black-holes
//! must not stall a call, and the host candidates already gathered are
//! a perfectly good answer.

use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Duration;

use forge_core::AudioCodec;
use forge_webrtc::{ConnectionState, PeerConnection, PeerEvent};
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::{Result, WebRtcGlueError, WebRtcSettings};

/// How long [`WebRtcLeg::answer`] waits for candidate gathering before
/// answering with what it has. Host candidates are immediate; this
/// budget exists for the STUN round trip, and expiring is a
/// degradation (fewer candidates), not a failure.
pub const GATHER_BUDGET: Duration = Duration::from_secs(2);

/// A leg that has answered and is establishing ICE/DTLS.
pub struct WebRtcLeg {
    peer: PeerConnection,
    codec: AudioCodec,
    payload_type: u8,
}

/// What [`WebRtcLeg::answer`] produces: the leg, the SDP to put in the
/// 200 OK, and the peer's event stream (candidates already gathered,
/// so what remains is state changes and media).
pub struct Answered {
    pub leg: WebRtcLeg,
    pub answer_sdp: String,
    pub events: mpsc::Receiver<PeerEvent>,
}

/// Where a browser leg is in ICE/DTLS, readable from **outside** the
/// task that owns the leg (`DEV_PLAN_WebRTC.md` §4.6 — the sightglass
/// item).
///
/// The peer connection lives inside its tap task and cannot be shared,
/// so the tap publishes its phase into this cell as it observes each
/// transition. An admin request reads it; nothing writes but the tap.
/// One relaxed atomic, so a per-call reader costs nothing and the
/// audio path is untouched (CLAUDE.md §4.3 — this is not a lock).
#[derive(Debug, Default)]
pub struct WebRtcLegState(AtomicU8);

impl WebRtcLegState {
    pub fn set(&self, phase: LegPhase) {
        self.0.store(phase as u8, Ordering::Relaxed);
    }

    pub fn phase(&self) -> LegPhase {
        LegPhase::from_u8(self.0.load(Ordering::Relaxed))
    }
}

/// The phases [`WebRtcLegState`] reports. Ordered as a call moves
/// through them; the strings are an admin-API vocabulary, so they are
/// stable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum LegPhase {
    /// Answered; ICE checks are running. Every leg starts here.
    Connecting = 0,
    /// ICE nominated a pair — there is a path — and DTLS is
    /// handshaking. A leg **stuck** here is the interesting one: the
    /// network works and the crypto does not.
    IceConnected = 1,
    /// DTLS finished, SRTP keys installed, media is flowing.
    Connected = 2,
    /// The transport failed, or setup ran out of budget.
    Failed = 3,
    /// Closed — normally, by the browser or by us.
    Closed = 4,
}

impl LegPhase {
    fn from_u8(v: u8) -> Self {
        match v {
            1 => LegPhase::IceConnected,
            2 => LegPhase::Connected,
            3 => LegPhase::Failed,
            4 => LegPhase::Closed,
            // Includes any value no `set` ever wrote, which cannot
            // happen — `Connecting` is the honest answer either way.
            _ => LegPhase::Connecting,
        }
    }

    /// Wire/display string. Stable: the admin API and sightglass both
    /// show it.
    pub fn as_str(self) -> &'static str {
        match self {
            LegPhase::Connecting => "connecting",
            LegPhase::IceConnected => "ice_connected",
            LegPhase::Connected => "connected",
            LegPhase::Failed => "failed",
            LegPhase::Closed => "closed",
        }
    }
}

/// How the ICE + DTLS setup phase ended, with the timings that
/// explain it (`DEV_PLAN_WebRTC.md` §4.6).
///
/// The split between the two timeout variants is the point: a browser
/// call with no audio has failed either at ICE (no path — NAT, a
/// firewall, a candidate we never reached) or at DTLS (a path exists
/// but the handshake did not finish), and those are different
/// problems with different fixes. `wait_connected`-style polling
/// cannot tell them apart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SetupOutcome {
    /// ICE nominated a pair and DTLS installed the SRTP keys: media
    /// may flow. The durations are `None` only when the transport
    /// connected before the wait began — see
    /// [`WebRtcLeg::wait_for_setup`].
    Connected {
        /// Wait start → ICE nomination.
        ice: Option<Duration>,
        /// ICE nomination → SRTP keys installed, i.e. the DTLS
        /// handshake itself.
        dtls: Option<Duration>,
    },
    /// The budget expired with no candidate pair nominated.
    IceTimeout,
    /// A pair was nominated, but DTLS never completed inside the
    /// budget.
    DtlsTimeout {
        /// How long ICE took, which succeeded.
        ice: Duration,
    },
    /// forge-webrtc gave up on the transport (its own ICE timeout, a
    /// DTLS error, a bad fingerprint).
    Failed(String),
    /// Closed — locally, or the event stream ended — before connecting.
    Closed,
}

impl SetupOutcome {
    /// Bounded label for the `result` dimension of
    /// `siphon_ai_webrtc_legs_total`.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Connected { .. } => "connected",
            Self::IceTimeout => "ice_timeout",
            Self::DtlsTimeout { .. } => "dtls_timeout",
            Self::Failed(_) => "failed",
            Self::Closed => "closed",
        }
    }
}

impl WebRtcLeg {
    /// Answer a browser's offer.
    ///
    /// Applies the remote description, waits (bounded) for local
    /// gathering, and produces the final answer SDP.
    /// `local_port` is where the media socket binds — the RTP port of
    /// a `PortReservation` drawn from the same pool a SIP call uses, or
    /// `0` to let the OS choose (tests, and only tests).
    pub async fn answer(
        offer_sdp: &str,
        settings: &WebRtcSettings,
        local_port: u16,
    ) -> Result<Answered> {
        Self::answer_with_gather_budget(offer_sdp, settings, local_port, GATHER_BUDGET).await
    }

    /// [`answer`](Self::answer) with an explicit gather budget — tests
    /// use a short one.
    pub async fn answer_with_gather_budget(
        offer_sdp: &str,
        settings: &WebRtcSettings,
        local_port: u16,
        gather_budget: Duration,
    ) -> Result<Answered> {
        let mut peer = PeerConnection::with_config(settings.peer_config_on_port(local_port))
            .await
            .map_err(|e| WebRtcGlueError::Setup(e.to_string()))?;

        // Applying the offer is what starts the transport (and so
        // gathering), so the events receiver must be taken after it.
        peer.set_remote_offer(offer_sdp)
            .await
            .map_err(|e| WebRtcGlueError::Offer(e.to_string()))?;
        let mut events = peer
            .take_events()
            .ok_or_else(|| WebRtcGlueError::Setup("peer event stream already taken".into()))?;

        // Drain candidate events until gathering finishes or the budget
        // expires. Candidates are inlined into the answer by
        // forge-webrtc, so nothing is lost by consuming the events —
        // and consuming them keeps the bounded channel from filling
        // while we wait.
        let gathered = wait_for_gathering(&mut events, gather_budget).await;
        if !gathered {
            warn!(
                budget_ms = gather_budget.as_millis(),
                "ICE gathering did not complete in time; answering with the \
                 candidates gathered so far"
            );
        }

        let answer_sdp = peer
            .create_answer()
            .await
            .map_err(|e| WebRtcGlueError::Answer(e.to_string()))?;
        let (codec, payload_type) = peer.negotiated_codec();
        debug!(
            connection_id = peer.connection_id(),
            ?codec,
            payload_type,
            gathered,
            "WebRTC leg answered"
        );

        Ok(Answered {
            leg: WebRtcLeg {
                peer,
                codec,
                payload_type,
            },
            answer_sdp,
            events,
        })
    }

    /// Wait for ICE + DTLS, watching the event stream. This is the
    /// `[webrtc].setup_timeout` budget: a browser that signalled but
    /// never completes media must not hold a call slot (§4.4).
    ///
    /// Driven by events rather than by polling the connection state
    /// (forge-webrtc offers both) because the two questions an
    /// operator asks of a browser call that never got audio —
    /// *did ICE nominate a pair?* and *how long did DTLS take?* — are
    /// only answerable if the transitions are observed as they happen.
    /// Polling collapses `IceConnected` and `Connected` into one
    /// "connected", which is the same reason §4.6's histograms exist.
    ///
    /// Takes the receiver the caller owns; everything after
    /// [`SetupOutcome::Connected`] (RTP, RTCP, `Closed`) stays queued
    /// for the caller's own loop.
    pub async fn wait_for_setup(
        &self,
        events: &mut mpsc::Receiver<PeerEvent>,
        budget: Duration,
        state: &WebRtcLegState,
    ) -> SetupOutcome {
        // The transport runs in its own task, so it can finish before
        // this is first called. Its state is the authority; the events
        // it already queued are then history we did not time, which is
        // what the `None` timings mean.
        match self.peer.get_state() {
            ConnectionState::Connected => {
                state.set(LegPhase::Connected);
                return SetupOutcome::Connected {
                    ice: None,
                    dtls: None,
                };
            }
            ConnectionState::Failed => {
                state.set(LegPhase::Failed);
                return SetupOutcome::Failed("transport failed before media started".into());
            }
            ConnectionState::Closed => {
                state.set(LegPhase::Closed);
                return SetupOutcome::Closed;
            }
            _ => {}
        }
        await_setup(events, budget, state).await
    }

    /// The negotiated codec and the payload type the browser expects.
    pub fn codec(&self) -> (AudioCodec, u8) {
        (self.codec, self.payload_type)
    }

    /// The PCM rate the WS bridge will see for this leg.
    ///
    /// **Not** the codec's RTP clock. Opus signals a 48 kHz clock on
    /// the wire but libopus decodes straight to the rate we ask for,
    /// so the bridge sees 16 kHz — exactly what a classic Opus call
    /// already delivers (`media-glue::sdp::Codec::audio_sample_rate`).
    /// Keeping the two paths on the same number is what lets a browser
    /// leg reuse the bridge's fixed 8/16 kHz frame contract untouched.
    pub fn bridge_sample_rate(&self) -> u32 {
        match self.codec {
            AudioCodec::Opus => 16_000,
            _ => 8_000,
        }
    }

    /// Current transport state.
    pub fn state(&self) -> ConnectionState {
        self.peer.get_state()
    }

    /// Stable id for logs/metrics.
    pub fn connection_id(&self) -> &str {
        self.peer.connection_id()
    }

    /// Borrow the peer connection (audio handles, renegotiation).
    pub fn peer(&self) -> &PeerConnection {
        &self.peer
    }

    /// Mutable borrow, for re-offer / rollback (hold, §4.2).
    pub fn peer_mut(&mut self) -> &mut PeerConnection {
        &mut self.peer
    }

    /// Close the transport. Idempotent.
    pub fn close(&mut self) {
        self.peer.close();
    }
}

/// Watch the event stream until the transport connects, fails, or the
/// budget expires.
///
/// Free-standing (rather than a method) so the state machine can be
/// tested against a synthetic event stream — building a real peer
/// that fails DTLS on demand is not something a unit test can do.
pub async fn await_setup(
    events: &mut mpsc::Receiver<PeerEvent>,
    budget: Duration,
    state: &WebRtcLegState,
) -> SetupOutcome {
    let start = tokio::time::Instant::now();
    let deadline = start + budget;
    let mut ice_at: Option<tokio::time::Instant> = None;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            state.set(LegPhase::Failed);
            return match ice_at {
                Some(at) => SetupOutcome::DtlsTimeout {
                    ice: at.duration_since(start),
                },
                None => SetupOutcome::IceTimeout,
            };
        }
        match tokio::time::timeout(remaining, events.recv()).await {
            Ok(Some(PeerEvent::IceConnected { local, remote })) => {
                ice_at = Some(tokio::time::Instant::now());
                // Published before DTLS starts, so a leg watched live
                // shows *which* phase it is stuck in rather than a
                // single "connecting" that could mean either.
                state.set(LegPhase::IceConnected);
                debug!(%local, %remote, "ICE nominated a pair");
            }
            Ok(Some(PeerEvent::Connected)) => {
                let now = tokio::time::Instant::now();
                state.set(LegPhase::Connected);
                return SetupOutcome::Connected {
                    ice: ice_at.map(|at| at.duration_since(start)),
                    // Measured from nomination because that is when
                    // forge-webrtc starts the handshake.
                    dtls: ice_at.map(|at| now.duration_since(at)),
                };
            }
            Ok(Some(PeerEvent::Failed(why))) => {
                state.set(LegPhase::Failed);
                return SetupOutcome::Failed(why);
            }
            Ok(Some(PeerEvent::Closed)) | Ok(None) => {
                state.set(LegPhase::Closed);
                return SetupOutcome::Closed;
            }
            // Late candidates and RTCP. Media cannot precede the SRTP
            // keys, so nothing audible is being skipped here.
            Ok(Some(_)) => {}
            // Only the deadline can expire the inner timeout, and the
            // top of the loop turns that into the right variant.
            Err(_) => {}
        }
    }
}

/// Consume events until `GatheringComplete`. Returns whether it
/// arrived within the budget.
async fn wait_for_gathering(events: &mut mpsc::Receiver<PeerEvent>, budget: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return false;
        }
        match tokio::time::timeout(remaining, events.recv()).await {
            Ok(Some(PeerEvent::GatheringComplete)) => return true,
            // Candidates are inlined into the answer by forge-webrtc;
            // draining them here just keeps the channel clear.
            Ok(Some(_)) => continue,
            // Transport gone before we answered — let the answer
            // attempt produce the real error.
            Ok(None) => return false,
            Err(_) => return false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHROME_OFFER: &str = include_str!("../fixtures/chrome-offer.sdp");

    #[tokio::test]
    async fn answers_the_real_chrome_offer_with_gathered_candidates() {
        let Answered {
            leg, answer_sdp, ..
        } = WebRtcLeg::answer(CHROME_OFFER, &WebRtcSettings::default(), 0)
            .await
            .expect("answer");

        assert!(answer_sdp.contains("a=setup:active"), "{answer_sdp}");
        assert!(
            answer_sdp.contains("a=candidate:"),
            "complete-gathering-before-answer means the answer carries \
             our candidates: {answer_sdp}"
        );
        // Opus by default → the bridge sees 16 kHz, same as a classic
        // Opus call.
        assert_eq!(leg.codec().0, AudioCodec::Opus);
        assert_eq!(leg.bridge_sample_rate(), 16_000);
    }

    #[tokio::test]
    async fn g711_leg_reports_the_8k_bridge_rate() {
        let settings = WebRtcSettings {
            prefer_g711: true,
            ..Default::default()
        };
        let Answered { leg, .. } = WebRtcLeg::answer(CHROME_OFFER, &settings, 0)
            .await
            .expect("answer");
        assert_eq!(leg.codec(), (AudioCodec::PCMU, 0));
        assert_eq!(leg.bridge_sample_rate(), 8_000);
    }

    /// A gather budget that expires must still produce an answer —
    /// degraded (fewer candidates), never a failed call.
    #[tokio::test]
    async fn expired_gather_budget_still_answers() {
        let Answered { answer_sdp, .. } = WebRtcLeg::answer_with_gather_budget(
            CHROME_OFFER,
            &WebRtcSettings::default(),
            0,
            Duration::from_millis(0),
        )
        .await
        .expect("answer even when gathering did not finish");
        assert!(answer_sdp.contains("a=ice-ufrag:"), "{answer_sdp}");
    }

    /// The two phases are timed separately, which is the reason the
    /// wait is event-driven at all. Paused clock: tokio advances time
    /// when every task is idle, so the sleeps below *are* the timings.
    #[tokio::test(start_paused = true)]
    async fn connected_times_ice_and_dtls_apart() {
        let (tx, mut rx) = mpsc::channel(8);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let addr: std::net::SocketAddr = "127.0.0.1:41000".parse().unwrap();
            tx.send(PeerEvent::IceConnected {
                local: addr,
                remote: addr,
            })
            .await
            .unwrap();
            tokio::time::sleep(Duration::from_millis(200)).await;
            tx.send(PeerEvent::Connected).await.unwrap();
        });

        let state = WebRtcLegState::default();
        assert_eq!(state.phase(), LegPhase::Connecting, "every leg starts here");
        let outcome = await_setup(&mut rx, Duration::from_secs(5), &state).await;
        assert_eq!(
            outcome,
            SetupOutcome::Connected {
                ice: Some(Duration::from_millis(50)),
                dtls: Some(Duration::from_millis(200)),
            }
        );
        assert_eq!(state.phase(), LegPhase::Connected);
    }

    /// Nothing nominated inside the budget: no path exists.
    #[tokio::test(start_paused = true)]
    async fn a_silent_transport_is_an_ice_timeout() {
        let (_tx, mut rx) = mpsc::channel::<PeerEvent>(8);
        let state = WebRtcLegState::default();
        let outcome = await_setup(&mut rx, Duration::from_millis(500), &state).await;
        assert_eq!(outcome, SetupOutcome::IceTimeout);
        assert_eq!(outcome.label(), "ice_timeout");
        assert_eq!(state.phase(), LegPhase::Failed);
    }

    /// A pair was nominated and then the handshake stalled — a
    /// different fault from "no path", and the whole reason these are
    /// two variants.
    #[tokio::test(start_paused = true)]
    async fn a_nominated_pair_that_never_handshakes_is_a_dtls_timeout() {
        let (tx, mut rx) = mpsc::channel(8);
        let addr: std::net::SocketAddr = "127.0.0.1:41000".parse().unwrap();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            tx.send(PeerEvent::IceConnected {
                local: addr,
                remote: addr,
            })
            .await
            .unwrap();
            // Hold the sender so the stream does not close, which
            // would be `Closed` rather than a timeout.
            tokio::time::sleep(Duration::from_secs(60)).await;
        });

        // The live phase is what an operator sees while it is stuck:
        // ICE succeeded, so the leg reports `ice_connected` until the
        // budget expires — which is the diagnosis, in one word.
        let state = WebRtcLegState::default();
        let peek = state.phase();
        let outcome = await_setup(&mut rx, Duration::from_millis(500), &state).await;
        assert_eq!(peek, LegPhase::Connecting);
        assert_eq!(
            outcome,
            SetupOutcome::DtlsTimeout {
                ice: Duration::from_millis(100)
            }
        );
        assert_eq!(outcome.label(), "dtls_timeout");
        assert_eq!(state.phase(), LegPhase::Failed);
    }

    #[tokio::test]
    async fn transport_failure_and_close_are_distinct_outcomes() {
        let (tx, mut rx) = mpsc::channel(8);
        tx.send(PeerEvent::Failed("dtls: bad fingerprint".into()))
            .await
            .unwrap();
        let state = WebRtcLegState::default();
        assert_eq!(
            await_setup(&mut rx, Duration::from_secs(1), &state).await,
            SetupOutcome::Failed("dtls: bad fingerprint".into())
        );
        assert_eq!(state.phase(), LegPhase::Failed);

        // A browser tab closing mid-setup drops the transport: a user
        // action, not a fault, and labeled as such.
        let (tx, mut rx) = mpsc::channel::<PeerEvent>(8);
        drop(tx);
        let state = WebRtcLegState::default();
        let outcome = await_setup(&mut rx, Duration::from_secs(1), &state).await;
        assert_eq!(outcome, SetupOutcome::Closed);
        assert_eq!(outcome.label(), "closed");
        assert_eq!(state.phase(), LegPhase::Closed);
    }

    /// Candidates and RTCP keep arriving during setup; they must not
    /// end the wait or restart the budget.
    #[tokio::test(start_paused = true)]
    async fn unrelated_events_do_not_end_the_wait() {
        let (tx, mut rx) = mpsc::channel(8);
        tokio::spawn(async move {
            for _ in 0..3 {
                tokio::time::sleep(Duration::from_millis(10)).await;
                tx.send(PeerEvent::GatheringComplete).await.unwrap();
            }
            tokio::time::sleep(Duration::from_secs(60)).await;
        });
        assert_eq!(
            await_setup(
                &mut rx,
                Duration::from_millis(200),
                &WebRtcLegState::default()
            )
            .await,
            SetupOutcome::IceTimeout
        );
    }

    #[tokio::test]
    async fn a_non_webrtc_offer_is_refused() {
        let plain = "v=0\r\no=- 1 1 IN IP4 192.0.2.1\r\ns=-\r\nc=IN IP4 192.0.2.1\r\n\
t=0 0\r\nm=audio 9000 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\n";
        let err = match WebRtcLeg::answer(plain, &WebRtcSettings::default(), 0).await {
            Err(e) => e,
            Ok(_) => panic!("a plain RTP offer has no ICE/fingerprint to build a leg from"),
        };
        assert!(matches!(err, WebRtcGlueError::Offer(_)), "{err:?}");
    }
}
