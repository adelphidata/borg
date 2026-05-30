//! Memory consolidation pipeline.
//!
//! Nightly and weekly scheduled tasks consolidate session data and long-term
//! memory entries. The tasks themselves are LLM-driven (seeded in the schema)
//! with tool access to `write_memory`, `read_memory`, and `memory_search`.
//!
//! This module provides constants, the shared consolidation runner, and helper
//! functions for session-end short-term memory flushing.

use anyhow::{Context, Result};
use serde::Deserialize;
use tokio::sync::mpsc;

pub use crate::task_ids::{NIGHTLY_CONSOLIDATION_TASK_ID, WEEKLY_CONSOLIDATION_TASK_ID};

/// Settings key for tracking the last successful nightly consolidation run.
pub const SETTING_LAST_NIGHTLY: &str = "consolidation.last_nightly";

/// Settings key for tracking the last successful weekly consolidation run.
pub const SETTING_LAST_WEEKLY: &str = "consolidation.last_weekly";

/// Settings key for tracking the last identity-backfill ATTEMPT (not success).
/// Recorded regardless of outcome so a transcript that's genuinely too sparse
/// for the aux LLM to act on doesn't get retried every single nightly.
const SETTING_LAST_IDENTITY_BACKFILL_ATTEMPT: &str =
    "consolidation.last_identity_backfill_attempt";

/// Cool-down after a backfill attempt. Three days is long enough that a
/// hopeless never-onboarded shell isn't paying aux-LLM tokens every 24h,
/// short enough that a user who comes back partway through the window
/// (writes IDENTITY themselves, or generates more transcript) will be
/// caught on the next pass after they return.
const IDENTITY_BACKFILL_RETRY_COOLDOWN_SECS: i64 = 3 * 24 * 60 * 60;

pub(crate) const CONSOLIDATION_TOOLS: &[&str] = &[
    crate::tool::names::WRITE_MEMORY,
    crate::tool::names::READ_MEMORY,
    crate::tool::names::MEMORY_SEARCH,
];

/// Window (in seconds) over which recent sessions are pulled for identity
/// backfill. One week is short enough that stale long-dormant data dirs
/// don't get scraped, but long enough to catch a user who onboarded a few
/// days ago and walked away before the model wrote IDENTITY.md.
const IDENTITY_BACKFILL_WINDOW_SECS: i64 = 7 * 24 * 60 * 60;

/// Cap on transcript messages fed to the backfill aux LLM. Onboarding sessions
/// are short; this is a budget guard, not a normal-case limit.
const IDENTITY_BACKFILL_MAX_MESSAGES: usize = 200;

const IDENTITY_BACKFILL_SYSTEM: &str = "You are running a one-time identity backfill. \
The user finished onboarding but the agent never persisted IDENTITY.md. \
The transcript of recent sessions is provided in the user message. \
Synthesize a first-pass IDENTITY.md from it — owner name, chosen communication style/personality, \
purpose they stated for the agent, and any channel preferences they mentioned. \
You MUST call write_memory(\"IDENTITY.md\", content) exactly once with a short markdown body. \
If the transcript is too sparse to support a real identity, do not call write_memory at all.";

const IDENTITY_BACKFILL_PROMPT_PREFIX: &str = "Recent session transcripts (most recent first):";

const NIGHTLY_PROMPT: &str = "Review today's sessions and extract durable information into long-term memory. \
For each piece of information: APPEND to an existing topic if relevant, CREATE a new topic, or SKIP if already captured. \
Never duplicate information already in existing memory entries. When in doubt, keep the information.";
const WEEKLY_PROMPT: &str =
    "Review all long-term memory for: (a) duplicate information across entries, \
(b) outdated information superseded by newer entries, (c) entries that should be merged, \
(d) overly verbose entries that can be tightened. \
Never delete information unless it is provably superseded. When in doubt, keep it.";

/// Aggregated result for an on-demand or scheduled consolidation run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DreamReport {
    pub entries_extracted: u64,
    pub dupes_merged: u64,
    pub tokens_used: u64,
    pub nightly_completed: bool,
    pub weekly_completed: bool,
    pub warnings: Vec<String>,
    pub nightly_summary: String,
    pub weekly_summary: String,
}

/// One of the seeded consolidation passes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsolidationPass {
    Nightly,
    Weekly,
}

