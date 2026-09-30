use super::*;
use crate::types::{FunctionCall, ToolCall, ToolDefinition};
use std::sync::{Arc, Mutex};

// ── Reconnect backoff ──

#[test]
fn a_lost_connection_waits_seconds_not_milliseconds() {
    // The configured schedule starts in the hundreds of milliseconds, which
    // burns the whole retry budget before a network that is merely between
    // states can come back.
    let configured = Duration::from_millis(200);
    let cfg = backoff_config_for(FailoverReason::ConnectionLost, configured);

    assert_eq!(cfg.initial, RECONNECT_INITIAL_DELAY);
    assert_eq!(cfg.max_delay, RECONNECT_MAX_DELAY);
    assert!(
        cfg.initial > configured,
        "the reconnect wait must be longer than the ordinary retry delay"
    );
}

#[test]
fn every_other_failure_class_keeps_the_configured_schedule() {
    // The provider is answering in all of these; retrying promptly is right,
    // and a minute-long pause would be a regression.
    let configured = Duration::from_millis(250);
    for reason in [
        FailoverReason::RateLimit,
        FailoverReason::Overloaded,
        FailoverReason::Timeout,
        FailoverReason::Unknown,
    ] {
        let cfg = backoff_config_for(reason, configured);
        assert_eq!(cfg.initial, configured, "unexpected schedule for {reason}");
        assert_eq!(cfg.max_delay, BackoffConfig::default().max_delay);
    }
}

#[test]
fn the_reconnect_wait_grows_and_then_holds_at_the_ceiling() {
    let cfg = backoff_config_for(FailoverReason::ConnectionLost, Duration::from_millis(200));

    let first = crate::util::retry::backoff_with_config(0, &cfg);
    let later = crate::util::retry::backoff_with_config(10, &cfg);

    assert!(first >= RECONNECT_INITIAL_DELAY);
    assert!(
        later > first,
        "the wait must grow while the provider is down"
    );
    // Jitter is additive on top of the cap, so allow the configured fraction.
    assert!(
        later <= RECONNECT_MAX_DELAY.mul_f64(1.0 + cfg.jitter_fraction),
        "the wait must stop growing so recovery is still noticed"
    );
}

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

async fn make_test_xai_client() -> LlmClient {
    let env_var = "BORG_TEST_XAI_LLM";
    std::env::set_var(env_var, "xai-test");
    let mut config = Config::default();
    config.llm.provider = Some("xai".to_string());
    config.llm.api_key_env = env_var.to_string();
    config.llm.model = "grok-4".to_string();
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

/// The fallback chain holds one slot per distinct (provider, model,
/// endpoint, credentials). A duplicate entry, in any spelling, and an entry
/// that restates the primary would only re-run the same failed request on
/// the same endpoint with the same key; a different model, endpoint (host
/// case aside — paths are case-sensitive), or key on the same provider is a
/// real alternative and stays.
#[tokio::test]
async fn fallback_chain_drops_duplicates_and_the_primary() {
    let env_var = "BORG_TEST_FALLBACK_DEDUP";
    let paid_env_var = "BORG_TEST_FALLBACK_DEDUP_PAID";
    std::env::set_var(env_var, "sk-test");
    std::env::set_var(paid_env_var, "sk-paid");
    let mut config = Config::default();
    config.llm.provider = Some("openai".to_string());
    config.llm.model = "gpt-4.1".to_string();
    config.llm.api_key_env = env_var.to_string();
    let keyed = |provider: &str, model: &str, base_url: Option<&str>, key_env: &str| {
        crate::config::LlmFallback {
            provider: provider.to_string(),
            model: model.to_string(),
            api_key: None,
            api_key_env: Some(key_env.to_string()),
            api_keys: Vec::new(),
            temperature: None,
            max_tokens: None,
            base_url: base_url.map(str::to_string),
        }
    };
    let entry = |provider: &str, model: &str, base_url: Option<&str>| {
        keyed(provider, model, base_url, env_var)
    };
    config.llm.fallback = vec![
        entry("anthropic", "claude-sonnet-4", None),
        entry("Anthropic", "Claude-Sonnet-4", None),
        entry("anthropic", "claude-sonnet-4", Some("")),
        entry("openai", "GPT-4.1", None),
        entry("openai", "gpt-4.1-mini", None),
        entry("openai", "gpt-4.1-mini", Some("https://proxy.example/v1/")),
        entry("openai", "gpt-4.1-mini", Some("https://PROXY.example/v1")),
        entry("openai", "gpt-4.1-mini", Some("https://proxy.example/V1")),
        // The primary's provider and model under a different key: the paid
        // account behind the free one is the whole point of the entry.
        keyed("openai", "gpt-4.1", None, paid_env_var),
        keyed("openai", "gpt-4.1", None, paid_env_var),
    ];

    let client = LlmClient::new(&config).await.expect("client");
    let slots: Vec<(Provider, String, Option<String>, Vec<String>)> = client
        .provider_slots
        .iter()
        .map(|s| {
            (
                s.provider,
                s.model.clone(),
                s.base_url.clone(),
                s.keys.clone(),
            )
        })
        .collect();
    let free = vec!["sk-test".to_string()];
    assert_eq!(
        slots,
        vec![
            (
                Provider::Anthropic,
                "claude-sonnet-4".to_string(),
                None,
                free.clone()
            ),
            (
                Provider::OpenAi,
                "gpt-4.1-mini".to_string(),
                None,
                free.clone()
            ),
            (
                Provider::OpenAi,
                "gpt-4.1-mini".to_string(),
                Some("https://proxy.example/v1/".to_string()),
                free.clone(),
            ),
            (
                Provider::OpenAi,
                "gpt-4.1-mini".to_string(),
                Some("https://proxy.example/V1".to_string()),
                free,
            ),
            (
                Provider::OpenAi,
                "gpt-4.1".to_string(),
                None,
                vec!["sk-paid".to_string()],
            ),
        ]
    );
}

/// The re-send cap is the affordable figure less the headroom, and only
/// when that lands on or above the floor: a cap whose remainder would be
/// cut off anyway (100, or 64 under the floor) is not worth one more
/// request, and a result that is not a credit-limited 402 never lowers.
#[test]
fn lowered_output_cap_keeps_headroom_and_honors_the_floor() {
    let affordable = |cap: u32| -> std::result::Result<(), LlmError> {
        Err(llm_error::classify_status(
            reqwest::StatusCode::PAYMENT_REQUIRED,
            &format!("You requested up to 56272 tokens, but can only afford {cap}."),
            Provider::OpenRouter,
        ))
    };
    let cases: &[(u32, Option<u32>)] = &[
        (12000, Some(12000 - OUTPUT_CAP_HEADROOM)),
        (
            OUTPUT_CAP_FLOOR + OUTPUT_CAP_HEADROOM,
            Some(OUTPUT_CAP_FLOOR),
        ),
        (OUTPUT_CAP_FLOOR + OUTPUT_CAP_HEADROOM - 1, None),
        (100, None),
    ];
    for (cap, want) in cases {
        assert_eq!(lowered_output_cap(&affordable(*cap)), *want, "afford {cap}");
    }
    let billing: std::result::Result<(), LlmError> = Err(llm_error::classify_status(
        reqwest::StatusCode::PAYMENT_REQUIRED,
        "Insufficient credits. Add more credits and retry.",
        Provider::OpenRouter,
    ));
    assert_eq!(lowered_output_cap(&billing), None);
    assert_eq!(lowered_output_cap::<()>(&Ok(())), None);
}

/// A host that blackholes SYNs must cost one connect timeout, not one per
/// attempt. Once recorded, the request path has to bail before dialing so the
/// fallback chain gets its turn immediately — and the error must stay
/// retryable, since the endpoint is expected to come back.
#[tokio::test]
async fn a_blackholed_endpoint_is_skipped_without_dialing() {
    let env_var = "BORG_TEST_BLACKHOLE_LLM";
    std::env::set_var(env_var, "sk-test");
    let mut config = Config::default();
    config.llm.provider = Some("openai".to_string());
    config.llm.api_key_env = env_var.to_string();
    // Reserved TEST-NET-1 (RFC 5737): routable-looking, never answers. If the
    // short-circuit fails to fire, this test hangs on a real connect instead of
    // silently passing.
    let dead_url = "http://192.0.2.1:9443/v1/chat/completions";
    config.llm.base_url = Some(dead_url.to_string());
    let client = LlmClient::new(&config).await.unwrap();

    // The registry is process-global, so an early failure below would otherwise
    // leak a 30s entry into every sibling test that touches this endpoint.
    struct ClearOnDrop(&'static str);
    impl Drop for ClearOnDrop {
        fn drop(&mut self) {
            crate::llm::blackhole::clear(self.0);
        }
    }
    let _guard = ClearOnDrop(dead_url);

    crate::llm::blackhole::record_connect_failure(dead_url, std::time::Instant::now());

    let started = std::time::Instant::now();
    let err = client
        .send_request(
            dead_url,
            &serde_json::json!({}),
            &tokio_util::sync::CancellationToken::new(),
        )
        .await
        .expect_err("a blackholed endpoint must not be dialed");

    assert!(
        started.elapsed() < std::time::Duration::from_secs(1),
        "must short-circuit rather than wait out a connect timeout"
    );
    assert!(
        err.to_string().contains("TCP connect"),
        "error must explain why the endpoint was skipped, got: {err}"
    );
    assert!(
        matches!(err.reason(), FailoverReason::Timeout),
        "a blackholed host is expected to recover — the error must stay retryable"
    );
}

/// A *refused* connection is not a blackhole: it fails instantly, so there is
/// no connect timeout to save, and marking it dead would replace the accurate
/// error ("is the Ollama server running?") with a generic skip for the whole
/// TTL — including on the first request after the operator starts the server.
#[tokio::test]
async fn a_refused_connection_does_not_blackhole_the_endpoint() {
    let mut config = Config::default();
    config.llm.provider = Some("ollama".to_string());
    // Loopback, port 1: nothing listens, so the connect is refused at once.
    let refused_url = "http://127.0.0.1:1/v1/chat/completions";
    config.llm.base_url = Some("http://127.0.0.1:1/v1".to_string());
    let client = LlmClient::new(&config).await.unwrap();

    let err = client
        .send_request(
            refused_url,
            &serde_json::json!({}),
            &tokio_util::sync::CancellationToken::new(),
        )
        .await
        .expect_err("a refused connection must surface as an error");
    assert!(
        err.to_string().contains("ollama serve"),
        "the refusal must keep its actionable hint, got: {err}"
    );
    assert!(
        !crate::llm::blackhole::is_blackholed(refused_url, std::time::Instant::now()),
        "a refusal must not suppress the next attempt's real error"
    );
}

/// A bare model id on a prefix-routed provider 404s, and the raw
/// "model not found" reads like an outage — users chase the provider's status
/// page instead of their own config, so the error has to name the missing
/// prefix. It must fire *only* for an id that actually reached the wire bare:
/// an id `normalize_model` repaired, or one that already carried its vendor,
/// went out fully qualified, so blaming the prefix would tell the user to set
/// exactly what the request already sent.
#[tokio::test]
async fn model_not_found_blames_the_vendor_prefix_only_when_one_is_missing() {
    let env_var = "BORG_TEST_OPENROUTER_VENDOR_PREFIX";
    std::env::set_var(env_var, "sk-test");

    for (model, should_explain) in [
        // Unrecognised family: survives normalization and reaches the wire bare.
        ("some-unknown-model-v9", true),
        // Recognised family: goes out as `anthropic/claude-does-not-exist-9`.
        ("claude-does-not-exist-9", false),
        // Already fully qualified.
        ("anthropic/claude-sonnet-4.6", false),
    ] {
        let mut config = Config::default();
        config.llm.provider = Some("openrouter".to_string());
        config.llm.api_key_env = env_var.to_string();
        config.llm.model = model.to_string();
        let client = LlmClient::new(&config).await.unwrap();

        let raw = crate::llm::llm_error::classify_status(
            reqwest::StatusCode::NOT_FOUND,
            "{\"error\":\"model not found\"}",
            crate::llm::Provider::OpenRouter,
        );
        let raw_message = raw.to_string();
        let explained = client.explain_missing_vendor_prefix(raw);
        let message = explained.to_string();

        if should_explain {
            assert!(
                message.contains(model) && message.contains("vendor"),
                "{model}: error must name the model and the missing vendor prefix, got: {message}"
            );
        } else {
            assert_eq!(
                message, raw_message,
                "{model}: a fully-qualified id must pass the error through untouched"
            );
        }
        assert!(
            matches!(explained.reason(), FailoverReason::ModelNotFound),
            "{model}: rewriting the message must not change failover behavior"
        );
    }
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
        response_id: None,
        upstream: None,
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
        response_id: None,
        upstream: None,
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
    let action =
        transport::parse_openai_sse_line(line, "openai", "gpt-4o", &mut Default::default());
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
    let action =
        transport::parse_openai_sse_line(line, "openai", "gpt-4o", &mut Default::default());
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
        &mut None,
        &mut None,
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
    let action = transport::parse_openai_sse_line(
        line,
        "openrouter",
        "openai/gpt-4o-mini",
        &mut Default::default(),
    );
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
    let action = transport::parse_openai_sse_line(
        line,
        "openrouter",
        "openai/gpt-4o-mini",
        &mut Default::default(),
    );
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
    let action = transport::parse_openai_sse_line(
        line,
        "openrouter",
        "openai/gpt-4o-mini",
        &mut Default::default(),
    );
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
        &mut Default::default(),
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
    let done_action = transport::parse_openai_sse_line(
        done_line,
        "openrouter",
        "anthropic/claude-sonnet-4.5",
        &mut Default::default(),
    );
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
        response_id: None,
        upstream: None,
    };

    client.emit_cost_event_best_effort("req-3", Some(&usage), "gpt-4.1");

    let events = sink.take();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].cost_usd.as_deref(), Some("0.006"));
    assert!(events[0].cost_usd_present);
    assert_eq!(events[0].cost_source, "table_openai");
}

