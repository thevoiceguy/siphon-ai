//! `IdleDetector` — derives `silence_detected` / `dead_air_detected`
//! events from VAD / playout activity timing.
//!
//! Pulled out of [`crate::tap::MediaTap`] so the detection logic is
//! testable without spinning up a forge session.
//!
//! ## Definitions
//!
//! - **Silence** is *one-sided*: the caller has not produced VAD
//!   speech for at least `silence_threshold`. Fires once per stretch
//!   of silence; the next event only after a speech → silence cycle.
//! - **Dead-air** is *two-sided*: neither caller VAD speech nor any
//!   outbound playout from the WS server has been observed for at
//!   least `dead_air_threshold`. Fires every time the threshold
//!   elapses without either side producing audio (re-anchors on each
//!   fire), so a hung call generates a steady drumbeat of events.
//!
//! Both thresholds are optional. `None` = disable that event entirely.

use std::time::{Duration, Instant};

/// What a [`IdleDetector::poll`] call wants the caller to emit on
/// the WS bridge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdleEvent {
    /// Caller silent (no VAD speech) for `duration_ms`. Suppresses
    /// re-firing until the next `note_speech_started`.
    SilenceDetected { duration_ms: u64 },
    /// Neither side has produced audio for `duration_ms`. Re-anchors
    /// the timer on every fire — a still-dead call will see another
    /// event every `dead_air_threshold` until something happens.
    DeadAirDetected { duration_ms: u64 },
}

/// Timing-only detector. Owns no I/O; the caller (typically the
/// media tap) feeds it event timestamps and polls it for derived
/// events on a regular cadence.
#[derive(Debug)]
pub struct IdleDetector {
    silence_threshold: Option<Duration>,
    dead_air_threshold: Option<Duration>,
    /// True between `note_speech_started` and `note_speech_stopped`:
    /// the caller is mid-utterance, so neither idle event may fire
    /// (#399 — measuring "silence" from the utterance's *onset* made
    /// any utterance longer than the threshold emit `silence_detected`
    /// while the caller was still talking).
    speech_active: bool,
    /// When the caller last **stopped** speaking (call start before
    /// any speech) — the anchor actual silence is measured from.
    last_speech_ended: Instant,
    last_any_audio: Instant,
    /// True once we've emitted `SilenceDetected` for the current
    /// silence stretch; cleared on the next `note_speech_started`.
    silence_fired: bool,
}

impl IdleDetector {
    /// `now` anchors both timers — pass [`Instant::now`] at tap
    /// construction time.
    pub fn new(
        silence_threshold: Option<Duration>,
        dead_air_threshold: Option<Duration>,
        now: Instant,
    ) -> Self {
        Self {
            silence_threshold,
            dead_air_threshold,
            speech_active: false,
            last_speech_ended: now,
            last_any_audio: now,
            silence_fired: false,
        }
    }

    /// True when at least one threshold is configured. The caller
    /// uses this to gate the poll-tick arm in its `select!` loop.
    pub fn is_active(&self) -> bool {
        self.silence_threshold.is_some() || self.dead_air_threshold.is_some()
    }

    /// VAD reported the caller started speaking. Marks speech active
    /// (gating both idle events until the matching
    /// [`Self::note_speech_stopped`]), notes the audio activity, and
    /// re-arms the once-per-stretch silence suppression.
    pub fn note_speech_started(&mut self, now: Instant) {
        self.speech_active = true;
        self.last_any_audio = now;
        self.silence_fired = false;
    }

    /// VAD reported the caller stopped speaking. Silence is measured
    /// from THIS moment (#399), and caller audio was flowing until it,
    /// so the dead-air anchor moves too. Safe on a stop without a
    /// matching start (forge emits one at call setup): both anchors
    /// just move forward.
    pub fn note_speech_stopped(&mut self, now: Instant) {
        self.speech_active = false;
        self.last_speech_ended = now;
        self.last_any_audio = now;
    }

