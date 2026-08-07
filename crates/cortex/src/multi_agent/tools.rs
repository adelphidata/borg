use anyhow::Result;
use serde_json::json;

use crate::tool::names as tool_names;
use crate::tool_handlers::require_str_param;
use crate::types::ToolDefinition;

use super::roles;
use super::{AgentControl, AgentRoleKind};

/// One-line summaries for multi-agent tools, surfaced in the system prompt's
/// `<tooling>` section. Mirrors [`crate::tool::definitions::TOOL_SUMMARIES`]
/// but lives here so the prose stays next to the definitions.
///
/// **Intentionally narrow.** `send_to_agent` is *not* listed because
/// `tool_definitions` does not emit its definition (the receive path is a
/// stub — see the NOTE block in [`tool_definitions`]); summarising a tool
/// the LLM cannot call would be dead text in the prompt. `list_agents` and
/// `manage_roles` are intentional fallbacks — `system_prompt`'s
/// description-first-sentence path renders an acceptable summary for them
/// without needing a hand-crafted entry here.
pub const MULTI_AGENT_TOOL_SUMMARIES: &[(&str, &str)] = &[
    (tool_names::SPAWN_AGENT, "Spawn an isolated sub-agent"),
    (
        tool_names::WAIT_FOR_AGENT,
        "Wait for a sub-agent to complete",
    ),
    (tool_names::CLOSE_AGENT, "Close a running sub-agent"),
];

