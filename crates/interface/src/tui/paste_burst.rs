//! Paste-burst detection for terminals without bracketed paste.
//!
//! When bracket paste mode is unavailable, pastes arrive as rapid streams of
//! individual `KeyCode::Char` events. This module detects those bursts and
//! buffers them so the composer can insert the entire paste as a single string.

use std::time::{Duration, Instant};

/// Maximum delay between consecutive chars to be considered part of a paste burst.
/// Windows uses a looser threshold because its console event dispatch has
/// coarser timing granularity than Unix terminals.
#[cfg(not(windows))]
const BURST_CHAR_INTERVAL: Duration = Duration::from_millis(8);
#[cfg(windows)]
const BURST_CHAR_INTERVAL: Duration = Duration::from_millis(30);

/// Idle timeout before flushing buffered paste content.
/// Windows uses a longer timeout to match the looser per-char threshold above.
#[cfg(not(windows))]
const BURST_IDLE_TIMEOUT: Duration = Duration::from_millis(8);
#[cfg(windows)]
const BURST_IDLE_TIMEOUT: Duration = Duration::from_millis(60);

/// Minimum consecutive fast chars to trigger burst detection.
const BURST_MIN_CHARS: u16 = 3;

/// Fast chars that make an immediately following Enter part of a paste even
/// though the burst has not started yet (a short first line such as `hi`).
/// No person presses two keys and Enter inside the burst interval.
const NEWLINE_MIN_FAST_CHARS: u16 = 2;

/// How long after an idle flush an Enter still counts as pasted text. A
/// terminal can stall mid-paste for longer than the idle timeout; the newline
/// that arrives next must not submit the half-pasted prompt.
const ENTER_SUPPRESS_WINDOW: Duration = Duration::from_millis(120);

/// What to do with a character event.
pub(crate) enum CharAction {
    /// Insert the character normally (not part of a burst).
    Insert,
    /// Character was buffered as part of a paste burst.
    Buffer,
}

#[derive(Default)]
pub(crate) struct PasteBurst {
    buffer: String,
    last_char_time: Option<Instant>,
    consecutive_fast_chars: u16,
    active: bool,
    /// When the burst that the idle timeout last flushed went idle.
    last_idle_flush: Option<Instant>,
}

impl PasteBurst {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed a plain character event. Returns whether to insert normally or buffer.
    pub fn on_char(&mut self, ch: char, now: Instant) -> CharAction {
        match self.last_char_time {
            Some(prev) if now.duration_since(prev) <= BURST_CHAR_INTERVAL => {
                self.consecutive_fast_chars = self.consecutive_fast_chars.saturating_add(1);
            }
            _ => {
                self.consecutive_fast_chars = 1;
                // A char at typing speed: the paste is over.
                self.last_idle_flush = None;
            }
        }
        self.last_char_time = Some(now);

        // Already actively buffering — append
        if self.active {
            self.buffer.push(ch);
            return CharAction::Buffer;
        }

        // Enough fast chars to start buffering
        if self.consecutive_fast_chars >= BURST_MIN_CHARS {
            self.active = true;
            self.buffer.push(ch);
            return CharAction::Buffer;
        }

        CharAction::Insert
    }

    /// Append a control character (`\n` from Enter, `\t` from Tab) to the
    /// buffer if a burst is active. Without bracketed paste these arrive as
    /// separate key events, not `KeyCode::Char`, so the caller routes them
    /// here before treating them as submit/completion keys.
    /// Returns true if the character was captured (burst is active). A
    /// capture refreshes the burst clock so the idle flush does not split
    /// the paste at the control character.
    pub fn append_control_char_if_active(&mut self, ch: char, now: Instant) -> bool {
        if self.active {
            self.buffer.push(ch);
            self.last_char_time = Some(now);
            true
        } else {
            false
        }
    }

