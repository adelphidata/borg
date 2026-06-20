//! System-prompt section builders and the top-level `build_system_prompt`
//! orchestrator.
//!
//! Extracted from `agent/mod.rs` so the per-section logic lives next to the
//! orchestrator that stitches them together — interleaving with
//! session-management, tool-execution, and loop state made `mod.rs`
//! unnavigable.
//!
//! All section builders and `build_system_prompt` itself are methods on
//! `Agent`; the trait-like surface is kept intact through additional
//! `impl Agent` blocks — Rust allows multiple impls of the same type within
//! a crate.

use std::sync::LazyLock;

use anyhow::{Context, Result};
use tracing::instrument;

use crate::constants;
use crate::identity::load_identity;
use crate::skills::load_skills_context;
use crate::template::Template;

use super::Agent;

fn parse_template(source: &str) -> Template {
    Template::parse(source).unwrap_or_else(|e| {
        tracing::error!("Failed to parse template: {e} — using fallback");
        // SAFETY: This literal template string is known valid at compile time
        #[allow(clippy::expect_used)]
        Template::parse("{{ memory }}").expect("fallback template must parse")
    })
}

static MEMORY_TEMPLATE: LazyLock<Template> = LazyLock::new(|| {
    parse_template("\n<long_term_memory trust=\"stored\">\n{{ memory }}\n</long_term_memory>\n")
});

static KNOWLEDGE_TEMPLATE: LazyLock<Template> = LazyLock::new(|| {
    parse_template("\n<knowledge_files trust=\"curated\">\n{{ knowledge }}\n</knowledge_files>\n")
});

static SKILLS_TEMPLATE: LazyLock<Template> =
    LazyLock::new(|| parse_template("\n<skills trust=\"verified\">\n{{ skills }}\n</skills>\n"));

pub(super) static SETUP_TEMPLATE: LazyLock<Template> = LazyLock::new(|| {
    parse_template("\n<first_conversation>\n{{ setup }}\n</first_conversation>\n")
});

/// Result of one pass of the SETUP.md lifecycle.
pub(super) struct SetupInjection {
    /// Raw SETUP.md content to render through `SETUP_TEMPLATE`.
    pub content: String,
    /// True exactly once per SETUP.md instance — on the turn the marker
    /// file was just created. Drives the one-shot tool-stripping in
    /// `prompt_assembly`.
    pub first_turn: bool,
}

/// Drive the SETUP.md lifecycle for one turn of `build_system_prompt`.
///
/// Behavior:
/// - If SETUP.md is absent, return `None` (nothing to inject — onboarding
///   was retired previously, or never happened).
/// - If `onboarding_complete` is true, retire SETUP.md and its marker
///   from disk and return `None`. The agent has shaped its identity, so
///   first-conversation rules no longer apply.
/// - Otherwise read SETUP.md and return its content. Atomically create
///   the marker; `first_turn` is true only on the call that created it.
///
/// This decouples the two responsibilities the previous "rename → read
/// → delete" pattern bundled: tool suppression (one-shot) and rule
/// persistence (multi-turn). The marker — not SETUP.md's existence — is
/// now the "we've already done turn-1 suppression" signal.
pub(super) async fn consume_setup_template(
    data_dir: &std::path::Path,
    onboarding_complete: bool,
) -> Option<SetupInjection> {
    let setup_path = data_dir.join(crate::constants::SETUP_FILE);
    let marker_path = data_dir.join(crate::constants::SETUP_FIRST_TURN_MARKER_FILE);

    if !tokio::fs::try_exists(&setup_path).await.unwrap_or(false) {
        return None;
    }

    if onboarding_complete {
        if let Err(e) = tokio::fs::remove_file(&setup_path).await {
            tracing::warn!("Failed to retire SETUP.md: {e}");
        }
        // Best-effort marker cleanup; ENOENT is fine.
        if let Err(e) = tokio::fs::remove_file(&marker_path).await {
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!("Failed to remove SETUP first-turn marker: {e}");
            }
        }
        return None;
    }

    let content = match tokio::fs::read_to_string(&setup_path).await {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("Failed to read SETUP.md: {e}");
            return None;
        }
    };

    // Atomic `create_new` is the synchronization primitive: exactly one
    // call across concurrent sessions succeeds, the rest see
    // `AlreadyExists` and treat the turn as non-first.
    let first_turn = match tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&marker_path)
        .await
    {
        Ok(_) => true,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => false,
        Err(e) => {
            tracing::warn!("Failed to create SETUP first-turn marker: {e}");
            false
        }
    };

    Some(SetupInjection {
        content,
        first_turn,
    })
}

