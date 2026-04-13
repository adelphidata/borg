use anyhow::{bail, Result};
use sha2::{Digest, Sha256};
use std::sync::{LazyLock, Mutex};
use tracing::{debug, instrument};

use crate::config::{Config, EmbeddingsConfig};
use crate::constants::{GEMINI_EMBEDDING_DIM, MAX_EMBEDDING_INPUT_CHARS, OPENAI_EMBEDDING_DIM};
use crate::db::{ChunkData, Database};

/// Shared HTTP client for all embedding API calls (connection pooling + keep-alive).
static EMBEDDING_CLIENT: LazyLock<reqwest::Client> = LazyLock::new(reqwest::Client::new);

/// Cached provider entry: config fingerprint + resolved provider.
type CachedProvider = Option<(String, Option<EmbeddingProvider>)>;

/// Cached embedding provider keyed on config fingerprint.
static PROVIDER_CACHE: LazyLock<Mutex<CachedProvider>> = LazyLock::new(|| Mutex::new(None));

/// Resolved embedding provider with endpoint, key, model, and dimension.
#[derive(Debug, Clone)]
pub struct EmbeddingProvider {
    /// API endpoint URL for embedding requests.
    pub endpoint: String,
    /// API key for authentication.
    pub api_key: String,
    /// Model identifier (e.g. "text-embedding-3-small").
    pub model: String,
    /// Dimensionality of the output embedding vectors.
    pub dimension: usize,
}

/// Static metadata for each supported embedding provider.
struct EmbeddingProviderMeta {
    name: &'static str,
    env_var: &'static str,
    endpoint: &'static str,
    default_model: &'static str,
    default_dim: usize,
}

/// Single source of truth for embedding provider configuration.
/// Order determines auto-detection priority (first match wins).
const EMBEDDING_PROVIDERS: &[EmbeddingProviderMeta] = &[
    EmbeddingProviderMeta {
        name: "openai",
        env_var: "OPENAI_API_KEY",
        endpoint: "https://api.openai.com/v1/embeddings",
        default_model: "text-embedding-3-small",
        default_dim: OPENAI_EMBEDDING_DIM,
    },
    EmbeddingProviderMeta {
        name: "openrouter",
        env_var: "OPENROUTER_API_KEY",
        endpoint: "https://openrouter.ai/api/v1/embeddings",
        default_model: "openai/text-embedding-3-small",
        default_dim: OPENAI_EMBEDDING_DIM,
    },
    EmbeddingProviderMeta {
        name: "gemini",
        env_var: "GEMINI_API_KEY",
        endpoint: "https://generativelanguage.googleapis.com/v1beta/openai/embeddings",
        default_model: "text-embedding-004",
        default_dim: GEMINI_EMBEDDING_DIM,
    },
];

/// Look up provider metadata by name.
fn find_provider_meta(name: &str) -> Option<&'static EmbeddingProviderMeta> {
    EMBEDDING_PROVIDERS.iter().find(|p| p.name == name)
}

impl EmbeddingProvider {
    /// Resolve an embedding provider from config, returning None if unavailable.
    pub fn from_config(config: &EmbeddingsConfig) -> Option<Self> {
        if !config.enabled {
            debug!("Embeddings disabled in config");
            return None;
        }

        // Determine provider and API key
        let (provider_name, api_key) = if let Some(ref explicit_provider) = config.provider {
            let default_env = find_provider_meta(explicit_provider)
                .map(|m| m.env_var)
                .unwrap_or("OPENAI_API_KEY");
            let env_var = config
                .api_key_env
                .clone()
                .unwrap_or_else(|| default_env.to_string());
            match std::env::var(&env_var) {
                Ok(key) if !key.is_empty() => (explicit_provider.clone(), key),
                _ => {
                    debug!(
                        "Embeddings: explicit provider '{}' configured but {} not set",
                        explicit_provider, env_var
                    );
                    return None;
                }
            }
        } else {
            // Auto-detect: iterate EMBEDDING_PROVIDERS in priority order
            let mut found = None;
            for meta in EMBEDDING_PROVIDERS {
                if let Ok(key) = std::env::var(meta.env_var) {
                    if !key.is_empty() {
                        debug!(
                            "Embeddings: auto-detected provider '{}' via {}",
                            meta.name, meta.env_var
                        );
                        found = Some((meta.name.to_string(), key));
                        break;
                    }
                }
            }
            match found {
                Some(pair) => pair,
                None => {
                    debug!(
                        "Embeddings: no embedding-capable provider found, falling back to recency"
                    );
                    return None;
                }
            }
        };

        let meta = match find_provider_meta(&provider_name) {
            Some(m) => m,
            None => {
                debug!("Embeddings: unknown provider '{provider_name}', cannot resolve endpoint");
                return None;
            }
        };

        let model = config
            .model
            .clone()
            .unwrap_or_else(|| meta.default_model.to_string());
        let dimension = config.dimension.unwrap_or(meta.default_dim);

        Some(Self {
            endpoint: meta.endpoint.to_string(),
            api_key,
            model,
            dimension,
        })
    }
}

