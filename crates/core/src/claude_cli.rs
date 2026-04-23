/// Claude Code CLI subprocess backend.
///
/// Spawns the `claude` CLI binary as a child process with `--output-format stream-json`
/// to use the user's Claude subscription (OAuth) instead of API key access.
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use crate::llm::StreamEvent;
use crate::llm_error::{FailoverReason, LlmError};
use crate::types::{Message, MessageContent, Role, ToolDefinition};

// ── CLI Detection ──

/// Find the `claude` CLI binary on the system.
///
/// Checks `CLAUDE_CLI_PATH` env var first, then falls back to PATH lookup.
pub fn detect_cli_path() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("CLAUDE_CLI_PATH") {
        let p = PathBuf::from(&path);
        if p.exists() {
            return Some(p);
        }
    }
    which::which("claude").ok()
}

/// Check if Claude CLI has valid OAuth credentials.
///
/// Reads `~/.claude/.credentials.json` and verifies that
/// `claudeAiOauth.accessToken` exists and `expiresAt` is in the future.
pub fn has_valid_auth() -> bool {
    read_credentials().is_some()
}

/// OAuth credential from the Claude CLI credentials file.
#[derive(Debug, Clone)]
pub struct ClaudeCredential {
    /// OAuth access token.
    pub access_token: String,
    /// Optional refresh token.
    pub refresh_token: Option<String>,
    /// Token expiration as Unix timestamp in milliseconds.
    pub expires_at: i64,
}

impl ClaudeCredential {
    /// Whether the token has expired (with 60s buffer).
    pub fn is_expired(&self) -> bool {
        let now_ms = chrono::Utc::now().timestamp_millis();
        self.expires_at < now_ms + 60_000
    }
}

/// Read Claude CLI credentials from `~/.claude/.credentials.json` or macOS keychain.
pub fn read_credentials() -> Option<ClaudeCredential> {
    // Try macOS keychain first
    #[cfg(target_os = "macos")]
    if let Some(cred) = read_keychain_credentials() {
        if !cred.is_expired() {
            return Some(cred);
        }
    }

    // Fall back to credentials file
    let home = dirs::home_dir()?;
    let cred_path = home.join(".claude/.credentials.json");
    read_credentials_file(&cred_path)
}

/// Read credentials from a JSON file.
fn read_credentials_file(path: &Path) -> Option<ClaudeCredential> {
    let content = std::fs::read_to_string(path).ok()?;
    parse_credentials_json(&content)
}

/// Parse credentials from JSON content.
fn parse_credentials_json(json: &str) -> Option<ClaudeCredential> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    let oauth = v.get("claudeAiOauth")?;

    let access_token = oauth.get("accessToken")?.as_str()?.to_string();
    if access_token.is_empty() {
        return None;
    }

    let expires_at = oauth.get("expiresAt")?.as_i64()?;
    if expires_at <= 0 {
        return None;
    }

    let now_ms = chrono::Utc::now().timestamp_millis();
    if expires_at < now_ms + 60_000 {
        return None; // Expired or about to expire
    }

    let refresh_token = oauth
        .get("refreshToken")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(ToString::to_string);

    Some(ClaudeCredential {
        access_token,
        refresh_token,
        expires_at,
    })
}

/// Read credentials from the macOS keychain.
#[cfg(target_os = "macos")]
fn read_keychain_credentials() -> Option<ClaudeCredential> {
    let output = std::process::Command::new("security")
        .args([
            "find-generic-password",
            "-s",
            "Claude Code-credentials",
            "-w",
        ])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let json = String::from_utf8(output.stdout).ok()?;
    parse_credentials_json(json.trim())
}

// ── Model Alias Mapping ──

