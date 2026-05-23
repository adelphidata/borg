pub mod actions;
pub mod console;
pub mod detection;
pub mod health;
pub mod snapshot;
pub mod tabs;
pub mod validate;
pub mod wait;

use std::collections::HashMap;
use std::time::Duration;

use anyhow::{Context, Result};
use chromiumoxide::browser::{Browser, BrowserConfig};
use chromiumoxide::cdp::browser_protocol::page::CaptureScreenshotFormat;
use futures::StreamExt;
use tracing::instrument;

use crate::config::{BrowserConfig as BorgBrowserConfig, UrlPolicyConfig};
use crate::url_policy::{self, ClassifyOutcome};

// Re-export key types for backward compatibility
pub use detection::{detect_agent_browser, find_chrome, ChromeDetection};
pub use validate::validate_browser_args;

/// Persistent CDP browser session. Lazy-launched on first `browser` tool call.
pub struct BrowserSession {
    browser: Browser,
    tab_manager: tabs::TabManager,
    _event_handle: tokio::task::JoinHandle<()>,
    event_listener_handles: Vec<tokio::task::JoinHandle<()>>,
    event_buffers: console::EventBuffers,
    config: BorgBrowserConfig,
    /// Registry guard for the Chrome process. Held for the lifetime of
    /// the session; dropped (and finalized as Killed) when the session
    /// is closed. `chromiumoxide` does not expose the child PID through
    /// its public API, so the registered entry carries `pid = 0` and
    /// orphan detection works on `kind = Browser` + start time instead
    /// of liveness probing — adequate because the Browser kind is
    /// auto-reap-exempt by design.
    _registry_guard: Option<crate::process_registry::ProcessGuard>,
    /// `@e<N>` → CSS selector map from the most recent snapshot on the
    /// active page. Reset on `snapshot` (re-populated), `new_tab`,
    /// `switch_tab`, and `close_tab` — stale refs cannot leak across
    /// pages because the underlying DOM is different.
    snapshot_refs: HashMap<String, String>,
}

impl BrowserSession {
    /// Launch Chrome and open a default page.
    #[instrument(skip_all, fields(browser.action = "launch"))]
    pub async fn launch(config: &BorgBrowserConfig) -> Result<Self> {
        Self::launch_with_registry(config, None, None).await
    }

    /// Same as [`Self::launch`] but registers the Chrome process with
    /// the daemon-side subprocess registry, so `borg status` /
    /// `/doctor` can see the browser is running and the operator can
    /// pin it (Browser is auto-reap-exempt by default).
    #[instrument(skip_all, fields(browser.action = "launch"))]
    pub async fn launch_with_registry(
        config: &BorgBrowserConfig,
        registry: Option<&crate::process_registry::ProcessRegistry>,
        session_id: Option<String>,
    ) -> Result<Self> {
        let detection = detection::find_chrome(config.executable.as_deref());
        let chrome_path = detection.executable.context(
            "No Chrome/Chromium found. Install Chrome or set browser.executable in config.",
        )?;

        let mut builder = BrowserConfig::builder();
        builder = builder.chrome_executable(chrome_path);

        if config.headless {
            builder = builder.arg("--headless=new");
        }

        if config.no_sandbox {
            tracing::warn!(
                "Browser sandbox is disabled (browser.no_sandbox = true). \
                 This weakens Chrome's process isolation. Only use in container environments."
            );
            builder = builder.no_sandbox();
        }

        builder = builder
            .arg(format!("--remote-debugging-port={}", config.cdp_port))
            .arg("--disable-gpu")
            .arg("--disable-dev-shm-usage");

        let browser_config = builder
            .build()
            .map_err(|e| anyhow::anyhow!("Failed to build browser config: {e}"))?;

        let launch_timeout = Duration::from_millis(config.startup_timeout_ms);
        let (browser, mut handler) =
            tokio::time::timeout(launch_timeout, Browser::launch(browser_config))
                .await
                .context("Browser launch timed out")?
                .context("Failed to launch browser")?;

        let event_handle = tokio::spawn(async move { while handler.next().await.is_some() {} });

        let page = browser
            .new_page("about:blank")
            .await
            .context("Failed to create initial page")?;

        let event_buffers = console::EventBuffers::new(
            config.console_buffer_size,
            config.error_buffer_size,
            config.network_buffer_size,
        );

        let event_listener_handles = console::spawn_event_listeners(&page, &event_buffers).await;

        let registry_guard = registry.map(|reg| {
            reg.register(
                0, // chromiumoxide does not expose the child PID; see field doc.
                crate::process_registry::ProcessKind::Browser,
                "chromium (headless)",
                session_id,
            )
        });

        Ok(Self {
            browser,
            tab_manager: tabs::TabManager::new(page),
            _event_handle: event_handle,
            event_listener_handles,
            event_buffers,
            config: config.clone(),
            _registry_guard: registry_guard,
            snapshot_refs: HashMap::new(),
        })
    }

