//! Dynamic OpenRouter catalog fetch.
//!
//! Hits `https://openrouter.ai/api/v1/models` once per session, intersects the
//! live catalog with our curated [`OPENROUTER_MODELS`] list, and caches the
//! result in memory. Callers read the cache via [`cached_models`]; a `None`
//! return means the caller should fall back to the hardcoded list (the cache
//! is empty because the fetch hasn't completed, never ran, or failed).
//!
//! The curated list provides stable ordering + labels; the live fetch drops
//! stale IDs and tags zero-priced models as "(free)". No refresh command, no
//! disk cache, no TTL.

use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use serde::Deserialize;

use crate::onboarding::OPENROUTER_MODELS;

const CATALOG_URL_DEFAULT: &str = "https://openrouter.ai/api/v1/models";
const FETCH_TIMEOUT: Duration = Duration::from_secs(8);

/// `(model_id, display_label)` pairs — matches the hardcoded shape in
/// [`crate::onboarding::OPENROUTER_MODELS`] but owned so it can carry a
/// dynamically-generated `(free)` tag.
pub type ModelList = Vec<(String, String)>;

static CACHE: LazyLock<Mutex<Option<ModelList>>> = LazyLock::new(|| Mutex::new(None));

/// Return the cached OpenRouter catalog if the session prefetch has populated
/// it. `None` means callers should fall back to the hardcoded curated list.
pub fn cached_models() -> Option<ModelList> {
    match CACHE.lock() {
        Ok(g) => g.clone(),
        Err(e) => {
            tracing::warn!(
                "openrouter catalog cache lock poisoned on read: {e}; using hardcoded list"
            );
            None
        }
    }
}

/// Spawn a background task that fetches the live OpenRouter catalog and
/// populates the session cache. Safe to call multiple times — subsequent calls
/// overwrite the cache only on success. Silently skipped when no Tokio runtime
/// is available (e.g. in synchronous unit tests).
pub fn spawn_prefetch() {
    if tokio::runtime::Handle::try_current().is_err() {
        return;
    }
    tokio::spawn(async move {
        match fetch_and_parse(CATALOG_URL_DEFAULT, FETCH_TIMEOUT).await {
            Ok(models) => store_in_cache(models),
            Err(e) => {
                tracing::warn!("openrouter catalog fetch failed: {e}; using hardcoded list");
            }
        }
    });
}