    /// The WS server pushed audio toward the caller. Resets ONLY
    /// the dead-air timer — the caller may still be silent.
    pub fn note_ws_audio(&mut self, now: Instant) {
        self.last_any_audio = now;
    }

    /// Check the timers and return any events that should fire.
    /// Updates internal state (the silence-suppression flag and the
    /// dead-air anchor) so calling repeatedly is safe.
    pub fn poll(&mut self, now: Instant) -> Vec<IdleEvent> {
        let mut out = Vec::new();

        // Mid-utterance: the caller is neither silent nor is the call
        // dead — audio is arriving right now (#399).
        if self.speech_active {
            return out;
        }

        if let Some(threshold) = self.silence_threshold {
            if !self.silence_fired {
                let elapsed = now.saturating_duration_since(self.last_speech_ended);
                if elapsed >= threshold {
                    out.push(IdleEvent::SilenceDetected {
                        duration_ms: elapsed.as_millis() as u64,
                    });
                    self.silence_fired = true;
                }
            }
        }

        if let Some(threshold) = self.dead_air_threshold {
            let elapsed = now.saturating_duration_since(self.last_any_audio);
            if elapsed >= threshold {
                out.push(IdleEvent::DeadAirDetected {
                    duration_ms: elapsed.as_millis() as u64,
                });
                // Re-anchor: the next dead-air event fires after
                // another full threshold elapses, not continuously.
                self.last_any_audio = now;
            }
        }

        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn fresh_detector_fires_nothing() {
        let now = Instant::now();
        let mut d = IdleDetector::new(Some(ms(3000)), Some(ms(10000)), now);
        assert!(d.poll(now).is_empty());
        assert!(d.poll(now + ms(100)).is_empty());
    }

    #[test]
    fn both_thresholds_disabled_means_inactive() {
        let now = Instant::now();
        let d = IdleDetector::new(None, None, now);
        assert!(!d.is_active());
    }

    #[test]
    fn silence_fires_at_threshold_then_suppressed() {
        let now = Instant::now();
        let mut d = IdleDetector::new(Some(ms(3000)), None, now);
        assert!(d.poll(now + ms(2999)).is_empty());
        // At the threshold the silence event fires…
        let events = d.poll(now + ms(3001));
        assert_eq!(events.len(), 1);
        assert!(matches!(
            events[0],
            IdleEvent::SilenceDetected { duration_ms } if duration_ms >= 3000
        ));
        // …and is suppressed for the rest of the same stretch.
        assert!(d.poll(now + ms(8000)).is_empty());
        assert!(d.poll(now + ms(15000)).is_empty());
    }

    #[test]
    fn silence_re_arms_after_speech() {
        let now = Instant::now();
        let mut d = IdleDetector::new(Some(ms(3000)), None, now);
        let _ = d.poll(now + ms(3100));
        // A speech cycle resets the suppression flag; the next stretch
        // is measured from the cycle's END.
        d.note_speech_started(now + ms(5000));
        d.note_speech_stopped(now + ms(5500));
        assert!(d.poll(now + ms(8499)).is_empty());
        let events = d.poll(now + ms(8501));
        assert_eq!(events.len(), 1);
        assert!(matches!(
            events[0],
            IdleEvent::SilenceDetected { duration_ms } if duration_ms == 3001
        ));
    }

    #[test]
    fn no_idle_events_while_speech_is_active() {
        // #399 regression: a caller utterance LONGER than the
        // thresholds must not fire either event mid-utterance, and the
        // following silence stretch must (a) still fire and (b) be
        // measured from the utterance's END, not its onset.
        let now = Instant::now();
        let mut d = IdleDetector::new(Some(ms(3000)), Some(ms(10000)), now);
        d.note_speech_started(now + ms(1000));
        // 14s into the utterance: old code had fired silence at +4s
        // and dead-air at +11s.
        assert!(d.poll(now + ms(4500)).is_empty());
        assert!(d.poll(now + ms(15000)).is_empty());
        d.note_speech_stopped(now + ms(16000));
        // Silence counts from the END (16s), not the onset (1s).
        assert!(d.poll(now + ms(18999)).is_empty());
        let events = d.poll(now + ms(19001));
        assert_eq!(events.len(), 1);
        assert!(matches!(
            events[0],
            IdleEvent::SilenceDetected { duration_ms } if duration_ms == 3001
        ));
        // Dead-air likewise: caller audio flowed until 16s.
        assert!(d.poll(now + ms(25999)).is_empty());
        let events = d.poll(now + ms(26001));
        assert_eq!(events.len(), 1);
        assert!(matches!(
            events[0],
            IdleEvent::DeadAirDetected { duration_ms } if duration_ms == 10001
        ));
    }

    #[test]
    fn stop_without_start_re_anchors_both_timers() {
        // forge emits a zero-length SpeechStopped at call setup; a
        // stop with no active speech must not wedge anything — both
        // anchors simply move forward.
        let now = Instant::now();
        let mut d = IdleDetector::new(Some(ms(3000)), None, now);
        d.note_speech_stopped(now + ms(500));
        assert!(d.poll(now + ms(3499)).is_empty());
        let events = d.poll(now + ms(3501));
        assert_eq!(events.len(), 1);
        assert!(matches!(
            events[0],
            IdleEvent::SilenceDetected { duration_ms } if duration_ms == 3001
        ));
    }

    #[test]
    fn dead_air_fires_repeatedly_at_each_threshold() {
        let now = Instant::now();
        let mut d = IdleDetector::new(None, Some(ms(10000)), now);
        assert!(d.poll(now + ms(9999)).is_empty());
        let events = d.poll(now + ms(10001));
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], IdleEvent::DeadAirDetected { .. }));
        // After firing the anchor re-bases at `now + 10001`, so the
        // next fire is at `+20001`, not still pending at `+10500`.
        assert!(d.poll(now + ms(10500)).is_empty());
        assert!(d.poll(now + ms(20002)).len() == 1);
    }

    #[test]
    fn ws_audio_resets_dead_air_but_not_silence() {
        let now = Instant::now();
        let mut d = IdleDetector::new(Some(ms(3000)), Some(ms(10000)), now);
        // WS server pushes audio every 2s. Caller stays silent.
        for tick in 1..=6 {
            d.note_ws_audio(now + ms(2000 * tick));
        }
        let events = d.poll(now + ms(12001));
        // Silence should fire (caller silent > 3s), but dead-air
        // must NOT (WS audio kept the anchor fresh).
        assert!(events
            .iter()
            .any(|e| matches!(e, IdleEvent::SilenceDetected { .. })));
        assert!(!events
            .iter()
            .any(|e| matches!(e, IdleEvent::DeadAirDetected { .. })));
    }

    #[test]
    fn speech_resets_both() {
        let now = Instant::now();
        let mut d = IdleDetector::new(Some(ms(3000)), Some(ms(10000)), now);
        d.note_speech_started(now + ms(8000));
        d.note_speech_stopped(now + ms(8100));
        // After the speech cycle, neither event fires until its
        // threshold elapses from the cycle's end.
        assert!(d.poll(now + ms(10000)).is_empty());
        let events = d.poll(now + ms(11101));
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], IdleEvent::SilenceDetected { .. }));
    }

    #[test]
    fn poll_at_anchor_time_is_a_no_op() {
        // saturating_duration_since against a future `now` is 0,
        // which must NOT count as past-threshold.
        let now = Instant::now();
        let mut d = IdleDetector::new(Some(ms(1)), Some(ms(1)), now);
        // Polling at exactly `now` — elapsed = 0, below threshold of 1ms.
        assert!(d.poll(now).is_empty());
    }
}