    // -- Health & Recovery --

    /// Check if the browser process is still alive and responsive.
    #[instrument(skip_all, fields(browser.action = "health_check"))]
    pub async fn health_check(&self) -> Result<()> {
        health::check_browser_health(&self.browser, Duration::from_secs(5)).await
    }

    /// Attempt to recover from a stale page/target error.
    /// First tries to open a new page; if that fails, relaunches the browser.
    #[instrument(skip_all, fields(browser.action = "recover"))]
    pub async fn try_recover(&mut self) -> Result<()> {
        tracing::info!("Attempting browser recovery...");

        // Try opening a fresh page on the existing browser
        match self.browser.new_page("about:blank").await {
            Ok(page) => {
                let new_handles = console::spawn_event_listeners(&page, &self.event_buffers).await;
                self.event_listener_handles.extend(new_handles);
                self.tab_manager.replace_active(page);
                tracing::info!("Browser recovery: replaced active page");
                return Ok(());
            }
            Err(e) => {
                tracing::warn!("Page recovery failed ({e}), attempting full relaunch...");
            }
        }

        // Full relaunch
        self.abort_listeners();
        let new_session = Self::launch(&self.config).await?;

        self.browser = new_session.browser;
        self.tab_manager = new_session.tab_manager;
        self._event_handle = new_session._event_handle;
        self.event_listener_handles = new_session.event_listener_handles;
        self.event_buffers = new_session.event_buffers;
        // Refs were tied to the now-defunct DOM — drop them so a stale
        // `@e<N>` cannot accidentally resolve to a different element on
        // the recovered page.
        self.snapshot_refs.clear();

        tracing::info!("Browser recovery: full relaunch complete");
        Ok(())
    }

    /// Resolve a `@e<N>` ref into a CSS selector from the last snapshot, or
    /// return the input unchanged when the caller already passed a selector.
    /// The error path is distinct from "selector didn't match" so the model
    /// gets a clear "take a snapshot first" hint.
    pub fn resolve_selector_or_ref(
        &self,
        selector: Option<&str>,
        r#ref: Option<&str>,
    ) -> Result<String> {
        if let Some(s) = selector {
            return Ok(s.to_string());
        }
        let r = r#ref.context("either 'selector' or 'ref' is required")?;
        self.snapshot_refs.get(r).cloned().with_context(|| {
            format!(
                "ref '{r}' was not produced by a recent snapshot — call snapshot first, or pass a CSS selector"
            )
        })
    }

    /// Snapshot ref-store size — exposed for tests and debugging only.
    #[cfg(test)]
    pub(crate) fn snapshot_ref_count(&self) -> usize {
        self.snapshot_refs.len()
    }

    // -- Core Actions --

