use anyhow::{bail, Context, Result};
use reqwest::Client;

use super::types::CreateMessageRequest;
use crate::constants::{
    GATEWAY_HTTP_TIMEOUT, GOOGLE_CHAT_API_BASE, GOOGLE_CHAT_MESSAGE_CHUNK_SIZE,
};
use crate::message_processing::chunker;
use crate::resilience::http_retry::{send_with_rate_limit_retry, RateLimitPolicy};

/// Validate a value that will be interpolated into a Google Chat REST URL
/// path — a space name or an attachment `resourceName`. Both arrive in the
/// webhook payload and are therefore attacker-influenced, so reject anything
/// that could break out of the path segment: an empty value (malformed
/// `//messages` URL), traversal (`..`), a leading slash (absolute-path /
/// `//host` confusion), or a `?`/`#` that would inject a query string or
/// fragment ahead of our own `?alt=media` / `?messageReplyOption=...`.
/// Interior `/` is allowed — both `spaces/<id>` and attachment resource
/// names legitimately contain it.
fn validate_url_path_component(label: &str, value: &str) -> Result<()> {
    if value.is_empty()
        || value.contains("..")
        || value.starts_with('/')
        || value.contains('?')
        || value.contains('#')
    {
        bail!("invalid Google Chat {label}: {value:?}");
    }
    Ok(())
}

/// A client for the Google Chat API.
#[derive(Clone)]
pub struct GoogleChatClient {
    client: Client,
    token: String,
    base_url: String,
}

impl GoogleChatClient {
    pub fn new(token: &str) -> Result<Self> {
        Self::with_base(token, GOOGLE_CHAT_API_BASE)
    }

    /// Build a client against an alternate API base URL. Tests point this
    /// at a loopback stub server; production always uses [`Self::new`].
    pub(crate) fn with_base(token: &str, base_url: &str) -> Result<Self> {
        Ok(Self {
            client: Client::builder()
                .timeout(GATEWAY_HTTP_TIMEOUT)
                .build()
                .context("Failed to build Google Chat HTTP client")?,
            token: token.to_string(),
            base_url: base_url.trim_end_matches('/').to_string(),
        })
    }

    /// Download an attachment binary by its `resource_name` (the
    /// `attachmentDataRef.resourceName` field on inbound messages).
    /// Used by the inbound voice-transcription path — voice messages
    /// arrive as `audio/*` attachments that we feed through STT before
    /// the agent runs.
    pub async fn download_attachment(&self, resource_name: &str) -> Result<Vec<u8>> {
        validate_url_path_component("attachment resource name", resource_name)?;
        let url = format!("{}/media/{resource_name}?alt=media", self.base_url);
        let resp = self
            .client
            .get(&url)
            .header("Authorization", format!("Bearer {}", self.token))
            .send()
            .await
            .context("google_chat: download attachment")?;
        if !resp.status().is_success() {
            bail!(
                "google_chat attachment download returned HTTP {}",
                resp.status()
            );
        }
        // Streamed read with a running cap — Content-Length can lie or be
        // absent, so the ceiling is enforced chunk-by-chunk while streaming.
        cortex::media::read_body_capped(
            resp,
            crate::constants::ATTACHMENT_DOWNLOAD_MAX_BYTES,
            "Google Chat attachment",
        )
        .await
        .context("google_chat: read body")
    }

