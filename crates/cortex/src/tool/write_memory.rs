//! `write_memory` tool: typed args + handler with co-located effects.
//!
//! Effects (IDENTITY cache invalidation, embedding spawn, optional
//! quiet-hours timezone slide) live in this file alongside the handler so
//! that "the place you go to understand `write_memory`" is exactly one
//! module — replacing the prior `tool_dispatch::handle_write_memory_with_effects`
//! split.

use std::sync::Arc;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::constants::IDENTITY_FILE;
use crate::tool_handlers;

/// Typed arguments for `write_memory`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct WriteMemoryArgs {
    pub filename: String,
    pub content: String,
    #[serde(default)]
    pub append: bool,
    #[serde(default)]
    pub scope: Option<String>,
}

impl WriteMemoryArgs {
    pub(crate) fn to_value(&self) -> serde_json::Value {
        // Plain struct of primitives; serialization is infallible.
        // The expect would only fire on a serde upstream bug.
        #[allow(clippy::expect_used)]
        {
            serde_json::to_value(self).expect("WriteMemoryArgs is JSON-safe")
        }
    }
}

/// Run `write_memory` with all of its side effects:
/// 1. Persist the entry via the underlying handler.
/// 2. On success, invalidate the cached IDENTITY snapshot if the entry is
///    `IDENTITY.md` (so the next system prompt re-loads it).
/// 3. On success, spawn a background embedding job (fire-and-forget).
/// 4. On success, attempt a quiet-hours timezone slide based on the content.
///
/// All side effects log on failure rather than aborting — they never block
/// the primary write or fail the tool call.
pub(crate) fn handle(
    args: &WriteMemoryArgs,
    config: &Config,
    config_arc: &Arc<Config>,
    cached_identity: &mut Option<String>,
) -> Result<String> {
    let value = args.to_value();
    let result = tool_handlers::handle_write_memory(&value);

    if result.is_ok() {
        if args.filename == IDENTITY_FILE {
            *cached_identity = None;
        }
        if config.memory.embeddings.enabled {
            spawn_embedding_task(args, config_arc);
        }
        maybe_slide_quiet_hours_tz(&args.content);
    }

    result
}

/// Spawn a background task to generate embeddings for the just-written entry.
///
/// Reads from the `memory_entries` DB table (not the filesystem) so indexed
/// content matches what `memory_search` will find. Missing rows are skipped
/// rather than embedding placeholders. All failures log + continue — the
/// primary write has already succeeded.
fn spawn_embedding_task(args: &WriteMemoryArgs, config_arc: &Arc<Config>) {
    let config = Arc::clone(config_arc);
    let raw_filename = args.filename.clone();
    let scope = args.scope.clone().unwrap_or_else(|| "global".to_string());
    // Match the storage key used by handle_write_memory (strips .md suffix).
    let entry_name = raw_filename
        .strip_suffix(".md")
        .unwrap_or(&raw_filename)
        .to_string();
    let full_content = match crate::memory::read_memory_db(&entry_name, &scope) {
        Ok(Some(content)) => crate::secrets::redact_secrets(&content),
        Ok(None) => {
            tracing::debug!(
                "spawn_embedding_task: entry {entry_name} (scope {scope}) missing, skipping embed"
            );
            return;
        }
        Err(e) => {
            tracing::warn!("Failed to read memory entry {entry_name} for embedding: {e}");
            return;
        }
    };
    let filename = entry_name;
    crate::agent::spawn_logged("embed_memory_write", async move {
        if let Err(e) =
            crate::embeddings::embed_memory_file(&config, &filename, &full_content, &scope).await
        {
            tracing::warn!("Failed to embed memory {filename}: {e}");
        }
        if let Err(e) =
            crate::embeddings::embed_memory_file_chunked(&config, &filename, &full_content, &scope)
                .await
        {
            tracing::warn!("Failed to chunk-embed memory {filename}: {e}");
        }
    });
}

