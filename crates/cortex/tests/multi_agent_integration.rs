//! End-to-end integration tests for the sub-agent caller surface.
//!
//! Drives `AgentControl::spawn_and_wait` against a wiremock OpenAI-shaped
//! LLM endpoint so the full path runs: handler → controller → `run_sub_agent`
//! → `Agent::new_sub_agent` → real LLM client → SSE stream → preamble in
//! the request body → completion JSON.
//!
//! These tests are the load-bearing proof that the *behavior* of the new
//! caller surface works, not just the shapes of the individual builders.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use serde_json::Value;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

use cortex::config::Config;
use cortex::multi_agent::{
    event_bus::{AgentTreeBus, AgentTreeEventKind},
    AgentControl, AgentRoleKind,
};

static TEST_COUNTER: AtomicU32 = AtomicU32::new(0);

/// Build a `Config` whose LLM points at the given wiremock URI, with a
/// unique API-key env var so parallel tests don't clobber each other.
fn test_config(server_uri: &str) -> Config {
    let n = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
    let env_key = format!("BORG_SUBAGENT_LLM_KEY_{n}");
    std::env::set_var(&env_key, format!("test-key-{n}"));

    let mut config = Config::default();
    config.llm.provider = Some("openai".into());
    config.llm.api_key_env = env_key;
    config.llm.base_url = Some(format!("{server_uri}/v1/chat/completions"));
    config.llm.max_retries = 0;
    // Keep the agent loop tight — no compaction games on a one-shot test.
    config.conversation.max_iterations = 3;
    config
}

/// Single-line completion SSE — one text delta then finish.
fn sse_text(text: &str) -> String {
    let escaped = serde_json::to_string(text).unwrap();
    format!(
        "data: {{\"choices\":[{{\"delta\":{{\"content\":{escaped}}},\"finish_reason\":null}}]}}\n\n\
         data: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"stop\"}}]}}\n\n\
         data: [DONE]\n\n"
    )
}

/// Pull the JSON request body out of a wiremock-captured request.
fn body_of(req: &Request) -> Value {
    serde_json::from_slice(&req.body).expect("request body must be JSON")
}

/// END-TO-END: `context` and `goal` reach the LLM as a structured
/// preamble in the first user message. The caller contract is that the
/// child sees these as distinct, parseable blocks — if a regression
/// drops or merges them, the child loses the ability to tell "background
/// fact" from "thing to do" and downstream subagent-driven-development
/// reviewer prompts (which grep for `<goal>`) break silently.
#[tokio::test]
async fn context_and_goal_reach_llm_as_structured_preamble() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_string(sse_text("done.")))
        .mount(&server)
        .await;

    let parent_cfg = test_config(&server.uri());
    let mut ctrl = AgentControl::new(&parent_cfg.agents, "parent-session", 0);

    let completion = ctrl
        .spawn_and_wait(
            "Summarize the file",
            Some("File path: src/foo.rs. User prefers tabs over spaces."),
            None, // role
            AgentRoleKind::Leaf,
            None, // toolsets
            None, // nickname
            None, // model_override
            &parent_cfg,
            None, // fork_context
            None, // tools_blocklist
            10,
        )
        .await
        .expect("spawn must complete against wiremock");

    assert_eq!(completion.status.as_str(), "completed");
    assert_eq!(completion.final_response.as_deref(), Some("done."));

    // Drain captured requests; find the one with our preamble.
    let received = server.received_requests().await.expect("requests captured");
    assert!(
        !received.is_empty(),
        "wiremock must have seen at least one request"
    );

    let user_block = received
        .iter()
        .filter_map(|req| {
            let body = body_of(req);
            body["messages"]
                .as_array()?
                .iter()
                .find(|m| m["role"] == "user")
                .and_then(|m| m["content"].as_str().map(str::to_string))
        })
        .next()
        .expect("at least one request must carry a user message");

    assert!(
        user_block.contains("<context trust=\"parent\">"),
        "child's user message must wrap context in a trust-tagged block, got:\n{user_block}"
    );
    assert!(
        user_block.contains("File path: src/foo.rs. User prefers tabs over spaces."),
        "context text must appear verbatim in preamble, got:\n{user_block}"
    );
    assert!(
        user_block.contains("<goal>\nSummarize the file"),
        "goal must be wrapped in <goal> block, got:\n{user_block}"
    );
    assert!(
        user_block.contains("You are a leaf"),
        "leaf preamble must signal non-delegation, got:\n{user_block}"
    );
}

