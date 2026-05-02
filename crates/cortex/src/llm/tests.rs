use super::*;
use crate::types::{FunctionCall, ToolCall, ToolDefinition};
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct CollectingSink {
    events: Mutex<Vec<CostEvent>>,
}

impl CollectingSink {
    fn take(&self) -> Vec<CostEvent> {
        let mut guard = self
            .events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        std::mem::take(&mut *guard)
    }
}

impl CostEventSink for CollectingSink {
    fn emit(&self, event: CostEvent) {
        self.events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(event);
    }
}

struct PanicSink;

impl CostEventSink for PanicSink {
    fn emit(&self, _event: CostEvent) {
        panic!("intentional sink panic");
    }
}

/// Helper to create an LlmClient with Anthropic provider for testing.
/// Uses a unique env var to avoid races with parallel tests.
fn make_test_anthropic_client() -> LlmClient {
    let env_var = "BORG_TEST_ANTHROPIC_LLM";
    std::env::set_var(env_var, "sk-test");
    let mut config = Config::default();
    config.llm.provider = Some("anthropic".to_string());
    config.llm.api_key_env = env_var.to_string();
    config.llm.model = "claude-sonnet-4".to_string();
    LlmClient::new(&config).unwrap()
}

fn make_test_openai_client() -> LlmClient {
    let env_var = "BORG_TEST_OPENAI_LLM";
    std::env::set_var(env_var, "sk-test");
    let mut config = Config::default();
    config.llm.provider = Some("openai".to_string());
    config.llm.api_key_env = env_var.to_string();
    config.llm.model = "gpt-4.1".to_string();
    LlmClient::new(&config).unwrap()
}

fn make_test_openrouter_client(cost_events: bool) -> LlmClient {
    let env_var = "BORG_TEST_OPENROUTER_LLM";
    std::env::set_var(env_var, "sk-test");
    let mut config = Config::default();
    config.llm.provider = Some("openrouter".to_string());
    config.llm.api_key_env = env_var.to_string();
    config.llm.model = "openai/gpt-4o-mini".to_string();
    config.telemetry.cost_events = cost_events;
    LlmClient::new(&config).unwrap()
}

fn cost_context() -> CostEventContext {
    CostEventContext {
        session_id: "session-123".to_string(),
        kind: "chat".to_string(),
    }
}

#[test]
fn air_gap_blocks_public_cloud_construction() {
    // outbound_allowed = false MUST refuse a PublicCloud provider before
    // any HTTP socket is created. Real failure mode: the ProviderRegistry
    // resolver loses its caller, leaving the air-gap setting silently
    // unenforced.
    let env_var = "BORG_TEST_AIRGAP_OPENAI";
    std::env::set_var(env_var, "sk-test");
    let mut config = Config::default();
    config.llm.provider = Some("openai".to_string());
    config.llm.api_key_env = env_var.to_string();
    config.llm.model = "gpt-4o-mini".to_string();
    config.providers.outbound_allowed = false;

    let err = match LlmClient::new(&config) {
        Ok(_) => panic!("expected air-gap rejection"),
        Err(e) => e,
    };
    let msg = format!("{err}");
    assert!(
        msg.contains("air-gap"),
        "expected air-gap rejection, got: {msg}"
    );
}

#[test]
fn air_gap_allows_local_construction() {
    // Ollama is Local — must construct cleanly under air-gap.
    let mut config = Config::default();
    config.llm.provider = Some("ollama".to_string());
    config.llm.model = "llama3".to_string();
    config.providers.outbound_allowed = false;

    LlmClient::new(&config).expect("local provider must be allowed under air-gap");
}

#[test]
fn air_gap_skips_public_cloud_fallback_slot() {
    // A primary Local provider with a PublicCloud fallback under air-gap
    // must construct, but the fallback slot must be dropped — otherwise
    // the failover path would silently make outbound calls.
    let env_var = "BORG_TEST_AIRGAP_FALLBACK";
    std::env::set_var(env_var, "sk-test");
    let mut config = Config::default();
    config.llm.provider = Some("ollama".to_string());
    config.llm.model = "llama3".to_string();
    config.providers.outbound_allowed = false;
    config.llm.fallback.push(crate::config::LlmFallback {
        provider: "openai".to_string(),
        model: "gpt-4o-mini".to_string(),
        api_key: None,
        api_key_env: Some(env_var.to_string()),
        api_keys: Vec::new(),
        temperature: None,
        max_tokens: None,
        base_url: None,
    });

    let client = LlmClient::new(&config).expect("primary local must succeed");
    assert!(
        client.provider_slots.is_empty(),
        "PublicCloud fallback must be dropped under air-gap"
    );
}

#[test]
fn openrouter_cost_event_uses_usage_cost_verbatim() {
    let sink = Arc::new(CollectingSink::default());
    let client = make_test_openrouter_client(true).with_cost_event_sink(sink.clone(), cost_context());
    let usage = UsageData {
        prompt_tokens: 10,
        completion_tokens: 20,
        total_tokens: 30,
        cached_input_tokens: 0,
        cache_creation_tokens: 0,
        provider: "openrouter".to_string(),
        model: "openai/gpt-4o-mini".to_string(),
        provider_cost_usd: Some("0.000123".to_string()),
    };

    client.emit_cost_event_best_effort("req-1", Some(&usage), "openai/gpt-4o-mini");

    let events = sink.take();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].provider, "openrouter");
    assert_eq!(events[0].cost_usd.as_deref(), Some("0.000123"));
    assert!(events[0].cost_usd_present);
    assert_eq!(events[0].cost_source, "openrouter_usage");
}

#[test]
fn openrouter_missing_cost_emits_unknown_cost() {
    let sink = Arc::new(CollectingSink::default());
    let client = make_test_openrouter_client(true).with_cost_event_sink(sink.clone(), cost_context());
    let usage = UsageData {
        prompt_tokens: 10,
        completion_tokens: 20,
        total_tokens: 30,
        cached_input_tokens: 0,
        cache_creation_tokens: 0,
        provider: "openrouter".to_string(),
        model: "openai/gpt-4o-mini".to_string(),
        provider_cost_usd: None,
    };

    client.emit_cost_event_best_effort("req-2", Some(&usage), "openai/gpt-4o-mini");

    let events = sink.take();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].cost_usd, None);
    assert!(!events[0].cost_usd_present);
    assert_eq!(events[0].cost_source, "unknown");
}

#[test]
fn openai_cost_event_uses_static_price_table() {
    let sink = Arc::new(CollectingSink::default());
    let client = make_test_openai_client().with_cost_event_sink(sink.clone(), cost_context());
    let usage = UsageData {
        prompt_tokens: 1_000,
        completion_tokens: 500,
        total_tokens: 1_500,
        cached_input_tokens: 0,
        cache_creation_tokens: 0,
        provider: "openai".to_string(),
        model: "gpt-4.1".to_string(),
        provider_cost_usd: None,
    };

    client.emit_cost_event_best_effort("req-3", Some(&usage), "gpt-4.1");

    let events = sink.take();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].cost_usd.as_deref(), Some("0.006"));
    assert!(events[0].cost_usd_present);
    assert_eq!(events[0].cost_source, "table_openai");
}