/// Tool definitions for the multi-agent system.
pub fn tool_definitions(spawn_depth: u32, max_spawn_depth: u32) -> Vec<ToolDefinition> {
    let mut defs = vec![
        ToolDefinition::new(
            "manage_roles",
            "Manage agent roles: list, create, update, or delete role definitions. Roles define model, temperature, allowed tools, and instructions for sub-agents.",
            json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["list", "create", "update", "delete"],
                        "description": "Action to perform"
                    },
                    "name": {
                        "type": "string",
                        "description": "Role name (required for create/update/delete)"
                    },
                    "description": {
                        "type": "string",
                        "description": "Role description"
                    },
                    "model": {
                        "type": "string",
                        "description": "Model override for this role"
                    },
                    "provider": {
                        "type": "string",
                        "description": "Provider override (openrouter, openai, anthropic, gemini)"
                    },
                    "temperature": {
                        "type": "number",
                        "description": "Temperature override"
                    },
                    "system_instructions": {
                        "type": "string",
                        "description": "Additional system instructions for this role"
                    },
                    "tools_allowed": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "Whitelist of tool names this role can use (null = all tools)"
                    },
                    "max_iterations": {
                        "type": "integer",
                        "description": "Max agent loop iterations for this role"
                    }
                },
                "required": ["action"]
            }),
        ),
    ];

    // Only add spawn/interact tools if we haven't hit depth limit
    if spawn_depth < max_spawn_depth {
        defs.push(ToolDefinition::new(
            "spawn_agent",
            "Spawn a sub-agent to work on a task. By default returns immediately with an agent_id you can wait on later. Set blocking=true to wait in-line and receive the child's final result in the same turn. Pass tasks=[…] to fan out N children in parallel and collect all results. Use roles (researcher, coder, writer) to specialize the child. Use `context` to share background info, `toolsets` to narrow the child's tool surface, and `role_kind: orchestrator` to permit further delegation (subject to depth and the operator kill switch).",
            json!({
                "type": "object",
                "properties": {
                    "message": {
                        "type": "string",
                        "description": "The task/message to send to the sub-agent. Required unless 'tasks' is provided."
                    },
                    "context": {
                        "type": "string",
                        "description": "Background information injected into the child's preamble (file paths, prior findings, constraints). Kept separate from `message` so the child knows what it should know vs. what it should do."
                    },
                    "role": {
                        "type": "string",
                        "description": "Role name (researcher, coder, writer, or custom). Determines model, temperature, and allowed tools."
                    },
                    "role_kind": {
                        "type": "string",
                        "enum": ["leaf", "orchestrator"],
                        "description": "Delegation kind overlay. `leaf` (default) cannot re-delegate. `orchestrator` may spawn grandchildren when depth + the `agents.orchestrator_enabled` kill switch permit."
                    },
                    "toolsets": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "Optional explicit allowlist of tool names this child may call. Intersected with the role's allowlist (if any) and with the parent's enabled tools minus the delegate blocklist. Empty array = no tools."
                    },
                    "nickname": {
                        "type": "string",
                        "description": "Optional friendly name for the agent"
                    },
                    "model": {
                        "type": "string",
                        "description": "Override the model for this agent"
                    },
                    "thinking": {
                        "type": "string",
                        "enum": ["off", "minimal", "low", "medium", "high", "xhigh", "adaptive", "max"],
                        "description": "Override the thinking/reasoning effort for this agent (default: inherit). Use low for mechanical subtasks, high+ for hard analysis."
                    },
                    "fork_context": {
                        "type": "boolean",
                        "description": "If true, copies current conversation history to the sub-agent"
                    },
                    "blocking": {
                        "type": "boolean",
                        "description": "If true, wait for the child to finish and return its result in this tool call (saves a round-trip vs spawn+wait_for_agent). Default: false."
                    },
                    "timeout_secs": {
                        "type": "integer",
                        "description": "Timeout for blocking mode or batch mode. Defaults to the configured agents.delegate_timeout_secs."
                    },
                    "tasks": {
                        "type": "array",
                        "description": "Batch mode: list of independent tasks to fan out in parallel. When set, 'message' is ignored and the call always blocks until all children finish.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "goal": {"type": "string"},
                                "context": {"type": "string"},
                                "role": {"type": "string"},
                                "role_kind": {"type": "string", "enum": ["leaf", "orchestrator"]},
                                "toolsets": {"type": "array", "items": {"type": "string"}},
                                "model": {"type": "string"},
                                "thinking": {"type": "string", "enum": ["off", "minimal", "low", "medium", "high", "xhigh", "adaptive", "max"]}
                            },
                            "required": ["goal"]
                        }
                    }
                }
            }),
        ));

        // NOTE: send_to_agent is intentionally excluded — the receiving end in
        // run_sub_agent silently drops messages. Re-add when properly implemented.

        defs.push(ToolDefinition::new(
            "wait_for_agent",
            "Wait for a sub-agent to complete and return its result. Blocks until the agent finishes or timeout is reached.",
            json!({
                "type": "object",
                "properties": {
                    "agent_id": {
                        "type": "string",
                        "description": "The agent ID to wait for"
                    },
                    "timeout_secs": {
                        "type": "integer",
                        "description": "Timeout in seconds (default: 300)",
                        "default": 300
                    }
                },
                "required": ["agent_id"]
            }),
        ));

        defs.push(ToolDefinition::new(
            "close_agent",
            "Shut down a sub-agent, cancelling any in-progress work.",
            json!({
                "type": "object",
                "properties": {
                    "agent_id": {
                        "type": "string",
                        "description": "The agent ID to shut down"
                    }
                },
                "required": ["agent_id"]
            }),
        ));
    }

    defs
}

