use std::time::Duration;

use anyhow::{Context, Result};
use base64::Engine as _;
use tracing::instrument;

use crate::browser::{validate_browser_args, BrowserSession};
use crate::config::Config;
use crate::types::{ContentPart, MediaData, ToolOutput};

use super::{check_enabled, require_str_param};

#[instrument(skip_all, fields(tool.name = "generate_image"))]
pub async fn handle_generate_image(args: &serde_json::Value, config: &Config) -> Result<String> {
    if let Some(msg) = check_enabled(config.image_gen.enabled, "image_gen") {
        return Ok(msg);
    }

    let prompt = args
        .get("prompt")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    if prompt.is_empty() {
        return Ok("Error: prompt is required".to_string());
    }

    let count = args
        .get("count")
        .and_then(serde_json::Value::as_u64)
        .map(|n| n as u32);
    let size = args.get("size").and_then(serde_json::Value::as_str);

    let provider = match crate::media::image_gen::ImageGenProvider::from_config(&config.image_gen) {
        Some(p) => p,
        None => {
            return Ok(
                "No image generation provider available. Set OPENAI_API_KEY or FAL_KEY environment variable, or configure [image_gen] in config.toml"
                    .to_string(),
            );
        }
    };

    match crate::media::image_gen::generate_image(&provider, prompt, size, count).await {
        Ok(results) if results.is_empty() => Ok("Image generation returned no results".to_string()),
        Ok(results) => {
            let count = results.len();
            let mut output = format!("Generated {count} image(s).\n");
            for (i, img) in results.iter().enumerate() {
                if let Some(ref revised) = img.revised_prompt {
                    output.push_str(&format!("Image {}: revised prompt: {revised}\n", i + 1));
                }
                let preview_len = img.base64_data.len().min(100);
                output.push_str(&format!(
                    "Image {}: {} bytes (base64: {}...)\n",
                    i + 1,
                    img.base64_data.len() * 3 / 4,
                    &img.base64_data[..preview_len]
                ));
            }
            Ok(output)
        }
        Err(e) => Ok(format!("Image generation failed: {e}")),
    }
}

#[instrument(skip_all, fields(tool.name = "text_to_speech"))]
pub async fn handle_text_to_speech(
    args: &serde_json::Value,
    synthesizer: &crate::media::tts::TtsSynthesizer,
) -> ToolOutput {
    let text = match require_str_param(args, "text") {
        Ok(t) => t,
        Err(e) => return ToolOutput::Text(format!("Error: {e}")),
    };
    let voice = args.get("voice").and_then(|v| v.as_str());
    let format = args
        .get("format")
        .and_then(|v| v.as_str())
        .and_then(crate::media::tts::AudioFormat::from_str_lossy);

    match synthesizer.synthesize(text, voice, format).await {
        Ok((audio_bytes, fmt, _attempts)) => {
            let b64 = base64::engine::general_purpose::STANDARD.encode(&audio_bytes);
            ToolOutput::Multimodal {
                text: format!(
                    "Generated {} audio ({} bytes, {})",
                    fmt.extension(),
                    audio_bytes.len(),
                    fmt.mime_type()
                ),
                parts: vec![ContentPart::AudioBase64 {
                    media: MediaData {
                        mime_type: fmt.mime_type().to_string(),
                        data: b64,
                        filename: Some(format!("speech.{}", fmt.extension())),
                    },
                }],
            }
        }
        Err(e) => ToolOutput::Text(format!("TTS error: {e}")),
    }
}

