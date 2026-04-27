pub mod actions;
pub mod console;
pub mod detection;
pub mod health;
pub mod tabs;
pub mod validate;
pub mod wait;

use std::time::Duration;

use anyhow::{Context, Result};
use chromiumoxide::browser::{Browser, BrowserConfig};
use chromiumoxide::cdp::browser_protocol::page::CaptureScreenshotFormat;
use futures::StreamExt;
use tracing::instrument;

use crate::config::BrowserConfig as BorgBrowserConfig;

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
}

impl BrowserSession {
    /// Launch Chrome and open a default page.
    #[instrument(skip_all, fields(browser.action = "launch"))]
    pub async fn launch(config: &BorgBrowserConfig) -> Result<Self> {
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

        Ok(Self {
            browser,
            tab_manager: tabs::TabManager::new(page),
            _event_handle: event_handle,
            event_listener_handles,
            event_buffers,
            config: config.clone(),
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

        tracing::info!("Browser recovery: full relaunch complete");
        Ok(())
    }

    // -- Core Actions --

    /// Navigate to a URL. Only http/https URLs are allowed.
    #[instrument(skip_all, fields(browser.action = "navigate"))]
    pub async fn navigate(&self, url: &str, timeout: Duration) -> Result<String> {
        validate_url_scheme(url)?;

        let page = self.tab_manager.active_page();
        tokio::time::timeout(timeout, async {
            page.goto(url).await.context("Navigation failed")?;
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

    /// Open a new tab.
    #[instrument(skip_all, fields(browser.action = "new_tab"))]
    pub async fn new_tab(&mut self, url: Option<&str>) -> Result<String> {
        let result = self.tab_manager.new_tab(&self.browser, url).await?;
        // Attach event listeners to the new page
        let new_handles =
            console::spawn_event_listeners(self.tab_manager.active_page(), &self.event_buffers)
                .await;
        self.event_listener_handles.extend(new_handles);
        Ok(result)
    }

    /// Switch to a tab by index.
    pub fn switch_tab(&mut self, index: usize) -> Result<String> {
        self.tab_manager.switch_tab(index)
    }

    /// Close the active tab.
    #[instrument(skip_all, fields(browser.action = "close_tab"))]
    pub async fn close_tab(&mut self) -> Result<String> {
        self.tab_manager.close_tab().await
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

    #[test]
    fn wrap_js_simple_expression() {
        let js = wrap_js_with_timeout("document.title", 5000);
        assert!(js.contains("Promise.race"));
        assert!(js.contains("document.title"));
        assert!(js.contains("5000"));
    }

    #[test]
    fn wrap_js_with_backticks() {
        let js = wrap_js_with_timeout("`hello ${world}`", 3000);
        assert!(js.contains("\\`hello \\${world}\\`"));
        assert!(js.contains("3000"));
    }

    #[test]
    fn wrap_js_complex_expression() {
        let js = wrap_js_with_timeout("document.querySelectorAll('div').length", 10000);
        assert!(js.contains("querySelectorAll"));
        assert!(js.contains("10000"));
    }

    #[test]
    fn wrap_js_timeout_value_embedded() {
        let js = wrap_js_with_timeout("1+1", 7500);
        assert!(js.contains("7500ms"));
        assert!(js.contains("7500)"));
    }

    #[test]
    fn wrap_js_preserves_quotes() {
        let js = wrap_js_with_timeout(r#"document.querySelector("div")"#, 5000);
        assert!(js.contains(r#"querySelector("div")"#));
    }

    #[test]
    fn validate_url_scheme_allows_http() {
        assert!(validate_url_scheme("http://example.com").is_ok());
    }

    #[test]
    fn validate_url_scheme_allows_https() {
        assert!(validate_url_scheme("https://example.com").is_ok());
    }

    #[test]
    fn validate_url_scheme_blocks_file() {
        assert!(validate_url_scheme("file:///etc/passwd").is_err());
    }

    #[test]
    fn validate_url_scheme_blocks_javascript() {
        assert!(validate_url_scheme("javascript:alert(1)").is_err());
    }

    #[test]
    fn validate_url_scheme_blocks_data() {
        assert!(validate_url_scheme("data:text/html,<h1>Hi</h1>").is_err());
    }

    #[test]
    fn validate_url_scheme_blocks_chrome() {
        assert!(validate_url_scheme("chrome://settings").is_err());
    }

    #[test]
    fn wrap_js_escapes_backslash_before_backtick() {
        // A raw backslash followed by a backtick should be double-escaped
        let js = wrap_js_with_timeout(r"test\`value", 1000);
        assert!(js.contains(r"test\\\\\\`value") || js.contains(r"test\\\`value"));
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
        };
        let result = BrowserSession::launch(&config).await;
        assert!(result.is_err());
    }
}
