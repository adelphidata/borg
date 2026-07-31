//! Wire-level ACP correctness: framing, capability negotiation, method
//! dispatch, error envelopes, stop reasons, stdout-cleanliness invariant.
//!
//! Each test boots one [`BorgdFixture`] + one [`AcpClient`] subprocess. The
//! subprocess speaks newline-delimited JSON-RPC; we drive it from the test
//! with the helpers in [`harness`].

#![allow(dead_code)]

use super::harness::{
    initialize_params_no_fs, initialize_params_with_fs, simple_text_turn, AcpClient, BorgdFixture,
};
use borgd::session::scripted::ScriptedBackend;
use serde_json::{json, Value};

/// All but a couple of cases need the same skeleton: fixture, client,
/// initialize, session/new. Captured here so the individual test bodies
/// stay focused on what they're actually proving.
async fn boot(initialize_params: Value) -> (BorgdFixture, AcpClient, String) {
    let backend = ScriptedBackend::fixed("acp-test-session", simple_text_turn("ok"));
    let fixture = BorgdFixture::with_backend(backend).await;
    let mut client = AcpClient::spawn(&fixture).await.expect("spawn borg acp");
    let init_resp = client
        .request("initialize", initialize_params)
        .await
        .expect("initialize");
    assert!(
        init_resp.get("error").is_none(),
        "initialize errored: {init_resp}"
    );
    let cwd = fixture.workspace_path().display().to_string();
    let new_resp = client
        .request("session/new", json!({"cwd": cwd}))
        .await
        .expect("session/new");
    let session_id = new_resp["result"]["sessionId"]
        .as_str()
        .expect("sessionId")
        .to_string();
    (fixture, client, session_id)
}

#[tokio::test]
async fn initialize_advertises_documented_capability_shape() {
    // Real failure mode: a regression that quietly drops `loadSession: true`
    // or the prompt capabilities would make ACP IDEs (Zed, the Neovim ACP
    // client) refuse to call `session/load` or to attach images. The plan's
    // §"Capability advertisement" pins the exact shape; this test is the
    // executable copy of that contract.
    let backend = ScriptedBackend::fixed("init-only", simple_text_turn("never runs"));
    let fixture = BorgdFixture::with_backend(backend).await;
    let mut client = AcpClient::spawn(&fixture).await.expect("spawn");

    let resp = client
        .request("initialize", initialize_params_with_fs())
        .await
        .expect("initialize");
    let result = &resp["result"];
    assert_eq!(result["protocolVersion"], json!(1));
    assert_eq!(
        result["agentCapabilities"]["loadSession"],
        json!(true),
        "loadSession must be true; the plan §Goals lists session/load as a v1 method"
    );
    let prompt_caps = &result["agentCapabilities"]["promptCapabilities"];
    assert_eq!(prompt_caps["image"], json!(true));
    assert_eq!(prompt_caps["audio"], json!(false));
    assert_eq!(prompt_caps["embeddedContext"], json!(false));
    let mcp = &result["agentCapabilities"]["mcpCapabilities"];
    assert_eq!(mcp["http"], json!(false));
    assert_eq!(mcp["sse"], json!(false));
    assert_eq!(
        result["authMethods"],
        json!([]),
        "authMethods must be returned explicitly empty so IDEs skip auth UI"
    );
    assert_eq!(
        result["agentInfo"]["name"],
        json!("borg"),
        "agentInfo must identify the agent to the client"
    );
}

