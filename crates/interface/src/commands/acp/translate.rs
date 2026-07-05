//! Pure translation from Borg agent events and persisted history into
//! ACP `session/update` frames. No I/O — every function here maps an
//! input value to an optional JSON payload.

use cortex::agent::AgentEvent;
use cortex::internal_tag_filter::strip_internal_tags;
use serde_json::{json, Value};

use super::HistoryMessage;

/// Map a Borg tool name to the ACP `tool_call` category. ACP clients (Zed,
/// etc.) render tool calls based on this `kind` field; without it they fall
/// back to a generic icon and label. Plan §"Tool lifecycle" specifies the
/// `run_shell → execute`, fs → `other` baseline; we extend it to the full
/// ACP `kind` enum (`read`, `edit`, `search`, `fetch`, `execute`, `other`)
/// so that read-only inspection, mutation, and network access render
/// distinctly. Unknown tools degrade to `"other"` rather than erroring.
pub(super) fn tool_kind_for(name: &str) -> &'static str {
    match name {
        "run_shell" => "execute",
        "read_file" | "read_memory" | "list_dir" | "list" => "read",
        "apply_patch" | "write_memory" => "edit",
        "memory_search" => "search",
        "web_fetch" | "web_search" | "browser" => "fetch",
        _ => "other",
    }
}

pub(super) fn event_to_update(event: &AgentEvent, session_id: &str) -> Option<Value> {
    match event {
        AgentEvent::TextDelta(text) => Some(json!({
            "sessionId": session_id,
            "update": {
                "kind": "agent_message_chunk",
                "text": strip_internal_tags(text),
            }
        })),
        AgentEvent::ThinkingDelta(text) => Some(json!({
            "sessionId": session_id,
            "update": {
                "kind": "agent_thought_chunk",
                "text": strip_internal_tags(text),
            }
        })),
        AgentEvent::ToolExecuting {
            tool_call_id,
            name,
            args,
        } => Some(json!({
            "sessionId": session_id,
            "update": {
                "kind": "tool_call",
                "toolCallId": tool_call_id,
                "name": name,
                "toolKind": tool_kind_for(name),
                "status": "running",
                "arguments": args,
            }
        })),
        AgentEvent::ToolOutputDelta {
            tool_call_id,
            name,
            delta,
            is_stderr,
        } => Some(json!({
            "sessionId": session_id,
            "update": {
                "kind": "tool_call_update",
                "toolCallId": tool_call_id,
                "name": name,
                "toolKind": tool_kind_for(name),
                "status": "running",
                "stream": if *is_stderr { "stderr" } else { "stdout" },
                "content": strip_internal_tags(delta),
            }
        })),
        AgentEvent::ToolResult {
            tool_call_id,
            name,
            result,
        } => Some(json!({
            "sessionId": session_id,
            "update": {
                "kind": "tool_call_update",
                "toolCallId": tool_call_id,
                "name": name,
                "toolKind": tool_kind_for(name),
                "status": "completed",
                "content": strip_internal_tags(result),
            }
        })),
        AgentEvent::PlanUpdated { steps } => Some(json!({
            "sessionId": session_id,
            "update": {
                "kind": "plan",
                "entries": steps.iter().enumerate().map(|(idx, step)| {
                    json!({
                        "id": idx.to_string(),
                        "title": step.title,
                        "status": step.status.as_wire_str(),
                        "priority": "medium"
                    })
                }).collect::<Vec<_>>()
            }
        })),
        _ => None,
    }
}

/// Translate a persisted `HistoryMessage` into the ACP `session/update`
/// frame the IDE expects on `session/load`.
///
/// Per ACP spec the replay update kinds are `user_message_chunk` (role
/// "user") and `agent_message_chunk` (role "assistant"); other roles
/// (e.g. internal "system" or "tool") are dropped because there's no
/// spec-blessed kind for them. Tool-call replay is a known follow-up:
/// the daemon's `MessageView` proto only carries `{role, text}` today,
/// so persisted tool calls and their results aren't surfaced to the
/// translator. Replaying tool calls/statuses requires extending
/// `MessageView` (and the underlying DB read in `borgd/src/grpc/admin.rs`)
/// to carry structured tool-event entries.
pub(super) fn history_replay_update(session_id: &str, message: &HistoryMessage) -> Option<Value> {
    let kind = match message.role.as_str() {
        "user" => "user_message_chunk",
        "assistant" => "agent_message_chunk",
        _ => return None,
    };
    Some(json!({
        "sessionId": session_id,
        "update": {
            "kind": kind,
            "text": strip_internal_tags(&message.text),
        }
    }))
}
