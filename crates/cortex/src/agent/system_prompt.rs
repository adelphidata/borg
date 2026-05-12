//! System-prompt section builders.
//!
//! Extracted from `agent/mod.rs` so the per-section logic lives next to each
//! other and isn't interleaved with session-management, tool-execution, and
//! loop state. `build_system_prompt` itself remains in `mod.rs` because it
//! pulls together many caches and fallbacks and is more agent-orchestration
//! than prompt assembly.
//!
//! These are all methods on `Agent`; the trait-like surface is kept intact
//! through a second `impl Agent` block — Rust allows multiple impls of the
//! same type within a crate.

use crate::constants;

use super::{Agent, COLLAB_MODE_EXECUTE, COLLAB_MODE_PLAN};

impl Agent {
    /// Build the `<environment>` section with time, CWD, git context, OS, and runtime info.
    pub(super) async fn build_environment_section(&self) -> String {
        let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S %Z");
        let mut s = String::new();
        s.push_str("<environment>\n");
        s.push_str(&format!("Current Time: {now}\n"));
        s.push_str(&format!(
            "Working directory: {}\n",
            self.execution_context.workspace_cwd().display()
        ));
        if let Some(ref root) = self.git_repo_root {
            let git_ctx = crate::git::collect_git_context(root).await;
            let formatted = crate::git::format_git_context(&git_ctx);
            if !formatted.is_empty() {
                s.push_str(&formatted);
            }
        }
        // Runtime info line (model, provider, thinking, OS)
        let mut runtime_parts: Vec<String> = Vec::new();
        runtime_parts.push(format!(
            "os={} ({})",
            std::env::consts::OS,
            std::env::consts::ARCH
        ));
        if let Some(ref provider) = self.config.llm.provider {
            runtime_parts.push(format!("provider={provider}"));
        }
        if !self.config.llm.model.is_empty() {
            runtime_parts.push(format!("model={}", self.config.llm.model));
        }
        let thinking = if self.config.llm.thinking.is_enabled() {
            match self.config.llm.thinking {
                crate::config::ThinkingLevel::Low => "low",
                crate::config::ThinkingLevel::Medium => "medium",
                crate::config::ThinkingLevel::High => "high",
                crate::config::ThinkingLevel::Xhigh => "xhigh",
                crate::config::ThinkingLevel::Off => "off",
            }
        } else {
            "off"
        };
        runtime_parts.push(format!("thinking={thinking}"));
        if let Some(ref tz) = self.config.user.timezone {
            runtime_parts.push(format!("timezone={tz}"));
        }
        s.push_str(&format!("Runtime: {}\n", runtime_parts.join(" | ")));
        s.push_str("</environment>\n");
        s
    }

    /// Build the `<tooling>` section listing available tools with summaries.
    pub(super) fn build_tooling_section(&self) -> String {
        let tool_summaries: &[(&str, &str)] = &[
            ("write_memory", "Write/append long-term memory entries"),
            ("read_memory", "Read a long-term memory entry"),
            (
                "memory_search",
                "Search long-term memory, knowledge files, and sessions",
            ),
            ("list", "List resources (skills, channels, agents)"),
            ("apply_patch", "Create/update/delete files via patch DSL"),
            (
                "run_shell",
                "Execute shell commands (full system access, not sandboxed)",
            ),
            (
                "read_file",
                "Read file contents with line numbers, images, PDFs",
            ),
            ("list_dir", "List directory contents"),
            ("web_fetch", "Fetch URL content"),
            ("web_search", "Search the web"),
            ("browser", "Control headless Chrome browser"),
            (
                "schedule",
                "Manage scheduled jobs: prompt tasks and cron commands",
            ),
            (
                "request_user_input",
                "Last-resort prompt for information no other tool can obtain (personal preference, credential, external decision). Do not use for questions answerable by reading files or running commands.",
            ),
            ("generate_image", "Generate images from text descriptions"),
            ("text_to_speech", "Convert text to speech audio"),
            ("spawn_agent", "Spawn an isolated sub-agent"),
            ("send_to_agent", "Send message to a running sub-agent"),
            ("wait_for_agent", "Wait for a sub-agent to complete"),
            ("close_agent", "Close a running sub-agent"),
        ];

        // Build the full tool list including multi-agent tools when available
        let mut defs = crate::tool::definitions::core_tool_definitions(&self.config);
        if self.agent_control.is_some() {
            defs.extend(crate::multi_agent::tools::tool_definitions(
                self.spawn_depth,
                self.config.agents.max_spawn_depth,
            ));
        }
        let available: std::collections::HashSet<&str> =
            defs.iter().map(|d| d.function.name.as_str()).collect();

        let mut lines = vec![
            "## Tooling".to_string(),
            "Available tools (filtered by config):".to_string(),
        ];
        for &(name, summary) in tool_summaries {
            if available.contains(name) {
                lines.push(format!("- {name}: {summary}"));
            }
        }
        // Include any tools not in the static list (e.g. integration tools)
        for def in &defs {
            let name = def.function.name.as_str();
            if !tool_summaries.iter().any(|&(n, _)| n == name) {
                let desc = def.function.description.split('.').next().unwrap_or("");
                lines.push(format!("- {name}: {desc}"));
            }
        }
        lines.push(String::new());
        format!("\n<tooling>\n{}\n</tooling>\n", lines.join("\n"))
    }

