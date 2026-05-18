//! Production [`EmbeddingDispatcher`] backed by
//! [`cortex::memory::embeddings`] — the same provider that backs the
//! memory hybrid-search path. Out-of-crate callers (Cursor, Continue,
//! Open WebUI) get the OpenAI wire shape; the underlying upstream call
//! is whatever `EmbeddingProvider::from_config` resolves (OpenAI,
//! OpenRouter, or Gemini).

use std::sync::Arc;

use async_trait::async_trait;
use cortex::config::Config;
use cortex::memory::embeddings::{generate_embedding_cached, get_or_init_provider, shared_client};

use super::dispatcher::{EmbeddingDispatchError, EmbeddingDispatcher};

/// Production dispatcher. Resolves the provider on each call so
/// operator config edits (`memory.embeddings.enabled`, etc.) propagate
/// without a daemon restart — `get_or_init_provider` is internally
/// cached and only re-resolves when the config fingerprint changes.
pub struct ConfigEmbeddingDispatcher {
    config: Arc<Config>,
}

impl ConfigEmbeddingDispatcher {
    /// Build a dispatcher pinned to the supplied config snapshot.
    pub fn new(config: Arc<Config>) -> Self {
        Self { config }
    }
}

#[async_trait]
impl EmbeddingDispatcher for ConfigEmbeddingDispatcher {
    async fn embed(
        &self,
        _model: &str,
        inputs: Vec<String>,
    ) -> Result<Vec<Vec<f32>>, EmbeddingDispatchError> {
        let provider = get_or_init_provider(&self.config.memory.embeddings)
            .ok_or(EmbeddingDispatchError::ProviderUnavailable)?;

        let client = shared_client();
        // Sequential to preserve input order and to avoid spraying
        // upstream rate limits — embedding providers typically rate-
        // limit per minute and parallel fan-out makes 429s the common
        // case for batches > a few items. Each call is independently
        // cached at the cortex layer, so retries are cheap.
        let mut out = Vec::with_capacity(inputs.len());
        for text in inputs {
            let vec = generate_embedding_cached(client, &provider, &text)
                .await
                .map_err(|e| EmbeddingDispatchError::Upstream(e.to_string()))?;
            out.push(vec);
        }
        Ok(out)
    }
}

/// Convenience constructor used by the gateway server when wiring the
/// OpenAI-compat router. Mirrors [`super::arc_dispatcher`].
pub fn arc_embedding_dispatcher(config: &Config) -> Arc<dyn EmbeddingDispatcher> {
    Arc::new(ConfigEmbeddingDispatcher::new(Arc::new(config.clone())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use cortex::config::Config;

    #[tokio::test]
    async fn embed_returns_provider_unavailable_when_embeddings_disabled() {
        // Real failure: operator explicitly disables embeddings (or
        // never set an API key). The dispatcher must surface a
        // distinguishable `ProviderUnavailable` so the route layer can
        // return 503 — silently degrading to an empty vector or zero
        // vectors would poison downstream similarity search.
        let mut config = Config::default();
        config.memory.embeddings.enabled = false;
        // Make sure the cache doesn't hand us back a stale resolution
        // from a previous test run.
        cortex::memory::embeddings::invalidate_provider_cache();

        let dispatcher = ConfigEmbeddingDispatcher::new(Arc::new(config));
        let err = dispatcher
            .embed("text-embedding-3-small", vec!["hi".into()])
            .await
            .expect_err("must error when disabled");
        assert!(
            matches!(err, EmbeddingDispatchError::ProviderUnavailable),
            "expected ProviderUnavailable, got: {err:?}"
        );
    }
}
