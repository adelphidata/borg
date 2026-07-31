//! Pure translation from Borg agent events and persisted history into
//! ACP `session/update` frames. No I/O — every function here maps an
//! input value to an optional JSON payload.
//!
//! Wire shapes follow the ACP v1 schema
//! (<https://agentclientprotocol.com/protocol/schema>): the `update`
//! object is discriminated by `sessionUpdate`, message/thought chunks
//! carry a `ContentBlock` under `content`, tool updates carry a
//! `ToolCallContent` array, and plan entries are
//! `{content, priority, status}`.

use cortex::agent::AgentEvent;
use cortex::internal_tag_filter::strip_internal_tags;
use serde_json::{json, Value};

use super::HistoryMessage;

/// Map a Borg tool name to the ACP `tool_call` category. ACP clients (Zed,
/// etc.) render tool calls based on this `kind` field; without it they fall
/// back to a generic icon and label. We map to the full
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

/// Wrap stripped text as the ACP `ContentBlock` used in message chunks.
fn text_content_block(text: &str) -> Value {
    json!({
        "type": "text",
        "text": strip_internal_tags(text),
    })
}

/// Wrap stripped text as a single-element ACP `ToolCallContent` array.
fn tool_text_content(text: &str) -> Value {
    json!([{
        "type": "content",
        "content": text_content_block(text),
    }])
}

/// Parse the tool arguments JSON the daemon streams as a string. Tool args
/// originate as JSON from the LLM, but a truncated/garbled string must not
/// drop the update — fall back to carrying the raw string.
fn raw_input_value(args: &str) -> Value {
    serde_json::from_str(args).unwrap_or_else(|_| Value::String(args.to_string()))
}

pub(super) fn event_to_update(event: &AgentEvent, session_id: &str) -> Option<Value> {
    match event {
        AgentEvent::TextDelta(text) => Some(json!({
            "sessionId": session_id,
            "update": {
                "sessionUpdate": "agent_message_chunk",
                "content": text_content_block(text),
            }
        })),
        AgentEvent::ThinkingDelta(text) => Some(json!({
            "sessionId": session_id,
            "update": {
                "sessionUpdate": "agent_thought_chunk",
                "content": text_content_block(text),
            }
        })),
        AgentEvent::ToolExecuting {
            tool_call_id,
            name,
            args,
        } => Some(json!({
            "sessionId": session_id,
            "update": {
                "sessionUpdate": "tool_call",
                "toolCallId": tool_call_id,
                "title": name,
                "kind": tool_kind_for(name),
                "status": "in_progress",
                "rawInput": raw_input_value(args),
            }
        })),
        AgentEvent::ToolOutputDelta {
            tool_call_id,
            name: _,
            delta,
            is_stderr,
        } => Some(json!({
            "sessionId": session_id,
            "update": {
                "sessionUpdate": "tool_call_update",
                "toolCallId": tool_call_id,
                "status": "in_progress",
                "content": tool_text_content(delta),
                // `_meta` is ACP's sanctioned extension point; spec content
                // blocks carry no stdout/stderr distinction, so surface it
                // there for clients that want to style error output.
                "_meta": {
                    "borg": { "stream": if *is_stderr { "stderr" } else { "stdout" } }
                }
            }
        })),
        AgentEvent::ToolResult {
            tool_call_id,
            name: _,
            result,
        } => Some(json!({
            "sessionId": session_id,
            "update": {
                "sessionUpdate": "tool_call_update",
                "toolCallId": tool_call_id,
                "status": "completed",
                "content": tool_text_content(result),
            }
        })),
        AgentEvent::PlanUpdated { steps } => Some(json!({
            "sessionId": session_id,
            "update": {
                "sessionUpdate": "plan",
                "entries": steps.iter().map(|step| {
                    json!({
                        "content": step.title,
                        "priority": "medium",
                        "status": step.status.as_wire_str(),
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
            "sessionUpdate": kind,
            "content": text_content_block(&message.text),
        }
    }))
}
