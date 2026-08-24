use std::collections::HashMap;
use std::time::Instant;

use serde::Serialize;

/// Live health state for a single channel.
#[derive(Debug)]
pub struct ChannelHealth {
    /// Timestamp of the most recent inbound message.
    pub last_inbound_at: Option<Instant>,
    /// Timestamp of the most recent outbound message.
    pub last_outbound_at: Option<Instant>,
    /// Most recent error message (truncated to 512 chars).
    pub last_error: Option<String>,
    /// Timestamp of the most recent error.
    pub last_error_at: Option<Instant>,
    /// Total number of inbound messages received.
    pub inbound_count: u64,
    /// Total number of outbound messages sent.
    pub outbound_count: u64,
    /// Total number of errors recorded.
    pub error_count: u64,
    /// Number of reconnection attempts made.
    pub reconnect_attempts: u64,
    created_at: Instant,
}

impl ChannelHealth {
    fn new() -> Self {
        Self {
            last_inbound_at: None,
            last_outbound_at: None,
            last_error: None,
            last_error_at: None,
            inbound_count: 0,
            outbound_count: 0,
            error_count: 0,
            reconnect_attempts: 0,
            created_at: Instant::now(),
        }
    }
}

/// Serializable point-in-time snapshot of a channel's health.
#[derive(Debug, Clone, Serialize)]
pub struct ChannelHealthSnapshot {
    /// Channel name.
    pub name: String,
    /// Total inbound messages received.
    pub inbound_count: u64,
    /// Total outbound messages sent.
    pub outbound_count: u64,
    /// Total errors recorded.
    pub error_count: u64,
    /// Number of reconnection attempts.
    pub reconnect_attempts: u64,
    /// Most recent error message, if any.
    pub last_error: Option<String>,
    /// Seconds since the channel was registered.
    pub uptime_secs: u64,
    /// Seconds since the last inbound message, if any.
    pub last_inbound_ago_secs: Option<u64>,
    /// Seconds since the last outbound message, if any.
    pub last_outbound_ago_secs: Option<u64>,
    /// Seconds since the last error, if any.
    pub last_error_ago_secs: Option<u64>,
}

/// Tracks health metrics for all gateway channels.
#[derive(Debug, Default)]
pub struct ChannelHealthRegistry {
    channels: HashMap<String, ChannelHealth>,
}

