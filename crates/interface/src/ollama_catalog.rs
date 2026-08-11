//! Dynamic Ollama model catalog fetch.
//!
//! Hits the local Ollama server's `GET /api/tags` once per session and caches
//! the installed-model list in memory. Callers read the cache via
//! [`cached_models`]; a `None` return means the caller should fall back to the
//! curated Ollama block in [`cortex::llm::model_catalog`] (fetch hasn't
//! completed, never ran, failed, or the server has no models pulled).
//!
//! Unlike the OpenRouter catalog there is no curated intersection: the point
//! is to show exactly what `ollama pull` has installed on this machine, which
//! rarely matches a hardcoded list.

use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use serde::Deserialize;

/// `(model_id, display_label)` pairs — same shape as
/// [`crate::openrouter_catalog::ModelList`].
pub type ModelList = Vec<(String, String)>;

// Local server: generous enough for a slow first response, short enough that
// a wedged remote OLLAMA_HOST can't stall a prefetch thread for long.
const FETCH_TIMEOUT: Duration = Duration::from_secs(3);

static CACHE: LazyLock<Mutex<Option<ModelList>>> = LazyLock::new(|| Mutex::new(None));

/// Return the cached installed-model list if a prefetch has populated it.
/// `None` means callers should fall back to the hardcoded list.
pub fn cached_models() -> Option<ModelList> {
    match CACHE.lock() {
        Ok(g) => g.clone(),
        Err(e) => {
            tracing::warn!("ollama catalog cache lock poisoned on read: {e}; using hardcoded list");
            None
        }
    }
}

/// Spawn a background task that fetches the installed-model list and
/// populates the session cache. `base_url_override` is the configured
/// `llm.base_url` (when the active provider is Ollama); precedence follows
/// [`cortex::llm::provider::ollama_origin`]. Safe to call multiple times —
/// the cache is overwritten only on success. Silently skipped when no Tokio
/// runtime is available.
pub fn spawn_prefetch(base_url_override: Option<String>) {
    if tokio::runtime::Handle::try_current().is_err() {
        tracing::debug!(
            "ollama catalog prefetch skipped: no tokio runtime (use prefetch_from_sync)"
        );
        return;
    }
    let url = tags_url(base_url_override.as_deref());
    tokio::spawn(async move {
        match fetch_and_parse(&url, FETCH_TIMEOUT).await {
            Ok(models) => store_in_cache(models),
            Err(e) => {
                tracing::debug!("ollama catalog fetch failed: {e}; using hardcoded list");
            }
        }
    });
}

/// Prefetch from a synchronous context (the onboarding wizard runs before any
/// Tokio runtime exists). Fire-and-forget on a plain thread so onboarding
/// startup never blocks; the cache populates while the user is still on the
/// name/security tabs, well before the model picker renders.
pub fn prefetch_from_sync() {
    if tokio::runtime::Handle::try_current().is_ok() {
        spawn_prefetch(None);
        return;
    }
    std::thread::spawn(|| {
        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                tracing::debug!("ollama catalog prefetch runtime failed: {e}");
                return;
            }
        };
        match rt.block_on(fetch_and_parse(&tags_url(None), FETCH_TIMEOUT)) {
            Ok(models) => store_in_cache(models),
            Err(e) => {
                tracing::debug!("ollama catalog fetch failed: {e}; using hardcoded list");
            }
        }
    });
}

fn store_in_cache(models: ModelList) {
    match CACHE.lock() {
        Ok(mut g) => *g = Some(models),
        Err(e) => tracing::warn!("ollama catalog cache lock poisoned: {e}"),
    }
}

#[cfg(test)]
fn reset_cache() {
    if let Ok(mut g) = CACHE.lock() {
        *g = None;
    }
}

#[cfg(test)]
fn seed_cache(models: ModelList) {
    store_in_cache(models);
}

/// The `/api/tags` URL for the effective Ollama origin (`llm.base_url`
/// override → `OLLAMA_HOST` → localhost default).
fn tags_url(base_url_override: Option<&str>) -> String {
    format!(
        "{}/api/tags",
        cortex::llm::provider::ollama_origin(base_url_override)
    )
}

#[derive(Debug, Deserialize)]
struct TagsResponse {
    models: Vec<TagEntry>,
}

#[derive(Debug, Deserialize)]
struct TagEntry {
    name: String,
    #[serde(default)]
    details: Option<TagDetails>,
}