/// Map a model identifier to the short alias the `claude` CLI expects for `--model`.
///
/// The CLI accepts both long IDs (`claude-sonnet-4-6`) and short aliases (`sonnet`) in
/// current versions, but the short aliases are the documented/stable surface. Mapping
/// here protects us from CLI version drift and new model revisions.
///
/// Case-insensitive. Unknown inputs pass through unchanged.
pub fn normalize_cli_model(model: &str) -> String {
    let key = model.trim().to_ascii_lowercase();
    match key.as_str() {
        // Opus family
        "opus" | "claude-opus" | "opus-4" | "opus-4-6" | "opus-4.6" | "claude-opus-4"
        | "claude-opus-4-6" | "claude-opus-4.6" => "opus".to_string(),
        // Sonnet family
        "sonnet" | "claude-sonnet" | "sonnet-4" | "sonnet-4-5" | "sonnet-4-6" | "sonnet-4.5"
        | "sonnet-4.6" | "claude-sonnet-4" | "claude-sonnet-4-5" | "claude-sonnet-4-6"
        | "claude-sonnet-4.5" | "claude-sonnet-4.6" => "sonnet".to_string(),
        // Haiku family
        "haiku" | "claude-haiku" | "haiku-3-5" | "haiku-4-5" | "haiku-3.5" | "haiku-4.5"
        | "claude-haiku-3-5" | "claude-haiku-4-5" | "claude-haiku-3.5" | "claude-haiku-4.5" => {
            "haiku".to_string()
        }
        _ => model.to_string(),
    }
}

// ── JSONL Stream Types ──

/// Top-level event from `claude --output-format stream-json`.
#[derive(Debug, Deserialize)]
struct CliEvent {
    #[serde(rename = "type")]
    event_type: String,
    #[serde(default)]
    subtype: Option<String>,
    #[serde(default)]
    content_block: Option<ContentBlock>,
    #[serde(default)]
    delta: Option<Delta>,
    #[serde(default)]
    index: Option<usize>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    usage: Option<CliUsage>,
}

