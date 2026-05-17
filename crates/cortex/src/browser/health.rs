use anyhow::Result;
use tracing::instrument;

/// Known error message patterns that indicate a recoverable browser issue
/// (stale target, disconnected page, crashed tab) vs a logic error.
const RECOVERABLE_PATTERNS: &[&str] = &[
    "target closed",
    "frame detached",
    "frame has been detached",
    "session closed",
    "page crashed",
    "connection reset",
    "connection refused",
    "target page, context or browser has been closed",
    "websocket error",
    "browser disconnected",
    "no such target",
];

/// Classify whether an error is recoverable (browser-level issue that
/// can be fixed by reconnecting) vs non-recoverable (logic/usage error).
pub fn is_recoverable_error(err: &anyhow::Error) -> bool {
    let msg = format!("{err:?}").to_lowercase();
    RECOVERABLE_PATTERNS
        .iter()
        .any(|pattern| msg.contains(pattern))
}

/// Attempt a health check by sending `Browser.getVersion` via CDP.
/// Returns `Ok(())` if the browser responds within `timeout`.
#[instrument(skip_all, fields(browser.action = "health_check"))]
pub async fn check_browser_health(
    browser: &chromiumoxide::Browser,
    timeout: std::time::Duration,
) -> Result<()> {
    use chromiumoxide::cdp::browser_protocol::browser::GetVersionParams;

    tokio::time::timeout(timeout, browser.execute(GetVersionParams::default()))
        .await
        .map_err(|_| anyhow::anyhow!("Browser health check timed out"))?
        .map_err(|e| anyhow::anyhow!("Browser health check failed: {e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_err(msg: &str) -> anyhow::Error {
        anyhow::anyhow!("{}", msg)
    }

    /// Every pattern in [`RECOVERABLE_PATTERNS`] must classify as
    /// recoverable, plus representative negative cases. Collapsed from 14
    /// near-duplicate per-pattern tests — adding a new pattern means one
    /// new row in `RECOVERABLE_PATTERNS`, not a new test function.
    #[test]
    fn classifies_recoverable_vs_terminal_errors() {
        for pattern in RECOVERABLE_PATTERNS {
            let err = make_err(pattern);
            assert!(
                is_recoverable_error(&err),
                "pattern {pattern:?} must classify as recoverable"
            );
        }
        for msg in [
            "invalid selector",
            "JS evaluation failed: SyntaxError",
            "some random error message",
        ] {
            assert!(
                !is_recoverable_error(&make_err(msg)),
                "{msg:?} must not classify as recoverable (logic error, not browser failure)"
            );
        }
    }

    #[test]
    fn classification_is_case_insensitive() {
        // Production sees error strings produced by chromiumoxide's Debug
        // impl; their casing is not stable. The `to_lowercase()` step in
        // `is_recoverable_error` must remain.
        assert!(is_recoverable_error(&make_err("TARGET CLOSED")));
        assert!(is_recoverable_error(&make_err("Page Crashed")));
    }

    #[test]
    fn classification_traverses_anyhow_context_chain() {
        // Production callers wrap the recoverable CDP error in an outer
        // context (`"Click failed"`, `"Navigate failed"`). The classifier
        // formats with `{:?}` so the pattern in any frame is detected.
        let err = anyhow::anyhow!("target closed").context("Click failed");
        assert!(
            is_recoverable_error(&err),
            "pattern hidden by an outer context must still be detected"
        );
    }
}
