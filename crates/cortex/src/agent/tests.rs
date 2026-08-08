use super::tool_classification::is_mutating_tool;
use super::tool_execution::dispatch_parallel_safe_tool;
use super::*;
use crate::types::ToolOutput;

// ── Prompt cache: stable prefix ordering ──
//
// `build_system_prompt` must emit its stable (cache-friendly) sections
// BEFORE the dynamic environment section, otherwise per-turn content
// (time, git status) invalidates the cached prefix. These guard tests
// read the source of `build_system_prompt` and fail if the order is
// perturbed. The full end-to-end behavior is exercised indirectly
// through the Anthropic request tests in `llm.rs`.

const AGENT_RS_SRC: &str = include_str!("mod.rs");
const SYSTEM_PROMPT_RS_SRC: &str = include_str!("system_prompt.rs");
const TOOL_EXECUTION_RS_SRC: &str = include_str!("tool_execution.rs");
const STREAM_HANDLER_RS_SRC: &str = include_str!("stream_handler.rs");

fn extract_build_system_prompt_body() -> &'static str {
    let start = SYSTEM_PROMPT_RS_SRC
        .find("async fn build_system_prompt")
        .expect("build_system_prompt must exist in system_prompt.rs");
    // End at the next item's doc comment rather than a byte count: a fixed
    // window silently truncates the body when the function grows, and a guard
    // that stops finding the call it checks for panics on `expect` instead of
    // reporting the ordering it exists to protect.
    let end = SYSTEM_PROMPT_RS_SRC[start..]
        .find("\n    /// ")
        .map_or(SYSTEM_PROMPT_RS_SRC.len(), |offset| start + offset);
    &SYSTEM_PROMPT_RS_SRC[start..end]
}

#[test]
fn build_system_prompt_environment_is_after_security_policy() {
    let body = extract_build_system_prompt_body();
    let security_idx = body
        .find("security_policy")
        .expect("security_policy section must exist in build_system_prompt");
    let env_idx = body
        .find("build_environment_section")
        .expect("build_environment_section call must exist in build_system_prompt");
    assert!(
        env_idx > security_idx,
        "build_environment_section must be pushed AFTER security_policy \
         to keep the cached prefix stable across turns \
         (env_idx={env_idx}, security_idx={security_idx})",
    );
}

#[test]
fn build_system_prompt_environment_is_after_identity() {
    let body = extract_build_system_prompt_body();
    let identity_idx = body
        .find("system_instructions")
        .expect("system_instructions (identity) section must exist");
    let env_idx = body
        .find("build_environment_section")
        .expect("build_environment_section call must exist");
    assert!(
        env_idx > identity_idx,
        "dynamic environment section must come after identity",
    );
}

#[test]
fn build_system_prompt_environment_is_after_tooling() {
    let body = extract_build_system_prompt_body();
    let tooling_idx = body
        .find("build_tooling_section")
        .expect("build_tooling_section call must exist");
    let env_idx = body
        .find("build_environment_section")
        .expect("build_environment_section call must exist");
    assert!(
        env_idx > tooling_idx,
        "dynamic environment section must come after tooling",
    );
}

#[test]
fn build_system_prompt_active_memory_in_dynamic_suffix() {
    // Cache-stability invariant: <active_memory> changes per turn (its
    // content is keyed off the latest user message), so it MUST live in
    // the DYNAMIC SUFFIX block — never before, where it would invalidate
    // the prefix cache on every turn.
    let body = extract_build_system_prompt_body();
    let suffix_marker = body
        .find("DYNAMIC SUFFIX")
        .expect("DYNAMIC SUFFIX marker must exist");
    let active_idx = body
        .find("build_active_memory_section")
        .expect("build_active_memory_section call must exist in build_system_prompt");
    assert!(
        active_idx > suffix_marker,
        "active_memory must be pushed AFTER the DYNAMIC SUFFIX marker \
         so it does not invalidate the prefix cache \
         (active_idx={active_idx}, suffix_marker={suffix_marker})",
    );
}

// ── Stream error-recovery guards ──
//
// `run_agent_loop` preserves partial assistant text *and* includes the
// upstream error reason in the persisted message when the LLM stream
// errors mid-response, so a resumed/replayed conversation can see that
// the turn was cut short rather than appearing as a silent completion.
// These guards assert the source-level structure so regressions surface
// in CI instead of in a support ticket six months later.

#[test]
fn run_agent_loop_captures_stream_error_reason() {
    assert!(
        AGENT_RS_SRC.contains("let mut stream_error: Option<String> = None;"),
        "run_agent_loop must declare a `stream_error` local — the handler for \
         StreamEvent::Error stores the reason there so the persisted message \
         can name the failure"
    );
    assert!(
        STREAM_HANDLER_RS_SRC.contains("stream_error = Some(e.clone());"),
        "StreamEvent::Error arm must clone the error into `stream_error` before \
         forwarding to the TUI; otherwise the error string is lost by the time \
         we persist the partial message"
    );
}

#[test]
fn run_agent_loop_diverts_on_stream_error_before_tool_exec() {
    // Invariant: when the LLM stream errors mid-response, the loop MUST NOT
    // call execute_tool_calls — partial tool-call JSON may be truncated.
    // Without this divert, a 429 / network blip that happened to flush
    // partial tool deltas hangs the TUI: the loop executes garbage tools
    // and re-loops without ever emitting TurnComplete. The fix is a
    // `stream_error.is_some()` early-return to persist_discarded_tool_calls
    // *before* the tool_calls / validate_tool_calls branches.
    let exec_idx = AGENT_RS_SRC
        .find("self.execute_tool_calls(")
        .expect("execute_tool_calls call must exist in run_agent_loop");
    let preceding = &AGENT_RS_SRC[..exec_idx];
    let divert_idx = preceding
        .rfind("if stream_error.is_some()")
        .expect("run_agent_loop must short-circuit to persist_discarded_tool_calls when stream_error.is_some() — before reaching execute_tool_calls — or the TUI hangs on 429+partial-tool-call");
    let divert_tail = &AGENT_RS_SRC[divert_idx..exec_idx];
    assert!(
        divert_tail.contains("persist_discarded_tool_calls"),
        "the stream_error divert must call persist_discarded_tool_calls (which emits TurnComplete) before any tool execution"
    );
}

#[test]
fn run_agent_loop_emits_turn_complete_on_stream_panic() {
    // Invariant: the LLM-stream-task panic branch must emit TurnComplete
    // before returning Err. The function's caller treats Err as a hard
    // failure and propagates it up, but the TUI runtime listens on the
    // event channel — without this TurnComplete the spinner hangs even
    // though the agent has given up.
    let panic_idx = AGENT_RS_SRC
        .find("LLM stream task panicked: ")
        .expect("panic-branch tracing::error message must exist");
    let panic_tail = &AGENT_RS_SRC[panic_idx..];
    let return_idx = panic_tail
        .find("return Err(anyhow::anyhow!(\"LLM stream task panicked\"))")
        .expect("panic branch must return Err");
    let between = &panic_tail[..return_idx];
    assert!(
        between.contains("AgentEvent::TurnComplete"),
        "panic branch must send AgentEvent::TurnComplete before returning Err"
    );
}

#[test]
fn run_agent_loop_persists_interruption_marker() {
    assert!(
        AGENT_RS_SRC.contains("[stream interrupted:"),
        "run_agent_loop must embed a `[stream interrupted: …]` marker in the \
         persisted assistant message when a mid-stream error happens, so the \
         next turn (and any later conversation replay) can tell a truncated \
         response from a completed one"
    );
}

// ── Autocompaction trigger wiring ──
//
// Regression guard for "ctx 100% but autocompaction never ran." The fix
// for the user-visible bug was in the TUI percent calc, but the agent
// wiring is what makes compaction actually fire — if the call site at
// run_agent_loop is dropped, history grows unbounded until an LLM error
// and the user has no way to know why. Source-grep guard, since
// exercising the full Agent constructor in a unit test is too expensive.

#[test]
fn run_agent_loop_calls_compact_history_if_needed_before_each_request() {
    // The dispatch must happen INSIDE the per-iteration loop and BEFORE
    // prepare_llm_request — otherwise an over-budget history hits the
    // LLM unmodified.
    let compact_idx = AGENT_RS_SRC
        .find("self.compact_history_if_needed(&event_tx).await?;")
        .expect(
            "run_agent_loop must call self.compact_history_if_needed(&event_tx).await?; \
             dropping this call lets history grow past max_history_tokens until the LLM \
             returns a context-length error, which the user perceives as a hang",
        );
    let prepare_idx = AGENT_RS_SRC
        .find("self.prepare_llm_request(iteration)")
        .expect("prepare_llm_request call must exist in run_agent_loop");
    assert!(
        compact_idx < prepare_idx,
        "compact_history_if_needed must run BEFORE prepare_llm_request \
         (compact_idx={compact_idx}, prepare_idx={prepare_idx})",
    );
}

#[test]
fn require_str_param_present() {
    let args = serde_json::json!({"name": "hello"});
    let val = tool_handlers::require_str_param(&args, "name").unwrap();
    assert_eq!(val, "hello");
}

#[test]
fn require_str_param_missing() {
    let args = serde_json::json!({"other": "value"});
    let err = tool_handlers::require_str_param(&args, "name").unwrap_err();
    assert!(err.to_string().contains("name"));
}

#[test]
fn require_str_param_null() {
    let args = serde_json::json!({"name": null});
    assert!(tool_handlers::require_str_param(&args, "name").is_err());
}

#[test]
fn require_str_param_wrong_type() {
    let args = serde_json::json!({"name": 42});
    assert!(tool_handlers::require_str_param(&args, "name").is_err());
}

// NOTE: `classify_action` is exhaustively covered (full match-arm table +
// unknown fallback) by `tool_classification::tests::classify_action_*`. No
// duplicate table here.

#[test]
fn mutating_tools_are_blocked_in_plan_mode() {
    // Plan mode security boundary: these tools must be blocked. If a new
    // mutating tool is added and someone forgets to tag it mutating, plan
    // mode turns into execute mode silently.
    assert!(is_mutating_tool("apply_patch"));
    assert!(is_mutating_tool("apply_skill_patch"));
    assert!(is_mutating_tool("create_channel"));
    assert!(is_mutating_tool("run_shell"));
    assert!(is_mutating_tool("write_memory"));
    assert!(is_mutating_tool("browser"));
    assert!(is_mutating_tool("schedule"));
    assert!(is_mutating_tool("generate_image"));
}