const COLLAB_MODE_EXECUTE: &str = include_str!("../../templates/collaboration_mode/execute.md");
const COLLAB_MODE_PLAN: &str = include_str!("../../templates/collaboration_mode/plan.md");

const SECURITY_POLICY: &str = include_str!("../../templates/security_policy.md");

impl Agent {
    /// Build the full system prompt for this turn.
    ///
    /// Ordering matters: the stable prefix (identity, tooling, security
    /// policy, skills, long-term memory, knowledge, project docs) MUST
    /// come before the dynamic suffix (active memory, goal context,
    /// working memory, collaboration mode, environment, scheduled tasks).
    /// Anything that changes turn-to-turn lives in the suffix so the
    /// prefix-cache stays warm on providers that hash the prompt.
    ///
    /// The `// === STABLE PREFIX ===` and `// === DYNAMIC SUFFIX ===`
    /// comment markers below are load-bearing — guard tests in
    /// `mod scheduled_tasks_section_placement_tests` and the
    /// `build_system_prompt_*` tests source-grep for them to detect
    /// reordering regressions.
    #[instrument(skip_all)]
    pub(super) async fn build_system_prompt(&mut self) -> Result<String> {
        let identity = self.caches.identity_or_load(load_identity)?;
        let memory = self.load_memory_with_ranking().await?;
        let knowledge = self.load_knowledge_with_ranking().await?;
        let mut system = String::with_capacity(16_384);

        // === STABLE PREFIX === ordered so the largest byte-stable content comes first, enabling
        // implicit prefix caching on providers that hash the prompt (OpenAI,
        // Gemini) and maximizing cache hits on providers with explicit markers
        // (Anthropic). Anything that changes turn-to-turn is pushed to the
        // dynamic suffix below.
        system.push_str("<system_instructions>\n");
        system.push_str(&identity);
        system.push_str("\n</system_instructions>\n\n");

        // Tooling: list available tools with summaries
        system.push_str(&self.build_tooling_section().await);

        // Tool call style guidance
        system.push_str(Self::build_tool_call_style_section());
        system.push_str(Self::build_skill_usage_section());

        // Safety / security policy
        use std::fmt::Write as _;
        let _ = write!(
            system,
            "\n<security_policy>\n{SECURITY_POLICY}\n</security_policy>\n"
        );

        if self.config.skills.enabled {
            let user_request = self.latest_user_message_text().to_string();
            let skills = if let Some(cached) = self.caches.skills_for_request(&user_request) {
                cached.to_string()
            } else {
                self.ensure_credentials_cached();
                let budget = crate::skills::effective_skill_budget(
                    &self.config.skills,
                    Some(self.active_context_window()),
                );
                let (rendered, report) = load_skills_context(
                    budget,
                    self.credentials_cached(),
                    &self.config.skills,
                    &user_request,
                )?;
                self.warn_skill_budget_loss(&report);
                self.caches.set_skills(user_request, rendered.clone());
                rendered
            };
            if !skills.is_empty() {
                system.push_str(
                    &SKILLS_TEMPLATE
                        .render([("skills", skills.as_str())])
                        .context("skills template render failed")?,
                );
            }
        }

        if !memory.is_empty() {
            system.push_str(
                &MEMORY_TEMPLATE
                    .render([("memory", memory.as_str())])
                    .context("memory template render failed")?,
            );
        }
        if !knowledge.is_empty() {
            system.push_str(
                &KNOWLEDGE_TEMPLATE
                    .render([("knowledge", knowledge.as_str())])
                    .context("knowledge template render failed")?,
            );
        }

        system.push_str("\n<memory_recall>\nWhen answering questions about prior work, past decisions, dates, people, preferences, todos, or anything previously discussed, use the memory_search tool to look up relevant context. Auto-loaded memory above may not contain all relevant information.\n</memory_recall>\n");

        // Reply tags for message threading (channels)
        system.push_str(Self::build_reply_tags_section());

        // Messaging / channel routing guidance
        system.push_str(&self.build_messaging_section());

        // Silent reply protocol
        system.push_str(&Self::build_silent_reply_section());

        // Heartbeat ack protocol
        system.push_str(&self.build_heartbeat_section());

        // Reasoning format (conditional on thinking level)
        system.push_str(&self.build_reasoning_section());

        // Project documentation (AGENTS.md / CLAUDE.md)
        let project_docs = self.caches.project_docs_or_load(|| {
            crate::project_doc::discover_project_docs(self.execution_context.workspace_cwd())
                .ok()
                .flatten()
        });
        if let Some(ref docs) = project_docs {
            let _ = write!(
                system,
                "\n<project_instructions trust=\"stored\">\n{docs}\n</project_instructions>\n"
            );
        }

        // === DYNAMIC SUFFIX === per-turn content that invalidates the prefix cache.

        // Active memory: per-turn hybrid search of memory + past sessions
        // keyed off the latest user message. Placed at the start of the
        // dynamic suffix so it sits adjacent to <working_memory> and after
        // the stable <long_term_memory> prefix. Empty string when disabled
        // or when the search returns no hits.
        system.push_str(&self.build_active_memory_section().await);

        // Active goal context — the agent must see the current objective so
        // every turn pulls toward it. Untrusted: the objective is user-data,
        // not a higher-priority instruction set.
        system.push_str(&self.build_goal_context_section().await);

        // Short-term working memory (session facts, active project)
        let working_memory = self.short_term_memory.render();
        if !working_memory.is_empty() {
            system.push_str(&working_memory);
        }

        system.push_str(&self.build_collaboration_section());
        system.push_str(&self.build_environment_section().await);
        system.push_str(&self.build_scheduled_tasks_section().await);

        // Inject first-conversation instructions from SETUP.md (created
        // during onboarding). The template stays on disk for the entire
        // first conversation and is retired only when the agent has
        // shaped its identity in long-term memory — otherwise the
        // request_user_input / choice-list rules would evaporate between
        // turn 1 and turn 2 and the model would default to prose menus.
        if let Ok(data_dir) = crate::config::Config::data_dir() {
            // Cheap stat first — the DB lookup for the onboarding-complete
            // signal only matters while SETUP.md still lives. Post-retirement
            // every install pays one stat per turn instead of opening libsql.
            let setup_path = data_dir.join(crate::constants::SETUP_FILE);
            if tokio::fs::try_exists(&setup_path).await.unwrap_or(false) {
                let onboarding_complete = crate::memory::identity_in_long_term_memory().await;
                if let Some(outcome) = consume_setup_template(&data_dir, onboarding_complete).await
                {
                    system.push_str(
                        &SETUP_TEMPLATE
                            .render([("setup", outcome.content.as_str())])
                            .context("setup template render failed")?,
                    );
                    if outcome.first_turn {
                        // Signal to the agent turn: on this turn we must
                        // not offer tools to the model. SETUP.md forbids
                        // first-message tool calls in prose, but that is
                        // not sufficient for weak instruction-following
                        // models (e.g. Grok-4.20 emits a malformed
                        // `write_memory` call anyway). Subsequent turns
                        // keep SETUP.md in context but unblock tools so
                        // the model can call `request_user_input`.
                        self.setup_just_consumed = true;
                    }
                }
            }
        }

        Ok(system)
    }

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
        // After a failover, report the provider/model that actually served the
        // last response so the model identifies itself truthfully; otherwise
        // report the configured primary.
        if let Some((provider, model)) = self.active_failover_identity.as_ref() {
            runtime_parts.push(format!("provider={provider} (failover active)"));
            if !model.is_empty() {
                runtime_parts.push(format!("model={model}"));
            }
        } else {
            if let Some(ref provider) = self.config.llm.provider {
                runtime_parts.push(format!("provider={provider}"));
            }
            if !self.config.llm.model.is_empty() {
                runtime_parts.push(format!("model={}", self.config.llm.model));
            }
        }
        // Render the level via serde so new variants (minimal, adaptive, max)
        // pick up automatically. `serde_json::to_value` always succeeds for
        // a simple enum; fall back defensively just in case.
        let effective = self.config.llm.effective_thinking(&self.config.llm.model);
        let thinking = serde_json::to_value(&effective)
            .ok()
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_else(|| "off".to_string());
        runtime_parts.push(format!("thinking={thinking}"));
        if let Some(ref tz) = self.config.user.timezone {
            runtime_parts.push(format!("timezone={tz}"));
        }
        s.push_str(&format!("Runtime: {}\n", runtime_parts.join(" | ")));
        s.push_str("</environment>\n");
        s
    }

    /// Build the `<tooling>` section listing available tools with summaries.
    ///
    /// Summaries are pulled from the per-module `TOOL_SUMMARIES` tables
    /// (`tool::definitions::TOOL_SUMMARIES` for the core set,
    /// `multi_agent::tools::MULTI_AGENT_TOOL_SUMMARIES` for the spawn/send/
    /// wait/close set). Dynamic tools (WASM plugins, integrations registered
    /// via skills) won't have a static entry — for those the section falls
    /// back to the first sentence of the tool's `ToolDefinition::description`.
    pub(super) async fn build_tooling_section(&self) -> String {
        let mut defs = crate::tool::definitions::core_tool_definitions(&self.config).await;
        if self.agent_control.is_some() {
            defs.extend(crate::multi_agent::tools::tool_definitions(
                self.spawn_depth,
                self.config.agents.max_spawn_depth,
            ));
        }

        let static_summary = |name: &str| -> Option<&'static str> {
            crate::tool::definitions::TOOL_SUMMARIES
                .iter()
                .chain(crate::multi_agent::tools::MULTI_AGENT_TOOL_SUMMARIES.iter())
                .find_map(|(n, summary)| (*n == name).then_some(*summary))
        };

        let mut lines = vec![
            "## Tooling".to_string(),
            "Available tools (filtered by config):".to_string(),
        ];
        for def in &defs {
            let name = def.function.name.as_str();
            let summary = static_summary(name).unwrap_or_else(|| {
                // Fallback for dynamic tools (WASM plugins / integrations).
                def.function.description.split('.').next().unwrap_or("")
            });
            lines.push(format!("- {name}: {summary}"));
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
        let effective = self.config.llm.effective_thinking(&self.config.llm.model);
        if !effective.is_enabled() {
            return String::new();
        }
        let level = serde_json::to_value(&effective)
            .ok()
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_else(|| "off".to_string());
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
    pub(super) async fn build_scheduled_tasks_section(&self) -> String {
        let guard = self.db_guard().await;
        let Some(db) = guard.as_ref() else {
            return String::new();
        };
        render_scheduled_tasks_snapshot(db, chrono::Utc::now().timestamp()).await
    }
}

