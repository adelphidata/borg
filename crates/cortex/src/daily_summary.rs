//! Deterministic data gathering for the daily summary scheduled task.
//!
//! Aggregates session transcripts and task run stats from the past 24 hours,
//! then combines them with an LLM prompt template for summarization.

use anyhow::Result;

use crate::db::{Database, MessageRow};

/// Fixed UUID for the built-in Daily Summary task.
pub const DAILY_SUMMARY_TASK_ID: &str = "00000000-0000-4000-8000-da1175000002";

/// LLM instructions prepended to the gathered context.
const SUMMARY_PROMPT: &str = include_str!("../templates/tasks/DAILY_SUMMARY.md");

/// Maximum total characters for all session transcripts combined.
const MAX_TOTAL_CHARS: usize = 8000;

/// Maximum characters per individual session transcript.
const MAX_PER_SESSION_CHARS: usize = 2000;

/// Build the full prompt for the daily summary task.
///
/// Gathers activity data from the past 24 hours, then prepends the
/// summarization instructions. Returns a string ready to send as the
/// user message to the LLM.
pub fn build_daily_summary_prompt() -> Result<String> {
    let db = Database::open()?;
    let context = gather_daily_context(&db)?;
    Ok(format!("{SUMMARY_PROMPT}{context}"))
}

/// Gather activity context from the past 24 hours.
///
/// Returns a formatted markdown string with:
/// - Session transcripts (user/assistant messages only)
/// - Task run statistics
pub fn gather_daily_context(db: &Database) -> Result<String> {
    let since = chrono::Utc::now().timestamp() - 86400;
    let mut out = String::new();

    // ── Sessions ──
    let sessions = db.sessions_since(since)?;
    out.push_str(&format!("## Sessions ({} in last 24h)\n\n", sessions.len()));

    let mut total_chars = 0;
    for session in &sessions {
        let budget_remaining = MAX_TOTAL_CHARS.saturating_sub(total_chars);
        if budget_remaining == 0 {
            out.push_str("_(remaining sessions truncated)_\n\n");
            break;
        }

        let title = if session.title.is_empty() {
            "Untitled"
        } else {
            &session.title
        };
        let time = chrono::DateTime::from_timestamp(session.updated_at, 0)
            .map(|dt| dt.format("%H:%M UTC").to_string())
            .unwrap_or_default();
        out.push_str(&format!("### {title} ({time})\n\n"));

        let messages = match db.load_session_messages(&session.id) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!("Failed to load messages for session {}: {e}", session.id);
                Vec::new()
            }
        };
        let transcript = build_transcript(&messages, MAX_PER_SESSION_CHARS.min(budget_remaining));
        total_chars += transcript.len();
        out.push_str(&transcript);
        out.push('\n');
    }

    // ── Task runs ──
    let (success, total) = db.count_task_runs_since(since, Some("success"))?;
    let failed = total - success;
    out.push_str(&format!(
        "## Task Runs\n\n{total} total, {success} successful, {failed} failed\n"
    ));

    Ok(out)
}

/// Build a text transcript from message rows, keeping only user/assistant messages.
fn build_transcript(messages: &[MessageRow], max_chars: usize) -> String {
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
                .take(crate::constants::MAX_SESSION_MESSAGE_CHARS)
                .collect();
            transcript.push_str(&format!("{role_label}: {truncated}\n\n"));

            if transcript.len() >= max_chars {
                break;
            }
        }
    }
    transcript
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_db() -> Database {
        Database::test_db()
    }

    #[test]
    fn gather_daily_context_empty_db() {
        let db = test_db();
        let ctx = gather_daily_context(&db).expect("should succeed");
        assert!(ctx.contains("## Sessions (0 in last 24h)"));
        assert!(ctx.contains("## Task Runs"));
        assert!(ctx.contains("0 total"));
    }

    #[test]
    fn gather_daily_context_with_sessions() {
        let db = test_db();
        let now = chrono::Utc::now().timestamp();
        db.upsert_session("s1", now - 100, now - 50, 500, "test-model", "My Session")
            .unwrap();
        db.insert_message("s1", "user", Some("Hello world"), None, None, None, None)
            .unwrap();
        db.insert_message("s1", "assistant", Some("Hi there!"), None, None, None, None)
            .unwrap();
        // Tool messages should be filtered out
        db.insert_message(
            "s1",
            "tool",
            Some("tool output"),
            None,
            Some("call1"),
            None,
            None,
        )
        .unwrap();

        let ctx = gather_daily_context(&db).expect("should succeed");
        assert!(ctx.contains("## Sessions (1 in last 24h)"));
        assert!(ctx.contains("### My Session"));
        assert!(ctx.contains("User: Hello world"));
        assert!(ctx.contains("Assistant: Hi there!"));
        assert!(!ctx.contains("tool output"));
    }

    #[test]
    fn gather_daily_context_excludes_old_sessions() {
        let db = test_db();
        let now = chrono::Utc::now().timestamp();
        // Session from 2 days ago
        let two_days_ago = now - 2 * 86400;
        db.upsert_session(
            "old",
            two_days_ago,
            two_days_ago,
            100,
            "model",
            "Old Session",
        )
        .unwrap();
        db.insert_message("old", "user", Some("old message"), None, None, None, None)
            .unwrap();

        let ctx = gather_daily_context(&db).expect("should succeed");
        assert!(ctx.contains("## Sessions (0 in last 24h)"));
        assert!(!ctx.contains("Old Session"));
        assert!(!ctx.contains("old message"));
    }

    #[test]
    fn build_daily_summary_prompt_includes_template() {
        let result = SUMMARY_PROMPT;
        assert!(result.contains("daily standup report"));
        assert!(result.contains("Done"));
        assert!(result.contains("Today"));
        assert!(result.contains("Blockers"));
    }

    #[test]
    fn build_transcript_filters_roles() {
        let messages = vec![
            MessageRow {
                id: 1,
                session_id: "s1".into(),
                role: "system".into(),
                content: Some("system prompt".into()),
                tool_calls_json: None,
                tool_call_id: None,
                timestamp: None,
                created_at: 0,
                content_parts_json: None,
            },
            MessageRow {
                id: 2,
                session_id: "s1".into(),
                role: "user".into(),
                content: Some("hello".into()),
                tool_calls_json: None,
                tool_call_id: None,
                timestamp: None,
                created_at: 0,
                content_parts_json: None,
            },
            MessageRow {
                id: 3,
                session_id: "s1".into(),
                role: "tool".into(),
                content: Some("result".into()),
                tool_calls_json: None,
                tool_call_id: Some("c1".into()),
                timestamp: None,
                created_at: 0,
                content_parts_json: None,
            },
            MessageRow {
                id: 4,
                session_id: "s1".into(),
                role: "assistant".into(),
                content: Some("world".into()),
                tool_calls_json: None,
                tool_call_id: None,
                timestamp: None,
                created_at: 0,
                content_parts_json: None,
            },
        ];

        let transcript = build_transcript(&messages, 10000);
        assert!(transcript.contains("User: hello"));
        assert!(transcript.contains("Assistant: world"));
        assert!(!transcript.contains("system prompt"));
        assert!(!transcript.contains("result"));
    }
}