    /// Build the tool call style guidance section.
    pub(super) fn build_tool_call_style_section() -> &'static str {
        "\n<tool_call_style>\n\
        Default: do not narrate routine, low-risk tool calls (just call the tool).\n\
        Narrate only when it helps: multi-step work, complex problems, sensitive actions (e.g. deletions), or when the user explicitly asks.\n\
        Keep narration brief and value-dense; avoid repeating obvious steps.\n\
        When a first-class tool exists for an action, use it directly instead of asking the user to run CLI commands.\n\
        Use apply_patch (not run_shell) to create or modify files. Use list_dir and read_file to understand code before editing.\n\
        </tool_call_style>\n"
    }

    /// Build the skill usage guidance section.
    pub(super) fn build_skill_usage_section() -> &'static str {
        "\n<skill_usage>\n\
        Skills teach reusable workflows and external CLI/API usage.\n\
        When a task clearly matches an available skill, use that skill's workflow instead of reinventing it from scratch.\n\
        If the task points at an integration or reusable workflow and the right skill is unclear, use the list tool with what=\"skills\".\n\
        Create a new user skill only when the workflow is reusable, non-trivial, and not already covered by existing tools or skills.\n\
        Do not create skills for one-off trivial tasks.\n\
        </skill_usage>\n"
    }

    /// Build the silent reply protocol section.
    pub(super) fn build_silent_reply_section() -> String {
        let token = constants::SILENT_REPLY_TOKEN;
        format!(
            "\n<silent_replies>\n\
            When you have nothing to say, respond with ONLY: {token}\n\
            Rules:\n\
            - It must be your ENTIRE message — nothing else\n\
            - Never append it to an actual response\n\
            - Never wrap it in markdown or code blocks\n\
            </silent_replies>\n"
        )
    }

    /// Build the heartbeat ack protocol section.
    pub(super) fn build_heartbeat_section(&self) -> String {
        let ok_token = constants::HEARTBEAT_OK_TOKEN;
        let interval = &self.config.heartbeat.interval;
        format!(
            "\n<heartbeat_protocol>\n\
            Heartbeat interval: {interval}. \
            If you receive a heartbeat poll (*heartbeat tick*) and there is nothing that needs attention, reply exactly:\n\
            {ok_token}\n\
            If something needs attention, do NOT include \"{ok_token}\"; reply with the alert text instead.\n\
            </heartbeat_protocol>\n"
        )
    }

    /// Build the reply tags section for message threading.
    pub(super) fn build_reply_tags_section() -> &'static str {
        "\n<reply_tags>\n\
        To request a native reply/quote on supported messaging channels, include one tag in your reply:\n\
        - [[reply_to_current]] replies to the triggering message. Must be the very first token (no leading text/newlines).\n\
        - Prefer [[reply_to_current]]. Use [[reply_to:<id>]] only when an id was explicitly provided.\n\
        Tags are stripped before sending; support depends on the channel.\n\
        </reply_tags>\n"
    }

    /// Build the messaging/channel routing guidance section.
    pub(super) fn build_messaging_section(&self) -> String {
        const CHANNELS: &str =
            "Telegram, Slack, Discord, Teams, Google Chat, Signal, Twilio, iMessage";
        format!(
            "\n<messaging>\n\
            - Reply in current session automatically routes to the source channel ({CHANNELS}).\n\
            - Native integrations are compiled in; do not use run_shell/curl for messaging.\n\
            - Gateway bindings provide per-channel/sender LLM routing overrides.\n\
            - Thread-scoped history: each sender+thread gets its own session.\n\
            </messaging>\n"
        )
    }

    /// Build the reasoning format section (conditional on thinking level).
    pub(super) fn build_reasoning_section(&self) -> String {
        let level = match self.config.llm.thinking {
            crate::config::ThinkingLevel::Off => return String::new(),
            crate::config::ThinkingLevel::Low => "low",
            crate::config::ThinkingLevel::Medium => "medium",
            crate::config::ThinkingLevel::High => "high",
            crate::config::ThinkingLevel::Xhigh => "xhigh",
        };
        format!(
            "\n<reasoning_format>\n\
            Extended thinking is enabled (level: {level}). \
            Internal reasoning is handled natively by the provider and hidden from the user.\n\
            </reasoning_format>\n"
        )
    }

    /// Build a `<scheduled_tasks>` snapshot for the dynamic suffix.
    ///
    /// Returns the empty string when there's nothing actionable — no failing
    /// tasks and no recent failed runs. When the heartbeat's `task_health`
    /// augmenter would fire, this section gives the *main user session* the
    /// same ground truth so a follow-up like "tell me what the tasks are"
    /// can be answered from real DB state instead of grepping source code.
    ///
    /// Regression context: heartbeat once said "5 failed task runs sitting
    /// in the queue" and the agent had no idea what it meant on follow-up
    /// because (a) the heartbeat ran in a separate session and (b) the
    /// system prompt had zero visibility into the task queue.
    pub(super) fn build_scheduled_tasks_section(&self) -> String {
        let guard = self.db_guard();
        let Some(db) = guard.as_ref() else {
            return String::new();
        };
        render_scheduled_tasks_snapshot(db, chrono::Utc::now().timestamp())
    }
}