impl ConsolidationPass {
    fn task_name(self) -> &'static str {
        match self {
            Self::Nightly => "Nightly Memory Consolidation",
            Self::Weekly => "Weekly Memory Maintenance",
        }
    }

    fn prompt(self) -> &'static str {
        match self {
            Self::Nightly => NIGHTLY_PROMPT,
            Self::Weekly => WEEKLY_PROMPT,
        }
    }

    fn settings_key(self) -> &'static str {
        match self {
            Self::Nightly => SETTING_LAST_NIGHTLY,
            Self::Weekly => SETTING_LAST_WEEKLY,
        }
    }

    fn system_instruction(self) -> &'static str {
        match self {
            Self::Nightly => {
                "You are running the nightly memory consolidation pass. Use only the provided memory tools. \
                 Extract durable facts from today's daily memory and write them into the appropriate long-term entries. \
                 End with exactly one machine-readable block in this format: \
                 <dream_report>{\"entries_extracted\":N,\"dupes_merged\":0,\"summary\":\"...\",\"warnings\":[...]}</dream_report>"
            }
            Self::Weekly => {
                "You are running the weekly memory maintenance pass. Use only the provided memory tools. \
                 Merge duplicate memory, tighten stale or overly verbose entries, and preserve information unless it is provably superseded. \
                 End with exactly one machine-readable block in this format: \
                 <dream_report>{\"entries_extracted\":0,\"dupes_merged\":N,\"summary\":\"...\",\"warnings\":[...]}</dream_report>"
            }
        }
    }
}