#[test]
fn non_mutating_tools_allowed_in_plan_mode() {
    // These should be allowed (non-mutating)
    assert!(!is_mutating_tool("read_file"));
    assert!(!is_mutating_tool("list_dir"));
    assert!(!is_mutating_tool("list"));
    assert!(!is_mutating_tool("read_memory"));
    assert!(!is_mutating_tool("memory_search"));
    assert!(!is_mutating_tool("web_fetch"));
    assert!(!is_mutating_tool("web_search"));
    // request_user_input is now allowlisted: clarification questions don't
    // mutate state and are essential to Plan-mode requirement gathering.
    assert!(!is_mutating_tool("request_user_input"));
}

/// Build a minimal in-memory `Agent` for unit tests that exercise prompt /
/// section assembly. No DB, network, or sub-agent control — just enough state
/// for the section builders to run.
fn minimal_test_agent(config: Config) -> Agent {
    let session = Session::new();
    let session_id = session.meta.id.clone();
    Agent {
        config: config.clone(),
        history: Vec::new(),
        session,
        policy: config.policy.clone(),
        hook_registry: HookRegistry::new(),
        turn_count: 0,
        storage_fault: None,
        rate_guard: SessionRateGuard::new(config.security.action_limits.clone()),
        agent_control: None,
        spawn_depth: 0,
        tools_filter: None,
        skill_env_allowlist: std::collections::HashSet::new(),
        metrics: BorgMetrics::default(),
        browser_session: None,
        tts_synthesizer: None,
        config_rx: None,
        db: Mutex::new(None),
        caches: SystemPromptCache::with_test_state(None, Some(HashMap::new()), None, None, None),
        ghost_commit: None,
        execution_context: crate::execution_context::ExecutionContext::unrestricted(
            std::path::PathBuf::from("."),
        ),
        git_repo_root: None,
        config_arc: Arc::new(config),
        steer_rx: None,
        recent_tool_names: std::collections::HashSet::new(),
        cost_event_sink: None,
        short_term_memory: crate::memory::short_term_memory::ShortTermMemory::new(session_id, 2000),
        tool_call_repeat: ToolCallRepeatTracker::default(),
        expensive_tool_budget: super::tool_validation::ExpensiveToolBudget::default(),
        unknown_tool_tracker: UnknownToolTracker::default(),
        turn_reasoning: String::new(),
        tool_intent_signalled: false,
        nudged_for_dropped_tools: false,
        llm_role: LlmRole::Primary,
        goal_runtime: crate::goals::runtime::GoalRuntimeState::new(),
        goal_event_sink: None,
        auto_title_spawned: false,
        active_failover_identity: None,
        compaction_strikes: super::prompt_assembly::CompactionStrikes::new(),
        compaction_outage: None,
        stream_health: super::stream_handler::StreamHealth::new(),
        prompt_cache_scope: None,
    }
}

