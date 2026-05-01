//! Tool dispatch helpers extracted from the main agent loop.
//!
//! These functions contain the non-trivial logic that was previously inlined
//! in `execute_tool`'s match arms (write_memory effects, multi-agent routing).

use std::sync::Arc;

use anyhow::Result;

use crate::config::Config;
use crate::constants::IDENTITY_FILE;
use crate::tool_handlers;
use crate::tool_names as tn;

/// Handle `write_memory` with side effects: identity cache invalidation and
/// background embedding generation.
///
/// Returns the tool handler result. Cache invalidation and embedding are
/// fire-and-forget background effects.
pub(crate) fn handle_write_memory_with_effects(
    args: &serde_json::Value,
    config: &Config,
    config_arc: &Arc<Config>,
    cached_identity: &mut Option<String>,
) -> Result<String> {
    let result = tool_handlers::handle_write_memory(args);

    if result.is_ok() {
        let target = args["filename"].as_str().unwrap_or_default();
        if target == IDENTITY_FILE {
            *cached_identity = None;
        }
    }

    if result.is_ok() && config.memory.embeddings.enabled {
        spawn_embedding_task(args, config_arc);
    }

    if result.is_ok() {
        maybe_slide_quiet_hours_tz(args);
    }

    result
}

/// If a `write_memory` payload mentions a common timezone AND the user hasn't
/// customized their quiet-hour times, slide `user.timezone` to match. The
/// "still on default times" check is what tells us the user has not yet taken
/// manual control of quiet hours.
///
/// Fire-and-forget. Errors log and continue — never abort the tool.
fn maybe_slide_quiet_hours_tz(args: &serde_json::Value) {
    let content = args["content"].as_str().unwrap_or_default().to_string();
    if content.is_empty() {
        return;
    }
    crate::agent::spawn_logged("tz_slide", async move {
        if let Err(e) = run_tz_slide(&content) {
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
///
/// Splitting this out keeps the gating logic unit-testable without touching DB.
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
///
/// Two tiers: explicit IANA strings (e.g. `Europe/Berlin`) parsed against
/// `chrono_tz`, and a small whole-word-only abbreviation map for common ones.
/// Abbreviations are case-sensitive (uppercase only) so that occurrences inside
/// identifiers, code, or paths don't trigger.
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
        // UTC / GMT intentionally omitted: too generic; that's the state
        // we're trying to leave, not the one we want to slide back to.
    ];
    for (abbr, tz) in ABBREVS {
        if contains_whole_word(text, abbr) {
            hits.insert((*tz).to_string());
        }
    }

    if hits.len() == 1 {
        hits.into_iter().next()
    } else {
        None // 0 → no match; >1 → ambiguous, refuse to guess
    }
}

/// Whole-word, case-sensitive substring check. Matches only when surrounded by
/// non-alphanumeric/underscore characters (or string boundaries).
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

/// Spawn a background task to generate embeddings for a memory file write.
///
/// Reads the written entry from the `memory_entries` DB table (not the filesystem)
/// so that indexed content matches what `memory_search` will find. Entries that
/// are missing — e.g. because the row was deleted between write and embed — are
/// skipped rather than embedding a placeholder string.
fn spawn_embedding_task(args: &serde_json::Value, config_arc: &Arc<Config>) {
    let config = Arc::clone(config_arc);
    let raw_filename = args["filename"].as_str().unwrap_or_default().to_string();
    let scope = args["scope"].as_str().unwrap_or("global").to_string();
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

    // Use entry_name (no `.md`) as the filename for embedding tables so that
    // memory_chunks/memory_embeddings rows align with memory_entries.name.
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

const MULTI_AGENT_DISABLED: &str = "Error: Multi-agent system is not enabled.";

/// Returned for `send_to_agent`, which is not implemented in this build.
/// Falling through with a structured tool-output string keeps the agent
/// loop alive (vs. an `Err` that aborts the turn) so the model can pick
/// a different tool. Any future implementation must remove this constant.
const SEND_TO_AGENT_UNSUPPORTED: &str =
    "Error: send_to_agent is unavailable in this build. Use spawn_agent + wait_for_agent to coordinate work.";

/// Dispatch a multi-agent tool call. Returns `None` if the tool name is not
/// a multi-agent tool, allowing the caller to fall through to the unknown-tool case.
pub(crate) async fn try_handle_multi_agent_tool(
    name: &str,
    args: &serde_json::Value,
    agent_control: &mut Option<crate::multi_agent::AgentControl>,
    config: &Config,
    history: &[crate::types::Message],
) -> Option<Result<String>> {
    Some(match name {
        tn::SPAWN_AGENT => {
            if let Some(ref mut ctrl) = agent_control {
                let hist = if args["fork_context"].as_bool().unwrap_or(false) {
                    Some(history)
                } else {
                    None
                };
                crate::multi_agent::tools::handle_spawn_agent(args, ctrl, config, hist).await
            } else {
                Ok(MULTI_AGENT_DISABLED.to_string())
            }
        }
        tn::SEND_TO_AGENT => Ok(SEND_TO_AGENT_UNSUPPORTED.to_string()),
        tn::WAIT_FOR_AGENT => {
            if let Some(ref mut ctrl) = agent_control {
                crate::multi_agent::tools::handle_wait_for_agent(args, ctrl).await
            } else {
                Ok(MULTI_AGENT_DISABLED.to_string())
            }
        }
        tn::LIST_AGENTS => {
            if let Some(ref ctrl) = agent_control {
                crate::multi_agent::tools::handle_list_agents(ctrl)
            } else {
                Ok(MULTI_AGENT_DISABLED.to_string())
            }
        }
        tn::CLOSE_AGENT => {
            if let Some(ref mut ctrl) = agent_control {
                crate::multi_agent::tools::handle_close_agent(args, ctrl)
            } else {
                Ok(MULTI_AGENT_DISABLED.to_string())
            }
        }
        tn::MANAGE_ROLES => crate::multi_agent::tools::handle_manage_roles(args),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn dispatch_returns_none_for_non_multi_agent_tool() {
        let mut ctrl: Option<crate::multi_agent::AgentControl> = None;
        let config = Config::default();
        let result = try_handle_multi_agent_tool(
            "not_a_multi_agent_tool",
            &json!({}),
            &mut ctrl,
            &config,
            &[],
        )
        .await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn spawn_agent_without_control_reports_disabled() {
        let mut ctrl: Option<crate::multi_agent::AgentControl> = None;
        let config = Config::default();
        let out = try_handle_multi_agent_tool("spawn_agent", &json!({}), &mut ctrl, &config, &[])
            .await
            .expect("multi-agent tool matched")
            .expect("returns Ok");
        assert!(out.contains("not enabled"), "got: {out}");
    }

    #[tokio::test]
    async fn wait_for_agent_without_control_reports_disabled() {
        let mut ctrl: Option<crate::multi_agent::AgentControl> = None;
        let config = Config::default();
        let out =
            try_handle_multi_agent_tool("wait_for_agent", &json!({}), &mut ctrl, &config, &[])
                .await
                .expect("multi-agent tool matched")
                .expect("returns Ok");
        assert!(out.contains("not enabled"), "got: {out}");
    }

    #[tokio::test]
    async fn list_agents_without_control_reports_disabled() {
        let mut ctrl: Option<crate::multi_agent::AgentControl> = None;
        let config = Config::default();
        let out = try_handle_multi_agent_tool("list_agents", &json!({}), &mut ctrl, &config, &[])
            .await
            .expect("multi-agent tool matched")
            .expect("returns Ok");
        assert!(out.contains("not enabled"), "got: {out}");
    }

    #[tokio::test]
    async fn close_agent_without_control_reports_disabled() {
        let mut ctrl: Option<crate::multi_agent::AgentControl> = None;
        let config = Config::default();
        let out = try_handle_multi_agent_tool(
            "close_agent",
            &json!({"agent_id": "x"}),
            &mut ctrl,
            &config,
            &[],
        )
        .await
        .expect("multi-agent tool matched")
        .expect("returns Ok");
        assert!(out.contains("not enabled"), "got: {out}");
    }

    #[tokio::test]
    async fn send_to_agent_returns_unsupported_string_not_panic() {
        // Real failure mode: returning `Err` would abort the agent turn;
        // panicking would crash the daemon. Either is worse than a tool
        // result that lets the model pick a different action.
        let mut ctrl: Option<crate::multi_agent::AgentControl> = None;
        let config = Config::default();
        let out = try_handle_multi_agent_tool(
            "send_to_agent",
            &json!({"agent_id": "x", "message": "y"}),
            &mut ctrl,
            &config,
            &[],
        )
        .await
        .expect("multi-agent tool matched")
        .expect("dispatch returns Ok with a structured tool-output string");
        assert!(
            out.starts_with("Error:") && out.contains("send_to_agent"),
            "got: {out}"
        );
    }

    #[tokio::test]
    async fn manage_roles_dispatches_without_control() {
        let mut ctrl: Option<crate::multi_agent::AgentControl> = None;
        let config = Config::default();
        // manage_roles doesn't need AgentControl — "list" should succeed.
        let out = try_handle_multi_agent_tool(
            "manage_roles",
            &json!({"action": "list"}),
            &mut ctrl,
            &config,
            &[],
        )
        .await
        .expect("multi-agent tool matched")
        .expect("list succeeds without control");
        // Output is JSON (array of roles, possibly empty).
        assert!(out.starts_with('[') || out.contains("error"), "got: {out}");
    }

    #[test]
    fn extract_timezone_mention_table() {
        let cases: &[(&str, Option<&str>)] = &[
            ("user is in PST", Some("America/Los_Angeles")),
            ("based in Europe/Berlin", Some("Europe/Berlin")),
            (
                "located in America/Los_Angeles right now",
                Some("America/Los_Angeles"),
            ),
            ("EST and PST mentioned", None),     // ambiguous → None
            ("est is the worst", None),          // case-sensitive guard
            ("running in UTC datacenter", None), // generic skipped
            ("GMT only here", None),             // generic skipped
            ("no tz here at all", None),
            ("PSTDouble", None),     // not a whole word
            ("greet_PST_var", None), // identifier-internal
            ("Switching to JST next month", Some("Asia/Tokyo")),
        ];
        for (input, expected) in cases {
            let got = extract_timezone_mention(input);
            assert_eq!(
                got.as_deref(),
                *expected,
                "input={input:?} expected={expected:?} got={got:?}"
            );
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
    fn decide_tz_slide_writes_when_times_default_and_tz_unset() {
        let out = decide_tz_slide("America/Los_Angeles", Some("00:00"), Some("06:00"), None);
        assert_eq!(out.as_deref(), Some("America/Los_Angeles"));
    }

    #[test]
    fn decide_tz_slide_noop_when_user_changed_start_time() {
        let out = decide_tz_slide(
            "America/Los_Angeles",
            Some("22:00"), // user customized start
            Some("06:00"),
            Some("UTC"),
        );
        assert_eq!(out, None);
    }

    #[test]
    fn decide_tz_slide_noop_when_user_changed_end_time() {
        let out = decide_tz_slide(
            "America/Los_Angeles",
            Some("00:00"),
            Some("07:30"), // user customized end
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

    #[test]
    fn write_memory_with_effects_invalid_args_propagates_error() {
        // No filename/content → handle_write_memory returns Err. Side effects
        // (identity cache invalidation, embedding spawn) should be skipped.
        let config = Config::default();
        let config_arc = std::sync::Arc::new(Config::default());
        let mut cached: Option<String> = Some("stale identity".to_string());
        let args = serde_json::json!({});
        let result = handle_write_memory_with_effects(&args, &config, &config_arc, &mut cached);
        assert!(result.is_err());
        // Cache should be untouched because write failed.
        assert_eq!(cached.as_deref(), Some("stale identity"));
    }
}
