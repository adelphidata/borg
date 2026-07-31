//! Event translation: text deltas, tool_call lifecycle with stable
//! tool_call_id, plan notifications, `<internal>` stripping, session/load
//! replay shape.
//!
//! These tests script the daemon to emit specific [`AgentEvent`] sequences,
//! then assert on the ACP `session/update` notifications the subprocess
//! produces. The contract under test is `event_to_update` (in
//! `crates/interface/src/commands/acp.rs`) — its in-file unit tests cover
//! the per-variant translation in isolation; here we cover them
//! end-to-end through the real bidi gRPC stream so a regression that
//! affects the wire-level shape is caught.

#![allow(dead_code)]

use super::harness::{initialize_params_with_fs, AcpClient, BorgdFixture, PromptOutcome};
use borgd::session::scripted::ScriptedBackend;
use cortex::agent::AgentEvent;
use cortex::types::{PlanStep, PlanStepStatus};
use serde_json::{json, Value};

async fn run_with_events(events: Vec<AgentEvent>) -> PromptOutcome {
    let backend = ScriptedBackend::fixed("streaming-test", events);
    let fixture = BorgdFixture::with_backend(backend).await;
    let mut client = AcpClient::spawn(&fixture).await.expect("spawn");
    let _ = client
        .request("initialize", initialize_params_with_fs())
        .await
        .expect("initialize");
    let cwd = fixture.workspace_path().display().to_string();
    let new = client
        .request("session/new", json!({"cwd": cwd}))
        .await
        .expect("session/new");
    let sid = new["result"]["sessionId"].as_str().unwrap().to_string();
    client.run_prompt(&sid, "go").await.expect("session/prompt")
}

/// Extract the text payload of a message-chunk update (spec shape:
/// `update.content` is a `{type:"text", text}` ContentBlock).
fn chunk_text(update: &Value) -> Option<&str> {
    update["update"]["content"]["text"].as_str()
}

#[tokio::test]
async fn text_delta_arrives_as_agent_message_chunk_in_order() {
    // Real failure mode: a regression that batched deltas would deliver
    // them all at the end of the turn, breaking the IDE's streaming UX.
    let outcome = run_with_events(vec![
        AgentEvent::TextDelta("Hello, ".into()),
        AgentEvent::TextDelta("world".into()),
        AgentEvent::TurnComplete,
    ])
    .await;
    let chunks = outcome.updates_of_kind("agent_message_chunk");
    let texts: Vec<&str> = chunks.iter().filter_map(|u| chunk_text(u)).collect();
    assert_eq!(
        texts,
        vec!["Hello, ", "world"],
        "text deltas must arrive in order, one chunk per delta"
    );
}

#[tokio::test]
async fn internal_tags_are_stripped_from_text_deltas() {
    // Real failure mode: `<internal>...</internal>` is the agent's hidden
    // scratchpad. Leaking it to the IDE exposes raw chain-of-thought,
    // which is both a UX and a security regression.
    let outcome = run_with_events(vec![
        AgentEvent::TextDelta(
            "visible-prefix<internal>secret thoughts</internal>visible-suffix".into(),
        ),
        AgentEvent::TurnComplete,
    ])
    .await;
    let chunks = outcome.updates_of_kind("agent_message_chunk");
    let combined: String = chunks.iter().filter_map(|u| chunk_text(u)).collect();
    assert!(
        !combined.contains("secret thoughts"),
        "internal-tag content must be stripped before reaching the IDE; got: {combined:?}"
    );
    assert!(
        combined.contains("visible-prefix"),
        "non-internal content must survive stripping"
    );
    assert!(
        combined.contains("visible-suffix"),
        "non-internal content after the closing tag must survive stripping"
    );
}

#[tokio::test]
async fn tool_lifecycle_preserves_stable_tool_call_id() {
    // Real failure mode: if `tool_call_id` were re-allocated per event, the
    // IDE would render three distinct tool-call cards instead of one with a
    // status progression — the single biggest source of ACP UX bugs.
    let outcome = run_with_events(vec![
        AgentEvent::ToolExecuting {
            tool_call_id: "call-abc".into(),
            name: "read_file".into(),
            args: r#"{"path":"foo.txt"}"#.into(),
        },
        AgentEvent::ToolOutputDelta {
            tool_call_id: "call-abc".into(),
            name: "read_file".into(),
            delta: "partial output".into(),
            is_stderr: false,
        },
        AgentEvent::ToolResult {
            tool_call_id: "call-abc".into(),
            name: "read_file".into(),
            result: "final result".into(),
        },
        AgentEvent::TurnComplete,
    ])
    .await;

    let tool_calls = outcome.updates_of_kind("tool_call");
    let tool_updates = outcome.updates_of_kind("tool_call_update");

    assert_eq!(
        tool_calls.len(),
        1,
        "one tool_call kickoff per ToolExecuting"
    );
    assert_eq!(
        tool_calls[0]["update"]["toolCallId"],
        json!("call-abc"),
        "kickoff carries the scripted id"
    );
    // Spec shape: required `title`, `kind` (not toolKind), valid status
    // enum (`in_progress`, never the non-spec `running`), structured
    // rawInput parsed from the args JSON.
    assert_eq!(tool_calls[0]["update"]["status"], json!("in_progress"));
    assert_eq!(tool_calls[0]["update"]["title"], json!("read_file"));
    assert_eq!(tool_calls[0]["update"]["kind"], json!("read"));
    assert_eq!(
        tool_calls[0]["update"]["rawInput"]["path"],
        json!("foo.txt")
    );

    assert_eq!(
        tool_updates.len(),
        2,
        "one tool_call_update each for ToolOutputDelta + ToolResult"
    );
    for u in &tool_updates {
        assert_eq!(
            u["update"]["toolCallId"],
            json!("call-abc"),
            "every lifecycle event must carry the SAME tool_call_id"
        );
    }
    assert_eq!(tool_updates[0]["update"]["status"], json!("in_progress"));
    assert_eq!(
        tool_updates[0]["update"]["content"][0]["content"]["text"],
        json!("partial output"),
        "tool output rides in a ToolCallContent array, not a raw string"
    );
    assert_eq!(tool_updates[1]["update"]["status"], json!("completed"));
    assert_eq!(
        tool_updates[1]["update"]["content"][0]["content"]["text"],
        json!("final result")
    );
}