/// A `[compaction]` override that stops working (retired model slug, dead
/// key) must not wedge compaction forever. The summarizer bails without
/// mutating history, so without a fallback every future turn stays over
/// budget and repeats the failure. This drives the real path end-to-end:
/// override client 404s → one retry with the primary client compacts.
#[tokio::test]
async fn compaction_retries_with_primary_model_when_override_fails() {
    use wiremock::matchers::{body_string_contains, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    // The `[compaction]` override model has been retired by the provider.
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(body_string_contains("dead-compaction-model"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    // The primary model still summarizes fine. The summarizer rides the
    // streaming wire path (liveness-based timeouts), so the mock answers
    // in SSE.
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(body_string_contains("live-primary-model"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            concat!(
                "data: {\"choices\":[{\"delta\":{\"content\":\"summary of earlier work\"}}]}\n\n",
                "data: [DONE]\n\n",
            ),
            "text/event-stream",
        ))
        .mount(&server)
        .await;

    std::env::set_var("BORG_COMPACTION_FALLBACK_TEST_KEY", "test-key");
    let mut config = Config::default();
    config.llm.provider = Some("openai".into());
    config.llm.model = "live-primary-model".into();
    config.llm.base_url = Some(format!("{}/v1/chat/completions", server.uri()));
    config.llm.api_key_env = "BORG_COMPACTION_FALLBACK_TEST_KEY".into();
    config.llm.max_retries = 0;
    config.compaction.model = Some("dead-compaction-model".into());
    // Tiny budget so LLM compaction triggers on a small history; keep the
    // pre-compaction memory flush out of the way (it would also hit the LLM).
    config.conversation.max_history_tokens = 200;
    config.memory.flush_before_compaction = false;

    let mut agent = minimal_test_agent(config);
    for i in 0..40 {
        agent.history.push(crate::types::Message::user(format!(
            "message {i}: {}",
            "long filler text about ongoing work ".repeat(20)
        )));
    }
    let before_len = agent.history.len();
    let before_tokens = crate::conversation::history_tokens(&agent.history);
    let budget = agent.effective_history_budget();
    assert!(
        before_tokens > budget,
        "test setup: history ({before_tokens} tokens) must exceed budget ({budget})"
    );

    let (tx, _rx) = mpsc::channel(64);
    agent
        .compact_history_if_needed(&tx)
        .await
        .expect("compaction must not surface an error");

    assert!(
        agent.history.len() < before_len,
        "override model failed, so the primary-model retry must have compacted \
         history ({before_len} → {} messages)",
        agent.history.len()
    );
}

/// Sub-agents must reuse the spawning tree's prompt-cache key — parent and
/// child share most of their prompt prefix, and OpenAI-family caches only hit
/// when both `prompt_cache_key` and prefix match. A root agent (no scope)
/// keys on its own session so unrelated sessions never share routing, and
/// setting a scope must also re-scope the agent's controller — otherwise a
/// depth-2 grandchild would be spawned under its immediate parent's session
/// and silently land on a different cache shard than the rest of the tree.
#[test]
fn prompt_cache_scope_propagates_to_controller_for_grandchildren() {
    let mut config = Config::default();
    config.agents.enabled = true;
    let mut agent = minimal_test_agent(config.clone());
    let own_session = agent.session.meta.id.clone();
    agent.agent_control = Some(crate::multi_agent::AgentControl::new(
        &config.agents,
        &own_session,
        1,
    ));

    assert_eq!(
        agent.prompt_cache_key(),
        own_session,
        "root agent must key the prompt cache on its own session"
    );
    assert_eq!(
        agent
            .agent_control
            .as_ref()
            .expect("control")
            .prompt_cache_scope(),
        own_session,
        "unscoped controller hands children its own session"
    );

    agent.set_prompt_cache_scope("root-session-123".to_string());
    assert_eq!(
        agent.prompt_cache_key(),
        "root-session-123",
        "spawned agent must key the prompt cache on the tree's root session"
    );
    assert_eq!(
        agent
            .agent_control
            .as_ref()
            .expect("control")
            .prompt_cache_scope(),
        "root-session-123",
        "controller must hand grandchildren the SAME root scope, not this \
         agent's own session"
    );
}

/// After a provider failover, the `<environment>` runtime line must report the
/// model that actually served the response — not the configured primary — so
/// the model identifies itself truthfully during an outage. Guards the
/// `active_failover_identity` → environment-section wiring.
#[tokio::test]
async fn environment_section_reports_failover_model_identity() {
    let mut config = Config::default();
    config.llm.provider = Some("anthropic".to_string());
    config.llm.model = "claude-primary".to_string();
    let mut agent = minimal_test_agent(config);

    // No failover this turn: report the configured primary, no marker.
    let env = agent.build_environment_section().await;
    assert!(env.contains("provider=anthropic"), "got: {env}");
    assert!(env.contains("model=claude-primary"), "got: {env}");
    assert!(
        !env.contains("failover active"),
        "no failover marker when the primary serves: {env}"
    );

    // A failover occurred: report the slot that actually served the response.
    agent.active_failover_identity = Some(("openai".to_string(), "gpt-fallback".to_string()));
    let env = agent.build_environment_section().await;
    assert!(
        env.contains("provider=openai (failover active)"),
        "got: {env}"
    );
    assert!(env.contains("model=gpt-fallback"), "got: {env}");
    assert!(
        !env.contains("model=claude-primary"),
        "must not still report the primary model after failover: {env}"
    );
}

/// Spec: once the summarizer's provider has rejected us in a way retrying
/// cannot fix, proactive compaction must stop attempting it. Without the
/// latch the agent re-pays for the identical rejection on every single turn,
/// and re-announces it every time.
#[tokio::test]
async fn latched_compaction_outage_skips_proactive_compaction() {
    let _mirror = super::prompt_assembly::lock_compaction_outage_mirror().await;
    let mut config = Config::default();
    // Force the history over budget so the LLM-compaction block is reached.
    config.conversation.max_history_tokens = 32;
    let mut agent = minimal_test_agent(config);
    agent.history = vec![
        Message::user("a".repeat(2000)),
        Message::assistant("b".repeat(2000)),
        Message::user("c".repeat(2000)),
    ];
    agent.compaction_outage = Some(super::prompt_assembly::CompactionOutage {
        category: crate::error_format::ErrorCategory::Auth,
        turns_until_retry: super::prompt_assembly::COMPACTION_OUTAGE_RETRY_TURNS,
        streak: 1,
        announced: true,
    });

    let before: Vec<Option<String>> = agent
        .history
        .iter()
        .map(|m| m.text_content().map(str::to_string))
        .collect();
    let (tx, mut rx) = mpsc::channel(16);

    agent
        .compact_history_if_needed(&tx)
        .await
        .expect("a latched outage is not an error — it just skips the pass");

    let after: Vec<Option<String>> = agent
        .history
        .iter()
        .map(|m| m.text_content().map(str::to_string))
        .collect();
    assert_eq!(
        after, before,
        "history must be left untouched while the summarizer is known-broken"
    );
    assert!(
        rx.try_recv().is_err(),
        "the outage was already announced once; it must not be re-announced every turn"
    );
    assert_eq!(
        agent
            .compaction_outage
            .expect("latch must persist while cooling down")
            .turns_until_retry,
        super::prompt_assembly::COMPACTION_OUTAGE_RETRY_TURNS - 1,
        "each skipped turn must burn down the cooldown, or the latch never re-probes"
    );
}

/// Spec: the latch must never be permanent. After the cooldown burns down, one
/// probe is let through — that is what lets a restored key resume compaction
/// without restarting the session.
#[tokio::test]
async fn compaction_outage_cooldown_expires_and_allows_a_probe() {
    let _mirror = super::prompt_assembly::lock_compaction_outage_mirror().await;
    let mut config = Config::default();
    config.conversation.max_history_tokens = 32;
    // No provider configured, so the probe fails at client construction —
    // reaching that error is itself proof the gate let the attempt through.
    config.llm.provider = None;
    let mut agent = minimal_test_agent(config);
    agent.history = vec![
        Message::user("a".repeat(2000)),
        Message::assistant("b".repeat(2000)),
        Message::user("c".repeat(2000)),
    ];
    agent.compaction_outage = Some(super::prompt_assembly::CompactionOutage {
        category: crate::error_format::ErrorCategory::Auth,
        turns_until_retry: 0,
        streak: 1,
        announced: true,
    });

    let (tx, _rx) = mpsc::channel(16);
    let result = agent.compact_history_if_needed(&tx).await;

    assert!(
        result.is_err(),
        "an expired cooldown must let a probe through to the summarizer, \
         not return early like a live latch"
    );
}

/// Spec: only the summarizer actually working clears the latch. A persistent
/// failure announces exactly once, no matter how many probes fail after it.
#[tokio::test]
async fn compaction_outage_announces_once_and_clears_only_on_success() {
    use crate::conversation::SummarizeFailure;
    use crate::error_format::ErrorCategory;

    let _mirror = super::prompt_assembly::lock_compaction_outage_mirror().await;
    let mut agent = minimal_test_agent(Config::default());
    let (tx, mut rx) = mpsc::channel(16);

    // First persistent failure: latch + tell the user.
    agent
        .record_compaction_outcome(
            0,
            Some(SummarizeFailure::Persistent(ErrorCategory::Auth)),
            Some(&tx),
        )
        .await;
    assert!(agent.compaction_outage.is_some(), "must latch");
    let first = rx.try_recv().expect("the user must be told the first time");
    match first {
        AgentEvent::SystemNotice { ref category, .. } => assert_eq!(category, "compaction"),
        _ => panic!("expected a SystemNotice"),
    }

    // A later probe fails the same way: re-arm silently, never re-announce.
    agent
        .record_compaction_outcome(
            0,
            Some(SummarizeFailure::Persistent(ErrorCategory::Auth)),
            Some(&tx),
        )
        .await;
    assert!(
        rx.try_recv().is_err(),
        "a repeat failure must not re-announce — that is the spam the latch exists to stop"
    );
    assert_eq!(
        agent
            .compaction_outage
            .expect("still latched")
            .turns_until_retry,
        super::prompt_assembly::compaction_cooldown_turns(2),
        "a failed probe must re-arm on the NEXT rung of the ladder, not the base \
         cooldown — otherwise a permanently-dead summarizer is re-probed at a \
         fixed rate forever"
    );

    // Only dropped messages prove the summarizer works.
    agent.record_compaction_outcome(3, None, Some(&tx)).await;
    assert!(
        agent.compaction_outage.is_none(),
        "a pass that actually compacted must clear the latch"
    );
}

/// Spec: the cooldown ladder escalates 1x/3x/9x off the base and then stops at
/// the absolute cap. A flat cooldown means a summarizer that is down for good
/// is re-probed at a fixed rate forever; an uncapped one eventually stops
/// probing within any realistic session, so a restored key would never resume.
#[test]
fn compaction_cooldown_escalates_then_clamps() {
    use super::prompt_assembly::{
        compaction_cooldown_turns as cooldown, COMPACTION_OUTAGE_MAX_RETRY_TURNS as CAP,
        COMPACTION_OUTAGE_RETRY_TURNS as BASE,
    };

    // A 0 streak is treated as the first failure, not as "no cooldown" — an
    // uninitialised streak must never mean "probe every turn".
    assert_eq!(cooldown(0), BASE);
    assert_eq!(cooldown(1), BASE);
    assert_eq!(cooldown(2), BASE * 3);
    assert_eq!(cooldown(3), BASE * 9);

    for streak in 4..=64 {
        assert_eq!(
            cooldown(streak),
            CAP,
            "streak {streak} must clamp rather than grow without bound or overflow"
        );
    }
}

/// REGRESSION: the outage lived only on `Agent`, and borgd builds a fresh
/// `Agent` for every scheduled and heartbeat turn — so the latch was absent on
/// each one, the dead summarizer was re-probed at full price every turn, and
/// the cooldown structurally never elapsed. A newly-built agent must inherit
/// the live outage from the process-wide mirror.
#[tokio::test]
async fn compaction_outage_survives_a_fresh_agent() {
    use crate::conversation::SummarizeFailure;
    use crate::error_format::ErrorCategory;

    let _mirror = super::prompt_assembly::lock_compaction_outage_mirror().await;

    let mut first = minimal_test_agent(Config::default());
    let (tx, _rx) = mpsc::channel(16);
    first
        .record_compaction_outcome(
            0,
            Some(SummarizeFailure::Persistent(ErrorCategory::Auth)),
            Some(&tx),
        )
        .await;

    let inherited = super::prompt_assembly::load_compaction_outage()
        .expect("a live outage must be visible to the next agent built in this process");
    assert_eq!(
        inherited.streak, 1,
        "the streak must carry so the next failure escalates instead of restarting the ladder"
    );
    assert_eq!(
        inherited.turns_until_retry,
        super::prompt_assembly::compaction_cooldown_turns(1),
        "the remaining cooldown must carry, or a per-turn agent never reaches the probe"
    );
    assert!(
        !inherited.announced,
        "the notice is per-session: a new agent has never told its user, so inheriting \
         `announced` would swallow the warning entirely"
    );

    // A success anywhere in the process clears it for everyone — the outage is
    // a property of the provider, not of one conversation.
    first.record_compaction_outcome(5, None, Some(&tx)).await;
    assert!(
        super::prompt_assembly::load_compaction_outage().is_none(),
        "a demonstrated success must clear the mirror, not just the local latch"
    );
}

/// The notice must name the failure class. "Compaction is paused" alone sends
/// the user hunting through context settings for a billing problem.
#[tokio::test]
async fn persistent_billing_outage_latches_and_names_the_cause() {
    use crate::conversation::SummarizeFailure;
    use crate::error_format::ErrorCategory;

    let _mirror = super::prompt_assembly::lock_compaction_outage_mirror().await;
    let mut agent = minimal_test_agent(Config::default());
    let (tx, mut rx) = mpsc::channel(16);

    agent
        .record_compaction_outcome(
            0,
            Some(SummarizeFailure::Persistent(ErrorCategory::Billing)),
            Some(&tx),
        )
        .await;

    let outage = agent
        .compaction_outage
        .expect("a billing outage must latch");
    assert_eq!(outage.category, ErrorCategory::Billing);
    assert!(outage.announced);
    let notice = rx.try_recv().expect("must announce");
    match notice {
        AgentEvent::SystemNotice { text, .. } => assert!(
            text.contains("billing"),
            "the notice must name the cause so the user knows what to fix: {text}"
        ),
        _ => panic!("expected a SystemNotice"),
    }
}

/// A transient failure must keep normal retry semantics — latching on a blip
/// would stop compacting for ten turns over a single 503.
#[tokio::test]
async fn transient_compaction_failure_does_not_latch() {
    use crate::conversation::SummarizeFailure;

    let _mirror = super::prompt_assembly::lock_compaction_outage_mirror().await;
    let mut agent = minimal_test_agent(Config::default());
    let (tx, mut rx) = mpsc::channel(16);

    agent
        .record_compaction_outcome(0, Some(SummarizeFailure::Transient), Some(&tx))
        .await;

    assert!(
        agent.compaction_outage.is_none(),
        "a transient failure must not suspend compaction"
    );
    assert!(rx.try_recv().is_err(), "and must not nag the user");
}

#[test]
fn skill_tool_write_refreshes_cached_skill_state() {
    let mut agent = minimal_test_agent(Config::default());
    agent.caches = SystemPromptCache::with_test_state(
        None,
        Some(HashMap::new()),
        Some("cached".to_string()),
        Some("old request".to_string()),
        Some(vec!["pdf".to_string()]),
    );

    agent.maybe_refresh_skills_after_tool(
        tn::APPLY_PATCH,
        &serde_json::json!({"target": "skills"}),
        "Patch applied successfully.",
    );

    assert!(agent.caches.skills_context_for_test().is_none());
    assert!(agent.caches.skills_request_for_test().is_none());
    assert!(agent.caches.last_skill_drop_signature_for_test().is_none());
    assert!(
        agent.skill_env_allowlist.contains("SLACK_BOT_TOKEN"),
        "skill env allowlist should be recomputed from loaded skills"
    );
}

/// `reload_config` must clear the skills caches (the env-allowlist they
/// were rendered against is stale) and replace the credentials wholesale,
/// while preserving identity (config does not affect IDENTITY.md). This
/// is the Agent-side end-to-end of `SystemPromptCache::on_config_reloaded`.
#[test]
fn reload_config_clears_skills_and_replaces_credentials_preserving_identity() {
    let mut agent = minimal_test_agent(Config::default());
    agent.caches = SystemPromptCache::with_test_state(
        Some("identity-snapshot".into()),
        Some(HashMap::from([("OLD_TOKEN".into(), "v".into())])),
        Some("rendered-skills".into()),
        Some("user-request".into()),
        Some(vec!["pdf".into()]),
    );

    agent.reload_config(Config::default());

    assert_eq!(
        agent.caches.identity_for_test(),
        Some("identity-snapshot"),
        "identity must survive config reload"
    );
    assert!(
        agent.caches.skills_context_for_test().is_none(),
        "skills context must clear on reload"
    );
    assert!(
        agent.caches.skills_request_for_test().is_none(),
        "skills request key must clear on reload"
    );
    assert!(
        agent.caches.last_skill_drop_signature_for_test().is_none(),
        "drop signature must clear on reload"
    );
    assert!(
        !agent.credentials_cached().contains_key("OLD_TOKEN"),
        "old credentials must be replaced wholesale"
    );
}

/// Starting a new session must drop the system-prompt input caches, or a
/// long-lived `Agent` reused across the reset would render the next session's
/// prompt from the prior session's cached IDENTITY.md / credential values —
/// a stale-context leak across the session boundary.
#[tokio::test]
async fn new_session_clears_system_prompt_caches() {
    let mut agent = minimal_test_agent(Config::default());
    agent.caches = SystemPromptCache::with_test_state(
        Some("prior-identity".into()),
        Some(HashMap::from([("PRIOR_TOKEN".into(), "v".into())])),
        Some("prior-skills".into()),
        Some("prior-request".into()),
        Some(vec!["pdf".into()]),
    );

    agent.new_session().await;

    assert_eq!(
        agent.caches.identity_for_test(),
        None,
        "prior-session identity must not survive a new session"
    );
    assert!(
        !agent.credentials_cached().contains_key("PRIOR_TOKEN"),
        "prior-session credentials must not survive a new session"
    );
    assert!(
        agent.caches.skills_context_for_test().is_none(),
        "prior-session skills context must not survive a new session"
    );
}

#[test]
fn unknown_tools_default_to_mutating() {
    // New/unknown tools should be blocked by default (safety)
    assert!(is_mutating_tool("some_new_tool"));
    assert!(is_mutating_tool(""));
}

#[tokio::test]
async fn spawn_logged_completes_normally() {
    // A non-panicking task should complete without issues
    let (tx, rx) = tokio::sync::oneshot::channel();
    spawn_logged("test_normal", async move {
        let _ = tx.send(42);
    });
    let result = rx.await;
    assert_eq!(result.unwrap(), 42);
}

#[tokio::test]
async fn spawn_logged_handles_panic() {
    // A panicking task should not propagate the panic to the caller
    spawn_logged("test_panic", async {
        panic!("intentional test panic");
    });
    // Give the spawned task time to run and panic
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    // If we get here, the panic was handled (not propagated)
}

#[tokio::test]
async fn dispatch_parallel_safe_tool_read_file_returns_content() {
    use std::io::Write;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("data.txt");
    std::fs::File::create(&path)
        .unwrap()
        .write_all(b"hello world")
        .unwrap();

    let config = Config::default();
    let (tx, _rx) = mpsc::channel(8);

    let args = serde_json::json!({"path": path.to_str().unwrap()}).to_string();
    let out = dispatch_parallel_safe_tool("read_file", &args, &config, &tx, None)
        .await
        .unwrap();
    let text = match out {
        ToolOutput::Text(t) => t,
        ToolOutput::Multimodal { text, .. } => text,
    };
    assert!(text.contains("hello world"), "got: {text}");
}

#[tokio::test]
async fn dispatch_parallel_safe_tool_rejects_unsafe_tool_name() {
    // Guard: the planner should never route an Unknown-effect tool here.
    // If it does, the dispatcher surfaces a loud error rather than
    // silently misbehaving.
    let config = Config::default();
    let (tx, _rx) = mpsc::channel(8);
    let result = dispatch_parallel_safe_tool("run_shell", "{}", &config, &tx, None).await;
    assert!(result.is_err(), "unsafe tool must return Err");
    let msg = format!("{}", result.unwrap_err());
    assert!(
        msg.contains("non-parallel-safe") || msg.contains("planner/dispatch mismatch"),
        "error should name the mismatch: {msg}"
    );
}

#[tokio::test]
async fn dispatch_parallel_safe_tool_invalid_json_returns_text() {
    let config = Config::default();
    let (tx, _rx) = mpsc::channel(8);
    let out = dispatch_parallel_safe_tool("read_file", "not json", &config, &tx, None)
        .await
        .unwrap();
    let text = match out {
        ToolOutput::Text(t) => t,
        _ => panic!("expected text output"),
    };
    assert!(text.contains("Invalid JSON"), "got: {text}");
}

#[tokio::test]
async fn dispatch_parallel_safe_tool_rejects_non_object_args() {
    // Valid JSON that is not an object (bare string / number / array / bool)
    // must fail closed with a structured error and NOT execute the tool —
    // otherwise a `#[serde(transparent)]`-over-`Value` handler would coerce it
    // to an empty arg set and walk into dispatch.
    let config = Config::default();
    let (tx, _rx) = mpsc::channel(8);
    for bad in ["\"/etc/passwd\"", "42", "[1,2,3]", "true"] {
        let out = dispatch_parallel_safe_tool("read_file", bad, &config, &tx, None)
            .await
            .unwrap();
        let text = match out {
            ToolOutput::Text(t) => t,
            _ => panic!("expected text output for {bad}"),
        };
        assert!(
            text.contains("must be a JSON object") && text.contains("was not executed"),
            "arg {bad:?} must be rejected as non-object, got: {text}"
        );
    }
}

#[tokio::test]
async fn dispatch_parallel_safe_tool_recovers_double_encoded_object_args() {
    // A whole-arguments object double-encoded as a JSON string is a known
    // open-weight-model failure mode. The non-object guard must NOT reject it:
    // it unwraps the encoded object so the call still runs (the coercion layer
    // relies on this recovery). Regression guard — an over-eager guard here
    // silently drops otherwise-valid tool calls.
    let config = Config::default();
    let (tx, _rx) = mpsc::channel(8);
    // args_json is a JSON *string* whose contents are the real argument object.
    let double_encoded = serde_json::to_string(&r#"{"path":"/etc/hostname"}"#).unwrap();
    let out = dispatch_parallel_safe_tool("read_file", &double_encoded, &config, &tx, None)
        .await
        .unwrap();
    let text = match out {
        ToolOutput::Text(t) => t,
        _ => panic!("expected text output"),
    };
    assert!(
        !text.contains("must be a JSON object"),
        "double-encoded object args must be recovered, not rejected: {text}"
    );
}

#[tokio::test]
async fn dispatch_parallel_safe_tool_null_args_normalize_to_empty_object() {
    // `null` is the conventional "no arguments" form: it is normalized to `{}`
    // and reaches arg parsing, so a no-argument tool still runs and an
    // argument-requiring tool fails at *parsing*, not with the non-object
    // rejection.
    let config = Config::default();
    let (tx, _rx) = mpsc::channel(8);

    // list_skills takes no args → null-as-{} must not be rejected as non-object.
    let out = dispatch_parallel_safe_tool("list_skills", "null", &config, &tx, None)
        .await
        .unwrap();
    let text = match out {
        ToolOutput::Text(t) => t,
        ToolOutput::Multimodal { text, .. } => text,
    };
    assert!(
        !text.contains("must be a JSON object"),
        "null args to a no-arg tool must not be rejected as non-object: {text}"
    );

    // read_file requires `path` → null-as-{} reaches parse and fails there.
    let out = dispatch_parallel_safe_tool("read_file", "null", &config, &tx, None)
        .await
        .unwrap();
    let text = match out {
        ToolOutput::Text(t) => t,
        _ => panic!("expected text output"),
    };
    assert!(
        text.contains("Invalid arguments") && !text.contains("must be a JSON object"),
        "null to read_file should fail at arg parsing, not non-object reject: {text}"
    );
}
//
// The two `setup_template_*` template-rendering tests live alongside
// `SETUP_TEMPLATE` in `agent::system_prompt`. The lifecycle tests below
// exercise `consume_setup_template`, the real driver used by
// `build_system_prompt`.

#[tokio::test]
async fn setup_absent_returns_none() {
    // No SETUP.md on disk → nothing to inject. The pre-onboarding and
    // post-retirement steady states both look like this.
    let tmp = tempfile::tempdir().unwrap();
    let outcome = super::system_prompt::consume_setup_template(tmp.path(), false).await;
    assert!(outcome.is_none());
}

#[tokio::test]
async fn setup_persists_across_turns_until_identity_written() {
    // Spec: SETUP.md must remain in the system prompt for the entire
    // first conversation. Turn 1 sets `first_turn = true` (drives the
    // one-shot tool-stripping); turn 2 keeps the content but flips
    // `first_turn` to false so tools come back online.
    let tmp = tempfile::tempdir().unwrap();
    let setup_path = tmp.path().join("SETUP.md");
    std::fs::write(&setup_path, "# First Boot\nRules.").unwrap();

    let turn1 = super::system_prompt::consume_setup_template(tmp.path(), false)
        .await
        .expect("turn 1 must inject SETUP.md");
    assert!(turn1.content.contains("First Boot"));
    assert!(turn1.first_turn, "turn 1 must be marked first_turn");
    assert!(
        setup_path.exists(),
        "SETUP.md must survive turn 1 for turn 2 to re-inject"
    );
    assert!(
        tmp.path().join("SETUP.md.first_turn_done").exists(),
        "marker file must exist after turn 1"
    );

    let turn2 = super::system_prompt::consume_setup_template(tmp.path(), false)
        .await
        .expect("turn 2 must still inject SETUP.md while onboarding incomplete");
    assert!(turn2.content.contains("First Boot"));
    assert!(
        !turn2.first_turn,
        "turn 2 must not re-trigger tool suppression"
    );
}

#[tokio::test]
async fn setup_retired_when_identity_in_long_term_memory() {
    // Spec: once the agent has shaped its identity (an IDENTITY entry
    // exists in long-term memory), the first-conversation rules no
    // longer apply. SETUP.md and its marker must be deleted from disk
    // and the helper must return `None`.
    let tmp = tempfile::tempdir().unwrap();
    let setup_path = tmp.path().join("SETUP.md");
    let marker_path = tmp.path().join("SETUP.md.first_turn_done");
    std::fs::write(&setup_path, "rules").unwrap();
    std::fs::write(&marker_path, "").unwrap();

    let outcome = super::system_prompt::consume_setup_template(tmp.path(), true).await;
    assert!(outcome.is_none(), "retirement must suppress injection");
    assert!(!setup_path.exists(), "SETUP.md must be deleted on retire");
    assert!(!marker_path.exists(), "marker must be deleted on retire");
}

#[tokio::test]
async fn setup_retirement_is_idempotent() {
    // Spec: post-retirement, every subsequent build is a no-op. The
    // helper must not error if SETUP.md is already gone, regardless of
    // the `onboarding_complete` flag.
    let tmp = tempfile::tempdir().unwrap();
    let with_flag = super::system_prompt::consume_setup_template(tmp.path(), true).await;
    let without_flag = super::system_prompt::consume_setup_template(tmp.path(), false).await;
    assert!(with_flag.is_none());
    assert!(without_flag.is_none());
}

#[tokio::test]
async fn setup_first_turn_marker_is_load_bearing() {
    // Spec: tool stripping fires exactly once per SETUP.md instance.
    // If the marker exists before SETUP.md is consumed (e.g. a stale
    // marker survived a crashed retirement), `first_turn` must be
    // false — the new SETUP.md should not re-trigger tool suppression
    // based on filename alone.
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("SETUP.md"), "rules").unwrap();
    std::fs::write(tmp.path().join("SETUP.md.first_turn_done"), "").unwrap();

    let outcome = super::system_prompt::consume_setup_template(tmp.path(), false)
        .await
        .expect("still must inject while onboarding incomplete");
    assert!(
        !outcome.first_turn,
        "pre-existing marker must veto first_turn so tools stay available"
    );
}

#[tokio::test]
async fn setup_first_turn_is_exclusive_across_concurrent_callers() {
    // Spec: tool stripping is a one-shot side effect of the very first
    // SETUP.md injection. Two sessions racing into `build_system_prompt`
    // must not both think they're turn 1 — `create_new` is the
    // synchronization primitive, and a future "simplify" that swaps it
    // for plain `write` would silently regress this invariant.
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("SETUP.md"), "rules").unwrap();
    let (a, b) = tokio::join!(
        super::system_prompt::consume_setup_template(tmp.path(), false),
        super::system_prompt::consume_setup_template(tmp.path(), false),
    );
    let first_turn_count = [&a, &b]
        .iter()
        .filter(|o| o.as_ref().is_some_and(|i| i.first_turn))
        .count();
    assert_eq!(
        first_turn_count, 1,
        "exactly one concurrent caller may see first_turn = true"
    );
    // Both still get the content — re-injection is the point.
    assert!(a.unwrap().content.contains("rules"));
    assert!(b.unwrap().content.contains("rules"));
}

#[tokio::test]
async fn setup_content_reread_each_turn_picks_up_mutations() {
    // Spec: SETUP.md is the source of truth for first-conversation
    // rules. The new design re-reads on every turn so an operator (or a
    // template-upgrade path) can land changes mid-flight. This test
    // pins that contract — caching the content from turn 1 would
    // silently break it.
    let tmp = tempfile::tempdir().unwrap();
    let setup_path = tmp.path().join("SETUP.md");
    std::fs::write(&setup_path, "v1 rules").unwrap();

    let turn1 = super::system_prompt::consume_setup_template(tmp.path(), false)
        .await
        .expect("turn 1 must inject");
    assert!(turn1.content.contains("v1"));

    std::fs::write(&setup_path, "v2 rules").unwrap();
    let turn2 = super::system_prompt::consume_setup_template(tmp.path(), false)
        .await
        .expect("turn 2 must still inject");
    assert!(
        turn2.content.contains("v2"),
        "mid-conversation SETUP.md edits must reach the system prompt"
    );
}

#[tokio::test]
async fn identity_in_long_term_memory_with_db_distinguishes_states() {
    // Spec: only a non-empty global/IDENTITY entry counts as completed
    // onboarding. Missing rows and whitespace-only content must not
    // flip the signal — otherwise SETUP.md would retire prematurely and
    // the agent would lose the first-conversation rules mid-flow.
    use crate::db::Database;
    use crate::memory::identity_in_long_term_memory_with_db;

    let db = Database::test_db().await;

    assert!(
        !identity_in_long_term_memory_with_db(&db).await,
        "no row → onboarding not complete"
    );

    db.upsert_memory_entry("global", "IDENTITY", "   \n  ")
        .await
        .unwrap();
    assert!(
        !identity_in_long_term_memory_with_db(&db).await,
        "whitespace-only content does not count as identity shaped"
    );

    db.upsert_memory_entry(
        "global",
        "IDENTITY",
        "# Borg\nWry, direct, biases toward small reversible changes.",
    )
    .await
    .unwrap();
    assert!(
        identity_in_long_term_memory_with_db(&db).await,
        "real content → onboarding complete"
    );

    // A non-global scope must not trip the signal — multi-tenant
    // installs could otherwise leak retirement across users.
    db.upsert_memory_entry("project:abc", "IDENTITY", "filled")
        .await
        .unwrap();
    db.delete_memory_entry("global", "IDENTITY").await.unwrap();
    assert!(
        !identity_in_long_term_memory_with_db(&db).await,
        "non-global scope must not count"
    );
}

#[test]
fn should_nudge_for_response_matrix() {
    // (text, tools_executed, already_retried, expected_nudge)
    let cases: &[(&str, bool, bool, bool)] = &[
        ("", true, false, true),       // empty after tools → nudge
        ("  \n  ", true, false, true), // whitespace-only counts as empty
        ("Done!", true, false, false), // non-empty text → no nudge
        // No tools ran and still nothing to say: a pure-reasoning first
        // iteration used to end the turn in total silence. One nudge.
        ("", false, false, true),
        ("", true, true, false), // already retried once → no nudge
        ("", false, true, false),
        // A silence marker is real text, so an intentionally-silent
        // autonomous turn is never nudged.
        (crate::agent::silence::SILENT_SENTINEL, false, false, false),
    ];
    for (text, tools, retried, expected) in cases {
        let got = should_nudge_for_response(text, *tools, *retried);
        assert_eq!(
            got, *expected,
            "should_nudge_for_response({text:?}, tools={tools}, retried={retried}) = {got}, expected {expected}"
        );
    }
}

#[tokio::test]
async fn dropped_tool_call_nudges_once_then_terminates() {
    use super::stream_handler::TextOnlyOutcome;
    // A provider that says finish_reason="tool_calls" but ships no tool call
    // must not have its narration accepted as the final answer — on
    // autonomous lanes that is a silent no-op run. Exactly one nudge, then
    // the turn is allowed to end so a persistently-broken provider can't loop.
    let mut agent = minimal_test_agent(Config::default());
    agent.tool_intent_signalled = true;
    let (tx, mut rx) = mpsc::channel(64);

    let outcome = agent
        .handle_text_only_response("Let me check the file...", None, false, false, &tx)
        .await;
    assert!(matches!(outcome, TextOnlyOutcome::NudgeAndContinue));
    let nudge = agent
        .history
        .iter()
        .rev()
        .find(|m| matches!(m.role, crate::types::Role::System))
        .and_then(|m| m.text_content())
        .expect("nudge must be persisted as a system message");
    assert!(
        nudge.contains("no tool call was received"),
        "unexpected nudge text: {nudge}"
    );

    // Second occurrence in the same turn (intent still signalled, guard set):
    // the turn ends normally with the text as the answer.
    agent.tool_intent_signalled = true;
    let outcome = agent
        .handle_text_only_response("Still narrating.", None, false, false, &tx)
        .await;
    assert!(matches!(outcome, TextOnlyOutcome::TerminateTurn));
    // Drain events so the channel send in finalize didn't error.
    while rx.try_recv().is_ok() {}
}

#[tokio::test]
async fn dropped_tool_call_with_empty_text_still_nudges() {
    use super::stream_handler::TextOnlyOutcome;
    // The strongest dropped-call shape: tool-intent finish reason, zero text,
    // zero calls. The tool-intent nudge must win over the empty-response
    // terminal — ending such a turn silently is exactly the failure class.
    let mut agent = minimal_test_agent(Config::default());
    agent.tool_intent_signalled = true;
    let (tx, _rx) = mpsc::channel(64);
    let outcome = agent
        .handle_text_only_response("", None, false, false, &tx)
        .await;
    assert!(
        matches!(outcome, TextOnlyOutcome::NudgeAndContinue),
        "empty text with tool intent must nudge, not terminate"
    );
}

#[tokio::test]
async fn text_only_response_without_tool_intent_is_unchanged() {
    use super::stream_handler::TextOnlyOutcome;
    // A normal stop-reason text answer must terminate exactly as before —
    // the dropped-tool-call path only arms on the provider's signal.
    let mut agent = minimal_test_agent(Config::default());
    let (tx, _rx) = mpsc::channel(64);
    let outcome = agent
        .handle_text_only_response("A complete answer.", None, false, false, &tx)
        .await;
    assert!(matches!(outcome, TextOnlyOutcome::TerminateTurn));
    assert!(
        !agent.nudged_for_dropped_tools,
        "guard must not arm without a provider tool-intent signal"
    );
}

#[tokio::test]
async fn context_breakdown_components_reflect_prompt_and_history() {
    // The breakdown must reflect what the next request would actually send:
    // a non-empty system prompt, tool schemas, and a history component that
    // tracks the real conversation size.
    let mut agent = minimal_test_agent(Config::default());
    agent
        .history
        .push(crate::types::Message::user("hello there, agent"));
    let breakdown = agent.context_breakdown().await.expect("breakdown");

    assert!(breakdown.context_window > 0);
    let labels: Vec<&str> = breakdown
        .components
        .iter()
        .map(|c| c.label.as_str())
        .collect();
    assert!(
        labels.iter().any(|l| l.contains("System prompt")),
        "must report the system prompt: {labels:?}"
    );
    assert!(
        labels.iter().any(|l| l.contains("Tool schemas")),
        "must report tool schemas: {labels:?}"
    );
    let history = breakdown
        .components
        .iter()
        .find(|c| c.label == "Conversation history")
        .expect("history component");
    assert_eq!(
        history.tokens,
        crate::conversation::history_tokens(&agent.history),
        "history component must equal the real history estimate"
    );
    // System prompt and native tool schemas are never empty on a default
    // config — a zero here means a component silently fell out.
    let stable = breakdown
        .components
        .iter()
        .find(|c| c.label.contains("cache-stable"))
        .expect("stable-prefix component");
    assert!(stable.tokens > 0, "stable prefix cannot be empty");
    let native_tools = breakdown
        .components
        .iter()
        .find(|c| c.label == "Tool schemas (native)")
        .expect("native tool component");
    assert!(native_tools.tokens > 0, "native tools cannot be empty");
    // Free space derives from the same numbers, clamped at zero.
    assert_eq!(
        breakdown.free_tokens(),
        breakdown
            .context_window
            .saturating_sub(breakdown.used_tokens())
    );
}

#[test]
fn empty_response_terminal_labels_reasoning_excerpt() {
    // When the model produced reasoning but no visible answer, the turn must
    // not end in silence — the reasoning frequently holds the actual answer.
    // The label is unconditional so it can't read as a normal reply.
    let mut agent = minimal_test_agent(Config::default());
    agent.turn_reasoning = "  The answer is 42 because of the mice.  ".to_string();
    let out = agent.empty_response_terminal();
    assert!(
        out.starts_with(crate::agent::EMPTY_RESPONSE_REASONING_LABEL),
        "excerpt must be labeled: {out}"
    );
    assert!(out.contains("The answer is 42"), "{out}");

    // Nothing at all → an explicit notice, still never silence.
    agent.turn_reasoning.clear();
    assert_eq!(
        agent.empty_response_terminal(),
        crate::agent::EMPTY_RESPONSE_NO_REASONING
    );
    agent.turn_reasoning = "   \n  ".to_string();
    assert_eq!(
        agent.empty_response_terminal(),
        crate::agent::EMPTY_RESPONSE_NO_REASONING,
        "whitespace-only reasoning is not an excerpt"
    );
}

/// Regression: the empty-response terminal must be display-only. Persisting
/// the reasoning excerpt puts chain-of-thought into replayable assistant
/// content, which providers read as injected reasoning — subsequent calls
/// then deterministically return empty responses, re-entering this exact
/// path and wedging the session permanently.
#[tokio::test]
async fn empty_response_persists_neutral_marker_not_reasoning() {
    let mut agent = minimal_test_agent(Config::default());
    let reasoning = "chain-of-thought: the answer is 42 because of the mice";
    agent.turn_reasoning = reasoning.to_string();

    let (tx, mut rx) = tokio::sync::mpsc::channel(32);
    agent.finalize_text_response("", &tx).await;
    drop(tx);

    // The user still sees the labeled excerpt live, via the display channel.
    let mut displayed = String::new();
    while let Some(ev) = rx.recv().await {
        if let AgentEvent::TextDelta(t) = ev {
            displayed.push_str(&t);
        }
    }
    assert!(
        displayed.contains("the answer is 42"),
        "reasoning excerpt must still be delivered for display: {displayed}"
    );

    // But the persisted assistant message carries only the neutral marker.
    let persisted = agent
        .history
        .last()
        .expect("empty-response turn must persist an assistant message");
    assert_eq!(persisted.role, crate::types::Role::Assistant);
    let content = persisted
        .content
        .as_ref()
        .and_then(|c| c.text())
        .expect("persisted marker must be non-empty text");
    assert!(
        !content.contains("answer is 42") && !content.contains("chain-of-thought"),
        "persisted assistant content must not contain reasoning: {content}"
    );
    assert_eq!(content, crate::agent::EMPTY_RESPONSE_PERSISTED_MARKER);
}

// NOTE: the `build_system_prompt_*_after_*` ordering tests above are the
// prompt-cache guards (an explicitly-allowed source-grep exception). The
// section *content* itself is verified by the tests below, which call the
// real section builders and assert on the emitted prompt text — not on the
// presence of a call site in the source.

#[test]
fn tool_call_style_section_contains_guidance() {
    let section = Agent::build_tool_call_style_section();
    assert!(
        section.contains("<tool_call_style>"),
        "missing open tag: {section}"
    );
    assert!(
        section.contains("do not narrate"),
        "tool-call-style guidance must instruct the model not to narrate: {section}"
    );
}

#[test]
fn silent_reply_section_embeds_the_live_token() {
    // The model is told to emit SILENT_REPLY_TOKEN verbatim; if the section
    // hardcoded a stale literal instead of the constant, the agent loop's
    // `[SILENT]` suppression would never match. Assert the section carries
    // the exact runtime token.
    let section = Agent::build_silent_reply_section();
    assert!(
        section.contains("<silent_replies>"),
        "missing open tag: {section}"
    );
    assert!(
        section.contains(constants::SILENT_REPLY_TOKEN),
        "silent-reply section must embed the live SILENT_REPLY_TOKEN ({:?}): {section}",
        constants::SILENT_REPLY_TOKEN
    );
}

#[test]
fn reply_tags_section_documents_reply_to_current() {
    let section = Agent::build_reply_tags_section();
    assert!(
        section.contains("<reply_tags>"),
        "missing open tag: {section}"
    );
    assert!(
        section.contains("[[reply_to_current]]"),
        "reply-tags section must document the [[reply_to_current]] tag: {section}"
    );
    assert!(
        section.contains("the very first token"),
        "reply-tags section must state the first-token constraint: {section}"
    );
}

#[tokio::test]
async fn heartbeat_section_embeds_live_ok_token_and_interval() {
    // The heartbeat ack protocol tells the model to reply with the exact
    // HEARTBEAT_OK_TOKEN and quotes the configured interval. A stale literal
    // or a dropped interval would make the heartbeat scheduler unable to
    // recognize the "nothing to report" reply.
    let mut config = Config::default();
    config.heartbeat.interval = "7m".to_string();
    let agent = Agent::new(config, BorgMetrics::default())
        .await
        .expect("Agent::new must succeed without credentials");

    let section = agent.build_heartbeat_section();
    assert!(
        section.contains(constants::HEARTBEAT_OK_TOKEN),
        "heartbeat section must embed the live HEARTBEAT_OK_TOKEN: {section}"
    );
    assert!(
        section.contains("7m"),
        "heartbeat section must quote the configured interval (7m): {section}"
    );
}

#[tokio::test]
async fn messaging_section_lists_native_channels_and_forbids_curl() {
    // The messaging section steers the model to the compiled-in native
    // integrations rather than run_shell/curl. Assert the concrete guidance
    // so a rewrite that drops the curl prohibition is caught.
    let agent = Agent::new(Config::default(), BorgMetrics::default())
        .await
        .expect("Agent::new must succeed without credentials");

    let section = agent.build_messaging_section();
    assert!(
        section.contains("<messaging>"),
        "missing open tag: {section}"
    );
    for channel in ["Telegram", "Slack", "Discord", "iMessage"] {
        assert!(
            section.contains(channel),
            "messaging section must name native channel {channel}: {section}"
        );
    }
    assert!(
        section.contains("do not use run_shell/curl"),
        "messaging section must forbid run_shell/curl for messaging: {section}"
    );
}

#[tokio::test]
async fn reasoning_section_is_empty_unless_thinking_enabled() {
    // The reasoning-format section is conditional: it must be empty when
    // extended thinking is off (default), so it never bloats the cached
    // prefix, and non-empty (quoting the level) when enabled.
    let off_agent = Agent::new(Config::default(), BorgMetrics::default())
        .await
        .expect("Agent::new must succeed without credentials");
    assert!(
        off_agent.build_reasoning_section().is_empty(),
        "reasoning section must be empty when thinking is disabled"
    );
}

#[test]
fn build_system_prompt_section_ordering() {
    // Verify: tooling -> security -> memory -> silent -> heartbeat -> environment
    let body = extract_build_system_prompt_body();
    let tooling = body.find("build_tooling_section").unwrap();
    let security = body.find("security_policy").unwrap();
    let silent = body.find("build_silent_reply_section").unwrap();
    let heartbeat = body.find("build_heartbeat_section").unwrap();
    let env = body.find("build_environment_section").unwrap();
    assert!(tooling < security, "tooling before security");
    assert!(security < silent, "security before silent reply");
    assert!(silent < heartbeat, "silent reply before heartbeat");
    assert!(heartbeat < env, "heartbeat before environment");
}

#[test]
fn run_iteration_routes_invalid_calls_through_execute_tool_calls() {
    // Source-grep guard for the contract between run_iteration and
    // execute_tool_calls: invalid (addressable) calls must NOT skip the
    // execute path, otherwise the assistant message persists with a
    // tool_call shell that has no matching tool_result and the LLM API
    // rejects the next turn.
    let iter_idx = AGENT_RS_SRC
        .find("let (tc, invalid) = partition_tool_calls(")
        .expect("partition_tool_calls call site must exist in run_iteration");
    let after = &AGENT_RS_SRC[iter_idx..];
    let early_return = after
        .find("if tc.is_empty() && invalid.is_empty()")
        .expect("early-return guard must check BOTH valid and invalid as empty");
    let exec_call = after
        .find("self.execute_tool_calls(&tc, &invalid,")
        .expect("execute_tool_calls must be invoked with both valid and invalid slices");
    assert!(
        early_return < exec_call,
        "the empty-check must precede the execute call",
    );
}

#[test]
fn tool_call_repeat_guard_runs_before_rate_guard_in_run_tool_calls() {
    // Source-grep guard: the repeat check must precede the rate-guard
    // call so blocked repeats do not consume rate-guard budget. If
    // someone reorders these, the per-session 200/500 tool-call
    // budget burns down on calls that were never going to execute.
    let fn_idx = TOOL_EXECUTION_RS_SRC
        .find("async fn run_tool_calls(")
        .expect("run_tool_calls must exist");
    let body = &TOOL_EXECUTION_RS_SRC[fn_idx
        ..TOOL_EXECUTION_RS_SRC[fn_idx..]
            .find("\n    async fn ")
            .unwrap_or(8000)
            + fn_idx];
    let repeat_idx = body
        .find("self.tool_call_repeat.observe(")
        .expect("run_tool_calls must call self.tool_call_repeat.observe(...)");
    let rate_idx = body
        .find("self.rate_guard.record(action_type)")
        .expect("run_tool_calls must call self.rate_guard.record(...)");
    assert!(
        repeat_idx < rate_idx,
        "tool_call_repeat.observe must run BEFORE rate_guard.record \
         (repeat_idx={repeat_idx}, rate_idx={rate_idx})",
    );
}

#[test]
fn tool_call_repeat_guard_runs_before_rate_guard_in_parallel_path() {
    // Same invariant as above for the parallel-execution path. Both
    // paths share the rate-guard, so both must guard repeats first.
    let fn_idx = TOOL_EXECUTION_RS_SRC
        .find("async fn run_tool_group_parallel(")
        .expect("run_tool_group_parallel must exist");
    let body = &TOOL_EXECUTION_RS_SRC[fn_idx
        ..TOOL_EXECUTION_RS_SRC[fn_idx..]
            .find("\n    async fn ")
            .unwrap_or(12000)
            + fn_idx];
    let repeat_idx = body
        .find("self.tool_call_repeat.observe(")
        .expect("run_tool_group_parallel must call self.tool_call_repeat.observe(...)");
    let rate_idx = body
        .find("self.rate_guard.record(action_type)")
        .expect("run_tool_group_parallel must call self.rate_guard.record(...)");
    assert!(
        repeat_idx < rate_idx,
        "tool_call_repeat.observe must run BEFORE rate_guard.record in parallel path \
         (repeat_idx={repeat_idx}, rate_idx={rate_idx})",
    );
}

#[test]
fn run_agent_loop_resets_per_turn_loop_guards() {
    // Source-grep guard: each user turn starts fresh. Without the
    // reset, a turn that ended with a blocked streak (or a refused
    // hallucinated tool) would block the first identical call /
    // refuse the same name on the next turn — which the user may
    // have explicitly asked for.
    let loop_idx = AGENT_RS_SRC
        .find("pub async fn run_agent_loop(")
        .expect("run_agent_loop must exist");
    let body = &AGENT_RS_SRC
        [loop_idx..AGENT_RS_SRC[loop_idx..].find("\n    fn ").unwrap_or(20_000) + loop_idx];
    assert!(
        body.contains("self.tool_call_repeat.reset()"),
        "run_agent_loop must reset the repeat tracker at start of turn"
    );
    assert!(
        body.contains("self.unknown_tool_tracker.reset()"),
        "run_agent_loop must reset the unknown-tool tracker at start of turn"
    );
}

#[test]
fn run_agent_loop_reaps_processes_only_on_abandoned_turns() {
    // Source-grep guard for the turn-scoped subprocess ownership seam.
    // Invariants the type system can't express:
    //  1. a process baseline is captured (begin_turn) BEFORE the loop body
    //     runs, so pre-existing processes are never reap candidates;
    //  2. the abandoned-turn reap runs ONLY under `cancel.is_cancelled()` —
    //     normal completion must leave backgrounded processes alive.
    let start = AGENT_RS_SRC
        .find("pub async fn run_agent_loop(")
        .expect("run_agent_loop must exist");
    let end_rel = AGENT_RS_SRC[start..]
        .find("async fn run_agent_loop_inner(")
        .expect("run_agent_loop_inner must follow the wrapper");
    let wrapper = &AGENT_RS_SRC[start..start + end_rel];

    let begin_idx = wrapper
        .find(".begin_turn(")
        .expect("run_agent_loop must capture a process baseline via begin_turn");
    let inner_idx = wrapper
        .find("run_agent_loop_inner(")
        .expect("wrapper must delegate to run_agent_loop_inner");
    assert!(
        begin_idx < inner_idx,
        "baseline must be captured BEFORE the loop body runs — otherwise \
         turn-spawned processes land in their own baseline and never reap"
    );
    let cancelled_idx = wrapper
        .find("cancel.is_cancelled()")
        .expect("wrapper must distinguish abandonment from normal completion");
    let reap_idx = wrapper
        .find("reap_abandoned_turn(")
        .expect("wrapper must reap turn-spawned processes on abandonment");
    assert!(
        inner_idx < cancelled_idx && cancelled_idx < reap_idx,
        "reap_abandoned_turn must run after the loop body and only under the \
         cancellation check — a reap on the normal-completion path would kill \
         every backgrounded process the user asked for"
    );
}

// ── Reactive compaction stalled-state error (Gap 1) ───────────────
//
// When reactive compaction frees zero history, the next LLM stream
// will overflow again. Before this fix, the user only saw a generic
// `[stream interrupted: <provider error>]` after the second failure.
// The error must be surfaced *now* with the actionable cause.

fn extract_reactive_compact_body() -> &'static str {
    let start = AGENT_RS_SRC
        .find("async fn reactive_compact_history(")
        .expect("reactive_compact_history must exist");
    let after = &AGENT_RS_SRC[start..];
    // Skip past our own signature (otherwise we'd match it as the
    // "next fn"), then find the soonest of `\n    async fn ` and
    // `\n    fn ` — whichever comes first marks the end of the body.
    let body_start = 1; // step over the leading `a` of `async fn`
    let search = &after[body_start..];
    let next_async = search.find("\n    async fn ").map(|i| i + body_start);
    let next_sync = search.find("\n    fn ").map(|i| i + body_start);
    let end_rel = match (next_async, next_sync) {
        (Some(a), Some(s)) => a.min(s),
        (Some(a), None) => a,
        (None, Some(s)) => s,
        (None, None) => after.len(),
    };
    &after[..end_rel]
}

#[test]
fn reactive_compact_zero_drop_emits_error_event() {
    // Source-grep guard: the dropped==0 branch must emit
    // AgentEvent::Error so the TUI surfaces an actionable message.
    // Replacing the warn! with only a log regresses to a silent-
    // degradation failure mode where the user sees no error.
    let body = extract_reactive_compact_body();
    // The dropped==0 else branch must contain BOTH the warn! and a
    // user-visible AgentEvent::Error send.
    let else_idx = body
        .find("warn!(\"reactive_compact_history: nothing dropped")
        .expect("zero-drop warn! must exist as the trigger");
    let after_else = &body[else_idx..];
    assert!(
        after_else.contains("AgentEvent::Error"),
        "zero-drop branch must emit AgentEvent::Error after the warn!"
    );
}

#[test]
fn reactive_compact_zero_drop_error_text_is_actionable() {
    // The error text must name the cause AND the actionable knob.
    // A vague message ("compaction failed") leaves the user with no
    // path to recovery. Tokens checked are stable identifiers, not
    // prose — small wording tweaks won't break this.
    let body = extract_reactive_compact_body();
    let err_idx = body
        .find("Context overflow and history compaction freed nothing")
        .expect("zero-drop error must mention 'Context overflow' cause");
    let snippet = &body[err_idx..(err_idx + 400).min(body.len())];
    assert!(
        snippet.contains("conversation.max_history_tokens"),
        "zero-drop error must reference the configurable knob: {snippet}"
    );
}

#[test]
fn reactive_compact_history_compacted_event_remains_gated() {
    // HistoryCompacted must only fire when dropped > 0. If someone
    // hoists the send out of the `if dropped > 0` branch, the TUI
    // will show a misleading "Compacted N messages" notice when
    // nothing actually changed.
    let body = extract_reactive_compact_body();
    // Find the second HistoryCompacted send (the first is the
    // cheap-pruning early-return). It must follow `if dropped > 0`.
    let first = body
        .find("AgentEvent::HistoryCompacted")
        .expect("first HistoryCompacted send must exist");
    let after_first = &body[first + 1..];
    let second_rel = after_first
        .find("AgentEvent::HistoryCompacted")
        .expect("second HistoryCompacted send must exist");
    let second = first + 1 + second_rel;
    // Find the most recent `if dropped > 0` before the second send.
    let preceding_guard = body[..second]
        .rfind("if dropped > 0")
        .expect("second HistoryCompacted send must be guarded by `if dropped > 0`");
    // No `else` should appear between the guard and the send.
    let between = &body[preceding_guard..second];
    let else_count =
        between.matches("\n        } else").count() + between.matches("} else {").count();
    // The guard's own `else` only appears AFTER the send, not between.
    assert_eq!(
        else_count, 0,
        "HistoryCompacted send at offset {second} must be inside the `if dropped > 0` \
         block, not after an else (between snippet: {between:?})"
    );
}

// ── No-user-message-replay regression guards (Gap 3) ──────────────
//
// Failure mode being guarded against: on context-overflow retry, an
// earlier implementation re-injected the user's last message, so the
// agent answered the same prompt over and over. Borg must not regress
// to that. These are source-grep guards because the invariant ("the
// retry path does not synthesize Message::user") is structural — types
// can't express it. Precedent: the TUI mouse-capture and prompt-cache
// guards.

fn extract_run_agent_loop_stream_error_block() -> &'static str {
    // The stream-error retry block: from `if let Some(err_msg) =
    // &stream_error {` to its matching `continue;`.
    let start = AGENT_RS_SRC
        .find("if let Some(err_msg) = &stream_error {")
        .expect("run_agent_loop must contain stream_error retry block");
    let after = &AGENT_RS_SRC[start..];
    let end_rel = after
        .find("continue;\n                }")
        .map(|i| i + "continue;\n                }".len())
        .or_else(|| after.find("continue;").map(|i| i + "continue;".len()))
        .expect("stream-error retry block must end with `continue;`");
    &after[..end_rel]
}