impl ChannelHealthRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a channel, initializing its health counters if not already present.
    pub fn register(&mut self, name: &str) {
        self.channels
            .entry(name.to_string())
            .or_insert_with(ChannelHealth::new);
    }

    /// Record an inbound message event for the named channel.
    pub fn record_inbound(&mut self, name: &str) {
        let entry = self
            .channels
            .entry(name.to_string())
            .or_insert_with(ChannelHealth::new);
        entry.last_inbound_at = Some(Instant::now());
        entry.inbound_count += 1;
    }

    /// Record an outbound message event for the named channel.
    pub fn record_outbound(&mut self, name: &str) {
        let entry = self
            .channels
            .entry(name.to_string())
            .or_insert_with(ChannelHealth::new);
        entry.last_outbound_at = Some(Instant::now());
        entry.outbound_count += 1;
    }

    /// Record an error event, truncating the message to 512 characters.
    pub fn record_error(&mut self, name: &str, msg: &str) {
        let entry = self
            .channels
            .entry(name.to_string())
            .or_insert_with(ChannelHealth::new);
        let truncated = if msg.len() > 512 {
            let mut end = 509;
            while end > 0 && !msg.is_char_boundary(end) {
                end -= 1;
            }
            format!("{}...", &msg[..end])
        } else {
            msg.to_string()
        };
        entry.last_error = Some(truncated);
        entry.last_error_at = Some(Instant::now());
        entry.error_count += 1;
    }

    /// Record a reconnection attempt for the named channel.
    pub fn record_reconnect(&mut self, name: &str) {
        let entry = self
            .channels
            .entry(name.to_string())
            .or_insert_with(ChannelHealth::new);
        entry.reconnect_attempts += 1;
    }

    /// Take a point-in-time snapshot of all channels' health metrics.
    pub fn snapshot(&self) -> Vec<ChannelHealthSnapshot> {
        let now = Instant::now();
        self.channels
            .iter()
            .map(|(name, h)| ChannelHealthSnapshot {
                name: name.clone(),
                inbound_count: h.inbound_count,
                outbound_count: h.outbound_count,
                error_count: h.error_count,
                reconnect_attempts: h.reconnect_attempts,
                last_error: h.last_error.clone(),
                uptime_secs: now.duration_since(h.created_at).as_secs(),
                last_inbound_ago_secs: h.last_inbound_at.map(|t| now.duration_since(t).as_secs()),
                last_outbound_ago_secs: h.last_outbound_at.map(|t| now.duration_since(t).as_secs()),
                last_error_ago_secs: h.last_error_at.map(|t| now.duration_since(t).as_secs()),
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counter_increments() {
        let mut reg = ChannelHealthRegistry::new();
        reg.register("slack");

        reg.record_inbound("slack");
        reg.record_inbound("slack");
        reg.record_outbound("slack");
        reg.record_error("slack", "timeout");

        let snap = reg.snapshot();
        let s = snap.iter().find(|s| s.name == "slack").unwrap();
        assert_eq!(s.inbound_count, 2);
        assert_eq!(s.outbound_count, 1);
        assert_eq!(s.error_count, 1);
        assert_eq!(s.last_error.as_deref(), Some("timeout"));
    }

    #[test]
    fn snapshot_format() {
        let mut reg = ChannelHealthRegistry::new();
        reg.register("discord");
        reg.record_inbound("discord");

        let snap = reg.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].name, "discord");
        assert!(snap[0].last_inbound_ago_secs.is_some());
        assert!(snap[0].last_outbound_ago_secs.is_none());
    }

    #[test]
    fn unregistered_channel_auto_created() {
        let mut reg = ChannelHealthRegistry::new();
        reg.record_inbound("new-channel");

        let snap = reg.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].inbound_count, 1);
    }

    #[test]
    fn reconnect_tracking() {
        let mut reg = ChannelHealthRegistry::new();
        reg.register("slack");
        reg.record_reconnect("slack");
        reg.record_reconnect("slack");

        let snap = reg.snapshot();
        let s = snap.iter().find(|s| s.name == "slack").unwrap();
        assert_eq!(s.reconnect_attempts, 2);
    }

    #[test]
    fn error_message_truncation() {
        let mut reg = ChannelHealthRegistry::new();
        let long_msg = "x".repeat(1000);
        reg.record_error("test", &long_msg);

        let snap = reg.snapshot();
        let s = snap.iter().find(|s| s.name == "test").unwrap();
        let err = s.last_error.as_ref().unwrap();
        assert!(err.len() <= 512, "error should be truncated to 512 chars");
        assert!(err.ends_with("..."));
    }

    #[test]
    fn error_at_exactly_512_bytes_is_kept_verbatim() {
        // 512 is the cap itself, not past it — no truncation, no ellipsis.
        let mut reg = ChannelHealthRegistry::new();
        let msg = "a".repeat(512);
        reg.record_error("test", &msg);

        let snap = reg.snapshot();
        let s = snap.iter().find(|s| s.name == "test").unwrap();
        assert_eq!(s.last_error.as_deref(), Some(msg.as_str()));
    }

    #[test]
    fn error_truncation_keeps_509_bytes_plus_ellipsis() {
        // Over-cap messages must keep exactly the first 509 bytes and append
        // "..." — total 512. Keeping less loses diagnostic detail; a broken
        // boundary walk that runs to 0 would keep nothing but "...".
        let mut reg = ChannelHealthRegistry::new();
        let msg = "x".repeat(1000);
        reg.record_error("test", &msg);

        let snap = reg.snapshot();
        let s = snap.iter().find(|s| s.name == "test").unwrap();
        let expected = format!("{}...", "x".repeat(509));
        assert_eq!(s.last_error.as_deref(), Some(expected.as_str()));
    }

    #[test]
    fn error_truncation_backs_up_to_a_char_boundary() {
        // 300 two-byte chars = 600 bytes. Byte 509 lands mid-char, so the cut
        // must back up to 508 (254 whole chars); slicing at 509 would panic
        // and stepping the wrong way would split the char.
        let mut reg = ChannelHealthRegistry::new();
        let msg = "é".repeat(300);
        reg.record_error("test", &msg);

        let snap = reg.snapshot();
        let s = snap.iter().find(|s| s.name == "test").unwrap();
        let expected = format!("{}...", "é".repeat(254));
        assert_eq!(s.last_error.as_deref(), Some(expected.as_str()));
    }

    #[test]
    fn error_message_short_not_truncated() {
        let mut reg = ChannelHealthRegistry::new();
        reg.record_error("test", "short error");

        let snap = reg.snapshot();
        let s = snap.iter().find(|s| s.name == "test").unwrap();
        assert_eq!(s.last_error.as_deref(), Some("short error"));
    }

    #[test]
    fn multiple_channels_independent() {
        let mut reg = ChannelHealthRegistry::new();
        reg.register("slack");
        reg.register("telegram");

        reg.record_inbound("slack");
        reg.record_inbound("slack");
        reg.record_inbound("telegram");

        let snap = reg.snapshot();
        let slack = snap.iter().find(|s| s.name == "slack").unwrap();
        let tg = snap.iter().find(|s| s.name == "telegram").unwrap();
        assert_eq!(slack.inbound_count, 2);
        assert_eq!(tg.inbound_count, 1);
    }

    #[test]
    fn empty_registry_snapshot() {
        let reg = ChannelHealthRegistry::new();
        let snap = reg.snapshot();
        assert!(snap.is_empty());
    }

    #[test]
    fn register_same_channel_twice_no_reset() {
        let mut reg = ChannelHealthRegistry::new();
        reg.register("slack");
        reg.record_inbound("slack");
        reg.register("slack"); // re-register should not reset

        let snap = reg.snapshot();
        let s = snap.iter().find(|s| s.name == "slack").unwrap();
        assert_eq!(s.inbound_count, 1);
    }

    #[test]
    fn uptime_is_nonnegative() {
        let mut reg = ChannelHealthRegistry::new();
        reg.register("test");
        let snap = reg.snapshot();
        // Uptime should be 0 or very small since we just created it
        assert!(snap[0].uptime_secs <= 1);
    }

    #[test]
    fn snapshot_serializable() {
        let mut reg = ChannelHealthRegistry::new();
        reg.register("test");
        reg.record_inbound("test");
        reg.record_error("test", "oops");

        let snap = reg.snapshot();
        let json = serde_json::to_string(&snap).unwrap();
        assert!(json.contains("\"test\""));
        assert!(json.contains("\"oops\""));
    }
}