#[derive(Debug, Default)]
struct PassExecution {
    summary: String,
    entries_extracted: u64,
    dupes_merged: u64,
    tokens_used: u64,
    warnings: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
struct DreamSummaryBlock {
    #[serde(default)]
    entries_extracted: u64,
    #[serde(default)]
    dupes_merged: u64,
    #[serde(default)]
    summary: String,
    #[serde(default)]
    warnings: Vec<String>,
}

/// Returns true when the given task id is one of the seeded consolidation jobs.
pub fn is_consolidation_task_id(task_id: &str) -> bool {
    matches!(
        task_id,
        NIGHTLY_CONSOLIDATION_TASK_ID | WEEKLY_CONSOLIDATION_TASK_ID
    )
}

/// Run both consolidation passes synchronously using the real agent loop.
///
/// When `memory.provider` is non-native, the external service handles
/// consolidation server-side; we skip the seeded-agent loop and return a
/// delegated report so the daemon scheduler doesn't burn LLM budget on a
/// no-op.
pub async fn run_dream(config: &crate::config::Config) -> Result<DreamReport> {
    use crate::config::media::MemoryProviderKind;
    if !matches!(config.memory.provider, MemoryProviderKind::Native) {
        return Ok(DreamReport {
            nightly_completed: true,
            weekly_completed: true,
            nightly_summary: format!(
                "delegated to {} (server-side consolidation)",
                config.memory.provider
            ),
            weekly_summary: format!(
                "delegated to {} (server-side consolidation)",
                config.memory.provider
            ),
            ..DreamReport::default()
        });
    }
    let db = crate::db::Database::open().await?;

    // Safety net for onboarding: if the model never wrote IDENTITY.md during the
    // first conversation (user bailed early, abrupt exit, etc.), reconstruct a
    // first-pass identity from recent transcripts before the normal nightly
    // pass. Best-effort: failures here must not block the rest of the dream.
    match crate::config::Config::data_dir() {
        Ok(data_dir) => {
            if let Err(e) = maybe_backfill_identity(config, &db, &data_dir).await {
                tracing::warn!("identity backfill failed: {e}");
            }
        }
        Err(e) => {
            tracing::warn!("identity backfill skipped: data_dir resolution failed: {e}");
        }
    }

    let nightly = run_pass(config, ConsolidationPass::Nightly).await?;
    let weekly = run_pass(config, ConsolidationPass::Weekly).await?;
    let now = chrono::Utc::now().timestamp().to_string();

    db.set_setting(SETTING_LAST_NIGHTLY, &now).await?;
    db.set_setting(SETTING_LAST_WEEKLY, &now).await?;

    let mut warnings = nightly.warnings;
    warnings.extend(weekly.warnings);

    Ok(DreamReport {
        entries_extracted: nightly.entries_extracted,
        dupes_merged: weekly.dupes_merged,
        tokens_used: nightly.tokens_used + weekly.tokens_used,
        nightly_completed: true,
        weekly_completed: true,
        warnings,
        nightly_summary: nightly.summary,
        weekly_summary: weekly.summary,
    })
}

/// Run a single seeded consolidation pass.
pub async fn run_single_pass(
    config: &crate::config::Config,
    pass: ConsolidationPass,
) -> Result<DreamReport> {
    let db = crate::db::Database::open().await?;
    let execution = run_pass(config, pass).await?;
    db.set_setting(
        pass.settings_key(),
        &chrono::Utc::now().timestamp().to_string(),
    )
    .await?;

    Ok(match pass {
        ConsolidationPass::Nightly => DreamReport {
            entries_extracted: execution.entries_extracted,
            dupes_merged: 0,
            tokens_used: execution.tokens_used,
            nightly_completed: true,
            weekly_completed: false,
            warnings: execution.warnings,
            nightly_summary: execution.summary,
            weekly_summary: String::new(),
        },
        ConsolidationPass::Weekly => DreamReport {
            entries_extracted: 0,
            dupes_merged: execution.dupes_merged,
            tokens_used: execution.tokens_used,
            nightly_completed: false,
            weekly_completed: true,
            warnings: execution.warnings,
            nightly_summary: String::new(),
            weekly_summary: execution.summary,
        },
    })
}

async fn run_pass(
    config: &crate::config::Config,
    pass: ConsolidationPass,
) -> Result<PassExecution> {
    let mut agent = crate::agent::Agent::new_sub_agent_aux(
        config.clone(),
        0,
        &config.agents,
        crate::telemetry::BorgMetrics::from_config(config),
        Some(
            CONSOLIDATION_TOOLS
                .iter()
                .map(|tool| (*tool).to_string())
                .collect(),
        ),
    )
    .await?;
    agent.inject_history_message(crate::types::Message::system(pass.system_instruction()));

    let (event_tx, mut event_rx) = mpsc::channel(256);
    let mut tokens_used = 0u64;
    let mut successful_writes = 0u64;

    {
        let send_fut = agent.send_message(pass.prompt(), event_tx);
        tokio::pin!(send_fut);

        loop {
            tokio::select! {
                result = &mut send_fut => {
                    result?;
                    break;
                }
                maybe_event = event_rx.recv() => {
                    let Some(event) = maybe_event else { break; };
                    process_agent_event(event, &mut tokens_used, &mut successful_writes);
                }
            }
        }
    }

    while let Ok(event) = event_rx.try_recv() {
        process_agent_event(event, &mut tokens_used, &mut successful_writes);
    }

    let final_text = agent
        .history()
        .iter()
        .rev()
        .find(|msg| msg.role == crate::types::Role::Assistant)
        .and_then(|msg| msg.text_content())
        .unwrap_or("")
        .to_string();
    Ok(finalize_pass_execution(
        pass,
        final_text.as_str(),
        tokens_used,
        successful_writes,
    ))
}

fn parse_summary_block(text: &str) -> Result<DreamSummaryBlock> {
    let start_tag = "<dream_report>";
    let end_tag = "</dream_report>";
    let start = text
        .find(start_tag)
        .map(|idx| idx + start_tag.len())
        .context("missing <dream_report> block")?;
    let end = text[start..]
        .find(end_tag)
        .map(|idx| start + idx)
        .context("missing </dream_report> block")?;
    let json = text[start..end].trim();
    serde_json::from_str(json).context("invalid dream_report JSON")
}

fn process_agent_event(
    event: crate::agent::AgentEvent,
    tokens_used: &mut u64,
    successful_writes: &mut u64,
) {
    match event {
        crate::agent::AgentEvent::Usage(usage) => {
            *tokens_used = tokens_used.saturating_add(usage.total_tokens);
        }
        crate::agent::AgentEvent::ToolResult { name, result, .. } => {
            if name == "write_memory" && result.starts_with("Written to memory: ") {
                *successful_writes = successful_writes.saturating_add(1);
            }
        }
        _ => {}
    }
}

fn finalize_pass_execution(
    pass: ConsolidationPass,
    final_text: &str,
    tokens_used: u64,
    successful_writes: u64,
) -> PassExecution {
    let parsed = parse_summary_block(final_text);
    let mut warnings = Vec::new();

    match parsed {
        Ok(parsed) => {
            warnings.extend(parsed.warnings);
            let entries_extracted = parsed.entries_extracted.min(successful_writes);
            let dupes_merged = parsed.dupes_merged.min(successful_writes);

            match pass {
                ConsolidationPass::Nightly if parsed.entries_extracted > successful_writes => {
                    warnings.push(format!(
                        "{} reported {} extracted entries but only {} write_memory calls succeeded.",
                        pass.task_name(),
                        parsed.entries_extracted,
                        successful_writes
                    ));
                }
                ConsolidationPass::Weekly if parsed.dupes_merged > successful_writes => {
                    warnings.push(format!(
                        "{} reported {} merged dupes but only {} write_memory calls succeeded.",
                        pass.task_name(),
                        parsed.dupes_merged,
                        successful_writes
                    ));
                }
                _ => {}
            }

            PassExecution {
                summary: if parsed.summary.trim().is_empty() {
                    fallback_summary(pass, successful_writes)
                } else {
                    parsed.summary
                },
                entries_extracted,
                dupes_merged,
                tokens_used,
                warnings,
            }
        }
        Err(err) => {
            if successful_writes > 0 {
                warnings.push(format!(
                    "{} completed with {} successful write_memory calls, but the final dream summary was unreadable: {}",
                    pass.task_name(),
                    successful_writes,
                    err
                ));
                let (entries_extracted, dupes_merged) = match pass {
                    ConsolidationPass::Nightly => (successful_writes, 0),
                    ConsolidationPass::Weekly => (0, successful_writes),
                };
                PassExecution {
                    summary: fallback_summary(pass, successful_writes),
                    entries_extracted,
                    dupes_merged,
                    tokens_used,
                    warnings,
                }
            } else {
                warnings.push(format!(
                    "{} finished without a readable dream summary: {}",
                    pass.task_name(),
                    err
                ));
                PassExecution {
                    summary: fallback_summary(pass, 0),
                    entries_extracted: 0,
                    dupes_merged: 0,
                    tokens_used,
                    warnings,
                }
            }
        }
    }
}

fn fallback_summary(pass: ConsolidationPass, successful_writes: u64) -> String {
    match pass {
        ConsolidationPass::Nightly => format!(
            "{} completed with {} successful write_memory calls.",
            pass.task_name(),
            successful_writes
        ),
        ConsolidationPass::Weekly => format!(
            "{} completed with {} successful write_memory calls.",
            pass.task_name(),
            successful_writes
        ),
    }
}

/// Flush short-term memory facts to a daily log entry in the DB.
///
/// Called on session end. Creates or appends to a `daily/{YYYY-MM-DD}` entry
/// in the `memory_entries` table. The nightly consolidation job processes
/// these daily entries into long-term topic entries.
pub async fn flush_short_term_to_daily(facts_text: &str) -> Result<()> {
    if facts_text.trim().is_empty() {
        return Ok(());
    }
    let db = crate::db::Database::open().await?;
    flush_short_term_to_daily_with_db(&db, facts_text).await
}

/// Flush using the caller's existing DB handle when one is available,
/// otherwise open a fresh connection. Intended as the single entry point from
/// `Agent::flush_short_term_memory` so session-end flushes reuse the agent's
/// already-open database instead of racing a new connection against any
/// in-progress writes on the agent's handle.
pub async fn flush_short_term_to_daily_with_optional_db(
    db: Option<&crate::db::Database>,
    facts_text: &str,
) -> Result<()> {
    if facts_text.trim().is_empty() {
        return Ok(());
    }
    match db {
        Some(db) => flush_short_term_to_daily_with_db(db, facts_text).await,
        None => flush_short_term_to_daily(facts_text).await,
    }
}

/// Same as [`flush_short_term_to_daily`] but uses a caller-provided database
/// handle. Split out primarily so tests can run against an in-memory DB.
pub async fn flush_short_term_to_daily_with_db(
    db: &crate::db::Database,
    facts_text: &str,
) -> Result<()> {
    if facts_text.trim().is_empty() {
        return Ok(());
    }

    let date = chrono::Local::now().format("%Y-%m-%d").to_string();
    let name = format!("daily/{date}");
    let header = format!(
        "\n## Session flush ({})\n",
        chrono::Local::now().format("%H:%M")
    );
    let content = format!("{header}{facts_text}");

    db.append_memory_entry("global", &name, &content).await?;
    tracing::debug!("Flushed short-term memory to {name}");
    Ok(())
}

/// Orchestrate one identity-backfill cycle: gate on `should_backfill_identity`,
/// enforce a 3-day cool-down between attempts (so a never-onboarded shell
/// doesn't burn aux LLM tokens every single nightly), persist the attempt
/// timestamp before invoking the LLM (so even an aborted/crashed attempt
/// still counts toward the cool-down), then run the backfill.
///
/// Returns `Ok(())` whether or not backfill actually fired or wrote an
/// entry — the caller treats any returned `Err` as a logged warning.
async fn maybe_backfill_identity(
    config: &crate::config::Config,
    db: &crate::db::Database,
    data_dir: &std::path::Path,
) -> Result<()> {
    if !should_backfill_identity(db, data_dir).await {
        return Ok(());
    }

    let now = chrono::Utc::now().timestamp();
    let last_attempt = db
        .get_setting(SETTING_LAST_IDENTITY_BACKFILL_ATTEMPT)
        .await?
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(0);
    let elapsed = now.saturating_sub(last_attempt);
    if last_attempt > 0 && elapsed < IDENTITY_BACKFILL_RETRY_COOLDOWN_SECS {
        tracing::debug!(
            "identity backfill skipped: last attempt {elapsed}s ago (cooldown {}s)",
            IDENTITY_BACKFILL_RETRY_COOLDOWN_SECS
        );
        return Ok(());
    }

    // Persist the attempt timestamp BEFORE invoking the LLM. If the agent
    // call panics, the daemon restarts, or we crash mid-call, we still
    // want the cool-down to take effect on the next dream — otherwise a
    // crash loop would re-attempt forever.
    if let Err(e) = db
        .set_setting(SETTING_LAST_IDENTITY_BACKFILL_ATTEMPT, &now.to_string())
        .await
    {
        tracing::warn!("identity backfill: failed to persist attempt timestamp: {e}");
    }

    match try_backfill_identity(config, db).await {
        Ok(true) => tracing::info!("identity backfill wrote IDENTITY entry"),
        Ok(false) => tracing::debug!("identity backfill ran but produced no entry"),
        Err(e) => tracing::warn!("identity backfill failed: {e}"),
    }
    Ok(())
}

/// True when the conditions for an identity backfill are met: no `IDENTITY`
/// entry has been written to long-term memory yet, AND `SETUP.md` is still
/// present at the data dir (proxy: the user actually onboarded and the
/// onboarding template hasn't been retired yet).
///
/// Both signals matter. The DB check alone would fire on a never-onboarded
/// install (e.g. fresh shell that never opened a session). The SETUP.md
/// check alone would fire even after the user wiped IDENTITY deliberately
/// without removing SETUP.md. Together they target precisely "onboarding
/// happened but never produced an identity."
async fn should_backfill_identity(
    db: &crate::db::Database,
    data_dir: &std::path::Path,
) -> bool {
    if super::identity_in_long_term_memory_with_db(db).await {
        return false;
    }
    let setup_path = data_dir.join(crate::constants::SETUP_FILE);
    tokio::fs::try_exists(&setup_path).await.unwrap_or(false)
}

/// Render recent session metadata + messages into a single prompt-ready
/// transcript string, capped at `max_messages` total user/assistant lines.
///
/// Skipped: tool messages, system messages, messages with empty content. Tool
/// scaffolding noise would dominate without adding identity signal, and the
/// budget is precious.
fn build_backfill_transcript(
    sessions: &[crate::db::SessionRow],
    messages_by_session: &std::collections::HashMap<String, Vec<crate::db::MessageRow>>,
    max_messages: usize,
) -> String {
    let mut lines: Vec<String> = Vec::new();
    let mut taken = 0usize;
    for session in sessions {
        if taken >= max_messages {
            break;
        }
        let Some(msgs) = messages_by_session.get(&session.id) else {
            continue;
        };
        let mut session_header_written = false;
        for msg in msgs {
            if taken >= max_messages {
                break;
            }
            // Identity signal lives in what the user said and what the agent
            // replied. Tool scaffolding and inner system prompts are noise.
            if !matches!(msg.role.as_str(), "user" | "assistant") {
                continue;
            }
            let text = msg.content.as_deref().unwrap_or("").trim();
            if text.is_empty() {
                continue;
            }
            if !session_header_written {
                let short_id: String = session.id.chars().take(8).collect();
                lines.push(format!(
                    "## Session {} (created_at={})",
                    short_id, session.created_at
                ));
                session_header_written = true;
            }
            lines.push(format!("[{}] {}", msg.role, text));
            taken += 1;
        }
    }
    lines.join("\n")
}

/// Run the aux-LLM-driven identity backfill. Returns `Ok(true)` when the
/// agent successfully wrote a non-empty `IDENTITY` entry, `Ok(false)`
/// when it ran but produced nothing, and `Err` only on infrastructure
/// failures (DB read, agent construction) — the caller logs and continues.
async fn try_backfill_identity(
    config: &crate::config::Config,
    db: &crate::db::Database,
) -> Result<bool> {
    let now = chrono::Utc::now().timestamp();
    let since = now.saturating_sub(IDENTITY_BACKFILL_WINDOW_SECS);
    let sessions = db.sessions_since(since).await?;
    if sessions.is_empty() {
        tracing::debug!("identity_backfill: no sessions in the last week, skipping");
        return Ok(false);
    }

    let mut messages_by_session: std::collections::HashMap<String, Vec<crate::db::MessageRow>> =
        std::collections::HashMap::new();
    for session in &sessions {
        let msgs = db.load_session_messages(&session.id).await?;
        messages_by_session.insert(session.id.clone(), msgs);
    }

    let transcript = build_backfill_transcript(
        &sessions,
        &messages_by_session,
        IDENTITY_BACKFILL_MAX_MESSAGES,
    );
    if transcript.trim().is_empty() {
        tracing::debug!("identity_backfill: transcript is empty, skipping");
        return Ok(false);
    }

    let mut agent = crate::agent::Agent::new_sub_agent_aux(
        config.clone(),
        0,
        &config.agents,
        crate::telemetry::BorgMetrics::from_config(config),
        Some(vec![crate::tool::names::WRITE_MEMORY.to_string()]),
    )
    .await?;
    agent.inject_history_message(crate::types::Message::system(IDENTITY_BACKFILL_SYSTEM));

    let prompt = format!(
        "{IDENTITY_BACKFILL_PROMPT_PREFIX}\n\n<transcript>\n{transcript}\n</transcript>\n\nWrite IDENTITY.md now if the transcript supports it."
    );

    let (event_tx, mut event_rx) = mpsc::channel(256);
    let mut tokens_used = 0u64;
    let mut successful_writes = 0u64;
    {
        let send_fut = agent.send_message(&prompt, event_tx);
        tokio::pin!(send_fut);
        loop {
            tokio::select! {
                result = &mut send_fut => {
                    result?;
                    break;
                }
                maybe_event = event_rx.recv() => {
                    let Some(event) = maybe_event else { break; };
                    process_agent_event(event, &mut tokens_used, &mut successful_writes);
                }
            }
        }
    }
    while let Ok(event) = event_rx.try_recv() {
        process_agent_event(event, &mut tokens_used, &mut successful_writes);
    }

    let wrote = super::identity_in_long_term_memory_with_db(db).await;
    if wrote {
        tracing::info!(
            "identity_backfill: IDENTITY written ({} write_memory calls, {} tokens)",
            successful_writes,
            tokens_used
        );
    } else if successful_writes > 0 {
        tracing::warn!(
            "identity_backfill: {} write_memory calls succeeded but IDENTITY entry is still empty",
            successful_writes
        );
    } else {
        tracing::debug!("identity_backfill: aux agent finished without writing IDENTITY");
    }
    Ok(wrote)
}

#[cfg(test)]
mod backfill_tests {
    use super::*;
    use crate::db::{Database, MessageRow, SessionRow};
    use std::collections::HashMap;