/// END-TO-END: `Heartbeat` events fire on the `AgentTreeBus` while a
/// child is running. The test configures a 1s interval (the minimum
/// post-clamp), holds the mock open for ~3s, then cancels — by the time
/// the cancel takes effect we should have seen at least two heartbeats.
/// A regression that disconnects the tick from the bus would silently
/// blind the TUI to "still working" status.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn heartbeat_events_fire_while_child_runs() {
    let server = MockServer::start().await;
    // Delay the LLM response to keep the child alive long enough to tick.
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(sse_text("ok"))
                .set_delay(Duration::from_millis(3500)),
        )
        .mount(&server)
        .await;

    let mut parent_cfg = test_config(&server.uri());
    parent_cfg.agents.heartbeat_interval_secs = 1; // minimum
    parent_cfg.agents.heartbeat_stale_cycles = 999; // don't escalate to Stalled

    let bus = AgentTreeBus::new();
    let mut rx = bus.subscribe();
    let mut ctrl = AgentControl::new(&parent_cfg.agents, "parent", 0).with_bus(bus);

    // Spawn (non-blocking) so we can observe events while the child runs.
    let (agent_id, _nickname) = ctrl
        .spawn_agent(
            "any",
            None,
            None,
            AgentRoleKind::Leaf,
            None,
            None,
            None,
            &parent_cfg,
            None,
            None,
        )
        .await
        .expect("spawn");

    // Collect events for ~2.5s. We expect ≥2 Heartbeat events at 1s cadence,
    // plus a Spawned at the start.
    let mut heartbeats = 0;
    let mut saw_spawned = false;
    let deadline = tokio::time::Instant::now() + Duration::from_millis(2500);
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(200), rx.recv()).await {
            Ok(Ok(ev)) => match ev.kind {
                AgentTreeEventKind::Heartbeat => heartbeats += 1,
                AgentTreeEventKind::Spawned => saw_spawned = true,
                _ => {}
            },
            _ => continue,
        }
    }

    // Cancel the child before the mock delay fires so we exit cleanly.
    let _ = ctrl.shutdown_agent(&agent_id).await;

    assert!(saw_spawned, "Spawned event must precede heartbeats");
    assert!(
        heartbeats >= 2,
        "expected ≥2 heartbeats over ~2.5s at 1s cadence, got {heartbeats}"
    );
}

/// END-TO-END: `Stalled` event fires after the configured threshold of
/// consecutive idle heartbeat cycles. Stalled signals the operator that
/// the child has gone quiet — without it, a hung tool or wedged LLM
/// would silently consume tokens with no UI indication.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stalled_event_fires_after_threshold_idle_cycles() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(sse_text("ok"))
                .set_delay(Duration::from_millis(5000)),
        )
        .mount(&server)
        .await;

    let mut parent_cfg = test_config(&server.uri());
    parent_cfg.agents.heartbeat_interval_secs = 1;
    parent_cfg.agents.heartbeat_stale_cycles = 2; // fires after 2 idle ticks

    let bus = AgentTreeBus::new();
    let mut rx = bus.subscribe();
    let mut ctrl = AgentControl::new(&parent_cfg.agents, "parent", 0).with_bus(bus);

    let (agent_id, _nickname) = ctrl
        .spawn_agent(
            "any",
            None,
            None,
            AgentRoleKind::Leaf,
            None,
            None,
            None,
            &parent_cfg,
            None,
            None,
        )
        .await
        .expect("spawn");

    let mut stalled_count = 0;
    let deadline = tokio::time::Instant::now() + Duration::from_millis(4000);
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(200), rx.recv()).await {
            Ok(Ok(ev)) => {
                if ev.kind == AgentTreeEventKind::Stalled {
                    stalled_count += 1;
                }
            }
            _ => continue,
        }
    }

    let _ = ctrl.shutdown_agent(&agent_id).await;

    assert_eq!(
        stalled_count, 1,
        "Stalled must fire exactly once (the at-most-once contract), got {stalled_count}"
    );
}

