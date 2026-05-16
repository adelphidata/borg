//! Embedding-cache TTL constant and embedding repair backlog.
//!
//! The embedding repair pass walks every memory scope, identifies entries
//! whose chunk rows are missing or stale, and re-embeds them — bounded by
//! `maintenance.embedding_repair_max_files` and
//! `maintenance.embedding_repair_max_chunks`.

use anyhow::Result;

use crate::config::Config;
use crate::constants::SECS_PER_DAY;
use crate::db::Database;

/// How long an embedding-cache entry can sit unaccessed before the
/// maintenance sweep evicts it. Mirrors the 30-day default elsewhere.
pub(super) const EMBEDDING_CACHE_TTL_SECS: i64 = 30 * SECS_PER_DAY;

#[derive(Debug, Default)]
pub(super) struct EmbeddingRepairReport {
    pub(super) repaired_files: usize,
    pub(super) repaired_chunks: usize,
    pub(super) failed: bool,
    pub(super) fixed_issue_keys: Vec<String>,
}

pub(super) async fn embedding_scopes(db: &Database, config: &Config) -> Vec<String> {
    let mut scopes = crate::memory::resolved_memory_scopes(config);
    if let Ok(mut db_scopes) = db.list_memory_scopes().await {
        scopes.append(&mut db_scopes);
    }
    scopes.sort();
    scopes.dedup();
    scopes
}

pub(super) fn count_stale_chunks(
    expected_chunks: &[crate::memory::chunker::Chunk],
    existing_chunks: &[crate::db::ChunkRow],
) -> usize {
    if expected_chunks.is_empty() {
        return 0;
    }
    expected_chunks
        .iter()
        .enumerate()
        .filter(|(idx, expected)| {
            existing_chunks.get(*idx).is_none_or(|existing| {
                existing.content_hash != crate::memory::embeddings::content_hash(&expected.content)
                    || existing.embedding.is_none()
            })
        })
        .count()
}

pub(super) async fn repair_embedding_backlog(
    db: &Database,
    config: &Config,
) -> Result<EmbeddingRepairReport> {
    let mut report = EmbeddingRepairReport::default();
    if !config.maintenance.embedding_repair_enabled || !config.memory.embeddings.enabled {
        return Ok(report);
    }
    if crate::memory::embeddings::get_or_init_provider(&config.memory.embeddings).is_none() {
        report.failed = true;
        return Ok(report);
    }

    let max_files = config.maintenance.embedding_repair_max_files as usize;
    let max_chunks = config.maintenance.embedding_repair_max_chunks as usize;
    let scopes = embedding_scopes(db, config).await;

    for scope in scopes {
        let entries = db.list_memory_entries(&scope).await?;
        for entry in entries {
            if report.repaired_files >= max_files {
                return Ok(report);
            }
            let whole_missing = db
                .get_embedding(&scope, &entry.name)
                .await?
                .is_none_or(|row| row.content_hash != entry.content_hash);
            let expected_chunks = crate::memory::chunker::chunk_content(
                &entry.content,
                config.memory.embeddings.chunk_size_tokens,
                config.memory.embeddings.chunk_overlap_tokens,
            );
            let existing_chunks = db.get_chunks_for_file(&scope, &entry.name).await?;
            let stale_chunk_count = count_stale_chunks(&expected_chunks, &existing_chunks);
            let chunk_missing = expected_chunks.is_empty()
                || existing_chunks.len() != expected_chunks.len()
                || stale_chunk_count > 0;
            if !whole_missing && !chunk_missing {
                continue;
            }
            if chunk_missing {
                let needed_chunks = stale_chunk_count;
                if report.repaired_chunks + needed_chunks > max_chunks {
                    continue;
                }
            }

            if whole_missing {
                crate::memory::embeddings::embed_memory_file(
                    config,
                    &entry.name,
                    &entry.content,
                    &scope,
                )
                .await?;
            }
            if chunk_missing {
                crate::memory::embeddings::embed_memory_file_chunked(
                    config,
                    &entry.name,
                    &entry.content,
                    &scope,
                )
                .await?;
            }

            report.repaired_files += 1;
            if chunk_missing {
                report.repaired_chunks += stale_chunk_count;
                report
                    .fixed_issue_keys
                    .push("Embeddings:chunk coverage".to_string());
            }
            if whole_missing {
                report
                    .fixed_issue_keys
                    .push("Embeddings:whole-entry coverage".to_string());
            }
        }
    }
    Ok(report)
}

#[cfg(all(test, feature = "phase9-tests-disabled"))]
mod tests {
    use super::*;

    #[test]
    fn repair_embedding_backlog_no_provider_is_bounded_failure() {
        let db = crate::db::Database::test_db();
        let mut config = Config::default();
        config.memory.embeddings.enabled = true;
        config.memory.embeddings.provider = Some("openai".into());
        config.memory.embeddings.api_key_env = Some("BORG_TEST_MISSING_EMBED_KEY".into());
        std::env::remove_var("BORG_TEST_MISSING_EMBED_KEY");

        let report = repair_embedding_backlog(&db, &config).expect("repair");
        assert!(report.failed, "missing provider creds should skip repair");
        assert_eq!(report.repaired_files, 0);
        assert_eq!(report.repaired_chunks, 0);
    }

    #[test]
    fn count_stale_chunks_only_counts_missing_or_stale_entries() {
        let expected_chunks = vec![
            crate::memory::chunker::Chunk {
                content: "alpha".into(),
                start_line: 1,
                end_line: 1,
            },
            crate::memory::chunker::Chunk {
                content: "beta".into(),
                start_line: 2,
                end_line: 2,
            },
            crate::memory::chunker::Chunk {
                content: "gamma".into(),
                start_line: 3,
                end_line: 3,
            },
        ];
        let existing_chunks = vec![
            crate::db::ChunkRow {
                id: 1,
                scope: "global".into(),
                filename: "doc".into(),
                chunk_index: 0,
                start_line: Some(1),
                end_line: Some(1),
                content: "alpha".into(),
                content_hash: crate::memory::embeddings::content_hash("alpha"),
                embedding: Some(vec![1, 2, 3]),
                created_at: 0,
            },
            crate::db::ChunkRow {
                id: 2,
                scope: "global".into(),
                filename: "doc".into(),
                chunk_index: 1,
                start_line: Some(2),
                end_line: Some(2),
                content: "beta".into(),
                content_hash: crate::memory::embeddings::content_hash("beta"),
                embedding: None,
                created_at: 0,
            },
        ];

        assert_eq!(
            count_stale_chunks(&expected_chunks, &existing_chunks),
            2,
            "only the missing tail chunk and the stale middle chunk should count against the budget"
        );
    }
}
