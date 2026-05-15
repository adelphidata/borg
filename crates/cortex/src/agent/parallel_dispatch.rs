//! Stateless dispatcher for parallel-safe tool calls.
//!
//! Lives outside [`super::Agent`] because nothing in the dispatch path touches
//! agent state — every tool here is a pure read against the filesystem,
//! memory store, or network. Hoisting it out of `agent/mod.rs` keeps the
//! orchestrator file focused on turn-state and frees this seam to be reused
//! by anything that needs to fan-out parallel reads against the same tool
//! surface (eval harness, planner dry-run).

use anyhow::Result;
use tokio::sync::mpsc;

use super::AgentEvent;
use crate::config::Config;
use crate::types::ToolOutput;

/// Execute a tool call that the planner classified as parallel-safe.
///
/// Parses through the typed [`crate::tool::Tool`] enum so the parallel-safety
/// gate is enforced by exhaustive `match` on `Tool::parallel_safe` — drift
/// between the planner and the dispatcher is impossible without a compile
/// error. Non-parallel-safe variants return a hard error so the planner bug
/// surfaces in tests instead of silent misbehaviour.
///
/// **No `ToolOutputDelta` from this path.** The surrounding loop emits
/// `ToolExecuting` / `ToolResult` events with the correct `tool_call_id`,
/// but the parallel dispatcher does not thread that id into the inner
/// handlers. None of the parallel-safe tools today (`read_file`,
/// `read_memory`, `memory_search`, `list_skills`, `list_channels`,
/// `list_dir`, `web_fetch`, `web_search`, `apply_patch`) emit streaming
/// output deltas, so the gap is invisible. Adding a tool here that wants
/// `ToolOutputDelta` requires extending this signature with `tool_call_id`
/// first, otherwise the delta will land at the OpenAI compat layer with an
/// empty id (see `agent_dispatcher::translate_events`).
pub(crate) async fn dispatch_parallel_safe_tool(
    name: &str,
    args_json: &str,
    config: &Config,
    _event_tx: &mpsc::Sender<AgentEvent>,
    execution_context: Option<&crate::execution_context::ExecutionContext>,
) -> Result<ToolOutput> {
    // Reject unknown / non-parallel-safe tools by *name* before parsing args.
    // Surface as a loud Err so a planner/dispatcher mismatch fails tests rather
    // than masquerading as an arg-validation error.
    if !crate::tool::is_parallel_safe_name(name) {
        return Err(anyhow::anyhow!(
            "dispatch_parallel_safe_tool called with non-parallel-safe tool '{name}' — planner/dispatch mismatch"
        ));
    }
    let raw_args: serde_json::Value = match serde_json::from_str(args_json) {
        Ok(v) => v,
        Err(e) => {
            return Ok(ToolOutput::Text(format!(
                "Error: Invalid JSON arguments: {e}. Please provide valid JSON."
            )));
        }
    };
    let tool = match crate::tool::Tool::parse(name, raw_args) {
        Ok(t) => t,
        Err(crate::tool::ToolParseError::UnknownTool(n)) => {
            return Err(anyhow::anyhow!(
                "dispatch_parallel_safe_tool called with unknown tool '{n}' — planner/dispatch mismatch"
            ));
        }
        Err(crate::tool::ToolParseError::InvalidArgs { tool, source }) => {
            return Ok(ToolOutput::Text(format!(
                "Error: Invalid arguments for tool '{tool}': {source}"
            )));
        }
    };

    debug_assert!(tool.parallel_safe(), "is_parallel_safe_name drift");

    match &tool {
        crate::tool::Tool::ReadFile(args) => match execution_context {
            Some(ctx) => crate::tool::read_file::handle_with_context(args, config, ctx),
            None => crate::tool::read_file::handle(args, config),
        },
        crate::tool::Tool::ReadMemory(args) => {
            crate::tool::read_memory::handle(args).map(ToolOutput::Text)
        }
        crate::tool::Tool::MemorySearch(args) => crate::tool::memory_search::handle(args, config)
            .await
            .map(ToolOutput::Text),
        crate::tool::Tool::ListSkills => {
            crate::tool::list::handle_list_skills(config).map(ToolOutput::Text)
        }
        crate::tool::Tool::ListChannels => {
            crate::tool::list::handle_list_channels(config).map(ToolOutput::Text)
        }
        crate::tool::Tool::ListDir(args) => match execution_context {
            Some(ctx) => crate::tool::list_dir::handle_with_context(args, config, ctx),
            None => crate::tool::list_dir::handle(args, config),
        }
        .map(ToolOutput::Text),
        crate::tool::Tool::WebFetch(args) => crate::tool::web_fetch::handle(args, config)
            .await
            .map(ToolOutput::Text),
        crate::tool::Tool::WebSearch(args) => crate::tool::web_search::handle(args, config)
            .await
            .map(ToolOutput::Text),
        crate::tool::Tool::ApplyPatch(args) => match execution_context {
            Some(ctx) => {
                crate::tool::apply_patch::handle_apply_patch_with_context(args, config, ctx)
            }
            None => crate::tool::apply_patch::handle_apply_patch(args, config),
        }
        .map(ToolOutput::Text),
        crate::tool::Tool::ApplySkillPatch(args) => {
            crate::tool::apply_patch::handle_apply_skill_patch(args).map(ToolOutput::Text)
        }
        // Anything still reaching here despite `parallel_safe()` returning true
        // is a closed-set inconsistency — fail loudly so tests catch it.
        other => Err(anyhow::anyhow!(
            "dispatch_parallel_safe_tool: variant '{}' is parallel_safe() but lacks a handler arm",
            other.name()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn dispatch_parallel_safe_tool_read_file_returns_content() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data.txt");
        std::fs::File::create(&path)
            .unwrap()
            .write_all(b"hello world")
            .unwrap();

        let config = Config::default();
        let (tx, _rx) = mpsc::channel(8);

        let args = serde_json::json!({"path": path.to_str().unwrap()}).to_string();
        let out = dispatch_parallel_safe_tool("read_file", &args, &config, &tx, None)
            .await
            .unwrap();
        let text = match out {
            ToolOutput::Text(t) => t,
            ToolOutput::Multimodal { text, .. } => text,
        };
        assert!(text.contains("hello world"), "got: {text}");
    }

    #[tokio::test]
    async fn dispatch_parallel_safe_tool_rejects_unsafe_tool_name() {
        // Guard: the planner should never route an Unknown-effect tool here.
        // If it does, the dispatcher surfaces a loud error rather than
        // silently misbehaving.
        let config = Config::default();
        let (tx, _rx) = mpsc::channel(8);
        let result = dispatch_parallel_safe_tool("run_shell", "{}", &config, &tx, None).await;
        assert!(result.is_err(), "unsafe tool must return Err");
        let msg = format!("{}", result.unwrap_err());
        assert!(
            msg.contains("non-parallel-safe") || msg.contains("planner/dispatch mismatch"),
            "error should name the mismatch: {msg}"
        );
    }

    #[tokio::test]
    async fn dispatch_parallel_safe_tool_invalid_json_returns_text() {
        let config = Config::default();
        let (tx, _rx) = mpsc::channel(8);
        let out = dispatch_parallel_safe_tool("read_file", "not json", &config, &tx, None)
            .await
            .unwrap();
        let text = match out {
            ToolOutput::Text(t) => t,
            _ => panic!("expected text output"),
        };
        assert!(text.contains("Invalid JSON"), "got: {text}");
    }
}