#[derive(Debug, Deserialize)]
struct ContentBlock {
    #[serde(rename = "type")]
    block_type: String,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    text: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Delta {
    #[serde(rename = "type")]
    delta_type: String,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    partial_json: Option<String>,
    #[serde(default)]
    thinking: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CliUsage {
    #[serde(default)]
    input_tokens: Option<u64>,
    #[serde(default)]
    output_tokens: Option<u64>,
    #[serde(default)]
    cache_read_input_tokens: Option<u64>,
    #[serde(default)]
    cache_creation_input_tokens: Option<u64>,
}

/// Parse a single JSONL line into a `StreamEvent`.
///
/// Returns `None` for events we don't care about (system init, content_block_stop, etc.).
fn parse_jsonl_event(line: &str) -> Option<StreamEvent> {
    let event: CliEvent = serde_json::from_str(line).ok()?;

    match event.event_type.as_str() {
        "content_block_start" => {
            let block = event.content_block?;
            match block.block_type.as_str() {
                "tool_use" => Some(StreamEvent::ToolCallDelta {
                    index: event.index.unwrap_or(0),
                    id: block.id,
                    name: block.name,
                    arguments_delta: String::new(),
                }),
                "text" => {
                    // If there's initial text in the block, emit it
                    block
                        .text
                        .filter(|t| !t.is_empty())
                        .map(StreamEvent::TextDelta)
                }
                _ => None,
            }
        }
        "content_block_delta" => {
            let delta = event.delta?;
            match delta.delta_type.as_str() {
                "text_delta" => delta.text.map(StreamEvent::TextDelta),
                "thinking_delta" => delta
                    .thinking
                    .or(delta.text)
                    .map(StreamEvent::ThinkingDelta),
                "input_json_delta" => Some(StreamEvent::ToolCallDelta {
                    index: event.index.unwrap_or(0),
                    id: None,
                    name: None,
                    arguments_delta: delta.partial_json.unwrap_or_default(),
                }),
                _ => None,
            }
        }
        "message_start" | "message_delta" => {
            // Check for usage data
            event.usage.map(|usage| {
                StreamEvent::Usage(crate::llm::UsageData {
                    prompt_tokens: usage.input_tokens.unwrap_or(0),
                    completion_tokens: usage.output_tokens.unwrap_or(0),
                    total_tokens: usage.input_tokens.unwrap_or(0)
                        + usage.output_tokens.unwrap_or(0),
                    cached_input_tokens: usage.cache_read_input_tokens.unwrap_or(0),
                    cache_creation_tokens: usage.cache_creation_input_tokens.unwrap_or(0),
                    provider: "claude-cli".to_string(),
                    model: String::new(),
                })
            })
        }
        "result" => {
            if event.subtype.as_deref() == Some("error") {
                Some(StreamEvent::Error(
                    event.error.unwrap_or_else(|| "CLI error".to_string()),
                ))
            } else {
                Some(StreamEvent::Done)
            }
        }
        "error" => Some(StreamEvent::Error(
            event.error.unwrap_or_else(|| "CLI error".to_string()),
        )),
        // content_block_stop, system, message_stop — skip
        _ => None,
    }
}

// ── Message Assembly ──

/// Assembled prompt with separated system and user content.
struct AssembledPrompt {
    /// System messages concatenated, passed via --append-system-prompt.
    system: String,
    /// User/assistant/tool messages concatenated, passed via stdin.
    user: String,
}

/// Assemble conversation messages into system and user prompt strings.
///
/// System messages go via `--append-system-prompt` to maintain role separation.
/// User/assistant/tool messages go to stdin for the `-p` pipe mode.
fn assemble_prompt(messages: &[Message]) -> AssembledPrompt {
    let mut system_parts = Vec::new();
    let mut user_parts = Vec::new();

    for msg in messages {
        let text = match &msg.content {
            Some(MessageContent::Text(t)) => t.clone(),
            Some(MessageContent::Parts(parts)) => parts
                .iter()
                .filter_map(|p| match p {
                    crate::types::ContentPart::Text(t) => Some(t.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n"),
            None => continue,
        };

        match msg.role {
            Role::System => system_parts.push(text),
            Role::User => user_parts.push(text),
            Role::Assistant => user_parts.push(format!("[Assistant]: {text}")),
            Role::Tool => {
                if let Some(id) = &msg.tool_call_id {
                    user_parts.push(format!("[Tool Result {id}]: {text}"));
                } else {
                    user_parts.push(format!("[Tool Result]: {text}"));
                }
            }
        }
    }

    AssembledPrompt {
        system: system_parts.join("\n\n"),
        user: user_parts.join("\n\n"),
    }
}

// ── Subprocess Streaming ──

/// Stream a response from the Claude CLI subprocess.
///
/// Spawns `claude -p --output-format stream-json --model <model>` and reads
/// JSONL from stdout, mapping events to `StreamEvent`s.
pub async fn stream_claude_cli(
    cli_path: &Path,
    messages: &[Message],
    _tools: Option<&[ToolDefinition]>,
    model: &str,
    _temperature: f32,
    tx: &mpsc::Sender<StreamEvent>,
    cancel: &CancellationToken,
) -> Result<(), LlmError> {
    let assembled = assemble_prompt(messages);
    if assembled.user.is_empty() && assembled.system.is_empty() {
        return Err(LlmError::Fatal {
            source: anyhow::anyhow!("empty prompt for Claude CLI"),
            reason: FailoverReason::Format,
        });
    }

    let cli_model = normalize_cli_model(model);
    debug!(
        cli = %cli_path.display(),
        requested_model = model,
        cli_model = %cli_model,
        user_prompt_len = assembled.user.len(),
        system_prompt_len = assembled.system.len(),
        "spawning claude CLI subprocess"
    );

    let mut cmd = Command::new(cli_path);
    cmd.args([
        "-p",
        "--output-format",
        "stream-json",
        "--verbose",
        "--model",
        &cli_model,
        "--permission-mode",
        "bypassPermissions",
    ]);

    // Pass system prompt as a separate argument to maintain role separation
    if !assembled.system.is_empty() {
        cmd.args(["--append-system-prompt", &assembled.system]);
    }

    // Clear API key env vars to force OAuth path. ANTHROPIC_API_KEY_OLD is an
    // undocumented fallback the CLI honors in some environments — clear both.
    cmd.env_remove("ANTHROPIC_API_KEY");
    cmd.env_remove("ANTHROPIC_API_KEY_OLD");

    cmd.stdin(std::process::Stdio::piped());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::null()); // Don't capture to avoid pipe buffer deadlock

    let mut child = cmd.spawn().map_err(|e| LlmError::Fatal {
        source: anyhow::anyhow!("failed to spawn claude CLI: {e}"),
        reason: FailoverReason::Unknown,
    })?;

    // Write user prompt to stdin
    if let Some(mut stdin) = child.stdin.take() {
        use tokio::io::AsyncWriteExt;
        let stdin_content = if assembled.user.is_empty() {
            &assembled.system // Fall back to system prompt if no user messages
        } else {
            &assembled.user
        };
        if let Err(e) = stdin.write_all(stdin_content.as_bytes()).await {
            warn!("failed to write to claude CLI stdin: {e}");
        }
        drop(stdin); // Close stdin to signal EOF
    }

    let stdout = child.stdout.take().ok_or_else(|| LlmError::Fatal {
        source: anyhow::anyhow!("failed to capture claude CLI stdout"),
        reason: FailoverReason::Unknown,
    })?;

    let mut reader = BufReader::new(stdout).lines();

    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                let _ = child.kill().await;
                let _ = tx.send(StreamEvent::Done).await;
                return Ok(());
            }
            line_result = reader.next_line() => {
                match line_result {
                    Ok(Some(line)) => {
                        let line = line.trim();
                        if line.is_empty() {
                            continue;
                        }
                        if let Some(event) = parse_jsonl_event(line) {
                            let is_done = matches!(event, StreamEvent::Done);
                            let is_error = matches!(event, StreamEvent::Error(_));
                            if tx.send(event).await.is_err() {
                                debug!("claude CLI: stream receiver dropped");
                                let _ = child.kill().await;
                                return Ok(());
                            }
                            if is_done || is_error {
                                break;
                            }
                        }
                    }
                    Ok(None) => {
                        // EOF — process exited
                        break;
                    }
                    Err(e) => {
                        warn!("error reading claude CLI stdout: {e}");
                        break;
                    }
                }
            }
        }
    }

    // Wait for process to finish
    let status = child.wait().await.map_err(|e| LlmError::Retryable {
        source: anyhow::anyhow!("claude CLI wait error: {e}"),
        retry_after: None,
        reason: FailoverReason::Timeout,
    })?;

    if !status.success() {
        let code = status.code().unwrap_or(-1);
        let _ = tx
            .send(StreamEvent::Error(format!(
                "Claude CLI exited with code {code}"
            )))
            .await;
    }

    let _ = tx.send(StreamEvent::Done).await;
    Ok(())
}

// ── Tests ──

#[cfg(test)]
mod tests {
    use super::*;

