//! When does "an app is using the microphone" become "a meeting started",
//! and when has it ended? Pure: time comes in as an `Instant` per tick, so
//! the rules are tested without waiting.
//!
//! The microphone must stay in use for `detect_after` before anything is
//! said (a voice memo or a dictation blip is not a meeting), and must stay
//! free for `end_after` before the meeting is called over (calls drop and
//! rejoin; a mute that releases the device is not the end).

use std::time::{Duration, Instant};

use cortex::meetings::calendar::CalendarEvent;

/// The detector's timings.
#[derive(Debug, Clone, Copy)]
pub struct DetectorConfig {
    /// How long the mic must be held before a meeting is reported.
    pub detect_after: Duration,
    /// How long the mic must be free before the meeting is over.
    pub end_after: Duration,
}

impl Default for DetectorConfig {
    fn default() -> Self {
        Self {
            detect_after: Duration::from_secs(5),
            end_after: Duration::from_secs(30),
        }
    }
}

/// What changed on this tick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Transition {
    /// A call has been going for `detect_after`.
    Detected {
        /// "Zoom", "Chrome"…
        source_app: String,
        /// The calendar event it lines up with.
        calendar_event: Option<CalendarEvent>,
    },
    /// The call has been over for `end_after`.
    Ended,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Phase {
    Idle,
    Candidate { since: Instant },
    Detected,
    Ending { since: Instant },
}

/// The detector's memory between ticks.
#[derive(Debug)]
pub struct DetectorState {
    config: DetectorConfig,
    phase: Phase,
}

impl DetectorState {
    /// Start idle.
    pub fn new(config: DetectorConfig) -> Self {
        Self {
            config,
            phase: Phase::Idle,
        }
    }

    /// Forget everything: the feature was turned off, or a meeting is
    /// recording and the detector stands aside.
    pub fn reset(&mut self) {
        self.phase = Phase::Idle;
    }

    /// Advance one tick. `mic_apps` are the meeting apps holding the
    /// microphone right now, already labelled; `event` the joinable
    /// calendar event nearest to now.
    pub fn on_tick(
        &mut self,
        now: Instant,
        mic_apps: &[&str],
        event: Option<&CalendarEvent>,
    ) -> Option<Transition> {
        let app = best_app(mic_apps);
        match (&self.phase, app) {
            (Phase::Idle, Some(_)) => {
                self.phase = Phase::Candidate { since: now };
                // A zero wait (tests, or a future "instant" mode) reports now.
                self.on_tick_candidate(now, app, event)
            }
            (Phase::Idle, None) => None,
            (Phase::Candidate { .. }, Some(_)) => self.on_tick_candidate(now, app, event),
            (Phase::Candidate { .. }, None) => {
                self.phase = Phase::Idle;
                None
            }
            (Phase::Detected, Some(_)) => None,
            (Phase::Detected, None) => {
                self.phase = Phase::Ending { since: now };
                None
            }
            // Back before the grace ran out: the same meeting, not a new one.
            (Phase::Ending { .. }, Some(_)) => {
                self.phase = Phase::Detected;
                None
            }
            (Phase::Ending { since }, None) => {
                if now.duration_since(*since) >= self.config.end_after {
                    self.phase = Phase::Idle;
                    Some(Transition::Ended)
                } else {
                    None
                }
            }
        }
    }

    fn on_tick_candidate(
        &mut self,
        now: Instant,
        app: Option<&str>,
        event: Option<&CalendarEvent>,
    ) -> Option<Transition> {
        let Phase::Candidate { since } = self.phase else {
            return None;
        };
        if now.duration_since(since) < self.config.detect_after {
            return None;
        }
        self.phase = Phase::Detected;
        Some(Transition::Detected {
            source_app: app.unwrap_or_default().to_string(),
            calendar_event: event.cloned(),
        })
    }
}

/// When several apps hold the mic, the one most surely a meeting: a
/// dedicated meeting app over a browser.
fn best_app<'a>(apps: &[&'a str]) -> Option<&'a str> {
    apps.iter().copied().min_by_key(|a| super::probe::rank(a))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(start: Instant, s: u64) -> Instant {
        start + Duration::from_secs(s)
    }

    fn event() -> CalendarEvent {
        CalendarEvent {
            id: "ics:x".into(),
            source: "ics".into(),
            title: "Tiger Sync".into(),
            start_unix: 0,
            end_unix: 1800,
            all_day: false,
            attendees: vec![],
            join_url: "https://meet.google.com/abc-defg-hij".into(),
            provider: "meet".into(),
        }
    }

    /// Feed `(second, apps)` ticks; collect what came out.
    fn run(ticks: &[(u64, &[&str])]) -> Vec<(u64, Transition)> {
        let t0 = Instant::now();
        let mut d = DetectorState::new(DetectorConfig::default());
        ticks
            .iter()
            .filter_map(|(s, apps)| d.on_tick(secs(t0, *s), apps, None).map(|t| (*s, t)))
            .collect()
    }

    fn detected(app: &str) -> Transition {
        Transition::Detected {
            source_app: app.into(),
            calendar_event: None,
        }
    }

    #[test]
    fn the_mic_must_be_held_for_five_seconds() {
        let zoom: &[&str] = &["Zoom"];
        let none: &[&str] = &[];
        // A 2-second blip says nothing.
        assert!(run(&[(0, zoom), (2, zoom), (4, none), (6, none)]).is_empty());
        // Held: reported once, at 5 s, not again.
        assert_eq!(
            run(&[(0, zoom), (2, zoom), (4, zoom), (6, zoom), (8, zoom)]),
            [(6, detected("Zoom"))]
        );
    }

    #[test]
    fn a_meeting_ends_only_after_thirty_quiet_seconds() {
        let zoom: &[&str] = &["Zoom"];
        let none: &[&str] = &[];
        let ticks = [
            (0, zoom),
            (6, zoom),
            (10, none),
            (30, none),
            // Back within the grace: same meeting, no new Detected.
            (35, zoom),
            (40, none),
            (60, none),
            (70, none),
        ];
        assert_eq!(
            run(&ticks),
            [(6, detected("Zoom")), (70, Transition::Ended)]
        );
    }

    #[test]
    fn a_dedicated_app_outranks_a_browser() {
        let both: &[&str] = &["Chrome", "Teams"];
        assert_eq!(run(&[(0, both), (5, both)]), [(5, detected("Teams"))]);
    }

    #[test]
    fn the_calendar_event_rides_along_and_reset_forgets() {
        let t0 = Instant::now();
        let mut d = DetectorState::new(DetectorConfig::default());
        let e = event();
        d.on_tick(t0, &["Chrome"], Some(&e));
        // Reset mid-candidate (a recording started): the clock restarts.
        d.reset();
        assert_eq!(d.on_tick(secs(t0, 5), &["Chrome"], Some(&e)), None);
        assert_eq!(
            d.on_tick(secs(t0, 10), &["Chrome"], Some(&e)),
            Some(Transition::Detected {
                source_app: "Chrome".into(),
                calendar_event: Some(e.clone())
            })
        );
    }
}
