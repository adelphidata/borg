//! `computer_use` tool — macOS-only screen capture and (eventually) UI control.
//!
//! Spawns the external `cua-driver` binary as an MCP-over-stdio child process
//! per tool call and speaks newline-delimited JSON-RPC 2.0 to it. We
//! intentionally do **not** pull in a full MCP client library: the framing is
//! one line per message and a tiny initialize → tools/call → terminate dance.
//! Mirroring `ndjson_codec` keeps the surface area small enough to audit at a
//! glance.
//!
//! ## Why this tool exists
//!
//! `cua-driver` is the host-side automation contract Borg already documents in
//! the user-facing skill (`~/.claude/skills/cua-driver/`). The tool surfaces it
//! to the LLM so the agent can capture the screen, enumerate apps, or wait
//! without shelling out via `run_shell` — the latter would skip the no-
//! foreground contract and the plan-mode allowlist.
//!
//! ## Action allowlist
//!
//! Read-only actions (`capture`, `list_apps`, `wait`) are allowed in Plan
//! mode. Mutating actions (`click`, `type`, …) are added in a follow-up commit
//! and stay blocked in Plan mode.
//!
//! ## Test seam
//!
//! Production code spawns the real binary; tests inject a [`Driver`] trait
//! impl over a `Mutex<VecDeque<DriverResponse>>` so the handler exercises the
//! envelope-construction and denylist paths without launching anything.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::types::{ContentPart, MediaData, ToolOutput};

/// The action discriminator. New variants land in this enum first so the
/// `serde` rejection happens at parse time, not deep in the handler.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ComputerUseAction {
    // ── Read-only (plan-mode-safe) ──
    /// Capture the screen as a PNG. Returns a multimodal envelope so the
    /// model gets pixels on the next turn.
    Capture,
    /// List the running macOS applications with pid + name.
    ListApps,
    /// Block for `duration_ms` milliseconds. Useful for waiting on
    /// animations or async UI redraws between captures.
    Wait,

    // ── Mutating (blocked in plan mode) ──
    /// Single left-click at `(x, y)` pixel coordinates in the screen frame.
    Click,
    /// Double-click at `(x, y)`.
    DoubleClick,
    /// Right-click at `(x, y)`.
    RightClick,
    /// Middle-click at `(x, y)`.
    MiddleClick,
    /// Click-and-drag from `(from_x, from_y)` to `(to_x, to_y)`.
    Drag,
    /// Scroll at `(x, y)` by `(delta_x, delta_y)`.
    Scroll,
    /// Type literal text via the keyboard. Goes through the hard denylist
    /// so fork-bomb-shaped payloads can't reach the OS.
    Type,
    /// Press a single named key (e.g. "Return", "Escape") or a hotkey
    /// described by `keys` (e.g. ["cmd", "s"]). Hotkeys hit the denylist.
    Key,
    /// Replace an AX-element's `AXValue` directly.
    SetValue,
    /// Bring an app to the foreground or route input without raising. When
    /// `raise_window` is `false`, focus is preserved — see the no-foreground
    /// contract in `docs/computer-use.md`.
    FocusApp,
}

impl ComputerUseAction {
    /// True for the three actions that don't mutate host state.
    pub(crate) const fn is_readonly(&self) -> bool {
        matches!(
            self,
            ComputerUseAction::Capture | ComputerUseAction::ListApps | ComputerUseAction::Wait
        )
    }
}

/// Capture mode. `Vision` keeps the current behaviour (raw PNG inline).
/// `Som` (Set-of-Marks) overlays numbered boxes the model can reference
/// by `element_index` on the next turn. `Ax` returns the
/// accessibility tree as text only — for non-vision models or when the
/// operator wants to keep image bytes out of LLM context.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CaptureMode {
    /// Plain screenshot. The historical default — bytes go straight to the LLM.
    #[default]
    Vision,
    /// Screenshot plus a JSON overlay of `{index, role, name, bounds}`.
    /// Pair with `element_index` on a follow-up click/drag.
    Som,
    /// Accessibility tree only — no image bytes. Useful with text-only models.
    Ax,
}

/// Args for `computer_use`. The schema fans out per-action — `duration_ms`
/// is only meaningful for `Wait`, `x`/`y` for click family, etc. We leave
/// them all optional and validate per-action so the LLM gets a single
/// predictable schema. Per-action required-field checks happen inside the
/// handler so the error path stays as one place to look.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ComputerUseArgs {
    pub action: ComputerUseAction,

    /// Capture only. Optional `pid` to capture a specific app's frontmost
    /// window (delegates to cua-driver's window-targeted capture). When
    /// `None`, captures the full screen.
    #[serde(default)]
    pub pid: Option<i64>,

    /// Capture / focus_app only. Optional `window_id` paired with `pid`.
    #[serde(default)]
    pub window_id: Option<i64>,

    /// Wait only. Sleep duration in milliseconds. Required when
    /// `action == Wait`.
    #[serde(default)]
    pub duration_ms: Option<u64>,

    /// Click family + drag start. Pixel x.
    #[serde(default)]
    pub x: Option<i32>,
    /// Click family + drag start. Pixel y.
    #[serde(default)]
    pub y: Option<i32>,
    /// Drag end. Pixel x.
    #[serde(default)]
    pub to_x: Option<i32>,
    /// Drag end. Pixel y.
    #[serde(default)]
    pub to_y: Option<i32>,
    /// Scroll deltas.
    #[serde(default)]
    pub delta_x: Option<i32>,
    #[serde(default)]
    pub delta_y: Option<i32>,

    /// `type` literal text. The denylist scans this before sending.
    #[serde(default)]
    pub text: Option<String>,

    /// `key` single named key, e.g. "Return". Mutually exclusive with `keys`.
    #[serde(default)]
    pub key: Option<String>,
    /// `key` hotkey combo, e.g. `["cmd", "s"]`. Lowercase modifier names.
    #[serde(default)]
    pub keys: Option<Vec<String>>,

    /// `set_value` element index (from cua-driver's AX cache).
    #[serde(default)]
    pub element_index: Option<i32>,
    /// `set_value` new value.
    #[serde(default)]
    pub value: Option<String>,

    /// `focus_app` target — accepts a bundle id ("com.apple.Finder"),
    /// a process name, or a pid (string-encoded).
    #[serde(default)]
    pub target: Option<String>,
    /// `focus_app` — when `false`, route input without stealing focus from
    /// the user's frontmost app. Default `true` (safe — raises and activates).
    #[serde(default)]
    pub raise_window: Option<bool>,

    /// `capture` only — selects between `vision` (default, PNG inline),
    /// `som` (image + numbered overlay), and `ax` (text-only). Optional
    /// everywhere else.
    #[serde(default)]
    pub mode: Option<CaptureMode>,

    /// Mutating actions only (click family + drag + scroll). When `true`,
    /// after the action succeeds the handler issues a follow-up driver
    /// `screenshot` and appends it to the multimodal envelope. Lets the
    /// model verify state without burning a second tool-call turn.
    /// Refused for `capture` itself (would be redundant) and never fires
    /// when the action is denied by the hardcoded denylist.
    #[serde(default)]
    pub capture_after: Option<bool>,
}

/// JSON-RPC response envelope from cua-driver.
#[derive(Debug, Clone)]
pub(crate) struct DriverResponse {
    /// The MCP `result.content` array — typed loosely as JSON so the handler
    /// can pick out `image` parts vs `text` parts without committing to a
    /// strict enum that drifts from upstream.
    pub content: Vec<DriverContent>,
}

/// One element of `result.content`. MCP uses a `{type, ...}` shape — we
/// preserve the discriminator so the multimodal envelope round-trips faithfully.
#[derive(Debug, Clone)]
pub(crate) enum DriverContent {
    Text(String),
    Image { mime_type: String, base64: String },
}

/// Trait the handler dispatches against. The production impl spawns
/// `cua-driver mcp`; tests inject an in-memory queue.
#[async_trait]
pub(crate) trait Driver: Send + Sync {
    async fn call(&self, method: &str, params: Value) -> Result<DriverResponse>;
}

/// Real driver: spawns `cua-driver mcp` per call, speaks MCP-stdio.
struct CuaDriver {
    binary: String,
}

impl CuaDriver {
    fn new(binary: String) -> Self {
        Self { binary }
    }
}

#[async_trait]
impl Driver for CuaDriver {
    async fn call(&self, method: &str, params: Value) -> Result<DriverResponse> {
        use tokio::io::{AsyncBufReadExt, BufReader};
        use tokio::process::Command;

        let mut child = Command::new(&self.binary)
            .arg("mcp")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("spawn `{}` mcp", self.binary))?;

        let mut stdin = child.stdin.take().context("driver stdin missing")?;
        let stdout = child.stdout.take().context("driver stdout missing")?;
        let mut reader = BufReader::new(stdout).lines();

