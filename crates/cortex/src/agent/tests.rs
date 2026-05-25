use super::tool_classification::{classify_action, is_mutating_tool};
use super::tool_execution::dispatch_parallel_safe_tool;
use super::*;
use crate::rate_guard::ActionType;
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
    // Extract ~6KB following the signature — plenty to cover the body.
    let end = (start + 6000).min(SYSTEM_PROMPT_RS_SRC.len());
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

// TODO: update_task_status is async; tests need tokio runtime
#[cfg(feature = "phase9-tests-disabled")]
#[test]
fn update_task_status_not_found() {}

#[cfg(feature = "phase9-tests-disabled")]
#[test]
fn update_task_status_formats_verb() {}

// ── classify_action tests ──

#[test]
fn classify_action_known_and_unknown_tools() {
    // classify_action feeds the rate-limiter and vitals events. If the
    // mapping regresses, a shell command could be counted as a generic
    // tool call and bypass the shell-specific rate bucket.
    let cases: &[(&str, ActionType)] = &[
        ("run_shell", ActionType::ShellCommand),
        ("apply_patch", ActionType::FileWrite),
        ("write_memory", ActionType::MemoryWrite),
        ("web_fetch", ActionType::WebRequest),
        // Unknowns must fall through to ToolCall (safe default).
        ("some_unknown_tool", ActionType::ToolCall),
        ("", ActionType::ToolCall),
    ];
    for (tool, expected) in cases {
        let got = classify_action(tool);
        assert!(
            std::mem::discriminant(&got) == std::mem::discriminant(expected),
            "classify_action({tool:?}) = {got:?}, expected {expected:?}"
        );
    }
}

// -- is_mutating_tool (allowlist-based) --

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

