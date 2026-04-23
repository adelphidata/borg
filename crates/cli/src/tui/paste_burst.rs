//! Paste-burst detection for terminals without bracketed paste.
//!
//! When bracket paste mode is unavailable, pastes arrive as rapid streams of
//! individual `KeyCode::Char` events. This module detects those bursts and
//! buffers them so the composer can insert the entire paste as a single string.

use std::time::{Duration, Instant};

/// Maximum delay between consecutive chars to be considered part of a paste burst.
#[cfg(not(windows))]
const BURST_CHAR_INTERVAL: Duration = Duration::from_millis(8);
#[cfg(windows)]
const BURST_CHAR_INTERVAL: Duration = Duration::from_millis(30);

/// Idle timeout before flushing buffered paste content.
#[cfg(not(windows))]
const BURST_IDLE_TIMEOUT: Duration = Duration::from_millis(8);
#[cfg(windows)]
const BURST_IDLE_TIMEOUT: Duration = Duration::from_millis(60);

/// Minimum consecutive fast chars to trigger burst detection.
const BURST_MIN_CHARS: u16 = 3;

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
}

impl PasteBurst {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed a plain character event. Returns whether to insert normally or buffer.
    pub fn on_char(&mut self, ch: char, now: Instant) -> CharAction {
        // Track inter-key timing
        match self.last_char_time {
            Some(prev) if now.duration_since(prev) <= BURST_CHAR_INTERVAL => {
                self.consecutive_fast_chars = self.consecutive_fast_chars.saturating_add(1);
            }
            _ => {
                self.consecutive_fast_chars = 1;
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

    /// Append a newline to the buffer if a burst is active.
    /// Returns true if the newline was captured (burst is active).
    pub fn append_newline_if_active(&mut self, now: Instant) -> bool {
        if self.active {
            self.buffer.push('\n');
            self.last_char_time = Some(now);
            true
        } else {
            false
        }
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
            Some(std::mem::take(&mut self.buffer))
        } else {
            None
        }
    }

    /// Flush immediately when a non-char key arrives during buffering.
    pub fn flush_immediate(&mut self) -> Option<String> {
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
    fn test_newline_during_burst() {
        let mut pb = PasteBurst::new();
        let t0 = Instant::now();

        pb.on_char('a', t0);
        pb.on_char('b', t0 + Duration::from_millis(1));
        pb.on_char('c', t0 + Duration::from_millis(2));
        assert!(pb.is_active());

        // Newline during burst should be captured
        assert!(pb.append_newline_if_active(t0 + Duration::from_millis(3)));

        pb.on_char('d', t0 + Duration::from_millis(4));

        let result = pb.flush_immediate();
        assert_eq!(result, Some("c\nd".to_string()));
    }

    #[test]
    fn test_newline_not_captured_when_inactive() {
        let mut pb = PasteBurst::new();
        let now = Instant::now();
        assert!(!pb.append_newline_if_active(now));
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
