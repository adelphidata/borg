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

use crate::llm::llm_error::{FailoverReason, LlmError};
use crate::llm::StreamEvent;
use crate::tool::computer_use::spawn_stderr_drain;
use crate::types::{Message, MessageContent, Role, ToolDefinition};

/// Find the `claude` CLI binary on the system.
///
/// Checks `CLAUDE_CLI_PATH` first, then PATH, then the usual install
/// locations under the home directory. The last step matters under a
/// service manager: the daemon's plist sets only `HOME`, so its PATH is the
/// bare system default and a user-local `claude` is invisible to `which`.
pub fn detect_cli_path() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("CLAUDE_CLI_PATH") {
        let p = PathBuf::from(&path);
        if p.exists() {
            return Some(p);
        }
    }
    if let Ok(p) = which::which("claude") {
        return Some(p);
    }
    let home = dirs::home_dir()?;
    find_cli_in_known_locations(&home, std::path::Path::is_file)
}

/// Probe the well-known `claude` install locations, in priority order, and
/// return the first one `exists` accepts. Pure over `home` and the
/// existence check so a temp dir can stand in for the real home.
///
/// Order: the native installer (`~/.local/bin`, `~/.claude/local`), Homebrew
/// and `/usr/local`, then the npm-style prefixes (`~/.npm-global`, bun,
/// volta) and finally every nvm node version, newest first.
pub(crate) fn find_cli_in_known_locations(
    home: &Path,
    exists: impl Fn(&Path) -> bool,
) -> Option<PathBuf> {
    let fixed = [
        home.join(".local/bin/claude"),
        home.join(".claude/local/claude"),
        PathBuf::from("/opt/homebrew/bin/claude"),
        PathBuf::from("/usr/local/bin/claude"),
        home.join(".npm-global/bin/claude"),
        home.join(".bun/bin/claude"),
        home.join(".volta/bin/claude"),
    ];
    if let Some(found) = fixed.into_iter().find(|p| exists(p)) {
        return Some(found);
    }
    nvm_node_versions_newest_first(&home.join(".nvm/versions/node"))
        .into_iter()
        .map(|dir| dir.join("bin/claude"))
        .find(|p| exists(p))
}

/// Version directories under an nvm `versions/node` root (`v20.11.0`, …),
/// newest first by numeric version — a lexical sort would rank `v9` above
/// `v20`. Unparseable names sort last, so a stray directory can never
/// shadow a real version.
fn nvm_node_versions_newest_first(root: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut versions: Vec<(Vec<u64>, PathBuf)> = entries
        .filter_map(Result::ok)
        .map(|e| {
            let name = e.file_name();
            let parsed: Vec<u64> = name
                .to_string_lossy()
                .trim_start_matches('v')
                .split('.')
                .map(|part| part.parse().unwrap_or(0))
                .collect();
            (parsed, e.path())
        })
        .collect();
    versions.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    versions.into_iter().map(|(_, path)| path).collect()
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
    /// On a `result` line: `true` for a run that failed, whose subtype is
    /// then `error_max_turns`, `error_during_execution` or the like.
    #[serde(default)]
    is_error: Option<bool>,
    /// On a `result` line: the final text, which for a failed run is the
    /// reason.
    #[serde(default)]
    result: Option<String>,
    #[serde(default)]
    usage: Option<CliUsage>,
    /// `{"type":"stream_event","event":{...}}` — the raw API event the CLI
    /// forwards under `--include-partial-messages`.
    #[serde(default)]
    event: Option<Box<CliEvent>>,
    /// `{"type":"assistant","message":{...}}` — a whole message, which is
    /// all the CLI emits when partial messages are off.
    #[serde(default)]
    message: Option<CliMessage>,
}