#[test]
fn anthropic_cost_event_uses_static_price_table() {
    let sink = Arc::new(CollectingSink::default());
    let client = make_test_anthropic_client().with_cost_event_sink(sink.clone(), cost_context());
    let usage = UsageData {
        prompt_tokens: 1_000,
        completion_tokens: 500,
        total_tokens: 1_500,
        cached_input_tokens: 0,
        cache_creation_tokens: 0,
        provider: "anthropic".to_string(),
        model: "claude-sonnet-4".to_string(),
        provider_cost_usd: None,
    };

    client.emit_cost_event_best_effort("req-4", Some(&usage), "claude-sonnet-4");

    let events = sink.take();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].cost_usd.as_deref(), Some("0.0105"));
    assert!(events[0].cost_usd_present);
    assert_eq!(events[0].cost_source, "table_anthropic");
}

#[test]
fn anthropic_cost_event_prices_prompt_cache_reads_and_writes() {
    let sink = Arc::new(CollectingSink::default());
    let client = make_test_anthropic_client().with_cost_event_sink(sink.clone(), cost_context());
    let usage = UsageData {
        prompt_tokens: 1_000,
        completion_tokens: 500,
        total_tokens: 1_500,
        cached_input_tokens: 800,
        cache_creation_tokens: 400,
        provider: "anthropic".to_string(),
        model: "claude-sonnet-4".to_string(),
        provider_cost_usd: None,
    };

    client.emit_cost_event_best_effort("req-4b", Some(&usage), "claude-sonnet-4");

    let events = sink.take();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].cost_usd.as_deref(), Some("0.01224"));
    assert!(events[0].cost_usd_present);
    assert_eq!(events[0].cost_source, "table_anthropic");
}

#[test]
fn unknown_openai_model_emits_unknown_cost() {
    let sink = Arc::new(CollectingSink::default());
    let client = make_test_openai_client().with_cost_event_sink(sink.clone(), cost_context());
    let usage = UsageData {
        prompt_tokens: 1_000,
        completion_tokens: 500,
        total_tokens: 1_500,
        cached_input_tokens: 0,
        cache_creation_tokens: 0,
        provider: "openai".to_string(),
        model: "missing-model".to_string(),
        provider_cost_usd: None,
    };

    client.emit_cost_event_best_effort("req-5", Some(&usage), "missing-model");

    let events = sink.take();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].cost_usd, None);
    assert!(!events[0].cost_usd_present);
    assert_eq!(events[0].cost_source, "unknown");
}

#[test]
fn openrouter_cost_opt_in_is_disabled_when_feature_is_off() {
    let client = make_test_openrouter_client(false);
    let request = ChatRequest {
        model: client.provider.normalize_model(&client.llm_config.model),
        messages: vec![Message::user("hi")],
        tools: None,
        temperature: client.llm_config.temperature,
        max_tokens: client.llm_config.max_tokens,
        stream: true,
        reasoning_effort: None,
        prompt_cache_key: None,
        user: None,
        stream_options: Some(StreamOptions {
            include_usage: true,
        }),
        usage: (client.provider == Provider::OpenRouter && client.cost_events_enabled())
            .then_some(UsageOptions { include: true }),
    };
    let json = serde_json::to_value(&request).unwrap();
    assert!(json.get("usage").is_none());
}

#[test]
fn sink_panic_is_caught_and_does_not_propagate() {
    let client = make_test_openai_client().with_cost_event_sink(Arc::new(PanicSink), cost_context());
    let usage = UsageData {
        prompt_tokens: 1_000,
        completion_tokens: 500,
        total_tokens: 1_500,
        cached_input_tokens: 0,
        cache_creation_tokens: 0,
        provider: "openai".to_string(),
        model: "gpt-4.1".to_string(),
        provider_cost_usd: None,
    };

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        client.emit_cost_event_best_effort("req-6", Some(&usage), "gpt-4.1");
    }));

    assert!(result.is_ok(), "sink panic must not escape the LLM path");
}

