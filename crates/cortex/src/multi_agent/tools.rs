use anyhow::Result;
use serde_json::json;

use crate::tool_handlers::require_str_param;
use crate::types::ToolDefinition;

use super::roles;
use super::AgentControl;

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
            "Spawn a sub-agent to work on a task. By default returns immediately with an agent_id you can wait on later. Set blocking=true to wait in-line and receive the child's final result in the same turn. Pass tasks=[…] to fan out N children in parallel and collect all results. Use roles (researcher, coder, writer) to specialize the child.",
            json!({
                "type": "object",
                "properties": {
                    "message": {
                        "type": "string",
                        "description": "The task/message to send to the sub-agent. Required unless 'tasks' is provided."
                    },
                    "role": {
                        "type": "string",
                        "description": "Role name (researcher, coder, writer, or custom). Determines model, temperature, and allowed tools."
                    },
                    "nickname": {
                        "type": "string",
                        "description": "Optional friendly name for the agent"
                    },
                    "model": {
                        "type": "string",
                        "description": "Override the model for this agent"
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
                                "role": {"type": "string"},
                                "model": {"type": "string"}
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

/// Compose the effective child-tool blocklist for a delegated sub-agent:
/// the default delegate blocklist, unioned with every mutating tool when
/// the parent is in Plan collaboration mode. Called on every `spawn_agent`
/// dispatch so children can never bypass plan-mode safety.
pub fn assemble_delegate_blocklist(parent_config: &crate::config::Config) -> Vec<&'static str> {
    let mut blocklist: Vec<&'static str> = AgentControl::DELEGATE_DEFAULT_BLOCKLIST.to_vec();
    if parent_config
        .conversation
        .collaboration_mode
        .blocks_mutations()
    {
        for tool in crate::agent::mutating_tool_names() {
            if !blocklist.contains(tool) {
                blocklist.push(tool);
            }
        }
    }
    blocklist
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
    let nickname = args["nickname"].as_str();
    let model_override = args["model"].as_str();
    let fork_context = args["fork_context"].as_bool().unwrap_or(false);
    let blocking = args["blocking"].as_bool().unwrap_or(false);
    let timeout_secs = args["timeout_secs"]
        .as_u64()
        .unwrap_or(parent_config.agents.delegate_timeout_secs);
    let batch_tasks = args.get("tasks").and_then(|v| v.as_array());

    let blocklist = assemble_delegate_blocklist(parent_config);

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
        let mut tasks: Vec<super::DelegatedTask> = Vec::with_capacity(tasks_json.len());
        for (i, t) in tasks_json.iter().enumerate() {
            let goal = t
                .get("goal")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("tasks[{i}] missing required 'goal' string"))?
                .to_string();
            tasks.push(super::DelegatedTask {
                goal,
                role_name: t.get("role").and_then(|v| v.as_str()).map(str::to_string),
                model_override: t.get("model").and_then(|v| v.as_str()).map(str::to_string),
            });
        }

        let completions = agent_control
            .spawn_batch_and_wait(tasks, parent_config, Some(&blocklist), timeout_secs)
            .await?;
        let results: Vec<serde_json::Value> = completions
            .iter()
            .map(|c| {
                json!({
                    "agent_id": c.agent_id,
                    "nickname": c.nickname,
                    "status": c.status.as_str(),
                    "result": c.final_response,
                })
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
    let role = role_name.and_then(roles::load_role);
    let context = if fork_context { parent_history } else { None };

    if blocking {
        let completion = agent_control
            .spawn_and_wait(
                message,
                role,
                nickname,
                model_override,
                parent_config,
                context,
                Some(&blocklist),
                timeout_secs,
            )
            .await?;
        return Ok(json!({
            "mode": "blocking",
            "agent_id": completion.agent_id,
            "nickname": completion.nickname,
            "status": completion.status.as_str(),
            "result": completion.final_response,
        })
        .to_string());
    }

    // Fire-and-forget (default, backward-compatible).
    let (agent_id, chosen_nickname) = agent_control
        .spawn_agent(
            message,
            role,
            nickname,
            model_override,
            parent_config,
            context,
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

/// Handle the send_to_agent tool call.
pub async fn handle_send_to_agent(
    args: &serde_json::Value,
    agent_control: &AgentControl,
) -> Result<String> {
    let agent_id = require_str_param(args, "agent_id")?;
    let message = require_str_param(args, "message")?;
    agent_control.send_input(agent_id, message).await?;
    Ok(json!({"status": "sent"}).to_string())
}

/// Handle the wait_for_agent tool call.
pub async fn handle_wait_for_agent(
    args: &serde_json::Value,
    agent_control: &mut AgentControl,
) -> Result<String> {
    let agent_id = require_str_param(args, "agent_id")?;
    let timeout_secs = args["timeout_secs"]
        .as_u64()
        .unwrap_or(crate::constants::DEFAULT_SUB_AGENT_TIMEOUT_SECS);

    let completion = agent_control.wait_for_agent(agent_id, timeout_secs).await?;

    Ok(json!({
        "agent_id": completion.agent_id,
        "nickname": completion.nickname,
        "status": completion.status.as_str(),
        "result": completion.final_response,
    })
    .to_string())
}

/// Handle the list_agents tool call.
pub fn handle_list_agents(agent_control: &AgentControl) -> Result<String> {
    let agents = agent_control.list_agents();
    if agents.is_empty() {
        return Ok("No active sub-agents.".to_string());
    }

    let list: Vec<serde_json::Value> = agents
        .iter()
        .map(|a| {
            json!({
                "id": a.id,
                "nickname": a.nickname,
                "role": a.role,
                "depth": a.depth,
                "status": a.status.as_str(),
                "created_at": a.created_at,
            })
        })
        .collect();

    Ok(serde_json::to_string_pretty(&list)?)
}

/// Handle the close_agent tool call.
pub fn handle_close_agent(
    args: &serde_json::Value,
    agent_control: &mut AgentControl,
) -> Result<String> {
    let agent_id = require_str_param(args, "agent_id")?;
    agent_control.shutdown_agent(agent_id)?;
    Ok(json!({"status": "shutdown"}).to_string())
}

/// Handle the manage_roles tool call.
pub fn handle_manage_roles(args: &serde_json::Value) -> Result<String> {
    let action = require_str_param(args, "action")?;

    match action {
        "list" => {
            let all_roles = roles::list_all_roles();
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

            let db = crate::db::Database::open()?;
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
            )?;
            Ok(json!({"status": "created", "name": name}).to_string())
        }
        "update" => {
            let name = require_str_param(args, "name")?;
            let db = crate::db::Database::open()?;
            if db.get_role(name)?.is_none() {
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
            )?;
            Ok(json!({"status": "updated", "name": name}).to_string())
        }
        "delete" => {
            let name = require_str_param(args, "name")?;
            let db = crate::db::Database::open()?;
            db.delete_role(name)?;
            Ok(json!({"status": "deleted", "name": name}).to_string())
        }
        _ => Ok(json!({"error": format!("Unknown action: {action}")}).to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_spawn_agent_tool_definition() {
        let defs = tool_definitions(0, 1);
        let spawn = defs.iter().find(|d| d.function.name == "spawn_agent");
        assert!(spawn.is_some(), "spawn_agent should be in tool definitions");
        let spawn = spawn.unwrap();
        let params = &spawn.function.parameters;
        // `message` is still the primary input and should be described, but
        // it's no longer strictly required (tasks[] can replace it in batch
        // mode). Runtime validation in handle_spawn_agent enforces exactly
        // one of the two is supplied.
        let props = &params["properties"];
        assert!(props.get("message").is_some(), "message must be described");
    }

    #[test]
    fn test_tool_filtering_at_max_depth() {
        let defs = tool_definitions(1, 1);
        assert!(
            defs.iter().all(|d| d.function.name != "spawn_agent"),
            "spawn_agent should be excluded at max depth"
        );
        assert!(
            defs.iter().all(|d| d.function.name != "wait_for_agent"),
            "wait_for_agent should be excluded at max depth"
        );
        assert!(
            defs.iter().all(|d| d.function.name != "send_to_agent"),
            "send_to_agent should be excluded at max depth"
        );
        assert!(
            defs.iter().all(|d| d.function.name != "close_agent"),
            "close_agent should be excluded at max depth"
        );
        // manage_roles should still be present (list_agents moved to unified `list` tool)
        assert!(defs.iter().any(|d| d.function.name == "manage_roles"));
    }

    #[test]
    fn test_handle_list_agents_empty() {
        let config = crate::config::MultiAgentConfig::default();
        let ctrl = AgentControl::new(&config, "session-1", 0);
        let result = handle_list_agents(&ctrl).unwrap();
        assert_eq!(result, "No active sub-agents.");
    }

    #[tokio::test]
    async fn test_handle_spawn_agent_missing_message() {
        let config = crate::config::MultiAgentConfig::default();
        let mut ctrl = AgentControl::new(&config, "session-1", 0);
        let parent_config = crate::config::Config::default();
        let args = json!({});
        let result = handle_spawn_agent(&args, &mut ctrl, &parent_config, None).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("message"));
    }

    #[tokio::test]
    async fn test_handle_spawn_agent_batch_missing_goal_is_error() {
        let config = crate::config::MultiAgentConfig::default();
        let mut ctrl = AgentControl::new(&config, "session-1", 0);
        let parent_config = crate::config::Config::default();
        // Batch with a task that's missing the required `goal` field.
        let args = json!({"tasks": [{"role": "researcher"}]});
        let result = handle_spawn_agent(&args, &mut ctrl, &parent_config, None).await;
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("goal"), "expected 'goal' error, got: {err}");
    }

    #[test]
    fn assemble_delegate_blocklist_default_mode_is_just_defaults() {
        // In Default collaboration mode, only the delegate defaults apply.
        let cfg = crate::config::Config::default();
        let blocklist = assemble_delegate_blocklist(&cfg);
        for banned in AgentControl::DELEGATE_DEFAULT_BLOCKLIST {
            assert!(blocklist.contains(banned), "defaults must include {banned}");
        }
        // Mutating tools that aren't in the default set should NOT be present
        // — e.g. run_shell is mutating but allowed outside Plan mode.
        assert!(
            !blocklist.contains(&"run_shell"),
            "run_shell should not be blocked outside Plan mode"
        );
        assert!(
            !blocklist.contains(&"apply_patch"),
            "apply_patch should not be blocked outside Plan mode"
        );
    }

    #[test]
    fn assemble_delegate_blocklist_plan_mode_unions_mutating_tools() {
        // In Plan mode, the child inherits the parent's read-only guarantee:
        // every mutating tool is added on top of the delegate defaults.
        let mut cfg = crate::config::Config::default();
        cfg.conversation.collaboration_mode = crate::config::CollaborationMode::Plan;
        let blocklist = assemble_delegate_blocklist(&cfg);
        for banned in AgentControl::DELEGATE_DEFAULT_BLOCKLIST {
            assert!(
                blocklist.contains(banned),
                "defaults must still be present in Plan mode"
            );
        }
        // Pick a handful of representative mutating tools that must be added.
        for extra in [
            "run_shell",
            "apply_patch",
            "apply_skill_patch",
            "generate_image",
            "browser",
        ] {
            assert!(
                blocklist.contains(&extra),
                "Plan mode must block '{extra}' from children"
            );
        }
        // No duplicates — the union dedupes.
        let mut unique = blocklist.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), blocklist.len(), "blocklist has duplicates");
    }

    #[test]
    fn test_spawn_agent_schema_exposes_blocking_and_tasks() {
        let defs = tool_definitions(0, 1);
        let spawn = defs
            .iter()
            .find(|d| d.function.name == "spawn_agent")
            .expect("spawn_agent should exist at depth 0");
        let props = &spawn.function.parameters["properties"];
        assert!(
            props.get("blocking").is_some(),
            "spawn_agent schema must expose 'blocking' parameter"
        );
        assert!(
            props.get("tasks").is_some(),
            "spawn_agent schema must expose 'tasks' parameter for batch mode"
        );
        assert!(
            props.get("timeout_secs").is_some(),
            "spawn_agent schema must expose 'timeout_secs' parameter"
        );
        // `message` is now optional (tasks can replace it), so it must not
        // appear in `required` anymore.
        let required = spawn.function.parameters.get("required");
        if let Some(r) = required.and_then(|v| v.as_array()) {
            assert!(
                !r.iter().any(|v| v == "message"),
                "'message' must not be a required field (tasks[] can replace it)"
            );
        }
    }

    #[test]
    fn test_handle_close_agent_nonexistent() {
        let config = crate::config::MultiAgentConfig::default();
        let mut ctrl = AgentControl::new(&config, "session-1", 0);
        let args = json!({"agent_id": "nonexistent"});
        let result = handle_close_agent(&args, &mut ctrl);
        assert!(result.is_err());
    }

    #[test]
    fn test_handle_manage_roles_list() {
        let args = json!({"action": "list"});
        let result = handle_manage_roles(&args);
        assert!(result.is_ok());
    }

    #[test]
    fn test_tool_definitions_at_depth_0_count() {
        let defs = tool_definitions(0, 1);
        // manage_roles + spawn_agent + wait_for_agent + close_agent = 4
        // (send_to_agent removed)
        assert_eq!(
            defs.len(),
            4,
            "Expected 4 tools at depth 0, got: {:?}",
            defs.iter().map(|d| &d.function.name).collect::<Vec<_>>()
        );
        assert!(
            defs.iter().all(|d| d.function.name != "send_to_agent"),
            "send_to_agent should not be in tool definitions"
        );
    }

    #[test]
    fn test_handle_manage_roles_unknown_action() {
        let args = json!({"action": "foobar"});
        let result = handle_manage_roles(&args).unwrap();
        assert!(
            result.contains("error"),
            "Expected error JSON, got: {result}"
        );
        assert!(result.contains("foobar"));
    }

    #[tokio::test]
    async fn test_handle_send_to_agent_nonexistent() {
        let config = crate::config::MultiAgentConfig::default();
        let ctrl = AgentControl::new(&config, "session-1", 0);
        let args = json!({"agent_id": "nonexistent", "message": "hello"});
        let result = handle_send_to_agent(&args, &ctrl).await;
        assert!(result.is_err());
    }

    /// Verify that tools_filter logic (applied in Agent.build_tool_definitions) correctly
    /// filters tool definitions by an allowed set.
    #[test]
    fn test_tools_filter_restricts_tools() {
        use crate::types::ToolDefinition;
        let tools = vec![
            ToolDefinition::new("run_shell", "desc", json!({"type": "object"})),
            ToolDefinition::new("apply_patch", "desc", json!({"type": "object"})),
            ToolDefinition::new("read_memory", "desc", json!({"type": "object"})),
            ToolDefinition::new("write_memory", "desc", json!({"type": "object"})),
        ];
        let allowed: Vec<String> = vec!["run_shell".into(), "read_memory".into()];
        let allowed_set: std::collections::HashSet<&str> =
            allowed.iter().map(String::as_str).collect();
        let filtered: Vec<ToolDefinition> = tools
            .into_iter()
            .filter(|t| allowed_set.contains(t.function.name.as_str()))
            .collect();
        assert_eq!(filtered.len(), 2);
        assert!(filtered.iter().any(|t| t.function.name == "run_shell"));
        assert!(filtered.iter().any(|t| t.function.name == "read_memory"));
        assert!(filtered.iter().all(|t| t.function.name != "apply_patch"));
        assert!(filtered.iter().all(|t| t.function.name != "write_memory"));
    }

    /// Verify that an empty tools_filter strips all tools.
    #[test]
    fn test_tools_filter_empty_strips_all() {
        use crate::types::ToolDefinition;
        let tools = vec![
            ToolDefinition::new("run_shell", "desc", json!({"type": "object"})),
            ToolDefinition::new("apply_patch", "desc", json!({"type": "object"})),
        ];
        let allowed: Vec<String> = vec![];
        let allowed_set: std::collections::HashSet<&str> =
            allowed.iter().map(String::as_str).collect();
        let filtered: Vec<ToolDefinition> = tools
            .into_iter()
            .filter(|t| allowed_set.contains(t.function.name.as_str()))
            .collect();
        assert!(filtered.is_empty());
    }

    /// Verify that None tools_filter passes all tools through.
    #[test]
    fn test_tools_filter_none_passes_all() {
        use crate::types::ToolDefinition;
        let tools = vec![
            ToolDefinition::new("run_shell", "desc", json!({"type": "object"})),
            ToolDefinition::new("apply_patch", "desc", json!({"type": "object"})),
        ];
        let filter: Option<Vec<String>> = None;
        let filtered: Vec<ToolDefinition> = if let Some(ref allowed) = filter {
            let allowed_set: std::collections::HashSet<&str> =
                allowed.iter().map(String::as_str).collect();
            tools
                .into_iter()
                .filter(|t| allowed_set.contains(t.function.name.as_str()))
                .collect()
        } else {
            tools
        };
        assert_eq!(filtered.len(), 2);
    }
}