/// Build a fingerprint for cache invalidation when config changes.
fn config_fingerprint(config: &EmbeddingsConfig) -> String {
    format!(
        "enabled={};provider={:?};model={:?};dim={:?};key_env={:?}",
        config.enabled, config.provider, config.model, config.dimension, config.api_key_env,
    )
}

/// Get or initialize the cached embedding provider.
/// Automatically re-resolves if the config has changed since the last call.
pub fn get_or_init_provider(config: &EmbeddingsConfig) -> Option<EmbeddingProvider> {
    let fingerprint = config_fingerprint(config);
    let mut cache = PROVIDER_CACHE.lock().unwrap_or_else(|e| {
        tracing::warn!("Embedding provider cache mutex was poisoned, recovering");
        e.into_inner()
    });
    if let Some((cached_fp, cached_provider)) = &*cache {
        if *cached_fp == fingerprint {
            return cached_provider.clone();
        }
    }
    let provider = EmbeddingProvider::from_config(config);
    *cache = Some((fingerprint, provider.clone()));
    provider
}

/// Clear the cached provider so the next call to `get_or_init_provider` re-resolves.
pub fn invalidate_provider_cache() {
    let mut cache = PROVIDER_CACHE.lock().unwrap_or_else(|e| {
        tracing::warn!("Embedding provider cache mutex was poisoned, recovering");
        e.into_inner()
    });
    *cache = None;
}

/// Generate an embedding vector via OpenAI-compatible API.
#[instrument(skip_all)]
pub async fn generate_embedding(
    client: &reqwest::Client,
    provider: &EmbeddingProvider,
    text: &str,
) -> Result<Vec<f32>> {
    // Truncate to ~8000 tokens (rough char estimate)
    let truncated = if text.len() > MAX_EMBEDDING_INPUT_CHARS {
        let mut end = MAX_EMBEDDING_INPUT_CHARS;
        while !text.is_char_boundary(end) && end > 0 {
            end -= 1;
        }
        &text[..end]
    } else {
        text
    };

    let body = serde_json::json!({
        "model": provider.model,
        "input": truncated,
    });

    let resp = client
        .post(&provider.endpoint)
        .header("Authorization", format!("Bearer {}", provider.api_key))
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        bail!("Embedding API error {status}: {text}");
    }

    let json: serde_json::Value = resp.json().await?;
    let embedding = json["data"][0]["embedding"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("Invalid embedding response: missing data[0].embedding"))?
        .iter()
        .map(|v| match v.as_f64() {
            Some(f) => f as f32,
            None => {
                tracing::warn!("Non-numeric value in embedding response: {v}");
                0.0
            }
        })
        .collect();

    Ok(embedding)
}

/// Cosine similarity between two vectors.
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let mut dot = 0.0f32;
    let mut norm_a = 0.0f32;
    let mut norm_b = 0.0f32;
    for i in 0..a.len() {
        dot += a[i] * b[i];
        norm_a += a[i] * a[i];
        norm_b += b[i] * b[i];
    }
    let denom = norm_a.sqrt() * norm_b.sqrt();
    if denom == 0.0 {
        0.0
    } else {
        dot / denom
    }
}