/// END-TO-END: explicit `model_override` arg dispatches the child against
/// the override, not the parent's model. Verified by inspecting the
/// `model` field in the JSON body wiremock receives.
#[tokio::test]
async fn model_override_takes_precedence_over_parent_model() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_string(sse_text("ok")))
        .mount(&server)
        .await;

    let mut parent_cfg = test_config(&server.uri());
    parent_cfg.llm.model = "parent-model-name".into();

    let mut ctrl = AgentControl::new(&parent_cfg.agents, "p", 0);
    let _ = ctrl
        .spawn_and_wait(
            "do thing",
            None,
            None,
            AgentRoleKind::Leaf,
            None,
            None,
            Some("override-model-name"),
            &parent_cfg,
            None,
            None,
            10,
        )
        .await
        .expect("spawn");

    let received = server.received_requests().await.expect("requests");
    let used_model = received
        .iter()
        .find_map(|req| {
            body_of(req)
                .get("model")
                .and_then(|m| m.as_str().map(str::to_string))
        })
        .expect("at least one request must carry a model");
    assert_eq!(
        used_model, "override-model-name",
        "explicit override must beat parent's model in the LLM request"
    );
}

/// END-TO-END: orchestrator role + depth-under-cap + kill switch on →
/// the child's tool list (visible in the LLM request body) MUST include
/// `spawn_agent`. The default leaf would strip it via the blocklist.
/// This is the load-bearing positive test for the orchestrator carve-out.
#[tokio::test]
async fn orchestrator_role_gets_spawn_agent_tool() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_string(sse_text("ok")))
        .mount(&server)
        .await;

    let mut parent_cfg = test_config(&server.uri());
    parent_cfg.agents.enabled = true;
    parent_cfg.agents.max_spawn_depth = 3; // allow orchestrator at depth 0 → 1
    parent_cfg.agents.orchestrator_enabled = true;

    let mut ctrl = AgentControl::new(&parent_cfg.agents, "p", 0);

    // Pre-compute the blocklist the way `handle_spawn_agent` does.
    use cortex::multi_agent::tools::{assemble_delegate_blocklist, can_orchestrate};
    let can = can_orchestrate(&parent_cfg, 0);
    assert!(
        can,
        "test premise: orchestrator should be permitted at depth 0 with cap=3"
    );
    let blocklist = assemble_delegate_blocklist(&parent_cfg, AgentRoleKind::Orchestrator, can);
    let blocklist_refs: Vec<&str> = blocklist.to_vec();

    let _ = ctrl
        .spawn_and_wait(
            "delegate further if needed",
            None,
            None,
            AgentRoleKind::Orchestrator,
            None,
            None,
            None,
            &parent_cfg,
            None,
            Some(&blocklist_refs),
            10,
        )
        .await
        .expect("spawn");

    let received = server.received_requests().await.expect("requests");
    let tool_names: Vec<String> = received
        .iter()
        .find_map(|req| {
            let body = body_of(req);
            let tools = body.get("tools")?.as_array()?.clone();
            Some(
                tools
                    .into_iter()
                    .filter_map(|t| {
                        t.get("function")
                            .and_then(|f| f.get("name"))
                            .and_then(|n| n.as_str())
                            .map(str::to_string)
                    })
                    .collect::<Vec<_>>(),
            )
        })
        .expect("at least one request must carry a tools array");

    assert!(
        tool_names.contains(&"spawn_agent".to_string()),
        "orchestrator child must have spawn_agent in its tool list, got {tool_names:?}"
    );
    // wait_for_agent and close_agent are also re-permitted by the carve-out.
    assert!(
        tool_names.contains(&"wait_for_agent".to_string()),
        "orchestrator child must also have wait_for_agent: {tool_names:?}"
    );
    // Mutating-but-not-delegation tools (write_memory) stay blocked.
    assert!(
        !tool_names.contains(&"write_memory".to_string()),
        "write_memory must remain blocked even for orchestrator: {tool_names:?}"
    );
}

