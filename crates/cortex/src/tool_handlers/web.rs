use anyhow::Result;
use tracing::instrument;

use crate::config::Config;
use crate::db::Database;
use crate::security::sanitize::wrap_untrusted;
use crate::web;

use super::{check_enabled, require_str_param};

// Successful web tool output is wrapped via `wrap_untrusted` before returning:
// fetched pages and search results are externally controlled, so they must be
// treated as data not instructions, and the `<untrusted_content>` marker keeps
// them out of durable memory via the pre-compaction flush filter
// (`contains_untrusted_marker` in `agent::prompt_assembly`). The two MUST agree.

/// Cache strategy parsed from the `mode` argument on `web_search`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchMode {
    /// Cache hit if fresh (within TTL); otherwise hit network and cache the
    /// result. Default when `mode` is omitted.
    Auto,
    /// Always hit the network. Cache is updated on success but never read.
    Live,
    /// Never hit the network. Returns the cached body if present, otherwise
    /// a short note so the LLM can choose to retry with `live`.
    Cached,
}

impl SearchMode {
    fn parse(raw: Option<&str>) -> Self {
        match raw.unwrap_or("auto") {
            "live" => SearchMode::Live,
            "cached" => SearchMode::Cached,
            _ => SearchMode::Auto,
        }
    }
}

/// Outcome of the synchronous cache lookup. `Serve` is a body to return as-is
/// (cached hit, or the "no cached results" note for `Cached` mode); `CallLive`
/// signals that the caller should hit the network and then `cache_store` the
/// result. Splitting cache logic from the network call keeps the rusqlite
/// `Connection` out of any `.await`-spanning scope.
#[derive(Debug)]
pub enum CacheDecision {
    Serve(String),
    CallLive,
}

/// Decide what `web_search` should do for a given mode + cached row + freshness.
/// Pure function — no I/O, fully unit-testable.
pub fn decide_cache(
    mode: SearchMode,
    query: &str,
    cached: Option<&crate::db::WebSearchCacheRow>,
    ttl_secs: u64,
    now_secs: i64,
) -> CacheDecision {
    let fresh = |row: &crate::db::WebSearchCacheRow| -> bool {
        ttl_secs > 0 && now_secs.saturating_sub(row.fetched_at) <= ttl_secs as i64
    };
    match mode {
        SearchMode::Cached => match cached {
            Some(row) => CacheDecision::Serve(row.body.clone()),
            None => CacheDecision::Serve(format!(
                "[cache miss] No cached results for \"{query}\". Retry with mode=live."
            )),
        },
        SearchMode::Auto => match cached {
            Some(row) if fresh(row) => CacheDecision::Serve(row.body.clone()),
            _ => CacheDecision::CallLive,
        },
        SearchMode::Live => CacheDecision::CallLive,
    }
}

/// Look up the cache and decide what to serve. Synchronous; the caller is
/// responsible for awaiting the live call (if needed) without holding any
/// `&Database` borrow across the await.
async fn cache_lookup(
    db: &Database,
    provider: &str,
    query_hash: &str,
    mode: SearchMode,
    query: &str,
    ttl_secs: u64,
    now_secs: i64,
) -> Result<CacheDecision> {
    let cached = db.web_search_cache_get(provider, query_hash).await?;
    Ok(decide_cache(
        mode,
        query,
        cached.as_ref(),
        ttl_secs,
        now_secs,
    ))
}

/// Persist a fresh body. Async mate of `cache_lookup`.
async fn cache_store(
    db: &Database,
    provider: &str,
    query_hash: &str,
    query: &str,
    body: &str,
    now_secs: i64,
) -> Result<()> {
    db.web_search_cache_put(provider, query_hash, query, body, now_secs)
        .await
}

#[instrument(skip_all, fields(tool.name = "web_fetch"))]
pub async fn handle_web_fetch(args: &serde_json::Value, config: &Config) -> Result<String> {
    if let Some(msg) = check_enabled(config.web.enabled, "web") {
        return Ok(msg);
    }
    let url = require_str_param(args, "url")?;
    let max_chars = args["max_chars"].as_u64().map(|v| v as usize);
    match web::web_fetch(url, max_chars, &config.security.url_policy).await {
        Ok(content) => Ok(wrap_untrusted("web_fetch", &content)),
        // `{:#}` walks the anyhow context chain so policy-block messages
        // from the redirect callback surface alongside the top context.
        Err(e) => Ok(format!("Error fetching URL: {e:#}")),
    }
}