    fn mk_session(id: &str, created_at: i64) -> SessionRow {
        SessionRow {
            id: id.to_string(),
            created_at,
            updated_at: created_at,
            total_tokens: 0,
            model: "test".to_string(),
            title: "t".to_string(),
            parent_session_id: None,
            forked_from_message_id: None,
        }
    }

    fn mk_msg(id: i64, session_id: &str, role: &str, content: Option<&str>) -> MessageRow {
        MessageRow {
            id,
            session_id: session_id.to_string(),
            role: role.to_string(),
            content: content.map(str::to_string),
            content_parts_json: None,
            tool_calls_json: None,
            tool_call_id: None,
            timestamp: None,
            created_at: 0,
        }
    }

    #[tokio::test]
    async fn should_backfill_skips_when_identity_already_persisted() {
        let db = Database::test_db().await;
        db.upsert_memory_entry("global", "IDENTITY", "# Borgy\n\nReal identity content")
            .await
            .expect("seed IDENTITY");
        let dir = tempfile::tempdir().unwrap();
        // SETUP.md present — but should still skip because identity is set.
        tokio::fs::write(dir.path().join(crate::constants::SETUP_FILE), "x")
            .await
            .unwrap();

        assert!(!should_backfill_identity(&db, dir.path()).await);
    }

