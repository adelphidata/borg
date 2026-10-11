//! Session transcript indexing: makes past conversations searchable via `memory_search`.
//!
//! Builds a text transcript from user+assistant messages, chunks it, generates embeddings,
//! and stores chunks under the "sessions" scope in `memory_chunks`.

use anyhow::Result;
use tracing::debug;

use crate::config::Config;
use crate::constants;
use crate::db::Database;

/// Index a single session's messages into searchable chunks.
/// Returns the number of messages processed, or 0 if already indexed or empty.
pub async fn index_session(config: &Config, session_id: &str) -> Result<usize> {
    let db = Database::open().await?;

    if db.is_session_indexed(session_id).await? {
        debug!("Session {session_id} already indexed, skipping");
        return Ok(0);
    }

    let messages = db.load_session_lineage_rows(session_id).await?;
    if messages.is_empty() {
        db.mark_session_indexed(session_id, 0).await?;
        return Ok(0);
    }

    let transcript = build_transcript(&messages);

    if transcript.trim().is_empty() {
        db.mark_session_indexed(session_id, 0).await?;
        return Ok(0);
    }

    let filename = format!("session_{session_id}");
    crate::memory::embeddings::embed_memory_file_chunked(
        config,
        &filename,
        &transcript,
        "sessions",
    )
    .await?;

    let count = messages.len();
    db.mark_session_indexed(session_id, count).await?;
    debug!("Indexed session {session_id} ({count} messages)");
    Ok(count)
}

/// Index all unindexed sessions up to `batch_size`.
/// Returns total messages processed across all sessions.
pub async fn index_pending_sessions(config: &Config, batch_size: usize) -> Result<usize> {
    let db = Database::open().await?;
    let session_ids = db.get_unindexed_sessions(batch_size).await?;

    if session_ids.is_empty() {
        return Ok(0);
    }

    debug!("Indexing {} pending sessions", session_ids.len());
    let mut total = 0;
    for sid in session_ids {
        match index_session(config, &sid).await {
            Ok(n) => total += n,
            Err(e) => debug!("Failed to index session {sid}: {e}"),
        }
    }
    Ok(total)
}

/// Maximum total transcript size in characters to prevent memory pressure.
const MAX_TRANSCRIPT_CHARS: usize = crate::constants::MAX_SESSION_TRANSCRIPT_CHARS;

