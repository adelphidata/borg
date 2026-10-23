//! `write_memory` tool: typed args + handler with co-located effects.
//!
//! Effects (IDENTITY cache invalidation, embedding spawn, optional
//! quiet-hours timezone slide, the owner's `user_name`) live in this file
//! alongside the handler so that "the place you go to understand
//! `write_memory`" is exactly one module — replacing the prior
//! `tool_dispatch::handle_write_memory_with_effects` split.

use std::sync::Arc;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::agent::system_prompt_cache::SystemPromptCache;
use crate::config::Config;
use crate::tool_handlers;

/// Action for a batch [`MemoryOp`]. `add` appends to an existing entry (or
/// creates it) so it can never drop sibling facts; `replace` is the explicit
/// whole-entry overwrite. Either verb with `old_text` patches exactly one
/// span. `remove` deletes the entry (and its embeddings/chunks).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum MemoryOpAction {
    Add,
    Replace,
    Remove,
}

/// One operation in a batch `write_memory` call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct MemoryOp {
    pub action: MemoryOpAction,
    pub filename: String,
    /// Required for `add`/`replace`; ignored for `remove`.
    #[serde(default)]
    pub content: Option<String>,
    /// Exact span of the current entry that `content` replaces. Must match
    /// exactly once; the rest of the entry is preserved.
    #[serde(default)]
    pub old_text: Option<String>,
    #[serde(default)]
    pub append: bool,
    /// Per-op scope. Inherits the call's top-level scope when omitted; a
    /// value that conflicts with a given top-level scope rejects the batch.
    #[serde(default)]
    pub scope: Option<String>,
}

/// Typed arguments for `write_memory`.
///
/// Two mutually exclusive shapes:
/// - **single op**: `filename` + `content` (+ optional `append`/`scope`);
/// - **batch**: `operations` — a list of add/replace/remove ops applied
///   atomically against the final per-entry token budget. When `operations`
///   is present, the top-level single-op fields are ignored.
///
/// Either shape may carry `user_name`, or it may come alone.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct WriteMemoryArgs {
    #[serde(default)]
    pub filename: Option<String>,
    #[serde(default)]
    pub content: Option<String>,
    /// Single-op patch: the exact span of the current entry that `content`
    /// replaces (must match exactly once).
    #[serde(default)]
    pub old_text: Option<String>,
    #[serde(default)]
    pub append: bool,
    #[serde(default)]
    pub scope: Option<String>,
    #[serde(default)]
    pub operations: Option<Vec<MemoryOp>>,
    /// What the owner is called: saved to the `user.name` setting, which the
    /// app shows and the system prompt reads. Rides on this tool rather than
    /// a tool of its own, since every schema is sent on every turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_name: Option<String>,
}

