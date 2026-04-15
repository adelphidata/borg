//! Tool dispatch helpers extracted from the main agent loop.
//!
//! These functions contain the non-trivial logic that was previously inlined
//! in `execute_tool`'s match arms (write_memory effects, multi-agent routing).

use std::sync::Arc;

use anyhow::Result;

use crate::config::Config;
use crate::constants::IDENTITY_FILE;
use crate::tool_handlers;

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

    result
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
        "spawn_agent" => {
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
        "send_to_agent" => Err(anyhow::anyhow!("send_to_agent is not yet implemented")),
        "wait_for_agent" => {
            if let Some(ref mut ctrl) = agent_control {
                crate::multi_agent::tools::handle_wait_for_agent(args, ctrl).await
            } else {
                Ok(MULTI_AGENT_DISABLED.to_string())
            }
        }
        "list_agents" => {
            if let Some(ref ctrl) = agent_control {
                crate::multi_agent::tools::handle_list_agents(ctrl)
            } else {
                Ok(MULTI_AGENT_DISABLED.to_string())
            }
        }
        "close_agent" => {
            if let Some(ref mut ctrl) = agent_control {
                crate::multi_agent::tools::handle_close_agent(args, ctrl)
            } else {
                Ok(MULTI_AGENT_DISABLED.to_string())
            }
        }
        "manage_roles" => crate::multi_agent::tools::handle_manage_roles(args),
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
    async fn send_to_agent_always_unimplemented() {
        let mut ctrl: Option<crate::multi_agent::AgentControl> = None;
        let config = Config::default();
        let err = try_handle_multi_agent_tool(
            "send_to_agent",
            &json!({"agent_id": "x", "message": "y"}),
            &mut ctrl,
            &config,
            &[],
        )
        .await
        .expect("multi-agent tool matched")
        .expect_err("send_to_agent is not yet implemented");
        assert!(err.to_string().contains("not yet implemented"));
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
