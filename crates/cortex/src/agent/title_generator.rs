//! Fire-and-forget session title generation.
//!
//! After the first user→assistant exchange completes, [`auto_title_session`]
//! runs in a background task: it asks the auxiliary LLM for a short
//! human-readable title (3–7 words) summarizing the conversation and writes
//! it back to both the SQLite `sessions.title` column and the on-disk session
//! JSON. The synchronous placeholder title produced by
//! [`crate::session::Session::update_from_history`] is the fallback shown
//! until this task lands.
//!
//! All failures `warn!`-log and return; this path is non-load-bearing and
//! must never crash the agent.

use tracing::warn;

use crate::config::Config;
use crate::db::Database;
use crate::llm::LlmClient;
use crate::session::Session;
use crate::types::Message;

/// Hard cap on input we feed to the aux LLM. Beyond this is wasted aux tokens
/// and rarely improves the title.
const SNIPPET_CHAR_CAP: usize = 500;

/// Hard cap on the final title length (chars). Beyond this it stops being a
/// glance-readable header and starts being a second copy of the message.
const TITLE_CHAR_CAP: usize = 80;

const TITLE_SYSTEM_PROMPT: &str =
    "Generate a short, descriptive title (3-7 words) for a conversation that starts \
     with the following exchange. The title should capture the main topic or intent. \
     Return ONLY the title text, nothing else. No quotes, no punctuation at the end, \
     no prefixes.";

/// The placeholder title set by [`Session::new`]. Used to decide whether the
/// user has set a custom title that we must not clobber.
const DEFAULT_PLACEHOLDER: &str = crate::session::DEFAULT_TITLE;