    /// Send a message to a Google Chat space, automatically chunking at 4096 chars.
    /// Optionally replies in a thread if `thread_name` is provided.
    pub async fn send_message(
        &self,
        space_name: &str,
        text: &str,
        thread_name: Option<&str>,
    ) -> Result<()> {
        let chunks = chunker::chunks_for_delivery(text, GOOGLE_CHAT_MESSAGE_CHUNK_SIZE)?;

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
        validate_url_path_component("space name", space_name)?;

        let mut url = format!("{}/{space_name}/messages", self.base_url);

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

        let policy = RateLimitPolicy::for_service("Google Chat");

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
    fn validate_url_path_component_accepts_real_names() {
        validate_url_path_component("space name", "spaces/AAAAAAAA").unwrap();
        validate_url_path_component("attachment resource name", "attachments/AAA/BBB.ext").unwrap();
    }

    #[test]
    fn validate_url_path_component_rejects_traversal_and_injection() {
        // Each value would otherwise break out of the URL path segment:
        // empty → `//messages`; `..` → traversal; leading `/` → absolute
        // path; `?`/`#` → query/fragment injection ahead of our own params.
        let bad = [
            "",
            "spaces/../spaces/EVIL",
            "/spaces/AAA",
            "spaces/AAA?messageReplyOption=EVIL",
            "spaces/AAA#frag",
            "attachments/x?alt=media&foo=bar",
        ];
        for value in bad {
            assert!(
                validate_url_path_component("x", value).is_err(),
                "expected rejection for {value:?}"
            );
        }
    }

    use wiremock::matchers::{body_partial_json, header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    async fn stub_client() -> (MockServer, GoogleChatClient) {
        let server = MockServer::start().await;
        let client = GoogleChatClient::with_base("tok-1", &server.uri()).expect("client");
        (server, client)
    }

    #[tokio::test]
    async fn send_message_posts_each_chunk_with_bearer_auth() {
        // 5000 chars must produce exactly two POSTs (4096 + 904) against
        // `/{space}/messages`, each with the bearer token. A stubbed-out
        // send would leave the server at zero requests.
        let (server, client) = stub_client().await;
        Mock::given(method("POST"))
            .and(path("/spaces/S1/messages"))
            .and(header("authorization", "Bearer tok-1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .expect(2)
            .mount(&server)
            .await;

        let text = "a".repeat(5000);
        client
            .send_message("spaces/S1", &text, None)
            .await
            .expect("send must succeed on 200");

        let reqs = server.received_requests().await.expect("recorded");
        assert_eq!(reqs.len(), 2);
        let first: serde_json::Value = serde_json::from_slice(&reqs[0].body).expect("json");
        let second: serde_json::Value = serde_json::from_slice(&reqs[1].body).expect("json");
        assert_eq!(first["text"].as_str().expect("text").len(), 4096);
        assert_eq!(second["text"].as_str().expect("text").len(), 904);
    }

    #[tokio::test]
    async fn send_message_thread_reply_sets_query_param_and_thread_body() {
        let (server, client) = stub_client().await;
        Mock::given(method("POST"))
            .and(path("/spaces/S1/messages"))
            .and(query_param(
                "messageReplyOption",
                "REPLY_MESSAGE_FALLBACK_TO_NEW_THREAD",
            ))
            .and(body_partial_json(
                serde_json::json!({"thread": {"name": "spaces/S1/threads/T1"}}),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .expect(1)
            .mount(&server)
            .await;

        client
            .send_message("spaces/S1", "hi", Some("spaces/S1/threads/T1"))
            .await
            .expect("threaded send must succeed");
    }

    #[tokio::test]
    async fn send_message_error_status_surfaces_api_error() {
        // A non-2xx from the API must bail with the status + body, not
        // report success — "we said it sent" must mean it sent.
        let (server, client) = stub_client().await;
        Mock::given(method("POST"))
            .and(path("/spaces/S1/messages"))
            .respond_with(ResponseTemplate::new(403).set_body_string("permission denied"))
            .mount(&server)
            .await;

        let err = client
            .send_message("spaces/S1", "hi", None)
            .await
            .expect_err("403 must be an error");
        let msg = err.to_string();
        assert!(msg.contains("403"), "error must carry status: {msg}");
        assert!(
            msg.contains("permission denied"),
            "error must carry body: {msg}"
        );
    }

    #[tokio::test]
    async fn download_attachment_returns_exact_body_bytes() {
        // The bytes feed STT verbatim; any substitution corrupts audio.
        let (server, client) = stub_client().await;
        let payload: Vec<u8> = b"\x00\x01OggS-fake-audio".to_vec();
        Mock::given(method("GET"))
            .and(path("/media/attachments/AUDIO1"))
            .and(query_param("alt", "media"))
            .and(header("authorization", "Bearer tok-1"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(payload.clone()))
            .expect(1)
            .mount(&server)
            .await;

        let got = client
            .download_attachment("attachments/AUDIO1")
            .await
            .expect("download");
        assert_eq!(got, payload);
    }

    #[tokio::test]
    async fn download_attachment_error_status_is_an_error() {
        let (server, client) = stub_client().await;
        Mock::given(method("GET"))
            .and(path("/media/attachments/GONE"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let err = client
            .download_attachment("attachments/GONE")
            .await
            .expect_err("404 must be an error");
        assert!(err.to_string().contains("404"), "got: {err:#}");
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
