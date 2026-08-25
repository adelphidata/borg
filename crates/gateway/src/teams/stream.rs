/// Teams streaming message protocol.
///
/// Implements incremental typing activities with `streaminfo` entities and
/// incrementing `streamSequence`, followed by a final message activity.
/// This provides real-time feedback in Teams while the LLM generates a response.
use std::time::{Duration, Instant};

use anyhow::Result;
use tracing::warn;

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

        if !self.sent_first && self.buffer.chars().count() < MIN_FIRST_CHUNK_CHARS {
            return Ok(());
        }

        // Respect throttle interval
        if self.throttled(Instant::now()) {
            return Ok(());
        }

        let next_sequence = self.sequence + 1;
        let activity = ReplyActivity::streaming_typing(&self.buffer, next_sequence);

        if let Err(e) = self
            .client
            .send_activity(&self.service_url, &self.conversation_id, &activity)
            .await
        {
            warn!("Failed to send Teams streaming chunk: {e}");
            return Ok(());
        }

        self.record_successful_send(next_sequence, Instant::now());
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

    /// True while `now` is still inside the throttle window. At exactly the
    /// interval the window has elapsed and a send is due.
    fn throttled(&self, now: Instant) -> bool {
        now.duration_since(self.last_sent) < self.throttle
    }

    fn record_successful_send(&mut self, sequence: u32, sent_at: Instant) {
        self.sequence = sequence;
        self.last_sent = sent_at;
        self.sent_first = true;
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

    /// The throttle window closes at exactly the interval: inside it chunks
    /// are buffered, at and beyond it a send is due.
    #[test]
    fn throttle_window_closes_at_exactly_the_interval() {
        let client = TeamsClient::new("app-id", "app-secret").unwrap();
        let mut stream = TeamsStream::new(
            &client,
            "https://smba.trafficmanager.net.botframework.com/",
            "conv-1",
            "act-1",
        );
        let t = Instant::now();
        stream.last_sent = t;
        assert!(stream.throttled(t), "window just opened");
        assert!(stream.throttled(t + STREAM_THROTTLE / 2), "mid-window");
        assert!(
            !stream.throttled(t + STREAM_THROTTLE),
            "a send is due at exactly the interval"
        );
        assert!(!stream.throttled(t + STREAM_THROTTLE * 2), "past the window");
    }

    #[test]
    fn record_successful_send_advances_stream_state() {
        let client = TeamsClient::new("app-id", "app-secret").unwrap();
        let mut stream = TeamsStream::new(
            &client,
            "https://smba.trafficmanager.net.botframework.com/",
            "conv-1",
            "act-1",
        );
        let sent_at = Instant::now();
        stream.record_successful_send(3, sent_at);
        assert_eq!(stream.sequence(), 3);
        assert!(stream.has_content());
        assert!(stream.last_sent >= sent_at);
    }

    /// Loopback-stub tests that drive the real send path end to end.
    mod wire {
        use super::*;
        use wiremock::matchers::{body_string_contains, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        fn client_for(server: &MockServer) -> TeamsClient {
            TeamsClient::with_endpoints("app-1", "s3cret", &format!("{}/token", server.uri()))
                .unwrap()
        }

        async fn mount_token(server: &MockServer) {
            Mock::given(method("POST"))
                .and(path("/token"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "access_token": "tok-abc", "expires_in": 3600
                })))
                .mount(server)
                .await;
        }

        async fn mount_activities(server: &MockServer, expected: u64) {
            Mock::given(method("POST"))
                .and(path("/v3/conversations/conv-1/activities"))
                .respond_with(ResponseTemplate::new(200))
                .expect(expected)
                .mount(server)
                .await;
        }

        /// A first chunk below `MIN_FIRST_CHUNK_CHARS` is buffered, never
        /// sent — the anti-flicker rule.
        #[tokio::test]
        async fn short_first_chunk_is_buffered_not_sent() {
            let server = MockServer::start().await;
            mount_token(&server).await;
            mount_activities(&server, 0).await;

            let client = client_for(&server);
            let mut stream = TeamsStream::new(&client, &server.uri(), "conv-1", "act-1");
            stream.send_chunk("short").await.unwrap();
            assert_eq!(stream.sequence(), 0, "no streaming update may go out");
            assert!(!stream.has_content());
        }

        /// A first chunk of exactly `MIN_FIRST_CHUNK_CHARS` characters is
        /// long enough: it must POST a streaming typing activity and advance
        /// the sequence.
        #[tokio::test]
        async fn first_chunk_at_min_threshold_sends() {
            let text = "exactly twenty chars";
            assert_eq!(text.chars().count(), MIN_FIRST_CHUNK_CHARS);

            let server = MockServer::start().await;
            mount_token(&server).await;
            Mock::given(method("POST"))
                .and(path("/v3/conversations/conv-1/activities"))
                .and(body_string_contains(text))
                .and(body_string_contains("streaminfo"))
                .respond_with(ResponseTemplate::new(200))
                .expect(1)
                .mount(&server)
                .await;

            let client = client_for(&server);
            let mut stream = TeamsStream::new(&client, &server.uri(), "conv-1", "act-1");
            stream.send_chunk(text).await.unwrap();
            assert_eq!(stream.sequence(), 1);
            assert!(stream.has_content());
        }

        /// A chunk arriving inside the throttle window is buffered; the
        /// sequence does not advance and no extra request goes out.
        #[tokio::test]
        async fn chunk_inside_throttle_window_is_buffered() {
            let server = MockServer::start().await;
            mount_token(&server).await;
            mount_activities(&server, 1).await;

            let client = client_for(&server);
            let mut stream = TeamsStream::new(&client, &server.uri(), "conv-1", "act-1");
            stream.send_chunk("a first chunk that is long enough").await.unwrap();
            assert_eq!(stream.sequence(), 1);
            stream.send_chunk(" immediately more").await.unwrap();
            assert_eq!(stream.sequence(), 1, "second chunk must be throttled");
        }

        /// Consecutive sends increment `streamSequence` by one and carry the
        /// cumulative buffer.
        #[tokio::test]
        async fn sequence_increments_across_sends() {
            let server = MockServer::start().await;
            mount_token(&server).await;
            mount_activities(&server, 2).await;

            let client = client_for(&server);
            let mut stream = TeamsStream::new(&client, &server.uri(), "conv-1", "act-1");
            stream.throttle = Duration::ZERO;
            stream.send_chunk("a first chunk that is long enough").await.unwrap();
            assert_eq!(stream.sequence(), 1);
            stream.send_chunk(" and a second piece").await.unwrap();
            assert_eq!(stream.sequence(), 2);

            let requests = server.received_requests().await.unwrap();
            let bodies: Vec<serde_json::Value> = requests
                .iter()
                .filter(|r| r.url.path().ends_with("/activities"))
                .map(|r| serde_json::from_slice(&r.body).unwrap())
                .collect();
            assert_eq!(bodies.len(), 2);
            assert_eq!(bodies[0]["entities"][0]["streamSequence"], 1);
            assert_eq!(bodies[1]["entities"][0]["streamSequence"], 2);
            assert_eq!(
                bodies[1]["text"],
                "a first chunk that is long enough and a second piece",
                "later updates must carry the cumulative buffer"
            );
        }

        /// `finalize` must POST the final message as a reply to the original
        /// activity — and surface a hard API failure instead of reporting
        /// success.
        #[tokio::test]
        async fn finalize_posts_final_reply_and_surfaces_errors() {
            let server = MockServer::start().await;
            mount_token(&server).await;
            Mock::given(method("POST"))
                .and(path("/v3/conversations/conv-1/activities/act-9"))
                .and(body_string_contains("\"final\""))
                .and(body_string_contains("complete answer"))
                .respond_with(ResponseTemplate::new(200))
                .expect(1)
                .mount(&server)
                .await;

            let client = client_for(&server);
            let mut stream = TeamsStream::new(&client, &server.uri(), "conv-1", "act-9");
            stream.finalize("complete answer").await.unwrap();

            // Failure direction: a 403 must come back as an error.
            let server = MockServer::start().await;
            mount_token(&server).await;
            Mock::given(method("POST"))
                .and(path("/v3/conversations/conv-1/activities/act-9"))
                .respond_with(ResponseTemplate::new(403))
                .expect(1)
                .mount(&server)
                .await;
            let client = client_for(&server);
            let mut stream = TeamsStream::new(&client, &server.uri(), "conv-1", "act-9");
            stream
                .finalize("complete answer")
                .await
                .expect_err("a rejected final message must not report success");
        }
    }
}
