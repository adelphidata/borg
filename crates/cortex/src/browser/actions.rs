use std::time::Duration;

use anyhow::{Context, Result};
use chromiumoxide::Page;
use tracing::instrument;

/// Hover over an element by CSS selector.
#[instrument(skip_all, fields(browser.action = "hover"))]
pub async fn hover(page: &Page, selector: &str, timeout: Duration) -> Result<String> {
    tokio::time::timeout(timeout, async {
        let el = page
            .find_element(selector)
            .await
            .with_context(|| format!("Element not found: {selector}"))?;
        el.hover().await.context("Hover failed")?;
        Ok(format!("Hovered over element: {selector}"))
    })
    .await
    .context("Hover timed out")?
}

/// Select a value in a dropdown/select element via JS.
#[instrument(skip_all, fields(browser.action = "select"))]
pub async fn select(page: &Page, selector: &str, value: &str, timeout: Duration) -> Result<String> {
    let escaped_sel = selector.replace('\\', "\\\\").replace('\'', "\\'");
    let escaped_val = value.replace('\\', "\\\\").replace('\'', "\\'");
    let js = format!(
        r#"(() => {{
            const el = document.querySelector('{escaped_sel}');
            if (!el) throw new Error('Element not found: {escaped_sel}');
            el.value = '{escaped_val}';
            el.dispatchEvent(new Event('change', {{ bubbles: true }}));
            el.dispatchEvent(new Event('input', {{ bubbles: true }}));
            return el.value;
        }})()"#
    );

    tokio::time::timeout(timeout, async {
        let result: serde_json::Value = page
            .evaluate(js.as_str())
            .await
            .context("Select evaluation failed")?
            .into_value()
            .context("Failed to deserialize select result")?;
        Ok(format!(
            "Selected value '{value}' in {selector} (set to: {result})"
        ))
    })
    .await
    .context("Select timed out")?
}

/// Press a keyboard key using CDP Input domain.
#[instrument(skip_all, fields(browser.action = "press"))]
pub async fn press(page: &Page, key: &str, timeout: Duration) -> Result<String> {
    use chromiumoxide::cdp::browser_protocol::input::{
        DispatchKeyEventParams, DispatchKeyEventType,
    };

    tokio::time::timeout(timeout, async {
        // KeyDown
        let key_down = DispatchKeyEventParams::builder()
            .r#type(DispatchKeyEventType::KeyDown)
            .key(key)
            .build()
            .map_err(|e| anyhow::anyhow!("Failed to build key down event: {e}"))?;
        page.execute(key_down)
            .await
            .context("Key down dispatch failed")?;

        // KeyUp
        let key_up = DispatchKeyEventParams::builder()
            .r#type(DispatchKeyEventType::KeyUp)
            .key(key)
            .build()
            .map_err(|e| anyhow::anyhow!("Failed to build key up event: {e}"))?;
        page.execute(key_up)
            .await
            .context("Key up dispatch failed")?;

        Ok(format!("Pressed key: {key}"))
    })
    .await
    .context("Press timed out")?
}