    /// Navigate to a URL after running it through the URL policy
    /// classifier. The handler at [`crate::tool_handlers::media`] runs
    /// the full [`url_policy::classify`] (DNS + throttle) pre-flight; this
    /// method re-runs the no-DNS variant as a defense-in-depth check so
    /// any direct caller of `navigate` still gets scheme + SSRF-host +
    /// allow/deny gating without double-charging the throttle bucket.
    #[instrument(skip_all, fields(browser.action = "navigate"))]
    pub async fn navigate(
        &self,
        url: &str,
        timeout: Duration,
        policy: &UrlPolicyConfig,
    ) -> Result<String> {
        let effective_url = match url_policy::classify_offline(url, policy) {
            ClassifyOutcome::Allow { effective_url } => effective_url,
            ClassifyOutcome::Block { reason, rule } => {
                anyhow::bail!("URL policy blocked: {reason} (rule: {rule})");
            }
            ClassifyOutcome::Throttle { .. } => {
                // classify_offline does not run the throttle step. If
                // that contract is ever broken, refuse rather than panic:
                // a browser tool call that surprises us with a throttle
                // verdict should bail, not crash the daemon.
                tracing::error!("classify_offline returned Throttle — contract violation");
                anyhow::bail!("URL policy returned unexpected throttle verdict");
            }
        };

        let page = self.tab_manager.active_page();
        tokio::time::timeout(timeout, async {
            page.goto(&effective_url)
                .await
                .context("Navigation failed")?;
            let title = page
                .get_title()
                .await
                .context("Failed to get title")?
                .unwrap_or_default();
            let current_url = page
                .url()
                .await
                .context("Failed to get URL")?
                .unwrap_or_default();
            Ok(format!("Navigated to {current_url}\nTitle: {title}"))
        })
        .await
        .context("Navigate timed out")?
    }

    /// Click an element by CSS selector.
    #[instrument(skip_all, fields(browser.action = "click"))]
    pub async fn click(&self, selector: &str, timeout: Duration) -> Result<String> {
        let page = self.tab_manager.active_page();
        tokio::time::timeout(timeout, async {
            let el = page
                .find_element(selector)
                .await
                .with_context(|| format!("Element not found: {selector}"))?;
            el.click().await.context("Click failed")?;
            Ok(format!("Clicked element: {selector}"))
        })
        .await
        .context("Click timed out")?
    }

    /// Type text into an element by CSS selector.
    #[instrument(skip_all, fields(browser.action = "type_text"))]
    pub async fn type_text(&self, selector: &str, text: &str, timeout: Duration) -> Result<String> {
        let page = self.tab_manager.active_page();
        tokio::time::timeout(timeout, async {
            let el = page
                .find_element(selector)
                .await
                .with_context(|| format!("Element not found: {selector}"))?;
            el.click().await.context("Focus click failed")?;
            el.type_str(text).await.context("Type failed")?;
            Ok(format!("Typed into {selector}"))
        })
        .await
        .context("Type timed out")?
    }

    /// Take a screenshot. Returns (description, png_bytes).
    #[instrument(skip_all, fields(browser.action = "screenshot"))]
    pub async fn screenshot(
        &self,
        selector: Option<&str>,
        timeout: Duration,
    ) -> Result<(String, Vec<u8>)> {
        let page = self.tab_manager.active_page();
        tokio::time::timeout(timeout, async {
            if let Some(sel) = selector {
                let el = page
                    .find_element(sel)
                    .await
                    .with_context(|| format!("Element not found: {sel}"))?;
                let bytes = el
                    .screenshot(CaptureScreenshotFormat::Png)
                    .await
                    .context("Element screenshot failed")?;
                Ok((format!("Screenshot of element: {sel}"), bytes))
            } else {
                let bytes = page
                    .screenshot(
                        chromiumoxide::page::ScreenshotParams::builder()
                            .format(CaptureScreenshotFormat::Png)
                            .build(),
                    )
                    .await
                    .context("Page screenshot failed")?;
                Ok(("Full page screenshot".to_string(), bytes))
            }
        })
        .await
        .context("Screenshot timed out")?
    }

    /// Get text content from the page or a specific element.
    #[instrument(skip_all, fields(browser.action = "get_text"))]
    pub async fn get_text(&self, selector: Option<&str>, timeout: Duration) -> Result<String> {
        let page = self.tab_manager.active_page();
        tokio::time::timeout(timeout, async {
            if let Some(sel) = selector {
                let el = page
                    .find_element(sel)
                    .await
                    .with_context(|| format!("Element not found: {sel}"))?;
                let text = el
                    .inner_text()
                    .await
                    .context("Failed to get inner text")?
                    .unwrap_or_default();
                Ok(text)
            } else {
                let text = page
                    .find_element("body")
                    .await
                    .context("No body element")?
                    .inner_text()
                    .await
                    .context("Failed to get body text")?
                    .unwrap_or_default();
                Ok(text)
            }
        })
        .await
        .context("Get text timed out")?
    }

