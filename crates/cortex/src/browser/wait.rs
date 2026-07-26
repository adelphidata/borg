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

    /// The raw JS expression for a `js` condition. This runs as arbitrary code
    /// in the poll loop (`!!({expr})`), the same capability as `evaluate_js`, so
    /// the caller must give it the same URL pre-scan. The other conditions embed
    /// only escaped string literals and cannot issue requests, so they return
    /// `None`.
    pub fn js_expression(&self) -> Option<&str> {
        match self {
            Self::JsFunction(expr) => Some(expr),
            _ => None,
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

// 100ms keeps a CDP round-trip per poll well under the interval while still
// resolving sub-frame conditions faster than a human notices.
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

    /// Every supported `condition` keyword maps to the right
    /// `WaitCondition` variant. `from_args` is the tool-call entry point —
    /// each row corresponds to a documented keyword.
    #[test]
    fn from_args_maps_each_keyword_to_its_variant() {
        let cases: &[(serde_json::Value, WaitCondition)] = &[
            (
                json!({"condition": "text", "value": "hello"}),
                WaitCondition::Text("hello".to_string()),
            ),
            (
                json!({"condition": "element", "value": "#btn"}),
                WaitCondition::Element("#btn".to_string()),
            ),
            (
                json!({"condition": "url", "value": "/dashboard"}),
                WaitCondition::Url("/dashboard".to_string()),
            ),
            (json!({"condition": "load"}), WaitCondition::LoadState),
            (
                json!({"condition": "js", "value": "window.ready"}),
                WaitCondition::JsFunction("window.ready".to_string()),
            ),
        ];
        for (args, expected) in cases {
            let got = WaitCondition::from_args(args).expect("parse ok");
            assert_eq!(&got, expected, "from_args({args}) = {got:?}");
        }
    }

    /// Malformed payloads must return Err, not silently pick a default —
    /// the LLM gets the error back as a tool-call failure and can retry.
    #[test]
    fn from_args_rejects_malformed_payloads() {
        let cases: &[serde_json::Value] = &[
            json!({}),                     // missing condition entirely
            json!({"condition": "magic"}), // unknown keyword
            // conditions that REQUIRE a value but didn't supply one
            json!({"condition": "text"}),
            json!({"condition": "element"}),
            json!({"condition": "url"}),
            json!({"condition": "js"}),
        ];
        for args in cases {
            assert!(
                WaitCondition::from_args(args).is_err(),
                "from_args({args}) must be Err"
            );
        }
    }

    /// Each variant's `to_js_check()` must emit a JS expression containing
    /// the discriminating substring the browser actually evaluates.
    /// Substring (not full equality) so accidental whitespace tweaks don't
    /// regress the test for the wrong reason.
    #[test]
    fn to_js_check_emits_variant_specific_expression() {
        let cases: &[(WaitCondition, &str)] = &[
            (
                WaitCondition::Text("hello".into()),
                "innerText.includes('hello')",
            ),
            (
                WaitCondition::Element("#btn".into()),
                "querySelector('#btn')",
            ),
            (
                WaitCondition::Url("/page".into()),
                "location.href.includes('/page')",
            ),
            (WaitCondition::LoadState, "readyState === 'complete'"),
            (
                WaitCondition::JsFunction("window.ready".into()),
                "!!(window.ready)",
            ),
        ];
        for (cond, needle) in cases {
            let js = cond.to_js_check();
            assert!(
                js.contains(needle),
                "to_js_check for {cond:?} missing {needle:?}; got: {js}"
            );
        }
    }

    /// Single-quote escaping is the JS-injection equivalent for the
    /// generated check expression. A user-supplied value containing `'`
    /// would otherwise break out of the surrounding `'...'` literal and
    /// run arbitrary JS in the page's main world.
    #[test]
    fn to_js_check_escapes_single_quotes_in_value() {
        let cond = WaitCondition::Text("it's a test".to_string());
        let js = cond.to_js_check();
        assert!(
            js.contains("it\\'s a test"),
            "single quote not escaped; got: {js}"
        );
        // Sanity: the unescaped form must not appear — would mean both the
        // escaped and the raw value leaked.
        assert!(
            !js.contains("'it's a test'"),
            "unescaped single quote leaked into JS; got: {js}"
        );
    }
}