#[tokio::test]
async fn tool_output_delta_marks_stderr_stream_separately() {
    // Real failure mode: collapsing stderr+stdout into one stream defeats
    // the IDE's ability to highlight error output differently. The plan
    // §"Tool-call lifecycle" pins the `stream: stderr|stdout` field.
    let outcome = run_with_events(vec![
        AgentEvent::ToolExecuting {
            tool_call_id: "x".into(),
            name: "run_shell".into(),
            args: "{}".into(),
        },
        AgentEvent::ToolOutputDelta {
            tool_call_id: "x".into(),
            name: "run_shell".into(),
            delta: "out-line".into(),
            is_stderr: false,
        },
        AgentEvent::ToolOutputDelta {
            tool_call_id: "x".into(),
            name: "run_shell".into(),
            delta: "err-line".into(),
            is_stderr: true,
        },
        AgentEvent::ToolResult {
            tool_call_id: "x".into(),
            name: "run_shell".into(),
            result: "done".into(),
        },
        AgentEvent::TurnComplete,
    ])
    .await;

    let updates = outcome.updates_of_kind("tool_call_update");
    // Spec content blocks carry no stdout/stderr distinction; Borg surfaces
    // it via `_meta` (ACP's sanctioned extension point) on output deltas.
    let streams: Vec<&str> = updates
        .iter()
        .filter_map(|u| u["update"]["_meta"]["borg"]["stream"].as_str())
        .collect();
    assert_eq!(
        streams,
        vec!["stdout", "stderr"],
        "_meta stream tag must distinguish stdout vs stderr in order"
    );
    // `tool_call_update.content` is replace-semantics per spec, so the
    // second delta frame must carry the accumulated output — a spec client
    // renders exactly this array and would otherwise drop the stdout line.
    assert_eq!(
        updates[1]["update"]["content"][0]["content"]["text"],
        json!("out-lineerr-line"),
        "each streamed frame must carry the full output so far"
    );
}

#[tokio::test]
async fn plan_updated_emits_full_entries_with_medium_priority() {
    // Real failure mode: a regression that emitted only the *changed*
    // step, or that omitted the `priority: "medium"` field, would break
    // ACP IDEs that show the plan as a list — they receive the full
    // state every update.
    let outcome = run_with_events(vec![
        AgentEvent::PlanUpdated {
            steps: vec![
                PlanStep {
                    title: "step one".into(),
                    status: PlanStepStatus::Completed,
                },
                PlanStep {
                    title: "step two".into(),
                    status: PlanStepStatus::InProgress,
                },
                PlanStep {
                    title: "step three".into(),
                    status: PlanStepStatus::Pending,
                },
            ],
        },
        AgentEvent::TurnComplete,
    ])
    .await;

    let plans = outcome.updates_of_kind("plan");
    assert_eq!(plans.len(), 1, "one plan notification per PlanUpdated");
    let entries = plans[0]["update"]["entries"]
        .as_array()
        .expect("entries array");
    assert_eq!(
        entries.len(),
        3,
        "all steps must be emitted, not just changed"
    );
    // Spec plan entries are {content, priority, status} — `content` carries
    // the human-readable step text; there is no id/title field.
    let contents: Vec<&str> = entries
        .iter()
        .filter_map(|e| e["content"].as_str())
        .collect();
    assert_eq!(contents, vec!["step one", "step two", "step three"]);
    let statuses: Vec<&str> = entries
        .iter()
        .filter_map(|e| e["status"].as_str())
        .collect();
    assert_eq!(statuses, vec!["completed", "in_progress", "pending"]);
    for (i, e) in entries.iter().enumerate() {
        assert_eq!(
            e["priority"],
            json!("medium"),
            "every step must default to priority `medium` (entry {i})"
        );
        assert!(
            e.get("id").is_none() && e.get("title").is_none(),
            "plan entries must use the spec `content` field, not id/title (entry {i}): {e}"
        );
    }
}

#[tokio::test]
async fn agent_error_event_terminates_with_failure_envelope() {
    // Real failure mode: a daemon that crashes mid-turn with `Error("boom")`
    // must surface that to the IDE as a JSON-RPC error, not silently end
    // the turn with a fake `end_turn` stop reason.
    let outcome = run_with_events(vec![
        AgentEvent::TextDelta("partial".into()),
        AgentEvent::Error("boom".into()),
    ])
    .await;
    assert!(
        outcome.error.is_some(),
        "Error event must surface as a JSON-RPC error envelope, got outcome: {outcome:?}"
    );
    assert!(
        outcome.stop_reason.is_none(),
        "no stop_reason on error path"
    );
}