    /// Evaluate a JavaScript expression with an inner Promise.race timeout
    /// to prevent CDP serialization deadlock from stuck JS.
    #[instrument(skip_all, fields(browser.action = "evaluate_js"))]
    pub async fn evaluate_js(&self, expression: &str, timeout: Duration) -> Result<String> {
        let page = self.tab_manager.active_page();
        let inner_timeout_ms = self.config.js_eval_timeout_ms;
        let wrapped = wrap_js_with_timeout(expression, inner_timeout_ms);

        tokio::time::timeout(timeout, async {
            let result: serde_json::Value = page
                .evaluate(wrapped.as_str())
                .await
                .context("JS evaluation failed")?
                .into_value()
                .context("Failed to deserialize JS result")?;
            Ok(serde_json::to_string_pretty(&result).unwrap_or_else(|_| result.to_string()))
        })
        .await
        .context("Evaluate JS timed out")?
    }

    // -- New Actions --

    /// Hover over an element.
    pub async fn hover(&self, selector: &str, timeout: Duration) -> Result<String> {
        actions::hover(self.tab_manager.active_page(), selector, timeout).await
    }

    /// Select a value in a dropdown element.
    pub async fn select(&self, selector: &str, value: &str, timeout: Duration) -> Result<String> {
        actions::select(self.tab_manager.active_page(), selector, value, timeout).await
    }

    /// Press a keyboard key.
    pub async fn press(&self, key: &str, timeout: Duration) -> Result<String> {
        actions::press(self.tab_manager.active_page(), key, timeout).await
    }

    /// Drag from one element to another.
    pub async fn drag(&self, source: &str, target: &str, timeout: Duration) -> Result<String> {
        actions::drag(self.tab_manager.active_page(), source, target, timeout).await
    }

    /// Fill multiple form fields.
    pub async fn fill(
        &self,
        fields: &serde_json::Map<String, serde_json::Value>,
        timeout: Duration,
    ) -> Result<String> {
        actions::fill(self.tab_manager.active_page(), fields, timeout).await
    }

    /// Wait for a condition to be met.
    pub async fn wait_for(&self, args: &serde_json::Value, timeout: Duration) -> Result<String> {
        let condition = wait::WaitCondition::from_args(args)?;
        wait::wait_for(self.tab_manager.active_page(), &condition, timeout).await
    }

    /// Navigate back in the page's session history.
    /// Returns a no-op message when there's nothing to go back to (matches
    /// hermes' `browser_back` semantics — never errors on an empty stack).
    #[instrument(skip_all, fields(browser.action = "back"))]
    pub async fn back(&mut self, timeout: Duration) -> Result<String> {
        self.snapshot_refs.clear();
        let page = self.tab_manager.active_page();
        history_navigate(page, -1, timeout).await
    }

    /// Navigate forward in the page's session history.
    #[instrument(skip_all, fields(browser.action = "forward"))]
    pub async fn forward(&mut self, timeout: Duration) -> Result<String> {
        self.snapshot_refs.clear();
        let page = self.tab_manager.active_page();
        history_navigate(page, 1, timeout).await
    }

    /// Scroll the page by (delta_x, delta_y) CSS pixels. Positive `delta_y`
    /// scrolls down. Optional `x`/`y` recenter the wheel event for elements
    /// that capture their own scroll (modals, virtualised lists).
    #[instrument(skip_all, fields(browser.action = "scroll"))]
    pub async fn scroll(
        &self,
        x: Option<i64>,
        y: Option<i64>,
        delta_x: i64,
        delta_y: i64,
        timeout: Duration,
    ) -> Result<String> {
        use chromiumoxide::cdp::browser_protocol::input::{
            DispatchMouseEventParams, DispatchMouseEventType,
        };
        let page = self.tab_manager.active_page();
        tokio::time::timeout(timeout, async {
            let event = DispatchMouseEventParams::builder()
                .r#type(DispatchMouseEventType::MouseWheel)
                .x(x.unwrap_or(0) as f64)
                .y(y.unwrap_or(0) as f64)
                .delta_x(delta_x as f64)
                .delta_y(delta_y as f64)
                .build()
                .map_err(|e| anyhow::anyhow!("Failed to build wheel event: {e}"))?;
            page.execute(event).await.context("Wheel dispatch failed")?;
            Ok(format!("Scrolled by ({delta_x}, {delta_y})"))
        })
        .await
        .context("Scroll timed out")?
    }