    // ── Credential Parsing ──

    #[test]
    fn parse_valid_credentials() {
        let future_ms = chrono::Utc::now().timestamp_millis() + 3_600_000; // +1h
        let json = format!(
            r#"{{"claudeAiOauth":{{"accessToken":"sk-ant-oat-test123","refreshToken":"rt-test456","expiresAt":{future_ms}}}}}"#
        );
        let cred = parse_credentials_json(&json).expect("should parse");
        assert_eq!(cred.access_token, "sk-ant-oat-test123");
        assert_eq!(cred.refresh_token.as_deref(), Some("rt-test456"));
        assert!(!cred.is_expired());
    }

    #[test]
    fn parse_expired_credentials_returns_none() {
        let past_ms = chrono::Utc::now().timestamp_millis() - 3_600_000; // -1h
        let json = format!(
            r#"{{"claudeAiOauth":{{"accessToken":"sk-ant-oat-expired","expiresAt":{past_ms}}}}}"#
        );
        assert!(parse_credentials_json(&json).is_none());
    }

    #[test]
    fn parse_missing_access_token_returns_none() {
        let json = r#"{"claudeAiOauth":{"expiresAt":9999999999999}}"#;
        assert!(parse_credentials_json(json).is_none());
    }

    #[test]
    fn parse_empty_access_token_returns_none() {
        let json = r#"{"claudeAiOauth":{"accessToken":"","expiresAt":9999999999999}}"#;
        assert!(parse_credentials_json(json).is_none());
    }

