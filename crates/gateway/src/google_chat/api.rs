use anyhow::{bail, Context, Result};
use reqwest::Client;

use super::types::CreateMessageRequest;
use crate::chunker;
use crate::constants::{
    GATEWAY_HTTP_TIMEOUT, GOOGLE_CHAT_API_BASE, GOOGLE_CHAT_MESSAGE_CHUNK_SIZE,
};
use crate::http_retry::{send_with_rate_limit_retry, RateLimitPolicy};

/// A client for the Google Chat API.
#[derive(Clone)]
pub struct GoogleChatClient {
    client: Client,
    token: String,
}

impl GoogleChatClient {
    pub fn new(token: &str) -> Result<Self> {
        Ok(Self {
            client: Client::builder()
                .timeout(GATEWAY_HTTP_TIMEOUT)
                .build()
                .context("Failed to build Google Chat HTTP client")?,
            token: token.to_string(),
        })
    }

    /// Send a message to a Google Chat space, automatically chunking at 4096 chars.
    /// Optionally replies in a thread if `thread_name` is provided.
    pub async fn send_message(
        &self,
        space_name: &str,
        text: &str,
        thread_name: Option<&str>,
    ) -> Result<()> {
        let chunks = chunker::chunk_text_nonempty(text, GOOGLE_CHAT_MESSAGE_CHUNK_SIZE);

        for chunk in &chunks {
            self.send_single_message(space_name, chunk, thread_name)
                .await?;
        }
        Ok(())
    }

    async fn send_single_message(
        &self,
        space_name: &str,
        text: &str,
        thread_name: Option<&str>,
    ) -> Result<()> {
        // Validate space_name to prevent path traversal
        if space_name.contains("..") || space_name.contains('?') || space_name.contains('#') {
            bail!("Invalid space name: {space_name}");
        }

        let mut url = format!("{GOOGLE_CHAT_API_BASE}/{space_name}/messages");

        let body = CreateMessageRequest {
            text: text.to_string(),
            thread: thread_name.map(|tn| super::types::ThreadRequest {
                name: tn.to_string(),
            }),
        };

        // When replying in a thread, add the messageReplyOption query parameter
        if thread_name.is_some() {
            url.push_str("?messageReplyOption=REPLY_MESSAGE_FALLBACK_TO_NEW_THREAD");
        }

        let policy = RateLimitPolicy {
            service_name: "Google Chat",
            ..RateLimitPolicy::default()
        };

        let resp = send_with_rate_limit_retry(&policy, || async {
            self.client
                .post(&url)
                .bearer_auth(&self.token)
                .json(&body)
                .send()
                .await
                .context("Failed to send Google Chat message")
        })
        .await?;

        let status = resp.status();
        if !status.is_success() {
            let error_body = resp.text().await.unwrap_or_default();
            bail!(
                "Google Chat API error ({}): {}",
                status.as_u16(),
                error_body
            );
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::google_chat::types::ThreadRequest;

    #[test]
    fn api_url_construction() {
        let url = format!("{GOOGLE_CHAT_API_BASE}/spaces/SPACE123/messages");
        assert_eq!(
            url,
            "https://chat.googleapis.com/v1/spaces/SPACE123/messages"
        );
    }

    #[test]
    fn api_url_with_thread_reply_option() {
        let url = format!(
            "{GOOGLE_CHAT_API_BASE}/spaces/SPACE123/messages?messageReplyOption=REPLY_MESSAGE_FALLBACK_TO_NEW_THREAD"
        );
        assert!(url.contains("messageReplyOption=REPLY_MESSAGE_FALLBACK_TO_NEW_THREAD"));
    }

    #[test]
    fn create_message_request_serialization() {
        let req = CreateMessageRequest {
            text: "hello".into(),
            thread: Some(ThreadRequest {
                name: "spaces/SPACE1/threads/THREAD1".into(),
            }),
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["text"], "hello");
        assert_eq!(json["thread"]["name"], "spaces/SPACE1/threads/THREAD1");
    }

    #[test]
    fn create_message_request_no_thread() {
        let req = CreateMessageRequest {
            text: "hello".into(),
            thread: None,
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["text"], "hello");
        assert!(json.get("thread").is_none());
    }

    #[test]
    fn chunking_integration() {
        let long_text: String = "a".repeat(9000);
        let chunks = chunker::chunk_text(&long_text, GOOGLE_CHAT_MESSAGE_CHUNK_SIZE);
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].len(), 4096);
        assert_eq!(chunks[1].len(), 4096);
        assert_eq!(chunks[2].len(), 808);
    }
}