/// xAI has its own rate table; a Grok call must price from it rather than
/// fall through to `unknown`.
#[tokio::test]
async fn xai_cost_event_uses_static_price_table() {
    let sink = Arc::new(CollectingSink::default());
    let client = make_test_xai_client()
        .await
        .with_cost_event_sink(sink.clone(), cost_context());
    let usage = UsageData {
        prompt_tokens: 1_000,
        completion_tokens: 500,
        total_tokens: 1_500,
        cached_input_tokens: 0,
        cache_creation_tokens: 0,
        provider: "xai".to_string(),
        model: "grok-4".to_string(),
        provider_cost_usd: None,
        response_id: None,
        upstream: None,
    };

    client.emit_cost_event_best_effort("req-xai", Some(&usage), "grok-4");

    let events = sink.take();
    assert_eq!(events.len(), 1);
    // 1,000 × $3/M + 500 × $15/M = 0.003 + 0.0075.
    assert_eq!(events[0].cost_usd.as_deref(), Some("0.0105"));
    assert!(events[0].cost_usd_present);
    assert_eq!(events[0].cost_source, "table_xai");
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
        response_id: None,
        upstream: None,
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
        response_id: None,
        upstream: None,
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
        response_id: None,
        upstream: None,
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
        response_id: None,
        upstream: None,
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
            reasoning_blocks: None,
            reasoning_content: None,
            reasoning_model: None,
        },
        Message::tool_result("call_1", "file contents here"),
    ];

    let anthropic_msgs = build_anthropic_messages(&messages, None);

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
            reasoning_blocks: None,
            reasoning_content: None,
            reasoning_model: None,
        },
        Message::tool_result_multimodal("call_cap", parts),
    ];

    let anthropic_msgs = build_anthropic_messages(&messages, None);
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
            reasoning_blocks: None,
            reasoning_content: None,
            reasoning_model: None,
        },
        Message::tool_result("call_1", "result a"),
        Message::tool_result("call_2", "result b"),
    ];

    let anthropic_msgs = build_anthropic_messages(&messages, None);

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

/// A non-streaming reply keeps its signed `thinking` and `redacted_thinking`
/// blocks verbatim and in order, so the next tool-loop request can replay
/// them exactly as returned; text and tool calls are unaffected.
#[test]
fn parse_anthropic_response_keeps_signed_reasoning_blocks_verbatim() {
    let thinking =
        serde_json::json!({"type": "thinking", "thinking": "plan", "signature": "sig_1"});
    let redacted = serde_json::json!({"type": "redacted_thinking", "data": "opaque=="});
    let resp = serde_json::json!({
        "content": [
            thinking, redacted,
            {"type": "text", "text": "Checking."},
            {"type": "tool_use", "id": "toolu_9", "name": "fs_read", "input": {"path": "a"}}
        ],
        "stop_reason": "tool_use"
    });

    let msg = parse_anthropic_response(&resp).unwrap();
    assert_eq!(msg.reasoning_blocks, Some(vec![thinking, redacted]));
    assert_eq!(msg.text_content(), Some("Checking."));
    assert_eq!(msg.tool_calls.as_ref().map(Vec::len), Some(1));

    let plain = parse_anthropic_response(&serde_json::json!({
        "content": [{"type": "text", "text": "hi"}]
    }))
    .unwrap();
    assert!(
        plain.reasoning_blocks.is_none(),
        "no blocks means None, not an empty list"
    );
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
    cd.record_failure(FailoverReason::RateLimit, None);
    assert!(cd.is_active());
    assert_eq!(cd.error_count, 1);
    assert_eq!(cd.reason, Some(FailoverReason::RateLimit));
}