const FORBIDDEN_USER_REPLAY_TOKENS: &[&str] = &[
    "Message::user(",
    "Role::User",
    "inject_history_message",
    "latest_user_message_text",
];

#[test]
fn reactive_compact_history_must_not_replay_user_messages() {
    // Regression guard: the compaction path reads the user's history
    // but must never *write* a new user message. If a future change
    // adds (e.g.) "preserve last user msg by re-pushing it after
    // summarization", it will trip this test.
    let body = extract_reactive_compact_body();
    for token in FORBIDDEN_USER_REPLAY_TOKENS {
        assert!(
            !body.contains(token),
            "reactive_compact_history must not contain `{token}` \
             (re-injecting a user message on retry causes the agent to \
             answer the same prompt repeatedly)"
        );
    }
}

#[test]
fn stream_error_retry_path_must_not_replay_user_messages() {
    // Same invariant for the run_agent_loop stream-error → continue
    // block. The retry must just compact + re-stream; it must not
    // synthesize a fresh user message.
    let body = extract_run_agent_loop_stream_error_block();
    for token in FORBIDDEN_USER_REPLAY_TOKENS {
        assert!(
            !body.contains(token),
            "run_agent_loop stream-error retry block must not contain `{token}` \
             (synthesizing a fresh user message on retry would re-answer \
             the same prompt)"
        );
    }
}