impl WriteMemoryArgs {
    /// The name to save, if the call carries one. Blank counts as absent: a
    /// model that fills every optional field with "" must not wipe the name.
    pub(crate) fn user_name(&self) -> Option<&str> {
        self.user_name
            .as_deref()
            .map(str::trim)
            .filter(|n| !n.is_empty())
    }

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
/// 5. On success, save `user_name` when the call carries one.
///
/// Side effects 2–4 log on failure rather than aborting — they never block
/// the primary write or fail the tool call. The name is a write of its own,
/// so its failure fails the call; it is checked before the entry is written
/// so a bad name rejects the call with nothing changed.
pub(crate) async fn handle(
    args: &WriteMemoryArgs,
    config: &Config,
    config_arc: &Arc<Config>,
    caches: &mut SystemPromptCache,
) -> Result<String> {
    let user_name = args.user_name();
    if let Some(name) = user_name {
        // A channel message may come from anyone the owner let in, and only
        // the owner names the owner.
        if crate::gateway_context::current().is_some() {
            anyhow::bail!(
                "user_name can only be saved from the owner's own app, not from a channel message"
            );
        }
        crate::user_name::validate(name).map_err(anyhow::Error::msg)?;
    }
    let has_entry = args.operations.as_ref().is_some_and(|ops| !ops.is_empty())
        || args.filename.as_deref().is_some_and(|f| !f.is_empty());
    let written = match (has_entry, user_name) {
        (false, Some(_)) => None,
        _ => Some(write_entries(args, config, config_arc, caches).await?),
    };
    let Some(name) = user_name else {
        return Ok(written.unwrap_or_default());
    };
    let saved = crate::user_name::save(name).await?;
    let line = format!("Owner's name saved: {saved}. The app shows it in the sidebar.");
    Ok(match written {
        Some(text) => format!("{text}\n{line}"),
        None => line,
    })
}

/// The memory-entry half of [`handle`]: one op or a batch.
async fn write_entries(
    args: &WriteMemoryArgs,
    config: &Config,
    config_arc: &Arc<Config>,
    caches: &mut SystemPromptCache,
) -> Result<String> {
    // Batch mode: when `operations` is present and non-empty it takes
    // precedence and the single-op fields are ignored.
    if let Some(ops) = args.operations.as_deref() {
        if !ops.is_empty() {
            return handle_batch(ops, args.scope.as_deref(), config, config_arc, caches).await;
        }
    }

    let filename = match args.filename.as_deref() {
        Some(f) if !f.is_empty() => f,
        _ => anyhow::bail!(
            "write_memory requires 'filename'+'content', a non-empty 'operations' array, or 'user_name'"
        ),
    };
    let value = args.to_value();
    let result = tool_handlers::handle_write_memory(&value, config).await;

    if result.is_ok() {
        let scope = args.scope.as_deref().unwrap_or("global");
        caches.on_memory_written(scope, filename);
        if config.memory.embeddings.enabled {
            spawn_embedding_task(filename, args.scope.as_deref(), config_arc);
        }
        maybe_slide_quiet_hours_tz(args.content.as_deref().unwrap_or(""));
    }

    result
}

/// Apply a batch of memory operations atomically. The validate-then-apply
/// happens in [`tool_handlers::handle_write_memory_batch`]; here we fire the
/// per-op side effects (IDENTITY-cache invalidation, embedding, tz-slide) on
/// success, mirroring the single-op path. Side effects use the same
/// per-op effective scope the handler wrote to (top-level scope inherited
/// by ops that omit their own).
async fn handle_batch(
    ops: &[MemoryOp],
    top_scope: Option<&str>,
    config: &Config,
    config_arc: &Arc<Config>,
    caches: &mut SystemPromptCache,
) -> Result<String> {
    let summary = tool_handlers::handle_write_memory_batch(ops, top_scope, config).await?;
    let scopes = tool_handlers::effective_batch_scopes(ops, top_scope)?;

    for (op, scope) in ops.iter().zip(scopes.iter()) {
        // Invalidate the IDENTITY cache for any touched entry (incl. removals).
        caches.on_memory_written(scope, &op.filename);
        match op.action {
            MemoryOpAction::Add | MemoryOpAction::Replace => {
                let entry_name = op.filename.strip_suffix(".md").unwrap_or(&op.filename);
                if config.memory.embeddings.enabled {
                    spawn_embedding_task(entry_name, Some(scope), config_arc);
                }
                if let Some(c) = op.content.as_deref() {
                    maybe_slide_quiet_hours_tz(c);
                }
            }
            MemoryOpAction::Remove => {}
        }
    }

    Ok(summary)
}

/// Spawn a background task to generate embeddings for the just-written entry.
///
/// Reads from the `memory_entries` DB table (not the filesystem) so indexed
/// content matches what `memory_search` will find. Missing rows are skipped
/// rather than embedding placeholders. All failures log + continue — the
/// primary write has already succeeded.
fn spawn_embedding_task(raw_filename: &str, scope: Option<&str>, config_arc: &Arc<Config>) {
    let config = Arc::clone(config_arc);
    let raw_filename = raw_filename.to_string();
    let scope = scope.unwrap_or("global").to_string();
    // Match the storage key used by handle_write_memory (strips .md suffix).
    let entry_name = raw_filename
        .strip_suffix(".md")
        .unwrap_or(&raw_filename)
        .to_string();
    let filename = entry_name;
    crate::agent::spawn_logged("embed_memory_write", async move {
        let full_content = match crate::memory::read_memory_db(&filename, &scope).await {
            Ok(Some(content)) => crate::security::secrets::redact_secrets(&content),
            Ok(None) => {
                tracing::debug!(
                    "spawn_embedding_task: entry {filename} (scope {scope}) missing, skipping embed"
                );
                return;
            }
            Err(e) => {
                tracing::warn!("Failed to read memory entry {filename} for embedding: {e}");
                return;
            }
        };
        if let Err(e) =
            crate::memory::embeddings::embed_memory_file(&config, &filename, &full_content, &scope)
                .await
        {
            tracing::warn!("Failed to embed memory {filename}: {e}");
        }
        if let Err(e) = crate::memory::embeddings::embed_memory_file_chunked(
            &config,
            &filename,
            &full_content,
            &scope,
        )
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
        if let Err(e) = run_tz_slide(&owned).await {
            tracing::warn!("tz_slide failed: {e}");
        }
    });
}

async fn run_tz_slide(content: &str) -> Result<()> {
    let Some(detected) = extract_timezone_mention(content) else {
        return Ok(());
    };
    let db = crate::db::Database::open().await?;
    let quiet_start = db.get_setting("heartbeat.quiet_hours_start").await?;
    let quiet_end = db.get_setting("heartbeat.quiet_hours_end").await?;
    let current_tz = db.get_setting("user.timezone").await?;
    let Some(target) = decide_tz_slide(
        &detected,
        quiet_start.as_deref(),
        quiet_end.as_deref(),
        current_tz.as_deref(),
    ) else {
        return Ok(());
    };
    db.set_setting("user.timezone", &target).await?;
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
        assert_eq!(args.filename.as_deref(), Some("x"));
        assert_eq!(args.content.as_deref(), Some("y"));
        assert!(!args.append);
        assert!(args.scope.is_none());
        assert!(args.operations.is_none());
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
    async fn handle_non_identity_write_preserves_identity_cache() {
        // The cache invalidation rule is "writing IDENTITY.md clears the
        // identity cache." A write to any other filename must leave the
        // identity slot intact even if the write itself succeeds or fails.
        let config = Config::default();
        let config_arc = Arc::new(Config::default());
        let mut caches = SystemPromptCache::with_test_state(
            Some("stale-identity".into()),
            None,
            None,
            None,
            None,
        );
        let args = WriteMemoryArgs {
            filename: Some("non_identity.md".into()),
            content: Some("x".into()),
            old_text: None,
            append: false,
            scope: Some("nonexistent_scope_just_for_test".into()),
            operations: None,
            user_name: None,
        };
        let _ = handle(&args, &config, &config_arc, &mut caches).await;
        assert_eq!(
            caches.identity_for_test(),
            Some("stale-identity"),
            "non-IDENTITY writes must not clear the identity cache"
        );
    }

    #[test]
    fn blank_user_name_counts_as_absent() {
        let parse = |v| serde_json::from_value::<WriteMemoryArgs>(v).unwrap();
        assert_eq!(
            parse(serde_json::json!({"user_name": "  "})).user_name(),
            None
        );
        assert_eq!(parse(serde_json::json!({})).user_name(), None);
        assert_eq!(
            parse(serde_json::json!({"user_name": " Mike "})).user_name(),
            Some("Mike")
        );
        // Absent stays absent on the way to the entry handler.
        assert!(parse(serde_json::json!({"filename": "x"}))
            .to_value()
            .get("user_name")
            .is_none());
    }

    #[tokio::test]
    async fn a_bad_user_name_rejects_the_call() {
        let config = Config::default();
        let config_arc = Arc::new(Config::default());
        let mut caches = SystemPromptCache::default();
        let args: WriteMemoryArgs =
            serde_json::from_value(serde_json::json!({"user_name": "Mi\nke"})).unwrap();
        let err = handle(&args, &config, &config_arc, &mut caches)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("line breaks"), "{err}");
    }