/// Compose the effective child-tool blocklist for a delegated sub-agent.
///
/// Starts from `DELEGATE_DEFAULT_BLOCKLIST` (delegate/memory/user-input
/// tools), then:
/// - If `role_kind == Orchestrator` and `can_orchestrate` is true, removes
///   the re-delegation tools (`spawn_agent`/`wait_for_agent`/`close_agent`)
///   so the child can fan out further. `can_orchestrate` is computed at the
///   call site as `agents.orchestrator_enabled && depth+1 < max_spawn_depth`
///   — the kill switch and the numeric cap both have to permit it.
/// - In Plan collaboration mode, additionally unions every mutating tool so
///   the child inherits the parent's read-only guarantee.
pub fn assemble_delegate_blocklist(
    parent_config: &crate::config::Config,
    role_kind: AgentRoleKind,
    can_orchestrate: bool,
) -> Vec<&'static str> {
    let mut blocklist: Vec<&'static str> = AgentControl::DELEGATE_DEFAULT_BLOCKLIST.to_vec();

    if role_kind == AgentRoleKind::Orchestrator && can_orchestrate {
        blocklist.retain(|t| !matches!(*t, "spawn_agent" | "wait_for_agent" | "close_agent"));
    }

    if parent_config
        .conversation
        .collaboration_mode
        .blocks_mutations()
    {
        // Unconditionally-mutating tools plus the args-dependent ones
        // (`computer_use` etc.): a static name-level blocklist can't express
        // per-action nuance, so children get the whole conditionally-mutating
        // tool blocked — the conservative reading of the Plan-mode guarantee.
        // Both lists live in tool_classification so the parent per-call gate
        // and this child blocklist can't drift.
        for tool in crate::agent::mutating_tool_names()
            .iter()
            .chain(crate::agent::conditionally_mutating_tool_names())
        {
            if !blocklist.contains(tool) {
                blocklist.push(tool);
            }
        }
    }
    blocklist
}

/// Returns true iff this controller is permitted to spawn an orchestrator
/// child at its current depth: both the operator kill switch
/// (`agents.orchestrator_enabled`) and the numeric depth cap
/// (`agents.max_spawn_depth`) must allow it.
pub fn can_orchestrate(parent_config: &crate::config::Config, current_depth: u32) -> bool {
    parent_config.agents.orchestrator_enabled
        && current_depth + 1 < parent_config.agents.max_spawn_depth
}

