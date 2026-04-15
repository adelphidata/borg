use anyhow::Result;
use tracing::instrument;

use crate::config::Config;
use crate::db::Database;
use crate::embeddings;
use crate::memory::{read_memory_db_or_not_found, write_memory_db, WriteMode};
use crate::mmr;

use super::{
    optional_bool_param, optional_f64_param, optional_str_param, optional_u64_param,
    require_str_param,
};

pub fn handle_write_memory(args: &serde_json::Value) -> Result<String> {
    let filename = require_str_param(args, "filename")?;
    let content = require_str_param(args, "content")?;
    let mode = if optional_bool_param(args, "append", false) {
        WriteMode::Append
    } else {
        WriteMode::Overwrite
    };
    let scope = optional_str_param(args, "scope").unwrap_or("global");
    // Strip .md extension for DB entry names (backward compat with old tool calls)
    let name = filename.strip_suffix(".md").unwrap_or(filename);
    write_memory_db(name, content, mode, scope)
}

pub fn handle_read_memory(args: &serde_json::Value) -> Result<String> {
    let filename = require_str_param(args, "filename")?;
    let name = filename.strip_suffix(".md").unwrap_or(filename);
    let scope = optional_str_param(args, "scope").unwrap_or("global");
    read_memory_db_or_not_found(name, scope)
}

/// Chunk metadata: (snippet, start_line, end_line).
type ChunkMeta<'a> = std::collections::HashMap<(String, i64), (&'a str, Option<i64>, Option<i64>)>;

/// Run hybrid FTS + vector search for a single scope, returning merged results.
#[allow(clippy::too_many_arguments)]
fn search_scope(
    db: &Database,
    scope: &str,
    query: &str,
    query_embedding: Option<&[f32]>,
    max_results: usize,
    min_score: f32,
    vector_weight: f32,
    bm25_weight: f32,
    vector_threshold_factor: f32,
) -> Vec<embeddings::SearchResult> {
    // FTS search
    let fts_rows = match db.fts_search(scope, query, max_results * 4) {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!("FTS search failed for scope {scope}: {e}");
            return Vec::new();
        }
    };
    let fts_owned: Vec<(String, i64, f32)> = fts_rows
        .iter()
        .map(|(c, score)| (c.filename.clone(), c.chunk_index, *score))
        .collect();

    let fts_meta: ChunkMeta<'_> = fts_rows
        .iter()
        .map(|(c, _)| {
            (
                (c.filename.clone(), c.chunk_index),
                (c.content.as_str(), c.start_line, c.end_line),
            )
        })
        .collect();

    // Vector search across chunks
    let chunks = match db.get_all_chunks(scope, None) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("Chunk retrieval failed for scope {scope}: {e}");
            return Vec::new();
        }
    };
    let vec_owned: Vec<(String, i64, f32)> = if let Some(query_emb) = query_embedding {
        chunks
            .iter()
            .filter_map(|c| {
                c.embedding.as_ref().and_then(|emb_bytes| {
                    let Ok(stored) = embeddings::bytes_to_embedding(emb_bytes) else {
                        return None;
                    };
                    let sim = embeddings::cosine_similarity(query_emb, &stored);
                    Some((c.filename.clone(), c.chunk_index, sim))
                })
            })
            .filter(|(_f, _ci, sim)| *sim >= min_score * vector_threshold_factor)
            .collect()
    } else {
        Vec::new()
    };

    let chunk_meta: ChunkMeta<'_> = chunks
        .iter()
        .map(|c| {
            (
                (c.filename.clone(), c.chunk_index),
                (c.content.as_str(), c.start_line, c.end_line),
            )
        })
        .collect();

    // Merge hybrid scores
    let fts_refs: Vec<(&str, i64, f32)> = fts_owned
        .iter()
        .map(|(f, ci, s)| (f.as_str(), *ci, *s))
        .collect();
    let vec_refs: Vec<(&str, i64, f32)> = vec_owned
        .iter()
        .map(|(f, ci, s)| (f.as_str(), *ci, *s))
        .collect();
    let merged = embeddings::merge_search_scores(&vec_refs, &fts_refs, vector_weight, bm25_weight);

    merged
        .into_iter()
        .filter(|(_filename, _chunk_index, score)| *score >= min_score)
        .map(|(filename, chunk_index, score)| {
            let key = (filename.clone(), chunk_index);
            let (snippet, start_line, end_line) = fts_meta
                .get(&key)
                .or_else(|| chunk_meta.get(&key))
                .map(|(s, sl, el)| (s.to_string(), *sl, *el))
                .unwrap_or_default();
            embeddings::SearchResult {
                filename,
                chunk_index,
                start_line,
                end_line,
                score,
                snippet,
            }
        })
        .collect()
}