/// Number of `task_runs` rows to inspect when summarizing 24h activity for
/// the `<scheduled_tasks>` snapshot. 24h matches the heartbeat augmenter's
/// window so the two surfaces stay in sync.
pub(super) const SCHEDULED_TASKS_WINDOW_SECS: i64 = 24 * 60 * 60;
/// Cap on per-task lines emitted into the snapshot to keep prompt size sane
/// when many tasks are failing simultaneously.
pub(super) const SCHEDULED_TASKS_MAX_LISTED: usize = 5;

/// Pure formatter — extracted so the section can be unit-tested without
/// constructing a full `Agent` (which requires sessions, metrics, etc.).
pub(super) fn render_scheduled_tasks_snapshot(db: &crate::db::Database, now: i64) -> String {
    let failing = match db.tasks_with_last_error() {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!("scheduled_tasks_section: tasks_with_last_error failed: {e}");
            return String::new();
        }
    };
    let recent_failed = db
        .count_failed_runs_since(now - SCHEDULED_TASKS_WINDOW_SECS)
        .unwrap_or(0);
    let recent_success = db
        .count_successful_runs_since(now - SCHEDULED_TASKS_WINDOW_SECS)
        .unwrap_or(0);

    if failing.is_empty() && recent_failed == 0 {
        return String::new();
    }

    let mut s = String::from("\n<scheduled_tasks trust=\"db\">\n");
    if !failing.is_empty() {
        s.push_str(&format!(
            "{} task(s) currently carry a last_error:\n",
            failing.len()
        ));
        for task in failing.iter().take(SCHEDULED_TASKS_MAX_LISTED) {
            let err = task
                .last_error
                .as_deref()
                .unwrap_or("(unknown)")
                .replace('\n', " ");
            let err = if err.len() > 120 { &err[..120] } else { &err };
            s.push_str(&format!(
                "  - {} (id={}, last_error: \"{}\")\n",
                task.name, task.id, err
            ));
        }
        if failing.len() > SCHEDULED_TASKS_MAX_LISTED {
            s.push_str(&format!(
                "  … and {} more (truncated)\n",
                failing.len() - SCHEDULED_TASKS_MAX_LISTED
            ));
        }
    }
    s.push_str(&format!(
        "task_runs in last 24h: {recent_success} succeeded, {recent_failed} failed\n"
    ));
    s.push_str(
        "When the user references \"the tasks\" or asks about failures, answer from this \
         list — do not invent counts or names. Use the `schedule` tool with action=\"runs\" \
         and an `id` from above for run history.\n",
    );
    s.push_str("</scheduled_tasks>\n");
    s
}