#[test]
fn skill_tool_write_refreshes_cached_skill_state() {
    let config = Config::default();
    let session = Session::new();
    let session_id = session.meta.id.clone();
    let mut agent = Agent {
        config: config.clone(),
        history: Vec::new(),
        session,
        policy: config.policy.clone(),
        hook_registry: HookRegistry::new(),
        turn_count: 0,
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
        caches: SystemPromptCache::with_test_state(
            None,
            Some(HashMap::new()),
            Some("cached".to_string()),
            Some("old request".to_string()),
            Some(vec!["pdf".to_string()]),
        ),
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
        previous_summary: None,
        setup_just_consumed: false,
        tool_call_repeat: ToolCallRepeatTracker::default(),
        unknown_tool_tracker: UnknownToolTracker::default(),
        llm_role: LlmRole::Primary,
        goal_runtime: crate::goals::runtime::GoalRuntimeState::new(),
        goal_event_sink: None,
        auto_title_spawned: false,
    };

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
    let config = Config::default();
    let session = Session::new();
    let session_id = session.meta.id.clone();
    let mut agent = Agent {
        config: config.clone(),
        history: Vec::new(),
        session,
        policy: config.policy.clone(),
        hook_registry: HookRegistry::new(),
        turn_count: 0,
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
        caches: SystemPromptCache::with_test_state(
            Some("identity-snapshot".into()),
            Some(HashMap::from([("OLD_TOKEN".into(), "v".into())])),
            Some("rendered-skills".into()),
            Some("user-request".into()),
            Some(vec!["pdf".into()]),
        ),
        ghost_commit: None,
        execution_context: crate::execution_context::ExecutionContext::unrestricted(
            std::path::PathBuf::from("."),
        ),
        git_repo_root: None,
        config_arc: Arc::new(config.clone()),
        steer_rx: None,
        recent_tool_names: std::collections::HashSet::new(),
        cost_event_sink: None,
        short_term_memory: crate::memory::short_term_memory::ShortTermMemory::new(session_id, 2000),
        previous_summary: None,
        setup_just_consumed: false,
        tool_call_repeat: ToolCallRepeatTracker::default(),
        unknown_tool_tracker: UnknownToolTracker::default(),
        llm_role: LlmRole::Primary,
        goal_runtime: crate::goals::runtime::GoalRuntimeState::new(),
        goal_event_sink: None,
        auto_title_spawned: false,
    };

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

// -- dispatch_parallel_safe_tool --

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

// -- SETUP template tests --
//
// The two `setup_template_*` template-rendering tests live alongside
// `SETUP_TEMPLATE` in `agent::system_prompt`. The filesystem-lifecycle
// tests below stay here because they don't touch the template itself.

#[test]
fn setup_file_lifecycle_atomic_consume() {
    // Simulates the atomic rename → read → delete lifecycle from build_system_prompt
    let tmp = tempfile::tempdir().unwrap();
    let setup_path = tmp.path().join("SETUP.md");
    let consumed_path = setup_path.with_extension("md.consumed");

    // Write SETUP.md
    std::fs::write(&setup_path, "# First Boot\nTest content").unwrap();
    assert!(setup_path.exists());

    // Atomic rename (simulates line 732)
    assert!(std::fs::rename(&setup_path, &consumed_path).is_ok());
    assert!(!setup_path.exists());
    assert!(consumed_path.exists());

    // Read consumed file (simulates line 733)
    let content = std::fs::read_to_string(&consumed_path).unwrap();
    assert!(content.contains("First Boot"));

    // Delete consumed file (simulates line 740)
    std::fs::remove_file(&consumed_path).unwrap();
    assert!(!consumed_path.exists());
    assert!(!setup_path.exists());
}

#[test]
fn setup_file_missing_does_not_inject() {
    // If SETUP.md doesn't exist, rename fails and nothing is injected
    let tmp = tempfile::tempdir().unwrap();
    let setup_path = tmp.path().join("SETUP.md");
    let consumed_path = setup_path.with_extension("md.consumed");

    assert!(std::fs::rename(&setup_path, &consumed_path).is_err());
}

#[test]
fn setup_file_not_injected_twice() {
    // After first consumption, file is gone — second attempt is a no-op
    let tmp = tempfile::tempdir().unwrap();
    let setup_path = tmp.path().join("SETUP.md");
    let consumed_path = setup_path.with_extension("md.consumed");

    std::fs::write(&setup_path, "content").unwrap();
    std::fs::rename(&setup_path, &consumed_path).unwrap();
    std::fs::remove_file(&consumed_path).unwrap();

    // Second attempt — file is gone
    assert!(std::fs::rename(&setup_path, &consumed_path).is_err());
}

// -- should_nudge_for_response --

#[test]
fn should_nudge_for_response_matrix() {
    // (text, tools_executed, already_retried, expected_nudge)
    let cases: &[(&str, bool, bool, bool)] = &[
        ("", true, false, true),       // empty after tools → nudge
        ("  \n  ", true, false, true), // whitespace-only counts as empty
        ("Done!", true, false, false), // non-empty text → no nudge
        ("", false, false, false),     // no tools ran → no nudge
        ("", true, true, false),       // already retried once → no nudge
    ];
    for (text, tools, retried, expected) in cases {
        let got = should_nudge_for_response(text, *tools, *retried);
        assert_eq!(
            got, *expected,
            "should_nudge_for_response({text:?}, tools={tools}, retried={retried}) = {got}, expected {expected}"
        );
    }
}

// -- collaboration mode template tests --

// -- new system prompt section tests --

#[test]
fn build_system_prompt_has_tooling_section() {
    let body = extract_build_system_prompt_body();
    assert!(
        body.contains("build_tooling_section"),
        "system prompt must include tooling section"
    );
}

#[test]
fn build_system_prompt_has_tool_call_style() {
    let body = extract_build_system_prompt_body();
    assert!(
        body.contains("build_tool_call_style_section"),
        "system prompt must include tool call style guidance"
    );
}

#[test]
fn build_system_prompt_has_silent_reply() {
    let body = extract_build_system_prompt_body();
    assert!(
        body.contains("build_silent_reply_section"),
        "system prompt must include silent reply protocol"
    );
}

#[test]
fn build_system_prompt_has_heartbeat_protocol() {
    let body = extract_build_system_prompt_body();
    assert!(
        body.contains("build_heartbeat_section"),
        "system prompt must include heartbeat ack protocol"
    );
}

#[test]
fn build_system_prompt_has_reply_tags() {
    let body = extract_build_system_prompt_body();
    assert!(
        body.contains("build_reply_tags_section"),
        "system prompt must include reply tags section"
    );
}

#[test]
fn build_system_prompt_has_messaging() {
    let body = extract_build_system_prompt_body();
    assert!(
        body.contains("build_messaging_section"),
        "system prompt must include messaging/channel routing"
    );
}

#[test]
fn build_system_prompt_has_reasoning() {
    let body = extract_build_system_prompt_body();
    assert!(
        body.contains("build_reasoning_section"),
        "system prompt must include reasoning format section"
    );
}

#[test]
fn tool_call_style_section_contains_guidance() {
    let section = Agent::build_tool_call_style_section();
    assert!(section.contains("do not narrate"));
    assert!(section.contains("tool_call_style"));
}

#[test]
fn silent_reply_section_contains_token() {
    let section = Agent::build_silent_reply_section();
    assert!(section.contains(constants::SILENT_REPLY_TOKEN));
    assert!(section.contains("silent_replies"));
}

#[test]
fn reply_tags_section_contains_tag() {
    let section = Agent::build_reply_tags_section();
    assert!(section.contains("[[reply_to_current]]"));
    assert!(section.contains("reply_tags"));
}

#[test]
fn silent_reply_token_is_not_empty() {
    assert!(!constants::SILENT_REPLY_TOKEN.is_empty());
}

#[test]
fn heartbeat_ok_token_is_not_empty() {
    assert!(!constants::HEARTBEAT_OK_TOKEN.is_empty());
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
