//! End-to-end smoke for the OpenAI-compat HTTP surface mounted by
//! `gateway::GatewayServer`.
//!
//! Real failure modes guarded:
//! 1. A regression that drops `.merge(build_openai_compat_router(...))`
//!    from `crates/gateway/src/server.rs` — exactly the bug shipped to
//!    main before this test existed. Asserted via "POST /v1/chat/completions
//!    returns 200, not 404."
//! 2. A regression that auths every request — asserted via "missing
//!    bearer returns 401, not 200."
//! 3. A regression that breaks the dispatcher → SSE translator —
//!    asserted via the streaming test that joins SSE deltas back into
//!    the seeded text.
//! 4. A regression that drops the `/v1/embeddings` route from the
//!    sub-router — asserted via "POST /v1/embeddings returns a
//!    structured 503, not 404."

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::time::Duration;

use cortex::agent::AgentEvent;

#[path = "ingress_e2e_common.rs"]
mod common;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn openai_compat_chat_completions_round_trip() {
    let harness = common::boot_gateway(true).await;
    let token = common::issue_token("e2e-openai").await;
    // Session key for OpenAI-compat is `<token-label>:` — see
    // `chat_completions` in routes.rs which sets sender_id to the
    // token label and leaves thread_id None.
    common::register_stub_for_session(
        "e2e-openai:",
        vec![
            AgentEvent::TextDelta("hello ".into()),
            AgentEvent::TextDelta("world".into()),
            AgentEvent::TurnComplete,
        ],
    );
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .expect("client");

    // 1. Missing bearer → 401. Reachability check + auth gate guard.
    let no_auth = client
        .post(harness.url("/v1/chat/completions"))
        .json(&serde_json::json!({
            "messages":[{"role":"user","content":"hi"}],
            "model":"borg-default"
        }))
        .send()
        .await
        .expect("send no-auth");
    assert_eq!(
        no_auth.status(),
        reqwest::StatusCode::UNAUTHORIZED,
        "missing bearer must be 401 (mounting + auth gate intact)"
    );

    // 2. Non-streaming round-trip.
    let resp = client
        .post(harness.url("/v1/chat/completions"))
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "messages":[{"role":"user","content":"hi"}],
            "model":"borg-default",
            "stream": false
        }))
        .send()
        .await
        .expect("send non-stream");
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let body: serde_json::Value = resp.json().await.expect("json");
    assert_eq!(body["object"], "chat.completion");
    assert_eq!(body["choices"][0]["message"]["content"], "hello world");
    assert_eq!(body["choices"][0]["finish_reason"], "stop");

    // 3. /v1/models is reachable + auth-gated.
    let models_no_auth = client
        .get(harness.url("/v1/models"))
        .send()
        .await
        .expect("models no-auth");
    assert_eq!(models_no_auth.status(), reqwest::StatusCode::UNAUTHORIZED);

    let models = client
        .get(harness.url("/v1/models"))
        .bearer_auth(&token)
        .send()
        .await
        .expect("models")
        .json::<serde_json::Value>()
        .await
        .expect("json");
    let ids: Vec<String> = models["data"]
        .as_array()
        .expect("data")
        .iter()
        .map(|m| m["id"].as_str().unwrap_or("").to_string())
        .collect();
    assert!(
        ids.contains(&"borg-default".to_string()),
        "models must include borg-default, got: {ids:?}"
    );

    harness.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn openai_compat_streaming_path_emits_done_terminator() {
    // SSE termination is the regression most likely to break clients
    // (ChatBox / Open WebUI hang waiting for `[DONE]` if missing).
    let harness = common::boot_gateway(true).await;
    let token = common::issue_token("e2e-openai-stream").await;
    common::register_stub_for_session(
        "e2e-openai-stream:",
        vec![
            AgentEvent::TextDelta("hi ".into()),
            AgentEvent::TextDelta("there".into()),
            AgentEvent::TurnComplete,
        ],
    );
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .expect("client");

    let resp = client
        .post(harness.url("/v1/chat/completions"))
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "messages":[{"role":"user","content":"x"}],
            "model":"borg-default",
            "stream": true
        }))
        .send()
        .await
        .expect("send stream");
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    assert!(resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .contains("text/event-stream"));
    let body = resp.text().await.expect("body");
    let frames: Vec<&str> = body
        .split("\n\n")
        .filter_map(|f| f.trim().strip_prefix("data: "))
        .collect();
    assert_eq!(
        frames.last().copied(),
        Some("[DONE]"),
        "must end with [DONE]"
    );

    let mut joined = String::new();
    for f in frames.iter().take(frames.len().saturating_sub(1)) {
        let v: serde_json::Value = serde_json::from_str(f).expect("chunk parses");
        if let Some(c) = v["choices"][0]["delta"]["content"].as_str() {
            joined.push_str(c);
        }
    }
    assert_eq!(joined, "hi there");

    harness.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn openai_compat_embeddings_endpoint_round_trip() {
    // The e2e harness runs without an embedding API key configured —
    // so the production dispatcher resolves to `ProviderUnavailable`.
    // We still want to prove three things end-to-end:
    //   1. The route is mounted (regression target: dropping
    //      `.route("/v1/embeddings", post(embeddings))` would surface
    //      as 404 here, not 503).
    //   2. The auth gate runs before the dispatcher (missing bearer →
    //      401, not 503 — no information leak about whether a
    //      provider is configured).
    //   3. The dispatcher → HTTP error mapping carries through to the
    //      real axum app (unconfigured-provider → 503 with the
    //      OpenAI-shaped `error.type = "service_unavailable"` body).
    let harness = common::boot_gateway(true).await;
    let token = common::issue_token("e2e-openai-embed").await;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .expect("client");

    // 1. Missing bearer → 401, never 503/404. Guards mount + auth gate
    //    order.
    let no_auth = client
        .post(harness.url("/v1/embeddings"))
        .json(&serde_json::json!({"model":"borg-embed","input":"hi"}))
        .send()
        .await
        .expect("send no-auth");
    assert_eq!(
        no_auth.status(),
        reqwest::StatusCode::UNAUTHORIZED,
        "missing bearer must be 401 (mount + auth gate intact)"
    );

    // 2. With auth, no provider configured → 503 with OpenAI-shaped
    //    error body. Asserts route mounted AND the dispatcher's
    //    ProviderUnavailable maps through to 503 (not a 500 panic).
    let resp = client
        .post(harness.url("/v1/embeddings"))
        .bearer_auth(&token)
        .json(&serde_json::json!({"model":"borg-embed","input":"hi"}))
        .send()
        .await
        .expect("send with auth");
    assert_eq!(
        resp.status(),
        reqwest::StatusCode::SERVICE_UNAVAILABLE,
        "harness has no embedding key configured → expect 503, not 404 (would mean route missing) or 500 (would mean panic)"
    );
    let body: serde_json::Value = resp.json().await.expect("json");
    assert_eq!(
        body["error"]["type"], "service_unavailable",
        "503 body must carry the OpenAI-shaped error.type so SDK error handlers route correctly"
    );

    harness.shutdown().await;
}