/// Handle the spawn_agent tool call.
///
/// Dispatches to one of three modes:
/// - **Batch** (`tasks` array present): fan out N children in parallel, wait
///   for all, return an ordered `results` array.
/// - **Blocking** (`blocking: true`): spawn one child, wait for it, and
///   return its result in the same tool call.
/// - **Fire-and-forget** (default): spawn and return an `agent_id` the
///   parent can wait on later via `wait_for_agent`.
///
/// In all modes the child's tool set is filtered through
/// [`AgentControl::DELEGATE_DEFAULT_BLOCKLIST`] so children can't delegate
/// further, prompt the user, or mutate long-term memory behind the parent's
/// back. When the parent is in Plan collaboration mode, the blocklist is
/// also unioned with every mutating tool so children inherit the same
/// read-only guarantee.
pub async fn handle_spawn_agent(
    args: &serde_json::Value,
    agent_control: &mut AgentControl,
    parent_config: &crate::config::Config,
    parent_history: Option<&[crate::types::Message]>,
) -> Result<String> {
    let role_name = args["role"].as_str();
    let role_kind = args["role_kind"]
        .as_str()
        .map(AgentRoleKind::parse)
        .unwrap_or_default();
    let toolsets_top = parse_string_array(args.get("toolsets"));
    let context_top = args["context"].as_str();
    let nickname = args["nickname"].as_str();
    let model_override = args["model"].as_str();
    let thinking_override = parse_thinking_arg(args.get("thinking"))?;
    let fork_context = args["fork_context"].as_bool().unwrap_or(false);
    let blocking = args["blocking"].as_bool().unwrap_or(false);
    let timeout_secs = args["timeout_secs"]
        .as_u64()
        .unwrap_or(parent_config.agents.delegate_timeout_secs);
    let batch_tasks = args.get("tasks").and_then(|v| v.as_array());

    // Batch mode — wins over single-task inputs. `blocking` is ignored here
    // (batch always blocks until every child finishes).
    if let Some(tasks_json) = batch_tasks {
        if tasks_json.is_empty() {
            anyhow::bail!(
                "tasks array must be non-empty (got []). Use 'message' for a single task."
            );
        }
        if blocking {
            tracing::debug!("spawn_agent: 'blocking' ignored; batch mode always blocks");
        }
        // For batch mode, every task in the batch shares the same kill-switch
        // posture (orchestrator gating is depth- and config-driven), so we
        // resolve the blocklist for *each* task by its own role_kind below.
        let mut tasks: Vec<super::DelegatedTask> = Vec::with_capacity(tasks_json.len());
        for (i, t) in tasks_json.iter().enumerate() {
            let goal = t
                .get("goal")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("tasks[{i}] missing required 'goal' string"))?
                .to_string();
            let per_task_kind = t
                .get("role_kind")
                .and_then(|v| v.as_str())
                .map(AgentRoleKind::parse)
                .unwrap_or(role_kind);
            tasks.push(super::DelegatedTask {
                goal,
                context: t
                    .get("context")
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
                    .or_else(|| context_top.map(str::to_string)),
                role_name: t.get("role").and_then(|v| v.as_str()).map(str::to_string),
                role_kind: per_task_kind,
                toolsets: parse_string_array(t.get("toolsets")).or_else(|| toolsets_top.clone()),
                model_override: t.get("model").and_then(|v| v.as_str()).map(str::to_string),
                thinking_override: parse_thinking_arg(t.get("thinking"))?
                    .or_else(|| thinking_override.clone()),
            });
        }

        // `spawn_batch_and_wait` computes the blocklist per task using
        // each task's own `role_kind`. No batch-wide blocklist — that
        // pattern let a leaf task inherit a sibling orchestrator's
        // carve-out (see the mixed-batch regression test).
        let completions = agent_control
            .spawn_batch_and_wait(tasks, parent_config, timeout_secs)
            .await?;
        // Divide the summary budget across the batch so N children can't
        // collectively blow the parent's context in one tool result.
        let per_child = super::per_child_summary_cap(completions.len());
        let results: Vec<serde_json::Value> = completions
            .iter()
            .map(|c| {
                let mut obj = json!({
                    "agent_id": c.agent_id,
                    "nickname": c.nickname,
                    "status": c.status.as_str(),
                    "result": c
                        .final_response
                        .as_deref()
                        .map(|s| super::bound_sub_agent_summary(s, per_child)),
                    "files_written": c.files_written,
                });
                attach_error_detail(&mut obj, c.error_detail());
                attach_undelivered_input(&mut obj, &c.undelivered_input);
                obj
            })
            .collect();
        return Ok(json!({
            "mode": "batch",
            "count": results.len(),
            "results": results,
        })
        .to_string());
    }

    let message = require_str_param(args, "message")?;
    let role = match role_name {
        Some(name) => roles::load_role(name).await,
        None => None,
    };
    let fork = if fork_context { parent_history } else { None };

    let can_orchestrate_now = can_orchestrate(parent_config, agent_control.current_depth());
    let blocklist = assemble_delegate_blocklist(parent_config, role_kind, can_orchestrate_now);

    if blocking {
        let completion = agent_control
            .spawn_and_wait(
                message,
                context_top,
                role,
                role_kind,
                toolsets_top.as_deref(),
                nickname,
                model_override,
                thinking_override,
                parent_config,
                fork,
                Some(&blocklist),
                timeout_secs,
            )
            .await?;
        let mut obj = json!({
            "mode": "blocking",
            "agent_id": completion.agent_id,
            "nickname": completion.nickname,
            "status": completion.status.as_str(),
            "result": completion
                .final_response
                .as_deref()
                .map(|s| super::bound_sub_agent_summary(s, super::SUB_AGENT_SUMMARY_MAX_CHARS)),
            "files_written": completion.files_written,
        });
        attach_error_detail(&mut obj, completion.error_detail());
        attach_undelivered_input(&mut obj, &completion.undelivered_input);
        return Ok(obj.to_string());
    }

    // Fire-and-forget (default, backward-compatible).
    let (agent_id, chosen_nickname) = agent_control
        .spawn_agent(
            message,
            context_top,
            role,
            role_kind,
            toolsets_top.as_deref(),
            nickname,
            model_override,
            thinking_override,
            parent_config,
            fork,
            Some(&blocklist),
        )
        .await?;

    Ok(json!({
        "mode": "spawned",
        "agent_id": agent_id,
        "nickname": chosen_nickname,
        "status": "spawned"
    })
    .to_string())
}