#[test]
fn reactive_compact_calls_continue_to_retry_not_replay() {
    // Positive-shape assertion paired with the negative ones above:
    // the retry mechanism is `continue` on the outer loop (re-using
    // the existing history). If someone replaces `continue` with a
    // recursive call to `run_agent_loop` or a re-push of the user
    // message, both this and the negative tests above would fail.
    let body = extract_run_agent_loop_stream_error_block();
    assert!(
        body.contains("continue;"),
        "stream-error retry must use `continue` to re-run the outer loop \
         with the same (compacted) history"
    );
    assert!(
        body.contains("reactive_compact_history"),
        "stream-error retry must invoke reactive_compact_history before retrying"
    );
}

#[test]
fn reactive_compact_history_only_drops_user_messages_never_adds() {
    // Behavioral assertion of the structural invariant: the only
    // calls inside reactive_compact_history that mutate self.history
    // are compact_tool_results, compact_history_v2, and
    // compact_history — all of which are documented to be
    // monotonically non-additive (they only drop or summarize).
    // If a new mutator is introduced, this test trips and forces
    // a deliberate audit.
    let body = extract_reactive_compact_body();
    let approved_mutators = [
        "compact_tool_results(&mut self.history",
        "compact_history_v2(",
        "compact_history(&mut self.history",
        "normalize_history(&mut self.history",
    ];
    // Forbid raw history mutations not in the approved list.
    let forbidden_mutations = [
        "self.history.push(",
        "self.history.insert(",
        "self.history.extend(",
        "self.history.append(",
    ];
    for forbidden in &forbidden_mutations {
        assert!(
            !body.contains(forbidden),
            "reactive_compact_history must not call `{forbidden}` — \
             history can only be drained/summarized, never extended. \
             Approved mutators: {approved_mutators:?}"
        );
    }
}

