//! Schema-level conformance against the official ACP v1 JSON schema.
//!
//! `fixtures/acp_schema_v1.json` is a vendored copy of the canonical
//! protocol schema (`schema/v1/schema.json` in the upstream
//! `agentclientprotocol/agent-client-protocol` repository). Every frame the
//! `borg acp` subprocess writes during a representative session is
//! validated against the matching schema definition. This is the closest a
//! hermetic test can get to "a real IDE (Zed, VS Code ACP clients) can
//! parse everything we emit": those clients deserialize with types
//! generated from this exact schema, so a frame that fails validation here
//! is a frame a real IDE drops or errors on.
//!
//! When bumping the vendored schema, re-run this suite — a new required
//! field or renamed variant upstream should fail here first, not in an
//! IDE bug report.

#![allow(dead_code)]

use super::harness::{initialize_params_with_fs, AcpClient, BorgdFixture};
use borgd::session::scripted::ScriptedBackend;
use cortex::agent::AgentEvent;
use cortex::types::{PlanStep, PlanStepStatus};
use serde_json::{json, Value};
use std::path::Path;

/// The vendored schema parsed once — it is ~240 KB, and each test builds
/// several per-definition validators from the same document.
static SCHEMA_ROOT: std::sync::LazyLock<Value> = std::sync::LazyLock::new(|| {
    serde_json::from_str(include_str!("fixtures/acp_schema_v1.json"))
        .expect("vendored ACP schema parses")
});

/// Build a validator for one `$defs` entry of the vendored schema.
fn validator_for_def(def: &str) -> jsonschema::Validator {
    let subschema = json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$defs": &SCHEMA_ROOT["$defs"],
        "$ref": format!("#/$defs/{def}"),
    });
    jsonschema::validator_for(&subschema)
        .unwrap_or_else(|e| panic!("compile validator for {def}: {e}"))
}

fn assert_valid(validator: &jsonschema::Validator, instance: &Value, context: &str) {
    let errors: Vec<String> = validator
        .iter_errors(instance)
        .map(|e| format!("  at {}: {e}", e.instance_path()))
        .collect();
    assert!(
        errors.is_empty(),
        "{context} failed ACP schema validation:\n{}\ninstance: {instance:#}",
        errors.join("\n")
    );
}