/// Default scope set used when the DB contains no memory entries yet.
const DEFAULT_SEARCH_SCOPES: &[&str] = &["global", "local", "extra", "sessions"];

/// Resolve the set of scopes to search. Prefers the scopes that actually have
/// entries in the DB (so custom scopes written via `write_memory scope="foo"`
/// surface through search) and falls back to the default list when the DB is
/// empty or the query fails.
fn resolve_search_scopes(db: &Database) -> Vec<String> {
    match db.list_memory_scopes() {
        Ok(scopes) if !scopes.is_empty() => scopes,
        Ok(_) => DEFAULT_SEARCH_SCOPES
            .iter()
            .map(ToString::to_string)
            .collect(),
        Err(e) => {
            tracing::warn!("list_memory_scopes failed, falling back to defaults: {e}");
            DEFAULT_SEARCH_SCOPES
                .iter()
                .map(ToString::to_string)
                .collect()
        }
    }
}

/// Execute hybrid memory search (FTS + vector) across all known scopes.
#[instrument(skip_all, fields(tool.name = "memory_search"))]
pub async fn handle_memory_search(args: &serde_json::Value, config: &Config) -> Result<String> {
    let query = require_str_param(args, "query")?;
    let max_results = optional_u64_param(args, "max_results", 5) as usize;
    let min_score = optional_f64_param(args, "min_score", 0.2) as f32;
    let vector_weight = config.memory.embeddings.vector_weight;
    let bm25_weight = config.memory.embeddings.bm25_weight;
    let vector_threshold_factor = config.memory.embeddings.vector_threshold_factor;
    let db = Database::open()?;
    let mut all_results = Vec::new();

    // Pre-compute query embedding once for all scopes
    let query_embedding = embeddings::generate_query_embedding(config, query)
        .await
        .map(|(_prov, emb)| emb)
        .ok();

    if query_embedding.is_none() {
        tracing::debug!("memory_search: no embedding provider, falling back to FTS-only");
    }

    let scopes = resolve_search_scopes(&db);
    for scope in &scopes {
        all_results.extend(search_scope(
            &db,
            scope,
            query,
            query_embedding.as_deref(),
            max_results,
            min_score,
            vector_weight,
            bm25_weight,
            vector_threshold_factor,
        ));
    }

    // If no results, try a looser FTS search with individual terms
    if all_results.is_empty() {
        let terms: Vec<&str> = query.split_whitespace().collect();
        if terms.len() > 1 {
            let mut seen: std::collections::HashSet<(String, i64)> =
                std::collections::HashSet::new();
            for scope in &scopes {
                for term in &terms {
                    let fts_rows = match db.fts_search(scope, term, max_results) {
                        Ok(rows) => rows,
                        Err(e) => {
                            tracing::warn!("FTS fallback search failed for term '{term}' in scope {scope}: {e}");
                            continue;
                        }
                    };
                    for (c, score) in fts_rows {
                        // Discount individual term scores so they rank below phrase matches
                        const TERM_FALLBACK_DISCOUNT: f32 = 0.7;
                        let adjusted = score * TERM_FALLBACK_DISCOUNT;
                        let key = (c.filename.clone(), c.chunk_index);
                        if adjusted >= min_score && seen.insert(key) {
                            all_results.push(embeddings::SearchResult {
                                filename: c.filename,
                                chunk_index: c.chunk_index,
                                start_line: c.start_line,
                                end_line: c.end_line,
                                score: adjusted,
                                snippet: c.content,
                            });
                        }
                    }
                }
            }
        }
    }

    all_results.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    // Pre-truncate before MMR to limit O(n^2) work
    all_results.truncate(max_results * 3);

    // Apply MMR diversity re-ranking if enabled
    if config.memory.embeddings.mmr_enabled && all_results.len() > 1 {
        let items: Vec<(usize, f32, &str)> = all_results
            .iter()
            .enumerate()
            .map(|(i, r)| (i, r.score, r.snippet.as_str()))
            .collect();
        let reordered = mmr::mmr_rerank(&items, config.memory.embeddings.mmr_lambda, max_results);
        let original = all_results.clone();
        all_results = reordered.into_iter().map(|i| original[i].clone()).collect();
    }

    all_results.truncate(max_results);
    Ok(format_search_results(&all_results))
}