    /// Decide whether an unmodified Enter is a pasted newline, and buffer it
    /// when it is. Three cases qualify: a burst is active; the Enter follows a
    /// run of fast chars too short to have started one; or a burst was flushed
    /// by the idle timeout a moment ago, which is a paste that stalled rather
    /// than one that ended. The last two start a burst so the rest of the
    /// paste is buffered behind the newline.
    pub fn capture_newline(&mut self, now: Instant) -> bool {
        if self.append_control_char_if_active('\n', now) {
            return true;
        }
        let in_fast_run = self.consecutive_fast_chars >= NEWLINE_MIN_FAST_CHARS
            && self
                .last_char_time
                .is_some_and(|t| now.duration_since(t) <= BURST_CHAR_INTERVAL);
        // One Enter per flush: taken here, so a second Enter is a submit.
        let just_flushed = self
            .last_idle_flush
            .take()
            .is_some_and(|t| now.saturating_duration_since(t) <= ENTER_SUPPRESS_WINDOW);
        if !(in_fast_run || just_flushed) {
            return false;
        }
        self.active = true;
        self.buffer.push('\n');
        self.last_char_time = Some(now);
        true
    }

    /// Called on each tick. Returns buffered text if idle timeout has elapsed.
    pub fn flush_if_due(&mut self, now: Instant) -> Option<String> {
        if !self.active || self.buffer.is_empty() {
            return None;
        }

        let timed_out = self
            .last_char_time
            .is_some_and(|t| now.duration_since(t) > BURST_IDLE_TIMEOUT);

        if timed_out {
            self.active = false;
            self.consecutive_fast_chars = 0;
            // Stamp the moment the burst went idle, not the moment this call
            // noticed. Ticks are skipped while a popup is open, so the flush
            // can run long after the paste ended, and an Enter right after
            // such a late flush is the user's own submit.
            self.last_idle_flush = self.last_char_time.map(|t| t + BURST_IDLE_TIMEOUT);
            Some(std::mem::take(&mut self.buffer))
        } else {
            None
        }
    }

    /// Flush immediately when a non-char key arrives during buffering.
    pub fn flush_immediate(&mut self) -> Option<String> {
        self.last_idle_flush = None;
        if !self.active || self.buffer.is_empty() {
            return None;
        }
        self.active = false;
        self.consecutive_fast_chars = 0;
        self.last_char_time = None;
        Some(std::mem::take(&mut self.buffer))
    }

