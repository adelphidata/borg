//! ACP request dispatch: `handle_request` plus the request-param types
//! and parsing/validation helpers it relies on.

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

use anyhow::{anyhow, Result};
use base64::Engine;
use cortex::agent::AgentEvent;
use serde::Deserialize;
use serde_json::{json, Value};

use super::translate::{event_to_update, history_replay_update};
use super::transport::{
    send_error, send_notification, send_result, JsonRpcCallbackClient, JsonRpcRequest, SharedWriter,
};
use super::{
    AcpDaemon, AcpRequestError, AcpState, ClientCapabilities, PromptImage, PromptInput,
    SessionMetadata, SessionRecord, SessionRuntime,
};

#[derive(Debug, Deserialize)]
struct InitializeParams {
    #[serde(default)]
    #[serde(rename = "clientCapabilities")]
    client_capabilities: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct SessionNewParams {
    cwd: String,
    #[serde(default)]
    #[serde(rename = "mcpServers")]
    mcp_servers: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct SessionLoadParams {
    #[serde(rename = "sessionId")]
    session_id: String,
    cwd: String,
}

#[derive(Debug, Deserialize)]
struct SessionPromptParams {
    #[serde(rename = "sessionId")]
    session_id: String,
    prompt: Vec<PromptBlock>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(super) enum PromptBlock {
    Text {
        text: String,
    },
    Image {
        #[serde(rename = "mimeType")]
        mime_type: String,
        data: String,
        #[serde(default)]
        filename: Option<String>,
    },
}

#[derive(Debug, Deserialize)]
struct SessionCancelParams {
    #[serde(rename = "sessionId")]
    session_id: String,
}

/// How a `session/prompt` event-stream loop terminated.
enum StreamTermination {
    /// Saw `AgentEvent::TurnComplete`.
    Complete,
    /// Saw `AgentEvent::Error("cancelled")`.
    Cancelled,
    /// Saw `AgentEvent::Error(other)` — a daemon-reported failure mid-turn.
    Failed(String),
    /// Channel closed before any terminal event arrived. Signals daemon
    /// disconnect unless the client had requested cancel.
    Disconnected,
}

/// Wrap a daemon-side failure as a JSON-RPC internal error (`-32603`)
/// carrying the full anyhow chain as detail.
fn internal_err(e: anyhow::Error) -> anyhow::Error {
    anyhow!(AcpRequestError::internal(format!("{e:#}")))
}

/// Shared tail of `session/new` and `session/load`.
///
/// Registers the transport-side `SessionRecord`, optionally replays
/// persisted history as `session/update` frames (`session/load` only),
/// fetches daemon-side session metadata, overwrites its workspace fields
/// with the transport's canonical record, and emits `session_info_update`.
/// Returns the metadata so each caller can shape its own result payload
/// (`session/new` echoes the opened `session_id`; `session/load` echoes
/// `info.session_id` as reported by daemon state).
async fn finish_open_session<D: AcpDaemon>(
    state: &AcpState<D>,
    writer: &SharedWriter,
    session_id: &str,
    cwd: &Path,
    replay_history: bool,
) -> Result<SessionMetadata> {
    {
        let mut sessions = state.sessions.lock().await;
        sessions.insert(
            session_id.to_string(),
            SessionRecord {
                workspace_cwd: cwd.to_path_buf(),
                workspace_root: cwd.to_path_buf(),
                runtime: SessionRuntime::default(),
            },
        );
    }
    if replay_history {
        let history = state
            .daemon
            .replay_history(session_id)
            .await
            .map_err(internal_err)?;
        for message in history {
            if let Some(update) = history_replay_update(session_id, &message) {
                send_notification(writer, "session/update", update).await?;
            }
        }
    }
    let mut info = state.daemon.state(session_id).await.map_err(internal_err)?;
    if let Some(record) = state.sessions.lock().await.get(session_id).cloned() {
        info.workspace_cwd = Some(record.workspace_cwd.display().to_string());
        info.workspace_root = Some(record.workspace_root.display().to_string());
    }
    send_notification(writer, "session_info_update", session_info_params(&info)).await?;
    Ok(info)
}

pub(super) async fn handle_request<D: AcpDaemon>(
    state: AcpState<D>,
    req: JsonRpcRequest,
    writer: SharedWriter,
    callback_client: JsonRpcCallbackClient,
) -> Result<()> {
    match req.method.as_str() {
        "initialize" => {
            let params: InitializeParams = parse_params(req.params)?;
            {
                let mut caps = state
                    .client_capabilities
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                *caps = parse_client_capabilities(params.client_capabilities.as_ref());
            }
            send_result(&writer, req.id, initialize_response()).await
        }
        "authenticate" => send_result(&writer, req.id, json!({"authenticated": true})).await,
        "session/new" => {
            require_onboarding(&state).await?;
            let params: SessionNewParams = parse_params(req.params)?;
            let cwd = validate_workspace_cwd(&params.cwd)?;
            // `mcpServers` is structurally accepted but not honored —
            // MCP-over-ACP bridging is explicitly out of scope for v1.
            _ = params.mcp_servers;
            let (session_id, _) = state
                .daemon
                .open_session(None, Some(&cwd), Some(&cwd), true)
                .await
                .map_err(internal_err)?;
            finish_open_session(&state, &writer, &session_id, &cwd, false).await?;
            send_result(&writer, req.id, json!({"sessionId": session_id})).await
        }
        "session/load" => {
            require_onboarding(&state).await?;
            let params: SessionLoadParams = parse_params(req.params)?;
            let cwd = validate_workspace_cwd(&params.cwd)?;
            let (session_id, _) = state
                .daemon
                .open_session(Some(&params.session_id), Some(&cwd), Some(&cwd), true)
                .await
                .map_err(internal_err)?;
            let info = finish_open_session(&state, &writer, &session_id, &cwd, true).await?;
            send_result(&writer, req.id, json!({"sessionId": info.session_id})).await
        }
        "session/prompt" => {
            require_onboarding(&state).await?;
            let params: SessionPromptParams = parse_params(req.params)?;
            // Mention/path expansion is deferred to the IDE in ACP sessions.
            // Zed, Neovim's ACP client, and similar editors implement
            // their own `@mention` resolution before sending the prompt;
            // calling `cortex::mentions::expand_mentions` here would
            // (a) double-expand for clients that already resolved their
            // mentions and (b) read from the daemon's process cwd via
            // `std::env::current_dir()` rather than the session's
            // workspace_cwd. If a future ACP version standardizes a server-
            // resolved mention block, route that through the workspace
            // context instead — do NOT re-introduce process-cwd expansion.
            let prompt = parse_prompt_blocks(&params.prompt)?;
            {
                let mut sessions = state.sessions.lock().await;
                let session = sessions
                    .get_mut(&params.session_id)
                    .ok_or_else(|| anyhow!(AcpRequestError::unknown_session(&params.session_id)))?;
                // Reject concurrent prompts on the same session: the daemon
                // serializes turns server-side, but the transport's
                // `cancel_requested` flag is per-session, not per-prompt — a
                // racing second prompt would clobber the first prompt's
                // pending cancel state. Reject with a structured busy error
                // so the IDE can surface it to the user (rather than queueing
                // or replacing, which both have observability downsides).
                if session.runtime.prompt_in_flight {
                    return Err(anyhow!(AcpRequestError::prompt_in_flight(
                        &params.session_id
                    )));
                }
                session.runtime.prompt_in_flight = true;
                session.runtime.cancel_requested = false;
            }
            let mut events = match state
                .daemon
                .send_prompt(&params.session_id, prompt, callback_client.clone())
                .await
            {
                Ok(rx) => rx,
                Err(e) => {
                    // The in-flight flag is now sticky (see busy-reject
                    // above); reset it before propagating so this session
                    // isn't permanently locked when the daemon hand-off
                    // fails (e.g. transient gRPC error on the bidi open).
                    let mut sessions = state.sessions.lock().await;
                    if let Some(session) = sessions.get_mut(&params.session_id) {
                        session.runtime.prompt_in_flight = false;
                    }
                    return Err(internal_err(e));
                }
            };
            let mut stop_reason = "end_turn";
            let mut termination = StreamTermination::Disconnected;
            while let Some(event) = events.recv().await {
                match &event {
                    AgentEvent::TurnComplete => {
                        termination = StreamTermination::Complete;
                        break;
                    }
                    AgentEvent::Error(message) if message == "cancelled" => {
                        termination = StreamTermination::Cancelled;
                        break;
                    }
                    AgentEvent::Error(message) => {
                        termination = StreamTermination::Failed(message.clone());
                        break;
                    }
                    _ => {
                        if let Some(update) = event_to_update(&event, &params.session_id) {
                            // A writer failure here used to propagate via
                            // `?`, bypassing the cleanup block below and
                            // leaving `prompt_in_flight = true` forever.
                            // Route the failure through `StreamTermination`
                            // so the unified cleanup path resets the flag.
                            if let Err(e) =
                                send_notification(&writer, "session/update", update).await
                            {
                                termination = StreamTermination::Failed(format!(
                                    "session/update write failed: {e:#}"
                                ));
                                break;
                            }
                        }
                    }
                }
            }
            let cancel_observed = {
                let mut sessions = state.sessions.lock().await;
                let mut observed = false;
                if let Some(session) = sessions.get_mut(&params.session_id) {
                    session.runtime.prompt_in_flight = false;
                    observed = session.runtime.cancel_requested;
                }
                observed
            };
            match termination {
                StreamTermination::Complete => {}
                StreamTermination::Cancelled => stop_reason = "cancelled",
                StreamTermination::Failed(message) => {
                    tracing::warn!(
                        session = %params.session_id,
                        reason = %message,
                        "ACP turn failed; surfacing as JSON-RPC internal error",
                    );
                    return Err(anyhow!(AcpRequestError::internal(message)));
                }
                StreamTermination::Disconnected => {
                    if cancel_observed {
                        // Cancellation race: client cancelled while the
                        // daemon was already winding the turn down. Surface
                        // as cancelled rather than disconnect.
                        stop_reason = "cancelled";
                    } else {
                        state.daemon_failed.store(true, Ordering::SeqCst);
                        return Err(anyhow!(AcpRequestError::daemon_disconnected()));
                    }
                }
            }
            if cancel_observed {
                stop_reason = "cancelled";
            }
            send_result(&writer, req.id, json!({"stopReason": stop_reason})).await
        }
        "session/cancel" => {
            let params: SessionCancelParams = parse_params(req.params)?;
            let known = {
                let mut sessions = state.sessions.lock().await;
                if let Some(session) = sessions.get_mut(&params.session_id) {
                    // Mark cancel_requested unconditionally — a cancel
                    // arriving in the narrow window between `send_prompt`
                    // returning and the dispatcher checking the flag would
                    // otherwise be lost. The flag is reset when the next
                    // prompt sets `prompt_in_flight = true`, so a stray
                    // pre-cancel can't bleed into a later turn.
                    session.runtime.cancel_requested = true;
                    true
                } else {
                    false
                }
            };
            // Unknown session id is a no-op per ACP spec semantics — the IDE
            // may issue cancel after the session was forgotten.
            if known {
                if let Err(e) = state.daemon.cancel(&params.session_id).await {
                    tracing::warn!(
                        session = %params.session_id,
                        error = %e,
                        "daemon cancel failed; treating as no-op",
                    );
                }
            }
            // ACP spec defines `session/cancel` as a notification, but a
            // tolerant client may send it as a request (with `id`). Honour
            // either shape: when an id is present, return a null result so
            // the client doesn't time out waiting for a response.
            if req.id.is_some() {
                send_result(&writer, req.id, Value::Null).await?;
            }
            Ok(())
        }
        _ => {
            if req.id.is_some() {
                send_error(
                    &writer,
                    req.id.unwrap_or(Value::Null),
                    -32601,
                    "Method not found",
                    None,
                )
                .await?;
            }
            Ok(())
        }
    }
}

async fn require_onboarding<D: AcpDaemon>(state: &AcpState<D>) -> Result<()> {
    // Onboarding state is monotonic: once `borg` has been onboarded it stays
    // onboarded for the lifetime of this process. Cache the first success
    // so we skip the gRPC roundtrip on every subsequent session/* call.
    if state.onboarding_confirmed.load(Ordering::SeqCst) {
        return Ok(());
    }
    match state.daemon.onboarding_complete().await {
        Ok(true) => {
            state.onboarding_confirmed.store(true, Ordering::SeqCst);
            Ok(())
        }
        Ok(false) => Err(anyhow!(AcpRequestError::onboarding_required())),
        Err(e) => Err(anyhow!(AcpRequestError::internal(format!(
            "onboarding probe failed: {e:#}"
        )))),
    }
}

pub(super) fn validate_workspace_cwd(cwd: &str) -> Result<PathBuf> {
    let path = PathBuf::from(cwd);
    if !path.is_absolute() {
        return Err(anyhow!(AcpRequestError::invalid_params(
            "cwd must be an absolute path",
        )));
    }
    path.canonicalize().map_err(|e| {
        anyhow!(AcpRequestError::invalid_params(format!(
            "cannot resolve cwd {}: {e}",
            path.display()
        )))
    })
}

/// Upper bound on a single ACP prompt's combined text + decoded image bytes.
/// A misbehaving or malicious IDE can otherwise stream arbitrarily large
/// prompts into the transport before the daemon ever sees them — this
/// short-circuits during accumulation so we never materialise more than the
/// cap. 2 MiB is well above any realistic IDE prompt (typical screenshots
/// fit comfortably).
pub(super) const MAX_PROMPT_BYTES: usize = 2 * 1024 * 1024;

/// Convert ACP prompt blocks into the daemon's `(text, images)` shape.
///
/// **Interleaving is not preserved.** Text fragments are concatenated in
/// order with `\n` separators; images are pushed onto a separate list and
/// reach the daemon as multimodal attachments after the text.
/// If a future ACP client relies on tight image-text interleaving,
/// extend `AcpStartTurn` with an ordered `repeated PromptBlock` field and
/// thread that through `Message::user_multimodal`.
pub(super) fn parse_prompt_blocks(blocks: &[PromptBlock]) -> Result<PromptInput> {
    let mut text_parts: Vec<String> = Vec::new();
    let mut images = Vec::new();
    let mut budget = MAX_PROMPT_BYTES;
    for block in blocks {
        match block {
            PromptBlock::Text { text } => {
                // `\n` separator added between non-first text blocks counts
                // toward the cap so we don't undercount the final string.
                let separator_bytes = if text_parts.is_empty() { 0 } else { 1 };
                let cost = text.len().saturating_add(separator_bytes);
                if cost > budget {
                    return Err(anyhow!(AcpRequestError::invalid_params(format!(
                        "prompt too large: text block exceeds remaining budget (cap {MAX_PROMPT_BYTES} bytes)"
                    ))));
                }
                budget -= cost;
                text_parts.push(text.clone());
            }
            PromptBlock::Image {
                mime_type,
                data,
                filename,
            } => {
                // Cheap upper-bound check before spending CPU on base64 decode:
                // decoded length is at most `data.len() * 3 / 4`, but use the
                // raw input length as a generous early-reject, then verify
                // against the actual decoded size.
                if data.len() > budget.saturating_mul(2) {
                    return Err(anyhow!(AcpRequestError::invalid_params(format!(
                        "prompt too large: image block {mime_type} exceeds remaining budget (cap {MAX_PROMPT_BYTES} bytes)"
                    ))));
                }
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(data)
                    .map_err(|e| {
                        anyhow!(AcpRequestError::invalid_params(format!(
                            "decode image block {mime_type}: {e}"
                        )))
                    })?;
                if bytes.len() > budget {
                    return Err(anyhow!(AcpRequestError::invalid_params(format!(
                        "prompt too large: image block {mime_type} ({} bytes) exceeds remaining budget (cap {MAX_PROMPT_BYTES} bytes)",
                        bytes.len()
                    ))));
                }
                budget -= bytes.len();
                images.push(PromptImage {
                    mime_type: mime_type.clone(),
                    data: bytes,
                    filename: filename.clone(),
                });
            }
        }
    }
    Ok(PromptInput {
        text: text_parts.join("\n"),
        images,
    })
}

fn parse_params<T: for<'de> Deserialize<'de>>(params: Option<Value>) -> Result<T> {
    serde_json::from_value(params.unwrap_or(Value::Null)).map_err(|e| {
        anyhow!(AcpRequestError::invalid_params(format!(
            "invalid params: {e}"
        )))
    })
}

/// Extract the client-advertised `fs.{readTextFile,writeTextFile}` flags from
/// the `clientCapabilities` field of `initialize`. Per ACP spec the flags
/// MUST be nested under `fs`; bare top-level booleans are ignored so a
/// malformed client doesn't get accidentally credited with capabilities it
/// didn't actually claim.
pub(super) fn parse_client_capabilities(value: Option<&Value>) -> ClientCapabilities {
    let fs = value.and_then(|value| value.get("fs"));
    if let Some(value) = value {
        if fs.is_none() && value.is_object() {
            // The IDE sent clientCapabilities but no nested `fs` object.
            // Per spec we treat this as "no fs capabilities", which means
            // the daemon falls back to disk reads/writes. Surface in the
            // log so an integrator who expected unsaved-buffer support
            // can see why it isn't firing.
            tracing::debug!(
                payload = %value,
                "ACP clientCapabilities present but no `fs` object — falling back to disk for read/write"
            );
        }
    }
    ClientCapabilities {
        read_text_file: fs
            .and_then(|fs| fs.get("readTextFile"))
            .and_then(Value::as_bool)
            .unwrap_or(false),
        write_text_file: fs
            .and_then(|fs| fs.get("writeTextFile"))
            .and_then(Value::as_bool)
            .unwrap_or(false),
    }
}

/// Build the spec-shaped `initialize` response.
///
/// Per ACP §"Initialization", the agent advertises `agentCapabilities`
/// (loadSession + promptCapabilities + mcpCapabilities). It does NOT
/// re-advertise `fs.*`, which is the **client's** capability surface —
/// `parse_client_capabilities` reads those from the request and stores
/// them on `AcpState` for later use during turns.
fn initialize_response() -> Value {
    json!({
        "protocolVersion": 1,
        "agentCapabilities": {
            "loadSession": true,
            "promptCapabilities": {
                "image": true,
                "audio": false,
                "embeddedContext": false,
            },
            "mcpCapabilities": {
                "http": false,
                "sse": false,
            },
        },
        "authMethods": [],
    })
}

fn session_info_params(info: &SessionMetadata) -> Value {
    json!({
        "sessionId": info.session_id,
        "title": info.title,
        "model": info.model,
        "workspaceCwd": info.workspace_cwd,
        "workspaceRoot": info.workspace_root,
    })
}
