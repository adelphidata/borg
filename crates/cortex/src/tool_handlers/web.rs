use anyhow::Result;
use tracing::instrument;

use crate::config::Config;
use crate::web;

use super::{check_enabled, require_str_param};

#[instrument(skip_all, fields(tool.name = "web_fetch"))]
pub async fn handle_web_fetch(args: &serde_json::Value, config: &Config) -> Result<String> {
    if let Some(msg) = check_enabled(config.web.enabled, "web") {
        return Ok(msg);
    }
    let url = require_str_param(args, "url")?;
    let max_chars = args["max_chars"].as_u64().map(|v| v as usize);
    match web::web_fetch(url, max_chars).await {
        Ok(content) => Ok(content),
        Err(e) => Ok(format!("Error fetching URL: {e}")),
    }
}

#[instrument(skip_all, fields(tool.name = "web_search"))]
pub async fn handle_web_search(args: &serde_json::Value, config: &Config) -> Result<String> {
    if let Some(msg) = check_enabled(config.web.enabled, "web") {
        return Ok(msg);
    }
    let query = require_str_param(args, "query")?;
    match web::web_search(query, &config.web).await {
        Ok(results) => Ok(results),
        Err(e) => Ok(format!("Error searching: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn handle_web_fetch_disabled() {
        let mut config = Config::default();
        config.web.enabled = false;
        let result = handle_web_fetch(&json!({"url": "https://example.com"}), &config)
            .await
            .unwrap();
        assert!(
            result.contains("disabled"),
            "expected 'disabled' in: {result}"
        );
    }

    #[tokio::test]
    async fn handle_web_search_disabled() {
        let mut config = Config::default();
        config.web.enabled = false;
        let result = handle_web_search(&json!({"query": "test"}), &config)
            .await
            .unwrap();
        assert!(
            result.contains("disabled"),
            "expected 'disabled' in: {result}"
        );
    }

    /// Records span names created during a test.
    struct SpanRecorder(std::sync::Arc<std::sync::Mutex<Vec<String>>>);

    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for SpanRecorder {
        fn on_new_span(
            &self,
            attrs: &tracing::span::Attributes<'_>,
            _id: &tracing::span::Id,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            self.0
                .lock()
                .unwrap()
                .push(attrs.metadata().name().to_string());
        }
    }

    #[tokio::test]
    async fn handle_web_fetch_emits_tracing_span() {
        use tracing_subscriber::layer::SubscriberExt;
        let spans = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::registry().with(SpanRecorder(spans.clone()));
        let _guard = tracing::subscriber::set_default(subscriber);

        let config = Config::default();
        let args = json!({"url": "http://localhost:1"});
        let _ = handle_web_fetch(&args, &config).await;

        let recorded = spans.lock().unwrap();
        assert!(
            recorded.iter().any(|s| s == "handle_web_fetch"),
            "expected 'handle_web_fetch' span, got: {recorded:?}"
        );
    }

    #[tokio::test]
    async fn handle_web_search_emits_tracing_span() {
        use tracing_subscriber::layer::SubscriberExt;
        let spans = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::registry().with(SpanRecorder(spans.clone()));
        let _guard = tracing::subscriber::set_default(subscriber);

        let config = Config::default();
        let args = json!({"query": "test"});
        let _ = handle_web_search(&args, &config).await;

        let recorded = spans.lock().unwrap();
        assert!(
            recorded.iter().any(|s| s == "handle_web_search"),
            "expected 'handle_web_search' span, got: {recorded:?}"
        );
    }
}