impl Agent {
    /// Build the `<collaboration_mode>` section from the current config.
    pub(super) fn build_collaboration_section(&self) -> String {
        let mode_template = match self.config.conversation.collaboration_mode {
            crate::config::CollaborationMode::Execute => COLLAB_MODE_EXECUTE,
            crate::config::CollaborationMode::Plan => COLLAB_MODE_PLAN,
        };
        format!("\n<collaboration_mode>\n{mode_template}\n</collaboration_mode>\n")
    }

    /// Build the `<active_memory trust="retrieved">` section: per-turn hybrid
    /// search of memory + past session transcripts, keyed off the last user
    /// message. Returns an empty string when disabled, when no last user
    /// message is available, or when the search returns no results.
    ///
    /// Lives in the dynamic suffix because the body changes per turn (would
    /// otherwise invalidate prefix-cache hits on every turn).
    pub(super) async fn build_active_memory_section(&self) -> String {
        let cfg = &self.config.memory.active_injection;
        if !cfg.enabled {
            return String::new();
        }
        let query = self
            .history
            .iter()
            .rev()
            .find(|m| m.role == crate::types::Role::User)
            .and_then(|m| m.text_content())
            .unwrap_or("")
            .trim()
            .to_string();
        // Match `load_memory_with_ranking`'s threshold: short acks ("ok", "yes")
        // produce noisy rankings.
        if query.len() < 10 {
            return String::new();
        }
        let args = serde_json::json!({
            "query": query,
            "max_results": cfg.top_k,
            "sources": ["memory", "messages"],
        });
        let body = match crate::tool_handlers::handle_memory_search(&args, &self.config).await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("active_memory search failed: {e}");
                return String::new();
            }
        };
        let trimmed = body.trim();
        if trimmed.is_empty()
            || trimmed == "No matching memories found."
            || trimmed == "No matching messages found."
        {
            return String::new();
        }
        let truncated = if crate::memory::tokenizer::estimate_tokens(trimmed) > cfg.max_tokens {
            // Cheap char-based clamp: 4 chars/token rule of thumb. Safe upper
            // bound — actual tokens will be ≤ cfg.max_tokens.
            let cap = cfg.max_tokens.saturating_mul(4);
            let mut cut = trimmed
                .char_indices()
                .take(cap)
                .last()
                .map(|(i, c)| i + c.len_utf8())
                .unwrap_or(trimmed.len());
            // Back off to the previous newline so we don't cut mid-result.
            if let Some(nl) = trimmed[..cut].rfind('\n') {
                cut = nl;
            }
            format!("{}\n…(truncated)", &trimmed[..cut])
        } else {
            trimmed.to_string()
        };
        format!("\n<active_memory trust=\"retrieved\">\n{truncated}\n</active_memory>\n")
    }
}

#[cfg(test)]
mod scheduled_tasks_snapshot_tests {
    //! Tests for the `<scheduled_tasks>` system-prompt snapshot.
    //!
    //! Regression context: the heartbeat once produced "5 failed task runs
    //! sitting in the queue" with zero DB grounding, and on follow-up the
    //! agent had no idea what tasks the user meant — it grep'd source for
    //! TODOs. The snapshot fixes that by feeding real DB state into every
    //! main-session turn. These tests pin both directions:
    //!   - **negative**: clean DB → no `<scheduled_tasks>` tag (no prompt bloat)
    //!   - **positive**: failing tasks/runs → tag present with names, ids,
    //!     errors, and counts that match DB rows verbatim.
    use super::{
        render_scheduled_tasks_snapshot, SCHEDULED_TASKS_MAX_LISTED, SCHEDULED_TASKS_WINDOW_SECS,
    };
    use crate::db::{Database, NewTask};
    use rusqlite::Connection;

    fn in_mem_db() -> Database {
        Database::from_connection(Connection::open_in_memory().expect("open")).expect("init db")
    }

