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

use super::{Agent, COLLAB_MODE_DEFAULT, COLLAB_MODE_EXECUTE, COLLAB_MODE_PLAN, WORKFLOW_GUIDANCE};

impl Agent {
    /// Build the `<environment>` section with time, CWD, git context, OS, and runtime info.
    pub(super) async fn build_environment_section(&self) -> String {
        let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S %Z");
        let mut s = String::new();
        s.push_str("<environment>\n");
        s.push_str(&format!("Current Time: {now}\n"));
        if let Ok(cwd) = std::env::current_dir() {
            s.push_str(&format!("Working directory: {}\n", cwd.display()));
        }
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
            ("write_memory", "Write/append to memory files"),
            ("read_memory", "Read a memory file"),
            (
                "memory_search",
                "Semantic search across memory and sessions",
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
                "Manage scheduled jobs: prompt tasks, cron commands, workflows",
            ),
            (
                "projects",
                "Manage projects (create/list/get/update/archive/delete)",
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
        let mut defs = crate::tool_definitions::core_tool_definitions(&self.config);
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

    /// Build the `<collaboration_mode>` section from the current config.
    pub(super) fn build_collaboration_section(&self) -> String {
        let mode_template = match self.config.conversation.collaboration_mode {
            crate::config::CollaborationMode::Default => COLLAB_MODE_DEFAULT,
            crate::config::CollaborationMode::Execute => COLLAB_MODE_EXECUTE,
            crate::config::CollaborationMode::Plan => COLLAB_MODE_PLAN,
        };
        format!("\n<collaboration_mode>\n{mode_template}\n</collaboration_mode>\n")
    }

    /// Build the `<workflow_guidance>` section when workflows are active.
    pub(super) fn build_workflow_guidance_section(&self) -> String {
        if crate::workflow::workflows_active(&self.config) {
            format!("\n<workflow_guidance>\n{WORKFLOW_GUIDANCE}\n</workflow_guidance>\n")
        } else {
            String::new()
        }
    }
}