/// Pack f32 embedding into little-endian bytes.
pub fn embedding_to_bytes(embedding: &[f32]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(embedding.len() * 4);
    for &val in embedding {
        bytes.extend_from_slice(&val.to_le_bytes());
    }
    bytes
}

/// Unpack little-endian bytes into f32 embedding.
pub fn bytes_to_embedding(bytes: &[u8]) -> Result<Vec<f32>> {
    if bytes.is_empty() {
        return Ok(Vec::new());
    }
    if bytes.len() % 4 != 0 {
        anyhow::bail!(
            "Corrupted embedding data: length {} not aligned to 4 bytes",
            bytes.len()
        );
    }
    Ok(bytes
        .chunks_exact(4)
        .map(|chunk| f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
        .collect())
}

/// SHA-256 hash of content.
pub fn content_hash(content: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// Blended score combining similarity and recency.
pub fn blended_score(similarity: f32, recency_score: f32, recency_weight: f32) -> f32 {
    (1.0 - recency_weight) * similarity + recency_weight * recency_score
}

/// Generate an embedding with cache. Checks DB cache first, falls back to API call.
#[instrument(skip_all)]
pub async fn generate_embedding_cached(
    client: &reqwest::Client,
    provider: &EmbeddingProvider,
    text: &str,
) -> Result<Vec<f32>> {
    let hash = content_hash(text);
    let db = match Database::open() {
        Ok(db) => Some(db),
        Err(e) => {
            tracing::debug!("embeddings: cache unavailable, skipping: {e}");
            None
        }
    };

    // Check cache
    if let Some(ref db) = db {
        if let Ok(Some((cached_bytes, _dim))) =
            db.get_cached_embedding(&provider.endpoint, &provider.model, &hash)
        {
            debug!("Embedding cache hit for hash {}", &hash[..8]);
            // Best-effort: update last_accessed_at so TTL pruning reflects actual usage.
            if let Err(e) = db.touch_cache_entry(&provider.endpoint, &provider.model, &hash) {
                tracing::debug!("touch_cache_entry failed: {e}");
            }
            return bytes_to_embedding(&cached_bytes);
        }
    }

    // API call
    let embedding = generate_embedding(client, provider, text).await?;

    // Store in cache
    if let Some(ref db) = db {
        let bytes = embedding_to_bytes(&embedding);
        if let Err(e) = db.cache_embedding(
            &provider.endpoint,
            &provider.model,
            &hash,
            &bytes,
            provider.dimension,
        ) {
            debug!("Failed to cache embedding: {e}");
        }
    }

    Ok(embedding)
}

/// Embed a memory file and store in the database. Skips if content unchanged.
#[instrument(skip_all)]
pub async fn embed_memory_file(
    config: &Config,
    filename: &str,
    content: &str,
    scope: &str,
) -> Result<()> {
    let provider = match get_or_init_provider(&config.memory.embeddings) {
        Some(p) => p,
        None => return Ok(()),
    };

    let hash = content_hash(content);

    // Check if embedding already exists with same hash
    let db = Database::open()?;
    if let Some(existing) = db.get_embedding(scope, filename)? {
        if existing.content_hash == hash {
            debug!("Embedding for {scope}/{filename} is up to date, skipping");
            return Ok(());
        }
    }

    let embedding = generate_embedding_cached(&EMBEDDING_CLIENT, &provider, content).await?;
    let bytes = embedding_to_bytes(&embedding);

    db.upsert_embedding(
        scope,
        filename,
        &hash,
        &bytes,
        provider.dimension,
        &provider.model,
    )?;
    debug!(
        "Stored embedding for {scope}/{filename} (dim={})",
        provider.dimension
    );
    Ok(())
}

/// Generate a query embedding for ranking. Returns (provider, embedding) or None.
#[instrument(skip_all)]
pub async fn generate_query_embedding(
    config: &Config,
    query: &str,
) -> Result<(EmbeddingProvider, Vec<f32>)> {
    let provider = match get_or_init_provider(&config.memory.embeddings) {
        Some(p) => p,
        None => bail!("No embedding provider available"),
    };
    let embedding = generate_embedding_cached(&EMBEDDING_CLIENT, &provider, query).await?;
    Ok((provider, embedding))
}

/// Rank memory files by similarity to a pre-computed query embedding.
/// Returns (filename, blended_score) sorted desc.
pub fn rank_embeddings_by_similarity(
    query_embedding: &[f32],
    scope: &str,
    recency_weight: f32,
) -> Result<Vec<(String, f32)>> {
    let db = Database::open()?;
    let stored = db.get_all_embeddings(scope)?;

    if stored.is_empty() {
        return Ok(Vec::new());
    }

    // Compute recency scores: normalize created_at to [0.0, 1.0]
    let min_time = stored.iter().map(|r| r.created_at).min().unwrap_or(0);
    let max_time = stored.iter().map(|r| r.created_at).max().unwrap_or(0);
    let time_range = (max_time - min_time) as f32;

    let mut scored: Vec<(String, f32)> = stored
        .iter()
        .map(|row| {
            let stored_emb = match bytes_to_embedding(&row.embedding) {
                Ok(emb) => emb,
                Err(_) => return (row.filename.clone(), 0.0),
            };
            let similarity = cosine_similarity(query_embedding, &stored_emb);
            let recency_score = if time_range > 0.0 {
                (row.created_at - min_time) as f32 / time_range
            } else {
                1.0
            };
            let score = blended_score(similarity, recency_score, recency_weight);
            (row.filename.clone(), score)
        })
        .collect();

    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    Ok(scored)
}

/// Convenience: rank memories by query text (generates embedding then ranks).
#[instrument(skip_all)]
pub async fn rank_memories_by_similarity(
    config: &Config,
    query: &str,
    scope: &str,
) -> Result<Vec<(String, f32)>> {
    let (_provider, query_embedding) = generate_query_embedding(config, query).await?;
    rank_embeddings_by_similarity(
        &query_embedding,
        scope,
        config.memory.embeddings.recency_weight,
    )
}

/// Embed a memory file using chunking. Chunks the content, hashes each chunk,
/// skips unchanged chunks, generates embeddings for new/changed chunks,
/// and upserts all chunks to the database.
#[instrument(skip_all)]
pub async fn embed_memory_file_chunked(
    config: &Config,
    filename: &str,
    content: &str,
    scope: &str,
) -> Result<()> {
    let provider = match get_or_init_provider(&config.memory.embeddings) {
        Some(p) => p,
        None => return Ok(()),
    };

    let chunk_size = config.memory.embeddings.chunk_size_tokens;
    let overlap = config.memory.embeddings.chunk_overlap_tokens;
    let chunks = crate::chunker::chunk_content(content, chunk_size, overlap);

    if chunks.is_empty() {
        return Ok(());
    }

    let db = Database::open()?;
    let existing = db.get_chunks_for_file(scope, filename)?;
    let existing_map: std::collections::HashMap<i64, String> = existing
        .iter()
        .map(|c| (c.chunk_index, c.content_hash.clone()))
        .collect();

    let mut chunk_data = Vec::new();
    let model = Some(provider.model.clone());

    for (i, chunk) in chunks.iter().enumerate() {
        let hash = content_hash(&chunk.content);
        let idx = i as i64;

        // Check if chunk is unchanged
        let needs_embedding = existing_map
            .get(&idx)
            .map(|existing_hash| existing_hash != &hash)
            .unwrap_or(true);

        let embedding = if needs_embedding {
            match generate_embedding_cached(&EMBEDDING_CLIENT, &provider, &chunk.content).await {
                Ok(emb) => Some(embedding_to_bytes(&emb)),
                Err(e) => {
                    debug!("Failed to embed chunk {i} of {filename}: {e}");
                    None
                }
            }
        } else {
            // Reuse existing embedding
            existing
                .iter()
                .find(|c| c.chunk_index == idx)
                .and_then(|c| c.embedding.clone())
        };

        chunk_data.push(ChunkData {
            chunk_index: idx,
            content: chunk.content.clone(),
            content_hash: hash,
            embedding,
            dimension: Some(provider.dimension),
            model: model.clone(),
            start_line: Some(chunk.start_line as i64),
            end_line: Some(chunk.end_line as i64),
        });
    }

    db.upsert_chunks(scope, filename, &chunk_data)?;
    debug!("Stored {} chunks for {scope}/{filename}", chunk_data.len());
    Ok(())
}

/// Result from hybrid memory search.
#[derive(Debug, Clone)]
pub struct SearchResult {
    /// Name of the memory file containing the match.
    pub filename: String,
    /// Index of the matched chunk within the file.
    pub chunk_index: i64,
    /// Starting line number of the chunk, if available.
    pub start_line: Option<i64>,
    /// Ending line number of the chunk, if available.
    pub end_line: Option<i64>,
    /// Blended relevance score (higher is better).
    pub score: f32,
    /// Text snippet from the matched chunk.
    pub snippet: String,
}

/// Merge vector and FTS search scores, deduplicate, and sort descending.
/// Each input is (filename, chunk_index, score).
/// Returns merged (filename, chunk_index, blended_score).
///
/// Uses adaptive weighting: when one result set is empty, the other set's
/// weight is scaled to 1.0 so scores remain meaningful. NaN/Inf scores
/// are filtered out.
pub fn merge_search_scores(
    vector_results: &[(&str, i64, f32)],
    fts_results: &[(&str, i64, f32)],
    vector_weight: f32,
    bm25_weight: f32,
) -> Vec<(String, i64, f32)> {
    use std::collections::HashMap;

    // Normalize scores to [0, 1] within each set, filtering non-finite values
    let normalize = |results: &[(&str, i64, f32)]| -> Vec<(String, i64, f32)> {
        let mut min_s = f32::INFINITY;
        let mut max_s = f32::NEG_INFINITY;
        let mut has_finite = false;
        for r in results {
            if r.2.is_finite() {
                min_s = min_s.min(r.2);
                max_s = max_s.max(r.2);
                has_finite = true;
            }
        }
        if !has_finite {
            return Vec::new();
        }
        let range = max_s - min_s;
        results
            .iter()
            .filter(|r| r.2.is_finite())
            .map(|r| {
                let norm = if range > 0.0 {
                    (r.2 - min_s) / range
                } else {
                    1.0
                };
                (r.0.to_string(), r.1, norm)
            })
            .collect()
    };

    let norm_vec = normalize(vector_results);
    let norm_fts = normalize(fts_results);

    // Adaptive weighting: when one set is empty, use the other at full weight
    let (eff_vec_w, eff_bm25_w) = match (norm_vec.is_empty(), norm_fts.is_empty()) {
        (true, true) => return Vec::new(),
        (true, false) => (0.0, 1.0),
        (false, true) => (1.0, 0.0),
        (false, false) => (vector_weight, bm25_weight),
    };

    // Merge into a map keyed by (filename, chunk_index)
    let mut scores: HashMap<(String, i64), (f32, f32)> = HashMap::new();

    for (f, ci, s) in &norm_vec {
        let entry = scores.entry((f.clone(), *ci)).or_insert((0.0, 0.0));
        entry.0 = entry.0.max(*s);
    }
    for (f, ci, s) in &norm_fts {
        let entry = scores.entry((f.clone(), *ci)).or_insert((0.0, 0.0));
        entry.1 = entry.1.max(*s);
    }

    let mut merged: Vec<(String, i64, f32)> = scores
        .into_iter()
        .map(|((f, ci), (vs, fs))| (f, ci, eff_vec_w * vs + eff_bm25_w * fs))
        .collect();

    merged.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));
    merged
}