    /// Whether we are actively buffering a paste burst.
    #[cfg(test)]
    pub fn is_active(&self) -> bool {
        self.active
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enter_after_a_short_fast_line_is_a_pasted_newline() {
        // "hi\nthere" without bracketed paste: two chars is under the burst
        // threshold, so the Enter used to submit "hi".
        let mut pb = PasteBurst::new();
        let t0 = Instant::now();
        let step = Duration::from_millis(1);
        assert!(matches!(pb.on_char('h', t0), CharAction::Insert));
        assert!(matches!(pb.on_char('i', t0 + step), CharAction::Insert));
        assert!(pb.capture_newline(t0 + step * 2));
        // The rest of the paste is buffered behind the newline.
        assert!(matches!(pb.on_char('t', t0 + step * 3), CharAction::Buffer));
        assert_eq!(
            pb.flush_if_due(t0 + Duration::from_millis(200)).as_deref(),
            Some("\nt")
        );
    }

    #[test]
    fn enter_after_typed_text_is_not_captured() {
        let mut pb = PasteBurst::new();
        let t0 = Instant::now();
        // One fast char is not a run.
        pb.on_char('y', t0);
        assert!(!pb.capture_newline(t0 + Duration::from_millis(1)));
        // Two chars at typing speed, then Enter.
        pb.on_char('o', t0 + Duration::from_millis(150));
        pb.on_char('k', t0 + Duration::from_millis(300));
        assert!(!pb.capture_newline(t0 + Duration::from_millis(301)));
        // Two fast chars, but the Enter comes at typing speed.
        let t1 = t0 + Duration::from_secs(1);
        pb.on_char('o', t1);
        pb.on_char('k', t1 + Duration::from_millis(1));
        assert!(!pb.capture_newline(t1 + Duration::from_millis(150)));
        assert!(!pb.is_active());
    }

    #[test]
    fn enter_just_after_an_idle_flush_continues_the_paste() {
        let mut pb = PasteBurst::new();
        let t0 = Instant::now();
        for (i, ch) in "abcd".chars().enumerate() {
            pb.on_char(ch, t0 + Duration::from_millis(i as u64));
        }
        // The terminal stalls past the idle timeout and the tick flushes.
        let flushed_at = t0 + Duration::from_millis(80);
        assert!(pb.flush_if_due(flushed_at).is_some());
        assert!(!pb.is_active());
        assert!(pb.capture_newline(flushed_at + Duration::from_millis(40)));
        assert!(pb.is_active());
        // The window is spent: after that newline flushes, the next Enter
        // is a submit.
        assert!(pb
            .flush_if_due(flushed_at + Duration::from_millis(60))
            .is_some());
        assert!(pb.capture_newline(flushed_at + Duration::from_millis(70)));
        assert!(pb
            .flush_if_due(flushed_at + Duration::from_millis(90))
            .is_some());
        assert!(!pb.capture_newline(flushed_at + Duration::from_millis(300)));

        // Outside the window an Enter is the user's own.
        let mut pb = PasteBurst::new();
        for (i, ch) in "abcd".chars().enumerate() {
            pb.on_char(ch, t0 + Duration::from_millis(i as u64));
        }
        assert!(pb.flush_if_due(flushed_at).is_some());
        assert!(!pb.capture_newline(flushed_at + ENTER_SUPPRESS_WINDOW * 2));
    }

    #[test]
    fn an_immediate_flush_does_not_open_the_enter_window() {
        // A non-char key ends the burst on purpose; the Enter that follows
        // is a real submit.
        let mut pb = PasteBurst::new();
        let t0 = Instant::now();
        for (i, ch) in "abcd".chars().enumerate() {
            pb.on_char(ch, t0 + Duration::from_millis(i as u64));
        }
        assert!(pb.flush_immediate().is_some());
        assert!(!pb.capture_newline(t0 + Duration::from_millis(20)));
    }

    #[test]
    fn test_single_char_not_buffered() {
        let mut pb = PasteBurst::new();
        let now = Instant::now();
        assert!(matches!(pb.on_char('a', now), CharAction::Insert));
        assert!(!pb.is_active());
        assert!(pb.flush_if_due(now + Duration::from_millis(100)).is_none());
    }

    #[test]
    fn test_slow_chars_not_buffered() {
        let mut pb = PasteBurst::new();
        let t0 = Instant::now();
        assert!(matches!(pb.on_char('a', t0), CharAction::Insert));
        assert!(matches!(
            pb.on_char('b', t0 + Duration::from_millis(100)),
            CharAction::Insert
        ));
        assert!(matches!(
            pb.on_char('c', t0 + Duration::from_millis(200)),
            CharAction::Insert
        ));
        assert!(matches!(
            pb.on_char('d', t0 + Duration::from_millis(300)),
            CharAction::Insert
        ));
        assert!(!pb.is_active());
    }

    #[test]
    fn test_rapid_chars_buffered() {
        let mut pb = PasteBurst::new();
        let t0 = Instant::now();

        // First two chars are below threshold — insert normally
        assert!(matches!(pb.on_char('a', t0), CharAction::Insert));
        assert!(matches!(
            pb.on_char('b', t0 + Duration::from_millis(1)),
            CharAction::Insert
        ));

        // Third char triggers buffering
        assert!(matches!(
            pb.on_char('c', t0 + Duration::from_millis(2)),
            CharAction::Buffer
        ));
        assert!(pb.is_active());

        // Fourth char continues buffering
        assert!(matches!(
            pb.on_char('d', t0 + Duration::from_millis(3)),
            CharAction::Buffer
        ));
    }

    #[test]
    fn test_flush_returns_buffer() {
        let mut pb = PasteBurst::new();
        let t0 = Instant::now();

        // Trigger burst
        pb.on_char('a', t0);
        pb.on_char('b', t0 + Duration::from_millis(1));
        pb.on_char('c', t0 + Duration::from_millis(2));
        pb.on_char('d', t0 + Duration::from_millis(3));

        // Not yet timed out
        assert!(pb.flush_if_due(t0 + Duration::from_millis(5)).is_none());

        // After idle timeout
        let flush_time =
            t0 + Duration::from_millis(3) + BURST_IDLE_TIMEOUT + Duration::from_millis(1);
        let result = pb.flush_if_due(flush_time);
        assert_eq!(result, Some("cd".to_string())); // only c and d were buffered
        assert!(!pb.is_active());
    }

    #[test]
    fn test_non_char_flushes_immediately() {
        let mut pb = PasteBurst::new();
        let t0 = Instant::now();

        pb.on_char('a', t0);
        pb.on_char('b', t0 + Duration::from_millis(1));
        pb.on_char('c', t0 + Duration::from_millis(2));

        let result = pb.flush_immediate();
        assert_eq!(result, Some("c".to_string()));
        assert!(!pb.is_active());
    }

    #[test]
    fn test_control_chars_during_burst_are_kept_in_order() {
        for ctrl in ['\n', '\t'] {
            let mut pb = PasteBurst::new();
            let t0 = Instant::now();

            pb.on_char('a', t0);
            pb.on_char('b', t0 + Duration::from_millis(1));
            pb.on_char('c', t0 + Duration::from_millis(2));
            assert!(pb.is_active());

            assert!(pb.append_control_char_if_active(ctrl, t0 + Duration::from_millis(3)));
            pb.on_char('d', t0 + Duration::from_millis(4));

            let result = pb.flush_immediate();
            assert_eq!(result, Some(format!("c{ctrl}d")), "ctrl {ctrl:?}");
        }
    }

    #[test]
    fn test_control_char_not_captured_when_inactive() {
        let mut pb = PasteBurst::new();
        let now = Instant::now();
        assert!(!pb.append_control_char_if_active('\n', now));
        assert!(!pb.append_control_char_if_active('\t', now));
        assert!(pb.flush_immediate().is_none());
    }

    /// A control char refreshes the burst clock: a tick that would have
    /// timed out relative to the last *plain* char must not flush while the
    /// paste is still streaming its whitespace.
    #[test]
    fn test_control_char_refreshes_idle_deadline() {
        let mut pb = PasteBurst::new();
        let t0 = Instant::now();
        pb.on_char('a', t0);
        pb.on_char('b', t0 + Duration::from_millis(1));
        pb.on_char('c', t0 + Duration::from_millis(2));

        let tab_at = t0 + Duration::from_millis(2) + BURST_IDLE_TIMEOUT;
        assert!(pb.append_control_char_if_active('\t', tab_at));
        // Past the idle timeout measured from 'c', within it measured from Tab.
        let probe = t0 + Duration::from_millis(3) + BURST_IDLE_TIMEOUT;
        assert!(pb.flush_if_due(probe).is_none());
        assert_eq!(
            pb.flush_if_due(tab_at + BURST_IDLE_TIMEOUT + Duration::from_millis(1)),
            Some("c\t".to_string())
        );
    }

    #[test]
    fn test_flush_immediate_when_inactive() {
        let mut pb = PasteBurst::new();
        assert!(pb.flush_immediate().is_none());
    }

    #[test]
    fn test_flush_if_due_when_inactive() {
        let mut pb = PasteBurst::new();
        assert!(pb.flush_if_due(Instant::now()).is_none());
    }
}
