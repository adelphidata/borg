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

    let provider = match crate::image_gen::ImageGenProvider::from_config(&config.image_gen) {
        Some(p) => p,
        None => {
            return Ok(
                "No image generation provider available. Set OPENAI_API_KEY or FAL_KEY environment variable, or configure [image_gen] in config.toml"
                    .to_string(),
            );
        }
    };

    match crate::image_gen::generate_image(&provider, prompt, size, count).await {
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
    synthesizer: &crate::tts::TtsSynthesizer,
) -> ToolOutput {
    let text = match require_str_param(args, "text") {
        Ok(t) => t,
        Err(e) => return ToolOutput::Text(format!("Error: {e}")),
    };
    let voice = args.get("voice").and_then(|v| v.as_str());
    let format = args
        .get("format")
        .and_then(|v| v.as_str())
        .and_then(crate::tts::AudioFormat::from_str_lossy);

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

    // Lazy-launch browser session
    if session.is_none() {
        match BrowserSession::launch(&config.browser).await {
            Ok(s) => *session = Some(s),
            Err(e) => return Ok(ToolOutput::Text(format!("Error launching browser: {e}"))),
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
            browser_action_with_recovery!(browser, "navigate", browser.navigate(url, timeout).await)
        }
        "click" => {
            let selector = require_str_param(args, "selector")?;
            browser_action_with_recovery!(browser, "click", browser.click(selector, timeout).await)
        }
        "type" => {
            let selector = require_str_param(args, "selector")?;
            let text = require_str_param(args, "text")?;
            browser_action_with_recovery!(
                browser,
                "type",
                browser.type_text(selector, text, timeout).await
            )
        }
        "screenshot" => {
            let selector = args.get("selector").and_then(|v| v.as_str());
            match browser.screenshot(selector, timeout).await {
                Ok((desc, png_bytes)) => {
                    // Save to disk
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
                browser.get_text(selector, timeout).await
            )
        }
        "evaluate_js" => {
            let expression = require_str_param(args, "expression")?;
            browser_action_with_recovery!(
                browser,
                "evaluate_js",
                browser.evaluate_js(expression, timeout).await
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
            browser_action_with_recovery!(browser, "wait", browser.wait_for(args, timeout).await)
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
            browser_result(browser.new_tab(url).await)
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
}