#[derive(Debug, Deserialize)]
struct TagDetails {
    #[serde(default)]
    parameter_size: Option<String>,
}

/// Fetch the installed-model list from `GET /api/tags`.
///
/// Returns `Err` when the HTTP call fails, the body isn't the expected JSON
/// shape, or the server reports zero installed models (an empty picker is
/// worse than the hardcoded suggestions, which double as `ollama pull`
/// candidates).
async fn fetch_and_parse(url: &str, timeout: Duration) -> anyhow::Result<ModelList> {
    // Shared pool, per-request budget: the caller's timeout is a property of
    // this one fetch, not a reason to own a second connection pool.
    let resp = cortex::http::shared()
        .get(url)
        .timeout(timeout)
        .header("Accept", "application/json")
        .send()
        .await?
        .error_for_status()?;
    let parsed: TagsResponse = resp.json().await?;
    if parsed.models.is_empty() {
        anyhow::bail!("ollama server reports no installed models");
    }
    Ok(parsed
        .models
        .into_iter()
        .map(|m| {
            let label = match m.details.and_then(|d| d.parameter_size) {
                Some(size) => format!("{} ({size})", m.name),
                None => m.name.clone(),
            };
            (m.name, label)
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// Serialize tests that touch the process-global [`CACHE`] static.
    static CACHE_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    async fn mock_tags_server(body: serde_json::Value, status: u16) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/tags"))
            .respond_with(ResponseTemplate::new(status).set_body_json(body))
            .mount(&server)
            .await;
        server
    }

    #[tokio::test]
    async fn fetch_lists_installed_models_with_parameter_size() {
        let body = serde_json::json!({
            "models": [
                {
                    "name": "llama3.3:latest",
                    "details": { "parameter_size": "70.6B", "quantization_level": "Q4_K_M" }
                },
                { "name": "qwen3:8b" }
            ]
        });
        let server = mock_tags_server(body, 200).await;
        let got = fetch_and_parse(&format!("{}/api/tags", server.uri()), FETCH_TIMEOUT)
            .await
            .expect("fetch ok");
        assert_eq!(
            got,
            vec![
                (
                    "llama3.3:latest".to_string(),
                    "llama3.3:latest (70.6B)".to_string()
                ),
                ("qwen3:8b".to_string(), "qwen3:8b".to_string()),
            ]
        );
    }

    /// Zero installed models → Err, so the picker falls back to the hardcoded
    /// suggestions instead of rendering empty.
    #[tokio::test]
    async fn fetch_empty_model_list_errors() {
        let server = mock_tags_server(serde_json::json!({ "models": [] }), 200).await;
        let err = fetch_and_parse(&format!("{}/api/tags", server.uri()), FETCH_TIMEOUT)
            .await
            .expect_err("empty list must error");
        assert!(
            err.to_string().contains("no installed models"),
            "got: {err}"
        );
    }

    #[tokio::test]
    async fn fetch_malformed_body_errors() {
        let server = mock_tags_server(serde_json::json!({ "tags": [] }), 200).await;
        let err = fetch_and_parse(&format!("{}/api/tags", server.uri()), FETCH_TIMEOUT)
            .await
            .expect_err("malformed body must error");
        assert!(
            err.downcast_ref::<reqwest::Error>()
                .is_some_and(reqwest::Error::is_decode),
            "expected decode error, got: {err}"
        );
    }

    #[tokio::test]
    async fn fetch_server_error_errors() {
        let server = mock_tags_server(serde_json::json!({}), 500).await;
        assert!(
            fetch_and_parse(&format!("{}/api/tags", server.uri()), FETCH_TIMEOUT)
                .await
                .is_err(),
            "500 must not populate the cache"
        );
    }

    /// `models_for_provider("ollama")` returns the cached installed list when
    /// populated, and the hardcoded fallback when empty.
    #[tokio::test(flavor = "multi_thread")]
    async fn models_for_provider_ollama_prefers_cache() {
        let _guard = CACHE_TEST_LOCK.lock().await;
        reset_cache();
        let seeded = vec![("installed:tag".to_string(), "installed:tag".to_string())];
        seed_cache(seeded.clone());
        assert_eq!(crate::onboarding::models_for_provider("ollama"), seeded);

        reset_cache();
        let fallback = crate::onboarding::models_for_provider("ollama");
        let curated = cortex::llm::model_catalog::curated("ollama");
        assert_eq!(fallback.len(), curated.len());
        assert_eq!(fallback[0].0, curated[0].0);
    }
}
