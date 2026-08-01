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
async fn make_test_anthropic_client() -> LlmClient {
    let env_var = "BORG_TEST_ANTHROPIC_LLM";
    std::env::set_var(env_var, "sk-test");
    let mut config = Config::default();
    config.llm.provider = Some("anthropic".to_string());
    config.llm.api_key_env = env_var.to_string();
    config.llm.model = "claude-sonnet-4".to_string();
    LlmClient::new(&config).await.unwrap()
}

async fn make_test_openai_client() -> LlmClient {
    let env_var = "BORG_TEST_OPENAI_LLM";
    std::env::set_var(env_var, "sk-test");
    let mut config = Config::default();
    config.llm.provider = Some("openai".to_string());
    config.llm.api_key_env = env_var.to_string();
    config.llm.model = "gpt-4.1".to_string();
    LlmClient::new(&config).await.unwrap()
}

async fn make_test_openrouter_client(cost_events: bool) -> LlmClient {
    let env_var = "BORG_TEST_OPENROUTER_LLM";
    std::env::set_var(env_var, "sk-test");
    let mut config = Config::default();
    config.llm.provider = Some("openrouter".to_string());
    config.llm.api_key_env = env_var.to_string();
    config.llm.model = "openai/gpt-4o-mini".to_string();
    config.telemetry.cost_events = cost_events;
    LlmClient::new(&config).await.unwrap()
}

fn cost_context() -> CostEventContext {
    CostEventContext {
        session_id: "session-123".to_string(),
        kind: "chat".to_string(),
    }
}

#[tokio::test]
async fn air_gap_blocks_public_cloud_construction() {
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

    let err = match LlmClient::new(&config).await {
        Ok(_) => panic!("expected air-gap rejection"),
        Err(e) => e,
    };
    let msg = format!("{err}");
    assert!(
        msg.contains("air-gap"),
        "expected air-gap rejection, got: {msg}"
    );
}

#[tokio::test]
async fn air_gap_allows_local_construction() {
    // Ollama is Local — must construct cleanly under air-gap.
    let mut config = Config::default();
    config.llm.provider = Some("ollama".to_string());
    config.llm.model = "llama3".to_string();
    config.providers.outbound_allowed = false;

    LlmClient::new(&config)
        .await
        .expect("local provider must be allowed under air-gap");
}

#[tokio::test]
async fn air_gap_custom_endpoint_policy_by_resolved_address() {
    // Custom (BYO) is statically refused under air-gap, EXCEPT when the
    // base_url provably resolves to a private/loopback address — a local
    // llama.cpp/LM Studio server is as air-gap-safe as Ollama. IP literals
    // keep this hermetic (no live DNS). The metadata case matters because
    // link-local counts as "private" in all_addrs_private — it must be
    // refused by the explicit metadata-host guard instead.
    let cases: &[(&str, bool)] = &[
        ("http://127.0.0.1:8080/v1/chat/completions", true),
        ("http://10.0.0.5:8080/v1/chat/completions", true),
        ("http://8.8.8.8/v1/chat/completions", false),
        ("http://169.254.169.254/v1/chat/completions", false),
        ("ftp://127.0.0.1/v1/chat/completions", false),
    ];
    for (base_url, allowed) in cases {
        let mut config = Config::default();
        config.llm.provider = Some("custom".to_string());
        config.llm.model = "gemma-3-4b-it".to_string();
        config.llm.base_url = Some((*base_url).to_string());
        config.providers.outbound_allowed = false;
        match LlmClient::new(&config).await {
            Ok(_) => assert!(*allowed, "base_url {base_url}: expected air-gap refusal"),
            Err(err) => {
                assert!(
                    !*allowed,
                    "base_url {base_url}: expected allowed, got: {err}"
                );
                // Refusals must go through the standard air-gap message.
                assert!(
                    format!("{err}").contains("air-gap"),
                    "expected air-gap rejection for {base_url}, got: {err}"
                );
            }
        }
    }
}

#[tokio::test]
async fn air_gap_skips_public_cloud_fallback_slot() {
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

    let client = LlmClient::new(&config)
        .await
        .expect("primary local must succeed");
    assert!(
        client.provider_slots.is_empty(),
        "PublicCloud fallback must be dropped under air-gap"
    );
}

#[tokio::test]
async fn openrouter_cost_event_uses_usage_cost_verbatim() {
    let sink = Arc::new(CollectingSink::default());
    let client = make_test_openrouter_client(true)
        .await
        .with_cost_event_sink(sink.clone(), cost_context());
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

#[tokio::test]
async fn openrouter_missing_cost_emits_unknown_cost() {
    let sink = Arc::new(CollectingSink::default());
    let client = make_test_openrouter_client(true)
        .await
        .with_cost_event_sink(sink.clone(), cost_context());
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
fn openai_sse_tool_calls_finish_reason_emits_tool_intent() {
    // A finish_reason="tool_calls" chunk must surface StreamEvent::ToolIntent
    // even when the delta carries no tool-call payload — some providers ship
    // the signal with an empty array, and the agent uses the event to nudge
    // instead of silently ending the turn on the model's narration.
    use crate::sse::SseAction;
    let line = r#"data: {"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#;
    let action = transport::parse_openai_sse_line(line, "openai", "gpt-4o");
    let events = match action {
        SseAction::Emit(ev) | SseAction::Done(ev) => ev,
        _ => panic!("expected tool-intent emit"),
    };
    assert!(
        events.iter().any(|e| matches!(e, StreamEvent::ToolIntent)),
        "finish_reason=tool_calls must emit ToolIntent"
    );

    // An ordinary stop must NOT signal tool intent.
    let line = r#"data: {"choices":[{"index":0,"delta":{"content":"hi"},"finish_reason":"stop"}]}"#;
    let action = transport::parse_openai_sse_line(line, "openai", "gpt-4o");
    if let SseAction::Emit(ev) | SseAction::Done(ev) = action {
        assert!(
            !ev.iter().any(|e| matches!(e, StreamEvent::ToolIntent)),
            "finish_reason=stop must not emit ToolIntent"
        );
    }
}

#[test]
fn anthropic_sse_tool_use_stop_reason_emits_tool_intent() {
    use crate::sse::SseAction;
    let mut tool_idx = 0usize;
    let mut is_tool = false;
    let mut is_thinking = false;
    let mut pending_input = 0u64;
    let mut pending_cached = 0u64;
    let mut pending_created = 0u64;
    let line = r#"data: {"type":"message_delta","usage":{"output_tokens":5},"delta":{"stop_reason":"tool_use"}}"#;
    let action = transport::parse_anthropic_sse_line(
        line,
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
        SseAction::Emit(ev) | SseAction::Done(ev) => ev,
        _ => panic!("expected terminal emit"),
    };
    assert!(
        events.iter().any(|e| matches!(e, StreamEvent::ToolIntent)),
        "stop_reason=tool_use must emit ToolIntent"
    );
    assert!(
        events.iter().any(|e| matches!(e, StreamEvent::Done)),
        "stop_reason chunk stays terminal"
    );
}

#[test]
fn openrouter_sse_usage_total_cost_populates_provider_cost_usd() {
    // Regression: OpenRouter's streaming usage chunk reports the dollar
    // total under `total_cost` (not `cost`). The SSE parser must accept
    // it so the cost-event emitter ends up with cost_source =
    // "openrouter_usage" rather than falling through to "unknown".
    use crate::sse::SseAction;
    let line = r#"data: {"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":20,"total_tokens":30,"total_cost":0.000123}}"#;
    let action = transport::parse_openai_sse_line(line, "openrouter", "openai/gpt-4o-mini");
    let events = match action {
        SseAction::Emit(ev) | SseAction::Done(ev) => ev,
        _ => panic!("expected usage emit"),
    };
    let usage = events
        .iter()
        .find_map(|e| match e {
            StreamEvent::Usage(u) => Some(u.clone()),
            _ => None,
        })
        .expect("usage event must be emitted");
    assert_eq!(usage.provider_cost_usd.as_deref(), Some("0.000123"));
}

#[test]
fn openrouter_sse_usage_without_cost_field_leaves_provider_cost_none() {
    // Negative case: when no cost field is present at all, the parser
    // still emits a Usage event but with provider_cost_usd = None, which
    // the emitter then attributes as cost_source = "unknown".
    use crate::sse::SseAction;
    let line = r#"data: {"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":20,"total_tokens":30}}"#;
    let action = transport::parse_openai_sse_line(line, "openrouter", "openai/gpt-4o-mini");
    let events = match action {
        SseAction::Emit(ev) | SseAction::Done(ev) => ev,
        _ => panic!("expected usage emit"),
    };
    let usage = events
        .iter()
        .find_map(|e| match e {
            StreamEvent::Usage(u) => Some(u.clone()),
            _ => None,
        })
        .expect("usage event must be emitted");
    assert_eq!(usage.provider_cost_usd, None);
}

/// Helper: extract the parsed `UsageData` from an SSE line.
fn parse_openrouter_usage_from_sse(line: &str) -> UsageData {
    use crate::sse::SseAction;
    let action = transport::parse_openai_sse_line(line, "openrouter", "openai/gpt-4o-mini");
    let events = match action {
        SseAction::Emit(ev) | SseAction::Done(ev) => ev,
        _ => panic!("expected usage emit, got non-emit action"),
    };
    events
        .into_iter()
        .find_map(|e| match e {
            StreamEvent::Usage(u) => Some(u),
            _ => None,
        })
        .expect("usage event must be emitted")
}

#[test]
fn openrouter_sse_usage_cost_field_populates_provider_cost_usd() {
    // Canonical OpenRouter SSE shape: `usage.cost` as a number. Previously
    // covered only via the `total_cost` alias path; this locks in the
    // documented field name.
    let line = r#"data: {"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":20,"total_tokens":30,"cost":0.000123}}"#;
    let usage = parse_openrouter_usage_from_sse(line);
    assert_eq!(usage.provider_cost_usd.as_deref(), Some("0.000123"));
}

#[test]
fn openrouter_sse_usage_cost_details_fallback_populates_provider_cost_usd() {
    // When top-level `cost` is absent, OpenRouter's `cost_details`
    // breakdown is authoritative. Without this fallback the eval harness
    // would mis-report `cost_source = "unknown"`.
    let line = r#"data: {"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":20,"total_tokens":30,"cost_details":{"upstream_inference_cost":0.000456}}}"#;
    let usage = parse_openrouter_usage_from_sse(line);
    assert_eq!(usage.provider_cost_usd.as_deref(), Some("0.000456"));
}

#[test]
fn openrouter_sse_usage_cost_details_total_cost_fallback() {
    // `cost_details.total_cost` takes precedence over
    // `upstream_inference_cost` when both are present.
    let line = r#"data: {"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":20,"total_tokens":30,"cost_details":{"total_cost":0.000789,"upstream_inference_cost":0.000456}}}"#;
    let usage = parse_openrouter_usage_from_sse(line);
    assert_eq!(usage.provider_cost_usd.as_deref(), Some("0.000789"));
}

#[test]
fn openrouter_sse_usage_cost_as_string_populates_provider_cost_usd() {
    // Some OpenRouter shapes serialize cost as a string. The parser must
    // accept both number and non-empty string, otherwise we silently lose
    // attribution on string-typed responses.
    let line = r#"data: {"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":20,"total_tokens":30,"cost":"0.000123"}}"#;
    let usage = parse_openrouter_usage_from_sse(line);
    assert_eq!(usage.provider_cost_usd.as_deref(), Some("0.000123"));
}

#[tokio::test]
async fn openrouter_sse_usage_cost_null_emits_unknown_cost_source() {
    // Negative: explicit `cost: null` with no `cost_details` must
    // resolve to `provider_cost_usd = None` AND propagate to
    // `cost_source = "unknown"`. Pins the parser→emitter contract that
    // the eval suite asserts on.
    let line = r#"data: {"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":20,"total_tokens":30,"cost":null}}"#;
    let usage = parse_openrouter_usage_from_sse(line);
    assert_eq!(usage.provider_cost_usd, None);

    let sink = Arc::new(CollectingSink::default());
    let client = make_test_openrouter_client(true)
        .await
        .with_cost_event_sink(sink.clone(), cost_context());
    client.emit_cost_event_best_effort("req-null", Some(&usage), "openai/gpt-4o-mini");
    let events = sink.take();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].cost_source, "unknown");
    assert!(!events[0].cost_usd_present);
}

