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

    #[test]
    fn recoverable_target_closed() {
        assert!(is_recoverable_error(&make_err("target closed")));
    }

    #[test]
    fn recoverable_frame_detached() {
        assert!(is_recoverable_error(&make_err("frame detached")));
    }

    #[test]
    fn recoverable_frame_has_been_detached() {
        assert!(is_recoverable_error(&make_err("frame has been detached")));
    }

    #[test]
    fn recoverable_session_closed() {
        assert!(is_recoverable_error(&make_err("session closed")));
    }

    #[test]
    fn recoverable_page_crashed() {
        assert!(is_recoverable_error(&make_err("page crashed")));
    }

    #[test]
    fn recoverable_connection_reset() {
        assert!(is_recoverable_error(&make_err("connection reset")));
    }

    #[test]
    fn recoverable_connection_refused() {
        assert!(is_recoverable_error(&make_err("connection refused")));
    }

    #[test]
    fn recoverable_browser_disconnected() {
        assert!(is_recoverable_error(&make_err("browser disconnected")));
    }

    #[test]
    fn recoverable_no_such_target() {
        assert!(is_recoverable_error(&make_err("no such target")));
    }

    #[test]
    fn recoverable_websocket_error() {
        assert!(is_recoverable_error(&make_err("websocket error")));
    }

    #[test]
    fn recoverable_nested_context() {
        let err = anyhow::anyhow!("target closed").context("Click failed");
        assert!(is_recoverable_error(&err));
    }

    #[test]
    fn not_recoverable_invalid_selector() {
        assert!(!is_recoverable_error(&make_err("invalid selector")));
    }

    #[test]
    fn not_recoverable_evaluation_failed() {
        assert!(!is_recoverable_error(&make_err(
            "JS evaluation failed: SyntaxError"
        )));
    }

    #[test]
    fn not_recoverable_random_error() {
        assert!(!is_recoverable_error(&make_err(
            "some random error message"
        )));
    }

    #[test]
    fn case_insensitive_matching() {
        assert!(is_recoverable_error(&make_err("TARGET CLOSED")));
        assert!(is_recoverable_error(&make_err("Page Crashed")));
    }
}
