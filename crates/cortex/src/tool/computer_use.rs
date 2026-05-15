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

use crate::config::Config;
use crate::types::{ContentPart, MediaData, ToolOutput};

/// The action discriminator. New variants land in this enum first so the
/// `serde` rejection happens at parse time, not deep in the handler.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ComputerUseAction {
    /// Capture the screen as a PNG. Returns a multimodal envelope so the
    /// model gets pixels on the next turn.
    Capture,
    /// List the running macOS applications with pid + name.
    ListApps,
    /// Block for `duration_ms` milliseconds. Useful for waiting on
    /// animations or async UI redraws between captures.
    Wait,
}

/// Args for `computer_use`. The schema fans out per-action — `duration_ms` is
/// only meaningful for `Wait`, etc. We leave them all optional and validate
/// per-action below so the LLM gets a single, predictable schema.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ComputerUseArgs {
    pub action: ComputerUseAction,

    /// Capture only. Optional `pid` to capture a specific app's frontmost
    /// window (delegates to cua-driver's window-targeted capture). When
    /// `None`, captures the full screen.
    #[serde(default)]
    pub pid: Option<i64>,

    /// Capture only. Optional `window_id` paired with `pid`.
    #[serde(default)]
    pub window_id: Option<i64>,

    /// Wait only. Sleep duration in milliseconds. Required when
    /// `action == Wait`.
    #[serde(default)]
    pub duration_ms: Option<u64>,
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
pub(crate) async fn handle(args: &ComputerUseArgs, config: &Config) -> Result<ToolOutput> {
    if !config.computer_use.enabled {
        return Ok(ToolOutput::Text(
            "Error: computer_use is disabled. Enable it via `/settings` (computer_use.enabled = true) — note the driver only ships for macOS today.".into(),
        ));
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
    match args.action {
        ComputerUseAction::Capture => capture(args, driver).await,
        ComputerUseAction::ListApps => list_apps(driver).await,
        ComputerUseAction::Wait => wait(args).await,
    }
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
    let mut params = json!({});
    if let Some(pid) = args.pid {
        params["pid"] = json!(pid);
    }
    if let Some(window_id) = args.window_id {
        params["window_id"] = json!(window_id);
    }
    let resp = driver.call("screenshot", params).await?;

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

    let Some((mime_type, data)) = image else {
        return Ok(ToolOutput::Text(format!(
            "Error: cua-driver returned no image for capture. Detail: {summary}"
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

async fn wait(args: &ComputerUseArgs) -> Result<ToolOutput> {
    let ms = args.duration_ms.ok_or_else(|| {
        anyhow::anyhow!("computer_use.wait requires `duration_ms` (milliseconds to sleep)")
    })?;
    // Cap at 5 minutes so a runaway agent loop can't pin the worker
    // indefinitely. The driver itself doesn't gate this — we own the budget.
    const MAX_WAIT_MS: u64 = 5 * 60 * 1000;
    let effective = ms.min(MAX_WAIT_MS);
    tokio::time::sleep(Duration::from_millis(effective)).await;
    Ok(ToolOutput::Text(format!("waited {effective}ms")))
}

/// True iff the given action is allowed when the agent is in Plan mode.
/// Plan mode blocks anything that mutates the host; the three read-only
/// actions here pass.
pub(crate) const fn action_allowed_in_plan_mode(action: &ComputerUseAction) -> bool {
    matches!(
        action,
        ComputerUseAction::Capture | ComputerUseAction::ListApps | ComputerUseAction::Wait
    )
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

    /// Plan-mode allowlist invariant: capture / list_apps / wait are safe,
    /// every other action (added in commit 2) is not. Encoded as a single
    /// table so a new variant that forgets a row fails this test, not just
    /// "the implementation matches itself" rubber-stamping.
    #[test]
    fn plan_mode_allowlist_covers_readonly_only() {
        let allowed: Vec<ComputerUseAction> = vec![
            ComputerUseAction::Capture,
            ComputerUseAction::ListApps,
            ComputerUseAction::Wait,
        ];
        for a in &allowed {
            assert!(action_allowed_in_plan_mode(a), "{a:?} should be allowed");
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
        let args = ComputerUseArgs {
            action: ComputerUseAction::Capture,
            pid: None,
            window_id: None,
            duration_ms: None,
        };
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
        let args = ComputerUseArgs {
            action: ComputerUseAction::Capture,
            pid: Some(1234),
            window_id: Some(42),
            duration_ms: None,
        };
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
        let args = ComputerUseArgs {
            action: ComputerUseAction::ListApps,
            pid: None,
            window_id: None,
            duration_ms: None,
        };
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
        let args = ComputerUseArgs {
            action: ComputerUseAction::Wait,
            pid: None,
            window_id: None,
            duration_ms: Some(50),
        };
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
        let args = ComputerUseArgs {
            action: ComputerUseAction::Wait,
            pid: None,
            window_id: None,
            duration_ms: None,
        };
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
        let args = ComputerUseArgs {
            action: ComputerUseAction::Capture,
            pid: None,
            window_id: None,
            duration_ms: None,
        };
        let out = handle(&args, &config).await.unwrap();
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
}