/// END-TO-END: a leaf child with the orchestrator kill switch turned on
/// at the operator level still does NOT get `spawn_agent`. Confirms that
/// only the role-kind opt-in re-permits delegation — kill switch alone
/// isn't enough. The negative-test counterpart to the orchestrator test.
#[tokio::test]
async fn leaf_role_never_gets_spawn_agent_even_with_kill_switch_on() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_string(sse_text("ok")))
        .mount(&server)
        .await;

    let mut parent_cfg = test_config(&server.uri());
    parent_cfg.agents.enabled = true;
    parent_cfg.agents.max_spawn_depth = 3;
    parent_cfg.agents.orchestrator_enabled = true;

    let mut ctrl = AgentControl::new(&parent_cfg.agents, "p", 0);
    use cortex::multi_agent::tools::{assemble_delegate_blocklist, can_orchestrate};
    let can = can_orchestrate(&parent_cfg, 0);
    let blocklist = assemble_delegate_blocklist(&parent_cfg, AgentRoleKind::Leaf, can);
    let blocklist_refs: Vec<&str> = blocklist.to_vec();

    let _ = ctrl
        .spawn_and_wait(
            "do not delegate",
            None,
            None,
            AgentRoleKind::Leaf,
            None,
            None,
            None,
            &parent_cfg,
            None,
            Some(&blocklist_refs),
            10,
        )
        .await
        .expect("spawn");

    let received = server.received_requests().await.expect("requests");
    let has_spawn = received.iter().any(|req| {
        let body = body_of(req);
        body.get("tools")
            .and_then(|t| t.as_array())
            .map(|arr| {
                arr.iter().any(|tool| {
                    tool.get("function")
                        .and_then(|f| f.get("name"))
                        .and_then(|n| n.as_str())
                        == Some("spawn_agent")
                })
            })
            .unwrap_or(false)
    });
    assert!(
        !has_spawn,
        "leaf child must never have spawn_agent in its tool list, even with kill switch on"
    );
}

/// REGRESSION: `agents.heartbeat_interval_secs = 0` is the explicit
/// disable sentinel — the bus must see NO Heartbeat events for the
/// child's whole lifetime. A previous version clamped to `max(1)` and
/// silently produced a 1Hz tick storm instead.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn heartbeat_interval_zero_disables_events() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(sse_text("ok"))
                .set_delay(Duration::from_millis(2500)),
        )
        .mount(&server)
        .await;

    let mut parent_cfg = test_config(&server.uri());
    parent_cfg.agents.heartbeat_interval_secs = 0; // explicit disable

    let bus = AgentTreeBus::new();
    let mut rx = bus.subscribe();
    let mut ctrl = AgentControl::new(&parent_cfg.agents, "parent", 0).with_bus(bus);

    let (agent_id, _) = ctrl
        .spawn_agent(
            "any",
            None,
            None,
            AgentRoleKind::Leaf,
            None,
            None,
            None,
            &parent_cfg,
            None,
            None,
        )
        .await
        .expect("spawn");

    let mut heartbeats = 0;
    let mut stalls = 0;
    let deadline = tokio::time::Instant::now() + Duration::from_millis(2000);
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(200), rx.recv()).await {
            Ok(Ok(ev)) => match ev.kind {
                AgentTreeEventKind::Heartbeat => heartbeats += 1,
                AgentTreeEventKind::Stalled => stalls += 1,
                _ => {}
            },
            _ => continue,
        }
    }

    let _ = ctrl.shutdown_agent(&agent_id).await;
    assert_eq!(
        heartbeats, 0,
        "heartbeat_interval_secs=0 must emit zero Heartbeat events, got {heartbeats}"
    );
    assert_eq!(
        stalls, 0,
        "disabled heartbeat must also disable Stalled escalation, got {stalls}"
    );
}