// ── Credential resolution must be deferred to first turn ──
//
// The TUI's `/settings` popup is the user's way to enter an API key.
// It lives inside the TUI, so the TUI must boot successfully even when
// no provider/key is configured. If `Agent::new` regresses to a
// preflight that fails on missing credentials, `Session.Open` errors,
// the TUI never paints, and users hit a catch-22 (the error tells them
// to use `/settings`, but they can't reach it). See the comment on
// `build_common`.

#[tokio::test]
async fn agent_new_succeeds_without_provider_configured() {
    // Real failure mode: a regression that re-introduces an LlmClient
    // preflight inside `build_common` would make `Agent::new` return
    // Err here. The credential check must move to first-turn
    // `make_llm_client_from_config`, not block session construction.
    let config = Config::default();
    assert!(
        config.llm.provider.is_none(),
        "test premise: default config must have no provider so the \
         credential resolver would have nothing to find",
    );

    let agent = Agent::new(config, BorgMetrics::default()).await;
    assert!(
        agent.is_ok(),
        "Agent::new must succeed without credentials so the TUI \
         can open and surface `/settings`. Got: {:?}",
        agent.err(),
    );
}

#[tokio::test]
async fn make_llm_client_still_rejects_missing_provider_on_turn() {
    // Symmetric guard: deferring the check is only safe because the
    // per-turn LLM-client construction still rejects missing
    // credentials. If this stops failing, the deferred path would
    // silently let a keyless agent reach the LLM transport and explode
    // with a less-actionable error.
    let agent = Agent::new(Config::default(), BorgMetrics::default())
        .await
        .expect("Agent::new must succeed without credentials");

    let result = agent.make_llm_client_from_config(&agent.config).await;
    let err = match result {
        Ok(_) => panic!("turn-time LLM construction must reject missing provider"),
        Err(e) => e,
    };
    let msg = format!("{err:#}");
    assert!(
        msg.contains("No LLM provider selected") || msg.contains("No API key found"),
        "credential error must remain user-actionable so the TUI can \
         render a `/settings` hint; got: {msg}",
    );
}