/// Parse an optional `thinking` arg into a [`ThinkingLevel`]. Absent/null
/// means "inherit from parent". An unknown level is a hard error — silently
/// mapping a typo to Off would run the child at the wrong effort with no
/// signal to the caller.
fn parse_thinking_arg(
    v: Option<&serde_json::Value>,
) -> Result<Option<crate::config::ThinkingLevel>> {
    let Some(v) = v.filter(|v| !v.is_null()) else {
        return Ok(None);
    };
    serde_json::from_value(v.clone()).map(Some).map_err(|_| {
        anyhow::anyhow!(
            "invalid 'thinking' value {v}: expected one of off, minimal, low, medium, \
             high, xhigh, adaptive, max"
        )
    })
}

/// Parse a JSON array of strings out of a tool-call argument value.
/// Returns `None` if the value is absent or not an array; returns
/// `Some(Vec::new())` for an explicit empty array (caller-distinguishable
/// from "unset", since an empty `toolsets` means "no tools"). Non-string
/// entries are dropped with a `tracing::warn!` so malformed-array bugs
/// surface in logs rather than silently truncating the allowlist.
fn parse_string_array(v: Option<&serde_json::Value>) -> Option<Vec<String>> {
    let arr = v?.as_array()?;
    let mut out = Vec::with_capacity(arr.len());
    let mut dropped = 0usize;
    for x in arr {
        match x.as_str() {
            Some(s) => out.push(s.to_string()),
            None => dropped += 1,
        }
    }
    if dropped > 0 {
        tracing::warn!(
            dropped,
            "spawn_agent: parse_string_array dropped {dropped} non-string element(s)"
        );
    }
    Some(out)
}

/// Handle the send_to_agent tool call.
pub async fn handle_send_to_agent(
    args: &serde_json::Value,
    agent_control: &AgentControl,
) -> Result<String> {
    let agent_id = require_str_param(args, "agent_id")?;
    let message = require_str_param(args, "message")?;
    agent_control.send_input(agent_id, message)?;
    // "queued", not "sent": the child absorbs steers at its next tool boundary,
    // so a child that finishes first never sees this. Claiming delivery here
    // would be the lie the old no-op delivery path told. The undelivered case
    // surfaces on the completion record.
    Ok(json!({
        "status": "queued",
        "note": "The sub-agent will pick this up at its next tool boundary. \
                 If it finishes first, the message is reported back as \
                 undelivered_input on its completion."
    })
    .to_string())
}

/// Handle the wait_for_agent tool call.
pub async fn handle_wait_for_agent(
    args: &serde_json::Value,
    agent_control: &mut AgentControl,
) -> Result<String> {
    let agent_id = require_str_param(args, "agent_id")?;
    let timeout_secs = args["timeout_secs"]
        .as_u64()
        .unwrap_or(agent_control.delegate_timeout_secs);

    let completion = agent_control.wait_for_agent(agent_id, timeout_secs).await?;

    let mut obj = json!({
        "agent_id": completion.agent_id,
        "nickname": completion.nickname,
        "status": completion.status.as_str(),
        "result": completion
            .final_response
            .as_deref()
            .map(|s| super::bound_sub_agent_summary(s, super::SUB_AGENT_SUMMARY_MAX_CHARS)),
    });
    attach_error_detail(&mut obj, completion.error_detail());
    attach_undelivered_input(&mut obj, &completion.undelivered_input);
    Ok(obj.to_string())
}