#[instrument(skip_all, fields(tool.name = "browser"))]
pub async fn handle_browser(
    args: &serde_json::Value,
    config: &Config,
    session: &mut Option<BrowserSession>,
) -> Result<ToolOutput> {
    if !config.browser.enabled {
        return Ok(ToolOutput::Text(
            "Browser automation is disabled. Enable it in config: [browser] enabled = true"
                .to_string(),
        ));
    }

    let action = require_str_param(args, "action")?;

    if let Some(err_msg) = validate_browser_args(action, args) {
        return Ok(ToolOutput::Text(format!("Error: {err_msg}")));
    }

    // Handle close without needing a session
    if action == "close" {
        if let Some(s) = session.take() {
            s.close().await.ok();
            return Ok(ToolOutput::Text("Browser closed.".to_string()));
        }
        return Ok(ToolOutput::Text("No browser session to close.".to_string()));
    }

    // Pre-flight: for navigate / new_tab, run the URL through the policy
    // classifier BEFORE launching Chrome. This avoids paying the Chrome
    // startup cost on a request the agent isn't allowed to make, and
    // ensures the blocked-URL surface is identical whether or not a
    // session exists. new_tab's URL is optional — only classify if set.
    let preflight_url = match action {
        "navigate" => Some(require_str_param(args, "url")?),
        "new_tab" => args.get("url").and_then(serde_json::Value::as_str),
        _ => None,
    };
    if let Some(url) = preflight_url {
        let outcome = crate::url_policy::classify(url, &config.security.url_policy).await;
        if let Some(msg) = crate::url_policy::describe_block_or_throttle(&outcome) {
            return Ok(ToolOutput::Text(format!("Error: {msg}")));
        }
    }

    // Drop any active session whose backend/engine/attach settings drifted since
    // it was created — `/browser connect|disconnect`, a backend switch (local ↔
    // cloud, or one cloud provider to another), and an engine change all take
    // effect on the very next tool call without needing an explicit signal.
    // `is_stale` compares the session's actual attach mode against the live
    // config; a stale cloud session is released on the provider by `close()`, so
    // switching away never leaks (or keeps billing) the old keep-alive session.
    if session.as_ref().is_some_and(|s| s.is_stale(&config.browser)) {
        if let Some(prev) = session.take() {
            tracing::info!("browser backend/engine setting drifted — closing prior session");
            // Close failures are non-fatal (CDP channel may already be dead) but
            // must be logged — without this a broken disconnect path is invisible.
            if let Err(e) = prev.close().await {
                tracing::warn!(error = %e, "failed to close prior browser session before reattach");
            }
        }
    }

    // Lazy-launch (or connect to) a browser session. Backend precedence:
    //   1. An explicit CDP attach URL wins (operator ran `/browser connect`).
    //   2. A remote cloud backend (browserbase / browser_use / firecrawl)
    //      creates a session and hands us its CDP endpoint to connect to.
    //   3. Otherwise launch a local browser for the configured engine.
    if session.is_none() {
        let outcome = if let Some(url) = config.browser.cdp_url.as_deref() {
            BrowserSession::connect(&config.browser, url)
                .await
                .map(|s| (s, None))
        } else if config.browser.backend == crate::config::BrowserBackend::CdpAttach {
            // backend=cdp_attach means "attach to an operator-managed browser",
            // but no browser.cdp_url is set to attach to. Silently launching a
            // fresh local Chrome would ignore the operator's intent (a different
            // profile with no cookies), so surface the misconfiguration.
            Err(anyhow::anyhow!(
                "browser.backend is 'cdp_attach' but browser.cdp_url is not set. \
                 Set browser.cdp_url (or run `/browser connect <url>`), or switch \
                 browser.backend to 'local' to launch a managed browser."
            ))
        } else {
            match crate::browser::backends::create_remote_session(
                &config.browser,
                &config.security.url_policy,
            )
            .await
            {
                Ok(Some(remote)) => {
                    match BrowserSession::connect(&config.browser, &remote.cdp_url).await {
                        Ok(s) => Ok((s, Some(remote))),
                        Err(e) => {
                            // The provider already created (and is billing) the
                            // session; release it before surfacing the connect
                            // error so we don't leak a keep-alive session.
                            crate::browser::backends::close_remote_session(
                                &config.browser,
                                &remote,
                            )
                            .await;
                            Err(e)
                        }
                    }
                }
                Ok(None) => BrowserSession::launch(&config.browser)
                    .await
                    .map(|s| (s, None)),
                Err(e) => Err(e),
            }
        };
        match outcome {
            Ok((mut s, remote)) => {
                if let Some(remote) = remote {
                    s.attach_remote(remote);
                }
                *session = Some(s);
            }
            Err(e) => {
                let hint = if let Some(url) = config.browser.cdp_url.as_deref() {
                    format!(
                        "Error opening browser session via CDP at {url}: {e}\n\
                         Hint: the configured browser may have exited or stopped serving CDP. \
                         Run `/browser disconnect` to revert to headless, or relaunch the browser \
                         with --remote-debugging-port and retry `/browser connect`."
                    )
                } else if config.browser.backend.is_remote() {
                    format!(
                        "Error opening remote browser session via '{backend}': {e}\n\
                         Hint: verify browser.{backend}_api_key (and, for browserbase, \
                         browser.browserbase_project_id) are set correctly, or switch \
                         browser.backend back to 'local'.",
                        backend = config.browser.backend
                    )
                } else {
                    format!("Error launching browser: {e}")
                };
                return Ok(ToolOutput::Text(hint));
            }
        }
    }

    let browser = session.as_mut().context("Browser session not available")?;
    let timeout = Duration::from_millis(config.browser.timeout_ms);

    /// Wrap a browser action result into a ToolOutput.
    fn browser_result(result: anyhow::Result<String>) -> Result<ToolOutput> {
        match result {
            Ok(msg) => Ok(ToolOutput::Text(msg)),
            Err(e) => Ok(ToolOutput::Text(format!("Error: {e}"))),
        }
    }

    /// Run a browser action with automatic recovery: try once, and if it fails
    /// with a recoverable error, attempt recovery then retry.
    macro_rules! browser_action_with_recovery {
        ($browser:expr, $name:expr, $action:expr) => {{
            let result = $action;
            if let Err(e) = &result {
                if crate::browser::health::is_recoverable_error(e) {
                    tracing::warn!("{} failed with recoverable error: {e}", $name);
                    if $browser.try_recover().await.is_ok() {
                        return browser_result($action);
                    }
                }
            }
            browser_result(result)
        }};
    }

    match action {
        "navigate" => {
            let url = require_str_param(args, "url")?;
            let policy = &config.security.url_policy;
            browser_action_with_recovery!(
                browser,
                "navigate",
                browser.navigate(url, timeout, policy).await
            )
        }
        "click" => {
            let selector_opt = args.get("selector").and_then(|v| v.as_str());
            let ref_opt = args.get("ref").and_then(|v| v.as_str());
            let resolved = match browser.resolve_selector_or_ref(selector_opt, ref_opt) {
                Ok(s) => s,
                Err(e) => return Ok(ToolOutput::Text(format!("Error: {e}"))),
            };
            browser_action_with_recovery!(browser, "click", browser.click(&resolved, timeout).await)
        }
        "type" => {
            let selector_opt = args.get("selector").and_then(|v| v.as_str());
            let ref_opt = args.get("ref").and_then(|v| v.as_str());
            let resolved = match browser.resolve_selector_or_ref(selector_opt, ref_opt) {
                Ok(s) => s,
                Err(e) => return Ok(ToolOutput::Text(format!("Error: {e}"))),
            };
            let text = require_str_param(args, "text")?;
            browser_action_with_recovery!(
                browser,
                "type",
                browser.type_text(&resolved, text, timeout).await
            )
        }
        "screenshot" => {
            let selector = args.get("selector").and_then(|v| v.as_str());
            match browser
                .screenshot(selector, timeout, &config.security.url_policy)
                .await
            {
                Ok((desc, png_bytes)) => {
                    let saved_path = Config::data_dir().ok().and_then(|data_dir| {
                        let dir = data_dir.join("screenshots");
                        if let Err(e) = std::fs::create_dir_all(&dir) {
                            tracing::warn!(
                                "Failed to create screenshot dir {}: {e}",
                                dir.display()
                            );
                            return None;
                        }
                        let ts = chrono::Utc::now().format("%Y%m%d_%H%M%S%3f");
                        let path = dir.join(format!("screenshot_{ts}.png"));
                        if let Err(e) = std::fs::write(&path, &png_bytes) {
                            tracing::warn!("Failed to save screenshot to {}: {e}", path.display());
                            return None;
                        }
                        Some(path)
                    });

                    let text = match &saved_path {
                        Some(p) => format!("{desc}\nSaved to: {}", p.display()),
                        None => desc.clone(),
                    };

                    let b64 = base64::Engine::encode(
                        &base64::engine::general_purpose::STANDARD,
                        &png_bytes,
                    );
                    Ok(ToolOutput::Multimodal {
                        text,
                        parts: vec![
                            ContentPart::Text(desc),
                            ContentPart::ImageBase64 {
                                media: MediaData {
                                    mime_type: "image/png".to_string(),
                                    data: b64,
                                    filename: Some("screenshot.png".to_string()),
                                },
                            },
                        ],
                    })
                }
                Err(e) => Ok(ToolOutput::Text(format!("Error: {e}"))),
            }
        }
        "get_text" => {
            let selector = args.get("selector").and_then(|v| v.as_str());
            browser_action_with_recovery!(
                browser,
                "get_text",
                browser
                    .get_text(selector, timeout, &config.security.url_policy)
                    .await
            )
        }
        "evaluate_js" => {
            let expression = require_str_param(args, "expression")?;
            let policy = &config.security.url_policy;
            browser_action_with_recovery!(
                browser,
                "evaluate_js",
                browser.evaluate_js(expression, timeout, policy).await
            )
        }
        "hover" => {
            let selector = require_str_param(args, "selector")?;
            browser_action_with_recovery!(browser, "hover", browser.hover(selector, timeout).await)
        }
        "select" => {
            let selector = require_str_param(args, "selector")?;
            let value = require_str_param(args, "value")?;
            browser_action_with_recovery!(
                browser,
                "select",
                browser.select(selector, value, timeout).await
            )
        }
        "press" => {
            let key = require_str_param(args, "key")?;
            browser_action_with_recovery!(browser, "press", browser.press(key, timeout).await)
        }
        "drag" => {
            let source = require_str_param(args, "source")?;
            let target = require_str_param(args, "target")?;
            browser_action_with_recovery!(
                browser,
                "drag",
                browser.drag(source, target, timeout).await
            )
        }
        "fill" => {
            let fields = args
                .get("fields")
                .and_then(|v| v.as_object())
                .context("fill requires 'fields' parameter (object)")?
                .clone();
            browser_action_with_recovery!(browser, "fill", browser.fill(&fields, timeout).await)
        }
        "wait" => {
            browser_action_with_recovery!(
                browser,
                "wait",
                browser
                    .wait_for(args, timeout, &config.security.url_policy)
                    .await
            )
        }
        "resize" => {
            let width = args
                .get("width")
                .and_then(serde_json::Value::as_u64)
                .context("resize requires 'width'")? as u32;
            let height = args
                .get("height")
                .and_then(serde_json::Value::as_u64)
                .context("resize requires 'height'")? as u32;
            browser_action_with_recovery!(
                browser,
                "resize",
                browser.resize(width, height, timeout).await
            )
        }
        "new_tab" => {
            let url = args.get("url").and_then(serde_json::Value::as_str);
            browser_result(browser.new_tab(url, &config.security.url_policy).await)
        }
        "list_tabs" => browser_result(browser.list_tabs().await),
        "switch_tab" => {
            let index = args
                .get("tab_index")
                .and_then(serde_json::Value::as_u64)
                .context("switch_tab requires 'tab_index'")? as usize;
            browser_result(browser.switch_tab(index))
        }
        "close_tab" => browser_result(browser.close_tab().await),
        "get_console_logs" => Ok(ToolOutput::Text(browser.get_console_logs())),
        "back" => browser_result(browser.back(timeout).await),
        "forward" => browser_result(browser.forward(timeout).await),
        "scroll" => {
            let x = args.get("x").and_then(serde_json::Value::as_i64);
            let y = args.get("y").and_then(serde_json::Value::as_i64);
            let dx = args
                .get("delta_x")
                .and_then(serde_json::Value::as_i64)
                .unwrap_or(0);
            let dy = args
                .get("delta_y")
                .and_then(serde_json::Value::as_i64)
                .unwrap_or(0);
            if dx == 0 && dy == 0 {
                return Ok(ToolOutput::Text(
                    "Error: scroll requires non-zero 'delta_x' or 'delta_y'".to_string(),
                ));
            }
            browser_action_with_recovery!(
                browser,
                "scroll",
                browser.scroll(x, y, dx, dy, timeout).await
            )
        }
        "snapshot" => {
            let cap = args
                .get("max_elements")
                .and_then(serde_json::Value::as_u64)
                .map(|n| n as usize);
            browser_result(
                browser
                    .snapshot(cap, timeout, &config.security.url_policy)
                    .await,
            )
        }
        "get_images" => {
            browser_action_with_recovery!(
                browser,
                "get_images",
                browser
                    .get_images(timeout, &config.security.url_policy)
                    .await
            )
        }
        "upload" => {
            let selector = require_str_param(args, "selector")?;
            let files: Vec<String> = args
                .get("files")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(std::string::ToString::to_string))
                        .collect()
                })
                .unwrap_or_default();
            if files.is_empty() {
                return Ok(ToolOutput::Text(
                    "Error: upload requires non-empty 'files' (string paths)".to_string(),
                ));
            }
            browser_action_with_recovery!(
                browser,
                "upload",
                browser.upload(selector, &files, timeout).await
            )
        }
        _ => Ok(ToolOutput::Text(format!(
            "Unknown browser action: {action}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn handle_generate_image_disabled() {
        let mut config = Config::default();
        config.image_gen.enabled = false;
        let result = handle_generate_image(&json!({"prompt": "a cat"}), &config)
            .await
            .unwrap();
        assert!(
            result.contains("disabled"),
            "expected 'disabled' in: {result}"
        );
    }

    #[tokio::test]
    async fn handle_generate_image_empty_prompt() {
        let mut config = Config::default();
        config.image_gen.enabled = true;
        let result = handle_generate_image(&json!({"prompt": ""}), &config)
            .await
            .unwrap();
        assert!(
            result.contains("required"),
            "expected 'required' in: {result}"
        );
    }

    #[tokio::test]
    async fn handle_generate_image_no_provider_configured() {
        // image_gen enabled but no provider env vars / config → reports missing provider.
        let mut config = Config::default();
        config.image_gen.enabled = true;
        let result = handle_generate_image(&json!({"prompt": "a cat"}), &config)
            .await
            .unwrap();
        // Either a provider was found in the env (rare in test) or we get the
        // "No image generation provider available" message. Only assert the
        // disabled-provider branch when no provider is configured.
        if crate::media::image_gen::ImageGenProvider::from_config(&config.image_gen).is_none() {
            assert!(
                result.contains("No image generation provider"),
                "got: {result}"
            );
        }
    }

    #[tokio::test]
    async fn handle_browser_disabled_short_circuits() {
        let mut config = Config::default();
        config.browser.enabled = false;
        let mut session: Option<BrowserSession> = None;
        let out = handle_browser(
            &json!({"action": "navigate", "url": "x"}),
            &config,
            &mut session,
        )
        .await
        .expect("ok");
        if let ToolOutput::Text(msg) = out {
            assert!(msg.contains("disabled"), "got: {msg}");
        } else {
            panic!("expected text output when browser disabled");
        }
    }

    #[tokio::test]
    async fn handle_browser_close_without_session() {
        let mut config = Config::default();
        config.browser.enabled = true;
        let mut session: Option<BrowserSession> = None;
        let out = handle_browser(&json!({"action": "close"}), &config, &mut session)
            .await
            .expect("ok");
        if let ToolOutput::Text(msg) = out {
            assert!(msg.contains("No browser session"), "got: {msg}");
        } else {
            panic!("expected text output for close-without-session");
        }
    }

    /// Scroll without a delta is a no-op the model wouldn't notice. Reject
    /// pre-launch so a confused agent can't spin its wheels (literally)
    /// without ever leaving the page.
    #[tokio::test]
    async fn handle_browser_scroll_requires_delta() {
        let mut config = Config::default();
        config.browser.enabled = true;
        let mut session: Option<BrowserSession> = None;
        let out = handle_browser(&json!({"action": "scroll"}), &config, &mut session)
            .await
            .expect("ok");
        if let ToolOutput::Text(msg) = out {
            assert!(
                msg.contains("Error"),
                "expected validation error, got: {msg}"
            );
            assert!(msg.contains("delta"), "missing delta hint: {msg}");
        } else {
            panic!("expected text output");
        }
    }

    /// Upload without files is a silent no-op at the CDP layer — surface
    /// it as an explicit validation error before Chrome is launched.
    #[tokio::test]
    async fn handle_browser_upload_requires_non_empty_files() {
        let mut config = Config::default();
        config.browser.enabled = true;
        let mut session: Option<BrowserSession> = None;
        let out = handle_browser(
            &json!({"action": "upload", "selector": "input[type=file]", "files": []}),
            &config,
            &mut session,
        )
        .await
        .expect("ok");
        if let ToolOutput::Text(msg) = out {
            assert!(msg.contains("Error"));
            assert!(msg.contains("files"));
        } else {
            panic!("expected text output");
        }
    }

    /// Snapshot with `max_elements: 0` is a degenerate request (returns
    /// nothing useful). Bail at validation rather than letting the JS walk
    /// run for zero results and the model be confused by the empty list.
    #[tokio::test]
    async fn handle_browser_snapshot_rejects_zero_max_elements() {
        let mut config = Config::default();
        config.browser.enabled = true;
        let mut session: Option<BrowserSession> = None;
        let out = handle_browser(
            &json!({"action": "snapshot", "max_elements": 0}),
            &config,
            &mut session,
        )
        .await
        .expect("ok");
        if let ToolOutput::Text(msg) = out {
            assert!(msg.contains("positive"), "expected validation msg: {msg}");
        } else {
            panic!("expected text output");
        }
    }

    /// click + both `selector` and `ref` is an ambiguous target — fail
    /// before launching Chrome, same reason ambiguous element_index on
    /// computer_use fails.
    #[tokio::test]
    async fn handle_browser_click_rejects_selector_and_ref_together() {
        let mut config = Config::default();
        config.browser.enabled = true;
        let mut session: Option<BrowserSession> = None;
        let out = handle_browser(
            &json!({"action": "click", "selector": "#a", "ref": "@e0"}),
            &config,
            &mut session,
        )
        .await
        .expect("ok");
        if let ToolOutput::Text(msg) = out {
            assert!(msg.contains("not both"), "got: {msg}");
        } else {
            panic!("expected text output");
        }
    }

    // ── text_to_speech handler ─────────────────────────────────────────

    /// Spec: the handler must surface "Error: ..." when the LLM omits the
    /// required `text` arg. Missing text used to fall through to an empty
    /// synthesize() call that returned a generic provider error — confusing
    /// because the actionable fix is on the caller's side, not the provider.
    #[tokio::test]
    async fn handle_text_to_speech_missing_text_returns_error() {
        let synth = crate::media::tts::TtsSynthesizer::empty_for_test();
        let out = handle_text_to_speech(&json!({}), &synth).await;
        match out {
            ToolOutput::Text(msg) => {
                assert!(msg.starts_with("Error:"), "expected error prefix: {msg}");
                assert!(
                    msg.contains("text"),
                    "error should name missing field: {msg}"
                );
            }
            other => panic!("expected Text output, got: {other:?}"),
        }
    }

    /// Spec: when every provider in the fallback chain fails (here, the
    /// chain is empty) the handler must produce an error string the LLM
    /// can act on — NOT a Multimodal audio part with zero bytes. The empty
    /// synthesizer is a deterministic stand-in for "all providers down".
    #[tokio::test]
    async fn handle_text_to_speech_all_providers_failed_returns_error() {
        let synth = crate::media::tts::TtsSynthesizer::empty_for_test();
        let out = handle_text_to_speech(&json!({"text": "hello"}), &synth).await;
        match out {
            ToolOutput::Text(msg) => {
                assert!(
                    msg.contains("TTS error"),
                    "expected TTS error prefix: {msg}"
                );
            }
            ToolOutput::Multimodal { .. } => {
                panic!("expected Text error output, got multimodal — handler returned audio for a failed synthesis")
            }
        }
    }

    /// Spec: when synthesis succeeds, the handler must return a
    /// `Multimodal` output carrying (a) a one-line text summary that
    /// includes the byte length and MIME, and (b) an `AudioBase64`
    /// content part with the provider's audio bytes base64-encoded and
    /// the correct MIME label. Until this test existed, every handler
    /// test asserted on an error branch — a regression that returned
    /// e.g. `ToolOutput::Text` on success would have shipped.
    #[tokio::test]
    async fn handle_text_to_speech_returns_multimodal_audio_on_success() {
        use wiremock::matchers::{method, path as wm_path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let audio_payload = b"\xff\xfb\x90\x00mock-handler-mp3".to_vec();
        Mock::given(method("POST"))
            .and(wm_path("/v1/audio/speech"))
            .respond_with(
                ResponseTemplate::new(200).set_body_raw(audio_payload.clone(), "audio/mpeg"),
            )
            .expect(1)
            .mount(&server)
            .await;

        let synth = crate::media::tts::TtsSynthesizer::single_openai_for_test(
            server.uri(),
            "sk-handler-test".to_string(),
        );
        let out = handle_text_to_speech(&json!({"text": "hello world"}), &synth).await;

        match out {
            ToolOutput::Multimodal { text, parts } => {
                assert!(text.contains("mp3"), "summary missing extension: {text}");
                assert!(
                    text.contains(&audio_payload.len().to_string()),
                    "summary missing byte count: {text}"
                );
                assert!(text.contains("audio/mpeg"), "summary missing MIME: {text}");
                assert_eq!(parts.len(), 1, "expected exactly one audio part");
                match &parts[0] {
                    ContentPart::AudioBase64 { media } => {
                        assert_eq!(media.mime_type, "audio/mpeg");
                        assert_eq!(
                            media.filename.as_deref(),
                            Some("speech.mp3"),
                            "filename must reflect chosen format"
                        );
                        let decoded = base64::engine::general_purpose::STANDARD
                            .decode(&media.data)
                            .expect("audio data must be valid base64");
                        assert_eq!(
                            decoded, audio_payload,
                            "decoded audio must match provider bytes"
                        );
                    }
                    other => panic!("expected AudioBase64 part, got: {other:?}"),
                }
            }
            ToolOutput::Text(msg) => {
                panic!("expected Multimodal on success, got Text: {msg}");
            }
        }
    }

    /// Spec: oversize input must be rejected at the synthesizer (4096-char
    /// cap), and the handler must convert that to a Text error rather than
    /// a half-formed audio response. Without this guard a model could
    /// stream a multi-megabyte completion into the TTS provider and rack
    /// up the operator's bill.
    #[tokio::test]
    async fn handle_text_to_speech_oversized_text_returns_error() {
        let synth = crate::media::tts::TtsSynthesizer::empty_for_test();
        let huge = "a".repeat(10_000);
        let out = handle_text_to_speech(&json!({"text": huge}), &synth).await;
        match out {
            ToolOutput::Text(msg) => {
                assert!(
                    msg.contains("TTS error") && msg.to_lowercase().contains("too long"),
                    "expected length-related TTS error, got: {msg}"
                );
            }
            other => panic!("expected Text output, got: {other:?}"),
        }
    }

    /// Spec: the `format` arg, when present, must be parsed via
    /// `AudioFormat::from_str_lossy`. An unknown value silently falls back
    /// to the synthesizer's default (Mp3); a known value flows through.
    /// We can't test the success path without a live provider, but we can
    /// check the parser the handler depends on hasn't drifted.
    #[test]
    fn audio_format_lossy_parser_covers_handler_inputs() {
        use crate::media::tts::AudioFormat;
        assert_eq!(AudioFormat::from_str_lossy("mp3"), Some(AudioFormat::Mp3));
        assert_eq!(AudioFormat::from_str_lossy("opus"), Some(AudioFormat::Opus));
        assert_eq!(AudioFormat::from_str_lossy("wav"), Some(AudioFormat::Wav));
        assert_eq!(AudioFormat::from_str_lossy("flac"), Some(AudioFormat::Flac));
        assert_eq!(AudioFormat::from_str_lossy("aac"), Some(AudioFormat::Aac));
        // Unknown formats must produce None so the handler's `.and_then`
        // chain falls back to the synthesizer default.
        assert_eq!(AudioFormat::from_str_lossy("xyz"), None);
        assert_eq!(AudioFormat::from_str_lossy(""), None);
    }

    #[tokio::test]
    async fn handle_browser_remote_backend_without_credentials_surfaces_hint() {
        // End-to-end routing smoke: with a cloud backend selected but no API
        // key configured, the lazy-launch path must reach the remote-session
        // resolver, fail with a precise error, and surface an actionable hint
        // that names the missing setting — never silently fall back to a local
        // launch or panic. Uses `snapshot` (no required args, no URL preflight)
        // so we never touch the network or spawn Chrome.
        let mut config = Config::default();
        config.browser.enabled = true;
        config.browser.backend = crate::config::BrowserBackend::Browserbase;
        // No browserbase_api_key set.
        let mut session: Option<BrowserSession> = None;
        let out = handle_browser(&json!({"action": "snapshot"}), &config, &mut session)
            .await
            .expect("handler returns Ok with an error message, not Err");
        let ToolOutput::Text(msg) = out else {
            panic!("expected text output");
        };
        assert!(
            msg.contains("browser.browserbase_api_key"),
            "hint must name the missing credential setting, got: {msg}"
        );
        assert!(
            session.is_none(),
            "no session should be established when the remote backend fails"
        );
    }

    #[tokio::test]
    async fn handle_browser_cdp_attach_without_url_errors_not_local_launch() {
        // backend=cdp_attach with no cdp_url must surface a misconfig error, not
        // silently launch a fresh local Chrome (which would ignore the operator's
        // intent to attach to their running browser). `snapshot` avoids the URL
        // preflight so we exercise the launch-routing branch directly.
        let mut config = Config::default();
        config.browser.enabled = true;
        config.browser.backend = crate::config::BrowserBackend::CdpAttach;
        // No browser.cdp_url set.
        let mut session: Option<BrowserSession> = None;
        let out = handle_browser(&json!({"action": "snapshot"}), &config, &mut session)
            .await
            .expect("handler returns Ok with an error message, not Err");
        let ToolOutput::Text(msg) = out else {
            panic!("expected text output");
        };
        assert!(
            msg.contains("cdp_attach") && msg.contains("cdp_url"),
            "error must explain the missing cdp_url for cdp_attach, got: {msg}"
        );
        assert!(
            session.is_none(),
            "no local session should be launched for a misconfigured cdp_attach"
        );
    }

    #[tokio::test]
    async fn handle_browser_invalid_action_args_returns_error() {
        // "navigate" without a "url" argument fails validate_browser_args early
        // (before any browser launch), so we can exercise the validation branch
        // without actually launching Chrome.
        let mut config = Config::default();
        config.browser.enabled = true;
        let mut session: Option<BrowserSession> = None;
        let out = handle_browser(&json!({"action": "navigate"}), &config, &mut session)
            .await
            .expect("ok");
        if let ToolOutput::Text(msg) = out {
            assert!(msg.contains("Error"), "got: {msg}");
        } else {
            panic!("expected text output on validation error");
        }
    }
}