#[tokio::test]
async fn tool_output_truncation_honors_configured_cap() {
    // Regression guard for a dead-wired setting: `truncate_and_redact` must read
    // `conversation.tool_output_max_tokens`, not a hardcoded constant. The input
    // below (~500 tokens) sits well under the 10k default but far above the
    // 10-token cap set here — so this test only passes if the config is actually
    // consulted. Against the previous hardcoded path it would not truncate.
    let mut config = Config::default();
    config.conversation.tool_output_max_tokens = 10;
    let agent = Agent::new(config, BorgMetrics::default())
        .await
        .expect("Agent::new must succeed without credentials");

    let raw = "abcd ".repeat(400); // 2000 bytes ≈ 500 tokens
    let out = agent.truncate_and_redact(&raw);

    assert!(
        out.len() < raw.len(),
        "configured cap of 10 tokens must truncate a ~500-token output"
    );
    assert!(
        out.contains("omitted"),
        "truncated output must carry the disclosure marker: {out}"
    );
}

#[tokio::test]
async fn tool_output_truncation_clamps_to_model_context_window() {
    // A fixed per-result cap ignores the model: a generous configured ceiling
    // would let one tool result exceed a small-context model's whole window.
    // The cap must additionally clamp to a quarter of the active window.
    let raw = "abcd ".repeat(3000); // ~3.7k tokens — above gpt-4's 8192/4 = 2048 cap

    // Small-context model (gpt-4 = 8192) with an effectively unbounded
    // configured cap → the window clamp must still truncate.
    let mut small = Config::default();
    small.conversation.tool_output_max_tokens = 1_000_000;
    small.llm.model = "gpt-4".to_string();
    let small_agent = Agent::new(small, BorgMetrics::default())
        .await
        .expect("Agent::new must succeed without credentials");
    let small_out = small_agent.truncate_and_redact(&raw);
    assert!(
        small_out.len() < raw.len() && small_out.contains("omitted"),
        "small-context model must clamp tool output to ~window/4 despite a huge \
         configured cap, got len {} of {}",
        small_out.len(),
        raw.len(),
    );

    // Control: a large-context model (default 200k window) with the same huge
    // configured cap must NOT truncate the same input — proving the clamp, not
    // the configured cap, did the work above.
    let mut large = Config::default();
    large.conversation.tool_output_max_tokens = 1_000_000;
    let large_agent = Agent::new(large, BorgMetrics::default())
        .await
        .expect("Agent::new must succeed without credentials");
    let large_out = large_agent.truncate_and_redact(&raw);
    assert!(
        !large_out.contains("omitted"),
        "large-context model must not truncate a ~3.7k-token output under a huge cap"
    );
}