    fn make_task(db: &Database, id: &str, name: &str) {
        db.create_task(&NewTask {
            id,
            name,
            prompt: "noop",
            schedule_type: "interval",
            schedule_expr: "30m",
            timezone: "UTC",
            next_run: Some(0),
            max_retries: Some(3),
            timeout_ms: None,
            delivery_channel: None,
            delivery_target: None,
            allowed_tools: None,
            task_type: "prompt",
            pre_run_command: None,
            pre_run_cwd: None,
            pre_run_timeout_ms: None,
            min_interval_secs: None,
        })
        .expect("create task");
    }

    #[test]
    fn snapshot_empty_when_no_failures() {
        // Negative case: clean DB (only the seeded default tasks, none with
        // last_error) must NOT emit a <scheduled_tasks> block. We don't want
        // to bloat the system prompt for users with healthy schedules.
        let db = in_mem_db();
        let snap = render_scheduled_tasks_snapshot(&db, 1_000_000);
        assert!(
            snap.is_empty(),
            "expected empty snapshot on clean DB, got: {snap}"
        );
    }

    #[test]
    fn snapshot_emits_block_with_names_ids_and_errors() {
        let db = in_mem_db();
        make_task(&db, "task-a-id", "daily_summary");
        make_task(&db, "task-b-id", "weekly_report");
        db.set_task_retry("task-a-id", 1, "openrouter 429", 9999)
            .expect("set");
        db.set_task_retry("task-b-id", 1, "timeout reading model", 9999)
            .expect("set");

        let snap = render_scheduled_tasks_snapshot(&db, 1_000_000);
        assert!(snap.contains("<scheduled_tasks trust=\"db\">"), "{snap}");
        // Names verbatim — so the LLM can resolve "the tasks" from the heartbeat.
        assert!(snap.contains("daily_summary"), "missing name: {snap}");
        assert!(snap.contains("weekly_report"), "missing name: {snap}");
        // IDs verbatim — so the LLM can call `schedule action=runs id=...`.
        assert!(snap.contains("task-a-id"), "missing id: {snap}");
        assert!(snap.contains("task-b-id"), "missing id: {snap}");
        // Errors verbatim — so the LLM can answer "what went wrong?".
        assert!(snap.contains("openrouter 429"), "missing err: {snap}");
        assert!(
            snap.contains("timeout reading model"),
            "missing err: {snap}"
        );
        // Real count.
        assert!(
            snap.contains("2 task(s) currently carry a last_error"),
            "expected count line: {snap}"
        );
        // Anti-hallucination directive must be present.
        assert!(
            snap.contains("do not invent counts or names"),
            "missing guard directive: {snap}"
        );
    }

    #[test]
    fn snapshot_window_filters_run_counts() {
        let db = in_mem_db();
        make_task(&db, "t1", "task1");
        let now = 1_000_000_i64;

        // Within 24h
        db.record_task_run("t1", now - 60, 100, None, Some("e1"))
            .expect("record");
        db.record_task_run("t1", now - 120, 100, Some("ok"), None)
            .expect("record");
        // Outside 24h — must be excluded
        db.record_task_run(
            "t1",
            now - SCHEDULED_TASKS_WINDOW_SECS - 1,
            100,
            None,
            Some("ancient"),
        )
        .expect("record");

        let snap = render_scheduled_tasks_snapshot(&db, now);
        assert!(
            snap.contains("task_runs in last 24h: 1 succeeded, 1 failed"),
            "expected 24h-windowed counts: {snap}"
        );
        assert!(
            !snap.contains("ancient"),
            "leaked outside-window error: {snap}"
        );
    }

    #[test]
    fn snapshot_truncates_long_failure_list() {
        let db = in_mem_db();
        let total = SCHEDULED_TASKS_MAX_LISTED + 3;
        for i in 0..total {
            let id = format!("id-{i}");
            let name = format!("task_{i}");
            make_task(&db, &id, &name);
            db.set_task_retry(&id, 1, "boom", 9999).expect("set");
        }
        let snap = render_scheduled_tasks_snapshot(&db, 1_000_000);
        // Header reflects the full count, not the truncated one.
        assert!(
            snap.contains(&format!("{total} task(s) currently carry a last_error")),
            "header must report real total: {snap}"
        );
        // Truncation marker present with the right remainder.
        assert!(
            snap.contains(&format!(
                "… and {} more (truncated)",
                total - SCHEDULED_TASKS_MAX_LISTED
            )),
            "expected truncation marker: {snap}"
        );
    }