/// Fold a sub-agent's terminal-error detail (bounded error text + recovery
/// hint, from [`crate::multi_agent::SubAgentCompletion::error_detail`]) into the
/// JSON object surfaced to the parent. No-op for non-error completions. Shared
/// by all three delegate output paths so the failure shape stays consistent.
fn attach_error_detail(obj: &mut serde_json::Value, detail: Option<(String, &'static str)>) {
    if let Some((error, next_action)) = detail {
        obj["error"] = json!(error);
        obj["next_action"] = json!(next_action);
    }
}

/// Fold any parent-queued message the child never absorbed into the JSON
/// surfaced to the parent. Absent when everything landed, so the common case
/// costs no context.
///
/// Without this a `send_to_agent` racing the child's final response would
/// report `queued` and then a clean `completed`, leaving the parent to assume
/// its guidance shaped the result when it never arrived.
fn attach_undelivered_input(obj: &mut serde_json::Value, undelivered: &[String]) {
    if undelivered.is_empty() {
        return;
    }
    obj["undelivered_input"] = json!(undelivered);
    obj["undelivered_input_note"] = json!(
        "The sub-agent finished before absorbing these queued messages — its \
         result does NOT reflect them. Re-delegate if the guidance still applies."
    );
}

/// Handle the list_agents tool call.
pub fn handle_list_agents(agent_control: &AgentControl) -> Result<String> {
    let agents = agent_control.list_agents();
    if agents.is_empty() {
        return Ok("No active sub-agents.".to_string());
    }

    let now = chrono::Utc::now().timestamp();
    let list: Vec<serde_json::Value> = agents
        .iter()
        .map(|a| {
            let mut entry = json!({
                "id": a.id,
                "nickname": a.nickname,
                "role": a.role,
                "depth": a.depth,
                "status": a.status.as_str(),
                "created_at": a.created_at,
            });
            // Live activity, present only while it says something useful:
            // lets the parent distinguish "grinding through a long tool
            // call" from "wedged since spawn" without waiting on the child.
            if let Some(tool) = &a.current_tool {
                entry["current_tool"] = json!(tool);
            }
            if let Some(ts) = a.last_activity_at {
                entry["seconds_since_activity"] = json!(now.saturating_sub(ts).max(0));
            }
            entry
        })
        .collect();

    Ok(serde_json::to_string_pretty(&list)?)
}

/// Handle the close_agent tool call.
pub async fn handle_close_agent(
    args: &serde_json::Value,
    agent_control: &mut AgentControl,
) -> Result<String> {
    let agent_id = require_str_param(args, "agent_id")?;
    agent_control.shutdown_agent(agent_id).await?;
    Ok(json!({"status": "shutdown"}).to_string())
}

/// Handle the manage_roles tool call.
pub async fn handle_manage_roles(args: &serde_json::Value) -> Result<String> {
    let action = require_str_param(args, "action")?;

    match action {
        "list" => {
            let all_roles = roles::list_all_roles().await;
            let list: Vec<serde_json::Value> = all_roles
                .iter()
                .map(|r| {
                    json!({
                        "name": r.name,
                        "description": r.description,
                        "model": r.model,
                        "provider": r.provider,
                        "temperature": r.temperature,
                        "tools_allowed": r.tools_allowed,
                        "max_iterations": r.max_iterations,
                    })
                })
                .collect();
            Ok(serde_json::to_string_pretty(&list)?)
        }
        "create" => {
            let name = require_str_param(args, "name")?;
            let description = require_str_param(args, "description")?;
            let model = args["model"].as_str();
            let provider = args["provider"].as_str();
            let temperature = args["temperature"].as_f64().map(|v| v as f32);
            let system_instructions = args["system_instructions"].as_str();
            let tools_json = args.get("tools_allowed").and_then(|v| {
                if v.is_array() {
                    serde_json::to_string(v).ok()
                } else {
                    None
                }
            });
            let max_iterations = args["max_iterations"].as_u64().map(|v| v as i64);

            let db = crate::db::Database::open().await?;
            db.insert_role(
                name,
                description,
                model,
                provider,
                temperature,
                system_instructions,
                tools_json.as_deref(),
                max_iterations,
                false,
            )
            .await?;
            Ok(json!({"status": "created", "name": name}).to_string())
        }
        "update" => {
            let name = require_str_param(args, "name")?;
            let db = crate::db::Database::open().await?;
            if db.get_role(name).await?.is_none() {
                return Ok(json!({"error": format!("Role '{name}' not found")}).to_string());
            }
            let description = args["description"].as_str();
            let model = args["model"].as_str();
            let provider = args["provider"].as_str();
            let temperature = args["temperature"].as_f64().map(|v| v as f32);
            let system_instructions = args["system_instructions"].as_str();
            let tools_json = args.get("tools_allowed").and_then(|v| {
                if v.is_array() {
                    serde_json::to_string(v).ok()
                } else {
                    None
                }
            });
            let max_iterations = args["max_iterations"].as_u64().map(|v| v as i64);

            db.update_role(
                name,
                description,
                model,
                provider,
                temperature,
                system_instructions,
                tools_json.as_deref(),
                max_iterations,
            )
            .await?;
            Ok(json!({"status": "updated", "name": name}).to_string())
        }
        "delete" => {
            let name = require_str_param(args, "name")?;
            let db = crate::db::Database::open().await?;
            db.delete_role(name).await?;
            Ok(json!({"status": "deleted", "name": name}).to_string())
        }
        _ => Ok(json!({"error": format!("Unknown action: {action}")}).to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `spawn_agent` schema is what the LLM sees every turn. The
    /// caller-surface parameters (`context`, `toolsets`, `role_kind`) must
    /// appear in `properties` so the model knows it can pass them. A
    /// regression that drops one silently downgrades the tool to the
    /// minimal pre-surface behavior with no error.
    #[test]
    fn spawn_agent_schema_advertises_caller_surface_params() {
        let defs = tool_definitions(0, 3);
        let spawn = defs
            .iter()
            .find(|d| d.function.name == "spawn_agent")
            .expect("spawn_agent must be present at depth 0");
        let props = &spawn.function.parameters["properties"];
        for required in ["context", "toolsets", "role_kind"] {
            assert!(
                props.get(required).is_some(),
                "spawn_agent schema must advertise '{required}'"
            );
        }
        // role_kind must be an enum, not an open string — keeps the LLM
        // from inventing "manager", "supervisor", etc.
        let role_kind = &props["role_kind"];
        let enum_vals = role_kind
            .get("enum")
            .and_then(|v| v.as_array())
            .expect("role_kind must declare an enum");
        let names: Vec<&str> = enum_vals.iter().filter_map(|v| v.as_str()).collect();
        assert!(
            names.contains(&"leaf") && names.contains(&"orchestrator"),
            "role_kind enum must include leaf + orchestrator, got {names:?}"
        );
    }

    /// Batch-mode `tasks[]` elements must accept the same per-task
    /// caller-surface fields. Without per-task `toolsets` and `context`
    /// the LLM can't differentiate sibling tasks in a fan-out — they all
    /// share the top-level context, which defeats the point of batch mode.
    #[test]
    fn spawn_agent_schema_per_task_fields() {
        let defs = tool_definitions(0, 3);
        let spawn = defs
            .iter()
            .find(|d| d.function.name == "spawn_agent")
            .unwrap();
        let item_props = &spawn.function.parameters["properties"]["tasks"]["items"]["properties"];
        for required in ["goal", "context", "role", "role_kind", "toolsets", "model"] {
            assert!(
                item_props.get(required).is_some(),
                "tasks[] item must allow '{required}'"
            );
        }
    }

    /// At max depth the multi-agent tool surface must disappear entirely.
    /// If a future refactor leaks any of the four through, a leaf at the
    /// cap could re-delegate without going through `can_orchestrate`.
    #[test]
    fn tool_definitions_at_max_depth_strips_delegation_surface() {
        let defs = tool_definitions(2, 2);
        let names: Vec<&str> = defs.iter().map(|d| d.function.name.as_str()).collect();
        for forbidden in ["spawn_agent", "wait_for_agent", "close_agent"] {
            assert!(
                !names.contains(&forbidden),
                "{forbidden} must be absent at max depth, got {names:?}"
            );
        }
        // `manage_roles` is depth-independent (metadata, not delegation).
        assert!(names.contains(&"manage_roles"));
    }

    /// `thinking` arg contract: absent/null inherits (None), valid levels
    /// parse, and a typo is a hard error — NOT a silent downgrade to Off,
    /// which would run the child at the wrong effort with no signal.
    #[test]
    fn parse_thinking_arg_inherits_parses_and_rejects() {
        use crate::config::ThinkingLevel;
        assert_eq!(parse_thinking_arg(None).unwrap(), None);
        assert_eq!(
            parse_thinking_arg(Some(&serde_json::Value::Null)).unwrap(),
            None,
            "JSON null must mean inherit, not error"
        );
        assert_eq!(
            parse_thinking_arg(Some(&serde_json::json!("minimal"))).unwrap(),
            Some(ThinkingLevel::Minimal)
        );
        let err = parse_thinking_arg(Some(&serde_json::json!("ultra")))
            .expect_err("unknown level must be a hard error");
        assert!(
            err.to_string().contains("minimal"),
            "error must list the valid levels so the model can self-correct: {err}"
        );
    }

    /// `parse_string_array` distinguishes Absent (None) from Empty
    /// (Some(empty)). The handler relies on this distinction for
    /// `toolsets: []` to mean "no tools" rather than "inherit".
    #[test]
    fn parse_string_array_distinguishes_absent_from_empty() {
        assert!(parse_string_array(None).is_none());
        assert!(
            parse_string_array(Some(&serde_json::Value::Null)).is_none(),
            "JSON null must yield None"
        );

        let empty = serde_json::json!([]);
        assert_eq!(
            parse_string_array(Some(&empty)),
            Some(Vec::<String>::new()),
            "[] must yield Some(empty), not None"
        );

        let filled = serde_json::json!(["a", "b"]);
        assert_eq!(
            parse_string_array(Some(&filled)),
            Some(vec!["a".to_string(), "b".to_string()])
        );

        // Mixed-type arrays drop non-string entries defensively.
        let mixed = serde_json::json!(["a", 42, "b"]);
        assert_eq!(
            parse_string_array(Some(&mixed)),
            Some(vec!["a".to_string(), "b".to_string()])
        );
    }

    /// Batch entry missing `goal` must error with the offending index
    /// named. Silent skip would produce a results array shorter than the
    /// input — unreconcilable for the calling LLM.
    #[tokio::test]
    async fn handle_spawn_agent_batch_rejects_missing_goal() {
        let cfg = crate::config::MultiAgentConfig::default();
        let mut ctrl = AgentControl::new(&cfg, "session-1", 0);
        let parent_config = crate::config::Config::default();
        let args = json!({"tasks": [{"role": "researcher"}]});
        let err = handle_spawn_agent(&args, &mut ctrl, &parent_config, None)
            .await
            .expect_err("batch with missing goal must error");
        let msg = err.to_string();
        assert!(msg.contains("goal"), "error must name 'goal': {msg}");
        assert!(msg.contains("tasks[0]"), "error must name index: {msg}");
    }

    /// Empty `tasks: []` is a user/LLM error, not a no-op. Catching it
    /// surfaces a clear message instead of silently returning
    /// `{count: 0, results: []}` (which the LLM treats as "succeeded").
    #[tokio::test]
    async fn handle_spawn_agent_batch_rejects_empty_array() {
        let cfg = crate::config::MultiAgentConfig::default();
        let mut ctrl = AgentControl::new(&cfg, "session-1", 0);
        let parent_config = crate::config::Config::default();
        let args = json!({"tasks": []});
        let err = handle_spawn_agent(&args, &mut ctrl, &parent_config, None)
            .await
            .expect_err("empty tasks must error");
        assert!(err.to_string().contains("non-empty"));
    }

    /// Outside Plan mode, the leaf blocklist contains the default set
    /// (memory mutation, user-input, delegation surface) and does NOT
    /// block mutating tools the agent normally calls.
    #[test]
    fn assemble_delegate_blocklist_default_set_outside_plan_mode() {
        let cfg = crate::config::Config::default();
        let blocklist = assemble_delegate_blocklist(&cfg, AgentRoleKind::Leaf, false);
        for required in [
            "spawn_agent",
            "wait_for_agent",
            "close_agent",
            "write_memory",
            "request_user_input",
        ] {
            assert!(
                blocklist.contains(&required),
                "leaf blocklist must contain '{required}': {blocklist:?}"
            );
        }
        assert!(
            !blocklist.contains(&"run_shell"),
            "run_shell must NOT be blocked outside Plan mode"
        );
    }
}