// ── session storage exhaustion ────────────────────────────────────────────

/// Build a test agent whose DB is a file-backed database with a page budget
/// small enough that the next insert returns a *real* `SQLITE_FULL` — not a
/// mocked error, the actual condition a full disk produces.
async fn agent_with_full_db() -> (Agent, tempfile::TempDir) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let db = crate::db::Database::open_at(&tmp.path().join("borg.db"))
        .await
        .expect("open db");
    // 2 pages is below the schema's own footprint, so any further write fails.
    // SQLite refuses to set `max_page_count` below the current page count, so
    // pin it to exactly what the schema already occupies. Any write that needs
    // a new page then returns a real SQLITE_FULL — the same error a full disk
    // produces, not a mocked one.
    let mut rows = db
        .conn()
        .query("PRAGMA page_count", ())
        .await
        .expect("read page count");
    let pages: i64 = rows
        .next()
        .await
        .expect("row")
        .expect("page_count row")
        .get(0)
        .expect("page count value");
    db.conn()
        .query(&format!("PRAGMA max_page_count = {pages}"), ())
        .await
        .expect("set page cap");

    let agent = minimal_test_agent(Config::default());
    *agent.db.lock().await = Some(db);
    (agent, tmp)
}

#[tokio::test]
async fn exhausted_storage_records_a_fault_and_does_not_extend_history() {
    let (mut agent, _tmp) = agent_with_full_db().await;
    let before = agent.history.len();

    agent
        // Large enough to require new pages rather than fitting in slack.
        .persist_messages(vec![Message::assistant("x".repeat(200_000))])
        .await;

    assert!(
        agent.storage_fault.is_some(),
        "a full disk must be recorded, not just logged"
    );
    // This is the assertion that fails if someone restores the unconditional
    // `history.extend`: the next turn's prompt would carry a message SQLite
    // never saw.
    assert_eq!(
        agent.history.len(),
        before,
        "in-memory history must not diverge from the durable log"
    );
    let msg = agent.storage_fault.clone().unwrap_or_default();
    assert!(
        msg.contains("NOT saved"),
        "the message must say the reply was lost: {msg}"
    );
    assert!(
        msg.contains(".borg"),
        "the message must say where to free space: {msg}"
    );
}

#[tokio::test]
async fn healthy_storage_records_no_fault_and_extends_history() {
    // Over-fire guard: normal persistence must be untouched by this path.
    let tmp = tempfile::tempdir().expect("tempdir");
    let db = crate::db::Database::open_at(&tmp.path().join("borg.db"))
        .await
        .expect("open db");
    let mut agent = minimal_test_agent(Config::default());
    *agent.db.lock().await = Some(db);

    agent
        .persist_messages(vec![Message::assistant("a reply")])
        .await;

    assert!(
        agent.storage_fault.is_none(),
        "healthy write must not fault"
    );
    assert_eq!(agent.history.len(), 1);
}

#[tokio::test]
async fn storage_fault_mid_turn_leaves_history_well_formed() {
    // A turn persists the assistant message carrying `tool_calls` in one
    // batch and its `tool_result`s in a later one. If the first faulted and a
    // later one squeezed through, history would hold tool results with no
    // preceding tool use — a shape Anthropic and OpenAI both reject, and one
    // that would outlive the aborted turn and poison every subsequent one.
    let (mut agent, _tmp) = agent_with_full_db().await;

    let mut assistant = Message::assistant("x".repeat(200_000));
    assistant.tool_calls = Some(vec![crate::types::ToolCall {
        id: "call_1".to_string(),
        call_type: "function".to_string(),
        function: crate::types::FunctionCall {
            name: "run_shell".to_string(),
            arguments: "{}".to_string(),
        },
    }]);

    // Head batch faults on the full disk...
    agent.persist_messages(vec![assistant]).await;
    assert!(agent.storage_fault.is_some(), "head batch must fault");

    // ...and the follow-up tool result must NOT slip in behind it, even
    // though it is small enough to fit.
    agent
        .persist_messages(vec![Message::tool_result("call_1", "ok")])
        .await;

    assert!(
        agent.history.is_empty(),
        "no partial turn may survive: {:?}",
        agent.history
    );
    assert!(
        !agent
            .history
            .iter()
            .any(|m| m.role == crate::types::Role::Tool),
        "a tool result must never be persisted without its tool call"
    );
}

#[tokio::test]
async fn a_new_turn_clears_a_stale_storage_fault() {
    // A fault raised between turns (a failed auto_save) must not kill a turn
    // that would succeed on a since-freed disk; if the disk is still full,
    // that turn's first persist re-raises it immediately.
    let (mut agent, _tmp) = agent_with_full_db().await;
    agent
        .persist_messages(vec![Message::assistant("x".repeat(200_000))])
        .await;
    assert!(agent.storage_fault.is_some());

    agent.prepare_loop().await;
    assert!(
        agent.storage_fault.is_none(),
        "a fresh turn starts from a clean slate"
    );
}