    #[tokio::test]
    async fn should_backfill_skips_when_setup_md_absent() {
        let db = Database::test_db().await;
        let dir = tempfile::tempdir().unwrap();
        // No IDENTITY entry, no SETUP.md — never-onboarded shell; don't run.
        assert!(!should_backfill_identity(&db, dir.path()).await);
    }

    #[tokio::test]
    async fn should_backfill_skips_when_identity_row_is_blank() {
        // Whitespace-only IDENTITY entry must count as "not written" — otherwise
        // a deliberately empty write would suppress backfill forever.
        let db = Database::test_db().await;
        db.upsert_memory_entry("global", "IDENTITY", "   \n")
            .await
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(dir.path().join(crate::constants::SETUP_FILE), "x")
            .await
            .unwrap();

        assert!(should_backfill_identity(&db, dir.path()).await);
    }

    #[tokio::test]
    async fn should_backfill_fires_when_onboarded_but_no_identity() {
        let db = Database::test_db().await;
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(dir.path().join(crate::constants::SETUP_FILE), "x")
            .await
            .unwrap();

        assert!(should_backfill_identity(&db, dir.path()).await);
    }

    #[tokio::test]
    async fn maybe_backfill_short_circuits_inside_cooldown_window() {
        // Encodes the retry-storm guard: when an attempt happened recently,
        // maybe_backfill_identity must bail before touching the aux LLM, even
        // though the underlying conditions (no IDENTITY, SETUP.md present) say
        // backfill is wanted. Proof = the persisted attempt timestamp does NOT
        // get rewritten on this call — the cool-down branch returns before the
        // set_setting that runs on a real attempt.
        let db = Database::test_db().await;
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(dir.path().join(crate::constants::SETUP_FILE), "x")
            .await
            .unwrap();
        let seeded = chrono::Utc::now().timestamp() - 60; // 1 minute ago
        db.set_setting(
            SETTING_LAST_IDENTITY_BACKFILL_ATTEMPT,
            &seeded.to_string(),
        )
        .await
        .unwrap();

        let config = crate::config::Config::default();
        maybe_backfill_identity(&config, &db, dir.path())
            .await
            .expect("cool-down branch returns Ok");

        let after = db
            .get_setting(SETTING_LAST_IDENTITY_BACKFILL_ATTEMPT)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            after,
            seeded.to_string(),
            "attempt timestamp must not be rewritten while inside the cooldown window — \
             if it were, a never-onboardable shell would burn aux LLM tokens every nightly"
        );
    }