    /// A channel sender is not necessarily the owner, so a name from one is
    /// refused. The name is also invalid and there is no entry: with the
    /// guard gone, the call fails on validation and still writes nothing, so
    /// this test never reaches the real settings or memory files.
    #[tokio::test]
    async fn a_channel_message_cannot_name_the_owner() {
        let config = Config::default();
        let config_arc = Arc::new(Config::default());
        let mut caches = SystemPromptCache::default();
        let args: WriteMemoryArgs =
            serde_json::from_value(serde_json::json!({"user_name": "Bo\nb"})).unwrap();
        let origin = crate::gateway_context::GatewayOriginContext {
            channel_name: "slack".into(),
            sender_id: "U1".into(),
            thread_id: None,
            session_key: "U1:".into(),
        };
        let err =
            crate::gateway_context::scope(origin, handle(&args, &config, &config_arc, &mut caches))
                .await
                .unwrap_err();
        assert!(err.to_string().contains("channel message"), "{err}");
    }

    #[tokio::test]
    async fn a_call_with_nothing_to_write_names_every_shape() {
        let config = Config::default();
        let config_arc = Arc::new(Config::default());
        let mut caches = SystemPromptCache::default();
        let args: WriteMemoryArgs =
            serde_json::from_value(serde_json::json!({"user_name": ""})).unwrap();
        let err = handle(&args, &config, &config_arc, &mut caches)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("'user_name'"), "{err}");
    }

    #[test]
    fn deserialize_batch_operations() {
        let args: WriteMemoryArgs = serde_json::from_value(serde_json::json!({
            "operations": [
                {"action": "remove", "filename": "stale.md"},
                {"action": "add", "filename": "new.md", "content": "hi"},
                {"action": "replace", "filename": "log.md", "content": "more", "append": true, "scope": "project:x"},
            ]
        }))
        .expect("batch args");
        assert!(args.filename.is_none());
        let ops = args.operations.expect("operations present");
        assert_eq!(ops.len(), 3);
        assert_eq!(ops[0].action, MemoryOpAction::Remove);
        assert!(ops[0].content.is_none());
        assert_eq!(ops[1].action, MemoryOpAction::Add);
        assert_eq!(ops[1].content.as_deref(), Some("hi"));
        assert!(ops[2].append);
        assert_eq!(ops[2].scope.as_deref(), Some("project:x"));
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