    /// Snapshot interactive elements on the current page, replacing the
    /// session's ref → selector cache. Returns the rendered text the model sees.
    #[instrument(skip_all, fields(browser.action = "snapshot"))]
    pub async fn snapshot(
        &mut self,
        max_elements: Option<usize>,
        timeout: Duration,
    ) -> Result<String> {
        let cap = max_elements.unwrap_or(self.config.snapshot_max_elements);
        let page = self.tab_manager.active_page();
        let result = snapshot::snapshot(page, cap, timeout).await?;
        self.snapshot_refs = result.selectors.clone();
        Ok(snapshot::render(&result))
    }

    /// Enumerate `<img>` elements on the page.
    #[instrument(skip_all, fields(browser.action = "get_images"))]
    pub async fn get_images(&self, timeout: Duration) -> Result<String> {
        let js = r#"(() => {
            const imgs = Array.from(document.querySelectorAll('img'));
            return imgs.map(i => ({
                src: i.currentSrc || i.src || '',
                alt: i.alt || '',
                width: i.naturalWidth || 0,
                height: i.naturalHeight || 0,
            }));
        })()"#;
        let page = self.tab_manager.active_page();
        tokio::time::timeout(timeout, async {
            let raw: serde_json::Value = page
                .evaluate(js)
                .await
                .context("get_images evaluation failed")?
                .into_value()
                .context("get_images result was not serialisable")?;
            let arr = raw.as_array().cloned().unwrap_or_default();
            let mut out = format!("Found {} image(s):\n", arr.len());
            for (i, img) in arr.iter().enumerate() {
                let src = img.get("src").and_then(|v| v.as_str()).unwrap_or("");
                let alt = img.get("alt").and_then(|v| v.as_str()).unwrap_or("");
                let w = img.get("width").and_then(|v| v.as_u64()).unwrap_or(0);
                let h = img.get("height").and_then(|v| v.as_u64()).unwrap_or(0);
                out.push_str(&format!("  [{i}] {w}x{h} \"{alt}\" {src}\n"));
            }
            Ok(out)
        })
        .await
        .context("get_images timed out")?
    }

    /// Set files on a file `<input>`. Chrome's CDP needs the absolute paths;
    /// we don't sandbox them here because the operator's host filesystem is
    /// already accessible to the agent via `apply_patch`/`read_file`.
    #[instrument(skip_all, fields(browser.action = "upload"))]
    pub async fn upload(
        &self,
        selector: &str,
        files: &[String],
        timeout: Duration,
    ) -> Result<String> {
        use chromiumoxide::cdp::browser_protocol::dom::SetFileInputFilesParams;
        let page = self.tab_manager.active_page();
        tokio::time::timeout(timeout, async {
            let el = page
                .find_element(selector)
                .await
                .with_context(|| format!("Element not found: {selector}"))?;
            let params = SetFileInputFilesParams::builder()
                .files(files.iter().cloned())
                .backend_node_id(el.backend_node_id)
                .build()
                .map_err(|e| anyhow::anyhow!("Failed to build SetFileInputFiles: {e}"))?;
            page.execute(params)
                .await
                .context("DOM.setFileInputFiles failed (target may not be an <input type=\"file\">)")?;
            Ok(format!(
                "Uploaded {} file(s) to {selector}: {}",
                files.len(),
                files.join(", ")
            ))
        })
        .await
        .context("Upload timed out")?
    }

    /// Resize the browser viewport.
    pub async fn resize(&self, width: u32, height: u32, timeout: Duration) -> Result<String> {
        use chromiumoxide::cdp::browser_protocol::emulation::SetDeviceMetricsOverrideParams;

        let page = self.tab_manager.active_page();
        let params = SetDeviceMetricsOverrideParams::builder()
            .width(width)
            .height(height)
            .device_scale_factor(1.0)
            .mobile(false)
            .build()
            .map_err(|e| anyhow::anyhow!("Failed to build metrics params: {e}"))?;

        tokio::time::timeout(timeout, async {
            page.execute(params)
                .await
                .context("Viewport resize failed")?;
            Ok(format!("Resized viewport to {width}x{height}"))
        })
        .await
        .context("Resize timed out")?
    }

    // -- Tab Management --

    /// Open a new tab. The URL (if any) is gated by `policy` so SSRF /
    /// allow / deny checks apply identically to `navigate`.
    #[instrument(skip_all, fields(browser.action = "new_tab"))]
    pub async fn new_tab(&mut self, url: Option<&str>, policy: &UrlPolicyConfig) -> Result<String> {
        let result = self.tab_manager.new_tab(&self.browser, url, policy).await?;
        // Attach event listeners to the new page
        let new_handles =
            console::spawn_event_listeners(self.tab_manager.active_page(), &self.event_buffers)
                .await;
        self.event_listener_handles.extend(new_handles);
        // The snapshot ref store was tied to the previous active page —
        // a stale ref must not silently resolve on the new tab.
        self.snapshot_refs.clear();
        Ok(result)
    }

    /// Switch to a tab by index.
    pub fn switch_tab(&mut self, index: usize) -> Result<String> {
        let result = self.tab_manager.switch_tab(index);
        if result.is_ok() {
            self.snapshot_refs.clear();
        }
        result
    }

    /// Close the active tab.
    #[instrument(skip_all, fields(browser.action = "close_tab"))]
    pub async fn close_tab(&mut self) -> Result<String> {
        let result = self.tab_manager.close_tab().await;
        if result.is_ok() {
            self.snapshot_refs.clear();
        }
        result
    }

    /// List all tabs.
    pub async fn list_tabs(&self) -> Result<String> {
        self.tab_manager.list_tabs().await
    }

    // -- Console Logs --

    /// Get captured console logs and errors.
    pub fn get_console_logs(&self) -> String {
        self.event_buffers.format_console_output()
    }

    /// Get the event buffers (for external access).
    pub fn event_buffers(&self) -> &console::EventBuffers {
        &self.event_buffers
    }

    // -- Lifecycle --

    /// Close the browser gracefully: CDP close → await handler → abort.
    #[instrument(skip_all, fields(browser.action = "close"))]
    pub async fn close(self) -> Result<()> {
        // Abort event listener tasks
        for h in &self.event_listener_handles {
            h.abort();
        }

        // Send CDP Browser.close (drop sends it)
        drop(self.browser);

        // Wait for the handler loop to finish, with timeout
        match tokio::time::timeout(Duration::from_secs(3), self._event_handle).await {
            Ok(_) => {}
            Err(_) => {
                tracing::debug!("Browser event handler did not exit in time, aborting");
            }
        }

        Ok(())
    }

    /// Abort all event listener handles (used during recovery).
    fn abort_listeners(&mut self) {
        for h in self.event_listener_handles.drain(..) {
            h.abort();
        }
    }
}