    #[test]
    fn snapshot_emits_when_only_recent_runs_failed() {
        // Positive edge case: no scheduled_tasks.last_error set, but task_runs
        // shows recent failures (e.g. a task that already cleared its
        // last_error after success but had failures earlier in the window).
        // The block should still surface the failed-runs count.
        let db = in_mem_db();
        make_task(&db, "t1", "fluky");
        let now = 1_000_000_i64;
        db.record_task_run("t1", now - 60, 100, None, Some("transient"))
            .expect("record");

        let snap = render_scheduled_tasks_snapshot(&db, now);
        assert!(
            !snap.is_empty(),
            "should emit when there are recent failed runs even with no last_error set"
        );
        assert!(
            snap.contains("0 succeeded, 1 failed"),
            "expected failed-run count: {snap}"
        );
        // No "task(s) currently carry a last_error" line because none do.
        assert!(
            !snap.contains("carry a last_error"),
            "should not claim last_error tasks when there are none: {snap}"
        );
    }

    #[test]
    fn snapshot_size_stays_bounded_under_worst_case() {
        // Bloat guard: the snapshot ships in EVERY main-session system prompt
        // when failures exist, so unbounded growth there is a per-turn token
        // tax. Worst case = MAX_LISTED tasks each carrying a 120-char error.
        // We pin the upper bound at 2KB (~500 tokens) — generous, but small
        // enough that a future change adding a new field per row will trip
        // this test before shipping.
        let db = in_mem_db();
        for i in 0..(SCHEDULED_TASKS_MAX_LISTED * 4) {
            let id = format!("id-{i}");
            let name = format!("task_with_a_reasonably_long_name_{i}");
            make_task(&db, &id, &name);
            db.set_task_retry(&id, 1, &"x".repeat(500), 9999)
                .expect("set");
        }
        let snap = render_scheduled_tasks_snapshot(&db, 1_000_000);
        assert!(
            snap.len() < 2048,
            "snapshot must stay under 2KB even with many long-error tasks; got {} bytes:\n{snap}",
            snap.len()
        );
    }

    #[test]
    fn snapshot_redacts_newlines_in_error_messages() {
        // Defensive: a multi-line error message would otherwise break the
        // single-line bullet format and confuse an LLM scanning the section.
        let db = in_mem_db();
        make_task(&db, "t1", "noisy");
        db.set_task_retry("t1", 1, "line one\nline two\nline three", 9999)
            .expect("set");
        let snap = render_scheduled_tasks_snapshot(&db, 1_000_000);
        let bullet = snap
            .lines()
            .find(|l| l.starts_with("  - noisy"))
            .expect("bullet line missing");
        assert!(
            !bullet.contains('\n') && bullet.contains("line one line two line three"),
            "newlines must be flattened to spaces in the bullet: {bullet}"
        );
    }
}

#[cfg(test)]
mod scheduled_tasks_section_placement_tests {
    //! Source-level guard: the snapshot must live in the dynamic suffix.
    //! The `<scheduled_tasks>` block changes whenever the task queue changes,
    //! so emitting it before the cache-stable prefix would invalidate prefix
    //! caching on every turn for every user — a perf regression that's hard
    //! to spot at runtime. This test pins it next to the other suffix calls.
    const AGENT_RS_SRC: &str = include_str!("mod.rs");

    #[test]
    fn snapshot_call_lives_in_dynamic_suffix() {
        let suffix_marker = AGENT_RS_SRC
            .find("DYNAMIC SUFFIX")
            .expect("DYNAMIC SUFFIX marker must exist in build_system_prompt");
        let call_idx = AGENT_RS_SRC
            .find("build_scheduled_tasks_section")
            .expect("build_scheduled_tasks_section call must exist");
        assert!(
            call_idx > suffix_marker,
            "build_scheduled_tasks_section must be invoked AFTER the DYNAMIC \
             SUFFIX marker so it does not invalidate the prompt-cache prefix \
             (call_idx={call_idx}, suffix_marker={suffix_marker})",
        );
    }
}