        // MCP handshake — `initialize`, then immediately the tool call.
        // Newline-delimited JSON, one message per line.
        let init = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "borg", "version": env!("CARGO_PKG_VERSION")},
            }
        });
        write_line(&mut stdin, &init).await?;
        // Drain the initialize response — we don't currently inspect it, but
        // skipping the read would leave the driver waiting for backpressure.
        let _ = reader.next_line().await?;

        let initialized = json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized",
            "params": {}
        });
        write_line(&mut stdin, &initialized).await?;

        let call = json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {"name": method, "arguments": params}
        });
        write_line(&mut stdin, &call).await?;

        let response_line = reader
            .next_line()
            .await?
            .context("driver closed stdout before reply")?;
        // Make the child wind down: closing stdin signals graceful shutdown
        // per MCP spec; `kill_on_drop` guards against a hung driver.
        drop(stdin);
        let _ = child.wait().await;

        parse_response(&response_line)
    }
}

async fn write_line<W: tokio::io::AsyncWriteExt + Unpin>(w: &mut W, payload: &Value) -> Result<()> {
    let mut bytes = serde_json::to_vec(payload)?;
    bytes.push(b'\n');
    w.write_all(&bytes).await?;
    w.flush().await?;
    Ok(())
}

fn parse_response(line: &str) -> Result<DriverResponse> {
    let v: Value = serde_json::from_str(line).context("driver reply was not JSON")?;
    if let Some(err) = v.get("error") {
        anyhow::bail!("cua-driver error: {err}");
    }
    let content = v
        .get("result")
        .and_then(|r| r.get("content"))
        .and_then(|c| c.as_array())
        .cloned()
        .unwrap_or_default();
    let mut parts = Vec::with_capacity(content.len());
    for item in content {
        let typ = item.get("type").and_then(|t| t.as_str()).unwrap_or("");
        match typ {
            "text" => {
                let text = item
                    .get("text")
                    .and_then(|t| t.as_str())
                    .unwrap_or("")
                    .to_string();
                parts.push(DriverContent::Text(text));
            }
            "image" => {
                let mime = item
                    .get("mimeType")
                    .and_then(|m| m.as_str())
                    .unwrap_or("image/png")
                    .to_string();
                let data = item
                    .get("data")
                    .and_then(|d| d.as_str())
                    .unwrap_or("")
                    .to_string();
                parts.push(DriverContent::Image {
                    mime_type: mime,
                    base64: data,
                });
            }
            _ => {
                // Unknown content type — preserve it as text so the model
                // still has a chance to act on the description.
                parts.push(DriverContent::Text(item.to_string()));
            }
        }
    }
    Ok(DriverResponse { content: parts })
}

/// Public entry point used by the agent loop's tool dispatcher.
///
/// `cancel` is the turn's interrupt token: when the user interrupts mid-turn,
/// a blocking `Wait` returns early instead of pinning the worker for up to its
/// full (capped) duration. Other actions are short driver round-trips and are
/// left untouched.
pub(crate) async fn handle(
    args: &ComputerUseArgs,
    config: &Config,
    cancel: &CancellationToken,
) -> Result<ToolOutput> {
    if !config.computer_use.enabled {
        return Ok(ToolOutput::Text(
            "Error: computer_use is disabled. Enable it via `/settings` (computer_use.enabled = true) — note the driver only ships for macOS today.".into(),
        ));
    }

    // `Wait` needs no driver and is the only long-blocking action, so handle it
    // here with interruption wired in rather than spinning up a `cua-driver`
    // child just to sleep.
    if matches!(args.action, ComputerUseAction::Wait) {
        return wait(args, Some(cancel)).await;
    }

    let driver = build_driver(config)?;
    handle_with_driver(args, driver.as_ref()).await
}

/// Test seam: the handler logic without the production driver. Production
/// callers go through [`handle`].
pub(crate) async fn handle_with_driver(
    args: &ComputerUseArgs,
    driver: &dyn Driver,
) -> Result<ToolOutput> {
    // Pre-driver argument sanity. `capture_after` on `capture` is
    // structurally meaningless — refuse here rather than silently take a
    // second screenshot. Equally, `capture_after` with `mode = ax` would
    // be paradoxical (no image to capture).
    if let Some(reason) = check_arg_coherence(args) {
        return Ok(ToolOutput::Text(format!("Error: {reason}")));
    }
    // Hard denylist — runs BEFORE we touch the driver so a malicious or
    // confused LLM can't burn its budget probing the denylist via real
    // keystrokes. Mirrors `policy::HARDCODED_DENY` in spirit. When this
    // fires, `capture_after` is never honoured.
    if let Some(reason) = check_denylist(args) {
        return Ok(ToolOutput::Text(format!("Error: {reason}")));
    }
    let result = match args.action {
        ComputerUseAction::Capture => return capture(args, driver).await,
        ComputerUseAction::ListApps => return list_apps(driver).await,
        ComputerUseAction::Wait => return wait(args, None).await,
        ComputerUseAction::Click => click(args, "click", driver).await,
        ComputerUseAction::DoubleClick => click(args, "double_click", driver).await,
        ComputerUseAction::RightClick => click(args, "right_click", driver).await,
        ComputerUseAction::MiddleClick => click(args, "middle_click", driver).await,
        ComputerUseAction::Drag => drag(args, driver).await,
        ComputerUseAction::Scroll => scroll(args, driver).await,
        ComputerUseAction::Type => type_text(args, driver).await,
        ComputerUseAction::Key => key(args, driver).await,
        ComputerUseAction::SetValue => set_value(args, driver).await,
        ComputerUseAction::FocusApp => focus_app(args, driver).await,
    };
    let primary = result?;
    if args.capture_after.unwrap_or(false) {
        // Tack on a fresh capture. We never fail the original action on
        // a follow-up capture error — the action did succeed, the model
        // just doesn't get pixels this turn. Log + continue per the
        // No-Silent-Swallowing invariant.
        let follow = capture_internal(args, driver).await;
        Ok(append_capture(primary, follow))
    } else {
        Ok(primary)
    }
}

/// Returns `Some(reason)` when arg combinations are structurally invalid
/// — caught pre-driver so the model gets a deterministic message rather
/// than discovering it from an opaque driver error.
fn check_arg_coherence(args: &ComputerUseArgs) -> Option<String> {
    // `capture_after` only applies to mutating actions that produce a
    // screen change worth verifying.
    if args.capture_after.unwrap_or(false) {
        match args.action {
            ComputerUseAction::Capture | ComputerUseAction::ListApps | ComputerUseAction::Wait => {
                return Some(format!(
                    "capture_after is not valid for action '{:?}' — only mutating actions take a follow-up capture",
                    args.action
                ));
            }
            _ => {}
        }
    }
    // `mode = ax` returns no image, so a follow-up capture wouldn't have
    // pixels either — but capture_after dispatches `screenshot` which is
    // always vision-shaped; the contradiction is only when the *initial*
    // action is `capture` with `mode = ax` AND capture_after is true, and
    // that already failed the check above.

    // Click family with element_index AND coordinates: ambiguous target.
    let has_coords = args.x.is_some() || args.y.is_some();
    let has_index = args.element_index.is_some();
    if has_coords && has_index {
        match args.action {
            ComputerUseAction::Click
            | ComputerUseAction::DoubleClick
            | ComputerUseAction::RightClick
            | ComputerUseAction::MiddleClick
            | ComputerUseAction::Drag
            | ComputerUseAction::Scroll => {
                return Some(format!(
                    "{:?}: provide either pixel `x`/`y` or `element_index`, not both",
                    args.action
                ));
            }
            _ => {}
        }
    }
    None
}

/// Merge a follow-up capture into the primary action's output. The merge
/// always returns a Multimodal envelope when the capture produced an image,
/// so the model sees pixels alongside the action's summary.
fn append_capture(primary: ToolOutput, follow: Result<ToolOutput>) -> ToolOutput {
    let follow_out = match follow {
        Ok(o) => o,
        Err(e) => {
            tracing::warn!("capture_after follow-up failed: {e}");
            return match primary {
                ToolOutput::Text(t) => {
                    ToolOutput::Text(format!("{t}\n[capture_after failed: {e}]"))
                }
                ToolOutput::Multimodal { text, parts } => ToolOutput::Multimodal {
                    text: format!("{text}\n[capture_after failed: {e}]"),
                    parts,
                },
            };
        }
    };
    let (primary_text, mut primary_parts) = match primary {
        ToolOutput::Text(t) => (t, Vec::new()),
        ToolOutput::Multimodal { text, parts } => (text, parts),
    };
    let (follow_text, follow_parts) = match follow_out {
        ToolOutput::Text(t) => (t, Vec::new()),
        ToolOutput::Multimodal { text, parts } => (text, parts),
    };
    let merged_text = if follow_text.is_empty() {
        primary_text
    } else {
        format!("{primary_text}\n--- capture_after ---\n{follow_text}")
    };
    primary_parts.extend(follow_parts);
    if primary_parts.is_empty() {
        ToolOutput::Text(merged_text)
    } else {
        ToolOutput::Multimodal {
            text: merged_text,
            parts: primary_parts,
        }
    }
}

