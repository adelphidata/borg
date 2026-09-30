//! Axum routes for `/v1/chat/completions` and `/v1/models`.

use std::sync::Arc;

use axum::{
    extract::{DefaultBodyLimit, State},
    http::{header, HeaderMap, StatusCode},
    response::{sse::Event, IntoResponse, Response, Sse},
    routing::{get, post},
    Json, Router,
};
use cortex::auth::tokens;
use cortex::constants::GATEWAY_MAX_BODY_SIZE;
use cortex::db::Database;
use cortex::ingress::{IngressGate, IngressMessage, IngressSource, MessageBody, PrincipalId};
use futures::StreamExt;
use uuid::Uuid;

use super::dispatcher::{
    AgentEventChunk, ChatDispatcher, EmbeddingDispatchError, EmbeddingDispatcher,
};
use super::types::{
    ChatCompletionChoice, ChatCompletionChunk, ChatCompletionChunkChoice, ChatCompletionDelta,
    ChatCompletionRequest, ChatCompletionResponse, ChatMessage, EmbeddingData, EmbeddingInput,
    EmbeddingsRequest, EmbeddingsResponse, EmbeddingsUsage, ModelInfo, ModelsList, ToolCall,
    ToolCallDelta, ToolCallFunction, Usage,
};

/// Shared state for the OpenAI-compat routes.
///
/// We open one DB handle per request via the supplied async `db_open`
/// factory. Tests inject a factory backed by a tempfile DB; production
/// uses [`Database::open_with_timeout`].
#[derive(Clone)]
pub struct OpenAiState {
    /// Open a fresh DB handle for the current request.
    pub db_open: crate::DbOpener,
    /// Pre-dispatch gate (rate-limit + sanitize + audit).
    pub gate: Arc<IngressGate>,
    /// Dispatcher into the agent loop.
    pub dispatcher: Arc<dyn ChatDispatcher>,
    /// Dispatcher into the embedding provider for `/v1/embeddings`.
    pub embedding_dispatcher: Arc<dyn EmbeddingDispatcher>,
    /// Model aliases advertised by `/v1/models`.
    pub model_aliases: Arc<Vec<String>>,
}

/// Build the OpenAI-compat sub-router. Caller merges this into the
/// gateway's main axum app via `.merge(openai_compat::router(state))`.
pub fn router(state: OpenAiState) -> Router {
    // The merged outer router applies a body limit but axum 0.8 layers
    // don't propagate backward across `.merge(...)`, so this sub-router
    // owns its own cap. Without this, the `axum::body::Bytes` extractor
    // on `/v1/embeddings` would accept unbounded payloads — see the
    // `embeddings_oversize_body_returns_413` regression test.
    Router::new()
        .route("/v1/chat/completions", post(chat_completions))
        .route("/v1/embeddings", post(embeddings))
        .route("/v1/models", get(list_models))
        .layer(DefaultBodyLimit::max(GATEWAY_MAX_BODY_SIZE))
        .with_state(state)
}

/// Authenticate a request via `Authorization: Bearer <token>`. Returns
/// the principal id on success, a 401 response on failure.
///
/// Opens a fresh DB connection (`Database` is `!Sync` so we can't
/// stash one in shared state). Generic error string matches OpenAI's:
/// clients distinguish "no key" from "wrong key" only by the
/// surrounding flow, not by message — anti-enumeration holds here.
#[allow(clippy::result_large_err)]
async fn authenticate(headers: &HeaderMap, state: &OpenAiState) -> Result<PrincipalId, Response> {
    let db = (state.db_open)()
        .await
        .map_err(|_| auth_error("auth backend unavailable"))?;
    authenticate_with_db(headers, &db).await
}

#[allow(clippy::result_large_err)]
async fn authenticate_with_db(headers: &HeaderMap, db: &Database) -> Result<PrincipalId, Response> {
    let auth = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| auth_error("missing Authorization header"))?;
    let token = auth
        .strip_prefix("Bearer ")
        .ok_or_else(|| auth_error("Authorization header must use Bearer scheme"))?;

    let row = tokens::verify(db, token)
        .await
        .map_err(|_| auth_error("invalid client token"))?;
    Ok(PrincipalId::new(format!("token:{}", row.label)))
}

fn auth_error(msg: &str) -> Response {
    let body = serde_json::json!({
        "error": {
            "message": msg,
            "type": "invalid_request_error",
            "code": "invalid_api_key",
        }
    });
    (StatusCode::UNAUTHORIZED, Json(body)).into_response()
}

/// `POST /v1/chat/completions` — stream + non-stream paths fan out
/// here off `request.stream`.
async fn chat_completions(
    State(state): State<OpenAiState>,
    headers: HeaderMap,
    Json(req): Json<ChatCompletionRequest>,
) -> Response {
    let principal = match authenticate(&headers, &state).await {
        Ok(p) => p,
        Err(resp) => return resp,
    };

    if req.messages.is_empty() {
        return bad_request("`messages` must be non-empty");
    }

    let last_user = req.messages.iter().rev().find(|m| m.role == "user");
    let user_text = last_user
        .and_then(|m| m.content.clone())
        .unwrap_or_default();

    if user_text.trim().is_empty() {
        return bad_request("last user message must contain non-empty `content`");
    }

    // Sender id derives from the token label so multi-tenant clients
    // (one token per app) get isolated session histories.
    let sender_id = principal
        .as_str()
        .strip_prefix("token:")
        .unwrap_or(principal.as_str())
        .to_string();

    let message = IngressMessage {
        source: IngressSource::Http(sender_id.clone()),
        principal,
        sender_id,
        thread_id: None,
        body: MessageBody::text(user_text),
        meta: serde_json::Value::Null,
        received_at: chrono::Utc::now().timestamp(),
    };

    let admitted = match state.gate.admit(message) {
        Ok(a) => a,
        Err(e) => return rejected(e),
    };

    let id = format!("chatcmpl-{}", Uuid::new_v4());
    let model = req.model.clone();
    let stream = state.dispatcher.dispatch(admitted).await;

    if req.stream {
        sse_response(id, model, stream)
    } else {
        accumulate_response(id, model, stream).await
    }
}