/// Truncate `s` to at most `n` chars (not bytes) — Unicode-safe.
fn char_truncate(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// Cleanup pipeline for raw LLM output. Pure function so tests can pin the
/// spec without standing up an LLM. Returns `None` on whitespace-only input.
pub(super) fn clean_title(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    // Strip a matched pair of surrounding straight quotes (single or double).
    let stripped = trimmed
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .or_else(|| {
            trimmed
                .strip_prefix('\'')
                .and_then(|s| s.strip_suffix('\''))
        })
        .unwrap_or(trimmed)
        .trim();
    // Strip a leading "Title:" / "title:" prefix the model sometimes emits
    // despite the system-prompt instruction.
    let no_prefix = stripped
        .strip_prefix("Title:")
        .or_else(|| stripped.strip_prefix("title:"))
        .or_else(|| stripped.strip_prefix("TITLE:"))
        .unwrap_or(stripped)
        .trim();
    // Strip trailing sentence punctuation (the system prompt asks for none,
    // but models sometimes add a period). Other punctuation (?, !) survives
    // because it can be meaningful in a title.
    let no_trailing_dot = no_prefix.trim_end_matches('.').trim();
    if no_trailing_dot.is_empty() {
        return None;
    }
    if no_trailing_dot.chars().count() > TITLE_CHAR_CAP {
        // Reserve 1 char for the ellipsis; use the single-char `…` so the
        // ratio of meaningful text → marker stays favorable.
        let head = char_truncate(no_trailing_dot, TITLE_CHAR_CAP - 1);
        Some(format!("{}…", head.trim_end()))
    } else {
        Some(no_trailing_dot.to_string())
    }
}

/// Decide whether `current_title` looks like a user-set custom title that we
/// must not overwrite. We treat the synchronous placeholders from
/// [`Session::update_from_history`] as "still default" — both the literal
/// `"New conversation"` and the 60-char prefix of the first user message
/// itself (which the function derives without any user input).
fn is_default_title(current_title: &str, first_user_message: &str) -> bool {
    if current_title == DEFAULT_PLACEHOLDER {
        return true;
    }
    // The sync titler takes the first paragraph and clips to 60 chars,
    // optionally appending "...". Reconstruct that derivation and compare —
    // if it matches, the title was never touched by a human.
    let first_para = first_user_message
        .split("\n\n")
        .next()
        .unwrap_or(first_user_message);
    let derived: String = first_para.chars().take(60).collect();
    let derived = if first_para.chars().count() > 60 {
        format!("{derived}...")
    } else {
        derived
    };
    current_title == derived
}

/// Build the aux LLM client and request a title for the given exchange.
/// Returns `None` on any aux-pipeline failure (cleaned and logged at `warn`).
async fn generate_session_title(
    session_id: &str,
    user_message: &str,
    assistant_response: &str,
    config: &Config,
) -> Option<String> {
    let user_snippet = char_truncate(user_message, SNIPPET_CHAR_CAP);
    let assistant_snippet = char_truncate(assistant_response, SNIPPET_CHAR_CAP);

    let mut llm = match LlmClient::for_auxiliary(config).await {
        Ok(c) => c.with_prompt_cache_key(session_id),
        Err(e) => {
            warn!("auto-title: aux LLM init failed: {e}");
            return None;
        }
    };

    let messages = vec![
        Message::system(TITLE_SYSTEM_PROMPT),
        Message::user(format!(
            "User: {user_snippet}\n\nAssistant: {assistant_snippet}"
        )),
    ];
    // Streamed aggregate: the timeout is inactivity-based (same contract as
    // compaction), so a slow aux model finishes while a wedged one dies.
    let response = match llm.chat_streamed(&messages).await {
        Ok(m) => m,
        Err(e) => {
            warn!("auto-title: aux chat failed: {e}");
            return None;
        }
    };
    let raw = response.text_content().unwrap_or("");
    clean_title(raw)
}

/// Background entry point. Wraps the full lifecycle: skip if the user has
/// already set a custom title, call the aux LLM, then mirror the result to
/// SQLite and the on-disk session JSON. Never returns an error.
pub(super) async fn auto_title_session(
    session_id: String,
    user_message: String,
    assistant_response: String,
    config: Config,
) {
    // Skip if the user already named the session via `/title` between the
    // first turn ending and this background task running.
    if let Ok(db) = Database::open().await {
        if let Ok(Some(row)) = db.session_by_id(&session_id).await {
            if !is_default_title(&row.title, &user_message) {
                return;
            }
        }
    }

    let Some(title) =
        generate_session_title(&session_id, &user_message, &assistant_response, &config).await
    else {
        return;
    };

    // Mirror to SQLite (source of truth for the daemon-side session list).
    match Database::open().await {
        Ok(db) => {
            if let Err(e) = db.update_session_title(&session_id, &title).await {
                warn!("auto-title: DB update failed for {session_id}: {e}");
            }
        }
        Err(e) => warn!("auto-title: DB open failed: {e}"),
    }

    // Mirror to the JSON session file (source of truth for `list_sessions`
    // in the TUI). Best-effort — if the JSON file isn't on disk yet (forked
    // sessions, in-memory tests) the DB update above is still durable.
    match Session::load(&session_id) {
        Ok(mut session) => {
            session.meta.title = title;
            if let Err(e) = session.save() {
                warn!("auto-title: session JSON save failed for {session_id}: {e}");
            }
        }
        Err(e) => {
            // Not a hard error — the JSON file may legitimately not exist
            // (e.g. tests that skip on-disk persistence).
            tracing::debug!("auto-title: session JSON not loadable for {session_id}: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_title_strips_surrounding_quotes() {
        for (raw, expected) in [
            ("\"Refactor auth flow\"", "Refactor auth flow"),
            ("'Refactor auth flow'", "Refactor auth flow"),
            ("  Refactor auth flow  ", "Refactor auth flow"),
        ] {
            assert_eq!(clean_title(raw).as_deref(), Some(expected), "input={raw:?}");
        }
    }

    #[test]
    fn clean_title_strips_title_prefix() {
        for raw in ["Title: Foo", "title: Foo", "TITLE: Foo", "Title:Foo"] {
            assert_eq!(clean_title(raw).as_deref(), Some("Foo"), "input={raw:?}");
        }
    }

    #[test]
    fn clean_title_strips_trailing_period_but_preserves_question_mark() {
        assert_eq!(clean_title("Foo bar.").as_deref(), Some("Foo bar"));
        assert_eq!(clean_title("Foo bar?").as_deref(), Some("Foo bar?"));
        assert_eq!(clean_title("Foo bar!").as_deref(), Some("Foo bar!"));
    }

    #[test]
    fn clean_title_caps_at_eighty_chars_with_ellipsis() {
        let long: String = "a".repeat(100);
        let cleaned = clean_title(&long).expect("non-empty input must produce a title");
        // ≤80 chars and ends in the single-char ellipsis marker.
        assert!(
            cleaned.chars().count() <= TITLE_CHAR_CAP,
            "len={} cap={}",
            cleaned.chars().count(),
            TITLE_CHAR_CAP
        );
        assert!(cleaned.ends_with('…'), "missing ellipsis: {cleaned:?}");
    }

    #[test]
    fn clean_title_returns_none_on_empty_or_whitespace_input() {
        assert_eq!(clean_title(""), None);
        assert_eq!(clean_title("   \n  \t "), None);
        // After stripping a leading "Title:" the remainder is empty.
        assert_eq!(clean_title("Title:   "), None);
        // After stripping the trailing period the remainder is empty.
        assert_eq!(clean_title("."), None);
    }

    #[test]
    fn clean_title_handles_unicode_input_without_panicking() {
        // Multi-byte characters: a 100-char emoji string must truncate by
        // chars, not bytes — naive `&s[..N]` would panic on a code-point
        // boundary, which is exactly the regression this guards against.
        let unicode: String = "😀".repeat(100);
        let cleaned = clean_title(&unicode).expect("unicode input must produce a title");
        assert!(cleaned.chars().count() <= TITLE_CHAR_CAP);
    }

    #[test]
    fn is_default_title_recognizes_new_conversation_placeholder() {
        assert!(is_default_title("New conversation", "anything"));
    }

    #[test]
    fn is_default_title_recognizes_sync_derived_short_title() {
        // The sync titler in `Session::update_from_history` uses the first
        // paragraph clipped to 60 chars; a short message becomes the title
        // verbatim. The aux pass must treat this as "still default".
        let msg = "Hello, world!";
        assert!(is_default_title(msg, msg));
    }

    #[test]
    fn is_default_title_recognizes_sync_derived_long_title() {
        // First paragraph longer than 60 chars gets "..."-suffixed.
        let msg = "a".repeat(100);
        let derived: String = msg.chars().take(60).collect::<String>() + "...";
        assert!(is_default_title(&derived, &msg));
    }

    #[test]
    fn is_default_title_strips_only_first_paragraph_for_comparison() {
        // The sync titler splits on "\n\n" and takes the first paragraph;
        // a message with trailing metadata blocks should still be detected
        // as default when only the first paragraph survived.
        let msg = "*heartbeat tick*\n\n<proactive_nudges>\n- foo\n</proactive_nudges>";
        assert!(is_default_title("*heartbeat tick*", msg));
    }

    #[test]
    fn is_default_title_rejects_user_chosen_title() {
        // A user-set title (anything not matching the sync derivation or
        // the placeholder) must be preserved.
        assert!(!is_default_title("My custom title", "Hello, world!"));
    }
}