fn store_in_cache(models: ModelList) {
    match CACHE.lock() {
        Ok(mut g) => *g = Some(models),
        Err(e) => tracing::warn!("openrouter catalog cache lock poisoned: {e}"),
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

#[derive(Debug, Deserialize)]
struct CatalogResponse {
    data: Vec<CatalogEntry>,
}

#[derive(Debug, Deserialize)]
struct CatalogEntry {
    id: String,
    #[serde(default)]
    pricing: Option<Pricing>,
}

#[derive(Debug, Deserialize)]
struct Pricing {
    #[serde(default)]
    prompt: Option<String>,
    #[serde(default)]
    completion: Option<String>,
}

impl Pricing {
    /// OpenRouter encodes pricing as strings ("0", "0.0", "0.000005", etc.).
    /// A model counts as free when both prompt and completion parse to a
    /// non-positive number — matching a bare `"0"` string would miss
    /// `"0.0"` / `"0.000000"` which some providers emit.
    fn is_free(&self) -> bool {
        fn is_zero(s: &Option<String>) -> bool {
            s.as_deref()
                .and_then(|v| v.parse::<f64>().ok())
                .is_some_and(|n| n <= 0.0)
        }
        is_zero(&self.prompt) && is_zero(&self.completion)
    }
}

/// Fetch the live catalog and intersect it against [`OPENROUTER_MODELS`].
///
/// Returns `Err` when:
/// - the HTTP call fails (timeout, non-2xx, transport)
/// - the response body is not valid JSON shaped like `{ "data": [...] }`
/// - the intersection with the curated list is empty (we'd otherwise return
///   nothing useful and callers should fall back to the hardcoded list)
async fn fetch_and_parse(base_url: &str, timeout: Duration) -> anyhow::Result<ModelList> {
    let client = reqwest::Client::builder().timeout(timeout).build()?;
    let resp = client
        .get(base_url)
        .header("Accept", "application/json")
        .send()
        .await?
        .error_for_status()?;
    let parsed: CatalogResponse = resp.json().await?;
    let curated = intersect_with_curated(&parsed.data);
    if curated.is_empty() {
        anyhow::bail!("openrouter catalog contained no curated IDs");
    }
    Ok(curated)
}

/// Preserve curated order; drop curated IDs missing from the live response;
/// append "(free)" to labels when pricing is zero.
fn intersect_with_curated(live: &[CatalogEntry]) -> ModelList {
    let mut result = Vec::with_capacity(OPENROUTER_MODELS.len());
    for (curated_id, curated_label) in OPENROUTER_MODELS {
        let Some(entry) = live.iter().find(|e| e.id == *curated_id) else {
            continue;
        };
        let label = if entry.pricing.as_ref().is_some_and(Pricing::is_free) {
            format!("{curated_label} (free)")
        } else {
            (*curated_label).to_string()
        };
        result.push(((*curated_id).to_string(), label));
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// Serialize tests that touch the process-global [`CACHE`] static —
    /// without this they race and observe each other's writes, producing
    /// flaky `None`/stale values. Uses a Tokio mutex so the guard can be
    /// held across `await` points without tripping clippy.
    static CACHE_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    fn catalog_body(ids: &[(&str, Option<(&str, &str)>)]) -> serde_json::Value {
        let data: Vec<serde_json::Value> = ids
            .iter()
            .map(|(id, pricing)| {
                let mut entry = serde_json::json!({ "id": id });
                if let Some((p, c)) = pricing {
                    entry["pricing"] = serde_json::json!({
                        "prompt": p,
                        "completion": c,
                    });
                }
                entry
            })
            .collect();
        serde_json::json!({ "data": data })
    }

    async fn mock_server_with(body: serde_json::Value, status: u16) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/models"))
            .respond_with(ResponseTemplate::new(status).set_body_json(body))
            .mount(&server)
            .await;
        server
    }

    fn server_url(server: &MockServer) -> String {
        format!("{}/api/v1/models", server.uri())
    }

    /// A curated ID that appears in the live response passes through; an
    /// uncurated live ID is ignored. Relies on real curated entries.
    #[tokio::test]
    async fn fetch_intersects_with_curated() {
        let curated_id = OPENROUTER_MODELS[0].0;
        let body = catalog_body(&[(curated_id, None), ("foo/not-curated", None)]);
        let server = mock_server_with(body, 200).await;
        let got = fetch_and_parse(&server_url(&server), Duration::from_secs(2))
            .await
            .expect("fetch ok");
        assert!(got.iter().any(|(id, _)| id == curated_id));
        assert!(got.iter().all(|(id, _)| id != "foo/not-curated"));
    }

    /// When a curated ID is absent from live, it's dropped; other curated IDs
    /// that *are* live still appear.
    #[tokio::test]
    async fn fetch_drops_stale_curated_ids() {
        assert!(OPENROUTER_MODELS.len() >= 2, "need two curated entries");
        let keep = OPENROUTER_MODELS[0].0;
        let drop = OPENROUTER_MODELS[1].0;
        let body = catalog_body(&[(keep, None)]);
        let server = mock_server_with(body, 200).await;
        let got = fetch_and_parse(&server_url(&server), Duration::from_secs(2))
            .await
            .expect("fetch ok");
        assert!(got.iter().any(|(id, _)| id == keep));
        assert!(got.iter().all(|(id, _)| id != drop));
    }

    /// Zero prompt+completion pricing surfaces a "(free)" suffix; nonzero
    /// pricing does not.
    #[tokio::test]
    async fn fetch_tags_free_models() {
        assert!(OPENROUTER_MODELS.len() >= 2);
        let free_id = OPENROUTER_MODELS[0].0;
        let paid_id = OPENROUTER_MODELS[1].0;
        let body = catalog_body(&[
            (free_id, Some(("0", "0"))),
            (paid_id, Some(("0.000005", "0.000025"))),
        ]);
        let server = mock_server_with(body, 200).await;
        let got = fetch_and_parse(&server_url(&server), Duration::from_secs(2))
            .await
            .expect("fetch ok");
        let free_label = &got.iter().find(|(id, _)| id == free_id).unwrap().1;
        let paid_label = &got.iter().find(|(id, _)| id == paid_id).unwrap().1;
        assert!(free_label.contains("(free)"), "got: {free_label}");
        assert!(!paid_label.contains("(free)"), "got: {paid_label}");
    }

    #[tokio::test]
    async fn fetch_falls_back_on_500() {
        let server = mock_server_with(serde_json::json!({}), 500).await;
        let err = fetch_and_parse(&server_url(&server), Duration::from_secs(2))
            .await
            .expect_err("expected error on 500");
        let s = err.to_string();
        assert!(
            s.contains("500") || s.to_lowercase().contains("status"),
            "error should reflect HTTP status, got: {s}"
        );
    }

    #[tokio::test]
    async fn fetch_falls_back_on_malformed_json() {
        let body = serde_json::json!({ "not_data": [] });
        let server = mock_server_with(body, 200).await;
        let err = fetch_and_parse(&server_url(&server), Duration::from_secs(2))
            .await
            .expect_err("expected decode error");
        // Must be a reqwest decode error, not a transport/status error — the
        // point of this test is to prove we surface malformed payloads as
        // failures rather than silently treating them as empty-but-ok.
        assert!(
            err.downcast_ref::<reqwest::Error>()
                .is_some_and(|e| e.is_decode()),
            "expected reqwest decode error, got: {err}"
        );
    }

    /// `"0.0"` / `"0.000000"` must count as free; these are values OpenRouter
    /// returns in practice and a naive `matches!(_, Some("0"))` would miss
    /// them.
    #[test]
    fn pricing_is_free_handles_decimal_zero_variants() {
        let cases = [
            (Some("0"), Some("0"), true),
            (Some("0.0"), Some("0.0"), true),
            (Some("0.000000"), Some("0.0"), true),
            (Some("0"), Some("0.000005"), false),
            (Some("0.000005"), Some("0"), false),
            (None, Some("0"), false),
            (Some("not-a-number"), Some("0"), false),
        ];
        for (prompt, completion, expected) in cases {
            let p = Pricing {
                prompt: prompt.map(str::to_string),
                completion: completion.map(str::to_string),
            };
            assert_eq!(
                p.is_free(),
                expected,
                "prompt={prompt:?} completion={completion:?}"
            );
        }
    }

    #[tokio::test]
    async fn fetch_falls_back_on_timeout() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/models"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "data": [] }))
                    .set_delay(Duration::from_millis(500)),
            )
            .mount(&server)
            .await;
        // Any transport error (including timeout) produces Err — that's
        // enough; the error is downcast to a reqwest::Error, whose display
        // wording varies across versions and platforms so we don't grep it.
        let err = fetch_and_parse(&server_url(&server), Duration::from_millis(50))
            .await
            .expect_err("short timeout against delayed response must error");
        assert!(
            err.downcast_ref::<reqwest::Error>()
                .is_some_and(|e| e.is_timeout() || e.is_request()),
            "expected reqwest transport error, got: {err}"
        );
    }

    /// Empty intersection → Err, so callers fall back to the hardcoded list
    /// rather than rendering an empty picker.
    #[tokio::test]
    async fn fetch_empty_intersection_errors() {
        let body = catalog_body(&[("foo/one", None), ("bar/two", None)]);
        let server = mock_server_with(body, 200).await;
        let err = fetch_and_parse(&server_url(&server), Duration::from_secs(2))
            .await
            .expect_err("expected empty-intersection error");
        assert!(err.to_string().contains("no curated IDs"), "got: {err}");
    }

    /// Successful fetch populates the cache.
    #[tokio::test(flavor = "multi_thread")]
    async fn cache_populated_after_fetch() {
        let _guard = CACHE_TEST_LOCK.lock().await;
        reset_cache();
        let curated_id = OPENROUTER_MODELS[0].0;
        let body = catalog_body(&[(curated_id, None)]);
        let server = mock_server_with(body, 200).await;
        let models = fetch_and_parse(&server_url(&server), Duration::from_secs(2))
            .await
            .unwrap();
        store_in_cache(models.clone());
        let cached = cached_models().expect("cache should be populated");
        assert_eq!(cached, models);
        reset_cache();
    }

    /// A later fetch failure doesn't clobber an already-good cache (the
    /// spawn_prefetch path explicitly only writes on `Ok`).
    #[tokio::test(flavor = "multi_thread")]
    async fn cache_preserved_when_later_fetch_fails() {
        let _guard = CACHE_TEST_LOCK.lock().await;
        reset_cache();
        let good = vec![("a/b".to_string(), "A B".to_string())];
        seed_cache(good.clone());
        // Simulate a failed fetch: we don't call store_in_cache, matching
        // spawn_prefetch's Err arm.
        let server = mock_server_with(serde_json::json!({}), 500).await;
        let _ = fetch_and_parse(&server_url(&server), Duration::from_secs(2)).await;
        assert_eq!(cached_models(), Some(good));
        reset_cache();
    }

    /// `models_for_provider("openrouter")` returns the cached list when
    /// populated (dynamic path), not the hardcoded fallback.
    #[tokio::test(flavor = "multi_thread")]
    async fn models_for_provider_openrouter_uses_cache_when_present() {
        let _guard = CACHE_TEST_LOCK.lock().await;
        reset_cache();
        let seeded = vec![("seeded/only".to_string(), "Seeded".to_string())];
        seed_cache(seeded.clone());
        let got = crate::onboarding::models_for_provider("openrouter");
        assert_eq!(got, seeded);
        reset_cache();
    }

    /// When the cache is empty, `models_for_provider("openrouter")` returns
    /// an owned copy of the hardcoded fallback.
    #[tokio::test(flavor = "multi_thread")]
    async fn models_for_provider_openrouter_falls_back_when_cache_empty() {
        let _guard = CACHE_TEST_LOCK.lock().await;
        reset_cache();
        let got = crate::onboarding::models_for_provider("openrouter");
        assert_eq!(got.len(), OPENROUTER_MODELS.len());
        assert_eq!(got[0].0, OPENROUTER_MODELS[0].0);
    }
}