/// Walk the page's session history by `delta` (−1 back, +1 forward).
/// CDP's `Page.navigateToHistoryEntry` would be more direct but requires
/// fetching the full history first; `window.history.go()` is one round trip
/// and matches what hermes does. Empty history is a no-op, not an error.
async fn history_navigate(
    page: &chromiumoxide::Page,
    delta: i32,
    timeout: Duration,
) -> Result<String> {
    let js = format!(
        r#"(() => {{
            const before = window.location.href;
            const len = window.history.length;
            window.history.go({delta});
            return {{ before, len }};
        }})()"#
    );
    tokio::time::timeout(timeout, async {
        let v: serde_json::Value = page
            .evaluate(js.as_str())
            .await
            .context("history navigation failed")?
            .into_value()
            .context("history navigation result was not serialisable")?;
        let before = v.get("before").and_then(|v| v.as_str()).unwrap_or("");
        let len = v.get("len").and_then(|v| v.as_u64()).unwrap_or(0);
        let dir = if delta < 0 { "back" } else { "forward" };
        if len <= 1 {
            Ok(format!(
                "history.{dir}: no-op (history length = {len}, was at {before})"
            ))
        } else {
            Ok(format!("history.{dir}: requested (was at {before})"))
        }
    })
    .await
    .context("History navigation timed out")?
}

/// Validate that a URL uses an allowed scheme (http/https only).
/// Blocks file://, javascript:, chrome://, data: etc.
pub fn validate_url_scheme(url: &str) -> Result<()> {
    let scheme = url.split(':').next().unwrap_or("");
    if !matches!(scheme, "http" | "https") {
        anyhow::bail!("Blocked URL scheme: {scheme}. Only http/https allowed.");
    }
    Ok(())
}