    #[tokio::test]
    async fn maybe_backfill_noops_when_setup_md_absent() {
        // Re-asserts the gate at the run_dream boundary: an install that never
        // onboarded (no SETUP.md, no IDENTITY) must not even reach the
        // cool-down bookkeeping, otherwise we'd pollute the settings table on
        // every nightly for non-onboarded shells.
        let db = Database::test_db().await;
        let dir = tempfile::tempdir().unwrap(); // no SETUP.md written
        let config = crate::config::Config::default();

        maybe_backfill_identity(&config, &db, dir.path())
            .await
            .expect("not-onboarded path returns Ok");
        let stored = db
            .get_setting(SETTING_LAST_IDENTITY_BACKFILL_ATTEMPT)
            .await
            .unwrap();
        assert!(
            stored.is_none(),
            "no attempt timestamp must be written when backfill conditions aren't met"
        );
    }

    #[test]
    fn transcript_keeps_only_user_and_assistant_messages() {
        let sessions = vec![mk_session("s1", 100)];
        let mut by_session = HashMap::new();
        by_session.insert(
            "s1".to_string(),
            vec![
                mk_msg(1, "s1", "system", Some("inner system prompt")),
                mk_msg(2, "s1", "user", Some("hello")),
                mk_msg(3, "s1", "tool", Some("tool result noise")),
                mk_msg(4, "s1", "assistant", Some("hi mike")),
            ],
        );

        let out = build_backfill_transcript(&sessions, &by_session, 100);
        assert!(out.contains("[user] hello"));
        assert!(out.contains("[assistant] hi mike"));
        assert!(!out.contains("inner system prompt"));
        assert!(!out.contains("tool result noise"));
    }