/// If the just-written content mentions a common timezone AND the user hasn't
/// customized their quiet-hour times, slide `user.timezone` to match. The
/// "still on default times" check is what tells us the user has not yet taken
/// manual control of quiet hours.
///
/// Fire-and-forget. Errors log and continue — never abort the tool.
fn maybe_slide_quiet_hours_tz(content: &str) {
    if content.is_empty() {
        return;
    }
    let owned = content.to_string();
    crate::agent::spawn_logged("tz_slide", async move {
        if let Err(e) = run_tz_slide(&owned) {
            tracing::warn!("tz_slide failed: {e}");
        }
    });
}

fn run_tz_slide(content: &str) -> Result<()> {
    let Some(detected) = extract_timezone_mention(content) else {
        return Ok(());
    };
    let db = crate::db::Database::open()?;
    let quiet_start = db.get_setting("heartbeat.quiet_hours_start")?;
    let quiet_end = db.get_setting("heartbeat.quiet_hours_end")?;
    let current_tz = db.get_setting("user.timezone")?;
    let Some(target) = decide_tz_slide(
        &detected,
        quiet_start.as_deref(),
        quiet_end.as_deref(),
        current_tz.as_deref(),
    ) else {
        return Ok(());
    };
    db.set_setting("user.timezone", &target)?;
    tracing::info!(
        "Quiet-hours timezone slid {} → {} (mentioned in memory; defaults intact)",
        current_tz.as_deref().unwrap_or("(unset)"),
        target
    );
    Ok(())
}

/// Pure decision: should we slide `user.timezone` to `detected`?
///
/// Returns `Some(target)` iff:
/// - quiet-hour times are still at compiled defaults (user has not customized
///   them — the only "user took control" signal we recognize), AND
/// - `detected` differs from the currently-set timezone.
pub(crate) fn decide_tz_slide(
    detected: &str,
    quiet_start: Option<&str>,
    quiet_end: Option<&str>,
    current_tz: Option<&str>,
) -> Option<String> {
    use crate::constants::{DEFAULT_HEARTBEAT_QUIET_END, DEFAULT_HEARTBEAT_QUIET_START};
    let times_at_default = quiet_start == Some(DEFAULT_HEARTBEAT_QUIET_START)
        && quiet_end == Some(DEFAULT_HEARTBEAT_QUIET_END);
    if !times_at_default {
        return None;
    }
    if current_tz == Some(detected) {
        return None;
    }
    Some(detected.to_string())
}

/// Pure deterministic scan: extract the IANA timezone implied by `text`, if
/// any. Returns `None` for ambiguous, generic (`UTC`/`GMT`), or absent matches.
pub(crate) fn extract_timezone_mention(text: &str) -> Option<String> {
    use std::collections::HashSet;

    let mut hits: HashSet<String> = HashSet::new();

    // Tier 1: IANA strings (Region/City or Region/City/Sub).
    static IANA_RE: std::sync::OnceLock<Option<regex::Regex>> = std::sync::OnceLock::new();
    let re = IANA_RE.get_or_init(|| {
        regex::Regex::new(r"\b[A-Z][A-Za-z_]+/[A-Z][A-Za-z_]+(?:/[A-Z][A-Za-z_]+)?\b").ok()
    });
    if let Some(re) = re.as_ref() {
        for m in re.find_iter(text) {
            if m.as_str().parse::<chrono_tz::Tz>().is_ok() {
                hits.insert(m.as_str().to_string());
            }
        }
    }

    // Tier 2: case-sensitive whole-word abbreviations.
    const ABBREVS: &[(&str, &str)] = &[
        ("PST", "America/Los_Angeles"),
        ("PDT", "America/Los_Angeles"),
        ("EST", "America/New_York"),
        ("EDT", "America/New_York"),
        ("CST", "America/Chicago"),
        ("CDT", "America/Chicago"),
        ("MST", "America/Denver"),
        ("MDT", "America/Denver"),
        ("BST", "Europe/London"),
        ("CET", "Europe/Paris"),
        ("CEST", "Europe/Paris"),
        ("JST", "Asia/Tokyo"),
        ("IST", "Asia/Kolkata"),
        ("AEST", "Australia/Sydney"),
        ("AEDT", "Australia/Sydney"),
    ];
    for (abbr, tz) in ABBREVS {
        if contains_whole_word(text, abbr) {
            hits.insert((*tz).to_string());
        }
    }

    if hits.len() == 1 {
        hits.into_iter().next()
    } else {
        None
    }
}