/// Build a text transcript from message rows suitable for chunking and embedding.
fn build_transcript(messages: &[crate::db::MessageRow]) -> String {
    let mut transcript = String::new();
    for msg in messages {
        if msg.role == "tool" || msg.role == "system" {
            continue;
        }
        if let Some(ref content) = msg.content {
            // Strip terminal escape sequences BEFORE truncating so persisted
            // ANSI bytes (colored shell output quoted into a message) never
            // consume the per-message content budget or reach model context
            // on recall.
            let clean = crate::util::ansi::strip_ansi(content);
            if clean.is_empty() {
                continue;
            }
            let role_label = match msg.role.as_str() {
                "user" => "User",
                "assistant" => "Assistant",
                _ => &msg.role,
            };
            let truncated: String = clean
                .chars()
                .take(constants::MAX_SESSION_MESSAGE_CHARS)
                .collect();
            transcript.push_str(&format!("{role_label}: {truncated}\n\n"));

            if transcript.len() >= MAX_TRANSCRIPT_CHARS {
                debug!("Transcript truncated at {} chars", transcript.len());
                break;
            }
        }
    }
    transcript
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::MessageRow;

    fn make_message(role: &str, content: &str) -> MessageRow {
        MessageRow {
            id: 0,
            session_id: "test-session".to_string(),
            role: role.to_string(),
            content: Some(content.to_string()),
            content_parts_json: None,
            tool_calls_json: None,
            tool_call_id: None,
            timestamp: None,
            created_at: 0,
            ..Default::default()
        }
    }

    #[test]
    fn build_transcript_basic() {
        let messages = vec![
            make_message("user", "Hello"),
            make_message("assistant", "Hi there!"),
            make_message("tool", "some tool output"),
            make_message("user", "How are you?"),
        ];
        let transcript = build_transcript(&messages);
        assert!(transcript.contains("User: Hello"));
        assert!(transcript.contains("Assistant: Hi there!"));
        assert!(transcript.contains("User: How are you?"));
        assert!(!transcript.contains("some tool output"));
    }

    #[test]
    fn build_transcript_skips_system() {
        let messages = vec![
            make_message("system", "You are a helpful assistant"),
            make_message("user", "Hello"),
        ];
        let transcript = build_transcript(&messages);
        assert!(!transcript.contains("system"));
        assert!(!transcript.contains("helpful assistant"));
        assert!(transcript.contains("User: Hello"));
    }

    #[test]
    fn build_transcript_truncates_long_messages() {
        let long_content = "x".repeat(5000);
        let messages = vec![make_message("user", &long_content)];
        let transcript = build_transcript(&messages);
        // Should be truncated to 2000 chars + "User: " prefix
        assert!(transcript.len() < 2100);
    }

    #[cfg(feature = "phase9-tests-disabled")]
    #[test]
    fn session_indexing_db_methods() {}

    #[test]
    fn build_transcript_strips_ansi_escapes() {
        // Colored output + cursor moves quoted into a message must come back
        // clean when the transcript re-enters model context via recall.
        let messages = vec![make_message(
            "assistant",
            "result: \u{1b}[31mFAILED\u{1b}[0m\u{1b}[2A\u{1b}[K retry \u{1b}]0;title\u{07}done",
        )];
        let transcript = build_transcript(&messages);
        assert!(transcript.contains("Assistant: result: FAILED retry done"));
        assert!(
            !transcript.contains('\u{1b}'),
            "no escape bytes may survive into the transcript"
        );
    }

    #[test]
    fn build_transcript_truncation_uses_stripped_length() {
        // Content that fits the per-message budget only after stripping must
        // NOT be truncated: escape bytes must not consume the content budget.
        let visible = "v".repeat(constants::MAX_SESSION_MESSAGE_CHARS);
        // Prepend enough ANSI that the raw char count far exceeds the budget.
        let content = format!("{}{visible}", "\u{1b}[31m".repeat(200));
        let messages = vec![make_message("user", &content)];
        let transcript = build_transcript(&messages);
        assert!(
            transcript.contains(&format!("User: {visible}")),
            "full visible text must survive; escapes must not eat the budget"
        );
    }

    #[test]
    fn build_transcript_skips_ansi_only_messages() {
        let messages = vec![make_message("user", "\u{1b}[31m\u{1b}[0m")];
        assert!(build_transcript(&messages).trim().is_empty());
    }

    #[test]
    fn build_transcript_large_session_bounded() {
        let messages: Vec<MessageRow> = (0..1000)
            .map(|i| {
                make_message(
                    "user",
                    &format!("Message number {i} with some padding text to make it longer"),
                )
            })
            .collect();
        let transcript = build_transcript(&messages);
        assert!(
            transcript.len() <= MAX_TRANSCRIPT_CHARS + 2100,
            "transcript should be bounded: got {} chars",
            transcript.len()
        );
    }

    #[test]
    fn build_transcript_all_system_and_tool() {
        let messages = vec![
            make_message("system", "system prompt"),
            make_message("tool", "tool output"),
            make_message("system", "another system"),
        ];
        let transcript = build_transcript(&messages);
        assert!(transcript.trim().is_empty());
    }

    /// A session transcript is embedded by a third-party endpoint. A key the
    /// user pasted into a turn must be redacted before the request leaves
    /// the machine; the mock records the bytes actually sent.
    #[tokio::test]
    async fn transcript_embedding_request_redacts_pasted_secret() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/embeddings"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [{ "embedding": [0.1, 0.2, 0.3] }]
            })))
            .mount(&server)
            .await;

        let aws_key = "AKIAIOSFODNN7EXAMPLE";
        let messages = vec![
            make_message("user", &format!("my access key is {aws_key}, why 403?")),
            make_message("assistant", "Check the bucket policy."),
        ];
        let transcript = build_transcript(&messages);
        assert!(
            transcript.contains(aws_key),
            "the local transcript itself is not the egress boundary"
        );

        let provider = crate::memory::embeddings::EmbeddingProvider {
            endpoint: format!("{}/v1/embeddings", server.uri()),
            api_key: "k".into(),
            model: "text-embedding-3-small".into(),
            dimension: 3,
            provider_tag: "openai".into(),
        };
        let client = reqwest::Client::new();
        let embedding =
            crate::memory::embeddings::generate_embedding(&client, &provider, &transcript)
                .await
                .expect("embedding call must succeed");
        assert_eq!(embedding, vec![0.1, 0.2, 0.3]);

        let requests = server.received_requests().await.expect("recording enabled");
        assert_eq!(requests.len(), 1);
        let body = String::from_utf8_lossy(&requests[0].body).into_owned();
        assert!(
            !body.contains(aws_key) && body.contains("[REDACTED"),
            "embedding request must carry the marker, not the key: {body}"
        );
        assert!(
            body.contains("why 403?") && body.contains("Check the bucket policy."),
            "non-secret transcript text must survive: {body}"
        );
    }

    #[cfg(feature = "phase9-tests-disabled")]
    #[test]
    fn get_unindexed_sessions_works() {}
}