/// Internal capture used by `capture_after`. Always runs in `vision` mode
/// so the operator's chosen primary-action mode (if any) doesn't surprise
/// the follow-up shape.
async fn capture_internal(args: &ComputerUseArgs, driver: &dyn Driver) -> Result<ToolOutput> {
    let mut probe = ComputerUseArgs {
        action: ComputerUseAction::Capture,
        pid: args.pid,
        window_id: args.window_id,
        duration_ms: None,
        x: None,
        y: None,
        to_x: None,
        to_y: None,
        delta_x: None,
        delta_y: None,
        text: None,
        key: None,
        keys: None,
        element_index: None,
        value: None,
        target: None,
        raise_window: None,
        mode: Some(CaptureMode::Vision),
        capture_after: None,
    };
    // Defensive: never recurse capture_after.
    probe.capture_after = None;
    capture(&probe, driver).await
}

fn build_driver(config: &Config) -> Result<Arc<dyn Driver>> {
    let binary = if config.computer_use.driver_path.is_empty() {
        // `which` semantics — fail fast with a clear message if the operator
        // hasn't installed cua-driver. We don't `command -v` shell out
        // because that hits the policy layer and we'd lose the specific error.
        which::which("cua-driver")
            .map(|p| p.to_string_lossy().to_string())
            .context("`cua-driver` not found on PATH. Install it (`brew install cua-driver` on macOS) or set computer_use.driver_path in `/settings`.")?
    } else {
        config.computer_use.driver_path.clone()
    };
    Ok(Arc::new(CuaDriver::new(binary)))
}

async fn capture(args: &ComputerUseArgs, driver: &dyn Driver) -> Result<ToolOutput> {
    let mode = args.mode.unwrap_or_default();
    let mut params = json!({"mode": mode_str(mode)});
    if let Some(pid) = args.pid {
        params["pid"] = json!(pid);
    }
    if let Some(window_id) = args.window_id {
        params["window_id"] = json!(window_id);
    }
    let method = match mode {
        // `ax` returns the AX tree as text — cua-driver exposes it under
        // `accessibility` per the SOM/AX split documented in the skill.
        CaptureMode::Ax => "accessibility",
        // SOM and Vision both come back from `screenshot`; SOM additionally
        // emits a text part with the numbered overlay JSON.
        CaptureMode::Som | CaptureMode::Vision => "screenshot",
    };
    let resp = driver.call(method, params).await?;

    let mut text_parts = Vec::new();
    let mut image: Option<(String, String)> = None;
    for part in resp.content {
        match part {
            DriverContent::Text(t) => text_parts.push(t),
            DriverContent::Image { mime_type, base64 } => image = Some((mime_type, base64)),
        }
    }

    let summary = if text_parts.is_empty() {
        "Captured screen.".to_string()
    } else {
        text_parts.join("\n")
    };

    // `ax` mode: text-only by contract. If the driver also sent an image
    // (some builds do), drop it on the floor — the mode's whole purpose
    // is keeping bytes out of LLM context.
    if mode == CaptureMode::Ax {
        return Ok(ToolOutput::Text(summary));
    }

    let Some((mime_type, data)) = image else {
        // Vision/SOM mode promised pixels; surfacing this loudly preserves
        // the "if we report success it must succeed" invariant.
        return Ok(ToolOutput::Text(format!(
            "Error: cua-driver returned no image for capture (mode={mode:?}). Detail: {summary}"
        )));
    };

    Ok(ToolOutput::Multimodal {
        text: summary.clone(),
        parts: vec![
            ContentPart::Text(summary),
            ContentPart::ImageBase64 {
                media: MediaData {
                    mime_type,
                    data,
                    filename: Some("capture.png".to_string()),
                },
            },
        ],
    })
}

fn mode_str(mode: CaptureMode) -> &'static str {
    match mode {
        CaptureMode::Vision => "vision",
        CaptureMode::Som => "som",
        CaptureMode::Ax => "ax",
    }
}

async fn list_apps(driver: &dyn Driver) -> Result<ToolOutput> {
    let resp = driver.call("list_apps", json!({})).await?;
    // The driver returns its own pretty-rendered text for `list_apps`; we
    // surface it verbatim so the model sees what the human user would.
    let combined: String = resp
        .content
        .into_iter()
        .filter_map(|p| match p {
            DriverContent::Text(t) => Some(t),
            DriverContent::Image { .. } => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    Ok(ToolOutput::Text(combined))
}

/// Cap at 5 minutes so a runaway agent loop can't pin the worker
/// indefinitely. The driver itself doesn't gate this — we own the budget.
const MAX_WAIT_MS: u64 = 5 * 60 * 1000;

/// Clamp a requested wait to the worker budget. Pulled out so the cap is
/// unit-testable without actually sleeping for the duration.
fn effective_wait_ms(requested: u64) -> u64 {
    requested.min(MAX_WAIT_MS)
}

async fn wait(args: &ComputerUseArgs, cancel: Option<&CancellationToken>) -> Result<ToolOutput> {
    let ms = args.duration_ms.ok_or_else(|| {
        anyhow::anyhow!("computer_use.wait requires `duration_ms` (milliseconds to sleep)")
    })?;
    let effective = effective_wait_ms(ms);
    let sleep = tokio::time::sleep(Duration::from_millis(effective));
    let interrupted = match cancel {
        // Interruptible path: a turn interrupt cuts the wait short rather than
        // blocking the agent for the full (capped) duration. The token may
        // already be cancelled before we get here (interrupt raced the tool
        // boundary); `select!` resolves that branch immediately, so the
        // signal is never lost.
        Some(token) => tokio::select! {
            () = sleep => false,
            () = token.cancelled() => true,
        },
        // No interrupt channel (e.g. the test seam): preserve the original
        // blocking semantics.
        None => {
            sleep.await;
            false
        }
    };
    Ok(ToolOutput::Text(if interrupted {
        "wait interrupted by new input".to_string()
    } else {
        format!("waited {effective}ms")
    }))
}

/// True iff the given action is allowed when the agent is in Plan mode.
/// Plan mode blocks anything that mutates the host; the three read-only
/// actions here pass and everything else is blocked.
pub(crate) const fn action_allowed_in_plan_mode(action: &ComputerUseAction) -> bool {
    action.is_readonly()
}

/// Hotkey combos that are always blocked. The strings match a normalised
/// form (modifiers lowercased, sorted alphabetically, joined by `+`) so
/// trivial reorderings ("shift+cmd+q" vs "cmd+shift+q") collapse to the
/// same key. Patterns mirror the catastrophic-command list in
/// `crate::policy::HARDCODED_DENY` — different surface, same idea: refuse
/// to even try to run the action.
const DENIED_HOTKEYS: &[(&str, &str)] = &[
    // Entries MUST be in the normalised form produced by `normalised_hotkey`:
    // lowercased, modifier aliases collapsed (option/alt → opt, command → cmd,
    // control → ctrl, escape → esc), then sorted alphabetically, joined by `+`.
    (
        "cmd+q+shift",
        "logout — quits the current macOS user session",
    ),
    (
        "cmd+ctrl+q",
        "lock screen — would lock the host out of its own session",
    ),
    ("cmd+esc+opt", "force-quit menu — disruptive system overlay"),
];

/// Prefix list for `focus_app` targets that must never be raised — these
/// are macOS system surfaces where activating them could log the user out
/// or grant the agent control over security settings.
const DENIED_FOCUS_TARGETS: &[&str] = &[
    "loginwindow",
    "Screen Lock",
    "ScreenSaver",
    "System Settings",
    "System Preferences",
    "Security & Privacy",
];

/// Returns `Some(reason)` if the action is denied. `None` means proceed.
fn check_denylist(args: &ComputerUseArgs) -> Option<String> {
    match args.action {
        ComputerUseAction::Key => {
            if let Some(combo) = normalised_hotkey(args) {
                for (denied, reason) in DENIED_HOTKEYS {
                    if combo == *denied {
                        return Some(format!(
                            "computer_use denied hotkey '{combo}' — {reason}. This is a hard denylist; refusing to dispatch."
                        ));
                    }
                }
            }
            None
        }
        ComputerUseAction::Type => {
            // Fork-bomb shape detection. The canonical bash fork bomb is
            // `:(){:|:&};:` — any near-cousin gets rejected too. We err on
            // the side of refusing rather than letting it through.
            let text = args.text.as_deref().unwrap_or("");
            if FORK_BOMB.is_match(text) {
                return Some(
                    "computer_use refused to type a fork-bomb-shaped payload. \
                     If this was a false positive, dispatch via a safer mechanism (run_shell with --no-exec, or paste from clipboard)."
                        .into(),
                );
            }
            None
        }
        ComputerUseAction::FocusApp => {
            let target = args.target.as_deref().unwrap_or("");
            for denied in DENIED_FOCUS_TARGETS {
                if target.eq_ignore_ascii_case(denied)
                    || target
                        .to_ascii_lowercase()
                        .contains(&denied.to_ascii_lowercase())
                {
                    return Some(format!(
                        "computer_use denied focus_app target '{target}': matches '{denied}', a macOS security surface that must not be driven."
                    ));
                }
            }
            None
        }
        _ => None,
    }
}

/// Normalise `args` into a canonical hotkey string for denylist matching.
/// Returns `None` when this isn't a hotkey (single key or absent).
fn normalised_hotkey(args: &ComputerUseArgs) -> Option<String> {
    let keys = args.keys.as_ref()?;
    if keys.len() < 2 {
        return None;
    }
    let mut lowered: Vec<String> = keys
        .iter()
        .map(|k| {
            let l = k.to_ascii_lowercase();
            // Common aliases people use interchangeably.
            match l.as_str() {
                "control" => "ctrl".into(),
                "command" => "cmd".into(),
                "option" => "opt".into(),
                "alt" => "opt".into(),
                "escape" => "esc".into(),
                _ => l,
            }
        })
        .collect();
    lowered.sort();
    Some(lowered.join("+"))
}

#[allow(clippy::expect_used)]
static FORK_BOMB: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
    // `:(){...}` or `bomb(){...}` — function definitions that recursively
    // pipe themselves into themselves are the shape we care about. The
    // pattern is intentionally a little loose so trivial obfuscations don't
    // walk past it.
    regex::Regex::new(r#"(?x)
        [A-Za-z_:][A-Za-z0-9_:]* \s* \(\) \s* \{ [^}]* \| [^}]* \&? [^}]* \} \s* ;? \s* [A-Za-z_:][A-Za-z0-9_:]* \s* ;?
    "#).expect("compile-time literal regex")
});