/// Drag from one element to another using synthesized mouse events.
#[instrument(skip_all, fields(browser.action = "drag"))]
pub async fn drag(
    page: &Page,
    source_selector: &str,
    target_selector: &str,
    timeout: Duration,
) -> Result<String> {
    let esc_src = source_selector.replace('\\', "\\\\").replace('\'', "\\'");
    let esc_tgt = target_selector.replace('\\', "\\\\").replace('\'', "\\'");

    // Get bounding box centers for both elements
    let js = format!(
        r#"(() => {{
            const src = document.querySelector('{esc_src}');
            const tgt = document.querySelector('{esc_tgt}');
            if (!src) throw new Error('Source element not found: {esc_src}');
            if (!tgt) throw new Error('Target element not found: {esc_tgt}');
            const sr = src.getBoundingClientRect();
            const tr = tgt.getBoundingClientRect();
            return {{
                sx: sr.x + sr.width / 2,
                sy: sr.y + sr.height / 2,
                tx: tr.x + tr.width / 2,
                ty: tr.y + tr.height / 2
            }};
        }})()"#
    );

    tokio::time::timeout(timeout, async {
        let coords: serde_json::Value = page
            .evaluate(js.as_str())
            .await
            .context("Failed to get element positions")?
            .into_value()
            .context("Failed to deserialize coordinates")?;

        let sx = coords["sx"].as_f64().context("Missing sx")? as f64;
        let sy = coords["sy"].as_f64().context("Missing sy")? as f64;
        let tx = coords["tx"].as_f64().context("Missing tx")? as f64;
        let ty = coords["ty"].as_f64().context("Missing ty")? as f64;

        use chromiumoxide::cdp::browser_protocol::input::{
            DispatchMouseEventParams, DispatchMouseEventType, MouseButton,
        };

        // Mouse down at source
        let mouse_down = DispatchMouseEventParams::builder()
            .r#type(DispatchMouseEventType::MousePressed)
            .x(sx)
            .y(sy)
            .button(MouseButton::Left)
            .click_count(1)
            .build()
            .map_err(|e| anyhow::anyhow!("Failed to build mouse down: {e}"))?;
        page.execute(mouse_down)
            .await
            .context("Mouse down failed")?;

        // Mouse move to target
        let mouse_move = DispatchMouseEventParams::builder()
            .r#type(DispatchMouseEventType::MouseMoved)
            .x(tx)
            .y(ty)
            .button(MouseButton::Left)
            .build()
            .map_err(|e| anyhow::anyhow!("Failed to build mouse move: {e}"))?;
        page.execute(mouse_move)
            .await
            .context("Mouse move failed")?;

        // Mouse up at target
        let mouse_up = DispatchMouseEventParams::builder()
            .r#type(DispatchMouseEventType::MouseReleased)
            .x(tx)
            .y(ty)
            .button(MouseButton::Left)
            .click_count(1)
            .build()
            .map_err(|e| anyhow::anyhow!("Failed to build mouse up: {e}"))?;
        page.execute(mouse_up).await.context("Mouse up failed")?;

        Ok(format!(
            "Dragged from {source_selector} to {target_selector}"
        ))
    })
    .await
    .context("Drag timed out")?
}

/// Fill multiple form fields. `fields` maps CSS selectors to values.
#[instrument(skip_all, fields(browser.action = "fill"))]
pub async fn fill(
    page: &Page,
    fields: &serde_json::Map<String, serde_json::Value>,
    timeout: Duration,
) -> Result<String> {
    tokio::time::timeout(timeout, async {
        let mut filled = Vec::new();
        for (selector, value) in fields {
            let text_owned;
            let text = match value.as_str() {
                Some(s) => s,
                None => {
                    text_owned = value.to_string();
                    &text_owned
                }
            };
            let el = page
                .find_element(selector)
                .await
                .with_context(|| format!("Element not found: {selector}"))?;
            el.click().await.context("Focus click failed")?;

            // Clear existing value first
            let esc_sel = selector.replace('\\', "\\\\").replace('\'', "\\'");
            let clear_js = format!("document.querySelector('{esc_sel}').value = ''");
            page.evaluate(clear_js.as_str()).await.ok();

            el.type_str(text)
                .await
                .with_context(|| format!("Failed to type into {selector}"))?;
            filled.push(selector.as_str());
        }
        Ok(format!(
            "Filled {} fields: {}",
            filled.len(),
            filled.join(", ")
        ))
    })
    .await
    .context("Fill timed out")?
}

#[cfg(test)]
mod tests {
    // Actions require a live browser, so integration tests are #[ignore].
    // Validation of action params is tested in validate.rs.

    #[test]
    fn escape_selector_single_quotes() {
        let sel = "input[name='email']";
        let escaped = sel.replace('\\', "\\\\").replace('\'', "\\'");
        assert_eq!(escaped, "input[name=\\'email\\']");
    }

    #[test]
    fn escape_selector_backslash() {
        let sel = r"div\.class";
        let escaped = sel.replace('\\', "\\\\").replace('\'', "\\'");
        assert!(escaped.contains("\\\\"));
    }
}