#[derive(Debug, Deserialize)]
struct CliMessage {
    #[serde(default)]
    content: Vec<ContentBlock>,
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
    /// Tool arguments on a complete `tool_use` block.
    #[serde(default)]
    input: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct Delta {
    /// Absent on a `message_delta`, whose `delta` only carries the stop
    /// reason; a required field here would drop that line's usage with it.
    #[serde(rename = "type", default)]
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

/// Whether `event` carries reply content, as opposed to usage or a terminator.
fn is_content_delta(event: &StreamEvent) -> bool {
    matches!(
        event,
        StreamEvent::TextDelta(_)
            | StreamEvent::ThinkingDelta(_)
            | StreamEvent::ToolCallDelta { .. }
    )
}

/// Parse a single JSONL line into a `StreamEvent`.
///
/// Returns `None` for events we don't care about (system init, content_block_stop, etc.).
fn parse_jsonl_event(line: &str) -> Option<StreamEvent> {
    let event: CliEvent = serde_json::from_str(line).ok()?;
    event_to_stream(event)
}

/// The content of a complete `assistant` message, as the deltas it stands for.
///
/// `None` for any other line. This is the fallback for a CLI that sends no
/// delta for a message — one that ignores `--include-partial-messages`, say —
/// so the reply arrives in one piece instead of not at all.
fn parse_assistant_message(line: &str) -> Option<Vec<StreamEvent>> {
    let event = serde_json::from_str::<CliEvent>(line).ok()?;
    if event.event_type != "assistant" {
        return None;
    }
    let content = event.message.map(|m| m.content).unwrap_or_default();
    Some(
        content
            .into_iter()
            .enumerate()
            .filter_map(|(index, block)| match block.block_type.as_str() {
                "text" => block
                    .text
                    .filter(|t| !t.is_empty())
                    .map(StreamEvent::TextDelta),
                "tool_use" => Some(StreamEvent::ToolCallDelta {
                    index,
                    id: block.id,
                    name: block.name,
                    arguments_delta: block.input.map(|v| v.to_string()).unwrap_or_default(),
                }),
                _ => None,
            })
            .collect(),
    )
}

fn event_to_stream(event: CliEvent) -> Option<StreamEvent> {
    match event.event_type.as_str() {
        // `--include-partial-messages` wraps each raw API event; the inner
        // one is the shape the arms below already read.
        "stream_event" => event.event.and_then(|inner| event_to_stream(*inner)),
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
        "message_start" | "message_delta" => event.usage.map(|usage| {
            StreamEvent::Usage(crate::llm::UsageData {
                prompt_tokens: usage.input_tokens.unwrap_or(0),
                completion_tokens: usage.output_tokens.unwrap_or(0),
                total_tokens: usage.input_tokens.unwrap_or(0) + usage.output_tokens.unwrap_or(0),
                cached_input_tokens: usage.cache_read_input_tokens.unwrap_or(0),
                cache_creation_tokens: usage.cache_creation_input_tokens.unwrap_or(0),
                provider: "claude-cli".to_string(),
                model: String::new(),
                provider_cost_usd: None,
                response_id: None,
                upstream: None,
            })
        }),
        "result" => {
            // A failed run is `is_error: true` with a subtype such as
            // `error_max_turns` or `error_during_execution`, and says why in
            // `result`; the process still exits 0, so this is the only place
            // the failure shows.
            let failed = event.is_error == Some(true)
                || event
                    .subtype
                    .as_deref()
                    .is_some_and(|s| s.starts_with("error"));
            if failed {
                Some(StreamEvent::Error(
                    event
                        .error
                        .or(event.result)
                        .filter(|s| !s.trim().is_empty())
                        .unwrap_or_else(|| "CLI error".to_string()),
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

/// The least idle time the CLI is given before it counts as wedged.
///
/// The CLI prints nothing while the model reads a long prompt and thinks:
/// no keep-alive, no partial line. An HTTP stream sends bytes during that
/// time; the CLI does not, so the HTTP per-chunk timeout (90s by default)
/// would kill healthy runs. Fifteen minutes covers a long prefill plus a
/// long think and still frees a schedule or subscription lane from a child
/// that will never answer.
pub(crate) const CLI_IDLE_TIMEOUT_FLOOR_SECS: u64 = 900;

/// The idle timeout for one CLI run: the configured per-chunk timeout,
/// raised to the model's reasoning floor and then to
/// [`CLI_IDLE_TIMEOUT_FLOOR_SECS`]. A configured 0 disables it, the same as
/// for HTTP streams.
pub(crate) fn cli_idle_timeout(configured_secs: u64, model: &str) -> Option<std::time::Duration> {
    if configured_secs == 0 {
        return None;
    }
    let secs = configured_secs
        .max(crate::llm::reasoning_timeouts::stale_timeout_floor_secs(
            model,
        ))
        .max(CLI_IDLE_TIMEOUT_FLOOR_SECS);
    Some(std::time::Duration::from_secs(secs))
}

/// The error for a CLI run that went quiet. Fatal with a timeout reason: a
/// same-slot retry would wait out the whole idle window again and re-run any
/// tools the CLI already started, so the fallback chain takes over at once.
fn cli_stall_error(what: &str, idle: std::time::Duration) -> LlmError {
    LlmError::Fatal {
        source: anyhow::anyhow!(
            "Claude CLI {what} for {}s; the process was stopped \
             (raise llm.stream_chunk_timeout_secs, or set it to 0 to disable)",
            idle.as_secs()
        ),
        reason: FailoverReason::Timeout,
        status: None,
    }
}

/// Stop a wedged or abandoned child and reap it. Failure is logged, not
/// returned: the caller is already on an error or cancel path.
///
/// The CLI runs its own tools as subprocesses, so on unix the whole process
/// group (the child leads it, see the spawn) is killed, not just the CLI.
async fn kill_child(child: &mut tokio::process::Child) {
    #[cfg(unix)]
    if let Some(pid) = child.id().and_then(|id| libc::pid_t::try_from(id).ok()) {
        // SAFETY: `killpg` only sends a signal; `pid` is our own child, which
        // leads its own process group, so no unrelated group is signalled.
        if unsafe { libc::killpg(pid, libc::SIGKILL) } != 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() != Some(libc::ESRCH) {
                warn!("failed to stop claude CLI process group: {err}");
            }
        }
    }
    if let Err(e) = child.kill().await {
        warn!("failed to stop claude CLI subprocess: {e}");
    }
}

/// The CLI child, owned so that a caller dropping the stream future mid-run
/// also takes down the tools the CLI started. `kill_on_drop` reaches only the
/// leader; this reaches its process group.
struct CliChild(tokio::process::Child);

impl std::ops::Deref for CliChild {
    type Target = tokio::process::Child;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::ops::DerefMut for CliChild {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl Drop for CliChild {
    fn drop(&mut self) {
        // `id()` is `None` once the child is reaped, so a pid the OS may have
        // reused is never signalled; until then the group id is ours.
        #[cfg(unix)]
        if let Some(pid) = self.0.id().and_then(|id| libc::pid_t::try_from(id).ok()) {
            // SAFETY: `killpg` only sends a signal to our own child's group.
            unsafe {
                libc::killpg(pid, libc::SIGKILL);
            }
        }
    }
}

/// Await `fut`, giving up after `idle` when set. `None` means it elapsed.
async fn within<F: std::future::Future>(
    idle: Option<std::time::Duration>,
    fut: F,
) -> Option<F::Output> {
    match idle {
        Some(limit) => tokio::time::timeout(limit, fut).await.ok(),
        None => Some(fut.await),
    }
}

/// Stream a response from the Claude CLI subprocess.
///
/// Spawns `claude -p --output-format stream-json --model <model>` and reads
/// JSONL from stdout, mapping events to `StreamEvent`s.
///
/// `idle_timeout` bounds both the prompt write to stdin and each wait for
/// the next stdout line (see [`cli_idle_timeout`]); `None` disables it. A run
/// that exceeds it is killed and returns a fatal timeout error, so the
/// fallback chain takes over.
#[allow(clippy::too_many_arguments)]
pub async fn stream_claude_cli(
    cli_path: &Path,
    messages: &[Message],
    _tools: Option<&[ToolDefinition]>,
    model: &str,
    _temperature: f32,
    idle_timeout: Option<std::time::Duration>,
    tx: &mpsc::Sender<StreamEvent>,
    cancel: &CancellationToken,
) -> Result<(), LlmError> {
    let assembled = assemble_prompt(messages);
    if assembled.user.is_empty() && assembled.system.is_empty() {
        return Err(LlmError::Fatal {
            source: anyhow::anyhow!("empty prompt for Claude CLI"),
            reason: FailoverReason::Format,
            status: None,
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
        // Without this the CLI emits one whole `assistant` message per turn
        // and no `content_block_delta`s, so nothing streams and the reply
        // only lands when the turn is over.
        "--include-partial-messages",
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
    cmd.stderr(std::process::Stdio::piped());
    // Its own process group, so `kill_child` also reaches the tools the CLI
    // starts; and never outlive a caller that drops this future.
    #[cfg(unix)]
    cmd.process_group(0);
    cmd.kill_on_drop(true);

    let mut child = CliChild(cmd.spawn().map_err(|e| LlmError::Fatal {
        source: anyhow::anyhow!("failed to spawn claude CLI: {e}"),
        reason: FailoverReason::Unknown,
        status: None,
    })?);

    // Drained on its own task so a chatty CLI can never fill the pipe and
    // stall; only the tail is kept, which is where the CLI puts its reason
    // for exiting.
    let stderr_tail = child.stderr.take().map(spawn_stderr_drain);

    // Write user prompt to stdin
    if let Some(mut stdin) = child.stdin.take() {
        use tokio::io::AsyncWriteExt;
        let stdin_content = if assembled.user.is_empty() {
            &assembled.system // Fall back to system prompt if no user messages
        } else {
            &assembled.user
        };
        // A child that never reads its stdin blocks this write once the
        // pipe buffer fills, so it races cancel and the idle timeout.
        let written = tokio::select! {
            _ = cancel.cancelled() => {
                kill_child(&mut child).await;
                // The receiver may already be gone during shutdown.
                let _ = tx.send(StreamEvent::Done).await;
                return Ok(());
            }
            written = within(idle_timeout, stdin.write_all(stdin_content.as_bytes())) => written,
        };
        match written {
            Some(Ok(())) => {}
            // A child that exits early closes the pipe; its exit status
            // below says why.
            Some(Err(e)) => warn!("failed to write to claude CLI stdin: {e}"),
            None => {
                kill_child(&mut child).await;
                let idle = idle_timeout.unwrap_or_default();
                warn!(
                    idle_secs = idle.as_secs(),
                    "claude CLI did not read its prompt; stopped it"
                );
                return Err(cli_stall_error("did not read its prompt", idle));
            }
        }
        drop(stdin); // Close stdin to signal EOF
    }

    let stdout = child.stdout.take().ok_or_else(|| LlmError::Fatal {
        source: anyhow::anyhow!("failed to capture claude CLI stdout"),
        reason: FailoverReason::Unknown,
        status: None,
    })?;

    let mut reader = BufReader::new(stdout).lines();
    // Whether the message under way arrived as deltas. The complete
    // `assistant` line that closes each message repeats what they carried, so
    // it is only read when there were none, and the flag starts over for the
    // next message.
    let mut saw_delta = false;
    // Whether any reply content was emitted at all, for the exit diagnostics.
    let mut saw_content = false;
    let mut terminal = false;

    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                kill_child(&mut child).await;
                // The Done event signals the receiver, which may itself be
                // dropped during shutdown.
                let _ = tx.send(StreamEvent::Done).await;
                return Ok(());
            }
            line_result = within(idle_timeout, reader.next_line()) => {
                let Some(line_result) = line_result else {
                    kill_child(&mut child).await;
                    let idle = idle_timeout.unwrap_or_default();
                    warn!(
                        idle_secs = idle.as_secs(),
                        "claude CLI sent no output within the idle timeout; stopped it"
                    );
                    return Err(cli_stall_error("sent no output", idle));
                };
                match line_result {
                    Ok(Some(line)) => {
                        let line = line.trim();
                        if line.is_empty() {
                            continue;
                        }
                        let events = match parse_jsonl_event(line) {
                            Some(StreamEvent::Usage(mut usage)) => {
                                // The CLI's usage names no model; the one asked for
                                // is what the ledger and the budget price it under.
                                usage.model = model.to_string();
                                vec![StreamEvent::Usage(usage)]
                            }
                            Some(event) => {
                                saw_delta |= is_content_delta(&event);
                                vec![event]
                            }
                            None => match parse_assistant_message(line) {
                                Some(whole) => {
                                    let fallback = if saw_delta { Vec::new() } else { whole };
                                    saw_delta = false;
                                    fallback
                                }
                                None => Vec::new(),
                            },
                        };
                        for event in events {
                            saw_content |= is_content_delta(&event);
                            terminal |= matches!(event, StreamEvent::Done | StreamEvent::Error(_));
                            if tx.send(event).await.is_err() {
                                debug!("claude CLI: stream receiver dropped");
                                kill_child(&mut child).await;
                                return Ok(());
                            }
                            if terminal {
                                break;
                            }
                        }
                        if terminal {
                            break;
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

    let status = child.wait().await.map_err(|e| LlmError::Retryable {
        source: anyhow::anyhow!("claude CLI wait error: {e}"),
        retry_after: None,
        reason: FailoverReason::Timeout,
        status: None,
    })?;

    let stderr = match &stderr_tail {
        Some(tail) => {
            tail.settle().await;
            tail.text()
        }
        None => String::new(),
    };
    if !status.success() {
        let code = status.code().unwrap_or(-1);
        let detail = if stderr.trim().is_empty() {
            String::new()
        } else {
            format!(": {}", stderr.trim())
        };
        let _ = tx
            .send(StreamEvent::Error(format!(
                "Claude CLI exited with code {code}{detail}"
            )))
            .await;
    } else if !saw_content && !stderr.is_empty() {
        // A clean exit with no reply is the case nobody can diagnose from
        // the UI; the CLI usually said why on stderr.
        warn!(stderr = %stderr.trim(), "claude CLI exited 0 without a reply");
    }

    let _ = tx.send(StreamEvent::Done).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn parse_stream_event_unwraps_the_api_event() {
        // `--include-partial-messages` nests every raw API event under
        // `event`; the text must come out exactly as a bare delta would.
        let line = r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hi there"}},"session_id":"s1","uuid":"u1"}"#;
        match parse_jsonl_event(line) {
            Some(StreamEvent::TextDelta(t)) => assert_eq!(t, "Hi there"),
            other => panic!("expected a text delta, got {other:?}"),
        }
        let usage = r#"{"type":"stream_event","event":{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"input_tokens":2,"output_tokens":11,"cache_read_input_tokens":5,"cache_creation_input_tokens":7}}}"#;
        match parse_jsonl_event(usage) {
            Some(StreamEvent::Usage(u)) => {
                assert_eq!((u.prompt_tokens, u.completion_tokens), (2, 11));
                assert_eq!((u.cached_input_tokens, u.cache_creation_tokens), (5, 7));
            }
            other => panic!("expected usage, got {other:?}"),
        }
    }

    #[test]
    fn complete_assistant_message_is_not_a_stream_event() {
        // Read only through the fallback, or a reply would print twice when
        // the deltas were streamed as well.
        let line = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Hi"}]}}"#;
        assert!(parse_jsonl_event(line).is_none());
    }

    #[test]
    fn failed_run_result_is_an_error_not_done() {
        // A run the CLI could not finish exits 0 and says so only here, with
        // the reason in `result`; `Done` would end the turn as an empty reply.
        for line in [
            r#"{"type":"result","subtype":"error_during_execution","is_error":true,"result":"Credit balance too low"}"#,
            r#"{"type":"result","subtype":"error_max_turns","is_error":true,"result":"Credit balance too low"}"#,
            r#"{"type":"result","subtype":"success","is_error":true,"result":"Credit balance too low"}"#,
        ] {
            match parse_jsonl_event(line) {
                Some(StreamEvent::Error(reason)) => assert_eq!(reason, "Credit balance too low"),
                other => panic!("{line}: expected an error, got {other:?}"),
            }
        }
        let bare = r#"{"type":"result","subtype":"error_during_execution","is_error":true}"#;
        match parse_jsonl_event(bare) {
            Some(StreamEvent::Error(reason)) => assert_eq!(reason, "CLI error"),
            other => panic!("expected a placeholder reason, got {other:?}"),
        }
        let ok = r#"{"type":"result","subtype":"success","is_error":false,"result":"Hi"}"#;
        assert!(matches!(parse_jsonl_event(ok), Some(StreamEvent::Done)));
    }

    #[test]
    fn assistant_message_fallback_yields_text_and_tool_calls() {
        let line = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Checking."},{"type":"text","text":""},{"type":"tool_use","id":"toolu_1","name":"run_shell","input":{"command":"ls"}}]}}"#;
        let events = parse_assistant_message(line).expect("an assistant line");
        assert_eq!(events.len(), 2, "empty text blocks are skipped: {events:?}");
        match &events[0] {
            StreamEvent::TextDelta(t) => assert_eq!(t, "Checking."),
            other => panic!("expected text first, got {other:?}"),
        }
        match &events[1] {
            StreamEvent::ToolCallDelta {
                index,
                id,
                name,
                arguments_delta,
            } => {
                assert_eq!(*index, 2);
                assert_eq!(id.as_deref(), Some("toolu_1"));
                assert_eq!(name.as_deref(), Some("run_shell"));
                assert_eq!(arguments_delta, r#"{"command":"ls"}"#);
            }
            other => panic!("expected a tool call, got {other:?}"),
        }
    }

    #[test]
    fn assistant_message_fallback_ignores_other_lines() {
        for line in [
            r#"{"type":"system","subtype":"init"}"#,
            r#"{"type":"result","subtype":"success"}"#,
            r#"{"type":"user","message":{"content":[{"type":"text","text":"echo"}]}}"#,
            "not json",
        ] {
            assert!(parse_assistant_message(line).is_none(), "{line}");
        }
        // An assistant line with nothing to show is still an assistant line:
        // it closes the message, so the per-message delta flag can reset.
        let empty = r#"{"type":"assistant","message":{"role":"assistant","content":[]}}"#;
        assert!(matches!(parse_assistant_message(empty), Some(v) if v.is_empty()));
    }

    #[test]
    fn content_deltas_are_what_marks_a_reply_as_streamed() {
        assert!(is_content_delta(&StreamEvent::TextDelta("x".into())));
        assert!(is_content_delta(&StreamEvent::ThinkingDelta("x".into())));
        assert!(is_content_delta(&StreamEvent::ToolCallDelta {
            index: 0,
            id: None,
            name: None,
            arguments_delta: String::new(),
        }));
        assert!(!is_content_delta(&StreamEvent::Done));
        assert!(!is_content_delta(&StreamEvent::Error("x".into())));
    }

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

    /// Existence check confined to the temp home, so a real `/opt/homebrew/bin/claude`
    /// on the developer machine cannot leak into the assertions.
    fn touch(path: &Path) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, b"#!/bin/sh\n").unwrap();
    }

    #[test]
    fn known_locations_probe_finds_user_local_install_without_path() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let under_home = |p: &Path| p.starts_with(home) && p.is_file();

        assert_eq!(find_cli_in_known_locations(home, under_home), None);

        touch(&home.join(".bun/bin/claude"));
        assert_eq!(
            find_cli_in_known_locations(home, under_home),
            Some(home.join(".bun/bin/claude"))
        );

        // The native installer location outranks a package-manager shim.
        touch(&home.join(".local/bin/claude"));
        assert_eq!(
            find_cli_in_known_locations(home, under_home),
            Some(home.join(".local/bin/claude"))
        );
    }

    #[test]
    fn known_locations_probe_picks_newest_nvm_node_numerically() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let under_home = |p: &Path| p.starts_with(home) && p.is_file();
        let nvm = home.join(".nvm/versions/node");
        // v9 sorts after v20 lexically; v22 has no claude and must be skipped.
        for v in ["v9.11.2", "v18.20.4", "v20.11.0"] {
            touch(&nvm.join(v).join("bin/claude"));
        }
        std::fs::create_dir_all(nvm.join("v22.1.0/bin")).unwrap();
        std::fs::create_dir_all(nvm.join("not-a-version/bin")).unwrap();
        touch(&nvm.join("not-a-version/bin/claude"));

        assert_eq!(
            find_cli_in_known_locations(home, under_home),
            Some(nvm.join("v20.11.0/bin/claude"))
        );
    }

    #[test]
    fn cli_idle_timeout_floors_and_disables() {
        // (configured, model, expected seconds; None = disabled)
        let cases: &[(u64, &str, Option<u64>)] = &[
            // 0 disables, as it does for HTTP streams.
            (0, "opus", None),
            // The 90s HTTP default is far below a silent prefill: floored.
            (90, "opus", Some(CLI_IDLE_TIMEOUT_FLOOR_SECS)),
            // A higher configured value is never lowered.
            (1_800, "sonnet", Some(1_800)),
        ];
        for &(configured, model, want) in cases {
            assert_eq!(
                cli_idle_timeout(configured, model).map(|d| d.as_secs()),
                want,
                "configured={configured} model={model}"
            );
        }
    }

    /// A fake `claude` binary: a shell script that first writes its pid to
    /// `pid_file`, then runs `body`. `exec` in `body` keeps that pid.
    #[cfg(unix)]
    fn fake_cli(dir: &Path, body: &str) -> (PathBuf, PathBuf) {
        use std::os::unix::fs::PermissionsExt;
        let pid_file = dir.join("pid");
        let script = dir.join("claude");
        std::fs::write(
            &script,
            format!("#!/bin/sh\necho $$ > '{}'\n{body}\n", pid_file.display()),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        (script, pid_file)
    }

    #[cfg(unix)]
    fn process_alive(pid_file: &Path) -> bool {
        let pid = std::fs::read_to_string(pid_file).expect("fake CLI wrote its pid");
        std::process::Command::new("kill")
            .args(["-0", pid.trim()])
            .status()
            .unwrap()
            .success()
    }

    #[cfg(unix)]
    async fn run_fake(
        script: &Path,
        prompt: String,
        idle: Option<std::time::Duration>,
    ) -> (Result<(), LlmError>, Vec<StreamEvent>) {
        let (tx, mut rx) = mpsc::channel(64);
        let cancel = CancellationToken::new();
        let messages = [Message::user(prompt)];
        // A generous outer bound: if the idle timeout fails, the test fails
        // instead of hanging the suite.
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(20),
            stream_claude_cli(script, &messages, None, "sonnet", 0.0, idle, &tx, &cancel),
        )
        .await
        .expect("stream_claude_cli must return once the idle timeout passes");
        drop(tx);
        let mut events = Vec::new();
        while let Some(event) = rx.recv().await {
            events.push(event);
        }
        (result, events)
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn silent_cli_is_killed_and_falls_back_after_the_idle_timeout() {
        let dir = tempfile::tempdir().unwrap();
        // Reads its prompt, then never prints a line.
        let (script, pid_file) = fake_cli(dir.path(), "cat > /dev/null\nexec sleep 30");
        let (result, events) = run_fake(
            &script,
            "hello".into(),
            Some(std::time::Duration::from_millis(1_500)),
        )
        .await;
        let err = result.expect_err("a silent CLI must fail, not hang");
        assert!(
            !err.is_retryable() && err.should_fallback(),
            "a stall skips same-slot retries and goes to the fallback chain: {err:?}"
        );
        assert_eq!(err.reason(), FailoverReason::Timeout);
        assert!(err.to_string().contains("sent no output"), "{err}");
        assert!(!process_alive(&pid_file), "the wedged child must be killed");
        assert!(
            !events.iter().any(|e| matches!(e, StreamEvent::Done)),
            "a stalled run must not look complete: {events:?}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cli_that_never_reads_its_prompt_is_killed_after_the_idle_timeout() {
        let dir = tempfile::tempdir().unwrap();
        // Never reads stdin, so a prompt larger than the pipe buffer blocks
        // the write.
        let (script, pid_file) = fake_cli(dir.path(), "exec sleep 30");
        let (result, _) = run_fake(
            &script,
            "x".repeat(4 * 1024 * 1024),
            Some(std::time::Duration::from_millis(1_500)),
        )
        .await;
        let err = result.expect_err("a blocked prompt write must fail, not hang");
        assert_eq!(err.reason(), FailoverReason::Timeout);
        assert!(!err.is_retryable() && err.should_fallback(), "{err:?}");
        assert!(err.to_string().contains("did not read its prompt"), "{err}");
        assert!(!process_alive(&pid_file), "the wedged child must be killed");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_stalled_cli_takes_the_tools_it_started_down_with_it() {
        let dir = tempfile::tempdir().unwrap();
        let grandchild = dir.path().join("grandchild");
        // Starts a long-running tool of its own, then goes quiet.
        let (script, pid_file) = fake_cli(
            dir.path(),
            &format!(
                "cat > /dev/null\nsleep 30 &\necho $! > '{}'\nwait",
                grandchild.display()
            ),
        );
        let (result, _) = run_fake(
            &script,
            "hello".into(),
            Some(std::time::Duration::from_millis(1_500)),
        )
        .await;
        assert_eq!(result.expect_err("stall").reason(), FailoverReason::Timeout);
        assert!(!process_alive(&pid_file), "the CLI must be killed");
        // SIGKILL is delivered at once, but the orphan is reaped by init;
        // allow it a moment to disappear from the process table.
        let mut alive = true;
        for _ in 0..50 {
            alive = process_alive(&grandchild);
            if !alive {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        assert!(!alive, "a tool the CLI started must not outlive it");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dropping_the_stream_future_takes_the_cli_tools_down() {
        let dir = tempfile::tempdir().unwrap();
        let grandchild = dir.path().join("grandchild");
        let (script, pid_file) = fake_cli(
            dir.path(),
            &format!(
                "cat > /dev/null\nsleep 30 &\necho $! > '{}'\nwait",
                grandchild.display()
            ),
        );
        let (tx, _rx) = mpsc::channel(64);
        let cancel = CancellationToken::new();
        let messages = [Message::user("hello".to_string())];
        // No idle timeout: only the caller giving up ends this run.
        let run = stream_claude_cli(&script, &messages, None, "sonnet", 0.0, None, &tx, &cancel);
        let gave_up = tokio::time::timeout(std::time::Duration::from_millis(1_500), run).await;
        assert!(gave_up.is_err(), "the fake CLI never answers");
        let mut alive = true;
        for _ in 0..50 {
            alive = process_alive(&pid_file) || process_alive(&grandchild);
            if !alive {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        assert!(
            !alive,
            "neither the CLI nor its tools may outlive a dropped run"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn prompt_cli_finishes_normally_under_the_idle_timeout() {
        // Negative control: a CLI that answers within the timeout completes.
        let dir = tempfile::tempdir().unwrap();
        let (script, _) = fake_cli(
            dir.path(),
            r#"cat > /dev/null
echo '{"type":"result","subtype":"success","result":"done"}'"#,
        );
        let (result, events) = run_fake(
            &script,
            "hello".into(),
            Some(std::time::Duration::from_secs(5)),
        )
        .await;
        result.expect("a CLI that answers in time must succeed");
        assert!(
            events.iter().any(|e| matches!(e, StreamEvent::Done)),
            "{events:?}"
        );
        assert!(
            !events.iter().any(|e| matches!(e, StreamEvent::Error(_))),
            "{events:?}"
        );
    }
}