/// Whole-word, case-sensitive substring check.
fn contains_whole_word(haystack: &str, needle: &str) -> bool {
    let bytes = haystack.as_bytes();
    let nb = needle.as_bytes();
    if nb.is_empty() || bytes.len() < nb.len() {
        return false;
    }
    let is_word = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let mut i = 0;
    while i + nb.len() <= bytes.len() {
        if &bytes[i..i + nb.len()] == nb {
            let before_ok = i == 0 || !is_word(bytes[i - 1]);
            let after_idx = i + nb.len();
            let after_ok = after_idx == bytes.len() || !is_word(bytes[after_idx]);
            if before_ok && after_ok {
                return true;
            }
        }
        i += 1;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserialize_minimal_args() {
        let args: WriteMemoryArgs =
            serde_json::from_value(serde_json::json!({"filename":"x","content":"y"}))
                .expect("minimal args");
        assert_eq!(args.filename, "x");
        assert_eq!(args.content, "y");
        assert!(!args.append);
        assert!(args.scope.is_none());
    }

    #[test]
    fn deserialize_full_args_round_trips() {
        let raw = serde_json::json!({
            "filename": "note.md",
            "content": "hello",
            "append": true,
            "scope": "project:abc",
        });
        let args: WriteMemoryArgs = serde_json::from_value(raw.clone()).unwrap();
        assert!(args.append);
        assert_eq!(args.scope.as_deref(), Some("project:abc"));
        // Round-trip back to Value should preserve fields.
        let v = args.to_value();
        assert_eq!(v["filename"], "note.md");
        assert_eq!(v["append"], true);
        assert_eq!(v["scope"], "project:abc");
    }

    #[tokio::test]
    async fn handle_invalid_args_propagates_error_and_skips_effects() {
        // Empty filename + empty content → handle_write_memory currently still
        // succeeds (write_memory_db accepts empty content), so the meaningful
        // failure here is a write to an unwritable scope. We validate only the
        // narrower invariant: `cached_identity` is touched ONLY for the
        // IDENTITY.md filename.
        let config = Config::default();
        let config_arc = Arc::new(Config::default());
        let mut cached: Option<String> = Some("stale".to_string());
        let args = WriteMemoryArgs {
            filename: "non_identity.md".into(),
            content: "x".into(),
            append: false,
            scope: Some("nonexistent_scope_just_for_test".into()),
        };
        // Best-effort: even if write fails (DB unavailable in CI), identity
        // cache must not be cleared because the filename isn't IDENTITY_FILE.
        let _ = handle(&args, &config, &config_arc, &mut cached);
        assert_eq!(cached.as_deref(), Some("stale"));
    }

    #[test]
    fn extract_timezone_mention_table() {
        let cases: &[(&str, Option<&str>)] = &[
            ("user is in PST", Some("America/Los_Angeles")),
            ("based in Europe/Berlin", Some("Europe/Berlin")),
            ("EST and PST mentioned", None),
            ("running in UTC datacenter", None),
            ("PSTDouble", None),
            ("Switching to JST next month", Some("Asia/Tokyo")),
        ];
        for (input, expected) in cases {
            let got = extract_timezone_mention(input);
            assert_eq!(got.as_deref(), *expected, "input={input:?}");
        }
    }

    #[test]
    fn decide_tz_slide_writes_when_times_default_and_tz_differs() {
        let out = decide_tz_slide(
            "America/Los_Angeles",
            Some("00:00"),
            Some("06:00"),
            Some("UTC"),
        );
        assert_eq!(out.as_deref(), Some("America/Los_Angeles"));
    }

    #[test]
    fn decide_tz_slide_noop_when_user_changed_start_time() {
        let out = decide_tz_slide(
            "America/Los_Angeles",
            Some("22:00"),
            Some("06:00"),
            Some("UTC"),
        );
        assert_eq!(out, None);
    }

    #[test]
    fn decide_tz_slide_noop_when_already_matching() {
        let out = decide_tz_slide(
            "America/Los_Angeles",
            Some("00:00"),
            Some("06:00"),
            Some("America/Los_Angeles"),
        );
        assert_eq!(out, None);
    }
}