    #[test]
    fn parse_missing_oauth_block_returns_none() {
        let json = r#"{"otherField": "value"}"#;
        assert!(parse_credentials_json(json).is_none());
    }

    #[test]
    fn parse_credentials_invalid_json_returns_none() {
        assert!(parse_credentials_json("not json").is_none());
    }

    #[test]
    fn parse_negative_expires_returns_none() {
        let json = r#"{"claudeAiOauth":{"accessToken":"token","expiresAt":-1}}"#;
        assert!(parse_credentials_json(json).is_none());
    }

    #[test]
    fn parse_no_refresh_token() {
        let future_ms = chrono::Utc::now().timestamp_millis() + 3_600_000;
        let json =
            format!(r#"{{"claudeAiOauth":{{"accessToken":"token","expiresAt":{future_ms}}}}}"#);
        let cred = parse_credentials_json(&json).expect("should parse");
        assert!(cred.refresh_token.is_none());
    }

    #[test]
    fn credential_expiry_check() {
        let cred = ClaudeCredential {
            access_token: "test".to_string(),
            refresh_token: None,
            expires_at: chrono::Utc::now().timestamp_millis() + 3_600_000,
        };
        assert!(!cred.is_expired());

        let expired_cred = ClaudeCredential {
            access_token: "test".to_string(),
            refresh_token: None,
            expires_at: chrono::Utc::now().timestamp_millis() - 3_600_000,
        };
        assert!(expired_cred.is_expired());
    }

    // ── JSONL Parsing ──

    #[test]
    fn parse_text_delta() {
        let line = r#"{"type":"content_block_delta","delta":{"type":"text_delta","text":"Hello"}}"#;
        let event = parse_jsonl_event(line).expect("should parse");
        assert!(matches!(event, StreamEvent::TextDelta(ref t) if t == "Hello"));
    }

    #[test]
    fn parse_thinking_delta() {
        let line = r#"{"type":"content_block_delta","delta":{"type":"thinking_delta","thinking":"Let me think..."}}"#;
        let event = parse_jsonl_event(line).expect("should parse");
        assert!(matches!(event, StreamEvent::ThinkingDelta(ref t) if t == "Let me think..."));
    }

    #[test]
    fn parse_tool_use_start() {
        let line = r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_123","name":"run_shell"}}"#;
        let event = parse_jsonl_event(line).expect("should parse");
        match event {
            StreamEvent::ToolCallDelta {
                index,
                id,
                name,
                arguments_delta,
            } => {
                assert_eq!(index, 0);
                assert_eq!(id.as_deref(), Some("toolu_123"));
                assert_eq!(name.as_deref(), Some("run_shell"));
                assert!(arguments_delta.is_empty());
            }
            other => panic!("expected ToolCallDelta, got {other:?}"),
        }
    }

    #[test]
    fn parse_input_json_delta() {
        let line = r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"cmd\":\"ls\"}"}}"#;
        let event = parse_jsonl_event(line).expect("should parse");
        match event {
            StreamEvent::ToolCallDelta {
                arguments_delta, ..
            } => {
                assert_eq!(arguments_delta, r#"{"cmd":"ls"}"#);
            }
            other => panic!("expected ToolCallDelta, got {other:?}"),
        }
    }

    #[test]
    fn parse_result_success() {
        let line = r#"{"type":"result","subtype":"success","result":"done","session_id":"abc"}"#;
        let event = parse_jsonl_event(line).expect("should parse");
        assert!(matches!(event, StreamEvent::Done));
    }

    #[test]
    fn parse_result_error() {
        let line = r#"{"type":"result","subtype":"error","error":"something broke"}"#;
        let event = parse_jsonl_event(line).expect("should parse");
        assert!(matches!(event, StreamEvent::Error(ref e) if e == "something broke"));
    }

    #[test]
    fn parse_error_event() {
        let line = r#"{"type":"error","error":"auth failed"}"#;
        let event = parse_jsonl_event(line).expect("should parse");
        assert!(matches!(event, StreamEvent::Error(ref e) if e == "auth failed"));
    }

