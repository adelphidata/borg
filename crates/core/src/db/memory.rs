use anyhow::Result;
use rusqlite::{params, OptionalExtension, TransactionBehavior};

use super::models::{ChunkData, ChunkRow, EmbeddingRow};
use super::Database;

impl Database {
    // ── Embedding Cache CRUD ──

    pub fn get_cached_embedding(
        &self,
        provider: &str,
        model: &str,
        content_hash: &str,
    ) -> Result<Option<(Vec<u8>, usize)>> {
        let mut stmt = self.conn.prepare(
            "SELECT embedding, dimension FROM embedding_cache
             WHERE provider = ?1 AND model = ?2 AND content_hash = ?3",
        )?;
        let result = stmt
            .query_row(params![provider, model, content_hash], |row| {
                let embedding: Vec<u8> = row.get(0)?;
                let dimension: usize = row.get(1)?;
                Ok((embedding, dimension))
            })
            .optional()?;
        Ok(result)
    }

    /// Cache an embedding result.
    pub fn cache_embedding(
        &self,
        provider: &str,
        model: &str,
        content_hash: &str,
        embedding: &[u8],
        dimension: usize,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO embedding_cache
             (provider, model, content_hash, embedding, dimension)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![provider, model, content_hash, embedding, dimension],
        )?;
        Ok(())
    }

    /// Clear all cached embeddings. Returns the number of rows deleted.
    pub fn clear_embedding_cache(&self) -> Result<usize> {
        let count = self.conn.execute("DELETE FROM embedding_cache", [])?;
        Ok(count)
    }

    // ── Session Index Status CRUD ──

    /// Check if a session has been indexed.
    pub fn is_session_indexed(&self, session_id: &str) -> Result<bool> {
        let mut stmt = self
            .conn
            .prepare("SELECT 1 FROM session_index_status WHERE session_id = ?1")?;
        Ok(stmt.exists(params![session_id])?)
    }

    /// Mark a session as indexed.
    pub fn mark_session_indexed(&self, session_id: &str, message_count: usize) -> Result<()> {
        let now = chrono::Utc::now().timestamp();
        self.conn.execute(
            "INSERT OR REPLACE INTO session_index_status
             (session_id, indexed_at, message_count)
             VALUES (?1, ?2, ?3)",
            params![session_id, now, message_count as i64],
        )?;
        Ok(())
    }

    /// Get session IDs that haven't been indexed yet.
    pub fn get_unindexed_sessions(&self, limit: usize) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT s.id FROM sessions s
             LEFT JOIN session_index_status si ON s.id = si.session_id
             WHERE si.session_id IS NULL
             ORDER BY s.updated_at DESC
             LIMIT ?1",
        )?;
        let rows = stmt
            .query_map(params![limit as i64], |row| row.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    // ── Embedding CRUD ──

    pub fn upsert_embedding(
        &self,
        scope: &str,
        filename: &str,
        content_hash: &str,
        embedding: &[u8],
        dimension: usize,
        model: &str,
    ) -> Result<()> {
        let now = chrono::Utc::now().timestamp();
        self.conn.execute(
            "INSERT INTO memory_embeddings (scope, filename, content_hash, embedding, dimension, model, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(scope, filename) DO UPDATE SET
                content_hash = ?3, embedding = ?4, dimension = ?5, model = ?6, created_at = ?7",
            params![scope, filename, content_hash, embedding, dimension as i64, model, now],
        )?;
        Ok(())
    }

    pub fn get_embedding(&self, scope: &str, filename: &str) -> Result<Option<EmbeddingRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, scope, filename, content_hash, embedding, dimension, model, created_at
             FROM memory_embeddings WHERE scope = ?1 AND filename = ?2",
        )?;
        let row = stmt
            .query_row(params![scope, filename], |row| {
                Ok(EmbeddingRow {
                    id: row.get(0)?,
                    scope: row.get(1)?,
                    filename: row.get(2)?,
                    content_hash: row.get(3)?,
                    embedding: row.get(4)?,
                    dimension: row.get::<_, i64>(5)? as usize,
                    model: row.get(6)?,
                    created_at: row.get(7)?,
                })
            })
            .optional()?;
        Ok(row)
    }

    pub fn get_all_embeddings(&self, scope: &str) -> Result<Vec<EmbeddingRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, scope, filename, content_hash, embedding, dimension, model, created_at
             FROM memory_embeddings WHERE scope = ?1",
        )?;
        let rows = stmt
            .query_map(params![scope], |row| {
                Ok(EmbeddingRow {
                    id: row.get(0)?,
                    scope: row.get(1)?,
                    filename: row.get(2)?,
                    content_hash: row.get(3)?,
                    embedding: row.get(4)?,
                    dimension: row.get::<_, i64>(5)? as usize,
                    model: row.get(6)?,
                    created_at: row.get(7)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn delete_embedding(&self, scope: &str, filename: &str) -> Result<bool> {
        let count = self.conn.execute(
            "DELETE FROM memory_embeddings WHERE scope = ?1 AND filename = ?2",
            params![scope, filename],
        )?;
        Ok(count > 0)
    }

    pub fn count_embeddings(&self, scope: &str) -> Result<usize> {
        let mut stmt = self
            .conn
            .prepare("SELECT COUNT(*) FROM memory_embeddings WHERE scope = ?1")?;
        let count: i64 = stmt.query_row(params![scope], |row| row.get(0))?;
        Ok(count as usize)
    }

    // ── Chunk CRUD ──

    /// Upsert a set of chunks for a given scope+filename, replacing any existing chunks for that file.
    ///
    /// Uses `unchecked_transaction()` because `transaction()` requires `&mut self`.
    /// **Safety invariant:** this function must NOT be called from within another transaction,
    /// as nesting `unchecked_transaction()` calls leads to silent data-loss or locking errors.
    pub fn upsert_chunks(&self, scope: &str, filename: &str, chunks: &[ChunkData]) -> Result<()> {
        debug_assert!(
            self.conn.is_autocommit(),
            "upsert_chunks must not be called from within another transaction"
        );
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        tx.execute(
            "DELETE FROM memory_chunks WHERE scope = ?1 AND filename = ?2",
            params![scope, filename],
        )?;
        let now = chrono::Utc::now().timestamp();
        let mut stmt = tx.prepare_cached(
            "INSERT INTO memory_chunks
                (scope, filename, chunk_index, start_line, end_line, content, content_hash, embedding, dimension, model, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
             ON CONFLICT(scope, filename, chunk_index) DO UPDATE SET
                start_line = ?4, end_line = ?5, content = ?6, content_hash = ?7,
                embedding = ?8, dimension = ?9, model = ?10, created_at = ?11",
        )?;
        for chunk in chunks {
            stmt.execute(params![
                scope,
                filename,
                chunk.chunk_index,
                chunk.start_line,
                chunk.end_line,
                chunk.content,
                chunk.content_hash,
                chunk.embedding,
                chunk.dimension.map(|d| d as i64),
                chunk.model,
                now
            ])?;
        }
        drop(stmt);
        tx.commit()?;
        Ok(())
    }

    /// Retrieve all chunks for a given scope, optionally limited to `limit` rows.
    pub fn get_all_chunks(&self, scope: &str, limit: Option<usize>) -> Result<Vec<ChunkRow>> {
        let map_row = |row: &rusqlite::Row<'_>| -> rusqlite::Result<ChunkRow> {
            Ok(ChunkRow {
                id: row.get(0)?,
                scope: row.get(1)?,
                filename: row.get(2)?,
                chunk_index: row.get(3)?,
                start_line: row.get(4)?,
                end_line: row.get(5)?,
                content: row.get(6)?,
                content_hash: row.get(7)?,
                embedding: row.get(8)?,
                created_at: row.get(9)?,
            })
        };
        let base = "SELECT id, scope, filename, chunk_index, start_line, end_line, content, content_hash, embedding, created_at
                     FROM memory_chunks WHERE scope = ?1 ORDER BY filename, chunk_index";
        if let Some(n) = limit {
            let query = format!("{base} LIMIT ?2");
            let mut stmt = self.conn.prepare(&query)?;
            let rows = stmt
                .query_map(params![scope, n as i64], map_row)?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok(rows)
        } else {
            let mut stmt = self.conn.prepare(base)?;
            let rows = stmt
                .query_map(params![scope], map_row)?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok(rows)
        }
    }

    /// Delete all chunks for a specific file.
    pub fn delete_chunks_for_file(&self, scope: &str, filename: &str) -> Result<bool> {
        let count = self.conn.execute(
            "DELETE FROM memory_chunks WHERE scope = ?1 AND filename = ?2",
            params![scope, filename],
        )?;
        Ok(count > 0)
    }

    /// Retrieve all chunks for a specific file in a scope.
    pub fn get_chunks_for_file(&self, scope: &str, filename: &str) -> Result<Vec<ChunkRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, scope, filename, chunk_index, start_line, end_line, content, content_hash, embedding, created_at
             FROM memory_chunks WHERE scope = ?1 AND filename = ?2 ORDER BY chunk_index",
        )?;
        let rows = stmt
            .query_map(params![scope, filename], |row| {
                Ok(ChunkRow {
                    id: row.get(0)?,
                    scope: row.get(1)?,
                    filename: row.get(2)?,
                    chunk_index: row.get(3)?,
                    start_line: row.get(4)?,
                    end_line: row.get(5)?,
                    content: row.get(6)?,
                    content_hash: row.get(7)?,
                    embedding: row.get(8)?,
                    created_at: row.get(9)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Sanitize a query string for FTS5 MATCH syntax.
    /// Wraps each word in double quotes to prevent FTS5 operator injection.
    fn sanitize_fts_query(query: &str) -> String {
        query
            .split_whitespace()
            .filter(|w| !w.is_empty())
            .map(|w| format!("\"{}\"", w.replace('"', "")))
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// Full-text search over chunk content within a scope.
    /// Returns matching (ChunkRow, bm25_score) pairs sorted by relevance, limited to `limit`.
    pub fn fts_search(
        &self,
        scope: &str,
        query: &str,
        limit: usize,
    ) -> Result<Vec<(ChunkRow, f32)>> {
        let sanitized = Self::sanitize_fts_query(query);
        if sanitized.is_empty() {
            return Ok(Vec::new());
        }
        let mut stmt = self.conn.prepare(
            "SELECT mc.id, mc.scope, mc.filename, mc.chunk_index, mc.start_line, mc.end_line,
                    mc.content, mc.content_hash, mc.embedding, mc.created_at,
                    -bm25(memory_chunks_fts) AS score
             FROM memory_chunks_fts
             JOIN memory_chunks mc ON mc.id = memory_chunks_fts.rowid
             WHERE memory_chunks_fts MATCH ?1
               AND mc.scope = ?2
             ORDER BY score DESC
             LIMIT ?3",
        )?;
        let rows = stmt
            .query_map(params![sanitized, scope, limit as i64], |row| {
                Ok((
                    ChunkRow {
                        id: row.get(0)?,
                        scope: row.get(1)?,
                        filename: row.get(2)?,
                        chunk_index: row.get(3)?,
                        start_line: row.get(4)?,
                        end_line: row.get(5)?,
                        content: row.get(6)?,
                        content_hash: row.get(7)?,
                        embedding: row.get(8)?,
                        created_at: row.get(9)?,
                    },
                    row.get::<_, f64>(10)? as f32,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }
}