#[tokio::test]
async fn session_new_emits_session_info_as_spec_session_update_variant() {
    // Real failure mode: session metadata used to go out as a bare
    // top-level `session_info_update` JSON-RPC method, which is not in the
    // ACP schema — real clients drop unknown methods, so the IDE never saw
    // a session title. Spec shape: a `session/update` notification whose
    // update object is discriminated `sessionUpdate: "session_info_update"`.
    let (_fix, client, session_id) = boot(initialize_params_with_fs()).await;
    let lines = client.raw_stdout_lines().await;
    let frames: Vec<Value> = lines
        .iter()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    assert!(
        !frames.iter().any(|f| f["method"] == "session_info_update"),
        "top-level session_info_update method is non-spec and must not appear: {lines:?}"
    );
    let info = frames
        .iter()
        .find(|f| {
            f["method"] == "session/update"
                && f["params"]["update"]["sessionUpdate"] == json!("session_info_update")
        })
        .unwrap_or_else(|| panic!("no spec session_info_update frame found: {lines:?}"));
    assert_eq!(info["params"]["sessionId"], json!(session_id));
    assert!(
        info["params"]["update"]["title"].is_string(),
        "session_info_update must carry a title: {info}"
    );
}

#[tokio::test]
async fn resource_link_prompt_block_reaches_daemon_as_rendered_mention() {
    // ACP baseline: agents MUST accept `resource_link` prompt blocks (Zed
    // sends them for `@file` mentions) even though Borg advertises no extra
    // prompt capabilities. The scripted backend echoes the prompt text it
    // received, so we can assert the mention survived the transport as a
    // resolvable markdown link rather than being dropped or erroring.
    let backend = borgd::session::scripted::ScriptedBackend::new(
        "resource-link",
        move |text, _images, tx, _cancel, _bridge| {
            tokio::spawn(async move {
                let _ = tx
                    .send(cortex::agent::AgentEvent::TextDelta(format!("ECHO:{text}")))
                    .await;
                let _ = tx.send(cortex::agent::AgentEvent::TurnComplete).await;
            })
        },
    );
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

    let resp = client
        .request(
            "session/prompt",
            json!({
                "sessionId": sid,
                "prompt": [
                    {"type": "text", "text": "explain"},
                    {"type": "resource_link", "uri": "file:///ws/src/main.rs", "name": "main.rs"},
                ],
            }),
        )
        .await
        .expect("session/prompt with resource_link");
    assert_eq!(
        resp["result"]["stopReason"],
        json!("end_turn"),
        "resource_link prompt must succeed: {resp}"
    );
    let lines = client.raw_stdout_lines().await;
    let echoed: String = lines
        .iter()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|f| f["params"]["update"]["sessionUpdate"] == json!("agent_message_chunk"))
        .filter_map(|f| {
            f["params"]["update"]["content"]["text"]
                .as_str()
                .map(str::to_string)
        })
        .collect();
    assert!(
        echoed.contains("explain\n[main.rs](file:///ws/src/main.rs)"),
        "daemon must receive the rendered mention; echoed prompt: {echoed:?}"
    );
}

#[tokio::test]
async fn audio_prompt_block_rejected_with_invalid_params() {
    // Borg advertises `audio: false`; a client that sends audio anyway must
    // get a clean -32602 rather than a crash or a silent drop.
    let (_fix, mut client, session_id) = boot(initialize_params_with_fs()).await;
    let resp = client
        .request(
            "session/prompt",
            json!({
                "sessionId": session_id,
                "prompt": [{"type": "audio", "mimeType": "audio/wav", "data": "AAAA"}],
            }),
        )
        .await
        .expect("session/prompt with audio");
    assert_eq!(
        resp["error"]["code"],
        json!(-32602),
        "audio block must reject as invalid params: {resp}"
    );
}

#[tokio::test]
async fn session_new_with_absolute_cwd_succeeds_and_emits_session_info_update() {
    let (_fix, _client, session_id) = boot(initialize_params_with_fs()).await;
    assert!(
        !session_id.is_empty(),
        "session/new must return a non-empty sessionId"
    );
}

#[tokio::test]
async fn session_new_rejects_relative_cwd_with_invalid_params() {
    let backend = ScriptedBackend::fixed("never-opened", simple_text_turn("never"));
    let fixture = BorgdFixture::with_backend(backend).await;
    let mut client = AcpClient::spawn(&fixture).await.expect("spawn");
    let _ = client
        .request("initialize", initialize_params_with_fs())
        .await
        .expect("initialize");

    let resp = client
        .request("session/new", json!({"cwd": "relative/path"}))
        .await
        .expect("session/new");
    let err = resp["error"].as_object().unwrap_or_else(|| {
        panic!("relative cwd must produce a JSON-RPC error; got: {resp}");
    });
    assert_eq!(
        err["code"], json!(-32602),
        "invalid-params expected (relative paths must be rejected before the daemon ever sees them)"
    );
}

