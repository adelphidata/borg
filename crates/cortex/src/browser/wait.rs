use std::time::Duration;

use anyhow::{Context, Result};
use chromiumoxide::Page;
use tracing::instrument;

/// Supported wait conditions for the `wait` browser action.
#[derive(Debug, Clone, PartialEq)]
pub enum WaitCondition {
    /// Wait until the given text appears anywhere on the page.
    Text(String),
    /// Wait until a CSS selector matches at least one element.
    Element(String),
    /// Wait until the page URL contains the given substring.
    Url(String),
    /// Wait until `document.readyState === "complete"`.
    LoadState,
    /// Wait until a JS expression returns a truthy value.
    JsFunction(String),
}

impl WaitCondition {
    /// Parse a `WaitCondition` from browser action arguments.
    pub fn from_args(args: &serde_json::Value) -> Result<Self> {
        let condition = args
            .get("condition")
            .and_then(|v| v.as_str())
            .context("wait requires 'condition' parameter")?;

        let value = args.get("value").and_then(|v| v.as_str());

        match condition {
            "text" => Ok(Self::Text(
                value
                    .context("wait with condition 'text' requires 'value'")?
                    .to_string(),
            )),
            "element" => Ok(Self::Element(
                value
                    .context("wait with condition 'element' requires 'value'")?
                    .to_string(),
            )),
            "url" => Ok(Self::Url(
                value
                    .context("wait with condition 'url' requires 'value'")?
                    .to_string(),
            )),
            "load" => Ok(Self::LoadState),
            "js" => Ok(Self::JsFunction(
                value
                    .context("wait with condition 'js' requires 'value'")?
                    .to_string(),
            )),
            other => {
                anyhow::bail!("Unknown wait condition: {other}. Use: text, element, url, load, js")
            }
        }
    }

    /// Generate the JS expression that returns `true` when the condition is met.
    fn to_js_check(&self) -> String {
        match self {
            Self::Text(text) => {
                let escaped = text.replace('\\', "\\\\").replace('\'', "\\'");
                format!("document.body && document.body.innerText.includes('{escaped}')")
            }
            Self::Element(selector) => {
                let escaped = selector.replace('\\', "\\\\").replace('\'', "\\'");
                format!("!!document.querySelector('{escaped}')")
            }
            Self::Url(substring) => {
                let escaped = substring.replace('\\', "\\\\").replace('\'', "\\'");
                format!("window.location.href.includes('{escaped}')")
            }
            Self::LoadState => "document.readyState === 'complete'".to_string(),
            Self::JsFunction(expr) => format!("!!({expr})"),
        }
    }

    /// Human-readable description of what we're waiting for.
    fn description(&self) -> String {
        match self {
            Self::Text(t) => format!("text '{t}' to appear"),
            Self::Element(s) => format!("element '{s}' to exist"),
            Self::Url(u) => format!("URL to contain '{u}'"),
            Self::LoadState => "page to fully load".to_string(),
            Self::JsFunction(e) => format!("JS expression to be truthy: {e}"),
        }
    }
}

const POLL_INTERVAL_MS: u64 = 100;

/// Poll the page until `condition` is met or `timeout` expires.
#[instrument(skip_all, fields(browser.action = "wait_for"))]
pub async fn wait_for(page: &Page, condition: &WaitCondition, timeout: Duration) -> Result<String> {
    let js_check = condition.to_js_check();
    let desc = condition.description();

    tokio::time::timeout(timeout, async {
        loop {
            let result: bool = page
                .evaluate(js_check.as_str())
                .await
                .ok()
                .and_then(|r| r.into_value().ok())
                .unwrap_or(false);

            if result {
                return Ok(format!("Wait completed: {desc}"));
            }

            tokio::time::sleep(Duration::from_millis(POLL_INTERVAL_MS)).await;
        }
    })
    .await
    .map_err(|_| anyhow::anyhow!("Wait timed out after {timeout:?}: {desc}"))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_text_condition() {
        let args = json!({"condition": "text", "value": "hello"});
        let cond = WaitCondition::from_args(&args).unwrap();
        assert_eq!(cond, WaitCondition::Text("hello".to_string()));
    }

    #[test]
    fn parse_element_condition() {
        let args = json!({"condition": "element", "value": "#btn"});
        let cond = WaitCondition::from_args(&args).unwrap();
        assert_eq!(cond, WaitCondition::Element("#btn".to_string()));
    }

    #[test]
    fn parse_url_condition() {
        let args = json!({"condition": "url", "value": "/dashboard"});
        let cond = WaitCondition::from_args(&args).unwrap();
        assert_eq!(cond, WaitCondition::Url("/dashboard".to_string()));
    }

    #[test]
    fn parse_load_condition() {
        let args = json!({"condition": "load"});
        let cond = WaitCondition::from_args(&args).unwrap();
        assert_eq!(cond, WaitCondition::LoadState);
    }

    #[test]
    fn parse_js_condition() {
        let args = json!({"condition": "js", "value": "window.ready"});
        let cond = WaitCondition::from_args(&args).unwrap();
        assert_eq!(cond, WaitCondition::JsFunction("window.ready".to_string()));
    }

    #[test]
    fn parse_missing_condition() {
        let args = json!({});
        assert!(WaitCondition::from_args(&args).is_err());
    }

    #[test]
    fn parse_unknown_condition() {
        let args = json!({"condition": "magic"});
        assert!(WaitCondition::from_args(&args).is_err());
    }

    #[test]
    fn parse_text_missing_value() {
        let args = json!({"condition": "text"});
        assert!(WaitCondition::from_args(&args).is_err());
    }

    #[test]
    fn parse_element_missing_value() {
        let args = json!({"condition": "element"});
        assert!(WaitCondition::from_args(&args).is_err());
    }

    #[test]
    fn js_check_text() {
        let cond = WaitCondition::Text("hello".to_string());
        let js = cond.to_js_check();
        assert!(js.contains("innerText.includes('hello')"));
    }

    #[test]
    fn js_check_element() {
        let cond = WaitCondition::Element("#btn".to_string());
        let js = cond.to_js_check();
        assert!(js.contains("querySelector('#btn')"));
    }

    #[test]
    fn js_check_url() {
        let cond = WaitCondition::Url("/page".to_string());
        let js = cond.to_js_check();
        assert!(js.contains("location.href.includes('/page')"));
    }

    #[test]
    fn js_check_load_state() {
        let cond = WaitCondition::LoadState;
        let js = cond.to_js_check();
        assert!(js.contains("readyState === 'complete'"));
    }

    #[test]
    fn js_check_js_function() {
        let cond = WaitCondition::JsFunction("window.ready".to_string());
        let js = cond.to_js_check();
        assert!(js.contains("!!(window.ready)"));
    }

    #[test]
    fn js_check_escapes_quotes() {
        let cond = WaitCondition::Text("it's a test".to_string());
        let js = cond.to_js_check();
        assert!(js.contains("it\\'s a test"));
    }

    #[test]
    fn description_messages() {
        assert!(WaitCondition::Text("x".into())
            .description()
            .contains("text 'x'"));
        assert!(WaitCondition::Element("#e".into())
            .description()
            .contains("element '#e'"));
        assert!(WaitCondition::Url("/u".into())
            .description()
            .contains("URL"));
        assert!(WaitCondition::LoadState.description().contains("load"));
        assert!(WaitCondition::JsFunction("f()".into())
            .description()
            .contains("JS expression"));
    }
}
