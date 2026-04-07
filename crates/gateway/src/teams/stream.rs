/// Teams streaming message protocol.
///
/// Implements incremental typing activities with `streaminfo` entities and
/// incrementing `streamSequence`, followed by a final message activity.
/// This provides real-time feedback in Teams while the LLM generates a response.
use std::time::{Duration, Instant};

use anyhow::Result;

use super::api::TeamsClient;
use super::types::ReplyActivity;

/// Default throttle interval between streaming updates (Teams recommends 1.5–2s).
const STREAM_THROTTLE: Duration = Duration::from_millis(1500);

/// Minimum characters before sending the first streaming chunk.
/// Avoids push notification flicker on short responses.
const MIN_FIRST_CHUNK_CHARS: usize = 20;

/// A streaming session that sends incremental typing activities to Teams,
/// then a final message when complete.
pub struct TeamsStream<'a> {
    client: &'a TeamsClient,
    service_url: String,
    conversation_id: String,
    activity_id: String,
    sequence: u32,
    last_sent: Instant,
    throttle: Duration,
    buffer: String,
    sent_first: bool,
}

impl<'a> TeamsStream<'a> {
    /// Create a new streaming session for a Teams conversation.
    pub fn new(
        client: &'a TeamsClient,
        service_url: &str,
        conversation_id: &str,
        activity_id: &str,
    ) -> Self {
        Self {
            client,
            service_url: service_url.to_string(),
            conversation_id: conversation_id.to_string(),
            activity_id: activity_id.to_string(),
            sequence: 0,
            last_sent: Instant::now() - STREAM_THROTTLE, // Allow immediate first send
            throttle: STREAM_THROTTLE,
            buffer: String::new(),
            sent_first: false,
        }
    }

    /// Append partial text to the buffer and send a streaming update if the
    /// throttle interval has elapsed.
    pub async fn send_chunk(&mut self, partial_text: &str) -> Result<()> {
        self.buffer.push_str(partial_text);

        // Wait for minimum content before first send
        if !self.sent_first && self.buffer.chars().count() < MIN_FIRST_CHUNK_CHARS {
            return Ok(());
        }

        // Respect throttle interval
        if self.last_sent.elapsed() < self.throttle {
            return Ok(());
        }

        self.sequence += 1;
        let activity = ReplyActivity::streaming_typing(&self.buffer, self.sequence);

        // Best-effort: don't fail the stream on a transient send error
        let _ = self
            .client
            .send_activity(&self.service_url, &self.conversation_id, &activity)
            .await;

        self.last_sent = Instant::now();
        self.sent_first = true;
        Ok(())
    }

    /// Send the final message, closing the stream.
    pub async fn finalize(&mut self, final_text: &str) -> Result<()> {
        let activity = ReplyActivity::streaming_final(final_text);
        self.client
            .send_reply_activity(
                &self.service_url,
                &self.conversation_id,
                &self.activity_id,
                &activity,
            )
            .await
    }

    /// Whether any content has been streamed.
    pub fn has_content(&self) -> bool {
        self.sent_first
    }

    /// Current sequence number.
    pub fn sequence(&self) -> u32 {
        self.sequence
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_initializes_with_zero_sequence() {
        let client = TeamsClient::new("app-id", "app-secret").unwrap();
        let stream = TeamsStream::new(
            &client,
            "https://smba.trafficmanager.net.botframework.com/",
            "conv-1",
            "act-1",
        );
        assert_eq!(stream.sequence(), 0);
        assert!(!stream.has_content());
    }

    #[test]
    fn streaming_typing_activity_serialization() {
        let activity = ReplyActivity::streaming_typing("partial response...", 1);
        let json = serde_json::to_value(&activity).unwrap();
        assert_eq!(json["type"], "typing");
        assert_eq!(json["text"], "partial response...");
        let entities = json["entities"].as_array().unwrap();
        assert_eq!(entities.len(), 1);
        assert_eq!(entities[0]["type"], "streaminfo");
        assert_eq!(entities[0]["streamType"], "streaming");
        assert_eq!(entities[0]["streamSequence"], 1);
    }

    #[test]
    fn streaming_final_activity_serialization() {
        let activity = ReplyActivity::streaming_final("complete response");
        let json = serde_json::to_value(&activity).unwrap();
        assert_eq!(json["type"], "message");
        assert_eq!(json["text"], "complete response");
        let entities = json["entities"].as_array().unwrap();
        assert_eq!(entities.len(), 1);
        assert_eq!(entities[0]["type"], "streaminfo");
        assert_eq!(entities[0]["streamType"], "final");
        assert!(entities[0].get("streamSequence").is_none());
    }

    #[test]
    fn non_streaming_activities_have_no_entities() {
        let msg = ReplyActivity::message("hello");
        let json = serde_json::to_value(&msg).unwrap();
        assert!(json.get("entities").is_none());

        let typing = ReplyActivity::typing();
        let json = serde_json::to_value(&typing).unwrap();
        assert!(json.get("entities").is_none());
    }

    #[test]
    fn streaming_typing_sequence_increments() {
        let a1 = ReplyActivity::streaming_typing("chunk 1", 1);
        let a2 = ReplyActivity::streaming_typing("chunk 1 chunk 2", 2);
        let j1 = serde_json::to_value(&a1).unwrap();
        let j2 = serde_json::to_value(&a2).unwrap();
        assert_eq!(j1["entities"][0]["streamSequence"], 1);
        assert_eq!(j2["entities"][0]["streamSequence"], 2);
    }

    #[test]
    fn min_first_chunk_constant() {
        assert_eq!(MIN_FIRST_CHUNK_CHARS, 20);
    }

    #[test]
    fn throttle_constant() {
        assert_eq!(STREAM_THROTTLE, Duration::from_millis(1500));
    }
}