    #[test]
    fn parse_unknown_event_returns_none() {
        let line = r#"{"type":"content_block_stop"}"#;
        assert!(parse_jsonl_event(line).is_none());
    }

    #[test]
    fn parse_system_init_returns_none() {
        let line = r#"{"type":"system","subtype":"init","session_id":"abc"}"#;
        assert!(parse_jsonl_event(line).is_none());
    }

    #[test]
    fn parse_invalid_json_returns_none() {
        assert!(parse_jsonl_event("not json at all").is_none());
    }

    #[test]
    fn parse_text_block_start_with_text() {
        let line = r#"{"type":"content_block_start","content_block":{"type":"text","text":"Hi"}}"#;
        let event = parse_jsonl_event(line).expect("should parse");
        assert!(matches!(event, StreamEvent::TextDelta(ref t) if t == "Hi"));
    }

    #[test]
    fn parse_text_block_start_empty_text_returns_none() {
        let line = r#"{"type":"content_block_start","content_block":{"type":"text","text":""}}"#;
        assert!(parse_jsonl_event(line).is_none());
    }

    // ── Message Assembly ──

    #[test]
    fn assemble_simple_user_message() {
        let msgs = vec![Message::user("Hello, Claude!")];
        let prompt = assemble_prompt(&msgs);
        assert_eq!(prompt.user, "Hello, Claude!");
        assert!(prompt.system.is_empty());
    }

    #[test]
    fn assemble_system_and_user_separated() {
        let msgs = vec![
            Message::system("You are a helpful assistant."),
            Message::user("What is 2+2?"),
        ];
        let prompt = assemble_prompt(&msgs);
        assert_eq!(prompt.system, "You are a helpful assistant.");
        assert_eq!(prompt.user, "What is 2+2?");
    }

    #[test]
    fn assemble_with_assistant_message() {
        let msgs = vec![
            Message::user("Hi"),
            Message::assistant("Hello!"),
            Message::user("How are you?"),
        ];
        let prompt = assemble_prompt(&msgs);
        assert!(prompt.user.contains("[Assistant]: Hello!"));
        assert!(prompt.user.contains("How are you?"));
    }

    #[test]
    fn assemble_empty_messages() {
        let msgs: Vec<Message> = vec![];
        let prompt = assemble_prompt(&msgs);
        assert!(prompt.user.is_empty());
        assert!(prompt.system.is_empty());
    }

    // ── Model Alias Mapping ──

    #[test]
    fn normalize_cli_model_long_ids_to_short() {
        assert_eq!(normalize_cli_model("claude-sonnet-4-6"), "sonnet");
        assert_eq!(normalize_cli_model("claude-opus-4-6"), "opus");
        assert_eq!(normalize_cli_model("claude-haiku-4-5"), "haiku");
    }

    #[test]
    fn normalize_cli_model_short_aliases_passthrough() {
        assert_eq!(normalize_cli_model("sonnet"), "sonnet");
        assert_eq!(normalize_cli_model("opus"), "opus");
        assert_eq!(normalize_cli_model("haiku"), "haiku");
    }

    #[test]
    fn normalize_cli_model_case_insensitive() {
        assert_eq!(normalize_cli_model("CLAUDE-SONNET-4-6"), "sonnet");
        assert_eq!(normalize_cli_model("Claude-Opus-4-6"), "opus");
    }

    #[test]
    fn normalize_cli_model_unknown_passthrough() {
        assert_eq!(
            normalize_cli_model("unknown-model-xyz"),
            "unknown-model-xyz"
        );
        assert_eq!(normalize_cli_model(""), "");
    }

    #[test]
    fn normalize_cli_model_dotted_variants() {
        assert_eq!(normalize_cli_model("sonnet-4.6"), "sonnet");
        assert_eq!(normalize_cli_model("claude-opus-4.6"), "opus");
        assert_eq!(normalize_cli_model("haiku-4.5"), "haiku");
    }

    // ── CLI Detection ──

    #[test]
    fn detect_cli_path_respects_env() {
        // This test just verifies the function doesn't panic
        // Actual CLI detection depends on system state
        let _result = detect_cli_path();
    }
}