pub(super) use crate::constants::{SCHEDULED_TASKS_MAX_LISTED, SCHEDULED_TASKS_WINDOW_SECS};

/// Pure formatter — extracted so the section can be unit-tested without
/// constructing a full `Agent` (which requires sessions, metrics, etc.).
pub(super) async fn render_scheduled_tasks_snapshot(db: &crate::db::Database, now: i64) -> String {
    let failing = match db.tasks_with_last_error().await {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!("scheduled_tasks_section: tasks_with_last_error failed: {e}");
            return String::new();
        }
    };
    let recent_failed = db
        .count_failed_runs_since(now - SCHEDULED_TASKS_WINDOW_SECS)
        .await
        .unwrap_or(0);
    let recent_success = db
        .count_successful_runs_since(now - SCHEDULED_TASKS_WINDOW_SECS)
        .await
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
    /// Build the `<goal_context trust="untrusted">` section if an Active /
    /// Paused / BudgetLimited goal exists for this session. The objective is
    /// XML-escaped so a malicious objective cannot inject tags. Complete goals
    /// are dropped (no steering value).
    ///
    /// Empty string when no goal is set, the DB is unavailable, or the goal
    /// is Complete. Lookups are best-effort and `tracing::warn!` on error;
    /// prompt assembly must never break the agent.
    pub(super) async fn build_goal_context_section(&self) -> String {
        if !self.config.goals.enabled {
            return String::new();
        }
        let session_id = &self.session.meta.id;
        let guard = self.db.lock().await;
        let db = match guard.as_ref() {
            Some(db) => db,
            None => return String::new(),
        };
        let goal = match db.get_goal(session_id).await {
            Ok(g) => g,
            Err(e) => {
                tracing::warn!("build_goal_context_section: get_goal failed: {e}");
                return String::new();
            }
        };
        let Some(goal) = goal else {
            return String::new();
        };
        if matches!(goal.status, crate::goals::GoalStatus::Complete) {
            return String::new();
        }
        let escaped = crate::goals::prompts::escape_xml_text(&goal.objective);
        let status_label = goal.status.as_str();
        let budget_line = match goal.token_budget {
            Some(b) => format!("\nbudget: {b} tokens (used: {})", goal.tokens_used),
            None => String::new(),
        };
        format!(
            "\n<goal_context trust=\"untrusted\" status=\"{status_label}\">\n{escaped}{budget_line}\n</goal_context>\n"
        )
    }

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

    async fn make_task(db: &Database, id: &str, name: &str) {
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
        .await
        .expect("create task");
    }

    #[tokio::test]
    async fn snapshot_empty_when_no_failures() {
        // Negative case: clean DB (only the seeded default tasks, none with
        // last_error) must NOT emit a <scheduled_tasks> block. We don't want
        // to bloat the system prompt for users with healthy schedules.
        let db = Database::test_db().await;
        let snap = render_scheduled_tasks_snapshot(&db, 1_000_000).await;
        assert!(
            snap.is_empty(),
            "expected empty snapshot on clean DB, got: {snap}"
        );
    }

    #[tokio::test]
    async fn snapshot_emits_block_with_names_ids_and_errors() {
        let db = Database::test_db().await;
        make_task(&db, "task-a-id", "daily_summary").await;
        make_task(&db, "task-b-id", "weekly_report").await;
        db.set_task_retry("task-a-id", 1, "openrouter 429", 9999)
            .await
            .expect("set");
        db.set_task_retry("task-b-id", 1, "timeout reading model", 9999)
            .await
            .expect("set");

        let snap = render_scheduled_tasks_snapshot(&db, 1_000_000).await;
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

    #[tokio::test]
    async fn snapshot_window_filters_run_counts() {
        let db = Database::test_db().await;
        make_task(&db, "t1", "task1").await;
        let now = 1_000_000_i64;

        // Within 24h
        db.record_task_run("t1", now - 60, 100, None, Some("e1"))
            .await
            .expect("record");
        db.record_task_run("t1", now - 120, 100, Some("ok"), None)
            .await
            .expect("record");
        // Outside 24h — must be excluded
        db.record_task_run(
            "t1",
            now - SCHEDULED_TASKS_WINDOW_SECS - 1,
            100,
            None,
            Some("ancient"),
        )
        .await
        .expect("record");

        let snap = render_scheduled_tasks_snapshot(&db, now).await;
        assert!(
            snap.contains("task_runs in last 24h: 1 succeeded, 1 failed"),
            "expected 24h-windowed counts: {snap}"
        );
        assert!(
            !snap.contains("ancient"),
            "leaked outside-window error: {snap}"
        );
    }

    #[tokio::test]
    async fn snapshot_truncates_long_failure_list() {
        let db = Database::test_db().await;
        let total = SCHEDULED_TASKS_MAX_LISTED + 3;
        for i in 0..total {
            let id = format!("id-{i}");
            let name = format!("task_{i}");
            make_task(&db, &id, &name).await;
            db.set_task_retry(&id, 1, "boom", 9999).await.expect("set");
        }
        let snap = render_scheduled_tasks_snapshot(&db, 1_000_000).await;
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

    #[tokio::test]
    async fn snapshot_emits_when_only_recent_runs_failed() {
        // Positive edge case: no scheduled_tasks.last_error set, but task_runs
        // shows recent failures (e.g. a task that already cleared its
        // last_error after success but had failures earlier in the window).
        // The block should still surface the failed-runs count.
        let db = Database::test_db().await;
        make_task(&db, "t1", "fluky").await;
        let now = 1_000_000_i64;
        db.record_task_run("t1", now - 60, 100, None, Some("transient"))
            .await
            .expect("record");

        let snap = render_scheduled_tasks_snapshot(&db, now).await;
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

    #[tokio::test]
    async fn snapshot_size_stays_bounded_under_worst_case() {
        // Bloat guard: the snapshot ships in EVERY main-session system prompt
        // when failures exist, so unbounded growth there is a per-turn token
        // tax. Worst case = MAX_LISTED tasks each carrying a 120-char error.
        // We pin the upper bound at 2KB (~500 tokens) — generous, but small
        // enough that a future change adding a new field per row will trip
        // this test before shipping.
        let db = Database::test_db().await;
        for i in 0..(SCHEDULED_TASKS_MAX_LISTED * 4) {
            let id = format!("id-{i}");
            let name = format!("task_with_a_reasonably_long_name_{i}");
            make_task(&db, &id, &name).await;
            db.set_task_retry(&id, 1, &"x".repeat(500), 9999)
                .await
                .expect("set");
        }
        let snap = render_scheduled_tasks_snapshot(&db, 1_000_000).await;
        assert!(
            snap.len() < 2048,
            "snapshot must stay under 2KB even with many long-error tasks; got {} bytes:\n{snap}",
            snap.len()
        );
    }

    #[tokio::test]
    async fn snapshot_redacts_newlines_in_error_messages() {
        // Defensive: a multi-line error message would otherwise break the
        // single-line bullet format and confuse an LLM scanning the section.
        let db = Database::test_db().await;
        make_task(&db, "t1", "noisy").await;
        db.set_task_retry("t1", 1, "line one\nline two\nline three", 9999)
            .await
            .expect("set");
        let snap = render_scheduled_tasks_snapshot(&db, 1_000_000).await;
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
mod build_system_prompt_tests {
    //! Source-level guards over `build_system_prompt`. The function ships in
    //! every turn's system prompt, so reordering bugs (dynamic content before
    //! the cache-stable prefix) silently break prompt caching across every
    //! provider. These tests source-grep the function body for the `===
    //! STABLE PREFIX ===` / `=== DYNAMIC SUFFIX ===` markers and pin the
    //! relative order of each section call.
    const SYSTEM_PROMPT_RS_SRC: &str = include_str!("system_prompt.rs");

    fn extract_build_system_prompt_body() -> &'static str {
        let start = SYSTEM_PROMPT_RS_SRC
            .find("async fn build_system_prompt")
            .expect("build_system_prompt must exist");
        // Extract ~6KB following the signature — plenty to cover the body.
        let end = (start + 6000).min(SYSTEM_PROMPT_RS_SRC.len());
        &SYSTEM_PROMPT_RS_SRC[start..end]
    }

    #[test]
    fn build_system_prompt_environment_is_after_security_policy() {
        let body = extract_build_system_prompt_body();
        let security_idx = body
            .find("security_policy")
            .expect("security_policy section must exist in build_system_prompt");
        let env_idx = body
            .find("build_environment_section")
            .expect("build_environment_section call must exist in build_system_prompt");
        assert!(
            env_idx > security_idx,
            "build_environment_section must be pushed AFTER security_policy \
             to keep the cached prefix stable across turns \
             (env_idx={env_idx}, security_idx={security_idx})",
        );
    }

    #[test]
    fn build_system_prompt_environment_is_after_identity() {
        let body = extract_build_system_prompt_body();
        let identity_idx = body
            .find("system_instructions")
            .expect("system_instructions (identity) section must exist");
        let env_idx = body
            .find("build_environment_section")
            .expect("build_environment_section call must exist");
        assert!(
            env_idx > identity_idx,
            "dynamic environment section must come after identity",
        );
    }

    #[test]
    fn build_system_prompt_environment_is_after_tooling() {
        let body = extract_build_system_prompt_body();
        let tooling_idx = body
            .find("build_tooling_section")
            .expect("build_tooling_section call must exist");
        let env_idx = body
            .find("build_environment_section")
            .expect("build_environment_section call must exist");
        assert!(
            env_idx > tooling_idx,
            "dynamic environment section must come after tooling",
        );
    }

    #[test]
    fn build_system_prompt_active_memory_in_dynamic_suffix() {
        // Cache-stability invariant: <active_memory> changes per turn (its
        // content is keyed off the latest user message), so it MUST live in
        // the DYNAMIC SUFFIX block — never before, where it would invalidate
        // the prefix cache on every turn.
        let body = extract_build_system_prompt_body();
        let suffix_marker = body
            .find("DYNAMIC SUFFIX")
            .expect("DYNAMIC SUFFIX marker must exist");
        let active_idx = body
            .find("build_active_memory_section")
            .expect("build_active_memory_section call must exist in build_system_prompt");
        assert!(
            active_idx > suffix_marker,
            "active_memory must be pushed AFTER the DYNAMIC SUFFIX marker \
             so it does not invalidate the prefix cache \
             (active_idx={active_idx}, suffix_marker={suffix_marker})",
        );
    }

    #[test]
    fn snapshot_call_lives_in_dynamic_suffix() {
        let body = extract_build_system_prompt_body();
        let suffix_marker = body
            .find("DYNAMIC SUFFIX")
            .expect("DYNAMIC SUFFIX marker must exist in build_system_prompt");
        let call_idx = body
            .find("build_scheduled_tasks_section")
            .expect("build_scheduled_tasks_section call must exist");
        assert!(
            call_idx > suffix_marker,
            "build_scheduled_tasks_section must be invoked AFTER the DYNAMIC \
             SUFFIX marker so it does not invalidate the prompt-cache prefix \
             (call_idx={call_idx}, suffix_marker={suffix_marker})",
        );
    }

    #[test]
    fn setup_template_wraps_in_first_conversation_tags() {
        let rendered = super::SETUP_TEMPLATE
            .render([("setup", "Hello world")])
            .unwrap();
        assert!(rendered.contains("<first_conversation>"));
        assert!(rendered.contains("</first_conversation>"));
        assert!(rendered.contains("Hello world"));
    }

    #[test]
    fn setup_template_preserves_multiline_content() {
        let content = "# First Boot\n\nLine 1\nLine 2\n- bullet";
        let rendered = super::SETUP_TEMPLATE.render([("setup", content)]).unwrap();
        assert!(rendered.contains("# First Boot"));
        assert!(rendered.contains("- bullet"));
    }
}