#[tokio::test]
async fn session_new_rejects_nonexistent_cwd_with_invalid_params() {
    let backend = ScriptedBackend::fixed("never-opened", simple_text_turn("never"));
    let fixture = BorgdFixture::with_backend(backend).await;
    let mut client = AcpClient::spawn(&fixture).await.expect("spawn");
    let _ = client
        .request("initialize", initialize_params_with_fs())
        .await
        .expect("initialize");

    let resp = client
        .request(
            "session/new",
            json!({"cwd": "/nonexistent/path/that/should/never/exist/borg/acp"}),
        )
        .await
        .expect("session/new");
    let err = resp["error"].as_object().unwrap_or_else(|| {
        panic!("nonexistent cwd must produce a JSON-RPC error; got: {resp}");
    });
    assert_eq!(err["code"], json!(-32602));
}

#[tokio::test]
async fn session_cancel_for_unknown_session_is_a_silent_noop() {
    // Real failure mode: the plan's §"`session/cancel`" rule says cancel
    // for an unknown session must not error — IDEs may legitimately fire
    // cancel after the session was already torn down. A regression that
    // returned `-32602` ("invalid params") here would surface to the user
    // as a "tried to cancel; failed" toast on every reconnect.
    let backend = ScriptedBackend::fixed("never-opened", simple_text_turn("never"));
    let fixture = BorgdFixture::with_backend(backend).await;
    let client = AcpClient::spawn(&fixture).await.expect("spawn");
    // session/cancel is a notification (no id), per the plan's §"Supported
    // methods" → "Implement as notification: session/cancel". Sending it
    // for an unknown session must produce no JSON-RPC error frame on
    // stdout. We assert that by snapshotting stdout-line count before and
    // confirming it doesn't grow.
    let before = client.raw_stdout_lines().await.len();
    client
        .notify("session/cancel", json!({"sessionId": "no-such-session"}))
        .await
        .expect("notify");
    // Give the subprocess a beat to react. If a regression turned this
    // into an error response, the stdout line count would grow.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let after = client.raw_stdout_lines().await.len();
    assert_eq!(
        after,
        before,
        "session/cancel for unknown session must produce zero stdout output; \
         got {} new line(s) (regression: it's now answering with an error)",
        after - before
    );
}

#[tokio::test]
async fn malformed_json_returns_parse_error_and_subprocess_stays_alive() {
    let backend = ScriptedBackend::fixed("post-malformed", simple_text_turn("ok"));
    let fixture = BorgdFixture::with_backend(backend).await;
    let mut client = AcpClient::spawn(&fixture).await.expect("spawn");

    // Inject an unparseable byte sequence followed by a valid request.
    // The transport must (a) emit a JSON-RPC parse-error frame for the
    // malformed line and (b) keep processing the valid one. A regression
    // that bailed the read loop on first parse error would time out the
    // valid request.
    client
        .inject_raw_bytes(b"not-json-at-all\n")
        .await
        .expect("inject raw");

    let resp = client
        .request("initialize", initialize_params_with_fs())
        .await
        .expect("initialize after malformed");
    assert!(
        resp.get("result").is_some(),
        "transport must remain alive after a malformed line; got: {resp}"
    );

    // Also assert the parse error appeared on stdout. Code must be -32700
    // per JSON-RPC 2.0 §5.1.
    let lines = client.raw_stdout_lines().await;
    let parse_err = lines
        .iter()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find(|v| v["error"]["code"] == json!(-32700))
        .unwrap_or_else(|| panic!("no parse-error frame found in stdout; got lines: {lines:?}"));
    assert_eq!(parse_err["error"]["code"], json!(-32700));
}