/// Format search results for display.
pub fn format_search_results(results: &[embeddings::SearchResult]) -> String {
    if results.is_empty() {
        return "No matching memories found.".to_string();
    }
    let mut output = String::new();
    for (i, r) in results.iter().enumerate() {
        let lines = match (r.start_line, r.end_line) {
            (Some(s), Some(e)) => format!("lines {s}-{e}, "),
            _ => String::new(),
        };
        output.push_str(&format!(
            "[{}] {} ({lines}score: {:.2})\n> {}\n\n",
            i + 1,
            r.filename,
            r.score,
            r.snippet.chars().take(500).collect::<String>()
        ));
    }
    output.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_memory_search_results() {
        let results = vec![
            embeddings::SearchResult {
                filename: "notes.md".into(),
                chunk_index: 0,
                start_line: Some(1),
                end_line: Some(10),
                score: 0.87,
                snippet: "Important decision about architecture".into(),
            },
            embeddings::SearchResult {
                filename: "daily/2026-03-19.md".into(),
                chunk_index: 2,
                start_line: Some(15),
                end_line: Some(22),
                score: 0.65,
                snippet: "Met with team about API design".into(),
            },
        ];
        let output = format_search_results(&results);
        assert!(output.contains("[1]"));
        assert!(output.contains("notes.md"));
        assert!(output.contains("0.87"));
        assert!(output.contains("Important decision"));
        assert!(output.contains("[2]"));
        assert!(output.contains("daily/2026-03-19.md"));
    }

    #[test]
    fn format_empty_search_results() {
        let results: Vec<embeddings::SearchResult> = vec![];
        let output = format_search_results(&results);
        assert!(output.contains("No matching memories found"));
    }

    #[test]
    fn handle_write_memory_missing_filename_errors() {
        let args = serde_json::json!({"content": "hi"});
        let err = handle_write_memory(&args).unwrap_err().to_string();
        assert!(err.contains("filename"), "got: {err}");
    }

    #[test]
    fn handle_write_memory_missing_content_errors() {
        let args = serde_json::json!({"filename": "x.md"});
        let err = handle_write_memory(&args).unwrap_err().to_string();
        assert!(err.contains("content"), "got: {err}");
    }

    #[test]
    fn handle_read_memory_missing_filename_errors() {
        let args = serde_json::json!({});
        let err = handle_read_memory(&args).unwrap_err().to_string();
        assert!(err.contains("filename"), "got: {err}");
    }

    /// Combined lifecycle test: uses BORG_DATA_DIR, so kept as a single test to
    /// avoid env-var races with other tests in this crate.
    #[test]
    fn handle_write_and_read_roundtrip_global() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::env::set_var("BORG_DATA_DIR", tmp.path());

        let args = serde_json::json!({
            "filename": "note.md",
            "content": "alpha",
        });
        let msg = handle_write_memory(&args).expect("write global");
        // handle_write_memory strips the .md suffix for DB entry names, so the
        // success message echoes the stripped name.
        assert!(msg.contains("note"));

        let read =
            handle_read_memory(&serde_json::json!({"filename": "note.md"})).expect("read global");
        assert_eq!(read.trim(), "alpha");

        // Append mode accumulates.
        handle_write_memory(&serde_json::json!({
            "filename": "note.md",
            "content": "beta",
            "append": true,
        }))
        .expect("append global");
        let read2 = handle_read_memory(&serde_json::json!({"filename": "note.md"}))
            .expect("read after append");
        assert!(read2.contains("alpha"));
        assert!(read2.contains("beta"));

        // Reading a missing file returns a friendly "not found" message rather
        // than an error.
        let missing = handle_read_memory(&serde_json::json!({"filename": "missing.md"}))
            .expect("read missing");
        assert!(missing.contains("not found"), "got: {missing}");

        std::env::remove_var("BORG_DATA_DIR");
    }

    // ── F4: vector_threshold_factor ──

    #[test]
    fn embeddings_config_default_threshold_factor() {
        let cfg = crate::config::media::EmbeddingsConfig::default();
        assert!(
            (cfg.vector_threshold_factor - 0.5).abs() < f32::EPSILON,
            "default factor should be 0.5"
        );
    }

    #[test]
    fn vector_threshold_factor_changes_filter_results() {
        use crate::db::ChunkData;
        // Three chunks with cosine similarities 0.9, 0.4, 0.2 against the
        // query. With a strict factor the weakest two get pre-filtered out,
        // so the middle chunk never participates in min-max normalization.
        // With a loose factor all three enter the pre-filter, normalization
        // spreads them across [0,1], and the middle chunk survives the final
        // `min_score` gate. This exercises the exact behavior the 0.5 magic
        // number controlled prior to F4.
        let db = Database::test_db();
        let query_emb = vec![1.0f32, 0.0];
        let chunks = vec![
            ChunkData {
                chunk_index: 0,
                start_line: None,
                end_line: None,
                content: "alpha".into(),
                content_hash: "ha".into(),
                embedding: Some(embeddings::embedding_to_bytes(&[0.9, 0.436])),
                dimension: Some(2),
                model: Some("test".into()),
            },
            ChunkData {
                chunk_index: 1,
                start_line: None,
                end_line: None,
                content: "beta".into(),
                content_hash: "hb".into(),
                embedding: Some(embeddings::embedding_to_bytes(&[0.4, 0.917])),
                dimension: Some(2),
                model: Some("test".into()),
            },
            ChunkData {
                chunk_index: 2,
                start_line: None,
                end_line: None,
                content: "gamma".into(),
                content_hash: "hc".into(),
                embedding: Some(embeddings::embedding_to_bytes(&[0.2, 0.980])),
                dimension: Some(2),
                model: Some("test".into()),
            },
        ];
        db.upsert_chunks("global", "e.md", &chunks).unwrap();

        let run = |factor: f32| -> std::collections::HashSet<i64> {
            search_scope(
                &db,
                "global",
                "zzzzzzz", // no FTS match → vector-only path
                Some(&query_emb),
                10,
                0.1,
                0.0,
                1.0,
                factor,
            )
            .iter()
            .map(|r| r.chunk_index)
            .collect()
        };

        let strict = run(3.0);
        let loose = run(0.1);
        assert_eq!(
            strict,
            [0].into_iter().collect(),
            "strict factor must leave only the strongest match: got {strict:?}"
        );
        assert!(
            loose.contains(&0) && loose.contains(&1),
            "loose factor must surface the middle chunk too: got {loose:?}"
        );
    }

    // ── F6: scope auto-discovery ──

    #[test]
    fn resolve_search_scopes_returns_defaults_for_empty_db() {
        let db = Database::test_db();
        let scopes = resolve_search_scopes(&db);
        assert_eq!(
            scopes,
            DEFAULT_SEARCH_SCOPES
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn resolve_search_scopes_surfaces_custom_scope() {
        let db = Database::test_db();
        db.upsert_memory_entry("project:abc", "notes", "hello")
            .unwrap();
        db.upsert_memory_entry("global", "INDEX", "- idx").unwrap();
        let scopes = resolve_search_scopes(&db);
        // Custom scope must be present so a later search hits it.
        assert!(scopes.contains(&"project:abc".to_string()));
        assert!(scopes.contains(&"global".to_string()));
        // And the default hard-coded list must NOT be applied over the top —
        // if the DB has scopes, that's what we use.
        assert!(!scopes.contains(&"sessions".to_string()));
    }
}