#[tokio::test]
async fn openrouter_sse_usage_cost_zero_emits_openrouter_usage_not_unknown() {
    // Regression guard: a reported cost of exactly 0 (e.g. free-tier
    // model) is still authoritative provider attribution. Must NOT
    // collapse to `cost_source = "unknown"`.
    let line = r#"data: {"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":20,"total_tokens":30,"cost":0}}"#;
    let usage = parse_openrouter_usage_from_sse(line);
    assert_eq!(usage.provider_cost_usd.as_deref(), Some("0"));

    let sink = Arc::new(CollectingSink::default());
    let client = make_test_openrouter_client(true)
        .await
        .with_cost_event_sink(sink.clone(), cost_context());
    client.emit_cost_event_best_effort("req-zero", Some(&usage), "openai/gpt-4o-mini");
    let events = sink.take();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].cost_source, "openrouter_usage");
    assert_eq!(events[0].cost_usd.as_deref(), Some("0"));
    assert!(events[0].cost_usd_present);
}

#[test]
fn openrouter_finish_reason_chunk_does_not_terminate_stream_before_usage() {
    // Real-world bug: OpenRouter splits termination across two SSE chunks.
    // First chunk has `finish_reason: "stop"` only. Second chunk has both
    // `finish_reason: "stop"` AND `usage:{cost:...}`. If the parser returns
    // `SseAction::Done` on the first finish_reason, `process_sse_stream`
    // exits and the second chunk's usage is never captured — leaving
    // `cost_source = "unknown"`. Both chunks must be `Emit`/`Continue`;
    // termination is driven solely by the `[DONE]` sentinel.
    use crate::sse::SseAction;

    let line_first_terminal = r#"data: {"choices":[{"index":0,"delta":{"content":"","role":"assistant"},"finish_reason":"stop"}]}"#;
    let action = transport::parse_openai_sse_line(
        line_first_terminal,
        "openrouter",
        "anthropic/claude-sonnet-4.5",
    );
    assert!(
        !matches!(action, SseAction::Done(_)),
        "first finish_reason chunk must NOT terminate the stream — would race past the usage chunk"
    );

    let line_second_terminal = r#"data: {"choices":[{"index":0,"delta":{"content":"","role":"assistant"},"finish_reason":"stop"}],"usage":{"prompt_tokens":16,"completion_tokens":5,"total_tokens":21,"cost":0.000123}}"#;
    let usage = parse_openrouter_usage_from_sse(line_second_terminal);
    assert_eq!(
        usage.provider_cost_usd.as_deref(),
        Some("0.000123"),
        "second terminal chunk's usage must populate provider_cost_usd"
    );

    let done_line = "data: [DONE]";
    let done_action =
        transport::parse_openai_sse_line(done_line, "openrouter", "anthropic/claude-sonnet-4.5");
    assert!(
        matches!(done_action, SseAction::Done(_)),
        "[DONE] sentinel must be the sole termination trigger"
    );
}

#[test]
fn openrouter_non_streaming_usage_cost_resolves_via_extractor() {
    // Non-streaming the OpenAI-compat wire path deserializes the same `StreamUsage`
    // struct out of the JSON response body. Lock in that the canonical
    // `cost` field on a non-streaming response resolves to
    // `provider_cost_usd` exactly as the streaming path does — this is
    // the path `test_cost_event_emitted_for_chat_call` exercises.
    let body = r#"{"prompt_tokens":10,"completion_tokens":20,"total_tokens":30,"cost":0.000123}"#;
    let usage: StreamUsage = serde_json::from_str(body).expect("usage must deserialize");
    let resolved = LlmClient::extract_provider_cost_usd(&usage);
    assert_eq!(resolved.as_deref(), Some("0.000123"));
}

