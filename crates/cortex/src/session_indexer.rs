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

    // Build transcript from user + assistant messages (skip tool role)
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
            if content.is_empty() {
                continue;
            }
            let role_label = match msg.role.as_str() {
                "user" => "User",
                "assistant" => "Assistant",
                _ => &msg.role,
            };
            let truncated: String = content
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

    #[cfg(feature = "phase9-tests-disabled")]
    #[test]
    fn get_unindexed_sessions_works() {}
}