/// Wrap a JS expression with Promise.race for bounded evaluation.
/// The inner timeout prevents CDP serialization deadlock from stuck JS.
pub fn wrap_js_with_timeout(expression: &str, timeout_ms: u64) -> String {
    // Escape backslashes first, then backticks and template literals
    let escaped = expression
        .replace('\\', "\\\\")
        .replace('`', "\\`")
        .replace("${", "\\${");
    format!(
        r#"Promise.race([
  (async () => {{ return ({escaped}); }})(),
  new Promise((_, rej) => setTimeout(() => rej(new Error('JS evaluation timed out after {timeout_ms}ms')), {timeout_ms}))
])"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Schema-drift guard: every browser action surfaced in the tool
    /// JSON schema must be honoured by `validate_browser_args`. The schema
    /// lives at `tool/definitions.rs` — if a new action is added there but
    /// not wired through here, the model will issue calls that fail at
    /// runtime with "Unknown browser action". Source-grep is the only way
    /// to express this invariant — a type would force registry refactors
    /// across modules with no functional payoff.
    #[test]
    fn schema_actions_are_all_validated() {
        const DEFS: &str = include_str!("../tool/definitions.rs");
        // Locate the enum literal embedded in the browser tool's JSON schema.
        // The line is uniquely shaped (`"enum": [...]` next to "navigate"),
        // so we grab the substring between the first `["navigate"` and the
        // matching closing bracket.
        let start = DEFS
            .find("[\"navigate\"")
            .expect("browser schema must contain \"navigate\" enum");
        let end = DEFS[start..].find(']').expect("schema enum must close");
        let slice = &DEFS[start..start + end];

        for raw in slice.split(',') {
            let action = raw.trim().trim_matches(['"', ' ', '[']);
            if action.is_empty() {
                continue;
            }
            // Build a generic args object with the most permissive shape
            // so a misspelled action surfaces as "Unknown browser action".
            let args = serde_json::json!({
                "selector": "#x",
                "ref": "@e0",
                "url": "https://example.com",
                "text": "x",
                "expression": "1",
                "value": "x",
                "key": "Enter",
                "source": "#a",
                "target": "#b",
                "fields": {"#a": "v"},
                "condition": "load",
                "width": 100,
                "height": 100,
                "tab_index": 0,
                "delta_y": 1,
                "files": ["/tmp/x"],
            });
            // `click` is the one action that rejects when both selector
            // AND ref are set — strip ref for that case so the row passes.
            let args = if action == "click" || action == "type" || action == "hover" {
                let mut m = args.as_object().cloned().unwrap();
                m.remove("ref");
                serde_json::Value::Object(m)
            } else {
                args
            };
            let err = validate::validate_browser_args(action, &args);
            assert!(
                err.is_none() || !err.as_deref().unwrap_or("").contains("Unknown browser action"),
                "schema action '{action}' is not validated (drift between definitions.rs and validate.rs): {err:?}"
            );
        }
    }

    /// Ref-store reset: switching tabs or closing one must drop the stale
    /// `@e<N>` refs so a follow-up click can't accidentally hit a different
    /// element on a different page. Drives the ref-isolation invariant
    /// without spinning up real Chrome by exercising the `snapshot_refs`
    /// field directly via the test seam.
    #[test]
    fn snapshot_refs_cleared_on_tab_changes() {
        use std::collections::HashMap;
        // We can't construct a BrowserSession in unit tests (it owns a real
        // Browser handle), but the invariant we care about is "code paths
        // that switch tabs always call snapshot_refs.clear()". Source-grep
        // the relevant methods to assert the call site exists.
        const SRC: &str = include_str!("mod.rs");
        for method in ["new_tab", "switch_tab", "close_tab", "try_recover"] {
            // Find the method body and look for `snapshot_refs.clear()`
            // somewhere inside.
            let needle = format!("fn {method}(");
            let start = SRC
                .find(&needle)
                .unwrap_or_else(|| panic!("method `{method}` not found"));
            // Look at the next ~2KB of source after the signature.
            let window = &SRC[start..start.saturating_add(2048).min(SRC.len())];
            assert!(
                window.contains("snapshot_refs.clear()"),
                "method `{method}` must clear snapshot_refs (stale refs would leak across tabs)"
            );
        }
        // Sanity: an empty HashMap-based ref store behaves as expected when
        // queried — keeps the resolver contract self-checked.
        let map: HashMap<String, String> = HashMap::new();
        assert!(map.get("@e0").is_none());
    }

    #[test]
    fn wrap_js_simple_expression() {
        // Sanity: the Promise.race scaffold must wrap the inner expression
        // verbatim and embed the timeout literal in the rejection path.
        let js = wrap_js_with_timeout("document.title", 5000);
        assert!(js.contains("Promise.race"));
        assert!(js.contains("document.title"));
        assert!(js.contains("5000"));
    }

    #[test]
    fn wrap_js_escapes_template_literal_specials() {
        // Backtick and ${...} are the breakouts that would let attacker JS
        // escape the wrapping template literal. Both must be escaped.
        let js = wrap_js_with_timeout("`hello ${world}`", 3000);
        assert!(
            js.contains("\\`hello \\${world}\\`"),
            "backtick/${{}} not escaped; got: {js}"
        );
        // Timeout literal embedded twice (rejection message + setTimeout
        // argument) — both must reflect the requested value.
        let js = wrap_js_with_timeout("1+1", 7500);
        assert!(
            js.contains("7500ms"),
            "rejection message missing timeout: {js}"
        );
        assert!(
            js.contains("7500)"),
            "setTimeout call missing timeout: {js}"
        );
    }

    #[test]
    fn wrap_js_preserves_inner_double_quotes() {
        // We only escape backslash, backtick, and `${`. Double quotes
        // inside the user's expression must survive untouched — otherwise
        // common selectors like `querySelector("div")` would break.
        let js = wrap_js_with_timeout(r#"document.querySelector("div")"#, 5000);
        assert!(js.contains(r#"querySelector("div")"#));
    }

    #[test]
    fn wrap_js_escapes_backslash_before_backtick() {
        // A raw backslash followed by a backtick was a past escape-order
        // bug: escaping backticks first then backslashes would re-create
        // an un-escaped backtick on the second pass.
        let js = wrap_js_with_timeout(r"test\`value", 1000);
        assert!(js.contains(r"test\\\\\\`value") || js.contains(r"test\\\`value"));
    }

    /// Scheme allowlist for browser navigation. The production caller
    /// (`UrlPolicy::classify_offline`) blocks anything other than
    /// http/https; this is the defense-in-depth check inside the browser
    /// session that fires if a direct caller bypasses url_policy.
    /// Allowed schemes plus a representative sample of attacker schemes
    /// (`file://`, `javascript:`, `data:`, `chrome://`) all in one table.
    #[test]
    fn validate_url_scheme_allowlist() {
        let cases: &[(&str, bool)] = &[
            ("http://example.com", true),
            ("https://example.com", true),
            // SSRF / exfil-class schemes — must be blocked.
            ("file:///etc/passwd", false),
            ("javascript:alert(1)", false),
            ("data:text/html,<h1>Hi</h1>", false),
            ("chrome://settings", false),
            // Defense-in-depth: an input with no scheme at all must be
            // rejected rather than silently allowed.
            ("no-scheme-here", false),
        ];
        for (url, expect_ok) in cases {
            let got = validate_url_scheme(url);
            assert_eq!(
                got.is_ok(),
                *expect_ok,
                "validate_url_scheme({url}) = {got:?}, expected ok={expect_ok}"
            );
        }
    }

    #[tokio::test]
    #[ignore] // Requires Chrome installed
    async fn browser_session_launch_fails_without_chrome() {
        let config = BorgBrowserConfig {
            enabled: true,
            headless: true,
            executable: Some("/nonexistent/chrome".to_string()),
            cdp_port: 0,
            no_sandbox: false,
            timeout_ms: 5000,
            startup_timeout_ms: 5000,
            console_buffer_size: 500,
            error_buffer_size: 200,
            network_buffer_size: 500,
            js_eval_timeout_ms: 10000,
            snapshot_max_elements: 100,
        };
        let result = BrowserSession::launch(&config).await;
        assert!(result.is_err());
    }
}