#[tokio::test]
async fn openai_cost_event_uses_static_price_table() {
    let sink = Arc::new(CollectingSink::default());
    let client = make_test_openai_client()
        .await
        .with_cost_event_sink(sink.clone(), cost_context());
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

#[tokio::test]
async fn anthropic_cost_event_uses_static_price_table() {
    let sink = Arc::new(CollectingSink::default());
    let client = make_test_anthropic_client()
        .await
        .with_cost_event_sink(sink.clone(), cost_context());
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

#[tokio::test]
async fn anthropic_cost_event_prices_prompt_cache_reads_and_writes() {
    let sink = Arc::new(CollectingSink::default());
    let client = make_test_anthropic_client()
        .await
        .with_cost_event_sink(sink.clone(), cost_context());
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

#[tokio::test]
async fn unknown_openai_model_emits_unknown_cost() {
    let sink = Arc::new(CollectingSink::default());
    let client = make_test_openai_client()
        .await
        .with_cost_event_sink(sink.clone(), cost_context());
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

#[tokio::test]
async fn openrouter_cost_opt_in_is_disabled_when_feature_is_off() {
    let client = make_test_openrouter_client(false).await;
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
        session_id: None,
        stream_options: Some(StreamOptions {
            include_usage: true,
        }),
        usage: (client.provider == Provider::OpenRouter && client.cost_events_enabled())
            .then_some(UsageOptions { include: true }),
    };
    let json = serde_json::to_value(&request).unwrap();
    assert!(json.get("usage").is_none());
}

#[tokio::test]
async fn sink_panic_is_caught_and_does_not_propagate() {
    let client = make_test_openai_client()
        .await
        .with_cost_event_sink(Arc::new(PanicSink), cost_context());
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

#[tokio::test]
async fn anthropic_system_extraction() {
    let messages = vec![Message::system("You are helpful."), Message::user("Hello")];

    let client = make_test_anthropic_client().await;
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
            seq: None,
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
fn anthropic_tool_result_carries_multimodal_image_blocks() {
    // Invariant: a tool result with image parts (e.g. computer_use screenshot,
    // vision_analyze native path) must reach Anthropic as content-block array
    // with text + image entries — not flattened to a single string.
    use crate::types::{ContentPart, MediaData};

    let parts = vec![
        ContentPart::Text("captured window".to_string()),
        ContentPart::ImageBase64 {
            media: MediaData {
                mime_type: "image/png".to_string(),
                data: "iVBORw0KGgo=".to_string(),
                filename: None,
            },
        },
    ];
    let messages = vec![
        Message::user("screenshot the active window"),
        Message {
            role: Role::Assistant,
            content: None,
            tool_calls: Some(vec![ToolCall {
                id: "call_cap".to_string(),
                call_type: "function".to_string(),
                function: FunctionCall {
                    name: "computer_use".to_string(),
                    arguments: r#"{"action":"capture"}"#.to_string(),
                },
            }]),
            tool_call_id: None,
            timestamp: None,
            seq: None,
        },
        Message::tool_result_multimodal("call_cap", parts),
    ];

    let anthropic_msgs = build_anthropic_messages(&messages);
    let tool_user = &anthropic_msgs[2];
    let tool_result = &tool_user["content"][0];
    assert_eq!(tool_result["type"], "tool_result");
    assert_eq!(tool_result["tool_use_id"], "call_cap");

    let inner = tool_result["content"]
        .as_array()
        .expect("multimodal tool_result.content must be a block array");
    assert_eq!(inner.len(), 2, "expected text + image blocks");
    assert_eq!(inner[0]["type"], "text");
    assert_eq!(inner[0]["text"], "captured window");
    assert_eq!(inner[1]["type"], "image");
    assert_eq!(inner[1]["source"]["type"], "base64");
    assert_eq!(inner[1]["source"]["media_type"], "image/png");
    assert_eq!(inner[1]["source"]["data"], "iVBORw0KGgo=");
}

#[tokio::test]
async fn anthropic_tool_definition_conversion() {
    let tools = vec![ToolDefinition::new(
        "test_tool",
        "A test tool",
        serde_json::json!({"type": "object", "properties": {"x": {"type": "string"}}}),
    )];

    let client = make_test_anthropic_client().await;

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
            seq: None,
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
    // Consecutive failures (without intervening success) must back off
    // longer each time — the whole point of the error_count counter.
    let mut cd = ProviderCooldown::default();
    cd.record_failure(FailoverReason::RateLimit);
    let first = cd.cooldown_until.unwrap();

    cd.record_failure(FailoverReason::RateLimit);
    let second = cd.cooldown_until.unwrap();

    cd.record_failure(FailoverReason::RateLimit);
    let third = cd.cooldown_until.unwrap();

    assert!(
        second > first,
        "second cooldown {second:?} did not escalate past first {first:?}"
    );
    assert!(
        third > second,
        "third cooldown {third:?} did not escalate past second {second:?}"
    );
    assert_eq!(cd.error_count, 3);
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

#[tokio::test]
async fn effective_base_url_uses_config_override() {
    let mut config = Config::default();
    config.llm.provider = Some("ollama".to_string());
    config.llm.model = "llama3.3".to_string();
    config.llm.base_url = Some("http://custom:8080/v1/chat/completions".to_string());
    let client = LlmClient::new(&config).await.expect("should create client");
    assert_eq!(
        client.effective_base_url().as_ref(),
        "http://custom:8080/v1/chat/completions"
    );
}

#[tokio::test]
async fn effective_base_url_falls_back_to_provider_default() {
    let mut config = Config::default();
    config.llm.provider = Some("ollama".to_string());
    config.llm.model = "llama3.3".to_string();
    let client = LlmClient::new(&config).await.expect("should create client");
    assert_eq!(
        client.effective_base_url().as_ref(),
        "http://localhost:11434/v1/chat/completions"
    );
}

#[tokio::test]
async fn custom_provider_constructs_keyless_with_base_url() {
    // The documented llama.cpp / LM Studio flow: custom provider + base_url,
    // no API key anywhere. Client construction must succeed and target the
    // configured endpoint.
    let mut config = Config::default();
    config.llm.provider = Some("custom".to_string());
    config.llm.model = "gemma-3-4b-it".to_string();
    config.llm.base_url = Some("http://localhost:8080/v1/chat/completions".to_string());
    let client = LlmClient::new(&config)
        .await
        .expect("keyless custom must construct");
    assert_eq!(
        client.effective_base_url().as_ref(),
        "http://localhost:8080/v1/chat/completions"
    );
}

/// A provider that returns 200 + headers and then stalls the body must not
/// hang `chat()` forever. Regression: the send_request timeout only wrapped
/// `send()` (headers), leaving `response.json()` unbounded.
#[tokio::test]
async fn chat_body_stall_times_out_instead_of_hanging() {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral");
    let port = listener.local_addr().expect("local addr").port();
    std::thread::spawn(move || {
        if let Ok((mut sock, _)) = listener.accept() {
            let mut buf = [0u8; 8192];
            let _ = sock.read(&mut buf);
            // Claim a large body, deliver one byte, then go quiet.
            let _ = sock.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 4096\r\n\r\n{",
            );
            let _ = sock.flush();
            std::thread::sleep(std::time::Duration::from_secs(20));
        }
    });

    let mut config = Config::default();
    config.llm.provider = Some("custom".to_string());
    config.llm.model = "test-model".to_string();
    config.llm.base_url = Some(format!("http://127.0.0.1:{port}/v1/chat/completions"));
    config.llm.request_timeout_ms = 400;
    config.llm.max_retries = 0;
    let client = LlmClient::new(&config).await.expect("client builds");

    let result = tokio::time::timeout(
        Duration::from_secs(10),
        client.chat(&[Message::user("hi")], None),
    )
    .await
    .expect("chat must fail within the body budget, not hang");
    let err = result.expect_err("stalled body must surface an error");
    assert!(
        format!("{err:#}").contains("stalled"),
        "error should name the body stall, got: {err:#}"
    );
}

/// `chat_streamed` runs the streaming wire path and aggregates the deltas
/// into one message — the non-streaming result compaction and title
/// generation consume, with the stream's inactivity-based timeouts.
#[tokio::test]
async fn chat_streamed_aggregates_sse_text_deltas() {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral");
    let port = listener.local_addr().expect("local addr").port();
    std::thread::spawn(move || {
        if let Ok((mut sock, _)) = listener.accept() {
            let mut buf = [0u8; 8192];
            let _ = sock.read(&mut buf);
            let sse = concat!(
                "data: {\"choices\":[{\"delta\":{\"content\":\"Hello \"}}]}\n\n",
                "data: {\"choices\":[{\"delta\":{\"content\":\"summary world\"}}]}\n\n",
                "data: [DONE]\n\n",
            );
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n{sse}"
            );
            let _ = sock.write_all(response.as_bytes());
            let _ = sock.flush();
        }
    });

    let mut config = Config::default();
    config.llm.provider = Some("custom".to_string());
    config.llm.model = "test-model".to_string();
    config.llm.base_url = Some(format!("http://127.0.0.1:{port}/v1/chat/completions"));
    config.llm.max_retries = 0;
    let mut client = LlmClient::new(&config).await.expect("client builds");

    let message = tokio::time::timeout(
        Duration::from_secs(10),
        client.chat_streamed(&[Message::user("summarize")]),
    )
    .await
    .expect("stream must complete promptly")
    .expect("aggregation must succeed");
    assert_eq!(message.text_content(), Some("Hello summary world"));
}

#[tokio::test]
async fn ollama_connection_refused_error_says_ollama_serve() {
    // Bind then drop a listener so the port is provably closed.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral");
    let port = listener.local_addr().expect("local addr").port();
    drop(listener);

    let mut config = Config::default();
    config.llm.provider = Some("ollama".to_string());
    config.llm.model = "llama3.3".to_string();
    config.llm.base_url = Some(format!("http://127.0.0.1:{port}/v1/chat/completions"));
    config.llm.max_retries = 0;
    let client = LlmClient::new(&config).await.expect("client builds");

    let err = client
        .chat(&[Message::user("hi")], None)
        .await
        .expect_err("no server listening");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("ollama serve"),
        "refused connection must tell the operator to start Ollama, got: {msg}"
    );
    assert!(
        msg.contains(&format!("127.0.0.1:{port}")),
        "error must name the endpoint it tried, got: {msg}"
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

#[tokio::test]
async fn effective_base_url_normalizes_gemini_override() {
    let mut config = Config::default();
    config.llm.provider = Some("gemini".to_string());
    config.llm.api_key_env = "GEMINI_API_KEY".to_string();
    config.llm.model = "gemini-2.5-flash".to_string();
    config.llm.base_url =
        Some("https://generativelanguage.googleapis.com/v1beta/openai".to_string());
    std::env::set_var("GEMINI_API_KEY", "test-key");
    let client = LlmClient::new(&config).await.expect("should create client");
    assert_eq!(
        client.effective_base_url().as_ref(),
        "https://generativelanguage.googleapis.com/v1beta/openai/chat/completions"
    );
    std::env::remove_var("GEMINI_API_KEY");
}

#[tokio::test]
async fn anthropic_request_thinking_off_includes_temperature() {
    let mut config = Config::default();
    config.llm.provider = Some("anthropic".to_string());
    config.llm.api_key_env = "ANTHROPIC_API_KEY".to_string();
    config.llm.model = "claude-sonnet-4-20250514".to_string();
    config.llm.thinking = crate::config::ThinkingLevel::Off;
    std::env::set_var("ANTHROPIC_API_KEY", "test-key");
    let client = LlmClient::new(&config).await.expect("should create client");

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

#[tokio::test]
async fn anthropic_request_thinking_enabled_omits_temperature() {
    let mut config = Config::default();
    config.llm.provider = Some("anthropic".to_string());
    config.llm.api_key_env = "ANTHROPIC_API_KEY".to_string();
    config.llm.model = "claude-sonnet-4-20250514".to_string();
    config.llm.thinking = crate::config::ThinkingLevel::High;
    std::env::set_var("ANTHROPIC_API_KEY", "test-key");
    let client = LlmClient::new(&config).await.expect("should create client");

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

#[tokio::test]
async fn anthropic_request_adaptive_emits_8192_budget() {
    // Adaptive on Anthropic is the moderate provider-best default. If anyone
    // bumps it to a different number, an operator running `/think adaptive`
    // suddenly pays a different bill — fail loudly instead.
    let mut config = Config::default();
    config.llm.provider = Some("anthropic".to_string());
    config.llm.api_key_env = "ANTHROPIC_API_KEY".to_string();
    config.llm.model = "claude-sonnet-4-20250514".to_string();
    config.llm.thinking = crate::config::ThinkingLevel::Adaptive;
    std::env::set_var("ANTHROPIC_API_KEY", "test-key");
    let client = LlmClient::new(&config).await.expect("should create client");

    let body = client.build_anthropic_request(&[Message::user("hi")], None, false);

    let thinking = body
        .get("thinking")
        .expect("thinking field should be present");
    assert_eq!(thinking["budget_tokens"], 8192, "Adaptive must map to 8192");
    let max_tokens = body["max_tokens"].as_u64().unwrap();
    assert!(
        max_tokens >= 8192 + 1024,
        "max_tokens must reserve at least budget + 1024 (got {max_tokens})"
    );
    std::env::remove_var("ANTHROPIC_API_KEY");
}

#[tokio::test]
async fn anthropic_request_max_emits_64000_budget() {
    // Max ceiling is load-bearing: it's what users get when they type
    // `/think max`. Drifting this number silently changes the cost of the
    // documented "go all-in" mode.
    let mut config = Config::default();
    config.llm.provider = Some("anthropic".to_string());
    config.llm.api_key_env = "ANTHROPIC_API_KEY".to_string();
    config.llm.model = "claude-sonnet-4-20250514".to_string();
    config.llm.thinking = crate::config::ThinkingLevel::Max;
    std::env::set_var("ANTHROPIC_API_KEY", "test-key");
    let client = LlmClient::new(&config).await.expect("should create client");

    let body = client.build_anthropic_request(&[Message::user("hi")], None, false);

    assert_eq!(body["thinking"]["budget_tokens"], 64000);
    std::env::remove_var("ANTHROPIC_API_KEY");
}

#[tokio::test]
async fn anthropic_request_per_model_override_disables_thinking() {
    // Operator pattern: global thinking = High, but an override pins one
    // model to Off (e.g. for a fast lane). The override MUST win, otherwise
    // per-model defaults are decorative.
    let mut config = Config::default();
    config.llm.provider = Some("anthropic".to_string());
    config.llm.api_key_env = "ANTHROPIC_API_KEY".to_string();
    config.llm.model = "claude-haiku-4-5".to_string();
    config.llm.thinking = crate::config::ThinkingLevel::High;
    config
        .llm
        .thinking_overrides
        .insert("haiku".to_string(), crate::config::ThinkingLevel::Off);
    std::env::set_var("ANTHROPIC_API_KEY", "test-key");
    let client = LlmClient::new(&config).await.expect("should create client");

    let body = client.build_anthropic_request(&[Message::user("hi")], None, false);

    assert!(
        body.get("thinking").is_none(),
        "per-model Off override must suppress the global High setting"
    );
    assert!(
        body.get("temperature").is_some(),
        "with thinking suppressed, temperature must return to the body"
    );
    std::env::remove_var("ANTHROPIC_API_KEY");
}

#[test]
fn openai_build_request_minimal_emits_minimal_effort() {
    // GPT-5's "minimal" reasoning_effort tier exists for a reason — cost
    // and latency. If we alias `/think minimal` to "low", we silently
    // forfeit that win. Builds through the real transport function instead
    // of constructing ChatRequest by hand to exercise the wired path.
    use crate::llm::provider::Provider;
    use crate::llm::transport::TransportRequest;

    let mut cfg = LlmConfig::default();
    cfg.temperature = 0.7;
    cfg.max_tokens = 1024;
    cfg.thinking = crate::config::ThinkingLevel::Minimal;
    let req = TransportRequest {
        provider: Provider::OpenAi,
        model: "gpt-5",
        messages: &[Message::user("hi")],
        tools: None,
        llm_config: &cfg,
        prompt_cache_key: None,
        system_stable_prefix: None,
        cost_events_enabled: false,
    };

    let body = crate::llm::transport::build_openai_request(&req, false).expect("should build");
    assert_eq!(body["reasoning_effort"], "minimal");
}

#[test]
fn openai_build_request_per_model_override_applied() {
    // Same wiring proof for the override map: global Off but override
    // pins gpt-5 to High -> body must carry reasoning_effort=high.
    use crate::llm::provider::Provider;
    use crate::llm::transport::TransportRequest;

    let mut cfg = LlmConfig::default();
    cfg.temperature = 0.7;
    cfg.max_tokens = 1024;
    cfg.thinking = crate::config::ThinkingLevel::Off;
    cfg.thinking_overrides
        .insert("gpt-5".to_string(), crate::config::ThinkingLevel::High);
    let req = TransportRequest {
        provider: Provider::OpenAi,
        model: "gpt-5-pro",
        messages: &[Message::user("hi")],
        tools: None,
        llm_config: &cfg,
        prompt_cache_key: None,
        system_stable_prefix: None,
        cost_events_enabled: false,
    };

    let body = crate::llm::transport::build_openai_request(&req, false).expect("should build");
    assert_eq!(body["reasoning_effort"], "high");
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
        session_id: None,
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
        session_id: None,
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
        session_id: None,
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
        session_id: None,
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
        session_id: None,
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
        session_id: None,
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
    let action = transport::parse_anthropic_sse_line(
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
    let action = transport::parse_anthropic_sse_line(
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
    let _ = transport::parse_anthropic_sse_line(
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
    let action = transport::parse_anthropic_sse_line(
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
    // on the OpenAI-compat streaming path should replace None content on assistant
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
            seq: None,
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
        session_id: None,
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
            seq: None,
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
            seq: None,
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
            seq: None,
        },
        Message::user("follow up"),
    ];

    let sanitized = sanitize_openai_messages(&messages);
    assert_eq!(sanitized.len(), 2);
}

// ── repair_message_sequence (strict-provider history repair) ──

/// Assistant message that requests a single tool call `id` with no text.
fn repair_asst_tool_call(id: &str) -> Message {
    Message {
        role: Role::Assistant,
        content: None,
        tool_calls: Some(vec![ToolCall {
            id: id.to_string(),
            call_type: "function".to_string(),
            function: FunctionCall {
                name: "noop".to_string(),
                arguments: "{}".to_string(),
            },
        }]),
        tool_call_id: None,
        timestamp: None,
        seq: None,
    }
}

/// All tool-call ids across the assistant messages, in order.
fn repair_all_call_ids(msgs: &[Message]) -> Vec<String> {
    msgs.iter()
        .filter_map(|m| m.tool_calls.as_ref())
        .flatten()
        .map(|c| c.id.clone())
        .collect()
}

#[test]
fn repair_dedups_duplicate_tool_call_ids() {
    // A resumed history re-appended the same call+result. DeepSeek 400s on the
    // duplicate id; repair must keep exactly one call and one result.
    let messages = vec![
        Message::user("a"),
        repair_asst_tool_call("call_1"),
        Message::tool_result("call_1", "ok"),
        repair_asst_tool_call("call_1"),      // duplicate id
        Message::tool_result("call_1", "ok"), // duplicate result
    ];
    let repaired = repair_message_sequence(&messages);
    assert_eq!(
        repair_all_call_ids(&repaired),
        vec!["call_1".to_string()],
        "exactly one tool_call id must survive"
    );
    let result_count = repaired
        .iter()
        .filter(|m| m.role == Role::Tool && m.tool_call_id.as_deref() == Some("call_1"))
        .count();
    assert_eq!(result_count, 1, "exactly one tool result must survive");
}

#[test]
fn repair_drops_orphaned_tool_result() {
    // A tool result whose requesting assistant turn was compacted away — no
    // preceding call with that id — must be dropped (otherwise a 400).
    let messages = vec![
        Message::user("a"),
        Message::tool_result("call_ghost", "stale"),
        Message::assistant("answer"),
    ];
    let repaired = repair_message_sequence(&messages);
    assert!(
        !repaired.iter().any(|m| m.role == Role::Tool),
        "orphaned tool result must be gone"
    );
    assert_eq!(repaired.len(), 2);
}

#[test]
fn repair_coalesces_consecutive_assistant_text_turns() {
    // Two adjacent assistant turns (interrupted turn re-appended) 400 on strict
    // providers; repair joins them into one.
    let messages = vec![
        Message::user("a"),
        Message::assistant("part one"),
        Message::assistant("part two"),
        Message::user("b"),
    ];
    let repaired = repair_message_sequence(&messages);
    assert_eq!(repaired.len(), 3, "the two assistants must merge into one");
    assert_eq!(repaired[1].role, Role::Assistant);
    assert_eq!(
        repaired[1].content.as_ref().unwrap().text().unwrap(),
        "part one\npart two"
    );
}

#[test]
fn repair_unions_tool_calls_when_merging_assistants() {
    // Adjacent assistant tool-call turns merge, unioning their calls; both
    // results remain valid afterward.
    let messages = vec![
        repair_asst_tool_call("call_1"),
        repair_asst_tool_call("call_2"),
        Message::tool_result("call_1", "r1"),
        Message::tool_result("call_2", "r2"),
    ];
    let repaired = repair_message_sequence(&messages);
    assert_eq!(repair_all_call_ids(&repaired), vec!["call_1", "call_2"]);
    // One merged assistant + two tool results.
    assert_eq!(repaired.len(), 3);
    assert_eq!(repaired[0].tool_calls.as_ref().unwrap().len(), 2);
}

#[test]
fn repair_leaves_well_formed_history_unchanged() {
    // The common case: a valid live-loop history must pass through untouched so
    // repair never corrupts a healthy conversation.
    let messages = vec![
        Message::user("a"),
        repair_asst_tool_call("call_1"),
        Message::tool_result("call_1", "ok"),
        Message::assistant("final"),
    ];
    let repaired = repair_message_sequence(&messages);
    assert_eq!(repaired.len(), 4);
    assert_eq!(repair_all_call_ids(&repaired), vec!["call_1"]);
    assert_eq!(repaired[0].role, Role::User);
    assert_eq!(
        repaired[3].content.as_ref().unwrap().text().unwrap(),
        "final"
    );
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
            seq: None,
        },
        Message {
            role: Role::Tool,
            content: Some(crate::types::MessageContent::Text(String::new())),
            tool_calls: None,
            tool_call_id: Some("call_1".to_string()),
            timestamp: None,
            seq: None,
        },
    ];

    let sanitized = sanitize_openai_messages(&messages);
    // All 3 messages should be preserved (tool result has tool_call_id)
    assert_eq!(sanitized.len(), 3);
}

// ── Prompt caching (Anthropic) ──

#[allow(clippy::expect_used, clippy::unwrap_used)]
#[tokio::test]
async fn anthropic_request_includes_cache_control_on_system() {
    let client = make_test_anthropic_client().await;
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
#[tokio::test]
async fn anthropic_request_caches_last_two_messages() {
    let client = make_test_anthropic_client().await;
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
#[tokio::test]
async fn anthropic_request_cache_disabled_emits_no_markers() {
    let env_var = "BORG_TEST_ANTHROPIC_CACHE_OFF";
    std::env::set_var(env_var, "sk-test");
    let mut config = Config::default();
    config.llm.provider = Some("anthropic".to_string());
    config.llm.api_key_env = env_var.to_string();
    config.llm.model = "claude-sonnet-4".to_string();
    config.llm.cache.enabled = false;
    let client = LlmClient::new(&config).await.unwrap();

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
    apply_message_cache_control(&mut msgs, 2, &marker, false);
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
    apply_message_cache_control(&mut msgs, 2, &marker, false);
    for msg in &msgs {
        let blocks = msg["content"]
            .as_array()
            .expect("string content should have been upgraded to a block array");
        assert_eq!(blocks[0]["type"], "text");
        assert_eq!(blocks[0]["cache_control"]["type"], "ephemeral");
    }
}

#[allow(clippy::expect_used, clippy::unwrap_used)]
#[tokio::test]
async fn anthropic_request_marks_last_tool_with_cache_control() {
    let client = make_test_anthropic_client().await;
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
#[tokio::test]
async fn anthropic_request_cache_disabled_leaves_tools_unmarked() {
    let env_var = "BORG_TEST_ANTHROPIC_TOOLS_NOCACHE";
    std::env::set_var(env_var, "sk-test");
    let mut config = Config::default();
    config.llm.provider = Some("anthropic".to_string());
    config.llm.api_key_env = env_var.to_string();
    config.llm.model = "claude-sonnet-4".to_string();
    config.llm.cache.enabled = false;
    let client = LlmClient::new(&config).await.unwrap();

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
    apply_message_cache_control(&mut msgs, 1, &marker, false);
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
    apply_message_cache_control(&mut msgs, 1, &marker, false);
    let blocks = msgs[0]["content"].as_array().unwrap();
    assert_eq!(blocks[0]["cache_control"]["type"], "ephemeral");
    assert!(blocks[1].get("cache_control").is_none());
}

#[test]
fn apply_message_cache_control_empty_is_noop() {
    let mut msgs: Vec<serde_json::Value> = Vec::new();
    let marker = build_cache_control_marker(crate::config::CacheTtl::FiveMin);
    apply_message_cache_control(&mut msgs, 2, &marker, false);
    assert!(msgs.is_empty());
}

#[test]
fn apply_message_cache_control_zero_rolling_is_noop() {
    let mut msgs = vec![serde_json::json!({
        "role": "user",
        "content": [{ "type": "text", "text": "hi" }],
    })];
    let marker = build_cache_control_marker(crate::config::CacheTtl::FiveMin);
    apply_message_cache_control(&mut msgs, 0, &marker, false);
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
#[tokio::test]
async fn anthropic_request_honors_one_hour_ttl() {
    let env_var = "BORG_TEST_ANTHROPIC_TTL_1H";
    std::env::set_var(env_var, "sk-test");
    let mut config = Config::default();
    config.llm.provider = Some("anthropic".to_string());
    config.llm.api_key_env = env_var.to_string();
    config.llm.model = "claude-sonnet-4".to_string();
    config.llm.cache.ttl = crate::config::CacheTtl::OneHour;
    let client = LlmClient::new(&config).await.unwrap();

    let tools = vec![make_tool("a")];
    let messages = vec![Message::system("sys"), Message::user("hi")];
    let body = client.build_anthropic_request(&messages, Some(&tools), false);
    assert_eq!(body["system"][0]["cache_control"]["ttl"], "1h");
    assert_eq!(body["tools"][0]["cache_control"]["ttl"], "1h");
    std::env::remove_var(env_var);
}

// ── Cache strategy: system_and_3 ──

#[allow(clippy::unwrap_used)]
async fn make_system_and_3_client(env_var: &'static str) -> (LlmClient, &'static str) {
    // Each caller passes a unique env var name so parallel tests don't race
    // on set_var / remove_var of a shared key (was causing flaky failures
    // where one test removed the var while another was still reading it).
    std::env::set_var(env_var, "sk-test");
    let mut config = Config::default();
    config.llm.provider = Some("anthropic".to_string());
    config.llm.api_key_env = env_var.to_string();
    config.llm.model = "claude-sonnet-4".to_string();
    config.llm.cache.strategy = crate::config::CacheStrategy::SystemAnd3;
    (LlmClient::new(&config).await.unwrap(), env_var)
}

#[allow(clippy::unwrap_used)]
#[tokio::test]
async fn anthropic_request_system_and_3_no_tools_marker() {
    let (client, env_var) = make_system_and_3_client("BORG_TEST_ANTHROPIC_SYSTEM_AND_3_A").await;
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
#[tokio::test]
async fn anthropic_request_system_and_3_marks_last_three_messages() {
    let (client, env_var) = make_system_and_3_client("BORG_TEST_ANTHROPIC_SYSTEM_AND_3_B").await;
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
#[tokio::test]
async fn anthropic_request_system_and_3_fewer_than_three_messages() {
    // With only 1 user message the helper must not crash; fewer breakpoints is fine.
    let (client, env_var) = make_system_and_3_client("BORG_TEST_ANTHROPIC_SYSTEM_AND_3_C").await;
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
fn normalize_cache_tokens_deepseek_native_cache_hit_field() {
    // The direct DeepSeek API reports cache hits as top-level
    // `prompt_cache_hit_tokens`, with no `cached_tokens` anywhere.
    let usage = serde_json::json!({
        "prompt_tokens": 1000,
        "completion_tokens": 120,
        "prompt_cache_hit_tokens": 640,
        "prompt_cache_miss_tokens": 360,
    });
    let (read, created) = normalize_cache_tokens(&usage);
    assert_eq!(read, 640);
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

#[tokio::test]
async fn with_prompt_cache_key_sets_field() {
    let client = make_test_anthropic_client()
        .await
        .with_prompt_cache_key("sess-xyz");
    assert_eq!(client.prompt_cache_key.as_deref(), Some("sess-xyz"));
}

/// OpenRouter uses a request-body `session_id` as its sticky routing key —
/// without it each call (especially aux calls) can route to a different
/// upstream provider than the conversation it serves.
#[tokio::test]
async fn openrouter_body_carries_session_id_sticky_key() {
    let client = make_test_openrouter_client(false)
        .await
        .with_prompt_cache_key("sess-root-1");
    let messages = vec![Message::user("hi")];
    let model = client.provider.normalize_model(&client.llm_config.model);
    let request = client.transport_request(&messages, None, &model);
    let body = crate::llm::transport::build_openai_request(&request, false).unwrap();
    assert_eq!(body["session_id"], "sess-root-1");
}

/// The `session_id` field is OpenRouter-specific — other OpenAI-compatible
/// hosts must not receive an unknown field they could reject.
#[tokio::test]
async fn non_openrouter_body_omits_session_id() {
    let client = make_test_openai_client()
        .await
        .with_prompt_cache_key("sess-root-1");
    let messages = vec![Message::user("hi")];
    let model = client.provider.normalize_model(&client.llm_config.model);
    let request = client.transport_request(&messages, None, &model);
    let body = crate::llm::transport::build_openai_request(&request, false).unwrap();
    assert!(body.get("session_id").is_none());
    assert_eq!(body["prompt_cache_key"], "sess-root-1");
}

/// xAI cache affinity rides in the `x-grok-conv-id` header, gated to grok
/// model slugs; absent without a cache key, and never sent for other models.
#[tokio::test]
async fn xai_grok_header_gated_on_model_and_key() {
    let env_var = "BORG_TEST_XAI_LLM";
    std::env::set_var(env_var, "sk-test");
    let mut config = Config::default();
    config.llm.provider = Some("xai".to_string());
    config.llm.api_key_env = env_var.to_string();
    config.llm.model = "grok-4".to_string();

    let client = LlmClient::new(&config)
        .await
        .unwrap()
        .with_prompt_cache_key("conv-9");
    let headers = client.request_headers().await.unwrap();
    assert_eq!(
        headers.get("x-grok-conv-id").map(|v| v.to_str().unwrap()),
        Some("conv-9")
    );

    // No cache key → no header.
    let bare = LlmClient::new(&config).await.unwrap();
    let headers = bare.request_headers().await.unwrap();
    assert!(headers.get("x-grok-conv-id").is_none());

    // Non-grok slug on the xAI endpoint → no header.
    config.llm.model = "other-model".to_string();
    let other = LlmClient::new(&config)
        .await
        .unwrap()
        .with_prompt_cache_key("conv-9");
    let headers = other.request_headers().await.unwrap();
    assert!(headers.get("x-grok-conv-id").is_none());
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

// ── OpenRouter prompt-caching tests ──

#[test]
fn supports_anthropic_cache_control_table() {
    // Native Anthropic always wins; OpenRouter only on Claude-family
    // models; everything else is off. Locks the (provider, model)
    // gating used by the streaming and non-streaming OpenAI-compat paths.
    let cases: &[(Provider, &str, bool)] = &[
        (Provider::Anthropic, "claude-sonnet-4", true),
        (Provider::Anthropic, "claude-opus-4-1", true),
        (Provider::OpenRouter, "anthropic/claude-sonnet-4.6", true),
        (Provider::OpenRouter, "anthropic/claude-opus-4.7", true),
        // Bare claude/* slug (some routers normalize the prefix away)
        (Provider::OpenRouter, "claude-3-5-sonnet", true),
        // Non-Anthropic OpenRouter models: explicit deny
        (Provider::OpenRouter, "openai/gpt-4o-mini", false),
        (Provider::OpenRouter, "google/gemini-2.5-pro", false),
        (Provider::OpenRouter, "deepseek/deepseek-r1", false),
        // Other OpenAI-compatible providers: implicit cache only
        (Provider::OpenAi, "gpt-4.1", false),
        (Provider::Gemini, "gemini-2.5-flash", false),
        (Provider::DeepSeek, "deepseek-chat", false),
        (Provider::Groq, "llama-3.1-70b", false),
        (Provider::Ollama, "llama3", false),
        (Provider::ClaudeCli, "claude-sonnet-4", false),
    ];
    for (provider, model, want) in cases {
        assert_eq!(
            super::supports_anthropic_cache_control(*provider, model),
            *want,
            "supports_anthropic_cache_control({provider:?}, {model:?})"
        );
    }
}

/// Build a synthetic OpenAI-compat request body for cache-marker tests.
/// Mirrors what the OpenAI-compat streaming path would produce: a system
/// message, several user/assistant turns, and (optionally) a `role: tool`
/// message in the rolling window.
fn build_test_openai_body(include_tool_role: bool) -> serde_json::Value {
    let mut messages = vec![
        serde_json::json!({"role": "system", "content": "You are helpful."}),
        serde_json::json!({"role": "user", "content": "first question"}),
        serde_json::json!({
            "role": "assistant",
            "content": "first answer",
            "tool_calls": [{
                "id": "call_1",
                "type": "function",
                "function": {"name": "noop", "arguments": "{}"}
            }],
        }),
    ];
    if include_tool_role {
        messages.push(serde_json::json!({
            "role": "tool",
            "tool_call_id": "call_1",
            "content": "tool result",
        }));
    }
    messages.push(serde_json::json!({"role": "user", "content": "follow up"}));
    serde_json::json!({
        "model": "anthropic/claude-sonnet-4.6",
        "messages": messages,
    })
}

fn message_has_cache_control(msg: &serde_json::Value) -> bool {
    let Some(content) = msg.get("content") else {
        return false;
    };
    let Some(blocks) = content.as_array() else {
        return false;
    };
    blocks.iter().any(|b| {
        b.get("cache_control")
            .and_then(|c| c.get("type"))
            .and_then(|t| t.as_str())
            == Some("ephemeral")
    })
}

#[test]
fn openrouter_claude_request_carries_cache_control_markers() {
    // System prompt + last 2 messages should each carry an ephemeral
    // cache_control marker under the default ToolsSystemAnd2 strategy
    // (cache_tools is silently dropped on the OpenAI-compat path because
    // OpenRouter's tools envelope can't carry the marker).
    let mut body = build_test_openai_body(false);
    let cfg = crate::config::PromptCacheConfig::default();
    super::apply_openai_cache_markers(&mut body, &cfg, None);

    let messages = body["messages"].as_array().expect("messages array");
    let system = messages
        .iter()
        .find(|m| m["role"] == "system")
        .expect("system message present");
    assert!(
        message_has_cache_control(system),
        "system message must carry cache_control: {system}"
    );

    // Default rolling = 2 → last user message and the assistant message
    // before it (the only non-system, non-tool tail messages here) get
    // markers. The first user message at the head should not.
    let first_user = &messages[1];
    assert_eq!(first_user["role"], "user");
    assert!(
        !message_has_cache_control(first_user),
        "head user message must NOT be marked: {first_user}"
    );

    let tail = messages.last().unwrap();
    assert_eq!(tail["role"], "user");
    assert!(
        message_has_cache_control(tail),
        "trailing user message must be marked: {tail}"
    );

    let assistant = messages
        .iter()
        .find(|m| m["role"] == "assistant")
        .expect("assistant message present");
    assert!(
        message_has_cache_control(assistant),
        "assistant message in rolling window must be marked: {assistant}"
    );
}

#[test]
fn openrouter_system_marker_splits_at_stable_prefix() {
    // Same invariant as the native-Anthropic split: the system breakpoint
    // must sit at the end of the stable prefix, not the end of the whole
    // message — the dynamic suffix ends with a second-resolution clock, so
    // a whole-message marker is byte-different every turn and never hits.
    let mut body = build_test_openai_body(false);
    let system_text = body["messages"][0]["content"]
        .as_str()
        .expect("string system content")
        .to_string();
    let (prefix, tail) = system_text.split_at(system_text.len() / 2);
    let cfg = crate::config::PromptCacheConfig::default();
    super::apply_openai_cache_markers(&mut body, &cfg, Some(prefix));

    let blocks = body["messages"][0]["content"]
        .as_array()
        .expect("system content split into blocks");
    assert_eq!(blocks.len(), 2, "expected stable + volatile blocks");
    assert_eq!(blocks[0]["text"], prefix);
    assert_eq!(
        blocks[0]["cache_control"]["type"], "ephemeral",
        "marker must sit on the stable block"
    );
    assert_eq!(blocks[1]["text"], tail);
    assert!(
        blocks[1].get("cache_control").is_none(),
        "volatile tail must NOT be marked: {}",
        blocks[1]
    );
}

#[test]
fn openrouter_system_marker_falls_back_to_one_block_on_prefix_mismatch() {
    // A stale or non-matching prefix must lose the split, not correctness:
    // one block, marked, same as the pre-split behavior.
    let mut body = build_test_openai_body(false);
    let cfg = crate::config::PromptCacheConfig::default();
    super::apply_openai_cache_markers(&mut body, &cfg, Some("not a prefix of the system text"));

    let blocks = body["messages"][0]["content"]
        .as_array()
        .expect("system content upgraded to blocks");
    assert_eq!(blocks.len(), 1, "mismatch must not split");
    assert_eq!(blocks[0]["cache_control"]["type"], "ephemeral");
}

#[test]
fn openrouter_skips_cache_control_on_tool_role() {
    // Silent-hang guard: OpenRouter rejects `cache_control` on a role:tool
    // message by hanging the request indefinitely. The rolling window must
    // skip past tool messages without consuming a breakpoint.
    let mut body = build_test_openai_body(true);
    let cfg = crate::config::PromptCacheConfig::default();
    super::apply_openai_cache_markers(&mut body, &cfg, None);

    let messages = body["messages"].as_array().expect("messages array");
    let tool_msg = messages
        .iter()
        .find(|m| m["role"] == "tool")
        .expect("tool message present");
    assert!(
        !message_has_cache_control(tool_msg),
        "tool-role message must NEVER carry cache_control on OpenRouter: {tool_msg}"
    );

    // Skipping the tool message must not drop a breakpoint — the
    // assistant message before it should still be marked.
    let assistant = messages
        .iter()
        .find(|m| m["role"] == "assistant")
        .expect("assistant message present");
    assert!(
        message_has_cache_control(assistant),
        "rolling window must keep walking past role:tool: {assistant}"
    );
}

#[test]
fn openrouter_disabled_cache_skips_helper_entirely() {
    // The eligibility gate (`cache.enabled`) lives at the call site, but
    // even if a caller invokes the helper with rolling=0 / cache_system
    // off (the SystemAnd3 vs ToolsSystemAnd2 effective layout), no
    // markers should escape onto the wire. Lock that here so a future
    // refactor that conflates "disabled" with "off-by-default" is caught.
    let mut body = build_test_openai_body(false);
    let cfg = crate::config::PromptCacheConfig {
        enabled: false,
        cache_tools: false,
        cache_system: false,
        rolling_messages: 0,
        ..crate::config::PromptCacheConfig::default()
    };
    super::apply_openai_cache_markers(&mut body, &cfg, None);

    let messages = body["messages"].as_array().expect("messages array");
    for msg in messages {
        assert!(
            !message_has_cache_control(msg),
            "no message must be marked when cache layout is fully off: {msg}"
        );
    }
}

#[test]
fn openrouter_sse_populates_cache_creation_tokens() {
    // OpenRouter passes Anthropic's `cache_creation_input_tokens` field
    // through verbatim on Claude responses. Without surfacing it the
    // cost-event pipeline reports zero cache *write* tokens, so the
    // first turn after a marker change has invisible cache spend.
    let line = r#"data: {"choices":[],"usage":{"prompt_tokens":100,"completion_tokens":20,"total_tokens":120,"cache_creation_input_tokens":1234,"prompt_tokens_details":{"cached_tokens":56}}}"#;
    let usage = parse_openrouter_usage_from_sse(line);
    assert_eq!(usage.cache_creation_tokens, 1234);
    assert_eq!(usage.cached_input_tokens, 56);
    assert_eq!(usage.prompt_tokens, 100);
}

#[test]
fn openrouter_sse_without_cache_creation_field_reports_zero() {
    // Negative case: when the field is absent (e.g. non-Claude
    // OpenRouter models, or Claude responses with no cache write on this
    // turn) we must report zero, not None — the agent wires
    // `cache_creation_tokens: u64` directly.
    let line = r#"data: {"choices":[],"usage":{"prompt_tokens":50,"completion_tokens":20,"total_tokens":70}}"#;
    let usage = parse_openrouter_usage_from_sse(line);
    assert_eq!(usage.cache_creation_tokens, 0);
    assert_eq!(usage.cached_input_tokens, 0);
}

#[tokio::test]
async fn for_auxiliary_falls_through_to_primary_when_aux_unset() {
    // Back-compat invariant: pre-aux-feature configs (`llm.aux = None`)
    // must produce a working client identical to `LlmClient::new`. A
    // regression here breaks every existing deployment on first reload.
    let env_var = "BORG_TEST_AUX_FALLTHROUGH";
    std::env::set_var(env_var, "sk-test");
    let mut config = Config::default();
    config.llm.provider = Some("anthropic".to_string());
    config.llm.api_key_env = env_var.to_string();
    config.llm.model = "claude-sonnet-4".to_string();
    assert!(config.llm.aux.is_none(), "aux defaults to None");

    let client = LlmClient::for_auxiliary(&config)
        .await
        .expect("aux-unset must fall through to primary");
    assert_eq!(client.provider, Provider::Anthropic);
    assert_eq!(client.llm_config.model, "claude-sonnet-4");
}

#[tokio::test]
async fn for_auxiliary_resolves_auto_sentinel_via_picker() {
    // The onboarding-emitted `[llm.aux] model = "auto"` row gets
    // materialized into a concrete model id at client-construction time.
    // If the resolver dead-ends silently, every aux call would request a
    // model named literally "auto" and 404 forever — locking this is the
    // sentinel-to-concrete-model invariant.
    let env_var = "BORG_TEST_AUX_AUTO_RESOLVE";
    std::env::set_var(env_var, "sk-test");
    let mut config = Config::default();
    config.llm.provider = Some("anthropic".to_string());
    config.llm.api_key_env = env_var.to_string();
    config.llm.model = "claude-sonnet-4".to_string();
    config.llm.aux = Some(crate::config::llm::AuxLlmConfig {
        provider: Some("anthropic".to_string()),
        model: "auto".to_string(),
        api_key_env: env_var.to_string(),
        ..Default::default()
    });

    let client = LlmClient::for_auxiliary(&config)
        .await
        .expect("auto sentinel must resolve");
    assert_eq!(client.provider, Provider::Anthropic);
    assert_eq!(
        client.llm_config.model,
        super::aux_model_picker::pick_aux_model("anthropic").unwrap(),
        "auto must resolve to the picker's anthropic default"
    );
}

#[tokio::test]
async fn for_auxiliary_passes_concrete_model_through() {
    // Power-user override: setting `llm.aux.model` to a real model id
    // bypasses the resolver entirely. Locks the override invariant — if
    // we later add a "normalize aux model" pass, it must not stomp the
    // explicit choice.
    let env_var = "BORG_TEST_AUX_OVERRIDE";
    std::env::set_var(env_var, "sk-test");
    let mut config = Config::default();
    config.llm.provider = Some("anthropic".to_string());
    config.llm.api_key_env = env_var.to_string();
    config.llm.model = "claude-sonnet-4".to_string();
    config.llm.aux = Some(crate::config::llm::AuxLlmConfig {
        provider: Some("anthropic".to_string()),
        model: "claude-haiku-4-5".to_string(),
        api_key_env: env_var.to_string(),
        ..Default::default()
    });

    let client = LlmClient::for_auxiliary(&config)
        .await
        .expect("concrete model must pass through");
    assert_eq!(client.llm_config.model, "claude-haiku-4-5");
}

#[tokio::test]
async fn for_auxiliary_falls_through_for_local_provider() {
    // Local providers (Ollama, Claude CLI) have no aux-default entry in
    // the picker. A naive `unwrap_or("")` would route every aux call to
    // the empty model id and 400 immediately — instead, we silently fall
    // through to the primary chain. Soft-fail invariant.
    let mut config = Config::default();
    config.llm.provider = Some("ollama".to_string());
    config.llm.model = "llama3".to_string();
    config.llm.aux = Some(crate::config::llm::AuxLlmConfig {
        provider: Some("ollama".to_string()),
        model: "auto".to_string(),
        ..Default::default()
    });

    let client = LlmClient::for_auxiliary(&config)
        .await
        .expect("local-provider aux must fall through, not error");
    // Falls through to primary, which is also ollama.
    assert_eq!(client.provider, Provider::Ollama);
    assert_eq!(client.llm_config.model, "llama3");
}

#[tokio::test]
async fn for_auxiliary_synthesizes_separate_fallback_chain() {
    // Isolation invariant: the aux client's `provider_slots` must be a
    // distinct allocation that mirrors `aux.fallback`, NOT shared with
    // (or derived from) the primary's `llm.fallback`. Shared cooldown
    // state would mean a primary outage immediately disables aux too,
    // defeating the feature.
    //
    // We give primary and aux DIFFERENT non-empty fallback chains with
    // different model ids so any structural sharing surfaces in model-
    // id assertions — a length-only check would pass even if the slot
    // vec were aliased.
    let primary_env = "BORG_TEST_AUX_ISOLATION_P";
    let aux_env = "BORG_TEST_AUX_ISOLATION_A";
    std::env::set_var(primary_env, "sk-test");
    std::env::set_var(aux_env, "sk-test");
    let mut config = Config::default();
    config.llm.provider = Some("anthropic".to_string());
    config.llm.api_key_env = primary_env.to_string();
    config.llm.model = "claude-sonnet-4".to_string();
    // Primary: 1-slot fallback (sonnet → haiku-4-5).
    config.llm.fallback.push(crate::config::LlmFallback {
        provider: "anthropic".to_string(),
        model: "claude-haiku-4-5".to_string(),
        api_key: None,
        api_key_env: Some(primary_env.to_string()),
        api_keys: Vec::new(),
        temperature: None,
        max_tokens: None,
        base_url: None,
    });
    // Aux: 2-slot chain with DIFFERENT model ids than primary.
    config.llm.aux = Some(crate::config::llm::AuxLlmConfig {
        provider: Some("anthropic".to_string()),
        model: "claude-haiku-4-5".to_string(),
        api_key_env: aux_env.to_string(),
        fallback: vec![
            crate::config::LlmFallback {
                provider: "anthropic".to_string(),
                model: "claude-3-5-haiku".to_string(),
                api_key: None,
                api_key_env: Some(aux_env.to_string()),
                api_keys: Vec::new(),
                temperature: None,
                max_tokens: None,
                base_url: None,
            },
            crate::config::LlmFallback {
                provider: "anthropic".to_string(),
                model: "claude-sonnet-4".to_string(),
                api_key: None,
                api_key_env: Some(aux_env.to_string()),
                api_keys: Vec::new(),
                temperature: None,
                max_tokens: None,
                base_url: None,
            },
        ],
        ..Default::default()
    });

    let primary = LlmClient::new(&config).await.expect("primary must build");
    let aux = LlmClient::for_auxiliary(&config)
        .await
        .expect("aux must build");

    assert_eq!(primary.provider_slots.len(), 1);
    assert_eq!(aux.provider_slots.len(), 2);
    // Aux's chain reflects `aux.fallback`, not `llm.fallback`.
    assert_eq!(aux.provider_slots[0].model, "claude-3-5-haiku");
    assert_eq!(aux.provider_slots[1].model, "claude-sonnet-4");
    // Primary's chain is intact.
    assert_eq!(primary.provider_slots[0].model, "claude-haiku-4-5");
    // Re-entry guard: aux client's own `llm.aux` is cleared so a
    // synthetic config can't loop back through `for_auxiliary`.
    assert!(aux.llm_config.aux.is_none(), "aux must not nest aux");
}

#[tokio::test]
async fn for_auxiliary_forces_thinking_off() {
    // Aux work doesn't benefit from extended reasoning, and the cheap
    // sibling models in `aux_model_picker` either don't support it or
    // pay latency for nothing. Primary's `thinking="high"` must NOT
    // bleed into the synthetic aux config.
    let env_var = "BORG_TEST_AUX_THINKING";
    std::env::set_var(env_var, "sk-test");
    let mut config = Config::default();
    config.llm.provider = Some("anthropic".to_string());
    config.llm.api_key_env = env_var.to_string();
    config.llm.model = "claude-sonnet-4".to_string();
    config.llm.thinking = crate::config::llm::ThinkingLevel::High;
    config.llm.aux = Some(crate::config::llm::AuxLlmConfig {
        provider: Some("anthropic".to_string()),
        model: "claude-haiku-4-5".to_string(),
        api_key_env: env_var.to_string(),
        ..Default::default()
    });

    let aux = LlmClient::for_auxiliary(&config)
        .await
        .expect("aux must build");
    assert_eq!(
        aux.llm_config.thinking,
        crate::config::llm::ThinkingLevel::Off,
        "primary thinking level must not bleed into aux"
    );
}

#[tokio::test]
async fn for_auxiliary_clears_thinking_overrides_so_picker_choice_stays_off() {
    // Regression for the failure mode the for_auxiliary doc-comment
    // describes: an operator pins `"claude"` → High on primary, the aux
    // picker then chooses `claude-haiku-4-5`, and the substring override
    // re-enables thinking on the aux head — defeating the whole "aux is
    // cheap and fast" intent. The fix clears `thinking_overrides`; this
    // test pins that fix in place.
    let env_var = "BORG_TEST_AUX_OVERRIDE_CLEAR";
    std::env::set_var(env_var, "sk-test");
    let mut config = Config::default();
    config.llm.provider = Some("anthropic".to_string());
    config.llm.api_key_env = env_var.to_string();
    config.llm.model = "claude-sonnet-4".to_string();
    config.llm.thinking = crate::config::llm::ThinkingLevel::Off;
    config
        .llm
        .thinking_overrides
        .insert("claude".into(), crate::config::llm::ThinkingLevel::High);
    config.llm.aux = Some(crate::config::llm::AuxLlmConfig {
        provider: Some("anthropic".to_string()),
        model: "claude-haiku-4-5".to_string(),
        api_key_env: env_var.to_string(),
        ..Default::default()
    });

    let aux = LlmClient::for_auxiliary(&config)
        .await
        .expect("aux must build");
    assert!(
        aux.llm_config.thinking_overrides.is_empty(),
        "primary overrides must be wiped from aux"
    );
    assert_eq!(
        aux.llm_config.effective_thinking(&aux.llm_config.model),
        crate::config::llm::ThinkingLevel::Off,
        "even after picker chose 'claude-haiku-4-5', aux must stay Off"
    );
    std::env::remove_var(env_var);
}

// ── Anthropic SSE error-event classification ──
//
// Regression guard for the bug that motivated centralizing
// `FailoverReason`: the SSE "error" branch used to bucket every Anthropic
// stream error as `Overloaded`, so an in-stream auth failure burned the
// retry budget and never rotated credentials. The handler now consults
// `classify_anthropic_stream_error` and lifts the result to a `Fatal`
// vs. `Retryable` shape that matches the failover layer's contract.

fn parse_anthropic_error_sse(err_type: &str, err_msg: &str) -> crate::sse::SseAction {
    let line = format!(
        r#"data: {{"type":"error","error":{{"type":"{err_type}","message":"{err_msg}"}}}}"#
    );
    let mut tool_idx = 0usize;
    let mut is_tool = false;
    let mut is_thinking = false;
    let mut pending_input = 0u64;
    let mut pending_cached = 0u64;
    let mut pending_created = 0u64;
    transport::parse_anthropic_sse_line(
        &line,
        "anthropic",
        "claude-sonnet-4",
        &mut tool_idx,
        &mut is_tool,
        &mut is_thinking,
        &mut pending_input,
        &mut pending_cached,
        &mut pending_created,
    )
}

#[test]
fn anthropic_sse_overloaded_error_is_retryable_overloaded() {
    let action = parse_anthropic_error_sse("overloaded_error", "Overloaded");
    match action {
        crate::sse::SseAction::Error(LlmError::Retryable { reason, .. }) => {
            assert_eq!(reason, FailoverReason::Overloaded);
        }
        _ => panic!("expected Retryable Overloaded"),
    }
}

#[test]
fn anthropic_sse_rate_limit_error_is_retryable_rate_limit() {
    // RateLimit is Retryable (cool down and try again) but distinct from
    // Overloaded so the cooldown table can hold separate backoff bases.
    let action = parse_anthropic_error_sse("rate_limit_error", "too many requests");
    match action {
        crate::sse::SseAction::Error(LlmError::Retryable { reason, .. }) => {
            assert_eq!(reason, FailoverReason::RateLimit);
        }
        _ => panic!("expected Retryable RateLimit"),
    }
}

#[test]
fn anthropic_sse_authentication_error_is_fatal_auth() {
    // The whole point of the fix: a 401-class stream error must be Fatal
    // so the failover layer rotates the credential or skips the provider
    // instead of retrying max_retries times.
    let action = parse_anthropic_error_sse("authentication_error", "invalid x-api-key");
    match action {
        crate::sse::SseAction::Error(LlmError::Fatal { reason, .. }) => {
            assert_eq!(reason, FailoverReason::Auth);
        }
        _ => panic!("expected Fatal Auth"),
    }
}

#[test]
fn anthropic_sse_permission_error_is_fatal_auth_permanent() {
    // 403-class — credential rotation cannot fix this; the whole account
    // is denied. Must NOT trigger should_rotate_credential.
    let action = parse_anthropic_error_sse("permission_error", "organization disabled");
    match action {
        crate::sse::SseAction::Error(LlmError::Fatal { reason, source }) => {
            assert_eq!(reason, FailoverReason::AuthPermanent);
            let err = LlmError::Fatal { source, reason };
            assert!(
                !err.should_rotate_credential(),
                "AuthPermanent must not rotate credentials",
            );
        }
        _ => panic!("expected Fatal AuthPermanent"),
    }
}

#[test]
fn anthropic_sse_invalid_request_with_context_overflow_is_fatal_compress() {
    // invalid_request_error splits by message text: a prompt-length body
    // must drive reactive compaction (should_compress true) rather than a
    // generic Format fatal.
    let action = parse_anthropic_error_sse(
        "invalid_request_error",
        "prompt is too long: 205000 tokens > 200000 maximum",
    );
    match action {
        crate::sse::SseAction::Error(err) => {
            assert_eq!(err.reason(), FailoverReason::ContextOverflow);
            assert!(
                err.should_compress(),
                "context overflow must drive compaction"
            );
        }
        _ => panic!("expected Error variant"),
    }
}

#[test]
fn anthropic_sse_invalid_request_generic_is_fatal_format() {
    // The opposite half of the split — a malformed body that has nothing
    // to do with context length must NOT drive compaction (which would
    // silently drop history).
    let action = parse_anthropic_error_sse(
        "invalid_request_error",
        "messages: at least one message is required",
    );
    match action {
        crate::sse::SseAction::Error(err) => {
            assert_eq!(err.reason(), FailoverReason::Format);
            assert!(!err.should_compress());
        }
        _ => panic!("expected Error variant"),
    }
}

// ── Azure OpenAI URL composition ──

fn azure_test_cfg(
    resource: Option<&str>,
    deployment: Option<&str>,
    api_version: Option<&str>,
) -> crate::config::LlmConfig {
    let mut cfg = crate::config::LlmConfig::default();
    cfg.azure_resource = resource.map(str::to_string);
    cfg.azure_deployment = deployment.map(str::to_string);
    cfg.azure_api_version = api_version.map(str::to_string);
    cfg
}

#[test]
fn compose_azure_openai_url_happy_path() {
    // Spec-literal URL — the operator-derived shape Azure expects. If
    // any of the three path segments drift, every Azure request 404s.
    let cfg = azure_test_cfg(Some("my-resource"), Some("my-gpt4o"), Some("2024-10-21"));
    let url = compose_azure_openai_url(&cfg).unwrap();
    assert_eq!(
        url,
        "https://my-resource.openai.azure.com/openai/deployments/my-gpt4o/chat/completions?api-version=2024-10-21"
    );
}

#[test]
fn compose_azure_openai_url_missing_resource_errors() {
    let cfg = azure_test_cfg(None, Some("d"), Some("v"));
    let err = compose_azure_openai_url(&cfg).unwrap_err().to_string();
    assert!(
        err.contains("llm.azure_resource"),
        "error must name the missing setting; got: {err}"
    );
}

#[test]
fn compose_azure_openai_url_missing_deployment_errors() {
    let cfg = azure_test_cfg(Some("r"), None, Some("v"));
    let err = compose_azure_openai_url(&cfg).unwrap_err().to_string();
    assert!(err.contains("llm.azure_deployment"), "got: {err}");
}

#[test]
fn compose_azure_openai_url_missing_api_version_errors() {
    // No silent default — operators must opt into a specific API
    // version so upstream shape drift surfaces as an explicit upgrade.
    let cfg = azure_test_cfg(Some("r"), Some("d"), None);
    let err = compose_azure_openai_url(&cfg).unwrap_err().to_string();
    assert!(err.contains("llm.azure_api_version"), "got: {err}");
}

#[test]
fn compose_azure_openai_url_empty_string_treated_as_missing() {
    // Setting to "" via `borg settings set llm.azure_resource ''` must
    // not produce a URL with an empty host segment that quietly resolves
    // to a wildcard match against `.openai.azure.com`.
    let cfg = azure_test_cfg(Some(""), Some("d"), Some("v"));
    assert!(compose_azure_openai_url(&cfg).is_err());
}

#[test]
fn compose_azure_openai_url_rejects_path_unsafe_chars() {
    // Reject characters that would either change the URL semantics
    // (`/`, `?`, `#`) or require percent-encoding. Surfacing as a
    // config error beats a runtime 404 with a baffling URL.
    let cfg = azure_test_cfg(Some("r"), Some("bad/name"), Some("2024-10-21"));
    let err = compose_azure_openai_url(&cfg).unwrap_err().to_string();
    assert!(err.contains("llm.azure_deployment"), "got: {err}");

    let cfg = azure_test_cfg(Some("r"), Some("d"), Some("v?injected=1"));
    assert!(compose_azure_openai_url(&cfg).is_err());
}

#[test]
fn compose_azure_openai_url_rejects_dot_in_resource() {
    // Subdomain-injection guard: a `.` in `azure_resource` would let an
    // operator typo (or a hostile setting) point requests at an arbitrary
    // host — `evil.attacker` →
    // `https://evil.attacker.openai.azure.com`. Resource names are DNS
    // labels (alphanumeric + `-` only), so `.` must be a hard error.
    let cfg = azure_test_cfg(Some("evil.attacker"), Some("d"), Some("2024-10-21"));
    let err = compose_azure_openai_url(&cfg).unwrap_err().to_string();
    assert!(err.contains("llm.azure_resource"), "got: {err}");
    assert!(err.contains("DNS label"), "got: {err}");

    // Underscore is also forbidden in DNS labels.
    let cfg = azure_test_cfg(Some("bad_resource"), Some("d"), Some("2024-10-21"));
    assert!(compose_azure_openai_url(&cfg).is_err());
}

fn azure_test_config(env_var: &str) -> Config {
    std::env::set_var(env_var, "test-key");
    let mut cfg = Config::default();
    cfg.llm.provider = Some("azure-openai".to_string());
    cfg.llm.model = "gpt-4o".to_string();
    cfg.llm.api_key_env = env_var.to_string();
    cfg
}

#[tokio::test]
async fn azure_openai_construction_fails_without_settings() {
    // LlmClient::new must reject an Azure config that's missing the
    // required Azure settings — fail-fast at start rather than 400 on
    // first prompt. This pins the construction-time guard.
    let cfg = azure_test_config("BORG_TEST_AZURE_MISSING");
    let err = match LlmClient::new(&cfg).await {
        Ok(_) => panic!("expected Azure config-missing error"),
        Err(e) => e.to_string(),
    };
    assert!(
        err.contains("Azure OpenAI"),
        "construction error must name Azure; got: {err}"
    );
}

#[tokio::test]
async fn azure_openai_construction_succeeds_with_settings() {
    // Mirror of the failure case: with all three fields set, construction
    // succeeds and the composed URL is stored as the effective base URL.
    let mut cfg = azure_test_config("BORG_TEST_AZURE_OK");
    cfg.llm.azure_resource = Some("acme".to_string());
    cfg.llm.azure_deployment = Some("gpt-4o-prod".to_string());
    cfg.llm.azure_api_version = Some("2024-10-21".to_string());

    let client = LlmClient::new(&cfg)
        .await
        .expect("Azure client should construct");
    let url = client.effective_base_url().to_string();
    assert_eq!(
        url,
        "https://acme.openai.azure.com/openai/deployments/gpt-4o-prod/chat/completions?api-version=2024-10-21"
    );
}

#[tokio::test]
async fn azure_openai_construction_respects_base_url_override() {
    // Operators sometimes proxy Azure through a private gateway. When
    // `llm.base_url` is set, the Azure URL composer must defer to it.
    let mut cfg = azure_test_config("BORG_TEST_AZURE_PROXY");
    cfg.llm.base_url = Some(
        "https://proxy.internal/azure-openai/chat/completions?api-version=2024-10-21".to_string(),
    );

    let client = LlmClient::new(&cfg)
        .await
        .expect("base_url override should bypass composer");
    let url = client.effective_base_url().to_string();
    assert_eq!(
        url,
        "https://proxy.internal/azure-openai/chat/completions?api-version=2024-10-21"
    );
}
