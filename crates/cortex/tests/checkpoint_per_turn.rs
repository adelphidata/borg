//! Integration test: every turn that changes files gets its own undo point.
//!
//! Spec: before the first file-changing tool call of a user turn, the agent
//! records the workspace in the checkpoint store under the data dir and files
//! the commit against that turn. The guard that makes it "once per turn" is
//! reset when the next turn starts — a guard that never reset would leave
//! every turn after the first without an undo point, which no unit test of
//! the guard itself can see.
//!
//! Drives the full `Agent::send_message` path against a wiremock LLM that
//! answers each prompt with an `apply_patch` call and then with text. One
//! test in this file, so setting `BORG_HOME` races with nothing.

// Drives the whole agent loop, whose future type nests deep enough that layout
// computation exceeds the default query depth.
#![recursion_limit = "256"]
#![allow(
    clippy::expect_used,
    clippy::field_reassign_with_default,
    clippy::unwrap_used
)]

use std::path::Path;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

use cortex::agent::{Agent, AgentEvent};
use cortex::config::Config;
use cortex::db::Database;
use cortex::execution_context::ExecutionContext;
use cortex::telemetry::BorgMetrics;

const FIRST: &str = "change one to two";
const SECOND: &str = "change two to three";
const CHAT: &str = "how are you";

fn sse(lines: &[String]) -> String {
    lines.iter().map(|l| format!("data: {l}\n\n")).collect()
}

fn text_sse(text: &str) -> String {
    sse(&[
        format!(r#"{{"choices":[{{"delta":{{"content":"{text}"}},"finish_reason":null}}]}}"#),
        r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#.to_string(),
        "[DONE]".to_string(),
    ])
}

/// An `apply_patch` call that rewrites the one line of `notes.txt`.
fn patch_sse(call_id: &str, from: &str, to: &str) -> String {
    let patch =
        format!("*** Begin Patch\n*** Update File: notes.txt\n@@\n-{from}\n+{to}\n*** End Patch");
    let args = serde_json::json!({ "patch": patch }).to_string();
    let delta = serde_json::json!({
        "choices": [{
            "delta": {"tool_calls": [{
                "index": 0,
                "id": call_id,
                "function": {"name": "apply_patch", "arguments": args},
            }]},
            "finish_reason": null,
        }]
    });
    sse(&[
        delta.to_string(),
        r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#.to_string(),
        "[DONE]".to_string(),
    ])
}

/// The model: patches the file when the person's latest message asks for it,
/// and says "done" once the tool result is in (or when asked anything else).
struct Model;

impl Respond for Model {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap_or_default();
        let messages = body["messages"].as_array().cloned().unwrap_or_default();
        let answering_a_tool = messages.last().is_some_and(|m| m["role"] == "tool");
        let asked = messages
            .iter()
            .rev()
            .find(|m| m["role"] == "user")
            .map(|m| m["content"].to_string())
            .unwrap_or_default();
        let reply = if answering_a_tool || body.get("tools").is_none() {
            text_sse("done")
        } else if asked.contains(FIRST) {
            patch_sse("call_1", "one", "two")
        } else if asked.contains(SECOND) {
            patch_sse("call_2", "two", "three")
        } else {
            text_sse("fine")
        };
        ResponseTemplate::new(200).set_body_raw(reply, "text/event-stream")
    }
}

async fn send(agent: &mut Agent, prompt: &str) {
    let (tx, mut rx) = mpsc::channel::<AgentEvent>(256);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    agent
        .send_message_with_cancel(prompt, tx, CancellationToken::new())
        .await
        .expect("turn");
    drain.abort();
}

/// `notes.txt` as the undo point `commit` holds it.
fn held(store: &Path, commit: &str) -> String {
    let out = std::process::Command::new("git")
        .args(["cat-file", "-p", &format!("{commit}:notes.txt")])
        .env("GIT_DIR", store)
        .output()
        .expect("spawn git");
    assert!(out.status.success(), "notes.txt is not in {commit}");
    String::from_utf8(out.stdout).expect("utf-8")
}

/// The agent loop's poll chain is deeper than the 2 MiB stack of a test
/// thread; the daemon runs it on 4 MiB workers (`borgd::runtime`), and so
/// does this test.
const WORKER_STACK_SIZE: usize = 4 * 1024 * 1024;

#[test]
fn each_turn_that_changes_files_gets_its_own_undo_point() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .thread_stack_size(WORKER_STACK_SIZE)
        .build()
        .expect("runtime");
    runtime
        .block_on(async { tokio::spawn(three_turns()).await })
        .expect("the scenario ran to its end");
}

async fn three_turns() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let base = tmp.path().canonicalize().expect("canonicalize");
    let home = base.join("home");
    let project = base.join("project");
    std::fs::create_dir_all(&home).expect("mkdir home");
    std::fs::create_dir_all(&project).expect("mkdir project");
    let notes = project.join("notes.txt");
    std::fs::write(&notes, "one\n").expect("seed notes");
    std::env::set_var("BORG_HOME", &home);
    std::env::set_var("BORG_TEST_CHECKPOINT_KEY", "test-key-value");

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(Model)
        .mount(&server)
        .await;

    let mut config = Config::default();
    config.llm.provider = Some("openai".into());
    config.llm.api_key_env = "BORG_TEST_CHECKPOINT_KEY".into();
    config.llm.base_url = Some(format!("{}/v1/chat/completions", server.uri()));
    config.llm.max_retries = 0;
    config.conversation.max_iterations = 4;
    config.conversation.concurrent_tools.enabled = false;
    config.memory.active_injection.enabled = false;
    config.memory.embeddings.enabled = false;

    let mut agent = Agent::new(config, BorgMetrics::default())
        .await
        .expect("Agent::new");
    agent.set_execution_context(ExecutionContext::unrestricted(project.clone()));
    let session_id = agent.session().meta.id.clone();
    let store = home.join("checkpoints").join("store");
    let db = Database::open().await.expect("open db");

    // Turn 1 changes the file: the undo point holds it as it was.
    send(&mut agent, FIRST).await;
    assert_eq!(std::fs::read_to_string(&notes).unwrap(), "two\n");
    let rows = db.list_session_snapshots(&session_id, 10).await.unwrap();
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].snapshot_root, project);
    assert_eq!(held(&store, &rows[0].commit_id), "one\n");

    // A turn that only talks takes none.
    send(&mut agent, CHAT).await;
    let rows = db.list_session_snapshots(&session_id, 10).await.unwrap();
    assert_eq!(rows.len(), 1, "{rows:?}");

    // Turn 3 changes the file again: a new undo point, holding what turn 1
    // left, filed under the later message.
    send(&mut agent, SECOND).await;
    assert_eq!(std::fs::read_to_string(&notes).unwrap(), "three\n");
    let rows = db.list_session_snapshots(&session_id, 10).await.unwrap();
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert_eq!(held(&store, &rows[0].commit_id), "two\n");
    assert_eq!(held(&store, &rows[1].commit_id), "one\n");
    let (later, earlier) = (rows[0].user_message_seq, rows[1].user_message_seq);
    assert!(
        earlier.is_some() && later > earlier,
        "each undo point names its own turn: {earlier:?} then {later:?}"
    );

    // The project itself was not turned into a repository.
    assert!(!project.join(".git").exists());
}