#[instrument(skip_all, fields(tool.name = "web_search"))]
pub async fn handle_web_search(args: &serde_json::Value, config: &Config) -> Result<String> {
    if let Some(msg) = check_enabled(config.web.enabled, "web") {
        return Ok(msg);
    }
    let query = require_str_param(args, "query")?;
    let mode = SearchMode::parse(args["mode"].as_str());
    let provider = web::resolve_provider_id(&config.web).to_string();

    let db = match Database::open().await {
        Ok(d) => d,
        Err(e) => {
            // Cache unavailable — degrade to a live fetch rather than failing the tool.
            tracing::warn!(error = %e, "web_search cache unavailable, falling back to live");
            return match web::web_search(query, &config.web).await {
                Ok(r) => Ok(wrap_untrusted("web_search", &r)),
                Err(e) => Ok(format!("Error searching: {e}")),
            };
        }
    };
    let now = chrono::Utc::now().timestamp();
    let ttl = config.web.search_cache_ttl_secs;
    let hash = crate::db::web_search_cache::hash_query(query);
    let decision = match cache_lookup(&db, &provider, &hash, mode, query, ttl, now).await {
        Ok(d) => d,
        Err(e) => return Ok(format!("Error searching: {e}")),
    };
    drop(db);

    match decision {
        CacheDecision::Serve(body) => Ok(wrap_untrusted("web_search", &body)),
        CacheDecision::CallLive => match web::web_search(query, &config.web).await {
            Ok(body) => {
                // Cache the raw body so the trust wrapper is applied once, at
                // read time, and never accumulates across cache round-trips.
                match Database::open().await {
                    Ok(db2) => {
                        if let Err(e) =
                            cache_store(&db2, &provider, &hash, query, &body, now).await
                        {
                            tracing::warn!(error = %e, "web_search cache write failed");
                        }
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "web_search cache reopen failed; result not cached");
                    }
                }
                Ok(wrap_untrusted("web_search", &body))
            }
            Err(e) => Ok(format!("Error searching: {e}")),
        },
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

    fn make_row(body: &str, fetched_at: i64) -> crate::db::WebSearchCacheRow {
        crate::db::WebSearchCacheRow {
            provider: "tavily".into(),
            query_hash: "h".into(),
            query: "q".into(),
            body: body.into(),
            fetched_at,
        }
    }

    #[test]
    fn decide_cache_auto_fresh_serves_cached() {
        let row = make_row("CACHED", 1000);
        let d = decide_cache(SearchMode::Auto, "q", Some(&row), 60, 1010);
        match d {
            CacheDecision::Serve(b) => assert_eq!(b, "CACHED"),
            CacheDecision::CallLive => panic!("expected Serve"),
        }
    }

    #[test]
    fn decide_cache_auto_stale_calls_live() {
        let row = make_row("CACHED", 1000);
        let d = decide_cache(SearchMode::Auto, "q", Some(&row), 60, 1100);
        assert!(matches!(d, CacheDecision::CallLive));
    }

    #[test]
    fn decide_cache_auto_miss_calls_live() {
        let d = decide_cache(SearchMode::Auto, "q", None, 60, 1000);
        assert!(matches!(d, CacheDecision::CallLive));
    }

    #[test]
    fn decide_cache_live_always_calls_live_even_with_fresh_cache() {
        let row = make_row("CACHED", 1000);
        let d = decide_cache(SearchMode::Live, "q", Some(&row), 60, 1005);
        assert!(matches!(d, CacheDecision::CallLive));
    }

    #[test]
    fn decide_cache_cached_hit_serves_cached_even_when_stale() {
        // Cached mode is opt-in by the model; staleness is its problem, not ours.
        let row = make_row("STALE-BUT-RETURNED", 0);
        let d = decide_cache(SearchMode::Cached, "q", Some(&row), 60, 999_999);
        match d {
            CacheDecision::Serve(b) => assert_eq!(b, "STALE-BUT-RETURNED"),
            _ => panic!("expected Serve"),
        }
    }

    #[test]
    fn decide_cache_cached_miss_returns_note_not_calllive() {
        let d = decide_cache(SearchMode::Cached, "rust async", None, 60, 1000);
        match d {
            CacheDecision::Serve(b) => {
                assert!(b.contains("[cache miss]"), "got: {b}");
                assert!(b.contains("rust async"), "expected query echoed: {b}");
                assert!(b.contains("mode=live"), "expected retry hint: {b}");
            }
            CacheDecision::CallLive => panic!("Cached mode must never hit network"),
        }
    }

    #[test]
    fn decide_cache_ttl_zero_disables_cache_in_auto_mode() {
        let row = make_row("CACHED", 1000);
        let d = decide_cache(SearchMode::Auto, "q", Some(&row), 0, 1000);
        assert!(matches!(d, CacheDecision::CallLive));
    }

    #[test]
    fn search_mode_parse_defaults_to_auto() {
        assert_eq!(SearchMode::parse(None), SearchMode::Auto);
        assert_eq!(SearchMode::parse(Some("nonsense")), SearchMode::Auto);
        assert_eq!(SearchMode::parse(Some("live")), SearchMode::Live);
        assert_eq!(SearchMode::parse(Some("cached")), SearchMode::Cached);
    }

    #[tokio::test]
    async fn cache_store_then_lookup_serves_back_in_auto() {
        let db = Database::test_db().await;
        let hash = crate::db::web_search_cache::hash_query("q");
        cache_store(&db, "tavily", &hash, "q", "BODY", 1000)
            .await
            .unwrap();
        let decision = cache_lookup(&db, "tavily", &hash, SearchMode::Auto, "q", 60, 1010)
            .await
            .unwrap();
        match decision {
            CacheDecision::Serve(b) => assert_eq!(b, "BODY"),
            CacheDecision::CallLive => panic!("expected fresh hit"),
        }
    }

    #[tokio::test]
    async fn cache_lookup_returns_calllive_after_ttl_expiry() {
        let db = Database::test_db().await;
        let hash = crate::db::web_search_cache::hash_query("q");
        cache_store(&db, "tavily", &hash, "q", "BODY", 1000)
            .await
            .unwrap();
        let decision = cache_lookup(&db, "tavily", &hash, SearchMode::Auto, "q", 60, 1100)
            .await
            .unwrap();
        assert!(matches!(decision, CacheDecision::CallLive));
    }

    #[tokio::test]
    async fn cache_store_overwrites_existing_row_on_refresh() {
        let db = Database::test_db().await;
        let hash = crate::db::web_search_cache::hash_query("q");
        cache_store(&db, "tavily", &hash, "q", "v1", 1000)
            .await
            .unwrap();
        cache_store(&db, "tavily", &hash, "q", "v2", 2000)
            .await
            .unwrap();
        let row = db
            .web_search_cache_get("tavily", &hash)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.body, "v2");
        assert_eq!(row.fetched_at, 2000);
        assert_eq!(db.web_search_cache_count().await.unwrap(), 1);
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