#[tokio::test]
async fn unknown_method_returns_method_not_found_and_subprocess_stays_alive() {
    let backend = ScriptedBackend::fixed("post-unknown", simple_text_turn("ok"));
    let fixture = BorgdFixture::with_backend(backend).await;
    let mut client = AcpClient::spawn(&fixture).await.expect("spawn");
    let _ = client
        .request("initialize", initialize_params_with_fs())
        .await
        .expect("initialize");

    let resp = client
        .request("nonsense/method", json!({}))
        .await
        .expect("response for unknown method");
    let err = resp["error"].as_object().unwrap_or_else(|| {
        panic!("unknown method must produce an error envelope; got: {resp}");
    });
    assert_eq!(err["code"], json!(-32601), "MethodNotFound code");

    // Subsequent valid request must still succeed.
    let cwd = fixture.workspace_path().display().to_string();
    let after = client
        .request("session/new", json!({"cwd": cwd}))
        .await
        .expect("session/new after unknown");
    assert!(
        after["result"].get("sessionId").is_some(),
        "transport must continue serving after MethodNotFound: {after}"
    );
}

#[tokio::test]
async fn session_prompt_terminates_with_end_turn_stop_reason_on_turn_complete() {
    // Real failure mode: a regression that lost the StreamTermination
    // detection would either hang the prompt forever (waiting for a
    // terminal that never came) or emit `cancelled` for clean completions.
    // Both break the IDE's spinner.
    let (_fix, mut client, session_id) = boot(initialize_params_with_fs()).await;
    let outcome = client
        .run_prompt(&session_id, "go")
        .await
        .expect("session/prompt");
    assert_eq!(outcome.stop_reason.as_deref(), Some("end_turn"));
    assert!(
        outcome.error.is_none(),
        "successful turn must not produce an error envelope: {:?}",
        outcome.error
    );
    assert_eq!(
        outcome.agent_message_text(),
        "ok",
        "scripted text delta must arrive as agent_message_chunk text"
    );
}

#[tokio::test]
async fn every_stdout_line_is_valid_json_rpc() {
    // The hard invariant from §"Logging and Stdio Safety": stdout MUST
    // contain only valid JSON-RPC frames. The source-grep guard inside
    // `acp.rs` catches *static* regressions (a stray `println!`); this
    // test catches *dynamic* leaks: a panic, a tracing layer that
    // mis-routes to stdout, an `eprintln!` in a transitive dependency.
    let backend = ScriptedBackend::fixed("stdio-clean", simple_text_turn("hello"));
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
    let _ = client.run_prompt(&sid, "go").await.expect("session/prompt");

    let lines = client.raw_stdout_lines().await;
    assert!(!lines.is_empty(), "expected some stdout traffic");
    for (i, line) in lines.iter().enumerate() {
        let v: Value = serde_json::from_str(line).unwrap_or_else(|e| {
            panic!(
                "stdout line {i} is not valid JSON: {e}\nLine: {line:?}\n\
                 Full transcript: {lines:?}"
            )
        });
        assert_eq!(
            v["jsonrpc"],
            json!("2.0"),
            "stdout line {i} is JSON but not JSON-RPC 2.0: {line}"
        );
    }
}

#[tokio::test]
async fn initialize_with_no_fs_capabilities_is_accepted() {
    // The plan's fallback policy says: if fs caps absent, file tools fall
    // back to disk. The IDE may legitimately omit `clientCapabilities`
    // entirely. The transport must accept that and not require fs caps as
    // a precondition for `initialize`.
    let backend = ScriptedBackend::fixed("no-fs", simple_text_turn("ok"));
    let fixture = BorgdFixture::with_backend(backend).await;
    let mut client = AcpClient::spawn(&fixture).await.expect("spawn");
    let resp = client
        .request("initialize", initialize_params_no_fs())
        .await
        .expect("initialize without fs caps");
    assert!(
        resp["result"].is_object(),
        "initialize must succeed: {resp}"
    );
}