/// Append any text the driver returned (success context OR soft-failure
/// detail) to the action's synthesised summary. cua-driver is responsible
/// for emitting failure text on soft failures (`element not focusable`,
/// `target window vanished` — JSON-RPC success with a text-part); this
/// helper does not classify success vs. failure, it forwards whatever the
/// driver said so the model can react on the next turn. CLAUDE.md's
/// "if we report success it must succeed" invariant is upheld by the
/// driver-text contract, not by post-hoc inspection here.
///
/// The final merged text is capped at `MAX_TOOL_TEXT_BYTES` to keep a
/// misbehaving cua-driver build or oversized synthesized summary from
/// pumping megabytes into LLM context.
fn merge_driver_text(summary: String, resp: DriverResponse) -> ToolOutput {
    const MAX_DRIVER_TEXT_BYTES: usize = 4096;
    const MAX_TOOL_TEXT_BYTES: usize = 4096;
    let mut driver_text = String::new();
    for part in resp.content {
        let DriverContent::Text(t) = part else {
            continue;
        };
        if t.is_empty() {
            continue;
        }
        if !driver_text.is_empty() {
            driver_text.push('\n');
        }
        driver_text.push_str(&t);
    }
    if driver_text.len() > MAX_DRIVER_TEXT_BYTES {
        // Truncate on a char boundary so we don't slice mid-UTF-8.
        let mut cut = MAX_DRIVER_TEXT_BYTES;
        while !driver_text.is_char_boundary(cut) {
            cut -= 1;
        }
        let overflow = driver_text.len() - cut;
        driver_text.truncate(cut);
        driver_text.push_str(&format!(
            "\n… [truncated {overflow} bytes from driver output]"
        ));
    }
    let mut merged = if driver_text.is_empty() {
        summary
    } else {
        format!("{summary}\n{driver_text}")
    };
    if merged.len() > MAX_TOOL_TEXT_BYTES {
        let marker = "\n[truncated tool output]";
        let mut cut = MAX_TOOL_TEXT_BYTES.saturating_sub(marker.len());
        while !merged.is_char_boundary(cut) {
            cut -= 1;
        }
        merged.truncate(cut);
        merged.push_str(marker);
    }
    ToolOutput::Text(merged)
}

async fn click(args: &ComputerUseArgs, method: &str, driver: &dyn Driver) -> Result<ToolOutput> {
    let mut params = json!({});
    let summary;
    if let Some(idx) = args.element_index {
        // SOM-targeted click: address the element by index, the way the
        // model saw it in the last capture(mode=som). Pixel coords are
        // omitted entirely so the driver can't fall back to them.
        params["element_index"] = json!(idx);
        summary = format!("{method} on element [{idx}]");
    } else {
        let x = args.x.ok_or_else(|| {
            anyhow::anyhow!(
                "computer_use.{method} requires `x`/`y` (pixel coords) or `element_index` (from capture mode=som)"
            )
        })?;
        let y = args.y.ok_or_else(|| {
            anyhow::anyhow!("computer_use.{method} requires `x`/`y` or `element_index`")
        })?;
        params["x"] = json!(x);
        params["y"] = json!(y);
        summary = format!("{method} at ({x}, {y})");
    }
    if let Some(pid) = args.pid {
        params["pid"] = json!(pid);
    }
    let resp = driver.call(method, params).await?;
    Ok(merge_driver_text(summary, resp))
}

async fn drag(args: &ComputerUseArgs, driver: &dyn Driver) -> Result<ToolOutput> {
    let from_x = args
        .x
        .ok_or_else(|| anyhow::anyhow!("computer_use.drag requires `x`/`y`/`to_x`/`to_y`"))?;
    let from_y = args
        .y
        .ok_or_else(|| anyhow::anyhow!("computer_use.drag requires `x`/`y`/`to_x`/`to_y`"))?;
    let to_x = args
        .to_x
        .ok_or_else(|| anyhow::anyhow!("computer_use.drag requires `to_x`/`to_y`"))?;
    let to_y = args
        .to_y
        .ok_or_else(|| anyhow::anyhow!("computer_use.drag requires `to_x`/`to_y`"))?;
    let mut params = json!({"from_x": from_x, "from_y": from_y, "to_x": to_x, "to_y": to_y});
    if let Some(pid) = args.pid {
        params["pid"] = json!(pid);
    }
    let resp = driver.call("drag", params).await?;
    Ok(merge_driver_text(
        format!("drag ({from_x},{from_y}) → ({to_x},{to_y})"),
        resp,
    ))
}

async fn scroll(args: &ComputerUseArgs, driver: &dyn Driver) -> Result<ToolOutput> {
    let dx = args.delta_x.unwrap_or(0);
    let dy = args.delta_y.unwrap_or(0);
    if dx == 0 && dy == 0 {
        return Ok(ToolOutput::Text(
            "Error: computer_use.scroll requires non-zero `delta_x` or `delta_y`".into(),
        ));
    }
    let mut params = json!({"delta_x": dx, "delta_y": dy});
    let summary;
    if let Some(idx) = args.element_index {
        params["element_index"] = json!(idx);
        summary = format!("scroll element [{idx}] by ({dx},{dy})");
    } else {
        params["x"] = json!(args.x.unwrap_or(0));
        params["y"] = json!(args.y.unwrap_or(0));
        summary = format!("scroll ({dx},{dy})");
    }
    let resp = driver.call("scroll", params).await?;
    Ok(merge_driver_text(summary, resp))
}

async fn type_text(args: &ComputerUseArgs, driver: &dyn Driver) -> Result<ToolOutput> {
    let text = args
        .text
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("computer_use.type requires `text`"))?;
    let mut params = json!({"text": text});
    if let Some(pid) = args.pid {
        params["pid"] = json!(pid);
    }
    let resp = driver.call("type_text", params).await?;
    Ok(merge_driver_text(
        format!("typed {} chars", text.chars().count()),
        resp,
    ))
}

async fn key(args: &ComputerUseArgs, driver: &dyn Driver) -> Result<ToolOutput> {
    // Branch: single key vs hotkey. The `key`/`keys` discrimination matches
    // the cua-driver CLI's `press_key` vs `hotkey` split.
    if let Some(combo) = &args.keys {
        let mut params = json!({"keys": combo});
        if let Some(pid) = args.pid {
            params["pid"] = json!(pid);
        }
        let resp = driver.call("hotkey", params).await?;
        Ok(merge_driver_text(
            format!("hotkey {}", combo.join("+")),
            resp,
        ))
    } else if let Some(k) = &args.key {
        let mut params = json!({"key": k});
        if let Some(pid) = args.pid {
            params["pid"] = json!(pid);
        }
        let resp = driver.call("press_key", params).await?;
        Ok(merge_driver_text(format!("press_key {k}"), resp))
    } else {
        Ok(ToolOutput::Text(
            "Error: computer_use.key requires either `key` (single) or `keys` (hotkey array)"
                .into(),
        ))
    }
}

