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
            // Vector threshold is halved: cosine similarity scores tend to be lower than BM25-normalized scores
            .filter(|(_f, _ci, sim)| *sim >= min_score * 0.5)
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

/// Execute hybrid memory search (FTS + vector) across global and local scopes.
///
/// `sources` selects the backing stores:
/// - `["memory"]` (default): curated long-term memory entries via `memory_chunks_fts`.
/// - `["messages"]`: raw past-session transcripts via `messages_fts` (V35).
/// - Both: query each store and interleave by score.
#[instrument(skip_all, fields(tool.name = "memory_search"))]
pub async fn handle_memory_search(args: &serde_json::Value, config: &Config) -> Result<String> {
    let query = require_str_param(args, "query")?;
    let max_results = optional_u64_param(args, "max_results", 5) as usize;
    let min_score = optional_f64_param(args, "min_score", 0.2) as f32;
    let vector_weight = config.memory.embeddings.vector_weight;
    let bm25_weight = config.memory.embeddings.bm25_weight;

    let sources = parse_sources(args);
    let search_memory = sources.contains(&"memory".to_string());
    let search_messages = sources.contains(&"messages".to_string());

    let db = Database::open()?;

    // Branch for messages-only searches: skip embedding generation (unused here)
    // and return a transcript-flavored format.
    if search_messages && !search_memory {
        return Ok(format_message_fts_hits(
            &db.messages_fts_search(query, max_results)?,
        ));
    }

    let mut all_results = Vec::new();

    // Pre-compute query embedding once for all scopes
    let query_embedding = embeddings::generate_query_embedding(config, query)
        .await
        .map(|(_prov, emb)| emb)
        .ok();

    if query_embedding.is_none() {
        tracing::debug!("memory_search: no embedding provider, falling back to FTS-only");
    }

    for scope in &["global", "local", "extra", "sessions"] {
        all_results.extend(search_scope(
            &db,
            scope,
            query,
            query_embedding.as_deref(),
            max_results,
            min_score,
            vector_weight,
            bm25_weight,
        ));
    }

    // If no results, try a looser FTS search with individual terms
    if all_results.is_empty() {
        let terms: Vec<&str> = query.split_whitespace().collect();
        if terms.len() > 1 {
            let mut seen: std::collections::HashSet<(String, i64)> =
                std::collections::HashSet::new();
            for scope in &["global", "local", "extra", "sessions"] {
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
    let memory_block = format_search_results(&all_results);

    if search_messages {
        // Combined search: append message-FTS hits after memory results.
        let hits = db.messages_fts_search(query, max_results)?;
        if !hits.is_empty() {
            let mut combined = memory_block;
            if !combined.is_empty() && combined != "No matching memories found." {
                combined.push_str("\n\n");
            } else {
                combined.clear();
            }
            combined.push_str(&format_message_fts_hits(&hits));
            return Ok(combined);
        }
    }
    Ok(memory_block)
}

/// Parse the optional `sources` array. Defaults to `["memory"]` when absent
/// or empty, preserving pre-V35 behavior. Unknown entries are ignored.
fn parse_sources(args: &serde_json::Value) -> Vec<String> {
    let values = args
        .get("sources")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str())
                .filter(|s| matches!(*s, "memory" | "messages"))
                .map(String::from)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if values.is_empty() {
        vec!["memory".to_string()]
    } else {
        values
    }
}

/// Render message FTS hits as a compact transcript view. Each hit shows
/// session_id and a content snippet so the agent can request targeted context.
fn format_message_fts_hits(hits: &[crate::db::MessageFtsHit]) -> String {
    if hits.is_empty() {
        return "No matching messages found.".to_string();
    }
    let mut out = String::from("## Session message matches\n");
    for (i, h) in hits.iter().enumerate() {
        let snippet = truncate_for_display(&h.content, 240);
        out.push_str(&format!(
            "\n{}. session={} role={} score={:.3}\n   {}\n",
            i + 1,
            h.session_id,
            h.role,
            h.score,
            snippet
        ));
    }
    out
}

fn truncate_for_display(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let cut: String = s.chars().take(max_chars).collect();
    format!("{cut}…")
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
    fn parse_sources_defaults_to_memory_when_absent() {
        let args = serde_json::json!({"query": "x"});
        assert_eq!(parse_sources(&args), vec!["memory".to_string()]);
    }

    #[test]
    fn parse_sources_defaults_to_memory_when_empty_array() {
        let args = serde_json::json!({"query": "x", "sources": []});
        assert_eq!(parse_sources(&args), vec!["memory".to_string()]);
    }

    #[test]
    fn parse_sources_accepts_messages() {
        let args = serde_json::json!({"query": "x", "sources": ["messages"]});
        assert_eq!(parse_sources(&args), vec!["messages".to_string()]);
    }

    #[test]
    fn parse_sources_accepts_both_and_preserves_order() {
        let args = serde_json::json!({"query": "x", "sources": ["memory", "messages"]});
        let got = parse_sources(&args);
        assert!(got.contains(&"memory".to_string()));
        assert!(got.contains(&"messages".to_string()));
    }

    #[test]
    fn parse_sources_ignores_unknown_values() {
        // Unknown source strings must not leak past the parser — otherwise the
        // dispatch branches below would silently query nothing.
        let args = serde_json::json!({"query": "x", "sources": ["garbage", "messages", 42]});
        assert_eq!(parse_sources(&args), vec!["messages".to_string()]);
    }

    #[test]
    fn format_message_fts_hits_renders_session_ids_and_snippets() {
        let hits = vec![
            crate::db::MessageFtsHit {
                id: 1,
                session_id: "abc".into(),
                role: "user".into(),
                content: "deploy the frontend".into(),
                created_at: 0,
                score: 1.23,
            },
            crate::db::MessageFtsHit {
                id: 2,
                session_id: "xyz".into(),
                role: "assistant".into(),
                content: "deployed successfully".into(),
                created_at: 0,
                score: 0.87,
            },
        ];
        let out = format_message_fts_hits(&hits);
        assert!(out.contains("abc"), "first session_id must be rendered");
        assert!(out.contains("xyz"), "second session_id must be rendered");
        assert!(out.contains("deploy"));
        assert!(out.contains("1.230") || out.contains("1.23"));
    }

    #[test]
    fn format_message_fts_hits_empty() {
        let out = format_message_fts_hits(&[]);
        assert!(out.contains("No matching messages"));
    }

    #[test]
    fn truncate_for_display_keeps_short_strings() {
        assert_eq!(truncate_for_display("hello", 240), "hello");
    }

    #[test]
    fn truncate_for_display_adds_ellipsis_when_long() {
        let long = "a".repeat(300);
        let out = truncate_for_display(&long, 240);
        assert_eq!(out.chars().count(), 241);
        assert!(out.ends_with('…'));
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
        // `.md` is stripped before storage (backward compat with old tool calls),
        // so the confirmation message echoes the bare entry name.
        assert!(msg.contains("note"), "got: {msg}");

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
}