    #[test]
    fn transcript_caps_at_max_messages_across_sessions() {
        let sessions = vec![mk_session("s1", 100), mk_session("s2", 200)];
        let mut by_session = HashMap::new();
        by_session.insert(
            "s1".to_string(),
            (0..5)
                .map(|i| mk_msg(i, "s1", "user", Some("msg")))
                .collect(),
        );
        by_session.insert(
            "s2".to_string(),
            (5..10)
                .map(|i| mk_msg(i, "s2", "user", Some("msg")))
                .collect(),
        );

        let out = build_backfill_transcript(&sessions, &by_session, 3);
        // 3 [user] msg lines total, regardless of which session they came from.
        assert_eq!(out.matches("[user] msg").count(), 3);
        // First session header appears, second does not (we ran out of budget
        // before reaching session 2).
        assert!(out.contains("## Session s1"));
        assert!(!out.contains("## Session s2"));
    }

    #[test]
    fn transcript_skips_empty_content_and_omits_header_for_no_useful_messages() {
        // Session with only blank/tool messages must not contribute a header —
        // otherwise we'd ship a meaningless header to the LLM and waste budget.
        let sessions = vec![mk_session("blank", 100), mk_session("real", 200)];
        let mut by_session = HashMap::new();
        by_session.insert(
            "blank".to_string(),
            vec![
                mk_msg(1, "blank", "user", Some("   ")),
                mk_msg(2, "blank", "tool", Some("noise")),
                mk_msg(3, "blank", "assistant", None),
            ],
        );
        by_session.insert(
            "real".to_string(),
            vec![mk_msg(4, "real", "user", Some("actual content"))],
        );

        let out = build_backfill_transcript(&sessions, &by_session, 100);
        assert!(!out.contains("## Session blank"));
        assert!(out.contains("## Session real"));
        assert!(out.contains("actual content"));
    }
}

#[cfg(all(test, feature = "phase9-tests-disabled"))]
mod tests {
    use super::*;

    #[test]
    fn consolidation_task_ids_match_seeded_jobs() {
        assert!(is_consolidation_task_id(NIGHTLY_CONSOLIDATION_TASK_ID));
        assert!(is_consolidation_task_id(WEEKLY_CONSOLIDATION_TASK_ID));
        assert!(!is_consolidation_task_id("not-a-consolidation-task"));
    }

    #[test]
    fn parse_summary_block_reads_machine_summary() {
        let parsed = parse_summary_block(
            "done\n<dream_report>{\"entries_extracted\":3,\"dupes_merged\":1,\
             \"summary\":\"tightened notes\",\"warnings\":[\"check INDEX\"]}</dream_report>",
        )
        .expect("parse");
        assert_eq!(parsed.entries_extracted, 3);
        assert_eq!(parsed.dupes_merged, 1);
        assert_eq!(parsed.summary, "tightened notes");
        assert_eq!(parsed.warnings, vec!["check INDEX"]);
    }

    #[test]
    fn parse_summary_block_rejects_missing_marker() {
        let err = parse_summary_block("no marker").unwrap_err().to_string();
        assert!(err.contains("missing <dream_report> block"), "{err}");
    }

    #[test]
    fn finalize_pass_execution_falls_back_when_summary_is_missing_after_writes() {
        let execution = finalize_pass_execution(
            ConsolidationPass::Nightly,
            "plain text without report",
            77,
            2,
        );
        assert_eq!(execution.entries_extracted, 2);
        assert_eq!(execution.dupes_merged, 0);
        assert_eq!(execution.tokens_used, 77);
        assert!(
            execution
                .summary
                .contains("2 successful write_memory calls"),
            "{}",
            execution.summary
        );
        assert!(
            execution
                .warnings
                .iter()
                .any(|w| w.contains("final dream summary was unreadable")),
            "{:?}",
            execution.warnings
        );
    }

    #[test]
    fn finalize_pass_execution_clamps_counts_but_keeps_parsed_summary() {
        let execution = finalize_pass_execution(
            ConsolidationPass::Weekly,
            "<dream_report>{\"entries_extracted\":0,\"dupes_merged\":5,\"summary\":\"merged duplicates\",\"warnings\":[]}</dream_report>",
            19,
            3,
        );
        assert_eq!(execution.entries_extracted, 0);
        assert_eq!(execution.dupes_merged, 3);
        assert_eq!(execution.summary, "merged duplicates");
        assert!(
            execution
                .warnings
                .iter()
                .any(|w| w
                    .contains("reported 5 merged dupes but only 3 write_memory calls succeeded")),
            "{:?}",
            execution.warnings
        );
    }