#[test]
fn anthropic_system_extraction() {
    let messages = vec![Message::system("You are helpful."), Message::user("Hello")];

    let client = make_test_anthropic_client();
    let body = client.build_anthropic_request(&messages, None, false);

    // With prompt caching enabled by default, system is emitted as a content-block array.
    let system_blocks = body["system"].as_array().expect("system should be array");
    assert_eq!(system_blocks.len(), 1);
    assert_eq!(system_blocks[0]["type"], "text");
    assert_eq!(system_blocks[0]["text"], "You are helpful.");

    let msgs = body["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0]["role"], "user");
}

#[test]
fn anthropic_tool_result_conversion() {
    let messages = vec![
        Message::user("test"),
        Message {
            role: Role::Assistant,
            content: None,
            tool_calls: Some(vec![ToolCall {
                id: "call_1".to_string(),
                call_type: "function".to_string(),
                function: FunctionCall {
                    name: "read_memory".to_string(),
                    arguments: r#"{"filename":"test.md"}"#.to_string(),
                },
            }]),
            tool_call_id: None,
            timestamp: None,
        },
        Message::tool_result("call_1", "file contents here"),
    ];

    let anthropic_msgs = build_anthropic_messages(&messages);

    // Should have: user, assistant, user (with tool_result)
    assert_eq!(anthropic_msgs.len(), 3);

    // Assistant message should have tool_use block
    let assistant = &anthropic_msgs[1];
    let content = assistant["content"].as_array().unwrap();
    assert_eq!(content[0]["type"], "tool_use");
    assert_eq!(content[0]["name"], "read_memory");

    // Tool result should be in a user message
    let tool_user = &anthropic_msgs[2];
    assert_eq!(tool_user["role"], "user");
    let tool_content = tool_user["content"].as_array().unwrap();
    assert_eq!(tool_content[0]["type"], "tool_result");
    assert_eq!(tool_content[0]["tool_use_id"], "call_1");
}

#[test]
fn anthropic_tool_definition_conversion() {
    let tools = vec![ToolDefinition::new(
        "test_tool",
        "A test tool",
        serde_json::json!({"type": "object", "properties": {"x": {"type": "string"}}}),
    )];

    let client = make_test_anthropic_client();

    let body = client.build_anthropic_request(&[Message::user("hi")], Some(&tools), false);

    let api_tools = body["tools"].as_array().unwrap();
    assert_eq!(api_tools.len(), 1);
    assert_eq!(api_tools[0]["name"], "test_tool");
    assert_eq!(api_tools[0]["description"], "A test tool");
    assert!(api_tools[0]["input_schema"].is_object());
    // Should NOT have "function" wrapping or "type":"function"
    assert!(api_tools[0]["function"].is_null());
}

#[test]
fn anthropic_adjacent_tool_results_merge() {
    let messages = vec![
        Message::user("test"),
        Message {
            role: Role::Assistant,
            content: None,
            tool_calls: Some(vec![
                ToolCall {
                    id: "call_1".to_string(),
                    call_type: "function".to_string(),
                    function: FunctionCall {
                        name: "tool_a".to_string(),
                        arguments: "{}".to_string(),
                    },
                },
                ToolCall {
                    id: "call_2".to_string(),
                    call_type: "function".to_string(),
                    function: FunctionCall {
                        name: "tool_b".to_string(),
                        arguments: "{}".to_string(),
                    },
                },
            ]),
            tool_call_id: None,
            timestamp: None,
        },
        Message::tool_result("call_1", "result a"),
        Message::tool_result("call_2", "result b"),
    ];

    let anthropic_msgs = build_anthropic_messages(&messages);

    // user, assistant, user (merged tool results)
    assert_eq!(anthropic_msgs.len(), 3);

    let tool_user = &anthropic_msgs[2];
    let tool_content = tool_user["content"].as_array().unwrap();
    assert_eq!(tool_content.len(), 2);
    assert_eq!(tool_content[0]["tool_use_id"], "call_1");
    assert_eq!(tool_content[1]["tool_use_id"], "call_2");
}

#[test]
fn parse_anthropic_response_text_only() {
    let resp = serde_json::json!({
        "content": [
            {"type": "text", "text": "Hello there!"}
        ],
        "stop_reason": "end_turn"
    });

    let msg = parse_anthropic_response(&resp).unwrap();
    assert_eq!(msg.role, Role::Assistant);
    assert_eq!(msg.text_content(), Some("Hello there!"));
    assert!(msg.tool_calls.is_none());
}

#[test]
fn parse_anthropic_response_with_tool_use() {
    let resp = serde_json::json!({
        "content": [
            {"type": "text", "text": "Let me check."},
            {
                "type": "tool_use",
                "id": "toolu_123",
                "name": "read_memory",
                "input": {"filename": "test.md"}
            }
        ],
        "stop_reason": "tool_use"
    });

    let msg = parse_anthropic_response(&resp).unwrap();
    assert_eq!(msg.text_content(), Some("Let me check."));
    let tcs = msg.tool_calls.unwrap();
    assert_eq!(tcs.len(), 1);
    assert_eq!(tcs[0].id, "toolu_123");
    assert_eq!(tcs[0].function.name, "read_memory");
    assert!(tcs[0].function.arguments.contains("test.md"));
}

// ── ProviderCooldown tests ──

#[test]
fn cooldown_initially_inactive() {
    let cd = ProviderCooldown::default();
    assert!(!cd.is_active());
    assert_eq!(cd.error_count, 0);
}

#[test]
fn cooldown_active_after_failure() {
    let mut cd = ProviderCooldown::default();
    cd.record_failure(FailoverReason::RateLimit);
    assert!(cd.is_active());
    assert_eq!(cd.error_count, 1);
    assert_eq!(cd.reason, Some(FailoverReason::RateLimit));
}

#[test]
fn cooldown_reset_on_success() {
    let mut cd = ProviderCooldown::default();
    cd.record_failure(FailoverReason::Overloaded);
    assert!(cd.is_active());

    cd.record_success();
    assert!(!cd.is_active());
    assert_eq!(cd.error_count, 0);
    assert!(cd.reason.is_none());
}

#[test]
fn cooldown_backoff_escalates() {
    let mut cd = ProviderCooldown::default();
    cd.record_failure(FailoverReason::RateLimit);
    let first = cd.cooldown_until.unwrap();

    cd.cooldown_until = None; // simulate expiry
    cd.error_count = 0; // reset will happen in record_failure
    cd.record_failure(FailoverReason::RateLimit);
    let second = cd.cooldown_until.unwrap();

    // Both cooldowns should be in the future, second may be different
    // since error_count resets on expiry
    assert!(first > std::time::Instant::now() - Duration::from_secs(120));
    assert!(second > std::time::Instant::now() - Duration::from_secs(120));
}

#[test]
fn cooldown_auth_has_longer_base() {
    let mut cd1 = ProviderCooldown::default();
    cd1.record_failure(FailoverReason::RateLimit);
    let rl_until = cd1.cooldown_until.unwrap();

    let mut cd2 = ProviderCooldown::default();
    cd2.record_failure(FailoverReason::Auth);
    let auth_until = cd2.cooldown_until.unwrap();

    // Auth base is 300s, RateLimit base is 60s — auth should be further out
    assert!(auth_until > rl_until);
}

#[test]
fn effective_base_url_uses_config_override() {
    let mut config = Config::default();
    config.llm.provider = Some("ollama".to_string());
    config.llm.model = "llama3.3".to_string();
    config.llm.base_url = Some("http://custom:8080/v1/chat/completions".to_string());
    let client = LlmClient::new(&config).expect("should create client");
    assert_eq!(
        client.effective_base_url().as_ref(),
        "http://custom:8080/v1/chat/completions"
    );
}

#[test]
fn effective_base_url_falls_back_to_provider_default() {
    let mut config = Config::default();
    config.llm.provider = Some("ollama".to_string());
    config.llm.model = "llama3.3".to_string();
    let client = LlmClient::new(&config).expect("should create client");
    assert_eq!(
        client.effective_base_url().as_ref(),
        "http://localhost:11434/v1/chat/completions"
    );
}

#[test]
fn normalize_gemini_base_table() {
    let canonical = "https://generativelanguage.googleapis.com/v1beta/openai/chat/completions";
    // (input, expected, must_be_borrowed). `Borrowed` means we returned the
    // input unchanged — a real perf invariant when the canonical URL or a
    // non-gemini host is passed in.
    let cases: &[(&str, &str, bool)] = &[
        (canonical, canonical, true),
        (
            "https://generativelanguage.googleapis.com",
            canonical,
            false,
        ),
        (
            "https://generativelanguage.googleapis.com/",
            canonical,
            false,
        ),
        (
            "https://generativelanguage.googleapis.com/v1beta",
            canonical,
            false,
        ),
        (
            "https://generativelanguage.googleapis.com/v1beta/",
            canonical,
            false,
        ),
        (
            "https://generativelanguage.googleapis.com/v1beta/openai",
            canonical,
            false,
        ),
        (
            "https://generativelanguage.googleapis.com/v1beta/openai/",
            canonical,
            false,
        ),
        (
            "https://generativelanguage.googleapis.com/openai",
            canonical,
            false,
        ),
        (
            "https://generativelanguage.googleapis.com/v1",
            canonical,
            false,
        ),
        // OpenAI-compatible proxies using /v1 must not be truncated.
        (
            "https://my-proxy.example.com/v1",
            "https://my-proxy.example.com/v1",
            true,
        ),
        (
            "https://api.openai.com/v1/chat/completions",
            "https://api.openai.com/v1/chat/completions",
            true,
        ),
    ];
    for (input, expected, want_borrowed) in cases {
        let got = normalize_gemini_base(input);
        assert_eq!(got.as_ref(), *expected, "wrong output for {input}");
        assert_eq!(
            matches!(got, Cow::Borrowed(_)),
            *want_borrowed,
            "wrong Cow variant for {input}"
        );
    }
}

#[test]
fn effective_base_url_normalizes_gemini_override() {
    let mut config = Config::default();
    config.llm.provider = Some("gemini".to_string());
    config.llm.api_key_env = "GEMINI_API_KEY".to_string();
    config.llm.model = "gemini-2.5-flash".to_string();
    config.llm.base_url =
        Some("https://generativelanguage.googleapis.com/v1beta/openai".to_string());
    std::env::set_var("GEMINI_API_KEY", "test-key");
    let client = LlmClient::new(&config).expect("should create client");
    assert_eq!(
        client.effective_base_url().as_ref(),
        "https://generativelanguage.googleapis.com/v1beta/openai/chat/completions"
    );
    std::env::remove_var("GEMINI_API_KEY");
}

#[test]
fn anthropic_request_thinking_off_includes_temperature() {
    let mut config = Config::default();
    config.llm.provider = Some("anthropic".to_string());
    config.llm.api_key_env = "ANTHROPIC_API_KEY".to_string();
    config.llm.model = "claude-sonnet-4-20250514".to_string();
    config.llm.thinking = crate::config::ThinkingLevel::Off;
    std::env::set_var("ANTHROPIC_API_KEY", "test-key");
    let client = LlmClient::new(&config).expect("should create client");

    let messages = vec![Message::user("hi")];
    let body = client.build_anthropic_request(&messages, None, false);

    assert!(
        body.get("temperature").is_some(),
        "temperature should be present when thinking is off"
    );
    assert!(
        body.get("thinking").is_none(),
        "thinking field should not be present when off"
    );
    std::env::remove_var("ANTHROPIC_API_KEY");
}

#[test]
fn anthropic_request_thinking_enabled_omits_temperature() {
    let mut config = Config::default();
    config.llm.provider = Some("anthropic".to_string());
    config.llm.api_key_env = "ANTHROPIC_API_KEY".to_string();
    config.llm.model = "claude-sonnet-4-20250514".to_string();
    config.llm.thinking = crate::config::ThinkingLevel::High;
    std::env::set_var("ANTHROPIC_API_KEY", "test-key");
    let client = LlmClient::new(&config).expect("should create client");

    let messages = vec![Message::user("hi")];
    let body = client.build_anthropic_request(&messages, None, false);

    assert!(
        body.get("temperature").is_none(),
        "temperature must be omitted when thinking is enabled"
    );
    let thinking = body
        .get("thinking")
        .expect("thinking field should be present");
    assert_eq!(thinking["type"], "enabled");
    assert_eq!(thinking["budget_tokens"], 16384);

    // max_tokens should be at least budget + 1024
    let max_tokens = body["max_tokens"].as_u64().unwrap();
    assert!(max_tokens >= 16384 + 1024);
    std::env::remove_var("ANTHROPIC_API_KEY");
}

#[test]
fn openai_request_includes_reasoning_effort() {
    let request = ChatRequest {
        model: "o3".to_string(),
        messages: vec![Message::user("test")],
        tools: None,
        temperature: 0.7,
        max_tokens: 4096,
        stream: false,
        reasoning_effort: Some("high".to_string()),
        prompt_cache_key: None,
        user: None,
        stream_options: None,
        usage: None,
    };
    let json = serde_json::to_value(&request).unwrap();
    assert_eq!(json["reasoning_effort"], "high");
}

#[test]
fn openai_request_omits_reasoning_effort_when_none() {
    let request = ChatRequest {
        model: "gpt-4.1".to_string(),
        messages: vec![Message::user("test")],
        tools: None,
        temperature: 0.7,
        max_tokens: 4096,
        stream: false,
        reasoning_effort: None,
        prompt_cache_key: None,
        user: None,
        stream_options: None,
        usage: None,
    };
    let json = serde_json::to_value(&request).unwrap();
    assert!(json.get("reasoning_effort").is_none());
}

#[test]
fn openai_request_serializes_prompt_cache_key_when_set() {
    let request = ChatRequest {
        model: "gpt-4.1".to_string(),
        messages: vec![Message::user("hi")],
        tools: None,
        temperature: 0.7,
        max_tokens: 4096,
        stream: false,
        reasoning_effort: None,
        prompt_cache_key: Some("sess-abc-123".to_string()),
        user: Some("sess-abc-123".to_string()),
        stream_options: None,
        usage: None,
    };
    let json = serde_json::to_value(&request).unwrap();
    assert_eq!(json["prompt_cache_key"], "sess-abc-123");
    assert_eq!(json["user"], "sess-abc-123");
}

#[test]
fn openai_request_omits_prompt_cache_key_when_none() {
    let request = ChatRequest {
        model: "gpt-4.1".to_string(),
        messages: vec![Message::user("hi")],
        tools: None,
        temperature: 0.7,
        max_tokens: 4096,
        stream: false,
        reasoning_effort: None,
        prompt_cache_key: None,
        user: None,
        stream_options: None,
        usage: None,
    };
    let json = serde_json::to_value(&request).unwrap();
    assert!(json.get("prompt_cache_key").is_none());
    assert!(json.get("user").is_none());
}

#[test]
fn openai_stream_request_serializes_include_usage() {
    // Invariant: streaming requests MUST set `stream_options.include_usage
    // = true`. Without this, OpenAI-compatible providers (OpenAI,
    // OpenRouter, DeepSeek, Groq, Gemini-via-OpenAI) never emit a `usage`
    // chunk and `/usage` + monthly budget tracking silently return zeros.
    let request = ChatRequest {
        model: "grok-4".to_string(),
        messages: vec![Message::user("hi")],
        tools: None,
        temperature: 0.7,
        max_tokens: 4096,
        stream: true,
        reasoning_effort: None,
        prompt_cache_key: None,
        user: None,
        stream_options: Some(StreamOptions {
            include_usage: true,
        }),
        usage: None,
    };
    let json = serde_json::to_value(&request).unwrap();
    assert_eq!(json["stream_options"]["include_usage"], true);
}

#[test]
fn openai_nonstream_request_omits_stream_options() {
    // Non-streaming responses carry usage natively; `stream_options` is
    // meaningless there and should not be sent.
    let request = ChatRequest {
        model: "gpt-4.1".to_string(),
        messages: vec![Message::user("hi")],
        tools: None,
        temperature: 0.7,
        max_tokens: 4096,
        stream: false,
        reasoning_effort: None,
        prompt_cache_key: None,
        user: None,
        stream_options: None,
        usage: None,
    };
    let json = serde_json::to_value(&request).unwrap();
    assert!(json.get("stream_options").is_none());
}

#[test]
fn anthropic_usage_carries_input_tokens_from_message_start() {
    // Anthropic reports `input_tokens` on `message_start` and only
    // `output_tokens` on `message_delta`. The parser must stash the
    // input count and emit it when the delta fires — else prompt_tokens
    // is always 0 and `/usage` + budget tracking silently return zeros.
    use crate::sse::SseAction;
    let mut tool_idx = 0usize;
    let mut is_tool = false;
    let mut is_thinking = false;
    let mut pending_input = 0u64;
    let mut pending_cached = 0u64;
    let mut pending_created = 0u64;

    // Feed message_start with input_tokens=150 → should be stashed, no emit.
    let start = r#"data: {"type":"message_start","message":{"usage":{"input_tokens":150,"output_tokens":0}}}"#;
    let action = LlmClient::parse_anthropic_sse_line(
        start,
        "anthropic",
        "claude-opus-4",
        &mut tool_idx,
        &mut is_tool,
        &mut is_thinking,
        &mut pending_input,
        &mut pending_cached,
        &mut pending_created,
    );
    assert!(matches!(action, SseAction::Continue));
    assert_eq!(pending_input, 150);

    // Feed message_delta with only output_tokens — emit should merge.
    let delta = r#"data: {"type":"message_delta","usage":{"output_tokens":42},"delta":{"stop_reason":"end_turn"}}"#;
    let action = LlmClient::parse_anthropic_sse_line(
        delta,
        "anthropic",
        "claude-opus-4",
        &mut tool_idx,
        &mut is_tool,
        &mut is_thinking,
        &mut pending_input,
        &mut pending_cached,
        &mut pending_created,
    );
    let events = match action {
        SseAction::Done(ev) | SseAction::Emit(ev) => ev,
        _ => panic!("expected emit/done for message_delta"),
    };
    let usage = events
        .iter()
        .find_map(|e| match e {
            StreamEvent::Usage(u) => Some(u.clone()),
            _ => None,
        })
        .expect("message_delta must emit a Usage event");
    assert_eq!(usage.prompt_tokens, 150);
    assert_eq!(usage.completion_tokens, 42);
    assert_eq!(usage.total_tokens, 192);
}

#[test]
fn anthropic_usage_prefers_larger_input_between_start_and_delta() {
    // If the delta re-emits input_tokens (cache-refresh paths sometimes do),
    // use whichever is larger so we never under-report.
    use crate::sse::SseAction;
    let mut tool_idx = 0usize;
    let mut is_tool = false;
    let mut is_thinking = false;
    let mut pending_input = 0u64;
    let mut pending_cached = 0u64;
    let mut pending_created = 0u64;

    let start = r#"data: {"type":"message_start","message":{"usage":{"input_tokens":100,"output_tokens":0}}}"#;
    let _ = LlmClient::parse_anthropic_sse_line(
        start,
        "anthropic",
        "claude-opus-4",
        &mut tool_idx,
        &mut is_tool,
        &mut is_thinking,
        &mut pending_input,
        &mut pending_cached,
        &mut pending_created,
    );

    let delta = r#"data: {"type":"message_delta","usage":{"input_tokens":120,"output_tokens":10},"delta":{"stop_reason":"end_turn"}}"#;
    let action = LlmClient::parse_anthropic_sse_line(
        delta,
        "anthropic",
        "claude-opus-4",
        &mut tool_idx,
        &mut is_tool,
        &mut is_thinking,
        &mut pending_input,
        &mut pending_cached,
        &mut pending_created,
    );
    let events = match action {
        SseAction::Done(ev) | SseAction::Emit(ev) => ev,
        _ => panic!("expected emit/done"),
    };
    let usage = events
        .iter()
        .find_map(|e| match e {
            StreamEvent::Usage(u) => Some(u.clone()),
            _ => None,
        })
        .expect("expected Usage event");
    assert_eq!(usage.prompt_tokens, 120);
    assert_eq!(usage.completion_tokens, 10);
}

#[test]
fn openai_sanitizes_null_content_on_tool_call_messages() {
    // Gemini (via OpenRouter) rejects `"content": null`. The sanitization
    // in stream_chat_openai_inner should replace None content on assistant
    // tool-call messages with an empty string.
    let messages = vec![
        Message::user("hello"),
        Message {
            role: Role::Assistant,
            content: None,
            tool_calls: Some(vec![ToolCall {
                id: "call_1".to_string(),
                call_type: "function".to_string(),
                function: FunctionCall {
                    name: "read_memory".to_string(),
                    arguments: "{}".to_string(),
                },
            }]),
            tool_call_id: None,
            timestamp: None,
        },
        Message::tool_result("call_1", "result"),
    ];

    let sanitized = sanitize_openai_messages(&messages);
    assert_eq!(sanitized.len(), 3);

    let request = ChatRequest {
        model: "google/gemini-2.5-flash".to_string(),
        messages: sanitized,
        tools: None,
        temperature: 0.7,
        max_tokens: 4096,
        stream: false,
        reasoning_effort: None,
        prompt_cache_key: None,
        user: None,
        stream_options: None,
        usage: None,
    };
    let json = serde_json::to_value(&request).unwrap();
    let msgs = json["messages"].as_array().unwrap();
    // Assistant message (index 1) should have content "" not null
    assert_eq!(msgs[1]["content"], "");
    assert!(!msgs[1]["content"].is_null());
    // User message content should be unchanged
    assert_eq!(msgs[0]["content"], "hello");
}

#[test]
fn openai_drops_empty_user_message_for_gemini_compat() {
    // Gemini 2.5 Pro/Flash reject user messages with no content parts.
    let messages = vec![
        Message::user("hello"),
        Message::assistant("hi there"),
        Message {
            role: Role::User,
            content: Some(crate::types::MessageContent::Text(String::new())),
            tool_calls: None,
            tool_call_id: None,
            timestamp: None,
        },
    ];

    let sanitized = sanitize_openai_messages(&messages);
    // Empty user message should be dropped
    assert_eq!(sanitized.len(), 2);
    assert_eq!(
        sanitized[0].content.as_ref().unwrap().text().unwrap(),
        "hello"
    );
    assert_eq!(
        sanitized[1].content.as_ref().unwrap().text().unwrap(),
        "hi there"
    );
}

#[test]
fn openai_drops_empty_assistant_message_without_tool_calls() {
    // Empty assistant responses (e.g. suppressed heartbeat acks) should be
    // dropped when they have no tool calls, to avoid Gemini rejection.
    let messages = vec![
        Message::user("*heartbeat tick*"),
        Message {
            role: Role::Assistant,
            content: Some(crate::types::MessageContent::Text(String::new())),
            tool_calls: None,
            tool_call_id: None,
            timestamp: None,
        },
        Message::user("actual question"),
    ];

    let sanitized = sanitize_openai_messages(&messages);
    // Empty assistant message should be dropped
    assert_eq!(sanitized.len(), 2);
    assert_eq!(
        sanitized[0].content.as_ref().unwrap().text().unwrap(),
        "*heartbeat tick*"
    );
    assert_eq!(
        sanitized[1].content.as_ref().unwrap().text().unwrap(),
        "actual question"
    );
}

#[test]
fn openai_drops_none_content_assistant_without_tool_calls() {
    // Assistant message with None content and no tool calls should be dropped.
    let messages = vec![
        Message::user("hello"),
        Message {
            role: Role::Assistant,
            content: None,
            tool_calls: None,
            tool_call_id: None,
            timestamp: None,
        },
        Message::user("follow up"),
    ];

    let sanitized = sanitize_openai_messages(&messages);
    assert_eq!(sanitized.len(), 2);
}

#[test]
fn openai_preserves_tool_result_messages() {
    // Tool result messages (with tool_call_id) should never be dropped,
    // even if their content happens to be empty.
    let messages = vec![
        Message::user("hello"),
        Message {
            role: Role::Assistant,
            content: None,
            tool_calls: Some(vec![ToolCall {
                id: "call_1".to_string(),
                call_type: "function".to_string(),
                function: FunctionCall {
                    name: "read_memory".to_string(),
                    arguments: "{}".to_string(),
                },
            }]),
            tool_call_id: None,
            timestamp: None,
        },
        Message {
            role: Role::Tool,
            content: Some(crate::types::MessageContent::Text(String::new())),
            tool_calls: None,
            tool_call_id: Some("call_1".to_string()),
            timestamp: None,
        },
    ];

    let sanitized = sanitize_openai_messages(&messages);
    // All 3 messages should be preserved (tool result has tool_call_id)
    assert_eq!(sanitized.len(), 3);
}

// ── Prompt caching (Anthropic) ──

#[allow(clippy::expect_used, clippy::unwrap_used)]
#[test]
fn anthropic_request_includes_cache_control_on_system() {
    let client = make_test_anthropic_client();
    let messages = vec![Message::system("stable system prompt"), Message::user("hi")];
    let body = client.build_anthropic_request(&messages, None, false);

    let system = body["system"]
        .as_array()
        .expect("system should be an array of blocks when caching is enabled");
    assert_eq!(system.len(), 1);
    assert_eq!(system[0]["type"], "text");
    assert_eq!(system[0]["text"], "stable system prompt");
    assert_eq!(system[0]["cache_control"]["type"], "ephemeral");
}

#[allow(clippy::expect_used, clippy::unwrap_used)]
#[test]
fn anthropic_request_caches_last_two_messages() {
    let client = make_test_anthropic_client();
    // 5 user/assistant messages so we can verify ONLY the last two carry markers.
    let messages = vec![
        Message::system("sys"),
        Message::user("turn 1"),
        Message::user("turn 2"),
        Message::user("turn 3"),
        Message::user("turn 4"),
        Message::user("turn 5"),
    ];
    let body = client.build_anthropic_request(&messages, None, false);
    let msgs = body["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 5, "system is stripped, 5 user msgs remain");

    // Last two should carry cache_control on their last content block.
    for idx in [3usize, 4] {
        let blocks = msgs[idx]["content"].as_array().unwrap();
        let last = blocks.last().unwrap();
        assert_eq!(
            last["cache_control"]["type"], "ephemeral",
            "msg[{idx}] last block should carry cache_control"
        );
    }

    // Earlier messages must NOT carry the marker. They may still be plain strings
    // (apply_message_cache_control only upgrades the trailing messages it marks).
    for idx in [0usize, 1, 2] {
        let content = &msgs[idx]["content"];
        if let Some(blocks) = content.as_array() {
            for block in blocks {
                assert!(
                    block.get("cache_control").is_none(),
                    "msg[{idx}] should not carry cache_control"
                );
            }
        } else {
            assert!(
                content.is_string(),
                "msg[{idx}] content should be string or block array"
            );
        }
    }
}

#[allow(clippy::expect_used, clippy::unwrap_used)]
#[test]
fn anthropic_request_cache_disabled_emits_no_markers() {
    let env_var = "BORG_TEST_ANTHROPIC_CACHE_OFF";
    std::env::set_var(env_var, "sk-test");
    let mut config = Config::default();
    config.llm.provider = Some("anthropic".to_string());
    config.llm.api_key_env = env_var.to_string();
    config.llm.model = "claude-sonnet-4".to_string();
    config.llm.cache.enabled = false;
    let client = LlmClient::new(&config).unwrap();

    let messages = vec![
        Message::system("sys"),
        Message::user("first"),
        Message::user("second"),
    ];
    let body = client.build_anthropic_request(&messages, None, false);

    // System reverts to a plain string.
    assert_eq!(body["system"].as_str(), Some("sys"));

    // No cache_control markers anywhere in messages.
    let msgs = body["messages"].as_array().unwrap();
    for msg in msgs {
        if let Some(blocks) = msg["content"].as_array() {
            for block in blocks {
                assert!(
                    block.get("cache_control").is_none(),
                    "no cache_control expected when caching disabled"
                );
            }
        }
    }
    std::env::remove_var(env_var);
}

#[allow(clippy::unwrap_used)]
#[test]
fn apply_message_cache_control_handles_fewer_than_two_messages() {
    let mut msgs = vec![serde_json::json!({
        "role": "user",
        "content": [{ "type": "text", "text": "only one" }],
    })];
    let marker = build_cache_control_marker(crate::config::CacheTtl::FiveMin);
    apply_message_cache_control(&mut msgs, 2, &marker);
    let blocks = msgs[0]["content"].as_array().unwrap();
    assert_eq!(blocks[0]["cache_control"]["type"], "ephemeral");
}

#[allow(clippy::expect_used)]
#[test]
fn apply_message_cache_control_upgrades_string_content() {
    // Plain-string content is upgraded to a single text block so cache_control can attach.
    let mut msgs = vec![
        serde_json::json!({ "role": "user", "content": "earlier" }),
        serde_json::json!({ "role": "user", "content": "latest" }),
    ];
    let marker = build_cache_control_marker(crate::config::CacheTtl::FiveMin);
    apply_message_cache_control(&mut msgs, 2, &marker);
    for msg in &msgs {
        let blocks = msg["content"]
            .as_array()
            .expect("string content should have been upgraded to a block array");
        assert_eq!(blocks[0]["type"], "text");
        assert_eq!(blocks[0]["cache_control"]["type"], "ephemeral");
    }
}

// ── New prompt caching tests ──

#[allow(clippy::expect_used, clippy::unwrap_used)]
#[test]
fn anthropic_request_marks_last_tool_with_cache_control() {
    let client = make_test_anthropic_client();
    let messages = vec![Message::system("sys"), Message::user("hi")];
    let tools = vec![make_tool("a"), make_tool("b"), make_tool("c")];
    let body = client.build_anthropic_request(&messages, Some(&tools), false);
    let tools_arr = body["tools"].as_array().expect("tools array");
    assert_eq!(tools_arr.len(), 3);
    assert!(
        tools_arr[0].get("cache_control").is_none(),
        "first tool must NOT carry cache_control"
    );
    assert!(
        tools_arr[1].get("cache_control").is_none(),
        "middle tool must NOT carry cache_control"
    );
    assert_eq!(
        tools_arr[2]["cache_control"]["type"], "ephemeral",
        "last tool should carry cache_control to cache the whole array"
    );
}

#[allow(clippy::unwrap_used)]
#[test]
fn anthropic_request_cache_disabled_leaves_tools_unmarked() {
    let env_var = "BORG_TEST_ANTHROPIC_TOOLS_NOCACHE";
    std::env::set_var(env_var, "sk-test");
    let mut config = Config::default();
    config.llm.provider = Some("anthropic".to_string());
    config.llm.api_key_env = env_var.to_string();
    config.llm.model = "claude-sonnet-4".to_string();
    config.llm.cache.enabled = false;
    let client = LlmClient::new(&config).unwrap();

    let tools = vec![make_tool("a"), make_tool("b")];
    let messages = vec![Message::system("sys"), Message::user("hi")];
    let body = client.build_anthropic_request(&messages, Some(&tools), false);
    let tools_arr = body["tools"].as_array().unwrap();
    for tool in tools_arr {
        assert!(tool.get("cache_control").is_none());
    }
    std::env::remove_var(env_var);
}

#[test]
fn apply_message_cache_control_skips_thinking_block() {
    // Assistant message ending in a thinking block must still be cacheable —
    // the marker should land on the previous (text) block instead.
    let mut msgs = vec![serde_json::json!({
        "role": "assistant",
        "content": [
            { "type": "text", "text": "answer text" },
            { "type": "thinking", "thinking": "reasoning..." },
        ],
    })];
    let marker = build_cache_control_marker(crate::config::CacheTtl::FiveMin);
    apply_message_cache_control(&mut msgs, 1, &marker);
    let blocks = msgs[0]["content"].as_array().unwrap();
    // Text block gets the marker.
    assert_eq!(blocks[0]["cache_control"]["type"], "ephemeral");
    // Thinking block must NOT get a marker.
    assert!(blocks[1].get("cache_control").is_none());
}

#[test]
fn apply_message_cache_control_skips_redacted_thinking_block() {
    let mut msgs = vec![serde_json::json!({
        "role": "assistant",
        "content": [
            { "type": "text", "text": "hello" },
            { "type": "redacted_thinking", "data": "opaque" },
        ],
    })];
    let marker = build_cache_control_marker(crate::config::CacheTtl::FiveMin);
    apply_message_cache_control(&mut msgs, 1, &marker);
    let blocks = msgs[0]["content"].as_array().unwrap();
    assert_eq!(blocks[0]["cache_control"]["type"], "ephemeral");
    assert!(blocks[1].get("cache_control").is_none());
}

#[test]
fn apply_message_cache_control_empty_is_noop() {
    let mut msgs: Vec<serde_json::Value> = Vec::new();
    let marker = build_cache_control_marker(crate::config::CacheTtl::FiveMin);
    apply_message_cache_control(&mut msgs, 2, &marker);
    assert!(msgs.is_empty());
}

#[test]
fn apply_message_cache_control_zero_rolling_is_noop() {
    let mut msgs = vec![serde_json::json!({
        "role": "user",
        "content": [{ "type": "text", "text": "hi" }],
    })];
    let marker = build_cache_control_marker(crate::config::CacheTtl::FiveMin);
    apply_message_cache_control(&mut msgs, 0, &marker);
    assert!(
        msgs[0]["content"][0].get("cache_control").is_none(),
        "rolling=0 should not attach any marker"
    );
}

#[test]
fn build_cache_control_marker_five_min() {
    let marker = build_cache_control_marker(crate::config::CacheTtl::FiveMin);
    assert_eq!(marker["type"], "ephemeral");
    assert!(marker.get("ttl").is_none());
}

#[test]
fn build_cache_control_marker_one_hour() {
    let marker = build_cache_control_marker(crate::config::CacheTtl::OneHour);
    assert_eq!(marker["type"], "ephemeral");
    assert_eq!(marker["ttl"], "1h");
}

#[allow(clippy::unwrap_used)]
#[test]
fn anthropic_request_honors_one_hour_ttl() {
    let env_var = "BORG_TEST_ANTHROPIC_TTL_1H";
    std::env::set_var(env_var, "sk-test");
    let mut config = Config::default();
    config.llm.provider = Some("anthropic".to_string());
    config.llm.api_key_env = env_var.to_string();
    config.llm.model = "claude-sonnet-4".to_string();
    config.llm.cache.ttl = crate::config::CacheTtl::OneHour;
    let client = LlmClient::new(&config).unwrap();

    let tools = vec![make_tool("a")];
    let messages = vec![Message::system("sys"), Message::user("hi")];
    let body = client.build_anthropic_request(&messages, Some(&tools), false);
    assert_eq!(body["system"][0]["cache_control"]["ttl"], "1h");
    assert_eq!(body["tools"][0]["cache_control"]["ttl"], "1h");
    std::env::remove_var(env_var);
}

// ── Cache strategy: system_and_3 ──

#[allow(clippy::unwrap_used)]
fn make_system_and_3_client(env_var: &'static str) -> (LlmClient, &'static str) {
    // Each caller passes a unique env var name so parallel tests don't race
    // on set_var / remove_var of a shared key (was causing flaky failures
    // where one test removed the var while another was still reading it).
    std::env::set_var(env_var, "sk-test");
    let mut config = Config::default();
    config.llm.provider = Some("anthropic".to_string());
    config.llm.api_key_env = env_var.to_string();
    config.llm.model = "claude-sonnet-4".to_string();
    config.llm.cache.strategy = crate::config::CacheStrategy::SystemAnd3;
    (LlmClient::new(&config).unwrap(), env_var)
}

#[allow(clippy::unwrap_used)]
#[test]
fn anthropic_request_system_and_3_no_tools_marker() {
    let (client, env_var) = make_system_and_3_client("BORG_TEST_ANTHROPIC_SYSTEM_AND_3_A");
    let tools = vec![make_tool("a"), make_tool("b"), make_tool("c")];
    let messages = vec![Message::system("sys"), Message::user("hi")];
    let body = client.build_anthropic_request(&messages, Some(&tools), false);

    // System still cached.
    assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");

    // Tools must NOT be marked — system_and_3 trades the tools breakpoint
    // for a deeper rolling tail.
    let tools_arr = body["tools"].as_array().unwrap();
    for tool in tools_arr {
        assert!(
            tool.get("cache_control").is_none(),
            "system_and_3 strategy must not cache tools"
        );
    }
    std::env::remove_var(env_var);
}

#[allow(clippy::unwrap_used)]
#[test]
fn anthropic_request_system_and_3_marks_last_three_messages() {
    let (client, env_var) = make_system_and_3_client("BORG_TEST_ANTHROPIC_SYSTEM_AND_3_B");
    let messages = vec![
        Message::system("sys"),
        Message::user("turn 1"),
        Message::user("turn 2"),
        Message::user("turn 3"),
        Message::user("turn 4"),
        Message::user("turn 5"),
    ];
    let body = client.build_anthropic_request(&messages, None, false);
    let msgs = body["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 5);

    // Last THREE should carry cache_control (vs last 2 in the default strategy).
    for idx in [2usize, 3, 4] {
        let blocks = msgs[idx]["content"].as_array().unwrap();
        let last = blocks.last().unwrap();
        assert_eq!(
            last["cache_control"]["type"], "ephemeral",
            "msg[{idx}] should carry cache_control under system_and_3"
        );
    }

    // Earlier messages must NOT carry the marker.
    for idx in [0usize, 1] {
        let content = &msgs[idx]["content"];
        if let Some(blocks) = content.as_array() {
            for block in blocks {
                assert!(
                    block.get("cache_control").is_none(),
                    "msg[{idx}] should not carry cache_control"
                );
            }
        }
    }
    std::env::remove_var(env_var);
}

#[allow(clippy::unwrap_used)]
#[test]
fn anthropic_request_system_and_3_fewer_than_three_messages() {
    // With only 1 user message the helper must not crash; fewer breakpoints is fine.
    let (client, env_var) = make_system_and_3_client("BORG_TEST_ANTHROPIC_SYSTEM_AND_3_C");
    let messages = vec![Message::system("sys"), Message::user("only one")];
    let body = client.build_anthropic_request(&messages, None, false);
    let msgs = body["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 1);
    let blocks = msgs[0]["content"].as_array().unwrap();
    assert_eq!(blocks.last().unwrap()["cache_control"]["type"], "ephemeral");
    std::env::remove_var(env_var);
}

#[test]
fn cache_strategy_roundtrips_snake_case() {
    use crate::config::CacheStrategy;
    let s: String = serde_json::to_string(&CacheStrategy::SystemAnd3).unwrap();
    assert_eq!(s, "\"system_and_3\"");
    let parsed: CacheStrategy = serde_json::from_str("\"tools_system_and_2\"").unwrap();
    assert_eq!(parsed, CacheStrategy::ToolsSystemAnd2);
}

#[test]
fn effective_layout_default_honors_individual_flags() {
    use crate::config::PromptCacheConfig;
    let cfg = PromptCacheConfig {
        cache_system: false,
        cache_tools: true,
        rolling_messages: 2,
        ..Default::default()
    };
    let (sys, tools, rolling) = cfg.effective_layout();
    assert!(!sys);
    assert!(tools);
    assert_eq!(rolling, 2);
}

#[test]
fn effective_layout_system_and_3_overrides_flags() {
    use crate::config::{CacheStrategy, PromptCacheConfig};
    let cfg = PromptCacheConfig {
        cache_system: false,
        cache_tools: true,
        rolling_messages: 1,
        strategy: CacheStrategy::SystemAnd3,
        ..Default::default()
    };
    let (sys, tools, rolling) = cfg.effective_layout();
    assert!(sys, "system_and_3 forces system cache");
    assert!(!tools, "system_and_3 drops tools cache");
    assert_eq!(rolling, 3, "system_and_3 forces rolling=3");
}

#[test]
fn normalize_cache_tokens_anthropic_fields() {
    let usage = serde_json::json!({
        "input_tokens": 1000,
        "output_tokens": 200,
        "cache_read_input_tokens": 800,
        "cache_creation_input_tokens": 50,
    });
    let (read, created) = normalize_cache_tokens(&usage);
    assert_eq!(read, 800);
    assert_eq!(created, 50);
}

#[test]
fn normalize_cache_tokens_openai_details() {
    let usage = serde_json::json!({
        "prompt_tokens": 1500,
        "completion_tokens": 300,
        "prompt_tokens_details": { "cached_tokens": 1200 }
    });
    let (read, created) = normalize_cache_tokens(&usage);
    assert_eq!(read, 1200);
    assert_eq!(created, 0);
}

#[test]
fn normalize_cache_tokens_deepseek_bare_cached_tokens() {
    let usage = serde_json::json!({
        "prompt_tokens": 800,
        "cached_tokens": 600,
    });
    let (read, created) = normalize_cache_tokens(&usage);
    assert_eq!(read, 600);
    assert_eq!(created, 0);
}

#[test]
fn normalize_cache_tokens_missing_fields_returns_zero() {
    let usage = serde_json::json!({ "prompt_tokens": 100 });
    let (read, created) = normalize_cache_tokens(&usage);
    assert_eq!(read, 0);
    assert_eq!(created, 0);
}

#[test]
fn normalize_cache_tokens_handles_bedrock_alt_name() {
    let usage = serde_json::json!({ "cache_write_input_tokens": 42 });
    let (_read, created) = normalize_cache_tokens(&usage);
    assert_eq!(created, 42);
}

#[test]
fn prompt_cache_config_defaults() {
    let cfg = crate::config::PromptCacheConfig::default();
    assert!(cfg.enabled);
    assert!(cfg.cache_tools);
    assert!(cfg.cache_system);
    assert_eq!(cfg.rolling_messages, 2);
    assert_eq!(cfg.ttl, crate::config::CacheTtl::Auto);
}

#[test]
fn prompt_cache_config_roundtrip_toml() {
    let cfg = crate::config::PromptCacheConfig {
        enabled: true,
        ttl: crate::config::CacheTtl::OneHour,
        cache_tools: false,
        cache_system: true,
        rolling_messages: 1,
        strategy: crate::config::CacheStrategy::default(),
    };
    let toml_str = toml::to_string(&cfg).unwrap();
    let parsed: crate::config::PromptCacheConfig = toml::from_str(&toml_str).unwrap();
    assert_eq!(parsed, cfg);
}

#[test]
fn prompt_cache_config_rolling_messages_clamped() {
    let mut cfg = crate::config::PromptCacheConfig::default();
    cfg.rolling_messages = 7;
    assert_eq!(cfg.rolling_messages_clamped(), 2);
    cfg.rolling_messages = 0;
    assert_eq!(cfg.rolling_messages_clamped(), 0);
    cfg.rolling_messages = 1;
    assert_eq!(cfg.rolling_messages_clamped(), 1);
}

#[test]
fn with_prompt_cache_key_sets_field() {
    let client = make_test_anthropic_client().with_prompt_cache_key("sess-xyz");
    assert_eq!(client.prompt_cache_key.as_deref(), Some("sess-xyz"));
}

// Helper: build a minimal ToolDefinition for request-shape tests.
fn make_tool(name: &str) -> ToolDefinition {
    ToolDefinition {
        tool_type: "function".to_string(),
        function: crate::types::FunctionDefinition {
            name: name.to_string(),
            description: format!("tool {name}"),
            parameters: serde_json::json!({ "type": "object", "properties": {} }),
        },
    }
}