#[test]
fn cooldown_reset_on_success() {
    let mut cd = ProviderCooldown::default();
    cd.record_failure(FailoverReason::Overloaded, None);
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
    cd.record_failure(FailoverReason::RateLimit, None);
    let first = cd.cooldown_until.unwrap();

    cd.record_failure(FailoverReason::RateLimit, None);
    let second = cd.cooldown_until.unwrap();

    cd.record_failure(FailoverReason::RateLimit, None);
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

/// The cooldown is max(backoff, Retry-After), with the hint capped so one
/// absurd header cannot park a provider for a day.
#[test]
fn cooldown_honors_retry_after_above_backoff_with_cap() {
    let cap = Duration::from_secs(crate::constants::PROVIDER_COOLDOWN_RETRY_AFTER_MAX_SECS);
    let backoff = Duration::from_secs_f64(crate::constants::PROVIDER_COOLDOWN_TRANSIENT_BASE_SECS);
    let cases = [
        (None, backoff, "no hint: plain backoff"),
        (
            Some(Duration::from_secs(5)),
            backoff,
            "short hint: backoff wins",
        ),
        (
            Some(Duration::from_secs(600)),
            Duration::from_secs(600),
            "long hint wins",
        ),
        (
            Some(Duration::from_secs(24 * 60 * 60)),
            cap,
            "huge hint is capped",
        ),
    ];
    for (hint, expected, label) in cases {
        let mut cd = ProviderCooldown::default();
        let before = std::time::Instant::now();
        cd.record_failure(FailoverReason::RateLimit, hint);
        let after = std::time::Instant::now();
        let until = cd.cooldown_until.expect("cooldown set");
        assert!(
            until >= before + expected && until <= after + expected,
            "{label}: expected ~{expected:?}, got {:?}",
            until - before
        );
    }
}

#[test]
fn cooldown_auth_has_longer_base() {
    let mut cd1 = ProviderCooldown::default();
    cd1.record_failure(FailoverReason::RateLimit, None);
    let rl_until = cd1.cooldown_until.unwrap();

    let mut cd2 = ProviderCooldown::default();
    cd2.record_failure(FailoverReason::Auth, None);
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
    let mut client = LlmClient::new(&config).await.expect("client builds");

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

/// Calls beside a turn (compaction, titles) have no stream handler to record
/// their usage, so the client keeps what the stream reported until the owner
/// takes it. A failed call's usage is kept too: the provider billed it.
#[tokio::test]
async fn chat_streamed_keeps_the_usage_for_the_owner_to_record() {
    use super::test_server::{reply_with_usage, serve_sse};
    for (finish, succeeds) in [("stop", true), ("content_filter", false)] {
        let (url, _) = serve_sse(vec![reply_with_usage("Summary", finish)]);
        let mut config = Config::default();
        config.llm.provider = Some("custom".to_string());
        config.llm.model = "test-model".to_string();
        config.llm.base_url = Some(url);
        config.llm.max_retries = 0;
        let mut client = LlmClient::new(&config).await.expect("client builds");

        let outcome = tokio::time::timeout(
            Duration::from_secs(10),
            client.chat_streamed_checked(&[Message::user("summarize")]),
        )
        .await
        .expect("stream must complete promptly");

        assert_eq!(outcome.is_ok(), succeeds, "finish_reason {finish}");
        let usage: Vec<(u64, u64)> = client
            .take_usage()
            .iter()
            .map(|u| (u.prompt_tokens, u.completion_tokens))
            .collect();
        assert_eq!(usage, vec![(11, 7)], "finish_reason {finish}");
        assert!(client.take_usage().is_empty(), "a take drains the usage");
    }
}

/// A retry sends the full answer again. The aggregate must hold the text of
/// the attempt that completes, without the text of the attempt that failed.
/// The usage of both attempts is kept: the provider billed both.
///
/// The first connection sends two deltas and a usage report, then closes
/// with no completion marker. The second connection sends a complete
/// answer. The wait between the two is the real reconnect delay, so the
/// test is slow.
#[tokio::test]
async fn chat_streamed_keeps_only_the_text_of_the_attempt_that_completes() {
    const CUT_SHORT: &str = concat!(
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hello \"},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"summ\"},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":2,\"total_tokens\":7}}\n\n",
    );
    const COMPLETE: &str = concat!(
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hello \"},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"summary world\"},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":11,\"completion_tokens\":7,\"total_tokens\":18}}\n\n",
        "data: [DONE]\n\n",
    );
    let (url, requests) = super::test_server::serve_sse(vec![CUT_SHORT, COMPLETE]);

    let mut config = Config::default();
    config.llm.provider = Some("custom".to_string());
    config.llm.model = "test-model".to_string();
    config.llm.base_url = Some(url);
    config.llm.max_retries = 1;
    let mut client = LlmClient::new(&config).await.expect("client builds");

    let (message, truncated) = tokio::time::timeout(
        Duration::from_secs(60),
        client.chat_streamed_checked(&[Message::user("summarize")]),
    )
    .await
    .expect("the retry must complete inside the reconnect delay")
    .expect("the second attempt is complete");

    assert_eq!(
        requests.lock().expect("requests").len(),
        2,
        "the test is valid only when the first attempt failed and a second one ran"
    );
    assert_eq!(message.text_content(), Some("Hello summary world"));
    assert!(!truncated);
    let usage: Vec<(u64, u64)> = client
        .take_usage()
        .iter()
        .map(|u| (u.prompt_tokens, u.completion_tokens))
        .collect();
    assert_eq!(usage, vec![(5, 2), (11, 7)]);
}

/// A model without tool support takes the non-streamed path. Its usage
/// must reach the owner the same way.
#[tokio::test]
async fn a_model_without_tool_support_keeps_its_usage_too() {
    // The non-streamed path reads the body as JSON whatever its content
    // type says.
    let reply = serde_json::json!({
        "id": "chatcmpl-1",
        "object": "chat.completion",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "Summary"},
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 11, "completion_tokens": 7, "total_tokens": 18}
    });
    let (url, requests) = super::test_server::serve_sse(vec![reply.to_string()]);
    let mut config = Config::default();
    config.llm.provider = Some("custom".to_string());
    config.llm.model = "deepseek-r1".to_string();
    config.llm.base_url = Some(url);
    config.llm.max_retries = 0;
    let mut client = LlmClient::new(&config).await.expect("client builds");

    let message = tokio::time::timeout(
        Duration::from_secs(10),
        client.chat_streamed(&[Message::user("summarize")]),
    )
    .await
    .expect("the call must complete promptly")
    .expect("the reply parses");

    assert!(
        !requests.lock().expect("requests")[0].contains("\"stream\":true"),
        "the test is valid only when the non-streamed path ran"
    );
    assert_eq!(message.text_content(), Some("Summary"));
    let usage: Vec<(u64, u64)> = client
        .take_usage()
        .iter()
        .map(|u| (u.prompt_tokens, u.completion_tokens))
        .collect();
    assert_eq!(usage, vec![(11, 7)]);
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
    let mut client = LlmClient::new(&config).await.expect("client builds");

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
async fn anthropic_request_omits_temperature_for_models_that_reject_it() {
    // Opus 4.7 onward answers `temperature` with a 400 even with thinking off.
    for model in ["claude-opus-5", "claude-opus-4-7", "claude-sonnet-5-5"] {
        let mut config = Config::default();
        config.llm.provider = Some("anthropic".to_string());
        config.llm.api_key_env = "ANTHROPIC_API_KEY".to_string();
        config.llm.model = model.to_string();
        config.llm.thinking = crate::config::ThinkingLevel::Off;
        std::env::set_var("ANTHROPIC_API_KEY", "test-key");
        let client = LlmClient::new(&config).await.expect("should create client");

        let body = client.build_anthropic_request(&[Message::user("hi")], None, false);

        assert!(
            body.get("temperature").is_none(),
            "{model} rejects temperature, got: {body}"
        );
    }
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

/// Gemini 3+ cannot switch thinking off; the native wire pins
/// `thinkingLevel: LOW` for Off, and the OpenAI-compat wire (OpenRouter)
/// must pin the same floor via `reasoning_effort` or the model thinks at
/// HIGH and spends the output cap. Pre-3 Gemini and non-Gemini models keep
/// omitting the field.
#[test]
fn openai_build_request_pins_low_reasoning_for_gemini_3_when_thinking_is_off() {
    use crate::llm::provider::Provider;
    use crate::llm::transport::TransportRequest;

    let mut cfg = LlmConfig::default();
    cfg.thinking = crate::config::ThinkingLevel::Off;
    let cases = [
        ("google/gemini-3-pro", Some("low")),
        ("google/gemini-4-flash", Some("low")),
        ("google/gemini-2.5-pro", None),
        ("gpt-4.1", None),
    ];
    for (model, expected) in cases {
        let req = TransportRequest {
            provider: Provider::OpenRouter,
            model,
            messages: &[Message::user("hi")],
            tools: None,
            llm_config: &cfg,
            prompt_cache_key: None,
            system_stable_prefix: None,
            cost_events_enabled: false,
        };
        let body = crate::llm::transport::build_openai_request(&req, false).expect("should build");
        assert_eq!(
            body.get("reasoning_effort").and_then(|v| v.as_str()),
            expected,
            "{model}"
        );
    }
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
fn openai_stream_request_sends_stream_options_by_default_and_drops_them_after_rejection() {
    // Invariant: streaming requests set `stream_options.include_usage =
    // true` by default. Without it, OpenAI-compatible providers (OpenAI,
    // OpenRouter, DeepSeek, Groq, Gemini-via-OpenAI) never emit a `usage`
    // chunk and `/usage` + monthly budget tracking silently return zeros.
    // The one sanctioned exception is an endpoint that has rejected the
    // field, after which the client strips it — and only it.
    use crate::llm::provider::Provider;
    use crate::llm::transport::TransportRequest;

    let cfg = LlmConfig::default();
    let req = TransportRequest {
        provider: Provider::XAi,
        model: "grok-4",
        messages: &[Message::user("hi")],
        tools: None,
        llm_config: &cfg,
        prompt_cache_key: None,
        system_stable_prefix: None,
        cost_events_enabled: false,
    };
    let mut body = transport::build_openai_request(&req, true).expect("build");
    assert_eq!(body["stream_options"]["include_usage"], true);

    assert!(
        transport::strip_stream_options(&mut body),
        "strip reports the field it removed"
    );
    assert!(body.get("stream_options").is_none());
    assert_eq!(body["stream"], true, "only stream_options is dropped");
    assert_eq!(body["model"], "grok-4");
    assert!(
        !transport::strip_stream_options(&mut body),
        "a second strip finds nothing to remove"
    );
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
        &mut None,
        &mut None,
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
        &mut None,
        &mut None,
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
        &mut None,
        &mut None,
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
        &mut None,
        &mut None,
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
            reasoning_blocks: None,
            reasoning_content: None,
            reasoning_model: None,
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
            reasoning_blocks: None,
            reasoning_content: None,
            reasoning_model: None,
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
            reasoning_blocks: None,
            reasoning_content: None,
            reasoning_model: None,
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
            reasoning_blocks: None,
            reasoning_content: None,
            reasoning_model: None,
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
        reasoning_blocks: None,
        reasoning_content: None,
        reasoning_model: None,
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

/// Assistant message issuing one tool call per `(id, name, args)` triple.
fn repair_asst_calls(calls: &[(&str, &str, &str)]) -> Message {
    Message {
        role: Role::Assistant,
        content: None,
        tool_calls: Some(
            calls
                .iter()
                .map(|(id, name, args)| ToolCall {
                    id: (*id).to_string(),
                    call_type: "function".to_string(),
                    function: FunctionCall {
                        name: (*name).to_string(),
                        arguments: (*args).to_string(),
                    },
                })
                .collect(),
        ),
        tool_call_id: None,
        timestamp: None,
        seq: None,
        reasoning_blocks: None,
        reasoning_content: None,
        reasoning_model: None,
    }
}

#[test]
fn repair_renames_id_reusing_calls_and_keeps_both_pairs() {
    // An id-reusing backend emitted two *different* calls under one id in a
    // single batch. Dropping the later one (the old behavior) silently loses
    // its result and the model confabulates the missing output. Repair must
    // instead rename the later call deterministically and re-pair its result
    // (results match by id + order) so both pairs survive intact.
    let messages = vec![
        Message::user("a"),
        repair_asst_calls(&[
            ("call_1", "read_file", "{\"path\": \"a.txt\"}"),
            ("call_1", "read_file", "{\"path\": \"b.txt\"}"),
        ]),
        Message::tool_result("call_1", "contents of a"),
        Message::tool_result("call_1", "contents of b"),
    ];
    let repaired = repair_message_sequence(&messages);
    assert_eq!(
        repaired.len(),
        4,
        "no message may be dropped: {repaired:#?}"
    );
    assert_eq!(
        repair_all_call_ids(&repaired),
        vec!["call_1", "call_1_d1"],
        "later colliding call gets a stable occurrence-indexed suffix"
    );
    // Pairing: first result stays with the original id, second follows its
    // renamed call — contents must land on the right call.
    let results: Vec<(String, String)> = repaired
        .iter()
        .filter(|m| m.role == Role::Tool)
        .map(|m| {
            (
                m.tool_call_id.clone().unwrap(),
                m.content.as_ref().unwrap().text().unwrap().to_string(),
            )
        })
        .collect();
    assert_eq!(
        results,
        vec![
            ("call_1".to_string(), "contents of a".to_string()),
            ("call_1_d1".to_string(), "contents of b".to_string()),
        ],
        "each result must stay paired with its own call"
    );
    // Determinism: same input → same renames (prompt-cache prefix stability).
    let again = repair_message_sequence(&messages);
    assert_eq!(repair_all_call_ids(&repaired), repair_all_call_ids(&again));
    let result_ids = |msgs: &[Message]| -> Vec<String> {
        msgs.iter()
            .filter(|m| m.role == Role::Tool)
            .map(|m| m.tool_call_id.clone().unwrap())
            .collect()
    };
    assert_eq!(result_ids(&repaired), result_ids(&again));
}

#[test]
fn repair_does_not_rename_a_colliding_call_that_has_no_result() {
    // Regression: renaming trades one invalid shape for another when the
    // history carries only ONE result for the reused id — an interrupted turn
    // re-appended on resume, whose second result never landed. A renamed
    // `call_1_d1` with no `tool` message after it 400s on the same strict
    // providers this repair exists for, so the extra call must be dropped.
    let messages = vec![
        Message::user("a"),
        repair_asst_calls(&[
            ("call_1", "read_file", "{\"path\": \"a.txt\"}"),
            ("call_1", "read_file", "{\"path\": \"b.txt\"}"),
        ]),
        Message::tool_result("call_1", "contents of a"),
    ];
    let repaired = repair_message_sequence(&messages);
    assert_eq!(
        repair_all_call_ids(&repaired),
        vec!["call_1"],
        "a call with no result to pair must be dropped, not renamed: {repaired:#?}"
    );
    let result_ids: Vec<String> = repaired
        .iter()
        .filter(|m| m.role == Role::Tool)
        .map(|m| m.tool_call_id.clone().unwrap())
        .collect();
    assert_eq!(
        repair_all_call_ids(&repaired),
        result_ids,
        "every surviving call must be answered by exactly one result"
    );
}

#[test]
fn repair_renames_id_reuse_across_turns_without_dropping_assistant() {
    // Regression: the old drop path could reduce a later assistant turn to
    // zero calls and delete the whole message. With id reuse across turns
    // (different payloads), both assistant messages and both results must
    // survive, with the later pair renamed.
    let messages = vec![
        Message::user("a"),
        repair_asst_calls(&[("call_1", "run_shell", "{\"cmd\": \"ls\"}")]),
        Message::tool_result("call_1", "r1"),
        repair_asst_calls(&[("call_1", "run_shell", "{\"cmd\": \"pwd\"}")]),
        Message::tool_result("call_1", "r2"),
    ];
    let repaired = repair_message_sequence(&messages);
    let assistant_count = repaired
        .iter()
        .filter(|m| m.role == Role::Assistant)
        .count();
    assert_eq!(
        assistant_count, 2,
        "no assistant message may be dropped: {repaired:#?}"
    );
    assert_eq!(repair_all_call_ids(&repaired), vec!["call_1", "call_1_d1"]);
    let last = repaired.last().unwrap();
    assert_eq!(last.role, Role::Tool);
    assert_eq!(
        last.tool_call_id.as_deref(),
        Some("call_1_d1"),
        "the second result must follow its renamed call"
    );
    assert_eq!(
        last.content.as_ref().unwrap().text().unwrap(),
        "r2",
        "result content must stay with its own call"
    );
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
            reasoning_blocks: None,
            reasoning_content: None,
            reasoning_model: None,
        },
        Message {
            role: Role::Tool,
            content: Some(crate::types::MessageContent::Text(String::new())),
            tool_calls: None,
            tool_call_id: Some("call_1".to_string()),
            timestamp: None,
            seq: None,
            reasoning_blocks: None,
            reasoning_content: None,
            reasoning_model: None,
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
    // Turn one of an agent conversation: no tools yet, one user message. The
    // next turn reads this prefix back, so it must be cached — the one-shot
    // exemption is an explicit client flag, never a guess from this shape.
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

#[allow(clippy::expect_used, clippy::unwrap_used)]
#[tokio::test]
async fn one_shot_client_sends_no_cache_markers_at_all() {
    // A summarizer/title/classification call is never read back from the
    // cache, so a marker only buys the cache write (1.25x / 2x input). The
    // flag is the caller's explicit claim; the same request without it is
    // turn one of a conversation and keeps its markers (see
    // `anthropic_request_includes_cache_control_on_system`).
    fn marker_count(v: &serde_json::Value) -> usize {
        match v {
            serde_json::Value::Object(map) => map
                .iter()
                .map(|(k, v)| usize::from(k == "cache_control") + marker_count(v))
                .sum(),
            serde_json::Value::Array(items) => items.iter().map(marker_count).sum(),
            _ => 0,
        }
    }
    let messages = vec![
        Message::system("Summarize the conversation."),
        Message::user("a very long transcript"),
    ];
    let tools = vec![ToolDefinition::new(
        "t",
        "d",
        serde_json::json!({"type": "object"}),
    )];

    let plain = make_test_anthropic_client().await;
    assert!(marker_count(&plain.build_anthropic_request(&messages, None, false)) > 0);

    let one_shot = make_test_anthropic_client().await.with_one_shot();
    assert_eq!(
        marker_count(&one_shot.build_anthropic_request(&messages, None, false)),
        0
    );
    assert_eq!(
        marker_count(&one_shot.build_anthropic_request(&messages, Some(&tools), false)),
        0,
        "the flag wins even over the tools breakpoint"
    );

    // A shared client can be flagged for one call and handed back as it was.
    let mut shared = make_test_anthropic_client().await;
    let prior = shared.set_one_shot(true);
    assert!(!prior);
    assert_eq!(
        marker_count(&shared.build_anthropic_request(&messages, None, false)),
        0
    );
    shared.set_one_shot(prior);
    assert!(marker_count(&shared.build_anthropic_request(&messages, None, false)) > 0);
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
fn sse_usage_reads_cache_tokens_under_every_provider_spelling() {
    // An operator pointing `llm.base_url` at a DeepSeek-direct or
    // Anthropic-shaped endpoint still lands on the OpenAI-compatible
    // transport. Those responses spell the cache fields differently, and
    // reading only the OpenAI spelling reports zero cached tokens — the
    // turn looks uncached and its cost is overstated.
    struct Case {
        name: &'static str,
        usage: &'static str,
        cached_read: u64,
        cache_write: u64,
    }
    let cases = [
        Case {
            name: "openai nested details",
            usage: r#""prompt_tokens_details":{"cached_tokens":700}"#,
            cached_read: 700,
            cache_write: 0,
        },
        Case {
            name: "anthropic flat fields",
            usage: r#""cache_read_input_tokens":800,"cache_creation_input_tokens":90"#,
            cached_read: 800,
            cache_write: 90,
        },
        Case {
            name: "deepseek direct",
            usage: r#""prompt_cache_hit_tokens":640"#,
            cached_read: 640,
            cache_write: 0,
        },
        Case {
            name: "kimi/moonshot top-level",
            usage: r#""cached_tokens":512"#,
            cached_read: 512,
            cache_write: 0,
        },
        Case {
            name: "anthropic per-ttl cache-write breakdown",
            usage: r#""cache_creation":{"ephemeral_5m_input_tokens":30,"ephemeral_1h_input_tokens":12}"#,
            cached_read: 0,
            cache_write: 42,
        },
        Case {
            name: "cache write alias",
            usage: r#""cache_write_input_tokens":77"#,
            cached_read: 0,
            cache_write: 77,
        },
    ];

    for case in cases {
        let line = format!(
            r#"data: {{"choices":[],"usage":{{"prompt_tokens":10,"completion_tokens":2,"total_tokens":12,{}}}}}"#,
            case.usage
        );
        let usage = parse_openrouter_usage_from_sse(&line);
        assert_eq!(
            usage.cached_input_tokens, case.cached_read,
            "cached reads for {}",
            case.name
        );
        assert_eq!(
            usage.cache_creation_tokens, case.cache_write,
            "cache writes for {}",
            case.name
        );
    }
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
async fn for_auxiliary_switching_provider_does_not_inherit_primary_credentials() {
    // Regression: the synthetic aux config only overrode credentials when the
    // aux block set them, so a bare `[llm.aux] provider = "anthropic"` over an
    // OpenAI primary kept OpenAI's keys and base_url. `resolve_api_keys` then
    // ran `correct_provider_from_key`, saw the `sk-proj-` prefix, and reverted
    // the provider to OpenAI — the aux pin looked applied and silently wasn't,
    // so every background call billed the primary provider.
    let primary_env = "BORG_TEST_AUX_PRIMARY_OPENAI_KEY";
    std::env::set_var(primary_env, "sk-proj-primary-openai-key");
    // The aux block names no key, so resolution must fall to the *aux*
    // provider's own default env var — not the primary's inherited key.
    let aux_default_env = Provider::Anthropic.default_env_var();
    let restore_aux = std::env::var(aux_default_env).ok();
    std::env::set_var(aux_default_env, "sk-ant-aux-key");

    let mut config = Config::default();
    config.llm.provider = Some("openai".to_string());
    config.llm.api_key_env = primary_env.to_string();
    config.llm.model = "gpt-5".to_string();
    config.llm.base_url = Some("https://primary.example/v1".to_string());
    // Aux names a different provider and nothing else — the shape that broke.
    config.llm.aux = Some(crate::config::llm::AuxLlmConfig {
        provider: Some("anthropic".to_string()),
        model: "claude-haiku-4-5".to_string(),
        ..Default::default()
    });

    let client = LlmClient::for_auxiliary(&config)
        .await
        .expect("aux client must build");

    assert_eq!(
        client.provider,
        Provider::Anthropic,
        "the aux provider pin must survive key resolution"
    );
    assert_ne!(
        client.llm_config.base_url.as_deref(),
        Some("https://primary.example/v1"),
        "the primary's endpoint must not be reused for a different provider"
    );

    std::env::remove_var(primary_env);
    match restore_aux {
        Some(v) => std::env::set_var(aux_default_env, v),
        None => std::env::remove_var(aux_default_env),
    }
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
        &mut None,
        &mut None,
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
        crate::sse::SseAction::Error(LlmError::Fatal {
            reason,
            source,
            status,
        }) => {
            assert_eq!(reason, FailoverReason::AuthPermanent);
            let err = LlmError::Fatal {
                source,
                reason,
                status,
            };
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

/// Build a client with an OpenAI primary and one Anthropic fallback slot,
/// for exercising the failover/restore state machine without any network.
async fn make_failover_restore_client(env_var: &str) -> LlmClient {
    std::env::set_var(env_var, "sk-test");
    let mut config = Config::default();
    config.llm.provider = Some("openai".to_string());
    config.llm.model = "gpt-primary".to_string();
    config.llm.api_key_env = env_var.to_string();
    config.llm.fallback.push(crate::config::LlmFallback {
        provider: "anthropic".to_string(),
        model: "claude-fallback".to_string(),
        api_key: None,
        api_key_env: Some(env_var.to_string()),
        api_keys: Vec::new(),
        temperature: None,
        max_tokens: None,
        base_url: None,
    });
    LlmClient::new(&config).await.expect("client must build")
}

#[tokio::test]
async fn rate_limited_primary_restores_after_cooldown_expires() {
    let mut client = make_failover_restore_client("BORG_TEST_PRIMARY_RESTORE_RL").await;

    client.record_provider_failure(FailoverReason::RateLimit, None);
    assert!(client.try_failover_provider(), "failover must switch slots");
    assert_eq!(client.active_slot_index, 0);
    assert_eq!(client.provider, Provider::Anthropic);
    assert_eq!(client.llm_config.model, "claude-fallback");
    assert!(
        client.primary_snapshot.is_some(),
        "leaving the primary must snapshot its identity"
    );

    // Cooldown still active: no restore.
    assert!(
        !client.maybe_restore_primary(),
        "primary must stay parked while its cooldown is active"
    );
    assert_eq!(client.active_slot_index, 0);

    // Expire the cooldown and restore.
    client.primary_cooldown.cooldown_until = Some(std::time::Instant::now());
    assert!(
        client.maybe_restore_primary(),
        "primary must be restored once its cooldown expires"
    );
    assert_eq!(client.active_slot_index, usize::MAX);
    assert_eq!(client.provider, Provider::OpenAi);
    assert_eq!(client.llm_config.model, "gpt-primary");
    assert!(
        client.primary_snapshot.is_none(),
        "restore must consume the snapshot"
    );
}

/// A primary that answered `Retry-After: 600` must stay parked until the
/// server-advised deadline, not the 60s transient backoff.
#[tokio::test]
async fn primary_is_not_restored_before_server_retry_after() {
    let mut client = make_failover_restore_client("BORG_TEST_PRIMARY_RESTORE_RETRY_AFTER").await;

    let before = std::time::Instant::now();
    client.record_provider_failure(FailoverReason::RateLimit, Some(Duration::from_secs(600)));
    assert!(client.try_failover_provider());

    let until = client
        .primary_cooldown
        .cooldown_until
        .expect("cooldown set");
    assert!(
        until >= before + Duration::from_secs(600),
        "cooldown must reach the server deadline, got {:?}",
        until - before
    );

    // The default 60s backoff alone would allow a probe once it lapses; the
    // advised 600s deadline keeps the primary parked.
    assert!(
        !client.maybe_restore_primary(),
        "primary must not be probed before its Retry-After deadline"
    );
    assert_eq!(client.active_slot_index, 0);
    assert_eq!(client.provider, Provider::Anthropic);
}

#[tokio::test]
async fn auth_failed_primary_is_never_restored() {
    let mut client = make_failover_restore_client("BORG_TEST_PRIMARY_RESTORE_AUTH").await;

    client.record_provider_failure(FailoverReason::AuthPermanent, None);
    assert!(client.try_failover_provider());

    // Even with the cooldown long expired, an auth-dead primary stays parked.
    client.primary_cooldown.cooldown_until = Some(std::time::Instant::now());
    assert!(
        !client.maybe_restore_primary(),
        "auth-class primary failures must not be probed again"
    );
    assert_eq!(client.active_slot_index, 0);
    assert_eq!(client.provider, Provider::Anthropic);
}

#[tokio::test]
async fn restored_primary_refailure_reparks_with_fresh_cooldown() {
    let mut client = make_failover_restore_client("BORG_TEST_PRIMARY_RESTORE_REFAIL").await;

    client.record_provider_failure(FailoverReason::RateLimit, None);
    assert!(client.try_failover_provider());
    client.primary_cooldown.cooldown_until = Some(std::time::Instant::now());
    assert!(client.maybe_restore_primary());

    // The probe fails again: the primary re-parks and the snapshot is
    // re-captured for the next restore cycle.
    client.record_provider_failure(FailoverReason::RateLimit, None);
    assert!(client.try_failover_provider());
    assert_eq!(client.active_slot_index, 0);
    assert!(client.primary_snapshot.is_some());
    assert!(
        client.primary_cooldown.is_active(),
        "a failed probe must re-arm the primary cooldown"
    );
    assert!(!client.maybe_restore_primary());
}

/// One-key credential pool on a throwaway DB with the real system clock, for
/// exercising the reset-aware branch of `maybe_restore_primary` (which
/// compares the pool's recovery clock against wall time).
async fn one_key_system_clock_pool() -> (Arc<credential_pool::CredentialPool>, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("pool.db");
    let db = crate::db::Database::open_at(&db_path).await.expect("db");
    let pool = credential_pool::CredentialPool::load(
        &db,
        &db_path,
        Provider::OpenAi,
        credential_pool::PoolLoadConfig {
            strategy: credential_pool::CredentialStrategy::FillFirst,
            cooldown_secs: 60,
            billing_cooldown_secs: 3600,
            max_concurrent_per_credential: 4,
            proactive_rate_limit_threshold_pct: 0,
            proactive_rate_limit_min_window_secs: 0,
            config_entries: vec![credential_pool::ConfigEntry {
                key: "sk-restore-gate".to_string(),
                label: Some("only".to_string()),
                config_name: "llm.api_keys[0]".to_string(),
            }],
            env_vars: vec![],
            oauth_home: None,
        },
    )
    .await
    .expect("pool loads");
    (pool, dir)
}

#[tokio::test]
async fn primary_restore_defers_while_the_pool_reports_a_future_reset() {
    // A subscription-window 429 can carry a reset hours away. Once the
    // provider-level exponential cooldown (tens of seconds) expires, probing
    // the primary every turn is a guaranteed failure plus two provider
    // switches and two prompt-cache invalidations per turn. The pool's
    // recovery clock knows better and must hold the client on the fallback.
    let mut client = make_failover_restore_client("BORG_TEST_PRIMARY_RESTORE_POOL_GATE").await;
    let (pool, _dir) = one_key_system_clock_pool().await;
    client.primary_pool = Some(pool.clone());

    client.record_provider_failure(FailoverReason::RateLimit, None);
    assert!(client.try_failover_provider(), "failover must switch slots");

    // The provider said "come back in an hour" — bench the only key that long.
    let id = pool.entries()[0].id.clone();
    pool.report_failure(
        &id,
        FailoverReason::RateLimit,
        Some(429),
        Some(std::time::Duration::from_secs(3600)),
        None,
    );

    // The generic provider-level cooldown has expired…
    client.primary_cooldown.cooldown_until = Some(std::time::Instant::now());
    // …but the provider-reported reset still holds the restore.
    assert!(
        !client.maybe_restore_primary(),
        "a provider-reported reset an hour away must keep the client on the fallback"
    );
    assert_eq!(client.active_slot_index, 0);

    // The bench clears (window elapsed / operator reset): restore proceeds.
    pool.reset_cooldowns();
    assert!(
        client.maybe_restore_primary(),
        "once the pool recovers the primary must be restored"
    );
    assert_eq!(client.active_slot_index, usize::MAX);
}

#[tokio::test]
async fn primary_restore_falls_open_when_the_pool_cannot_name_a_recovery_time() {
    // Fail-open guard: the reset-aware gate may only delay a restore when it
    // holds a concrete future timestamp. A pool with no available key but no
    // recovery clock (every entry tombstoned) must not park the client on the
    // fallback forever — the provider-level cooldown stays the sole authority,
    // so recovery is never later than it was before the gate existed.
    let mut client = make_failover_restore_client("BORG_TEST_PRIMARY_RESTORE_POOL_FAILOPEN").await;
    let (pool, _dir) = one_key_system_clock_pool().await;
    client.primary_pool = Some(pool.clone());

    client.record_provider_failure(FailoverReason::RateLimit, None);
    assert!(client.try_failover_provider(), "failover must switch slots");

    // Tombstone the only key: nothing is available, yet there is no reset
    // time to honor.
    let id = pool.entries()[0].id.clone();
    pool.report_failure(&id, FailoverReason::AuthPermanent, Some(403), None, None);
    assert!(!pool.has_available());
    assert_eq!(pool.next_recovery_at_ms(), None);

    client.primary_cooldown.cooldown_until = Some(std::time::Instant::now());
    assert!(
        client.maybe_restore_primary(),
        "an unknowable recovery time must fall open to the provider-level cooldown"
    );
    assert_eq!(client.active_slot_index, usize::MAX);
}

/// Collect every `{"type":"text","text":…}` value anywhere in the Anthropic
/// message array — the shape Anthropic rejects when it is whitespace-only.
fn collect_anthropic_text_blocks(v: &serde_json::Value, out: &mut Vec<String>) {
    match v {
        serde_json::Value::Object(map) => {
            if map.get("type").and_then(|t| t.as_str()) == Some("text") {
                if let Some(s) = map.get("text").and_then(|t| t.as_str()) {
                    out.push(s.to_string());
                }
            }
            for val in map.values() {
                collect_anthropic_text_blocks(val, out);
            }
        }
        serde_json::Value::Array(arr) => {
            for item in arr {
                collect_anthropic_text_blocks(item, out);
            }
        }
        _ => {}
    }
}

#[test]
fn anthropic_messages_never_emit_blank_text_block() {
    // "text content blocks must contain non-whitespace text" — the 400 this
    // guards. Blank user/assistant/tool text and the empty-content repair
    // (which used to push a literal "") were all reachable.
    let mut assistant_with_calls = Message::assistant("  ");
    assistant_with_calls.tool_calls = Some(vec![ToolCall {
        id: "call_1".to_string(),
        call_type: "function".to_string(),
        function: FunctionCall {
            name: "run_shell".to_string(),
            arguments: "{}".to_string(),
        },
    }]);

    let messages = vec![
        Message::user("   "),
        Message::assistant("\n\t"),
        assistant_with_calls,
        Message::tool_result("call_1", "  "),
    ];
    let built = crate::llm::anthropic::build_anthropic_messages(&messages, None);
    let body = serde_json::Value::Array(built);

    let mut texts = Vec::new();
    collect_anthropic_text_blocks(&body, &mut texts);
    for t in &texts {
        assert!(
            !t.trim().is_empty(),
            "blank text block would 400 at Anthropic; body: {body}"
        );
    }

    // A message whose content array is empty is rejected too.
    for msg in body.as_array().unwrap() {
        if let Some(arr) = msg["content"].as_array() {
            assert!(!arr.is_empty(), "empty content array; body: {body}");
        }
    }
}

#[test]
fn sanitize_drops_whitespace_only_user_message() {
    // Whitespace-only content used to pass the `is_empty` guard and reach
    // the wire as a real (rejected) turn.
    let messages = vec![
        Message::user("real question"),
        Message::user("   \n "),
        Message::assistant("answer"),
    ];
    let sanitized = sanitize_openai_messages(&messages);
    assert_eq!(
        sanitized.len(),
        2,
        "the blank user turn must be dropped: {sanitized:?}"
    );
    assert!(sanitized.iter().all(|m| m.text_content() != Some("   \n ")));
}

#[test]
fn sanitize_keeps_whitespace_only_assistant_that_has_tool_calls() {
    // Over-drop guard: blank text alongside a real tool call must keep the
    // turn — dropping it strands the following tool_result.
    let mut assistant = Message::assistant("  ");
    assistant.tool_calls = Some(vec![ToolCall {
        id: "call_9".to_string(),
        call_type: "function".to_string(),
        function: FunctionCall {
            name: "read_memory".to_string(),
            arguments: "{}".to_string(),
        },
    }]);
    let messages = vec![
        Message::user("go"),
        assistant,
        Message::tool_result("call_9", "ok"),
    ];
    let sanitized = sanitize_openai_messages(&messages);
    assert_eq!(sanitized.len(), 3, "got: {sanitized:?}");
    assert_eq!(
        sanitized[1].tool_calls.as_ref().map(|c| c[0].id.as_str()),
        Some("call_9")
    );
}

// ── auxiliary admission gate wiring ───────────────────────────────────────

/// Every aux client must carry the admission limit, and no primary client may.
///
/// Asserted on the field rather than by timing a mock server: the gate's
/// *bounding* behaviour is covered by the semaphore unit tests in
/// `llm::aux_admission`, and what can actually regress here is the wiring —
/// specifically `for_auxiliary`'s four `Self::new(config)` fall-through paths,
/// which a later edit could easily leave untagged.
#[tokio::test]
async fn for_auxiliary_tags_every_return_path_and_new_tags_none() {
    let env_var = "BORG_TEST_AUX_GATE_TAGGING";
    std::env::set_var(env_var, "sk-test");

    let mut base = Config::default();
    base.llm.provider = Some("anthropic".to_string());
    base.llm.api_key_env = env_var.to_string();
    base.llm.model = "claude-sonnet-4".to_string();
    base.llm.aux_max_concurrent = 3;

    // Each row drives a different `for_auxiliary` exit: the no-aux
    // fall-through, the `"auto"` sentinel resolved via the picker, a concrete
    // model, and a provider with no aux default (falls through to primary).
    let mut with_auto = base.clone();
    with_auto.llm.aux = Some(crate::config::AuxLlmConfig {
        model: "auto".to_string(),
        ..Default::default()
    });

    let mut with_concrete = base.clone();
    with_concrete.llm.aux = Some(crate::config::AuxLlmConfig {
        model: "claude-haiku-4-5-20251001".to_string(),
        ..Default::default()
    });

    let mut local_provider = base.clone();
    local_provider.llm.provider = Some("ollama".to_string());
    local_provider.llm.aux = Some(crate::config::AuxLlmConfig {
        model: "auto".to_string(),
        ..Default::default()
    });

    for (label, config) in [
        ("aux unset", base.clone()),
        ("aux auto", with_auto),
        ("aux concrete", with_concrete),
        ("aux on local provider", local_provider),
    ] {
        let client = LlmClient::for_auxiliary(&config)
            .await
            .unwrap_or_else(|e| panic!("{label}: aux client builds: {e}"));
        assert_eq!(
            client.aux_concurrency,
            Some(3),
            "{label}: this return path lost the admission tag, so its requests \
             would bypass llm.aux_max_concurrent"
        );
    }

    // The counterpart contract: interactive turns must never be gated.
    let primary = LlmClient::new(&base).await.expect("primary client builds");
    assert_eq!(
        primary.aux_concurrency, None,
        "primary/interactive clients must not be gated by the aux limit"
    );
}

/// Spec: no `tool_use` id may appear twice in one Anthropic request.
///
/// Anthropic rejects a duplicate id outright, and history deliberately keeps
/// both pairings when a server reuses one id for every call — that is what
/// lets the model see its own tool results at all. The uniqueness the wire
/// requires therefore has to be restored at the transport, by renaming the
/// later pair rather than dropping it.
#[test]
fn an_anthropic_request_never_carries_a_repeated_tool_use_id() {
    use crate::types::{FunctionCall, Message, Role, ToolCall};

    let call = |args: &str| ToolCall {
        id: "same".to_string(),
        call_type: "function".to_string(),
        function: FunctionCall {
            name: "run_shell".to_string(),
            arguments: args.to_string(),
        },
    };
    let assistant_with = |tc: ToolCall| Message {
        role: Role::Assistant,
        content: None,
        tool_calls: Some(vec![tc]),
        tool_call_id: None,
        timestamp: None,
        seq: None,
        reasoning_blocks: None,
        reasoning_content: None,
        reasoning_model: None,
    };
    let messages = vec![
        Message::user("do a two-step task"),
        assistant_with(call(r#"{"command":"step one"}"#)),
        Message::tool_result("same", "one done"),
        assistant_with(call(r#"{"command":"step two"}"#)),
        Message::tool_result("same", "two done"),
    ];

    // Go through the real request builder, not the repair helper directly:
    // the property under test is that the *transport* restores uniqueness,
    // and calling the helper here would pass even if it were never wired in.
    let llm_config = crate::config::LlmConfig::default();
    let request = transport::TransportRequest {
        provider: crate::llm::Provider::Anthropic,
        model: "claude-sonnet-5",
        messages: &messages,
        tools: None,
        llm_config: &llm_config,
        prompt_cache_key: None,
        system_stable_prefix: None,
        cost_events_enabled: false,
    };
    let body = transport::build_anthropic_request(&request, false);
    let wire = serde_json::to_string(&body).expect("serializable");
    let built: Vec<serde_json::Value> = body
        .get("messages")
        .and_then(|m| m.as_array())
        .cloned()
        .expect("request carries messages");

    let mut ids: Vec<String> = Vec::new();
    for m in &built {
        let Some(blocks) = m.get("content").and_then(|c| c.as_array()) else {
            continue;
        };
        for b in blocks {
            if b.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
                ids.push(
                    b.get("id")
                        .and_then(|i| i.as_str())
                        .unwrap_or_default()
                        .to_string(),
                );
            }
        }
    }
    assert_eq!(ids.len(), 2, "both calls must survive; wire: {wire}");
    assert_ne!(
        ids[0], ids[1],
        "a repeated tool_use id is rejected by the provider; wire: {wire}"
    );

    // And each result must still answer its own call, or the model reads the
    // second step's output as the first step's.
    let mut result_ids: Vec<String> = Vec::new();
    for m in &built {
        let Some(blocks) = m.get("content").and_then(|c| c.as_array()) else {
            continue;
        };
        for b in blocks {
            if b.get("type").and_then(|t| t.as_str()) == Some("tool_result") {
                result_ids.push(
                    b.get("tool_use_id")
                        .and_then(|i| i.as_str())
                        .unwrap_or_default()
                        .to_string(),
                );
            }
        }
    }
    assert_eq!(result_ids, ids, "each result must pair with its own call");
}

/// Sets an environment variable and puts the previous value back on drop —
/// including when a test unwinds, which a hand-rolled restore at the end of the
/// body does not.
struct RestoreEnv {
    name: &'static str,
    previous: Option<String>,
}

impl RestoreEnv {
    fn set(name: &'static str, value: &str) -> Self {
        let previous = std::env::var(name).ok();
        std::env::set_var(name, value);
        Self { name, previous }
    }
}

impl Drop for RestoreEnv {
    fn drop(&mut self) {
        match self.previous.take() {
            Some(value) => std::env::set_var(self.name, value),
            None => std::env::remove_var(self.name),
        }
    }
}

#[tokio::test]
// The lock has to span the await: the value under test is read inside
// `resolve_fallback_slot`, so releasing it first would reopen the race it
// exists to close. Same trade-off as the bedrock credential tests.
#[allow(clippy::await_holding_lock)]
async fn custom_fallback_slot_never_borrows_the_default_provider_key() {
    // A `custom` endpoint is a URL the operator typed — a self-hosted llama.cpp,
    // an LM Studio, a colleague's box. Handing it the built-in
    // `OPENROUTER_API_KEY` default as a bearer credential would mail an
    // unrelated provider's key to that address. `Config::resolve_provider`
    // already refuses this for the primary provider; the fallback slot must
    // refuse it too, both when the name is merely defaulted and when it is
    // spelled out (the two are indistinguishable once resolved).
    //
    // A working key on a *named* provider is the control: the rule has to be
    // specific to `custom`, or every fallback slot would stop authenticating.
    // The variable under test *has* to be the real default: the property is
    // that the built-in default is not borrowed, so a unique per-test name
    // (this file's usual convention) cannot express it. That makes the shared
    // lock and the restoring guard load-bearing rather than tidiness — without
    // the guard, an assertion failure below would leak
    // "sk-borrowed-by-mistake" into every later test in the process.
    let _env = crate::constants::default_llm_api_key_env_lock();
    let key_var = crate::constants::DEFAULT_LLM_API_KEY_ENV;
    let _restore = RestoreEnv::set(key_var, "sk-borrowed-by-mistake");

    let config = Config::default();
    let cases: [(&str, Option<&str>, bool); 3] = [
        // (provider, explicit api_key_env, expect a key)
        ("custom", None, false),
        ("custom", Some(key_var), false),
        ("openrouter", None, true),
    ];

    for (provider, api_key_env, expect_key) in cases {
        let fb = crate::config::LlmFallback {
            provider: provider.to_string(),
            model: "some-model".to_string(),
            api_key: None,
            api_key_env: api_key_env.map(str::to_string),
            api_keys: Vec::new(),
            temperature: None,
            max_tokens: None,
            base_url: None,
        };
        // A `custom` slot with nothing to authenticate with is *refused*
        // outright rather than silently falling back, so a rejected slot counts
        // as "did not borrow the key" — the property under test either way.
        let borrowed = LlmClient::resolve_fallback_slot(&fb, &config)
            .await
            .is_ok_and(|slot| slot.keys.iter().any(|k| k == "sk-borrowed-by-mistake"));
        assert_eq!(
            borrowed, expect_key,
            "provider={provider} api_key_env={api_key_env:?}"
        );
    }
}

#[test]
fn same_origin_compares_scheme_host_and_effective_port() {
    let cases = [
        (
            "https://api.openai.com/v1/x",
            "https://API.openai.com:443/y",
            true,
        ),
        ("http://h.example/a", "http://h.example:80/b", true),
        (
            "https://api.openai.com/v1",
            "http://api.openai.com:8080/v1",
            false,
        ),
        (
            "https://api.openai.com/v1",
            "https://api.openai.com:8443/v1",
            false,
        ),
        (
            "https://api.openai.com/v1",
            "http://api.openai.com/v1",
            false,
        ),
        (
            "https://api.openai.com/v1",
            "https://evil.example/v1",
            false,
        ),
        ("https://api.openai.com/v1", "not a url", false),
        ("", "", false),
    ];
    for (a, b, expected) in cases {
        assert_eq!(super::same_origin(a, b), expected, "{a} vs {b}");
    }
}

#[tokio::test]
async fn for_auxiliary_same_provider_keeps_main_key_only_on_main_origin() {
    // Regression: an aux block on the primary's provider that named its own
    // `base_url` but no key inherited the primary's credentials and sent them
    // to the new URL — a different port or host received the main API key.
    let main_env = "BORG_TEST_AUX_ORIGIN_MAIN_KEY";
    let aux_env = "BORG_TEST_AUX_ORIGIN_OWN_KEY";
    std::env::set_var(main_env, "sk-main-origin-key");
    std::env::set_var(aux_env, "sk-aux-own-key");

    // (provider, main base_url, aux base_url, aux api_key_env, expected key;
    //  None = the aux client must not build with any key)
    type Case<'a> = (&'a str, Option<&'a str>, &'a str, &'a str, Option<&'a str>);
    let cases: [Case; 7] = [
        // Azure primary with no base_url talks to the resource URL composed
        // from `llm.azure_*`; an aux on that resource keeps the key.
        (
            "azure-openai",
            None,
            "https://my-resource.openai.azure.com/openai/deployments/mini/chat/completions?api-version=2024-10-21",
            "",
            Some("sk-main-origin-key"),
        ),
        // Azure aux on another resource: the main key is not sent there.
        (
            "azure-openai",
            None,
            "https://other-resource.openai.azure.com/openai/deployments/mini/chat/completions",
            "",
            None,
        ),
        // Default primary origin, aux on another port: no key.
        ("openai", None, "http://api.openai.com:8080/v1", "", None),
        // Identical origin to the provider default: key kept.
        (
            "openai",
            None,
            "https://api.openai.com/v1/chat/completions",
            "",
            Some("sk-main-origin-key"),
        ),
        // Identical origin to a configured primary base_url: key kept.
        (
            "openai",
            Some("https://proxy.example/v1"),
            "https://PROXY.example:443/other",
            "",
            Some("sk-main-origin-key"),
        ),
        // Aux brings its own key for its own endpoint: that key is used.
        (
            "openai",
            None,
            "https://elsewhere.example/v1",
            aux_env,
            Some("sk-aux-own-key"),
        ),
        // Keyless-capable custom endpoint: builds, but with no inherited key.
        (
            "custom",
            Some("https://a.example/v1"),
            "https://b.example/v1",
            "",
            Some(""),
        ),
    ];
    for (provider, main_url, aux_url, aux_key_env, expected) in cases {
        let mut config = Config::default();
        config.llm.provider = Some(provider.to_string());
        config.llm.api_key_env = main_env.to_string();
        config.llm.model = "gpt-5".to_string();
        config.llm.base_url = main_url.map(str::to_string);
        // Only read by the Azure rows.
        config.llm.azure_resource = Some("my-resource".to_string());
        config.llm.azure_deployment = Some("main".to_string());
        config.llm.azure_api_version = Some("2024-10-21".to_string());
        config.llm.aux = Some(crate::config::llm::AuxLlmConfig {
            provider: Some(provider.to_string()),
            model: "gpt-5-mini".to_string(),
            api_key_env: aux_key_env.to_string(),
            base_url: Some(aux_url.to_string()),
            ..Default::default()
        });
        let built = LlmClient::for_auxiliary(&config).await;
        let label = format!("provider={provider} main={main_url:?} aux={aux_url}");
        match expected {
            None => {
                let err = built
                    .err()
                    .unwrap_or_else(|| panic!("{label}: must refuse"));
                assert!(
                    !err.to_string().contains("sk-main-origin-key"),
                    "{label}: {err}"
                );
            }
            Some(key) => {
                let client = built.unwrap_or_else(|e| panic!("{label}: {e}"));
                assert_eq!(client.api_key, key, "{label}");
                assert_eq!(
                    client.llm_config.base_url.as_deref(),
                    Some(aux_url),
                    "{label}"
                );
            }
        }
    }

    std::env::remove_var(main_env);
    std::env::remove_var(aux_env);
}

#[tokio::test]
// Holds the default-env lock across the await; see the test above.
#[allow(clippy::await_holding_lock)]
async fn fallback_slot_default_env_key_only_reaches_the_provider_origin() {
    // A fallback slot that points at its own `base_url` and names no key used
    // to pick up the provider's default env key and send it to that URL.
    let _env = crate::constants::default_llm_api_key_env_lock();
    let key_var = crate::constants::DEFAULT_LLM_API_KEY_ENV;
    let _restore = RestoreEnv::set(key_var, "sk-or-default-key");

    let config = Config::default();
    // (base_url, explicit api_key_env, expect the default key)
    let cases: [(Option<&str>, Option<&str>, bool); 4] = [
        (None, None, true),
        (Some("https://openrouter.ai/api/v1/other"), None, true),
        (Some("https://proxy.example/v1"), None, false),
        // Naming the variable explicitly is the operator's choice.
        (Some("https://proxy.example/v1"), Some(key_var), true),
    ];
    for (base_url, api_key_env, expect_key) in cases {
        let fb = crate::config::LlmFallback {
            provider: "openrouter".to_string(),
            model: "some-model".to_string(),
            api_key: None,
            api_key_env: api_key_env.map(str::to_string),
            api_keys: Vec::new(),
            temperature: None,
            max_tokens: None,
            base_url: base_url.map(str::to_string),
        };
        let got_key = LlmClient::resolve_fallback_slot(&fb, &config)
            .await
            .is_ok_and(|slot| slot.keys.iter().any(|k| k == "sk-or-default-key"));
        assert_eq!(
            got_key, expect_key,
            "base_url={base_url:?} api_key_env={api_key_env:?}"
        );
    }

    // Azure has no static default URL: its origin is the resource URL that
    // `llm.azure_*` composes. The key is stored as a credential (not the
    // shared `AZURE_OPENAI_API_KEY` env var) so no other test races on it.
    let azure_key_env = "BORG_TEST_FB_AZURE_RESOURCE_KEY";
    let _azure_key = RestoreEnv::set(azure_key_env, "azure-resource-key");
    let mut config = Config::default();
    config.llm.azure_resource = Some("my-resource".to_string());
    config.llm.azure_deployment = Some("main".to_string());
    config.llm.azure_api_version = Some("2024-10-21".to_string());
    config.credentials.insert(
        "AZURE_OPENAI_API_KEY".to_string(),
        crate::config::CredentialValue::EnvVar(azure_key_env.to_string()),
    );
    let azure_cases = [
        (
            "https://my-resource.openai.azure.com/openai/deployments/fb/chat/completions",
            true,
        ),
        (
            "https://other-resource.openai.azure.com/openai/deployments/fb/chat/completions",
            false,
        ),
    ];
    for (base_url, expect_key) in azure_cases {
        let fb = crate::config::LlmFallback {
            provider: "azure-openai".to_string(),
            model: "fb".to_string(),
            api_key: None,
            api_key_env: None,
            api_keys: Vec::new(),
            temperature: None,
            max_tokens: None,
            base_url: Some(base_url.to_string()),
        };
        let slot = LlmClient::resolve_fallback_slot(&fb, &config)
            .await
            .unwrap_or_else(|e| panic!("{base_url}: {e}"));
        assert_eq!(
            slot.keys.iter().any(|k| k == "azure-resource-key"),
            expect_key,
            "{base_url}"
        );
    }
}

// ── stream_options compatibility resend ──

/// An OpenAI-compatible endpoint that refuses unknown request keys: 400 on
/// any body carrying `stream_options`, a normal SSE stream otherwise.
async fn stream_options_rejecting_server() -> wiremock::MockServer {
    use wiremock::matchers::{body_string_contains, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(body_string_contains("stream_options"))
        .respond_with(ResponseTemplate::new(400).set_body_string(
            r#"{"error":{"message":"Unrecognized request argument supplied: stream_options","type":"invalid_request_error"}}"#,
        ))
        .with_priority(1)
        .mount(&server)
        .await;
    let sse = concat!(
        "data: {\"id\":\"chatcmpl-1\",\"choices\":[{\"delta\":{\"content\":\"hello\"}}]}\n\n",
        "data: [DONE]\n\n",
    );
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse),
        )
        .mount(&server)
        .await;
    server
}

fn request_bodies(requests: &[wiremock::Request]) -> Vec<serde_json::Value> {
    requests
        .iter()
        .map(|r| serde_json::from_slice(&r.body).expect("request body is JSON"))
        .collect()
}

#[tokio::test]
async fn stream_options_rejection_resends_once_without_the_field_and_remembers() {
    let server = stream_options_rejecting_server().await;

    let mut config = Config::default();
    config.llm.provider = Some("custom".to_string());
    config.llm.model = "test-model".to_string();
    config.llm.base_url = Some(format!("{}/v1/chat/completions", server.uri()));
    // Zero transient budget: if the compat resend were implemented as an
    // ordinary retry, the call would fail here instead of succeeding.
    config.llm.max_retries = 0;
    let mut client = LlmClient::new(&config).await.expect("client builds");

    let message = tokio::time::timeout(
        Duration::from_secs(10),
        client.chat_streamed(&[Message::user("hi")]),
    )
    .await
    .expect("stream completes promptly")
    .expect("the resend without stream_options must succeed");
    assert_eq!(message.text_content(), Some("hello"));

    let bodies = request_bodies(&server.received_requests().await.expect("recording on"));
    assert_eq!(bodies.len(), 2, "exactly one rejected send plus one resend");
    assert_eq!(
        bodies[0]["stream_options"]["include_usage"], true,
        "the first request carries the field by default"
    );
    assert!(
        bodies[1].get("stream_options").is_none(),
        "the resend drops the field"
    );
    assert_eq!(bodies[1]["stream"], true, "the resend is still a stream");

    // A later call on the same client never sends the field again — no
    // second rejected round trip.
    client
        .chat_streamed(&[Message::user("again")])
        .await
        .expect("second call succeeds");
    let bodies = request_bodies(&server.received_requests().await.expect("recording on"));
    assert_eq!(bodies.len(), 3, "the second call is a single request");
    assert!(bodies[2].get("stream_options").is_none());
}

#[tokio::test]
async fn a_400_that_does_not_name_stream_options_is_not_resent() {
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(400).set_body_string(
            r#"{"error":{"message":"Invalid request: messages must be a non-empty array"}}"#,
        ))
        .mount(&server)
        .await;

    let mut config = Config::default();
    config.llm.provider = Some("custom".to_string());
    config.llm.model = "test-model".to_string();
    config.llm.base_url = Some(format!("{}/v1/chat/completions", server.uri()));
    config.llm.max_retries = 0;
    let mut client = LlmClient::new(&config).await.expect("client builds");

    client
        .chat_streamed(&[Message::user("hi")])
        .await
        .expect_err("a generic 400 still fails the call");
    let requests = server.received_requests().await.expect("recording on");
    assert_eq!(
        requests.len(),
        1,
        "an unrelated 400 must not trigger the compat resend"
    );
}

// ── Response id + serving upstream on UsageData ──

#[test]
fn openai_stream_usage_carries_response_id_and_upstream_from_the_first_chunk() {
    // OpenRouter stamps `id` and `provider` on every chunk but usage only
    // on the last; the state must carry them across the stream.
    use crate::sse::SseAction;
    let mut state = transport::OpenAiStreamState::default();
    let first = r#"data: {"id":"gen-abc123","provider":"Anthropic","choices":[{"delta":{"content":"hi"}}]}"#;
    let action =
        transport::parse_openai_sse_line(first, "openrouter", "anthropic/claude", &mut state);
    assert!(matches!(action, SseAction::Emit(_)));

    // The usage tail chunk omits both fields.
    let tail = r#"data: {"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":2,"total_tokens":12,"cache_creation_input_tokens":7}}"#;
    let action =
        transport::parse_openai_sse_line(tail, "openrouter", "anthropic/claude", &mut state);
    let events = match action {
        SseAction::Emit(ev) | SseAction::Done(ev) => ev,
        _ => panic!("expected usage emit"),
    };
    let usage = events
        .into_iter()
        .find_map(|e| match e {
            StreamEvent::Usage(u) => Some(u),
            _ => None,
        })
        .expect("usage event");
    assert_eq!(usage.response_id.as_deref(), Some("gen-abc123"));
    assert_eq!(usage.upstream.as_deref(), Some("Anthropic"));
    assert_eq!(usage.cache_creation_tokens, 7);
}

#[test]
fn openai_stream_usage_without_id_or_provider_reports_none_not_empty() {
    let cases = [
        (
            "fields absent",
            r#"data: {"choices":[],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}"#,
        ),
        (
            "fields empty",
            r#"data: {"id":"","provider":"  ","choices":[],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}"#,
        ),
    ];
    for (name, line) in cases {
        let usage = parse_openrouter_usage_from_sse(line);
        assert_eq!(usage.response_id, None, "{name}: response_id");
        assert_eq!(usage.upstream, None, "{name}: upstream");
    }
}

/// A provider error that arrives inside a 200 body must surface the
/// provider's own message, classified like a streamed error frame, and a
/// relay that wraps the choices in `data` must still parse. Before this,
/// every one of these shapes was the generic fatal "No response from LLM".
#[test]
fn in_body_provider_errors_and_data_wrapped_choices_parse() {
    let transport = transport::openai_compat_transport();
    let parse = |body: serde_json::Value| transport.parse_chat_response(body, "openai", "m");

    let quota = parse(serde_json::json!({
        "error": {"message": "You exceeded your current quota", "type": "insufficient_quota"}
    }))
    .err()
    .expect("an error body is not a response");
    let quota = quota
        .downcast::<LlmError>()
        .expect("the error carries the classification");
    assert_eq!(quota.reason(), FailoverReason::Billing);
    assert!(!quota.is_retryable());
    assert!(
        quota.to_string().contains("exceeded your current quota"),
        "the provider's message must reach the caller: {quota}"
    );

    let wrapped = parse(serde_json::json!({
        "success": false,
        "data": {"error": {"message": "Rate limit reached", "code": "rate_limit_exceeded"}}
    }))
    .err()
    .expect("a success:false envelope is not a response");
    let wrapped = wrapped
        .downcast::<LlmError>()
        .expect("the error carries the classification");
    assert_eq!(wrapped.reason(), FailoverReason::RateLimit);
    assert!(wrapped.is_retryable());
    assert!(wrapped.to_string().contains("Rate limit reached"));

    let data_wrapped = parse(serde_json::json!({
        "data": {
            "choices": [{"message": {"role": "assistant", "content": "ok"}}],
            "usage": {"prompt_tokens": 5, "completion_tokens": 1, "total_tokens": 6}
        }
    }))
    .expect("data-wrapped choices are a response");
    assert_eq!(data_wrapped.message.text_content(), Some("ok"));
    assert_eq!(data_wrapped.usage.map(|u| u.total_tokens), Some(6));

    // A null error next to real choices is the normal OpenAI shape.
    let with_null_error = parse(serde_json::json!({
        "error": null,
        "choices": [{"message": {"role": "assistant", "content": "fine"}}]
    }))
    .expect("error: null is not an error");
    assert_eq!(with_null_error.message.text_content(), Some("fine"));
}

#[test]
fn openai_chat_response_carries_response_id_and_upstream() {
    let with = serde_json::json!({
        "id": "chatcmpl-9xyz",
        "provider": "DeepInfra",
        "choices": [{"message": {"role": "assistant", "content": "ok"}}],
        "usage": {"prompt_tokens": 5, "completion_tokens": 1, "total_tokens": 6}
    });
    let parsed = transport::openai_compat_transport()
        .parse_chat_response(with, "openrouter", "m")
        .expect("parses");
    let usage = parsed.usage.expect("usage present");
    assert_eq!(usage.response_id.as_deref(), Some("chatcmpl-9xyz"));
    assert_eq!(usage.upstream.as_deref(), Some("DeepInfra"));

    let without = serde_json::json!({
        "choices": [{"message": {"role": "assistant", "content": "ok"}}],
        "usage": {"prompt_tokens": 5, "completion_tokens": 1, "total_tokens": 6}
    });
    let parsed = transport::openai_compat_transport()
        .parse_chat_response(without, "openai", "m")
        .expect("parses");
    let usage = parsed.usage.expect("usage present");
    assert_eq!(usage.response_id, None);
    assert_eq!(usage.upstream, None);
}

#[test]
fn anthropic_usage_carries_message_id_from_message_start() {
    use crate::sse::SseAction;
    let mut tool_idx = 0usize;
    let mut is_tool = false;
    let mut is_thinking = false;
    let mut pending_input = 0u64;
    let mut pending_cached = 0u64;
    let mut pending_created = 0u64;
    let mut pending_id: Option<String> = None;

    let start = r#"data: {"type":"message_start","message":{"id":"msg_01ABC","usage":{"input_tokens":3,"output_tokens":0}}}"#;
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
        &mut pending_id,
        &mut None,
    );
    let delta = r#"data: {"type":"message_delta","usage":{"output_tokens":4},"delta":{"stop_reason":"end_turn"}}"#;
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
        &mut pending_id,
        &mut None,
    );
    let events = match action {
        SseAction::Done(ev) | SseAction::Emit(ev) => ev,
        _ => panic!("expected emit/done"),
    };
    let usage = events
        .into_iter()
        .find_map(|e| match e {
            StreamEvent::Usage(u) => Some(u),
            _ => None,
        })
        .expect("usage event");
    assert_eq!(usage.response_id.as_deref(), Some("msg_01ABC"));
    assert_eq!(usage.upstream, None, "first-party wire names no upstream");
}

#[test]
fn anthropic_chat_response_carries_message_id() {
    let resp = serde_json::json!({
        "id": "msg_02DEF",
        "content": [{"type": "text", "text": "ok"}],
        "usage": {"input_tokens": 2, "output_tokens": 1}
    });
    let parsed = transport::anthropic_transport()
        .parse_chat_response(resp, "anthropic", "claude")
        .expect("parses");
    assert_eq!(
        parsed.usage.expect("usage").response_id.as_deref(),
        Some("msg_02DEF")
    );

    let no_id = serde_json::json!({
        "content": [{"type": "text", "text": "ok"}],
        "usage": {"input_tokens": 2, "output_tokens": 1}
    });
    let parsed = transport::anthropic_transport()
        .parse_chat_response(no_id, "anthropic", "claude")
        .expect("parses");
    assert_eq!(parsed.usage.expect("usage").response_id, None);
}

// ── OpenAI-compatible in-stream error frames ──
//
// An aggregator relays an upstream failure as an error object inside an
// HTTP 200 stream. The frame must end the call as a classified error, so
// key rotation and fallback see the real cause.

#[test]
fn openai_sse_error_frame_is_classified_and_never_continues() {
    use crate::sse::SseAction;

    struct Case {
        name: &'static str,
        line: &'static str,
        retryable: bool,
        reason: FailoverReason,
        status: Option<u16>,
    }
    let cases = [
        Case {
            name: "numeric 429",
            line: r#"data: {"error":{"code":429,"message":"Rate limit exceeded"}}"#,
            retryable: true,
            reason: FailoverReason::RateLimit,
            status: Some(429),
        },
        Case {
            name: "numeric 403",
            line: r#"data: {"error":{"code":403,"message":"Key disabled"}}"#,
            retryable: false,
            reason: FailoverReason::AuthPermanent,
            status: Some(403),
        },
        Case {
            name: "status_code field wins over a string code",
            line: r#"data: {"error":{"code":"upstream_error","status_code":503,"message":"no capacity"}}"#,
            retryable: true,
            reason: FailoverReason::Overloaded,
            status: Some(503),
        },
        Case {
            name: "code 200 with success false is not a status",
            line: r#"data: {"code":200,"success":false,"error":{"code":200,"message":"something broke"}}"#,
            retryable: true,
            reason: FailoverReason::Unknown,
            status: None,
        },
        Case {
            name: "an out-of-range first field does not hide a valid later status",
            line: r#"data: {"error":{"status_code":200,"code":429,"message":"Rate limit exceeded"}}"#,
            retryable: true,
            reason: FailoverReason::RateLimit,
            status: Some(429),
        },
        Case {
            name: "string code with a disconnect message",
            line: r#"data: {"error":{"code":"server_error","message":"Provider disconnected"}}"#,
            retryable: true,
            reason: FailoverReason::Unknown,
            status: None,
        },
        Case {
            name: "account block text without a status",
            line: r#"data: {"error":{"message":"Your account is blocked for a previous policy violation"}}"#,
            retryable: false,
            reason: FailoverReason::AuthPermanent,
            status: None,
        },
        Case {
            name: "finish_reason error without an error object",
            line: r#"data: {"choices":[{"index":0,"delta":{"content":"par"},"finish_reason":"error"}]}"#,
            retryable: true,
            reason: FailoverReason::Unknown,
            status: None,
        },
    ];

    for case in cases {
        let action = transport::parse_openai_sse_line(
            case.line,
            "openrouter",
            "openai/gpt-4o-mini",
            &mut Default::default(),
        );
        let SseAction::Error(err) = action else {
            panic!("{}: an error frame must give SseAction::Error", case.name);
        };
        assert_eq!(err.is_retryable(), case.retryable, "{}", case.name);
        assert_eq!(err.reason(), case.reason, "{}", case.name);
        assert_eq!(err.status_code(), case.status, "{}", case.name);
    }
}

#[test]
fn openai_sse_error_frame_redacts_secrets_in_the_message() {
    use crate::sse::SseAction;
    let line = r#"data: {"error":{"code":"server_error","message":"bad token ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijk"}}"#;
    let action = transport::parse_openai_sse_line(line, "openrouter", "m", &mut Default::default());
    let SseAction::Error(err) = action else {
        panic!("expected an error");
    };
    assert!(
        !err.to_string()
            .contains("ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijk"),
        "the key must not reach the error text: {err}"
    );
}

#[test]
fn openai_sse_content_chunk_with_null_error_still_emits() {
    use crate::sse::SseAction;
    let line = r#"data: {"error":null,"choices":[{"index":0,"delta":{"content":"hi"},"finish_reason":null}]}"#;
    let action =
        transport::parse_openai_sse_line(line, "openai", "gpt-4o", &mut Default::default());
    match action {
        SseAction::Emit(events) => {
            assert!(matches!(events.as_slice(), [StreamEvent::TextDelta(t)] if t == "hi"));
        }
        _ => panic!("a normal content chunk must emit its text"),
    }
}

// ── A clean close before the completion marker ──
//
// A proxy or load balancer can close the socket in the middle of an answer.
// The byte stream then ends cleanly. Only the completion marker of the wire
// proves that the response is complete.

/// One AWS event-stream frame with no headers. The reader does not check
/// the two CRC fields.
fn bedrock_frame(payload: &str) -> Vec<u8> {
    let total = 16 + payload.len();
    let mut frame = Vec::with_capacity(total);
    frame.extend_from_slice(&(total as u32).to_be_bytes());
    frame.extend_from_slice(&0u32.to_be_bytes());
    frame.extend_from_slice(&[0; 4]);
    frame.extend_from_slice(payload.as_bytes());
    frame.extend_from_slice(&[0; 4]);
    frame
}

/// Drive `chunks` through `transport` and return the result with every
/// event that reached the consumer.
async fn drive_fixture(
    transport: &'static dyn transport::Transport,
    chunks: Vec<Vec<u8>>,
) -> (std::result::Result<(), LlmError>, Vec<StreamEvent>) {
    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    let cancel = tokio_util::sync::CancellationToken::new();
    let stream = futures_util::stream::iter(
        chunks
            .into_iter()
            .map(Ok::<Vec<u8>, std::io::Error>)
            .collect::<Vec<_>>(),
    );
    let result =
        LlmClient::drive_transport_stream(transport, stream, &tx, &cancel, 0, "test", "m", |_| {})
            .await;
    drop(tx);
    let mut events = Vec::new();
    while let Some(event) = rx.recv().await {
        events.push(event);
    }
    (result, events)
}

/// Once the parser has seen the completion marker, every byte of the answer
/// is in. A stall or a dropped connection after that point must end the
/// stream as complete: retrying it would request, and pay for, the same
/// answer a second time. The same failure before the marker stays an error.
#[tokio::test(start_paused = true)]
async fn a_stall_after_the_completion_marker_is_a_complete_response() {
    use futures_util::StreamExt;

    const DELTA: &str =
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"the answer\"},\"finish_reason\":null}]}\n\n";
    const FINISH: &str =
        "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n";
    let io_error = || std::io::Error::new(std::io::ErrorKind::ConnectionReset, "reset by peer");

    // (name, chunks before the failure, hang instead of erroring, complete)
    let cases = [
        (
            "read error after finish_reason",
            vec![DELTA, FINISH],
            false,
            true,
        ),
        (
            "chunk timeout after finish_reason",
            vec![DELTA, FINISH],
            true,
            true,
        ),
        ("read error before finish_reason", vec![DELTA], false, false),
        (
            "chunk timeout before finish_reason",
            vec![DELTA],
            true,
            false,
        ),
    ];
    for (name, chunks, hang, complete) in cases {
        let mut items: Vec<Result<Vec<u8>, std::io::Error>> = chunks
            .into_iter()
            .map(|c| Ok(c.as_bytes().to_vec()))
            .collect();
        if !hang {
            items.push(Err(io_error()));
        }
        let head = futures_util::stream::iter(items);
        let tail = if hang {
            futures_util::stream::pending().boxed()
        } else {
            futures_util::stream::empty().boxed()
        };
        let stream = head.chain(tail);

        let (tx, mut rx) = tokio::sync::mpsc::channel(64);
        let cancel = tokio_util::sync::CancellationToken::new();
        let result = LlmClient::drive_transport_stream(
            transport::openai_compat_transport(),
            stream,
            &tx,
            &cancel,
            1,
            "openai",
            "m",
            |_| {},
        )
        .await;
        drop(tx);
        let mut events = Vec::new();
        while let Some(event) = rx.recv().await {
            events.push(event);
        }
        let text: String = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::TextDelta(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "the answer", "{name}: the deltas reach the consumer");
        let done = matches!(events.last(), Some(StreamEvent::Done));
        if complete {
            assert!(
                result.is_ok(),
                "{name}: must be a complete response: {result:?}"
            );
            assert!(done, "{name}: the consumer must get Done, got {events:?}");
        } else {
            let err = result.expect_err(name);
            assert!(
                err.is_retryable(),
                "{name}: a cut before the marker is retried"
            );
            assert!(!done, "{name}: no Done for a cut answer, got {events:?}");
        }
    }
}

#[tokio::test]
async fn clean_close_is_complete_only_after_the_completion_marker() {
    const ANTHROPIC_HEAD: &str = concat!(
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"usage\":{\"input_tokens\":3}}}\n\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"half an ans\"}}\n\n",
    );
    const OPENAI_DELTA: &str =
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"half an ans\"},\"finish_reason\":null}]}\n\n";
    const OPENAI_FINISH: &str =
        "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n";
    const GEMINI_DELTA: &str =
        "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"half an ans\"}]}}]}\n\n";
    const BEDROCK_DELTA: &str =
        r#"{"contentBlockDelta":{"contentBlockIndex":0,"delta":{"text":"half an ans"}}}"#;

    struct Case {
        name: &'static str,
        transport: &'static dyn transport::Transport,
        chunks: Vec<Vec<u8>>,
        complete: bool,
    }
    let text = |parts: &[&str]| vec![parts.concat().into_bytes()];
    let cases = vec![
        Case {
            name: "anthropic without message_stop",
            transport: transport::anthropic_transport(),
            chunks: text(&[ANTHROPIC_HEAD]),
            complete: false,
        },
        Case {
            name: "anthropic with message_stop",
            transport: transport::anthropic_transport(),
            chunks: text(&[ANTHROPIC_HEAD, "data: {\"type\":\"message_stop\"}\n\n"]),
            complete: true,
        },
        Case {
            name: "openai deltas only",
            transport: transport::openai_compat_transport(),
            chunks: text(&[OPENAI_DELTA]),
            complete: false,
        },
        Case {
            name: "openai finish_reason without the [DONE] sentinel",
            transport: transport::openai_compat_transport(),
            chunks: text(&[OPENAI_DELTA, OPENAI_FINISH]),
            complete: true,
        },
        Case {
            name: "openai with the [DONE] sentinel",
            transport: transport::openai_compat_transport(),
            chunks: text(&[OPENAI_DELTA, OPENAI_FINISH, "data: [DONE]\n\n"]),
            complete: true,
        },
        Case {
            name: "gemini without finishReason",
            transport: transport::gemini_transport(),
            chunks: text(&[GEMINI_DELTA]),
            complete: false,
        },
        Case {
            name: "gemini with finishReason",
            transport: transport::gemini_transport(),
            chunks: text(&[
                GEMINI_DELTA,
                "data: {\"candidates\":[{\"content\":{\"parts\":[]},\"finishReason\":\"STOP\"}]}\n\n",
            ]),
            complete: true,
        },
        Case {
            name: "bedrock without messageStop",
            transport: transport::bedrock_transport(),
            chunks: vec![bedrock_frame(BEDROCK_DELTA)],
            complete: false,
        },
        Case {
            name: "bedrock with messageStop",
            transport: transport::bedrock_transport(),
            chunks: vec![
                bedrock_frame(BEDROCK_DELTA),
                bedrock_frame(r#"{"messageStop":{"stopReason":"end_turn"}}"#),
            ],
            complete: true,
        },
    ];

    for case in cases {
        let (result, events) = drive_fixture(case.transport, case.chunks).await;
        let done_count = events
            .iter()
            .filter(|e| matches!(e, StreamEvent::Done))
            .count();
        assert!(
            events
                .iter()
                .any(|e| matches!(e, StreamEvent::TextDelta(t) if t == "half an ans")),
            "{}: the text before the close must reach the consumer",
            case.name
        );
        if case.complete {
            assert!(result.is_ok(), "{}: {:?}", case.name, result.err());
            assert_eq!(done_count, 1, "{}: Done must arrive once", case.name);
        } else {
            let err = result.expect_err(case.name);
            assert!(err.is_retryable(), "{}", case.name);
            assert_eq!(
                err.reason(),
                FailoverReason::ConnectionLost,
                "{}",
                case.name
            );
            assert_eq!(
                done_count, 0,
                "{}: a cut response must not be reported as done",
                case.name
            );
        }
    }
}

// ── Long provider reset windows ──

#[test]
fn only_a_wait_longer_than_the_bound_ends_the_in_turn_retry() {
    let rate_limited = |retry_after| LlmError::Retryable {
        source: anyhow::anyhow!("429"),
        retry_after,
        reason: FailoverReason::RateLimit,
        status: Some(429),
    };
    let cases = [
        (None, false),
        (Some(Duration::from_secs(5)), false),
        (Some(MAX_IN_TURN_RETRY_WAIT), false),
        (Some(MAX_IN_TURN_RETRY_WAIT + Duration::from_secs(1)), true),
        (Some(Duration::from_secs(8 * 3600)), true),
    ];
    for (retry_after, expected) in cases {
        assert_eq!(
            is_long_provider_wait(&rate_limited(retry_after)),
            expected,
            "{retry_after:?}"
        );
    }
}

#[test]
fn the_usage_limit_message_names_the_reset_time_in_plain_words() {
    use chrono::TimeZone;
    let now = chrono::Local
        .with_ymd_and_hms(2026, 3, 10, 9, 15, 0)
        .single()
        .expect("valid local time");

    let cases = [
        // The window rounds up to the minute. The clock time does not.
        (121, "about 3 minutes (at about 09:17)"),
        (3600, "about 1 hour (at about 10:15)"),
        (3660, "about 1 hour 1 minute (at about 10:16)"),
        (
            2 * 3600 + 5 * 60,
            "about 2 hours 5 minutes (at about 11:20)",
        ),
        (20 * 3600, "about 20 hours (at about 05:15 on Mar 11)"),
    ];
    for (secs, expected) in cases {
        let msg = usage_limit_message(Provider::OpenRouter, Duration::from_secs(secs), now);
        assert!(msg.contains(expected), "{secs}s: {msg}");
        assert!(msg.starts_with("openrouter usage limit reached."), "{msg}");
        assert!(
            msg.contains("Add another provider or key, or try again after that."),
            "{msg}"
        );
        assert!(
            !msg.contains(&format!("{secs}")),
            "the message must not show raw seconds: {msg}"
        );
    }
}

#[tokio::test]
async fn failover_skips_a_slot_whose_every_credential_is_benched() {
    use crate::llm::credential_pool::{
        ConfigEntry, CredentialPool, CredentialStrategy, PoolLoadConfig,
    };

    let mut client = make_failover_restore_client("BORG_TEST_FAILOVER_BENCHED_SLOT").await;

    // Give the fallback slot a pool with one key, in its own database.
    let db_dir = tempfile::tempdir().expect("tempdir");
    let db_path = db_dir.path().join("pool.db");
    let db = crate::db::Database::open_at(&db_path)
        .await
        .expect("open pool db");
    let pool = CredentialPool::load(
        &db,
        &db_path,
        Provider::Anthropic,
        PoolLoadConfig {
            strategy: CredentialStrategy::FillFirst,
            cooldown_secs: 60,
            billing_cooldown_secs: 3600,
            max_concurrent_per_credential: 4,
            proactive_rate_limit_threshold_pct: 0,
            proactive_rate_limit_min_window_secs: 0,
            config_entries: vec![ConfigEntry {
                key: "sk-ant-fallback-key".to_string(),
                label: Some("fallback".to_string()),
                config_name: "llm.fallback[0]".to_string(),
            }],
            env_vars: vec![],
            oauth_home: Some(db_dir.path().to_path_buf()),
        },
    )
    .await
    .expect("pool loads");
    client.provider_slots[0].pool = Some(pool.clone());

    // The provider benched the only key for an hour. The slot itself has no
    // provider cooldown.
    let id = pool.acquire_lease(None).expect("lease").id().to_string();
    pool.report_failure(
        &id,
        FailoverReason::RateLimit,
        Some(429),
        Some(Duration::from_secs(3600)),
        None,
    );
    assert!(!pool.has_available(), "precondition: the key is benched");

    assert!(
        !client.try_failover_provider(),
        "a slot that cannot serve must not be selected"
    );
    assert_eq!(client.active_slot_index, usize::MAX);
    assert_eq!(client.provider, Provider::OpenAi);

    // The same slot is selected again when its key is available.
    pool.reset_cooldowns();
    assert!(client.try_failover_provider());
    assert_eq!(client.provider, Provider::Anthropic);
}