/// Drive one rich session — text, thinking, full tool lifecycle (stdout +
/// stderr), plan, fs read/write callbacks — then validate every stdout
/// frame against the schema definition its method/shape selects.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_emitted_frame_validates_against_official_acp_schema() {
    let backend = ScriptedBackend::new("spec-conformance", move |_t, _i, tx, _c, bridge| {
        tokio::spawn(async move {
            let bridge = bridge.expect("fs caps advertised → bridge present");
            // Exercise the fs callback path so the agent→client fs
            // requests appear on the wire and get validated too.
            let _ = bridge.read_text_file(Path::new("buffer.txt"));
            let _ = bridge.write_text_file(Path::new("out.txt"), "new-content");
            let _ = tx.send(AgentEvent::TextDelta("Hello ".into())).await;
            let _ = tx.send(AgentEvent::ThinkingDelta("mulling".into())).await;
            let _ = tx
                .send(AgentEvent::ToolExecuting {
                    tool_call_id: "call-1".into(),
                    name: "run_shell".into(),
                    args: r#"{"command":"ls"}"#.into(),
                })
                .await;
            let _ = tx
                .send(AgentEvent::ToolOutputDelta {
                    tool_call_id: "call-1".into(),
                    name: "run_shell".into(),
                    delta: "file-a\n".into(),
                    is_stderr: false,
                })
                .await;
            let _ = tx
                .send(AgentEvent::ToolOutputDelta {
                    tool_call_id: "call-1".into(),
                    name: "run_shell".into(),
                    delta: "warning\n".into(),
                    is_stderr: true,
                })
                .await;
            let _ = tx
                .send(AgentEvent::ToolResult {
                    tool_call_id: "call-1".into(),
                    name: "run_shell".into(),
                    result: "file-a".into(),
                })
                .await;
            let _ = tx
                .send(AgentEvent::PlanUpdated {
                    steps: vec![
                        PlanStep {
                            title: "list files".into(),
                            status: PlanStepStatus::Completed,
                        },
                        PlanStep {
                            title: "summarise".into(),
                            status: PlanStepStatus::InProgress,
                        },
                    ],
                })
                .await;
            let _ = tx.send(AgentEvent::TextDelta("done".into())).await;
            let _ = tx.send(AgentEvent::TurnComplete).await;
        })
    });
    let fixture = BorgdFixture::with_backend(backend).await;
    let mut client = AcpClient::spawn(&fixture).await.expect("spawn");
    client.fs_map().insert("buffer.txt", "unsaved");

    let init = client
        .request("initialize", initialize_params_with_fs())
        .await
        .expect("initialize");
    let cwd = fixture.workspace_path().display().to_string();
    let new = client
        .request("session/new", json!({"cwd": cwd, "mcpServers": []}))
        .await
        .expect("session/new");
    let sid = new["result"]["sessionId"].as_str().unwrap().to_string();
    let prompt = client
        .request(
            "session/prompt",
            json!({
                "sessionId": sid,
                "prompt": [
                    {"type": "text", "text": "go"},
                    {"type": "resource_link", "uri": "file:///ws/a.rs", "name": "a.rs"},
                ],
            }),
        )
        .await
        .expect("session/prompt");

    // ── Response-shape validation ────────────────────────────────────────
    assert_valid(
        &validator_for_def("InitializeResponse"),
        &init["result"],
        "initialize result",
    );
    assert_valid(
        &validator_for_def("NewSessionResponse"),
        &new["result"],
        "session/new result",
    );
    assert_valid(
        &validator_for_def("PromptResponse"),
        &prompt["result"],
        "session/prompt result",
    );

    // ── Notification / callback-request validation ───────────────────────
    let session_update = validator_for_def("SessionNotification");
    let fs_read = validator_for_def("ReadTextFileRequest");
    let fs_write = validator_for_def("WriteTextFileRequest");

    let mut seen_kinds: Vec<String> = Vec::new();
    let mut validated_fs = 0usize;
    for line in client.raw_stdout_lines().await {
        let frame: Value = serde_json::from_str(&line).expect("stdout line is JSON");
        match frame["method"].as_str() {
            Some("session/update") => {
                assert_valid(&session_update, &frame["params"], "session/update params");
                if let Some(kind) = frame["params"]["update"]["sessionUpdate"].as_str() {
                    seen_kinds.push(kind.to_string());
                }
            }
            Some("fs/read_text_file") => {
                assert_valid(&fs_read, &frame["params"], "fs/read_text_file params");
                validated_fs += 1;
            }
            Some("fs/write_text_file") => {
                assert_valid(&fs_write, &frame["params"], "fs/write_text_file params");
                validated_fs += 1;
            }
            _ => {}
        }
    }

    // Vacuity guard: the frames we set out to validate must actually have
    // been emitted — an event pipeline regression that silently dropped a
    // variant would otherwise turn this whole test into a no-op.
    for expected in [
        "session_info_update",
        "agent_message_chunk",
        "agent_thought_chunk",
        "tool_call",
        "tool_call_update",
        "plan",
    ] {
        assert!(
            seen_kinds.iter().any(|k| k == expected),
            "expected a {expected} update on the wire; saw kinds: {seen_kinds:?}"
        );
    }
    assert!(
        validated_fs >= 2,
        "expected both fs/read_text_file and fs/write_text_file callbacks; validated {validated_fs}"
    );
}

/// The schema validator itself must be able to fail — feed it the legacy
/// (pre-spec) update shape and require rejection. Guards against a fixture
/// or `$ref` wiring bug making `assert_valid` vacuously true.
#[test]
fn validator_rejects_legacy_non_spec_update_shape() {
    let session_update = validator_for_def("SessionNotification");
    let legacy = json!({
        "sessionId": "sess-1",
        "update": {
            "kind": "agent_message_chunk",
            "text": "hi",
        }
    });
    assert!(
        !session_update.is_valid(&legacy),
        "legacy `kind`/`text` update shape must FAIL schema validation"
    );
    let spec = json!({
        "sessionId": "sess-1",
        "update": {
            "sessionUpdate": "agent_message_chunk",
            "content": {"type": "text", "text": "hi"},
        }
    });
    assert!(
        session_update.is_valid(&spec),
        "spec-shaped update must pass schema validation"
    );
}