/// Non-streaming path: drain the chunk stream, accumulate text and
/// tool calls, return a single JSON response.
async fn accumulate_response(
    id: String,
    model: String,
    mut stream: futures::stream::BoxStream<'static, AgentEventChunk>,
) -> Response {
    let mut text = String::new();
    let mut tool_calls: Vec<ToolCall> = Vec::new();
    let mut finish_reason = "stop";

    while let Some(chunk) = stream.next().await {
        match chunk {
            AgentEventChunk::Text(t) => text.push_str(&t),
            AgentEventChunk::Retract(bytes) => cortex::agent::retract_text_tail(&mut text, bytes),
            AgentEventChunk::ToolCall {
                id: cid,
                name,
                arguments,
            } => {
                tool_calls.push(ToolCall {
                    id: cid,
                    kind: "function".into(),
                    function: ToolCallFunction { name, arguments },
                });
            }
            AgentEventChunk::Done => break,
            AgentEventChunk::Error(e) => {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(&format!("[error] {e}"));
                finish_reason = "stop";
                break;
            }
        }
    }

    let resp = ChatCompletionResponse {
        id,
        object: "chat.completion",
        created: chrono::Utc::now().timestamp(),
        model,
        choices: vec![ChatCompletionChoice {
            index: 0,
            message: ChatMessage {
                role: "assistant".into(),
                content: Some(text),
                tool_call_id: None,
                tool_calls: if tool_calls.is_empty() {
                    None
                } else {
                    Some(tool_calls)
                },
            },
            finish_reason: finish_reason.to_string(),
        }],
        usage: Usage::default(),
    };

    Json(resp).into_response()
}

/// Streaming path: each chunk in the dispatcher stream becomes one
/// `data: {…}\n\n` SSE event, terminated by `data: [DONE]\n\n`.
fn sse_response(
    id: String,
    model: String,
    stream: futures::stream::BoxStream<'static, AgentEventChunk>,
) -> Response {
    let id_for_chunks = id;
    let model_for_chunks = model;
    let mut tool_index_seq: u32 = 0;
    let mut role_emitted = false;

    // Map agent chunks into one or more SSE events, terminating with
    // `[DONE]` per the OpenAI streaming contract.
    let event_stream = async_stream::stream! {
        let mut s = stream;
        while let Some(chunk) = s.next().await {
            match chunk {
                AgentEventChunk::Text(t) => {
                    let mut delta = ChatCompletionDelta {
                        content: Some(t),
                        ..Default::default()
                    };
                    if !role_emitted {
                        delta.role = Some("assistant".into());
                        role_emitted = true;
                    }
                    yield Ok::<_, std::convert::Infallible>(chunk_event(
                        &id_for_chunks,
                        &model_for_chunks,
                        delta,
                        None,
                    ));
                }
                AgentEventChunk::ToolCall { id: cid, name, arguments } => {
                    let idx = tool_index_seq;
                    tool_index_seq += 1;
                    let delta = ChatCompletionDelta {
                        role: if !role_emitted {
                            role_emitted = true;
                            Some("assistant".into())
                        } else {
                            None
                        },
                        content: None,
                        tool_calls: Some(vec![ToolCallDelta {
                            index: idx,
                            id: Some(cid),
                            kind: Some("function".into()),
                            function: Some(ToolCallFunction { name, arguments }),
                        }]),
                    };
                    yield Ok(chunk_event(&id_for_chunks, &model_for_chunks, delta, None));
                }
                AgentEventChunk::Done => {
                    yield Ok(chunk_event(
                        &id_for_chunks,
                        &model_for_chunks,
                        ChatCompletionDelta::default(),
                        Some("stop".into()),
                    ));
                    break;
                }
                // The OpenAI stream format has no way to take back a
                // chunk that went out; the client keeps the text of the
                // failed attempt in front of the full response.
                AgentEventChunk::Retract(_) => {}
                AgentEventChunk::Error(e) => {
                    let delta = ChatCompletionDelta {
                        content: Some(format!("[error] {e}")),
                        ..Default::default()
                    };
                    yield Ok(chunk_event(
                        &id_for_chunks,
                        &model_for_chunks,
                        delta,
                        Some("stop".into()),
                    ));
                    break;
                }
            }
        }
        // OpenAI's terminator — clients that read until `[DONE]` need
        // it even when the underlying stream already emitted a stop.
        yield Ok(Event::default().data("[DONE]"));
    };

    Sse::new(event_stream).into_response()
}

fn chunk_event(
    id: &str,
    model: &str,
    delta: ChatCompletionDelta,
    finish_reason: Option<String>,
) -> Event {
    let chunk = ChatCompletionChunk {
        id: id.to_string(),
        object: "chat.completion.chunk",
        created: chrono::Utc::now().timestamp(),
        model: model.to_string(),
        choices: vec![ChatCompletionChunkChoice {
            index: 0,
            delta,
            finish_reason,
        }],
    };
    let json = serde_json::to_string(&chunk).unwrap_or_default();
    Event::default().data(json)
}