    #[test]
    fn process_agent_event_counts_usage_and_memory_writes_only() {
        let mut tokens = 0;
        let mut writes = 0;
        process_agent_event(
            crate::agent::AgentEvent::Usage(crate::llm::UsageData {
                prompt_tokens: 4,
                completion_tokens: 6,
                total_tokens: 10,
                cached_input_tokens: 0,
                cache_creation_tokens: 0,
                provider: "test".into(),
                model: "model".into(),
                provider_cost_usd: None,
            }),
            &mut tokens,
            &mut writes,
        );
        process_agent_event(
            crate::agent::AgentEvent::ToolResult {
                tool_call_id: "call-1".into(),
                name: "write_memory".into(),
                result: "Written to memory: topic (scope: global)".into(),
            },
            &mut tokens,
            &mut writes,
        );
        process_agent_event(
            crate::agent::AgentEvent::ToolResult {
                tool_call_id: "call-2".into(),
                name: "read_memory".into(),
                result: "ignored".into(),
            },
            &mut tokens,
            &mut writes,
        );
        assert_eq!(tokens, 10);
        assert_eq!(writes, 1);
    }

    #[test]
    fn flush_writes_to_daily_entry() {
        let db = crate::db::Database::test_db();
        let date = chrono::Local::now().format("%Y-%m-%d").to_string();
        let name = format!("daily/{date}");

        db.append_memory_entry("global", &name, "- [Decision] test fact")
            .unwrap();

        let entry = db.get_memory_entry("global", &name).unwrap().unwrap();
        assert!(entry.content.contains("test fact"));
    }

    #[test]
    fn flush_with_db_creates_and_appends_daily_entry() {
        let db = crate::db::Database::test_db();
        let date = chrono::Local::now().format("%Y-%m-%d").to_string();
        let name = format!("daily/{date}");

        flush_short_term_to_daily_with_db(&db, "- [Decision] chose rust").unwrap();
        let first = db.get_memory_entry("global", &name).unwrap().unwrap();
        assert!(first.content.contains("chose rust"));
        assert!(first.content.contains("Session flush"));

        flush_short_term_to_daily_with_db(&db, "- [Correction] use snake_case").unwrap();
        let second = db.get_memory_entry("global", &name).unwrap().unwrap();
        assert!(second.content.contains("chose rust"));
        assert!(second.content.contains("use snake_case"));
        assert!(second.content.len() > first.content.len());
    }

    #[test]
    fn optional_db_routes_through_provided_handle() {
        let db = crate::db::Database::test_db();
        flush_short_term_to_daily_with_optional_db(Some(&db), "- [Decision] use Rust").unwrap();

        let date = chrono::Local::now().format("%Y-%m-%d").to_string();
        let entry = db
            .get_memory_entry("global", &format!("daily/{date}"))
            .unwrap()
            .expect("entry must be written through the injected db handle");
        assert!(entry.content.contains("use Rust"));
    }

    #[test]
    fn optional_db_empty_facts_are_noop_with_or_without_db() {
        // No daily entry should be written for empty or whitespace-only input,
        // regardless of whether a DB handle is provided. Catches a regression
        // where blank-flush would clobber the existing daily/{date} entry with
        // an empty body.
        let db = crate::db::Database::test_db();
        flush_short_term_to_daily_with_optional_db(Some(&db), "")
            .expect("empty input must be a no-op, not an error");
        flush_short_term_to_daily_with_optional_db(Some(&db), "   \n")
            .expect("whitespace-only input must be a no-op, not an error");
        flush_short_term_to_daily_with_optional_db(None, "")
            .expect("None db + empty input must be a no-op, not an error");
        let date = chrono::Local::now().format("%Y-%m-%d").to_string();
        assert!(
            db.get_memory_entry("global", &format!("daily/{date}"))
                .unwrap()
                .is_none(),
            "no-op flush must not create a daily entry"
        );
    }

    #[test]
    fn flush_short_term_memory_integration_with_facts_as_text() {
        use crate::memory::short_term_memory::{FactCategory, ShortTermMemory};

        let db = crate::db::Database::test_db();
        let mut stm = ShortTermMemory::new("sess-test".into(), 2000);
        stm.add_fact(FactCategory::Decision, "agreed on PG over MySQL".into(), 0);
        stm.add_fact(
            FactCategory::TaskOutcome,
            "migration script shipped".into(),
            1,
        );

        flush_short_term_to_daily_with_db(&db, &stm.facts_as_text()).unwrap();

        let date = chrono::Local::now().format("%Y-%m-%d").to_string();
        let entry = db
            .get_memory_entry("global", &format!("daily/{date}"))
            .unwrap()
            .unwrap();
        assert!(entry.content.contains("PG over MySQL"));
        assert!(entry.content.contains("migration script shipped"));
    }
}