/// Aggregate chunk-level scores to file-level scores (max chunk score per file).
pub fn aggregate_to_file_scores(chunk_scores: &[(String, i64, f32)]) -> Vec<(String, f32)> {
    use std::collections::HashMap;
    let mut file_max: HashMap<String, f32> = HashMap::new();
    for (filename, _ci, score) in chunk_scores {
        let entry = file_max.entry(filename.clone()).or_insert(0.0f32);
        *entry = entry.max(*score);
    }
    let mut result: Vec<(String, f32)> = file_max.into_iter().collect();
    result.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cosine_similarity_identical() {
        let a = vec![1.0, 2.0, 3.0];
        let b = vec![1.0, 2.0, 3.0];
        let sim = cosine_similarity(&a, &b);
        assert!((sim - 1.0).abs() < 1e-6);
    }

    #[test]
    fn cosine_similarity_orthogonal() {
        let a = vec![1.0, 0.0];
        let b = vec![0.0, 1.0];
        let sim = cosine_similarity(&a, &b);
        assert!(sim.abs() < 1e-6);
    }

    #[test]
    fn cosine_similarity_opposite() {
        let a = vec![1.0, 2.0, 3.0];
        let b = vec![-1.0, -2.0, -3.0];
        let sim = cosine_similarity(&a, &b);
        assert!((sim + 1.0).abs() < 1e-6);
    }

    #[test]
    fn cosine_similarity_empty() {
        let sim = cosine_similarity(&[], &[]);
        assert_eq!(sim, 0.0);
    }

    #[test]
    fn cosine_similarity_mismatched_lengths() {
        let sim = cosine_similarity(&[1.0, 2.0], &[1.0]);
        assert_eq!(sim, 0.0);
    }

    #[test]
    fn embedding_bytes_roundtrip() {
        let original = vec![1.0f32, -2.5, 3.14159, 0.0, f32::MIN, f32::MAX];
        let bytes = embedding_to_bytes(&original);
        let recovered = bytes_to_embedding(&bytes).unwrap();
        assert_eq!(original, recovered);
    }

    #[test]
    fn content_hash_deterministic() {
        let h1 = content_hash("hello world");
        let h2 = content_hash("hello world");
        assert_eq!(h1, h2);
    }

    #[test]
    fn content_hash_changes_with_content() {
        let h1 = content_hash("hello");
        let h2 = content_hash("world");
        assert_ne!(h1, h2);
    }

    #[test]
    fn blended_score_pure_similarity() {
        let score = blended_score(0.8, 0.5, 0.0);
        assert!((score - 0.8).abs() < 1e-6);
    }

    #[test]
    fn blended_score_pure_recency() {
        let score = blended_score(0.8, 0.5, 1.0);
        assert!((score - 0.5).abs() < 1e-6);
    }

    #[test]
    fn blended_score_balanced() {
        let score = blended_score(0.8, 0.4, 0.5);
        // 0.5 * 0.8 + 0.5 * 0.4 = 0.6
        assert!((score - 0.6).abs() < 1e-6);
    }

    #[test]
    fn blended_score_default_weight() {
        // recency_weight = 0.2
        let score = blended_score(1.0, 0.0, 0.2);
        // 0.8 * 1.0 + 0.2 * 0.0 = 0.8
        assert!((score - 0.8).abs() < 1e-6);
    }

    #[test]
    fn search_result_ordering() {
        let vector_results = vec![("file_a.md", 0i64, 0.9f32), ("file_b.md", 0, 0.3)];
        let fts_results = vec![("file_b.md", 0i64, 0.8f32), ("file_a.md", 0, 0.2)];
        let merged = merge_search_scores(&vector_results, &fts_results, 0.7, 0.3);
        assert!(merged[0].0 == "file_a.md", "file_a should rank first");
        assert!(merged[0].2 > merged[1].2, "scores should be descending");
    }

    #[test]
    fn search_result_deduplication() {
        let vector_results = vec![("file.md", 0i64, 0.8f32), ("file.md", 1, 0.5)];
        let fts_results = vec![("file.md", 0i64, 0.7f32)];
        let merged = merge_search_scores(&vector_results, &fts_results, 0.7, 0.3);
        let chunk0_entries: Vec<_> = merged
            .iter()
            .filter(|r| r.0 == "file.md" && r.1 == 0)
            .collect();
        assert_eq!(chunk0_entries.len(), 1, "should deduplicate");
    }

    #[test]
    fn search_min_score_filter() {
        let vector_results = vec![("file_a.md", 0i64, 0.9f32), ("file_b.md", 0, 0.1)];
        let fts_results: Vec<(&str, i64, f32)> = vec![];
        let merged = merge_search_scores(&vector_results, &fts_results, 0.7, 0.3);
        let filtered: Vec<_> = merged.into_iter().filter(|r| r.2 >= 0.2).collect();
        assert!(filtered.len() >= 1);
        assert!(filtered.iter().all(|r| r.2 >= 0.2));
    }

    // -- provider cache --

    #[test]
    fn get_or_init_provider_returns_none_when_disabled() {
        invalidate_provider_cache();
        let config = EmbeddingsConfig {
            enabled: false,
            ..Default::default()
        };
        assert!(get_or_init_provider(&config).is_none());
    }

    #[test]
    fn cached_provider_is_consistent() {
        invalidate_provider_cache();
        let config = EmbeddingsConfig {
            enabled: false,
            ..Default::default()
        };
        let a = get_or_init_provider(&config);
        let b = get_or_init_provider(&config);
        assert_eq!(a.is_some(), b.is_some());
    }

    #[test]
    fn invalidate_cache_allows_re_resolution() {
        invalidate_provider_cache();
        let config = EmbeddingsConfig {
            enabled: false,
            ..Default::default()
        };
        assert!(get_or_init_provider(&config).is_none());
        invalidate_provider_cache();
        // Re-resolves (still None because disabled)
        assert!(get_or_init_provider(&config).is_none());
    }

    #[test]
    fn aggregate_chunk_scores_to_file_scores() {
        let chunk_scores = vec![
            ("file_a.md".to_string(), 0i64, 0.8f32),
            ("file_a.md".to_string(), 1, 0.9),
            ("file_b.md".to_string(), 0, 0.7),
        ];
        let file_scores = aggregate_to_file_scores(&chunk_scores);
        assert_eq!(file_scores.len(), 2);
        let a_score = file_scores
            .iter()
            .find(|(f, _)| f == "file_a.md")
            .unwrap()
            .1;
        assert!((a_score - 0.9).abs() < 1e-6);
    }

    // -- EmbeddingProvider::from_config --

    #[test]
    fn from_config_disabled_returns_none() {
        let config = EmbeddingsConfig {
            enabled: false,
            provider: Some("openai".to_string()),
            ..Default::default()
        };
        assert!(EmbeddingProvider::from_config(&config).is_none());
    }

    #[test]
    fn from_config_unknown_provider_returns_none() {
        let config = EmbeddingsConfig {
            enabled: true,
            provider: Some("unknownprovider".to_string()),
            api_key_env: Some("FAKE_KEY_FOR_TEST_XYZ".to_string()),
            ..Default::default()
        };
        // Even if the env var exists, unknown provider has no endpoint
        assert!(EmbeddingProvider::from_config(&config).is_none());
    }

    #[test]
    fn from_config_no_api_key_returns_none() {
        let config = EmbeddingsConfig {
            enabled: true,
            provider: Some("openai".to_string()),
            api_key_env: Some("NONEXISTENT_EMBEDDING_KEY_ABC123".to_string()),
            ..Default::default()
        };
        assert!(EmbeddingProvider::from_config(&config).is_none());
    }

    // -- find_provider_meta --

    #[test]
    fn find_provider_meta_known_providers() {
        assert_eq!(
            find_provider_meta("openai").unwrap().env_var,
            "OPENAI_API_KEY"
        );
        assert_eq!(
            find_provider_meta("openrouter").unwrap().env_var,
            "OPENROUTER_API_KEY"
        );
        assert_eq!(
            find_provider_meta("gemini").unwrap().env_var,
            "GEMINI_API_KEY"
        );
    }

    #[test]
    fn find_provider_meta_unknown_returns_none() {
        assert!(find_provider_meta("somethingelse").is_none());
    }

    // -- config_fingerprint --

    #[test]
    fn config_fingerprint_deterministic() {
        let config = EmbeddingsConfig {
            enabled: true,
            provider: Some("openai".to_string()),
            ..Default::default()
        };
        let fp1 = config_fingerprint(&config);
        let fp2 = config_fingerprint(&config);
        assert_eq!(fp1, fp2);
    }

    #[test]
    fn config_fingerprint_changes_with_enabled() {
        let c1 = EmbeddingsConfig {
            enabled: true,
            ..Default::default()
        };
        let c2 = EmbeddingsConfig {
            enabled: false,
            ..Default::default()
        };
        assert_ne!(config_fingerprint(&c1), config_fingerprint(&c2));
    }

    // -- bytes_to_embedding edge case --

    #[test]
    fn bytes_to_embedding_unaligned_returns_empty() {
        let result = bytes_to_embedding(&[1, 2, 3]); // 3 bytes, not aligned to 4
        assert!(result.is_err());
    }

    #[test]
    fn bytes_to_embedding_empty() {
        let result = bytes_to_embedding(&[]).unwrap();
        assert!(result.is_empty());
    }

    // -- cosine_similarity edge case --

    #[test]
    fn cosine_similarity_zero_vector() {
        let a = vec![0.0, 0.0, 0.0];
        let b = vec![1.0, 2.0, 3.0];
        assert_eq!(cosine_similarity(&a, &b), 0.0);
    }

    // -- adaptive hybrid merge --

    #[test]
    fn merge_empty_vector_uses_fts_only() {
        let vector: Vec<(&str, i64, f32)> = vec![];
        let fts = vec![("file.md", 0i64, 0.8f32), ("other.md", 0, 0.4)];
        let merged = merge_search_scores(&vector, &fts, 0.7, 0.3);
        assert_eq!(merged.len(), 2);
        // With adaptive weighting, FTS weight should be 1.0
        assert!(
            merged[0].2 > 0.9,
            "top score should be ~1.0 (full FTS weight)"
        );
    }

    #[test]
    fn merge_empty_fts_uses_vector_only() {
        let vector = vec![("file.md", 0i64, 0.9f32), ("other.md", 0, 0.3)];
        let fts: Vec<(&str, i64, f32)> = vec![];
        let merged = merge_search_scores(&vector, &fts, 0.7, 0.3);
        assert_eq!(merged.len(), 2);
        assert!(
            merged[0].2 > 0.9,
            "top score should be ~1.0 (full vector weight)"
        );
    }

    #[test]
    fn merge_handles_nan_scores() {
        let vector = vec![("good.md", 0i64, 0.8f32), ("bad.md", 0, f32::NAN)];
        let fts = vec![("good.md", 0i64, 0.6f32)];
        let merged = merge_search_scores(&vector, &fts, 0.7, 0.3);
        // NaN entry should be filtered out during normalization
        assert!(merged.iter().all(|r| r.2.is_finite()));
    }

    #[test]
    fn merge_both_empty_returns_empty() {
        let vector: Vec<(&str, i64, f32)> = vec![];
        let fts: Vec<(&str, i64, f32)> = vec![];
        let merged = merge_search_scores(&vector, &fts, 0.7, 0.3);
        assert!(merged.is_empty());
    }

    #[test]
    fn bytes_to_embedding_various_unaligned() {
        for len in [1, 2, 3, 5, 7, 9, 11] {
            let data = vec![0u8; len];
            assert!(
                bytes_to_embedding(&data).is_err(),
                "length {len} should fail"
            );
        }
    }

    #[test]
    fn bytes_to_embedding_nan_inf_values() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&f32::NAN.to_le_bytes());
        bytes.extend_from_slice(&f32::INFINITY.to_le_bytes());
        bytes.extend_from_slice(&f32::NEG_INFINITY.to_le_bytes());
        bytes.extend_from_slice(&1.0f32.to_le_bytes());
        let result = bytes_to_embedding(&bytes).unwrap();
        assert_eq!(result.len(), 4);
        assert!(result[0].is_nan());
        assert!(result[1].is_infinite());
        assert!(result[2].is_infinite());
        assert!((result[3] - 1.0).abs() < 1e-6);
    }
}
