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

/// Fixed UUID for the nightly memory consolidation task (seeded in the schema).
pub const NIGHTLY_CONSOLIDATION_TASK_ID: &str = "00000000-0000-4000-8000-c005011d0001";

/// Fixed UUID for the weekly memory maintenance task (seeded in the schema).
pub const WEEKLY_CONSOLIDATION_TASK_ID: &str = "00000000-0000-4000-8000-c005011d0002";

/// Settings key for tracking the last successful nightly consolidation run.
pub const SETTING_LAST_NIGHTLY: &str = "consolidation.last_nightly";

/// Settings key for tracking the last successful weekly consolidation run.
pub const SETTING_LAST_WEEKLY: &str = "consolidation.last_weekly";

const CONSOLIDATION_TOOLS: &[&str] = &["memory_search", "read_memory", "write_memory"];
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
pub async fn run_dream(config: &crate::config::Config) -> Result<DreamReport> {
    let db = crate::db::Database::open()?;
    let nightly = run_pass(config, ConsolidationPass::Nightly).await?;
    let weekly = run_pass(config, ConsolidationPass::Weekly).await?;
    let now = chrono::Utc::now().timestamp().to_string();

    db.set_setting(SETTING_LAST_NIGHTLY, &now)?;
    db.set_setting(SETTING_LAST_WEEKLY, &now)?;

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
    let db = crate::db::Database::open()?;
    let execution = run_pass(config, pass).await?;
    db.set_setting(
        pass.settings_key(),
        &chrono::Utc::now().timestamp().to_string(),
    )?;

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
    )?;
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
pub fn flush_short_term_to_daily(facts_text: &str) -> Result<()> {
    if facts_text.trim().is_empty() {
        return Ok(());
    }
    let db = crate::db::Database::open()?;
    flush_short_term_to_daily_with_db(&db, facts_text)
}

/// Flush using the caller's existing DB handle when one is available,
/// otherwise open a fresh connection. Intended as the single entry point from
/// `Agent::flush_short_term_memory` so session-end flushes reuse the agent's
/// already-open database instead of racing a new connection against any
/// in-progress writes on the agent's handle.
pub fn flush_short_term_to_daily_with_optional_db(
    db: Option<&crate::db::Database>,
    facts_text: &str,
) -> Result<()> {
    if facts_text.trim().is_empty() {
        return Ok(());
    }
    match db {
        Some(db) => flush_short_term_to_daily_with_db(db, facts_text),
        None => flush_short_term_to_daily(facts_text),
    }
}

/// Same as [`flush_short_term_to_daily`] but uses a caller-provided database
/// handle. Split out primarily so tests can run against an in-memory DB.
pub fn flush_short_term_to_daily_with_db(db: &crate::db::Database, facts_text: &str) -> Result<()> {
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

    db.append_memory_entry("global", &name, &content)?;
    tracing::debug!("Flushed short-term memory to {name}");
    Ok(())
}

#[cfg(test)]
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
        let db = crate::db::Database::test_db();
        assert!(flush_short_term_to_daily_with_optional_db(Some(&db), "").is_ok());
        assert!(flush_short_term_to_daily_with_optional_db(Some(&db), "   \n").is_ok());
        assert!(flush_short_term_to_daily_with_optional_db(None, "").is_ok());
        let date = chrono::Local::now().format("%Y-%m-%d").to_string();
        assert!(db
            .get_memory_entry("global", &format!("daily/{date}"))
            .unwrap()
            .is_none());
    }

    #[test]
    fn flush_short_term_memory_integration_with_facts_as_text() {
        use crate::short_term_memory::{FactCategory, ShortTermMemory};

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