/// `POST /v1/embeddings` — auth-gated, OpenAI-shaped wrapper around
/// the configured `cortex` embedding provider.
///
/// The dispatcher fans each input out as one upstream call, so a
/// stolen token plus a large batch can burn the operator's embedding
/// quota in seconds. Three layers defend against that: the body limit
/// applied by [`router`], the per-batch cap [`MAX_EMBEDDING_BATCH_SIZE`],
/// and the per-principal rate limiter inside [`cortex::ingress::IngressGate`].
async fn embeddings(
    State(state): State<OpenAiState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let principal = match authenticate(&headers, &state).await {
        Ok(p) => p,
        Err(resp) => {
            tracing::warn!("openai_compat embeddings auth rejected");
            return resp;
        }
    };

    // Parse manually so deserialization failures (e.g. `input: [1,2,3]`
    // — numeric token-id arrays we don't support) surface as 400 with
    // an OpenAI-shaped error body rather than axum's default 422 with
    // a plain-text payload. OpenAI SDKs decode `error.message`; the
    // 422 path would break them silently.
    let req: EmbeddingsRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => return bad_request(&format!("invalid request body: {e}")),
    };

    let inputs: Vec<String> = match req.input {
        EmbeddingInput::Single(s) => vec![s],
        EmbeddingInput::Batch(v) => v,
    };
    if inputs.is_empty() {
        return bad_request("`input` must be non-empty");
    }
    // Batch cap MUST run before per-input validation: otherwise an
    // attacker can ship millions of one-byte entries inside the body
    // limit and the per-input loop alone is the DoS surface.
    if inputs.len() > cortex::constants::MAX_EMBEDDING_BATCH_SIZE {
        return bad_request(&format!(
            "`input` batch size {} exceeds limit of {}",
            inputs.len(),
            cortex::constants::MAX_EMBEDDING_BATCH_SIZE
        ));
    }
    // Reject empty/whitespace-only entries individually — OpenAI 400s
    // on this and a silent skip would desynchronize the `index` field
    // that clients zip back against their own input array.
    if inputs.iter().any(|s| s.trim().is_empty()) {
        return bad_request("`input` entries must be non-empty");
    }
    // Per-input size cap matches the truncation threshold inside
    // cortex; surfacing it as 400 here is deterministic and lets
    // clients fix their batching rather than getting a silently
    // truncated embedding.
    if let Some(too_long) = inputs
        .iter()
        .find(|s| s.len() > cortex::constants::MAX_EMBEDDING_INPUT_CHARS)
    {
        return bad_request(&format!(
            "`input` entry exceeds {} char limit (got {})",
            cortex::constants::MAX_EMBEDDING_INPUT_CHARS,
            too_long.len()
        ));
    }

    // Run the request through the same gate the chat path uses so a
    // stolen token gets per-principal rate-limited and audited. The
    // gate operates on a body string; we feed it the first input as
    // representative content. Sanitization passes here are fine —
    // embedding inputs are arbitrary text from the same trust tier as
    // chat user messages.
    let sender_id = principal
        .as_str()
        .strip_prefix("token:")
        .unwrap_or(principal.as_str())
        .to_string();
    let gate_message = IngressMessage {
        source: IngressSource::Http(sender_id.clone()),
        principal: principal.clone(),
        sender_id,
        thread_id: None,
        body: MessageBody::text(inputs[0].clone()),
        meta: serde_json::Value::Null,
        received_at: chrono::Utc::now().timestamp(),
    };
    if let Err(e) = state.gate.admit(gate_message) {
        tracing::warn!(principal = %principal.as_str(), reason = %e, "openai_compat embeddings rate-limited");
        return rejected(e);
    }

    let n_inputs = inputs.len();
    match state.embedding_dispatcher.embed(&req.model, inputs).await {
        Ok(vectors) => {
            tracing::info!(
                principal = %principal.as_str(),
                model = %req.model,
                n_inputs,
                "openai_compat embeddings ok"
            );
            let data = vectors
                .into_iter()
                .enumerate()
                .map(|(i, embedding)| EmbeddingData {
                    object: "embedding",
                    index: i as u32,
                    embedding,
                })
                .collect();
            let resp = EmbeddingsResponse {
                object: "list",
                data,
                model: req.model,
                usage: EmbeddingsUsage::default(),
            };
            Json(resp).into_response()
        }
        Err(EmbeddingDispatchError::ProviderUnavailable) => {
            tracing::warn!(
                principal = %principal.as_str(),
                "openai_compat embeddings provider unavailable"
            );
            provider_unavailable()
        }
        Err(EmbeddingDispatchError::Upstream(msg)) => {
            // The full upstream message can contain org ids, account-
            // level rate-limit detail, or request fingerprints —
            // forwarding it to an arbitrary authenticated client (e.g.
            // a third-party Cursor plugin) is a soft info-leak. Log
            // verbosely for operators; return a generic message.
            tracing::warn!(
                principal = %principal.as_str(),
                upstream_error = %msg,
                "openai_compat embeddings upstream error"
            );
            upstream_error()
        }
    }
}

fn provider_unavailable() -> Response {
    let body = serde_json::json!({
        "error": {
            "message": "embedding provider not configured",
            "type": "service_unavailable",
        }
    });
    (StatusCode::SERVICE_UNAVAILABLE, Json(body)).into_response()
}

fn upstream_error() -> Response {
    let body = serde_json::json!({
        "error": {
            "message": "upstream embedding provider returned an error",
            "type": "api_error",
        }
    });
    (StatusCode::BAD_GATEWAY, Json(body)).into_response()
}

/// `GET /v1/models` — auth-protected enumeration of configured aliases.
async fn list_models(State(state): State<OpenAiState>, headers: HeaderMap) -> Response {
    if let Err(resp) = authenticate(&headers, &state).await {
        return resp;
    }
    let now = chrono::Utc::now().timestamp();
    let data: Vec<ModelInfo> = state
        .model_aliases
        .iter()
        .map(|id| ModelInfo {
            id: id.clone(),
            object: "model",
            created: now,
            owned_by: "borg",
        })
        .collect();
    Json(ModelsList {
        object: "list",
        data,
    })
    .into_response()
}