async fn set_value(args: &ComputerUseArgs, driver: &dyn Driver) -> Result<ToolOutput> {
    let element_index = args
        .element_index
        .ok_or_else(|| anyhow::anyhow!("computer_use.set_value requires `element_index`"))?;
    let value = args
        .value
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("computer_use.set_value requires `value`"))?;
    let pid = args
        .pid
        .ok_or_else(|| anyhow::anyhow!("computer_use.set_value requires `pid`"))?;
    let window_id = args.window_id.ok_or_else(|| {
        anyhow::anyhow!("computer_use.set_value requires `window_id` (AX cache key)")
    })?;
    let params = json!({
        "pid": pid,
        "window_id": window_id,
        "element_index": element_index,
        "value": value,
    });
    let resp = driver.call("set_value", params).await?;
    Ok(merge_driver_text(
        format!("set_value [{element_index}] = {value:?}"),
        resp,
    ))
}

async fn focus_app(args: &ComputerUseArgs, driver: &dyn Driver) -> Result<ToolOutput> {
    let target = args.target.as_deref().ok_or_else(|| {
        anyhow::anyhow!("computer_use.focus_app requires `target` (bundle id or process name)")
    })?;
    // `raise_window` defaults to true (visible, focus-stealing) per the
    // safer-default principle. Background co-work is opt-in.
    let raise = args.raise_window.unwrap_or(true);
    let mut params = json!({"target": target, "raise_window": raise});
    if let Some(window_id) = args.window_id {
        params["window_id"] = json!(window_id);
    }
    let resp = driver.call("focus_app", params).await?;
    let mode = if raise { "raised" } else { "backgrounded" };
    Ok(merge_driver_text(
        format!("focus_app {target} ({mode})"),
        resp,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// In-memory driver: pops queued responses in FIFO order. The recorded
    /// `(method, params)` pairs are inspected by the assertions below so the
    /// envelope-construction path is exercised end-to-end without spawning
    /// the binary.
    struct FakeDriver {
        responses: Mutex<std::collections::VecDeque<DriverResponse>>,
        calls: Mutex<Vec<(String, Value)>>,
    }

    impl FakeDriver {
        fn new(responses: Vec<DriverResponse>) -> Self {
            Self {
                responses: Mutex::new(responses.into()),
                calls: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl Driver for FakeDriver {
        async fn call(&self, method: &str, params: Value) -> Result<DriverResponse> {
            self.calls
                .lock()
                .unwrap()
                .push((method.to_string(), params));
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| anyhow::anyhow!("FakeDriver: no queued response for {method}"))
        }
    }

    fn one_by_one_png_b64() -> String {
        use base64::Engine;
        let png_bytes: &[u8] = &[
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48,
            0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00,
            0x00, 0x1F, 0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x44, 0x41, 0x54, 0x78,
            0x9C, 0x62, 0x00, 0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00,
            0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
        ];
        base64::engine::general_purpose::STANDARD.encode(png_bytes)
    }

    #[test]
    fn parse_response_extracts_image_and_text() {
        // The driver returns a result object with an array of {type, ...}
        // entries. Parsing must round-trip both kinds — silently dropping
        // either would manifest as an empty multimodal envelope downstream.
        let b64 = one_by_one_png_b64();
        let line = json!({
            "jsonrpc": "2.0",
            "id": 2,
            "result": {
                "content": [
                    {"type": "text", "text": "ok"},
                    {"type": "image", "mimeType": "image/png", "data": b64},
                ]
            }
        })
        .to_string();
        let resp = parse_response(&line).unwrap();
        assert_eq!(resp.content.len(), 2);
        assert!(matches!(resp.content[0], DriverContent::Text(ref t) if t == "ok"));
        match &resp.content[1] {
            DriverContent::Image { mime_type, base64 } => {
                assert_eq!(mime_type, "image/png");
                assert!(!base64.is_empty());
            }
            _ => panic!("expected image content"),
        }
    }

    #[test]
    fn parse_response_surfaces_driver_error() {
        // MCP error envelope — the handler must turn this into a Rust Err
        // rather than silently returning empty content (which would make the
        // model think the call succeeded).
        let line = json!({
            "jsonrpc": "2.0",
            "id": 2,
            "error": {"code": -32603, "message": "screen capture failed"}
        })
        .to_string();
        let err = parse_response(&line).unwrap_err().to_string();
        assert!(err.contains("screen capture failed"), "got: {err}");
    }

    /// Plan-mode allowlist invariant: capture / list_apps / wait are safe;
    /// every mutating action is not. Encoded as one exhaustive table so a
    /// new variant that forgets a row fails this test, not just "the
    /// implementation matches itself" rubber-stamping.
    #[test]
    fn plan_mode_allowlist_covers_readonly_only() {
        let cases: &[(ComputerUseAction, bool)] = &[
            (ComputerUseAction::Capture, true),
            (ComputerUseAction::ListApps, true),
            (ComputerUseAction::Wait, true),
            (ComputerUseAction::Click, false),
            (ComputerUseAction::DoubleClick, false),
            (ComputerUseAction::RightClick, false),
            (ComputerUseAction::MiddleClick, false),
            (ComputerUseAction::Drag, false),
            (ComputerUseAction::Scroll, false),
            (ComputerUseAction::Type, false),
            (ComputerUseAction::Key, false),
            (ComputerUseAction::SetValue, false),
            (ComputerUseAction::FocusApp, false),
        ];
        for (action, expected) in cases {
            assert_eq!(
                action_allowed_in_plan_mode(action),
                *expected,
                "plan-mode drift on {action:?}: expected {expected}"
            );
        }
    }

    #[tokio::test]
    async fn capture_returns_multimodal_envelope_with_image_part() {
        // The whole reason the tool exists: pixels reach the LLM on the
        // next turn. If this regresses, the model sees only the summary
        // string and the screenshot might as well not have happened.
        let b64 = one_by_one_png_b64();
        let driver = FakeDriver::new(vec![DriverResponse {
            content: vec![
                DriverContent::Text("captured 1x1".into()),
                DriverContent::Image {
                    mime_type: "image/png".into(),
                    base64: b64.clone(),
                },
            ],
        }]);
        let args = args_with_action(ComputerUseAction::Capture);
        let out = handle_with_driver(&args, &driver).await.unwrap();
        let (text, parts) = match out {
            ToolOutput::Multimodal { text, parts } => (text, parts),
            other => panic!("expected multimodal, got {other:?}"),
        };
        assert!(text.contains("captured 1x1"));
        assert_eq!(parts.len(), 2);
        match &parts[1] {
            ContentPart::ImageBase64 { media } => {
                assert_eq!(media.mime_type, "image/png");
                assert_eq!(media.data, b64);
            }
            other => panic!("expected ImageBase64, got {other:?}"),
        }
        assert_eq!(driver.calls.lock().unwrap()[0].0, "screenshot");
    }

    #[tokio::test]
    async fn capture_surfaces_pid_and_window_id() {
        // Window-targeted capture needs (pid, window_id) to reach the driver
        // intact — silently dropping them would always full-screen capture
        // and the agent's "look at this app" intent would be lost.
        let driver = FakeDriver::new(vec![DriverResponse {
            content: vec![DriverContent::Image {
                mime_type: "image/png".into(),
                base64: one_by_one_png_b64(),
            }],
        }]);
        let mut args = args_with_action(ComputerUseAction::Capture);
        args.pid = Some(1234);
        args.window_id = Some(42);
        let _ = handle_with_driver(&args, &driver).await.unwrap();
        let (_, params) = driver.calls.lock().unwrap()[0].clone();
        assert_eq!(params["pid"], 1234);
        assert_eq!(params["window_id"], 42);
    }

    #[tokio::test]
    async fn list_apps_concatenates_text_parts() {
        let driver = FakeDriver::new(vec![DriverResponse {
            content: vec![
                DriverContent::Text("Safari (pid=1)".into()),
                DriverContent::Text("Finder (pid=2)".into()),
            ],
        }]);
        let args = args_with_action(ComputerUseAction::ListApps);
        let out = handle_with_driver(&args, &driver).await.unwrap();
        let text = match out {
            ToolOutput::Text(t) => t,
            other => panic!("expected text, got {other:?}"),
        };
        assert!(text.contains("Safari"));
        assert!(text.contains("Finder"));
    }

    #[tokio::test]
    async fn wait_blocks_for_requested_duration() {
        // 50ms is small enough not to slow the test suite but big enough
        // to detect a "wait returns immediately" regression — `tokio::sleep`
        // has worst-case ~ms-scale wakeup jitter, not 50ms.
        let driver = FakeDriver::new(vec![]);
        let mut args = args_with_action(ComputerUseAction::Wait);
        args.duration_ms = Some(50);
        let start = std::time::Instant::now();
        handle_with_driver(&args, &driver).await.unwrap();
        let elapsed = start.elapsed();
        assert!(
            elapsed >= Duration::from_millis(50),
            "wait elapsed {elapsed:?}, expected ≥ 50ms"
        );
        // No driver call should have occurred — wait is host-side only.
        assert!(driver.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn wait_requires_duration_ms() {
        let driver = FakeDriver::new(vec![]);
        let args = args_with_action(ComputerUseAction::Wait);
        let err = handle_with_driver(&args, &driver).await.unwrap_err();
        assert!(err.to_string().contains("duration_ms"));
    }

    #[tokio::test]
    async fn disabled_config_skips_driver() {
        let mut config = Config::default();
        config.computer_use.enabled = false;
        // Bake in a non-existent driver path so any spawn attempt would fail
        // loudly — the assertion is that we *never* even try.
        config.computer_use.driver_path = "/definitely/does/not/exist/cua-driver".into();
        let args = args_with_action(ComputerUseAction::Capture);
        let out = handle(&args, &config, &CancellationToken::new())
            .await
            .unwrap();
        let text = match out {
            ToolOutput::Text(t) => t,
            other => panic!("expected text error, got {other:?}"),
        };
        assert!(text.contains("disabled"), "got: {text}");
    }

    #[test]
    fn deserializes_minimal_capture_args() {
        let args: ComputerUseArgs = serde_json::from_value(json!({"action": "capture"})).unwrap();
        assert_eq!(args.action, ComputerUseAction::Capture);
        assert!(args.pid.is_none());
    }

    #[test]
    fn rejects_unknown_action() {
        let result: std::result::Result<ComputerUseArgs, _> =
            serde_json::from_value(json!({"action": "launch_app"}));
        assert!(result.is_err(), "unknown action should fail schema parse");
    }

    fn args_with_action(action: ComputerUseAction) -> ComputerUseArgs {
        ComputerUseArgs {
            action,
            pid: None,
            window_id: None,
            duration_ms: None,
            x: None,
            y: None,
            to_x: None,
            to_y: None,
            delta_x: None,
            delta_y: None,
            text: None,
            key: None,
            keys: None,
            element_index: None,
            value: None,
            target: None,
            raise_window: None,
            mode: None,
            capture_after: None,
        }
    }

    fn ok_response() -> DriverResponse {
        DriverResponse {
            content: vec![DriverContent::Text("ok".into())],
        }
    }

    /// Cap guard: a multi-hour request must be clamped to the 5-minute budget;
    /// a sub-cap request passes through untouched. Pure arithmetic, so no sleep.
    #[test]
    fn effective_wait_clamps_runaway_durations_to_the_budget() {
        assert_eq!(effective_wait_ms(60 * 60 * 1000), 300_000); // 1h → 5min cap
        assert_eq!(effective_wait_ms(300_000), 300_000); // exactly the cap
        assert_eq!(effective_wait_ms(5), 5); // sub-cap unchanged
        assert_eq!(effective_wait_ms(0), 0);
    }

    /// Sleep branch wins when an interrupt token is present but never fires:
    /// the wait elapses fully and reports its (clamped) duration.
    #[tokio::test]
    async fn wait_completes_normally_with_uncancelled_token() {
        let mut args = args_with_action(ComputerUseAction::Wait);
        args.duration_ms = Some(5);
        let token = CancellationToken::new();
        let out = wait(&args, Some(&token)).await.unwrap();
        match out {
            ToolOutput::Text(t) => assert_eq!(t, "waited 5ms"),
            other => panic!("expected text, got {other:?}"),
        }
    }

    /// Interrupt branch: a long wait must abort promptly once the turn is
    /// interrupted instead of blocking for the full (capped) duration.
    #[tokio::test]
    async fn wait_returns_early_when_token_cancelled_mid_wait() {
        let mut args = args_with_action(ComputerUseAction::Wait);
        args.duration_ms = Some(5 * 60 * 1000); // would block ~5 min
        let token = CancellationToken::new();
        let canceller = token.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            canceller.cancel();
        });
        let start = std::time::Instant::now();
        let out = wait(&args, Some(&token)).await.unwrap();
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "interrupted wait should return promptly, took {:?}",
            start.elapsed()
        );
        match out {
            ToolOutput::Text(t) => assert_eq!(t, "wait interrupted by new input"),
            other => panic!("expected interrupt text, got {other:?}"),
        }
    }

    /// Race coverage: an interrupt that landed before the tool boundary leaves
    /// the token already cancelled. The signal must not be lost — the wait
    /// returns the interrupt result without sleeping.
    #[tokio::test]
    async fn wait_with_already_cancelled_token_returns_immediately() {
        let mut args = args_with_action(ComputerUseAction::Wait);
        args.duration_ms = Some(5 * 60 * 1000);
        let token = CancellationToken::new();
        token.cancel();
        let start = std::time::Instant::now();
        let out = wait(&args, Some(&token)).await.unwrap();
        assert!(start.elapsed() < Duration::from_secs(1));
        match out {
            ToolOutput::Text(t) => assert_eq!(t, "wait interrupted by new input"),
            other => panic!("expected interrupt text, got {other:?}"),
        }
    }

    /// Denylist invariant: every hotkey shaped like a logout / lock /
    /// force-quit must be refused BEFORE the driver is reached. Encoded as
    /// a table so adding a new combo to the denylist requires a matching
    /// test row.
    #[tokio::test]
    async fn dispatch_rejects_unsafe_keystrokes() {
        let combos: &[Vec<&str>] = &[
            // user-typed orderings differ; the normaliser must collapse them.
            vec!["cmd", "shift", "q"],
            vec!["shift", "cmd", "q"],
            vec!["cmd", "ctrl", "q"],
            vec!["cmd", "option", "esc"],
            vec!["cmd", "alt", "esc"],
        ];
        for combo in combos {
            let driver = FakeDriver::new(vec![ok_response()]);
            let mut args = args_with_action(ComputerUseAction::Key);
            args.keys = Some(combo.iter().map(|s| s.to_string()).collect());
            let out = handle_with_driver(&args, &driver).await.unwrap();
            let text = match out {
                ToolOutput::Text(t) => t,
                other => panic!("expected text error, got {other:?}"),
            };
            assert!(
                text.contains("denied hotkey"),
                "combo {combo:?} should have been denied, got: {text}"
            );
            // The driver must NOT have been called.
            assert!(
                driver.calls.lock().unwrap().is_empty(),
                "combo {combo:?} reached driver"
            );
        }
    }

    #[tokio::test]
    async fn dispatch_rejects_logout_focus_target() {
        // `loginwindow` is the canonical "this will log the user out" target.
        let denied_targets = ["loginwindow", "Screen Lock", "System Settings"];
        for target in denied_targets {
            let driver = FakeDriver::new(vec![ok_response()]);
            let mut args = args_with_action(ComputerUseAction::FocusApp);
            args.target = Some(target.into());
            let out = handle_with_driver(&args, &driver).await.unwrap();
            let text = match out {
                ToolOutput::Text(t) => t,
                other => panic!("expected text error, got {other:?}"),
            };
            assert!(
                text.contains("denied focus_app"),
                "target {target} should have been denied, got: {text}"
            );
            assert!(driver.calls.lock().unwrap().is_empty());
        }
    }

    #[tokio::test]
    async fn dispatch_rejects_fork_bomb_type_payload() {
        // The classic shape — function definition that pipes itself.
        let payload = ":(){ :|:& };:";
        let driver = FakeDriver::new(vec![ok_response()]);
        let mut args = args_with_action(ComputerUseAction::Type);
        args.text = Some(payload.into());
        let out = handle_with_driver(&args, &driver).await.unwrap();
        let text = match out {
            ToolOutput::Text(t) => t,
            other => panic!("expected text error, got {other:?}"),
        };
        assert!(text.contains("fork-bomb"), "got: {text}");
        assert!(driver.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn click_invokes_driver_with_coords() {
        let driver = FakeDriver::new(vec![ok_response()]);
        let mut args = args_with_action(ComputerUseAction::Click);
        args.x = Some(42);
        args.y = Some(13);
        let _ = handle_with_driver(&args, &driver).await.unwrap();
        let (method, params) = driver.calls.lock().unwrap()[0].clone();
        assert_eq!(method, "click");
        assert_eq!(params["x"], 42);
        assert_eq!(params["y"], 13);
    }

    #[tokio::test]
    async fn type_invokes_driver_with_text() {
        let driver = FakeDriver::new(vec![ok_response()]);
        let mut args = args_with_action(ComputerUseAction::Type);
        args.text = Some("Hello world".into());
        let _ = handle_with_driver(&args, &driver).await.unwrap();
        let (method, params) = driver.calls.lock().unwrap()[0].clone();
        assert_eq!(method, "type_text");
        assert_eq!(params["text"], "Hello world");
    }

    #[tokio::test]
    async fn focus_app_no_raise_passes_flag_through() {
        // The whole point of the `raise_window: false` knob is that the
        // user's frontmost app stays frontmost. If the flag silently
        // becomes `true` somewhere along the way the no-foreground contract
        // breaks — this guards exactly that.
        let driver = FakeDriver::new(vec![ok_response()]);
        let mut args = args_with_action(ComputerUseAction::FocusApp);
        args.target = Some("com.google.Chrome".into());
        args.raise_window = Some(false);
        let _ = handle_with_driver(&args, &driver).await.unwrap();
        let (method, params) = driver.calls.lock().unwrap()[0].clone();
        assert_eq!(method, "focus_app");
        assert_eq!(params["target"], "com.google.Chrome");
        assert_eq!(params["raise_window"], false);
    }

    #[tokio::test]
    async fn focus_app_defaults_to_raise_true() {
        // Safer default — anything that defaults to "stealth-mode UI driving"
        // is too easy to misuse. Operators have to ask for backgrounded.
        let driver = FakeDriver::new(vec![ok_response()]);
        let mut args = args_with_action(ComputerUseAction::FocusApp);
        args.target = Some("com.apple.Finder".into());
        let _ = handle_with_driver(&args, &driver).await.unwrap();
        let (_, params) = driver.calls.lock().unwrap()[0].clone();
        assert_eq!(params["raise_window"], true);
    }

    #[tokio::test]
    async fn click_surfaces_driver_soft_failure_text() {
        // Regression guard: cua-driver returns soft failures (element-not-
        // focusable, target-window-vanished) as JSON-RPC success with a
        // text-part. Before the fix the tool said "click at (x, y)" and the
        // model believed it; the user only saw the failure when the next
        // capture showed nothing changed. The merge surfaces the driver's
        // message so the agent can recover on the next turn.
        let driver = FakeDriver::new(vec![DriverResponse {
            content: vec![DriverContent::Text(
                "error: target element not focusable".into(),
            )],
        }]);
        let mut args = args_with_action(ComputerUseAction::Click);
        args.x = Some(100);
        args.y = Some(200);
        let out = handle_with_driver(&args, &driver).await.unwrap();
        let text = match out {
            ToolOutput::Text(t) => t,
            other => panic!("expected text, got {other:?}"),
        };
        assert!(
            text.contains("click at (100, 200)"),
            "summary missing: {text}"
        );
        assert!(
            text.contains("error: target element not focusable"),
            "driver soft-failure text missing: {text}"
        );
    }

    #[tokio::test]
    async fn focus_app_surfaces_driver_soft_failure_text() {
        // Covers the focus_app branch — the no-foreground contract makes this
        // a particularly easy soft-failure surface to ignore (the driver
        // can't raise a sandboxed app and shrugs back a text-part).
        let driver = FakeDriver::new(vec![DriverResponse {
            content: vec![DriverContent::Text(
                "error: target app sandbox prevents activation".into(),
            )],
        }]);
        let mut args = args_with_action(ComputerUseAction::FocusApp);
        args.target = Some("com.google.Chrome".into());
        args.raise_window = Some(false);
        let out = handle_with_driver(&args, &driver).await.unwrap();
        let text = match out {
            ToolOutput::Text(t) => t,
            other => panic!("expected text, got {other:?}"),
        };
        assert!(text.contains("focus_app com.google.Chrome"));
        assert!(text.contains("error: target app sandbox prevents activation"));
    }

    #[tokio::test]
    async fn click_truncates_oversized_driver_text() {
        // A misbehaving cua-driver build could return arbitrarily large text;
        // every action's output flows into LLM context, so the merge must cap
        // it. Guard the 4KB cap with a clear truncation marker — without this
        // the merge would silently inflate context budget on every turn.
        let huge = "x".repeat(10_000);
        let driver = FakeDriver::new(vec![DriverResponse {
            content: vec![DriverContent::Text(huge)],
        }]);
        let mut args = args_with_action(ComputerUseAction::Click);
        args.x = Some(7);
        args.y = Some(8);
        let out = handle_with_driver(&args, &driver).await.unwrap();
        let text = match out {
            ToolOutput::Text(t) => t,
            other => panic!("expected text, got {other:?}"),
        };
        // Summary preserved; driver text capped; truncation marker present.
        assert!(text.starts_with("click at (7, 8)\n"));
        assert!(
            text.contains("truncated"),
            "missing truncation marker: {text}"
        );
        assert!(
            text.len() <= 4096,
            "output not capped (len={}): {text}",
            text.len()
        );
    }

    #[tokio::test]
    async fn click_empty_driver_text_keeps_summary_only() {
        // No trailing newline when the driver returns no text — matches the
        // pre-fix behaviour for the happy path so existing log output / model
        // expectations don't shift.
        let driver = FakeDriver::new(vec![DriverResponse { content: vec![] }]);
        let mut args = args_with_action(ComputerUseAction::Click);
        args.x = Some(1);
        args.y = Some(2);
        let out = handle_with_driver(&args, &driver).await.unwrap();
        let text = match out {
            ToolOutput::Text(t) => t,
            other => panic!("expected text, got {other:?}"),
        };
        assert_eq!(text, "click at (1, 2)");
    }

    /// AX mode is text-only by contract — pixels would defeat the whole
    /// "keep bytes out of LLM context" purpose. The driver method must
    /// route to `accessibility` and the response must NOT include an
    /// ImageBase64 part even if the driver accidentally returned one.
    #[tokio::test]
    async fn capture_ax_mode_is_text_only_and_calls_accessibility() {
        let driver = FakeDriver::new(vec![DriverResponse {
            content: vec![
                DriverContent::Text("AXWindow > AXButton \"Submit\"".into()),
                // Even if a driver build returns an image, AX mode must
                // strip it — the regression we're guarding against is the
                // tree silently getting padded with image bytes.
                DriverContent::Image {
                    mime_type: "image/png".into(),
                    base64: one_by_one_png_b64(),
                },
            ],
        }]);
        let mut args = args_with_action(ComputerUseAction::Capture);
        args.mode = Some(CaptureMode::Ax);
        let out = handle_with_driver(&args, &driver).await.unwrap();
        match out {
            ToolOutput::Text(t) => {
                assert!(t.contains("AXButton"));
                assert!(!t.contains("base64"), "image leaked into AX text: {t}");
            }
            other => panic!("expected text-only output for AX mode, got {other:?}"),
        }
        assert_eq!(driver.calls.lock().unwrap()[0].0, "accessibility");
        assert_eq!(driver.calls.lock().unwrap()[0].1["mode"], "ax");
    }

    /// SOM mode keeps the image AND surfaces the overlay text from the
    /// driver. The mode string is what the driver branches on for the
    /// numbered-overlay render.
    #[tokio::test]
    async fn capture_som_mode_returns_image_plus_overlay_text() {
        let driver = FakeDriver::new(vec![DriverResponse {
            content: vec![
                DriverContent::Text("[{\"index\":0,\"role\":\"button\"}]".into()),
                DriverContent::Image {
                    mime_type: "image/png".into(),
                    base64: one_by_one_png_b64(),
                },
            ],
        }]);
        let mut args = args_with_action(ComputerUseAction::Capture);
        args.mode = Some(CaptureMode::Som);
        let out = handle_with_driver(&args, &driver).await.unwrap();
        let (text, parts) = match out {
            ToolOutput::Multimodal { text, parts } => (text, parts),
            other => panic!("SOM mode must return Multimodal, got {other:?}"),
        };
        assert!(
            text.contains("\"role\":\"button\""),
            "overlay missing: {text}"
        );
        assert!(parts
            .iter()
            .any(|p| matches!(p, ContentPart::ImageBase64 { .. })));
        assert_eq!(driver.calls.lock().unwrap()[0].0, "screenshot");
        assert_eq!(driver.calls.lock().unwrap()[0].1["mode"], "som");
    }

    /// Default mode keeps backward compatibility — no `mode` set must behave
    /// exactly like the pre-parity tool: `screenshot`, `vision`, image inline.
    #[tokio::test]
    async fn capture_default_mode_is_vision() {
        let driver = FakeDriver::new(vec![DriverResponse {
            content: vec![DriverContent::Image {
                mime_type: "image/png".into(),
                base64: one_by_one_png_b64(),
            }],
        }]);
        let args = args_with_action(ComputerUseAction::Capture);
        let _ = handle_with_driver(&args, &driver).await.unwrap();
        let (method, params) = driver.calls.lock().unwrap()[0].clone();
        assert_eq!(method, "screenshot");
        assert_eq!(params["mode"], "vision");
    }

    /// Vision/SOM promised pixels — if the driver doesn't return any,
    /// the No-Silent-Swallowing invariant says we surface a clear error
    /// rather than emit an empty multimodal envelope the model would treat
    /// as a success.
    #[tokio::test]
    async fn capture_vision_no_image_surfaces_error() {
        let driver = FakeDriver::new(vec![DriverResponse {
            content: vec![DriverContent::Text("screenshot denied".into())],
        }]);
        let mut args = args_with_action(ComputerUseAction::Capture);
        args.mode = Some(CaptureMode::Vision);
        let out = handle_with_driver(&args, &driver).await.unwrap();
        match out {
            ToolOutput::Text(t) => {
                assert!(t.starts_with("Error:"), "expected error prefix: {t}");
                assert!(t.contains("no image"));
                assert!(t.contains("screenshot denied"));
            }
            other => panic!("expected text error, got {other:?}"),
        }
    }

    // ── element_index targeting ────────────────────────────────────

    /// SOM follow-up: model captures with mode=som, sees `[3] button`, then
    /// clicks via `element_index: 3`. Coordinates must NOT appear in the
    /// driver call — otherwise the driver could pick whichever is set,
    /// silently turning a SOM click into a stale pixel click.
    #[tokio::test]
    async fn click_by_element_index_omits_coordinates() {
        let driver = FakeDriver::new(vec![ok_response()]);
        let mut args = args_with_action(ComputerUseAction::Click);
        args.element_index = Some(7);
        let _ = handle_with_driver(&args, &driver).await.unwrap();
        let (method, params) = driver.calls.lock().unwrap()[0].clone();
        assert_eq!(method, "click");
        assert_eq!(params["element_index"], 7);
        assert!(
            params.get("x").is_none(),
            "stray x in element_index click: {params}"
        );
        assert!(
            params.get("y").is_none(),
            "stray y in element_index click: {params}"
        );
    }

    /// Ambiguous target: model sets both pixel coords and element_index.
    /// Reject pre-driver — either could be "right" and silently picking
    /// one means the failure mode is undebuggable.
    #[tokio::test]
    async fn click_rejects_coords_and_element_index_together() {
        let driver = FakeDriver::new(vec![ok_response()]);
        let mut args = args_with_action(ComputerUseAction::Click);
        args.x = Some(10);
        args.y = Some(20);
        args.element_index = Some(3);
        let out = handle_with_driver(&args, &driver).await.unwrap();
        match out {
            ToolOutput::Text(t) => assert!(
                t.contains("not both"),
                "expected 'not both' rejection, got: {t}"
            ),
            other => panic!("expected text error, got {other:?}"),
        }
        assert!(
            driver.calls.lock().unwrap().is_empty(),
            "ambiguous click reached driver"
        );
    }

    /// Scroll by element_index is the SOM equivalent of "scroll inside the
    /// list with overlay box [N]". Driver must see the index and the
    /// deltas; pixel coords are omitted entirely.
    #[tokio::test]
    async fn scroll_by_element_index_passes_index_and_deltas() {
        let driver = FakeDriver::new(vec![ok_response()]);
        let mut args = args_with_action(ComputerUseAction::Scroll);
        args.element_index = Some(2);
        args.delta_y = Some(-50);
        let _ = handle_with_driver(&args, &driver).await.unwrap();
        let (_, params) = driver.calls.lock().unwrap()[0].clone();
        assert_eq!(params["element_index"], 2);
        assert_eq!(params["delta_y"], -50);
        assert!(params.get("x").is_none());
        assert!(params.get("y").is_none());
    }

    // ── capture_after ──────────────────────────────────────────────

    /// `capture_after: true` on a successful click must produce two driver
    /// calls (the click, then a screenshot) and merge their outputs into a
    /// Multimodal envelope so the model sees pixels for verification.
    #[tokio::test]
    async fn click_with_capture_after_appends_screenshot() {
        let driver = FakeDriver::new(vec![
            ok_response(),
            DriverResponse {
                content: vec![DriverContent::Image {
                    mime_type: "image/png".into(),
                    base64: one_by_one_png_b64(),
                }],
            },
        ]);
        let mut args = args_with_action(ComputerUseAction::Click);
        args.x = Some(1);
        args.y = Some(2);
        args.capture_after = Some(true);
        let out = handle_with_driver(&args, &driver).await.unwrap();
        let (text, parts) = match out {
            ToolOutput::Multimodal { text, parts } => (text, parts),
            other => panic!("expected Multimodal with capture, got {other:?}"),
        };
        assert!(text.contains("click at (1, 2)"));
        assert!(text.contains("capture_after"));
        assert!(parts
            .iter()
            .any(|p| matches!(p, ContentPart::ImageBase64 { .. })));
        let calls = driver.calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].0, "click");
        assert_eq!(calls[1].0, "screenshot");
    }

    /// Denylist short-circuits the action entirely — capture_after must
    /// NOT fire either, otherwise denying a hotkey would still poke the
    /// driver via the follow-up screenshot.
    #[tokio::test]
    async fn capture_after_does_not_fire_when_denylist_rejects() {
        let driver = FakeDriver::new(vec![]);
        let mut args = args_with_action(ComputerUseAction::Key);
        args.keys = Some(vec!["cmd".into(), "shift".into(), "q".into()]);
        args.capture_after = Some(true);
        let _ = handle_with_driver(&args, &driver).await.unwrap();
        assert!(
            driver.calls.lock().unwrap().is_empty(),
            "denied hotkey still reached driver"
        );
    }

    /// `capture_after` is structurally meaningless for `capture` itself —
    /// refuse rather than silently take a second redundant screenshot.
    #[tokio::test]
    async fn capture_with_capture_after_is_rejected_pre_driver() {
        let driver = FakeDriver::new(vec![]);
        let mut args = args_with_action(ComputerUseAction::Capture);
        args.capture_after = Some(true);
        let out = handle_with_driver(&args, &driver).await.unwrap();
        match out {
            ToolOutput::Text(t) => assert!(t.contains("capture_after is not valid")),
            other => panic!("expected text error, got {other:?}"),
        }
        assert!(driver.calls.lock().unwrap().is_empty());
    }

    /// capture_after fail-safe: when the follow-up capture itself errors
    /// out, the primary action's success must NOT regress to a failure.
    /// The error is appended to the text so the model knows verification
    /// didn't happen, but the action stands.
    #[tokio::test]
    async fn capture_after_failure_does_not_fail_primary_action() {
        let driver = FakeDriver::new(vec![
            ok_response(),
            // No second response queued — capture_internal will error.
        ]);
        let mut args = args_with_action(ComputerUseAction::Click);
        args.x = Some(5);
        args.y = Some(6);
        args.capture_after = Some(true);
        let out = handle_with_driver(&args, &driver).await.unwrap();
        match out {
            ToolOutput::Text(t) => {
                assert!(t.contains("click at (5, 6)"));
                assert!(t.contains("capture_after failed"));
            }
            ToolOutput::Multimodal { text, .. } => {
                assert!(text.contains("capture_after failed"));
            }
        }
    }

    // ── Plan-mode allowlist still covers everything ─────────────────

    /// Adding `mode` and `capture_after` must not have accidentally
    /// changed the plan-mode allowlist. This re-asserts the table on
    /// every new variant pairing — including the new modes.
    #[test]
    fn plan_mode_table_covers_all_capture_modes() {
        for mode in [CaptureMode::Vision, CaptureMode::Som, CaptureMode::Ax] {
            // Capture is read-only regardless of mode.
            let mut args = args_with_action(ComputerUseAction::Capture);
            args.mode = Some(mode);
            assert!(
                action_allowed_in_plan_mode(&args.action),
                "capture must stay plan-mode-safe (mode={mode:?})"
            );
        }
    }

    #[test]
    fn hotkey_normaliser_collapses_aliases_and_order() {
        // "command" and "cmd" are the same key; ordering shouldn't matter.
        let mut a = args_with_action(ComputerUseAction::Key);
        a.keys = Some(vec!["shift".into(), "command".into(), "Q".into()]);
        assert_eq!(normalised_hotkey(&a).unwrap(), "cmd+q+shift");

        let mut b = args_with_action(ComputerUseAction::Key);
        b.keys = Some(vec!["cmd".into(), "shift".into(), "q".into()]);
        assert_eq!(normalised_hotkey(&b).unwrap(), "cmd+q+shift");
    }
}