fn bad_request(msg: &str) -> Response {
    let body = serde_json::json!({
        "error": {
            "message": msg,
            "type": "invalid_request_error",
        }
    });
    (StatusCode::BAD_REQUEST, Json(body)).into_response()
}

fn rejected(reason: cortex::ingress::IngressReject) -> Response {
    use cortex::ingress::IngressReject::*;
    let (status, msg) = match &reason {
        EmptyBody => (StatusCode::BAD_REQUEST, "empty body".to_string()),
        RateLimited { .. } => (StatusCode::TOO_MANY_REQUESTS, reason.to_string()),
        PrincipalCapacityExhausted => (
            StatusCode::TOO_MANY_REQUESTS,
            "ingress principal capacity exhausted".to_string(),
        ),
    };
    let body = serde_json::json!({
        "error": {
            "message": msg,
            "type": "rate_limit_error",
        }
    });
    (status, Json(body)).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use axum::body::Body;
    use axum::http::Request;
    use cortex::ingress::AdmittedIngress;
    use futures::stream::{self, BoxStream};
    use std::sync::Arc;
    use std::time::Duration;
    use tower::ServiceExt;

    async fn make_state(
        chunks: Vec<AgentEventChunk>,
    ) -> (
        OpenAiState,
        cortex::auth::tokens::IssuedToken,
        tempfile::TempDir,
    ) {
        // Use a tempfile-backed DB so each call of the factory opens a
        // fresh handle against the same persisted state. The route
        // handler must see the setup token issued below.
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("test.db");
        let path_for_factory = path.clone();
        let db_open: crate::DbOpener = Arc::new(move || {
            let p = path_for_factory.clone();
            Box::pin(async move { Database::open_at(&p).await })
        });

        let setup_db = (db_open)().await.expect("open setup db");
        let issued = cortex::auth::tokens::issue(&setup_db, "test-tok", "openai_compat")
            .await
            .expect("issue token");

        let gate = Arc::new(IngressGate::with_noop_audit(100, Duration::from_secs(1)));
        let dispatcher: Arc<dyn ChatDispatcher> = Arc::new(FakeDispatcher { chunks });
        let embedding_dispatcher: Arc<dyn EmbeddingDispatcher> =
            Arc::new(FakeEmbeddingDispatcher::unavailable());
        let state = OpenAiState {
            db_open,
            gate,
            dispatcher,
            embedding_dispatcher,
            model_aliases: Arc::new(vec!["borg-default".into()]),
        };
        (state, issued, tmp)
    }

    /// State factory for embedding tests — same DB + token setup, with
    /// a scripted `EmbeddingDispatcher` instead of the chat-stub one.
    async fn make_embedding_state(
        outcome: FakeEmbeddingOutcome,
    ) -> (
        OpenAiState,
        cortex::auth::tokens::IssuedToken,
        tempfile::TempDir,
    ) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("test.db");
        let path_for_factory = path.clone();
        let db_open: crate::DbOpener = Arc::new(move || {
            let p = path_for_factory.clone();
            Box::pin(async move { Database::open_at(&p).await })
        });

        let setup_db = (db_open)().await.expect("open setup db");
        let issued = cortex::auth::tokens::issue(&setup_db, "test-tok", "openai_compat")
            .await
            .expect("issue token");

        let gate = Arc::new(IngressGate::with_noop_audit(100, Duration::from_secs(1)));
        // Chat dispatcher unused for embedding tests but the struct
        // requires a value; reuse the same fake.
        let dispatcher: Arc<dyn ChatDispatcher> = Arc::new(FakeDispatcher { chunks: vec![] });
        let embedding_dispatcher: Arc<dyn EmbeddingDispatcher> =
            Arc::new(FakeEmbeddingDispatcher { outcome });
        let state = OpenAiState {
            db_open,
            gate,
            dispatcher,
            embedding_dispatcher,
            model_aliases: Arc::new(vec!["borg-default".into()]),
        };
        (state, issued, tmp)
    }

    /// Scripted outcome the fake embedding dispatcher returns. Each
    /// test picks one — `Vectors` for the happy path and the order-
    /// preservation guard, the two error variants for the failure
    /// mapping tests.
    #[derive(Clone)]
    enum FakeEmbeddingOutcome {
        Vectors(Vec<Vec<f32>>),
        ProviderUnavailable,
        Upstream(String),
    }

    struct FakeEmbeddingDispatcher {
        outcome: FakeEmbeddingOutcome,
    }

    impl FakeEmbeddingDispatcher {
        fn unavailable() -> Self {
            Self {
                outcome: FakeEmbeddingOutcome::ProviderUnavailable,
            }
        }
    }

    #[async_trait]
    impl EmbeddingDispatcher for FakeEmbeddingDispatcher {
        async fn embed(
            &self,
            _model: &str,
            _inputs: Vec<String>,
        ) -> Result<Vec<Vec<f32>>, EmbeddingDispatchError> {
            match &self.outcome {
                FakeEmbeddingOutcome::Vectors(v) => Ok(v.clone()),
                FakeEmbeddingOutcome::ProviderUnavailable => {
                    Err(EmbeddingDispatchError::ProviderUnavailable)
                }
                FakeEmbeddingOutcome::Upstream(msg) => {
                    Err(EmbeddingDispatchError::Upstream(msg.clone()))
                }
            }
        }
    }

    struct FakeDispatcher {
        chunks: Vec<AgentEventChunk>,
    }

    #[async_trait]
    impl ChatDispatcher for FakeDispatcher {
        async fn dispatch(
            &self,
            _admitted: AdmittedIngress,
        ) -> BoxStream<'static, AgentEventChunk> {
            stream::iter(self.chunks.clone()).boxed()
        }
    }

    fn req_with(token: Option<&str>, body: serde_json::Value) -> Request<Body> {
        let mut b = Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header("content-type", "application/json");
        if let Some(t) = token {
            b = b.header("authorization", format!("Bearer {t}"));
        }
        b.body(Body::from(body.to_string())).expect("build req")
    }

    fn embed_req(token: Option<&str>, body: serde_json::Value) -> Request<Body> {
        let mut b = Request::builder()
            .method("POST")
            .uri("/v1/embeddings")
            .header("content-type", "application/json");
        if let Some(t) = token {
            b = b.header("authorization", format!("Bearer {t}"));
        }
        b.body(Body::from(body.to_string())).expect("build req")
    }

    async fn body_json(resp: Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .expect("read body");
        serde_json::from_slice(&bytes).expect("json")
    }

    async fn body_text(resp: Response) -> String {
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .expect("read body");
        String::from_utf8(bytes.to_vec()).expect("utf-8")
    }

    #[tokio::test]
    async fn missing_bearer_returns_401() {
        // Auth-bypass would expose the agent loop publicly. The
        // missing-header path must be 401, not 200, not 500.
        let (state, _issued, _tmp) = make_state(vec![AgentEventChunk::Done]).await;
        let app = router(state);
        let resp = app
            .oneshot(req_with(
                None,
                serde_json::json!({
                    "messages":[{"role":"user","content":"hi"}],
                    "model":"borg-default"
                }),
            ))
            .await
            .expect("call");
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn wrong_token_returns_401() {
        // Brute-forcing tokens must hit the same generic 401 as a
        // missing header — no oracle for "did this token ever exist".
        let (state, _issued, _tmp) = make_state(vec![AgentEventChunk::Done]).await;
        let app = router(state);
        let resp = app
            .oneshot(req_with(
                Some("definitely-not-a-real-token"),
                serde_json::json!({
                    "messages":[{"role":"user","content":"hi"}],
                    "model":"borg-default"
                }),
            ))
            .await
            .expect("call");
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn non_stream_response_concatenates_text_chunks() {
        // The dispatcher splits "hello world" across two text chunks;
        // the non-stream response must reassemble them into a single
        // `choices[0].message.content`. Regressing this means clients
        // see partial responses.
        let (state, issued, _tmp) = make_state(vec![
            AgentEventChunk::Text("hello ".into()),
            AgentEventChunk::Text("world".into()),
            AgentEventChunk::Done,
        ])
        .await;
        let app = router(state);
        let resp = app
            .oneshot(req_with(
                Some(&issued.plaintext),
                serde_json::json!({
                    "messages":[{"role":"user","content":"hi"}],
                    "model":"borg-default",
                    "stream":false
                }),
            ))
            .await
            .expect("call");
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert_eq!(body["object"], "chat.completion");
        assert_eq!(body["model"], "borg-default");
        assert_eq!(body["choices"][0]["message"]["role"], "assistant");
        assert_eq!(body["choices"][0]["message"]["content"], "hello world");
        assert_eq!(body["choices"][0]["finish_reason"], "stop");
        // `id` must be the `chatcmpl-` prefixed form clients expect.
        let id = body["id"].as_str().expect("id is string");
        assert!(
            id.starts_with("chatcmpl-"),
            "id must start with chatcmpl-, got: {id}"
        );
    }

    #[tokio::test]
    async fn non_stream_response_includes_tool_calls() {
        // Tool-driven agent flows must surface tool_calls in the
        // assistant message — clients displaying tool execution rely
        // on this field.
        let (state, issued, _tmp) = make_state(vec![
            AgentEventChunk::ToolCall {
                id: "call_1".into(),
                name: "memory_search".into(),
                arguments: r#"{"q":"x"}"#.into(),
            },
            AgentEventChunk::Text("found 3 results".into()),
            AgentEventChunk::Done,
        ])
        .await;
        let app = router(state);
        let resp = app
            .oneshot(req_with(
                Some(&issued.plaintext),
                serde_json::json!({
                    "messages":[{"role":"user","content":"search"}],
                    "model":"borg-default"
                }),
            ))
            .await
            .expect("call");
        let body = body_json(resp).await;
        let calls = body["choices"][0]["message"]["tool_calls"]
            .as_array()
            .expect("tool_calls present");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["id"], "call_1");
        assert_eq!(calls[0]["type"], "function");
        assert_eq!(calls[0]["function"]["name"], "memory_search");
        assert_eq!(calls[0]["function"]["arguments"], r#"{"q":"x"}"#);
    }

    #[tokio::test]
    async fn stream_response_emits_sse_chunks_then_done() {
        // The streaming path is the most common usage in real OpenAI
        // SDKs. Each chunk must decode as valid JSON, last must be
        // `[DONE]`, and joined deltas must equal the non-stream text.
        // Regressing any of these breaks ChatBox / Open WebUI.
        let (state, issued, _tmp) = make_state(vec![
            AgentEventChunk::Text("hi ".into()),
            AgentEventChunk::Text("there".into()),
            AgentEventChunk::Done,
        ])
        .await;
        let app = router(state);
        let resp = app
            .oneshot(req_with(
                Some(&issued.plaintext),
                serde_json::json!({
                    "messages":[{"role":"user","content":"x"}],
                    "model":"borg-default",
                    "stream":true
                }),
            ))
            .await
            .expect("call");
        assert_eq!(resp.status(), StatusCode::OK);
        let content_type = resp
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        assert!(
            content_type.contains("text/event-stream"),
            "must serve SSE content-type, got: {content_type}"
        );
        let body = body_text(resp).await;
        // Parse the SSE frames: split on blank lines, strip `data: `.
        let mut data_lines: Vec<&str> = body
            .split("\n\n")
            .filter_map(|frame| frame.trim().strip_prefix("data: "))
            .collect();
        let last = data_lines.pop().expect("at least one frame");
        assert_eq!(last, "[DONE]", "stream must terminate with [DONE]");

        // First chunk includes role; last carries finish_reason; joined
        // content equals the original text.
        let first: serde_json::Value =
            serde_json::from_str(data_lines[0]).expect("first chunk is JSON");
        assert_eq!(first["object"], "chat.completion.chunk");
        assert_eq!(first["choices"][0]["delta"]["role"], "assistant");

        let mut joined = String::new();
        let mut saw_finish = false;
        for d in &data_lines {
            let v: serde_json::Value = serde_json::from_str(d).expect("chunk parses");
            if let Some(c) = v["choices"][0]["delta"]["content"].as_str() {
                joined.push_str(c);
            }
            if v["choices"][0]["finish_reason"] == "stop" {
                saw_finish = true;
            }
        }
        assert_eq!(joined, "hi there");
        assert!(saw_finish, "must emit a chunk with finish_reason=stop");
    }

    #[tokio::test]
    async fn non_stream_error_after_partial_text_is_newline_separated() {
        // A mid-stream agent error must land on its own line below the
        // partial text — gluing "[error] …" onto the last word makes the
        // failure invisible in chat clients.
        let (state, issued, _tmp) = make_state(vec![
            AgentEventChunk::Text("partial answer".into()),
            AgentEventChunk::Error("boom".into()),
        ])
        .await;
        let app = router(state);
        let resp = app
            .oneshot(req_with(
                Some(&issued.plaintext),
                serde_json::json!({
                    "messages":[{"role":"user","content":"hi"}],
                    "model":"borg-default",
                    "stream":false
                }),
            ))
            .await
            .expect("call");
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert_eq!(
            body["choices"][0]["message"]["content"],
            "partial answer\n[error] boom"
        );
    }

    #[tokio::test]
    async fn empty_messages_returns_400() {
        // OpenAI returns 400 on empty messages; we match for SDK
        // compatibility (the `openai` Python SDK raises BadRequestError
        // and decodes the JSON `error.message`).
        let (state, issued, _tmp) = make_state(vec![AgentEventChunk::Done]).await;
        let app = router(state);
        let resp = app
            .oneshot(req_with(
                Some(&issued.plaintext),
                serde_json::json!({"messages":[],"model":"borg-default"}),
            ))
            .await
            .expect("call");
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn embeddings_missing_bearer_returns_401() {
        // The embeddings route holds the same upstream provider key as
        // the memory subsystem — letting unauthenticated traffic
        // through would burn that key's quota on attackers.
        let (state, _issued, _tmp) =
            make_embedding_state(FakeEmbeddingOutcome::Vectors(vec![vec![0.1]])).await;
        let app = router(state);
        let resp = app
            .oneshot(embed_req(
                None,
                serde_json::json!({"model":"m","input":"hi"}),
            ))
            .await
            .expect("call");
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn embeddings_single_string_returns_one_vector() {
        // Happy path with the bare-string `input` form. Asserts: route
        // mounted, dispatcher wired, response shape matches OpenAI
        // (object="list", one data entry with index=0).
        let (state, issued, _tmp) =
            make_embedding_state(FakeEmbeddingOutcome::Vectors(vec![vec![0.1, 0.2, 0.3]])).await;
        let app = router(state);
        let resp = app
            .oneshot(embed_req(
                Some(&issued.plaintext),
                serde_json::json!({"model":"text-embedding-3-small","input":"hi"}),
            ))
            .await
            .expect("call");
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert_eq!(body["object"], "list");
        assert_eq!(body["model"], "text-embedding-3-small");
        let data = body["data"].as_array().expect("data array");
        assert_eq!(data.len(), 1);
        assert_eq!(data[0]["object"], "embedding");
        assert_eq!(data[0]["index"], 0);
        let vec0: Vec<f32> = data[0]["embedding"]
            .as_array()
            .expect("embedding array")
            .iter()
            .map(|v| v.as_f64().expect("number") as f32)
            .collect();
        assert_eq!(vec0, vec![0.1, 0.2, 0.3]);
    }

    #[tokio::test]
    async fn embeddings_array_input_preserves_order_and_indexes() {
        // Clients zip response[i] against input[i]. A future
        // dispatcher refactor that uses `FuturesUnordered` to fan out
        // upstream calls would silently scramble this — the assertion
        // pins the contract.
        let (state, issued, _tmp) = make_embedding_state(FakeEmbeddingOutcome::Vectors(vec![
            vec![1.0],
            vec![2.0],
            vec![3.0],
        ]))
        .await;
        let app = router(state);
        let resp = app
            .oneshot(embed_req(
                Some(&issued.plaintext),
                serde_json::json!({"model":"m","input":["a","b","c"]}),
            ))
            .await
            .expect("call");
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        let data = body["data"].as_array().expect("data array");
        assert_eq!(data.len(), 3);
        for (i, expected) in [1.0f32, 2.0, 3.0].iter().enumerate() {
            assert_eq!(data[i]["index"], i as i64);
            let v = data[i]["embedding"][0].as_f64().unwrap_or(0.0) as f32;
            assert!(
                (v - *expected).abs() < 1e-6,
                "index {i} expected {expected}, got {v}"
            );
        }
    }

    #[tokio::test]
    async fn embeddings_response_echoes_requested_model() {
        // Clients cache responses keyed on the model field. Echoing a
        // different value than the client requested would corrupt
        // those caches.
        let (state, issued, _tmp) =
            make_embedding_state(FakeEmbeddingOutcome::Vectors(vec![vec![0.0]])).await;
        let app = router(state);
        let resp = app
            .oneshot(embed_req(
                Some(&issued.plaintext),
                serde_json::json!({"model":"my-fancy-alias","input":"hi"}),
            ))
            .await
            .expect("call");
        let body = body_json(resp).await;
        assert_eq!(body["model"], "my-fancy-alias");
    }

    #[tokio::test]
    async fn embeddings_empty_input_returns_400() {
        // Both forms — `input: ""` and `input: []` — must 400. OpenAI
        // matches; the `openai-py` SDK raises `BadRequestError` and
        // decodes `error.message`.
        let (state, issued, _tmp) =
            make_embedding_state(FakeEmbeddingOutcome::Vectors(vec![])).await;

        // Empty string
        let app = router(state.clone());
        let resp = app
            .oneshot(embed_req(
                Some(&issued.plaintext),
                serde_json::json!({"model":"m","input":""}),
            ))
            .await
            .expect("call");
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        // Empty array
        let app = router(state);
        let resp = app
            .oneshot(embed_req(
                Some(&issued.plaintext),
                serde_json::json!({"model":"m","input":[]}),
            ))
            .await
            .expect("call");
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn embeddings_numeric_token_input_returns_400() {
        // OpenAI also accepts token-id arrays here. We model strings-
        // only; numeric inputs must fail deserialization and surface
        // as 400 (axum's Json extractor maps the parse failure to
        // BAD_REQUEST). Regression target: a future change to
        // `EmbeddingInput` that silently coerces numbers to strings
        // and forwards them to the upstream — that would burn quota
        // on a request the upstream will reject anyway.
        let (state, issued, _tmp) =
            make_embedding_state(FakeEmbeddingOutcome::Vectors(vec![])).await;
        let app = router(state);
        let resp = app
            .oneshot(embed_req(
                Some(&issued.plaintext),
                serde_json::json!({"model":"m","input":[1,2,3]}),
            ))
            .await
            .expect("call");
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn embeddings_provider_unavailable_returns_503() {
        // Operator hasn't configured `OPENAI_API_KEY` /
        // `OPENROUTER_API_KEY` / `GEMINI_API_KEY`. Distinguishing this
        // from an upstream fault (502, below) lets monitoring alert
        // separately on "you forgot to configure this" vs. "the
        // provider is sick."
        let (state, issued, _tmp) =
            make_embedding_state(FakeEmbeddingOutcome::ProviderUnavailable).await;
        let app = router(state);
        let resp = app
            .oneshot(embed_req(
                Some(&issued.plaintext),
                serde_json::json!({"model":"m","input":"hi"}),
            ))
            .await
            .expect("call");
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = body_json(resp).await;
        assert_eq!(body["error"]["type"], "service_unavailable");
    }

    #[tokio::test]
    async fn embeddings_upstream_error_returns_502_with_sanitized_message() {
        // Upstream provider returned 5xx or the network call failed.
        // The dispatcher's `Upstream(msg)` carries the verbatim provider
        // text — that often contains org ids and account-level
        // rate-limit detail. We surface 502 with a *sanitized*
        // generic message; the verbose detail stays in `tracing` for
        // operators. Regression target: a refactor that "helpfully"
        // pipes the raw upstream string back to the client.
        let (state, issued, _tmp) = make_embedding_state(FakeEmbeddingOutcome::Upstream(
            "org_abc123: secret detail must not leak".into(),
        ))
        .await;
        let app = router(state);
        let resp = app
            .oneshot(embed_req(
                Some(&issued.plaintext),
                serde_json::json!({"model":"m","input":"hi"}),
            ))
            .await
            .expect("call");
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
        let body = body_json(resp).await;
        assert_eq!(body["error"]["type"], "api_error");
        let msg = body["error"]["message"].as_str().unwrap_or("");
        assert!(
            !msg.contains("org_abc123") && !msg.contains("secret detail"),
            "upstream detail must not leak to client, got: {msg:?}"
        );
        assert!(
            !msg.is_empty(),
            "message must be a non-empty OpenAI-shaped string"
        );
    }

    #[tokio::test]
    async fn embeddings_batch_over_cap_returns_400() {
        // The dispatcher fans each input out as a separate upstream
        // call. Without this cap a single authenticated request could
        // burn the operator's embedding quota in seconds.
        let (state, issued, _tmp) =
            make_embedding_state(FakeEmbeddingOutcome::Vectors(vec![])).await;
        let app = router(state);
        let oversize: Vec<String> = (0..cortex::constants::MAX_EMBEDDING_BATCH_SIZE + 1)
            .map(|i| format!("input-{i}"))
            .collect();
        let resp = app
            .oneshot(embed_req(
                Some(&issued.plaintext),
                serde_json::json!({"model":"m","input":oversize}),
            ))
            .await
            .expect("call");
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = body_json(resp).await;
        let msg = body["error"]["message"].as_str().unwrap_or("");
        assert!(
            msg.contains("batch size"),
            "error must explain batch-cap violation, got: {msg:?}"
        );
    }

    #[tokio::test]
    async fn embeddings_batch_at_cap_is_accepted() {
        // The cap is exclusive: exactly MAX_EMBEDDING_BATCH_SIZE inputs
        // must go through — rejecting a full legal batch would force
        // clients into off-by-one workarounds.
        let (state, issued, _tmp) =
            make_embedding_state(FakeEmbeddingOutcome::Vectors(vec![vec![0.0]])).await;
        let app = router(state);
        let at_cap: Vec<String> = (0..cortex::constants::MAX_EMBEDDING_BATCH_SIZE)
            .map(|i| format!("input-{i}"))
            .collect();
        let resp = app
            .oneshot(embed_req(
                Some(&issued.plaintext),
                serde_json::json!({"model":"m","input":at_cap}),
            ))
            .await
            .expect("call");
        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "a batch of exactly the cap must be accepted"
        );
    }

    #[tokio::test]
    async fn embeddings_per_input_char_limit_boundary() {
        // Exactly MAX_EMBEDDING_INPUT_CHARS is legal; one char over is a
        // 400 that names the limit. Silent acceptance of over-limit
        // entries would hand cortex input it silently truncates.
        let (state, issued, _tmp) =
            make_embedding_state(FakeEmbeddingOutcome::Vectors(vec![vec![0.0]])).await;

        let at_limit = "x".repeat(cortex::constants::MAX_EMBEDDING_INPUT_CHARS);
        let resp = router(state.clone())
            .oneshot(embed_req(
                Some(&issued.plaintext),
                serde_json::json!({"model":"m","input":at_limit}),
            ))
            .await
            .expect("call");
        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "input of exactly the char limit must be accepted"
        );

        let over_limit = "x".repeat(cortex::constants::MAX_EMBEDDING_INPUT_CHARS + 1);
        let resp = router(state)
            .oneshot(embed_req(
                Some(&issued.plaintext),
                serde_json::json!({"model":"m","input":over_limit}),
            ))
            .await
            .expect("call");
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = body_json(resp).await;
        let msg = body["error"]["message"].as_str().unwrap_or("");
        assert!(
            msg.contains("char limit"),
            "error must name the per-input limit, got: {msg:?}"
        );
    }

    #[tokio::test]
    async fn embeddings_oversize_body_returns_413() {
        // The /v1/embeddings handler uses `axum::body::Bytes` directly
        // (the manual JSON parse path) so it doesn't inherit the
        // `Json<T>` extractor's implicit 2MB cap. The router applies
        // `DefaultBodyLimit` to close that gap; this test pins it.
        // Regression target: dropping the `.layer(DefaultBodyLimit::…)`
        // call inside `openai_compat::router`.
        let (state, issued, _tmp) =
            make_embedding_state(FakeEmbeddingOutcome::Vectors(vec![])).await;
        let app = router(state);
        // Build a body larger than GATEWAY_MAX_BODY_SIZE (2 MiB).
        let huge_input = "x".repeat(cortex::constants::GATEWAY_MAX_BODY_SIZE + 1024);
        let body = serde_json::json!({"model":"m","input":huge_input});
        let resp = app
            .oneshot(embed_req(Some(&issued.plaintext), body))
            .await
            .expect("call");
        assert_eq!(
            resp.status(),
            StatusCode::PAYLOAD_TOO_LARGE,
            "oversize body must be rejected with 413 by the body-limit layer"
        );
    }

    #[tokio::test]
    async fn embeddings_rate_limit_returns_429() {
        // Embeddings traffic flows through `IngressGate::admit` so a
        // stolen token can't loop the route at full speed against a
        // paid upstream. The gate's per-principal capacity surfaces
        // here as a 429 with `error.type = "rate_limit_error"`. Build
        // a state with a capacity-1 gate, burn the budget, expect 429
        // on the next call.
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("test.db");
        let path_for_factory = path.clone();
        let db_open: crate::DbOpener = Arc::new(move || {
            let p = path_for_factory.clone();
            Box::pin(async move { Database::open_at(&p).await })
        });
        let setup_db = (db_open)().await.expect("open setup db");
        let issued = cortex::auth::tokens::issue(&setup_db, "test-tok", "openai_compat")
            .await
            .expect("issue token");

        // Capacity 1 — second request inside the window must reject.
        let gate = Arc::new(IngressGate::with_noop_audit(1, Duration::from_secs(60)));
        let dispatcher: Arc<dyn ChatDispatcher> = Arc::new(FakeDispatcher { chunks: vec![] });
        let embedding_dispatcher: Arc<dyn EmbeddingDispatcher> =
            Arc::new(FakeEmbeddingDispatcher {
                outcome: FakeEmbeddingOutcome::Vectors(vec![vec![0.0]]),
            });
        let state = OpenAiState {
            db_open,
            gate,
            dispatcher,
            embedding_dispatcher,
            model_aliases: Arc::new(vec!["borg-default".into()]),
        };
        let app = router(state);

        // First request consumes the single slot.
        let resp1 = app
            .clone()
            .oneshot(embed_req(
                Some(&issued.plaintext),
                serde_json::json!({"model":"m","input":"hi"}),
            ))
            .await
            .expect("call");
        assert_eq!(resp1.status(), StatusCode::OK);

        // Second request must be rate-limited at the gate (not by the
        // dispatcher). 429 with OpenAI-shaped `rate_limit_error`.
        let resp2 = app
            .oneshot(embed_req(
                Some(&issued.plaintext),
                serde_json::json!({"model":"m","input":"hi again"}),
            ))
            .await
            .expect("call");
        assert_eq!(
            resp2.status(),
            StatusCode::TOO_MANY_REQUESTS,
            "second call within the window must hit the gate's rate limit"
        );
        let body = body_json(resp2).await;
        assert_eq!(body["error"]["type"], "rate_limit_error");
    }

    #[tokio::test]
    async fn list_models_requires_auth_and_returns_aliases() {
        // /v1/models must be auth-gated — listing the configured
        // aliases shouldn't be a public information leak. With auth,
        // it must return the configured set so SDK model pickers
        // populate.
        let (state, issued, _tmp) = make_state(vec![AgentEventChunk::Done]).await;
        let app = router(state.clone());
        let resp = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/v1/models")
                    .body(Body::empty())
                    .expect("build"),
            )
            .await
            .expect("call");
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

        let app = router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/v1/models")
                    .header("authorization", format!("Bearer {}", issued.plaintext))
                    .body(Body::empty())
                    .expect("build"),
            )
            .await
            .expect("call");
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert_eq!(body["object"], "list");
        let ids: Vec<String> = body["data"]
            .as_array()
            .expect("array")
            .iter()
            .map(|m| m["id"].as_str().unwrap_or("").to_string())
            .collect();
        assert!(
            ids.contains(&"borg-default".to_string()),
            "configured alias must appear, got: {ids:?}"
        );
    }
}
