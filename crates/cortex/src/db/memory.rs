use anyhow::Result;
use turso::params;

use super::models::{ChunkData, ChunkRow, EmbeddingRow, MemoryEntryRow};
use super::Database;

/// Build a comma-separated `?` placeholder string for an SQL `IN (...)`
/// clause. `n` must be > 0 — callers handle the empty case by short-circuiting
/// before they would build SQL at all.
fn in_placeholders(n: usize) -> String {
    debug_assert!(n > 0, "IN () is not valid SQL");
    vec!["?"; n].join(", ")
}

impl Database {
    // ── Embedding Cache CRUD ──

    pub async fn get_cached_embedding(
        &self,
        provider: &str,
        model: &str,
        content_hash: &str,
    ) -> Result<Option<(Vec<u8>, usize)>> {
        let mut rows = self
            .conn
            .query(
                "SELECT embedding, dimension FROM embedding_cache
             WHERE provider = ?1 AND model = ?2 AND content_hash = ?3",
                params!(provider, model, content_hash),
            )
            .await?;
        match rows.next().await? {
            Some(row) => {
                let embedding: Vec<u8> = row.get::<Vec<u8>>(0)?;
                let dimension: i64 = row.get::<i64>(1)?;
                Ok(Some((embedding, dimension.max(0) as usize)))
            }
            None => Ok(None),
        }
    }

    /// Cache an embedding result.
    ///
    /// Writes `last_accessed_at` explicitly so TTL pruning stays correct even
    /// if the schema's column default is changed or a future helper drops it.
    pub async fn cache_embedding(
        &self,
        provider: &str,
        model: &str,
        content_hash: &str,
        embedding: &[u8],
        dimension: usize,
    ) -> Result<()> {
        let now = chrono::Utc::now().timestamp();
        self.conn.execute(
            "INSERT OR REPLACE INTO embedding_cache
             (provider, model, content_hash, embedding, dimension, last_accessed_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params!(provider, model, content_hash, embedding, dimension as i64, now),
        ).await?;
        Ok(())
    }

    /// Clear all cached embeddings. Returns the number of rows deleted.
    pub async fn clear_embedding_cache(&self) -> Result<usize> {
        let count = self.conn.execute("DELETE FROM embedding_cache", ()).await?;
        Ok(count as usize)
    }

    // ── Session Index Status CRUD ──

    /// Check if a session has been indexed.
    pub async fn is_session_indexed(&self, session_id: &str) -> Result<bool> {
        let mut rows = self
            .conn
            .query(
                "SELECT 1 FROM session_index_status WHERE session_id = ?1",
                params!(session_id),
            )
            .await?;
        Ok(rows.next().await?.is_some())
    }

    /// Mark a session as indexed.
    pub async fn mark_session_indexed(
        &self,
        session_id: &str,
        message_count: usize,
    ) -> Result<()> {
        let now = chrono::Utc::now().timestamp();
        self.conn.execute(
            "INSERT OR REPLACE INTO session_index_status
             (session_id, indexed_at, message_count)
             VALUES (?1, ?2, ?3)",
            params!(session_id, now, message_count as i64),
        ).await?;
        Ok(())
    }

    /// Get session IDs that haven't been indexed yet.
    ///
    /// Uses `NOT EXISTS` so SQLite performs an indexed anti-join against
    /// `session_index_status`'s primary key, instead of materializing a full
    /// `LEFT JOIN ... IS NULL`.
    pub async fn get_unindexed_sessions(&self, limit: usize) -> Result<Vec<String>> {
        let mut rows = self
            .conn
            .query(
                "SELECT s.id FROM sessions s
             WHERE NOT EXISTS (
                 SELECT 1 FROM session_index_status si WHERE si.session_id = s.id
             )
             ORDER BY s.updated_at DESC
             LIMIT ?1",
                params!(limit as i64),
            )
            .await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push(row.get::<String>(0)?);
        }
        Ok(out)
    }

    // ── Embedding CRUD ──

    pub async fn upsert_embedding(
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
            params!(scope, filename, content_hash, embedding, dimension as i64, model, now),
        ).await?;
        Ok(())
    }

    pub async fn get_embedding(
        &self,
        scope: &str,
        filename: &str,
    ) -> Result<Option<EmbeddingRow>> {
        let mut rows = self.conn.query(
            "SELECT id, scope, filename, content_hash, embedding, dimension, model, created_at
             FROM memory_embeddings WHERE scope = ?1 AND filename = ?2",
            params!(scope, filename),
        ).await?;
        match rows.next().await? {
            Some(row) => Ok(Some(EmbeddingRow {
                id: row.get::<i64>(0)?,
                scope: row.get::<String>(1)?,
                filename: row.get::<String>(2)?,
                content_hash: row.get::<String>(3)?,
                embedding: row.get::<Vec<u8>>(4)?,
                dimension: row.get::<i64>(5)? as usize,
                model: row.get::<String>(6)?,
                created_at: row.get::<i64>(7)?,
            })),
            None => Ok(None),
        }
    }

    pub async fn get_all_embeddings(&self, scope: &str) -> Result<Vec<EmbeddingRow>> {
        let mut rows = self.conn.query(
            "SELECT id, scope, filename, content_hash, embedding, dimension, model, created_at
             FROM memory_embeddings WHERE scope = ?1",
            params!(scope),
        ).await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push(EmbeddingRow {
                id: row.get::<i64>(0)?,
                scope: row.get::<String>(1)?,
                filename: row.get::<String>(2)?,
                content_hash: row.get::<String>(3)?,
                embedding: row.get::<Vec<u8>>(4)?,
                dimension: row.get::<i64>(5)? as usize,
                model: row.get::<String>(6)?,
                created_at: row.get::<i64>(7)?,
            });
        }
        Ok(out)
    }

    pub async fn delete_embedding(&self, scope: &str, filename: &str) -> Result<bool> {
        let count = self.conn.execute(
            "DELETE FROM memory_embeddings WHERE scope = ?1 AND filename = ?2",
            params!(scope, filename),
        ).await?;
        Ok(count > 0)
    }

    pub async fn count_embeddings(&self, scope: &str) -> Result<usize> {
        let mut rows = self
            .conn
            .query(
                "SELECT COUNT(*) FROM memory_embeddings WHERE scope = ?1",
                params!(scope),
            )
            .await?;
        let count: i64 = match rows.next().await? {
            Some(row) => row.get::<i64>(0)?,
            None => 0,
        };
        Ok(count.max(0) as usize)
    }

    // ── Chunk CRUD ──

    /// Upsert a set of chunks for a given scope+filename, replacing any existing chunks for that file.
    pub async fn upsert_chunks(
        &self,
        scope: &str,
        filename: &str,
        chunks: &[ChunkData],
    ) -> Result<()> {
        let tx = self.conn.unchecked_transaction().await?;
        tx.execute(
            "DELETE FROM memory_chunks WHERE scope = ?1 AND filename = ?2",
            params!(scope, filename),
        )
        .await?;
        let now = chrono::Utc::now().timestamp();
        for chunk in chunks {
            tx.execute(
                "INSERT INTO memory_chunks
                    (scope, filename, chunk_index, start_line, end_line, content, content_hash, embedding, dimension, model, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
                 ON CONFLICT(scope, filename, chunk_index) DO UPDATE SET
                    start_line = ?4, end_line = ?5, content = ?6, content_hash = ?7,
                    embedding = ?8, dimension = ?9, model = ?10, created_at = ?11",
                params!(
                    scope,
                    filename,
                    chunk.chunk_index,
                    chunk.start_line,
                    chunk.end_line,
                    chunk.content.as_str(),
                    chunk.content_hash.as_str(),
                    chunk.embedding.as_deref(),
                    chunk.dimension.map(|d| d as i64),
                    chunk.model.as_deref(),
                    now,
                ),
            ).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Retrieve all chunks for a given scope, optionally limited to `limit` rows.
    pub async fn get_all_chunks(
        &self,
        scope: &str,
        limit: Option<usize>,
    ) -> Result<Vec<ChunkRow>> {
        let base = "SELECT id, scope, filename, chunk_index, start_line, end_line, content, content_hash, embedding, created_at
                     FROM memory_chunks WHERE scope = ?1 ORDER BY filename, chunk_index";
        let mut rows = if let Some(n) = limit {
            let sql = format!("{base} LIMIT ?2");
            self.conn.query(&sql, params!(scope, n as i64)).await?
        } else {
            self.conn.query(base, params!(scope)).await?
        };
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push(ChunkRow {
                id: row.get::<i64>(0)?,
                scope: row.get::<String>(1)?,
                filename: row.get::<String>(2)?,
                chunk_index: row.get::<i64>(3)?,
                start_line: row.get::<Option<i64>>(4)?,
                end_line: row.get::<Option<i64>>(5)?,
                content: row.get::<String>(6)?,
                content_hash: row.get::<String>(7)?,
                embedding: row.get::<Option<Vec<u8>>>(8)?,
                created_at: row.get::<i64>(9)?,
            });
        }
        Ok(out)
    }

    /// Delete all chunks for a specific file.
    pub async fn delete_chunks_for_file(
        &self,
        scope: &str,
        filename: &str,
    ) -> Result<bool> {
        let count = self.conn.execute(
            "DELETE FROM memory_chunks WHERE scope = ?1 AND filename = ?2",
            params!(scope, filename),
        ).await?;
        Ok(count > 0)
    }

    /// Retrieve all chunks for a specific file in a scope.
    pub async fn get_chunks_for_file(
        &self,
        scope: &str,
        filename: &str,
    ) -> Result<Vec<ChunkRow>> {
        let mut rows = self.conn.query(
            "SELECT id, scope, filename, chunk_index, start_line, end_line, content, content_hash, embedding, created_at
             FROM memory_chunks WHERE scope = ?1 AND filename = ?2 ORDER BY chunk_index",
            params!(scope, filename),
        ).await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push(ChunkRow {
                id: row.get::<i64>(0)?,
                scope: row.get::<String>(1)?,
                filename: row.get::<String>(2)?,
                chunk_index: row.get::<i64>(3)?,
                start_line: row.get::<Option<i64>>(4)?,
                end_line: row.get::<Option<i64>>(5)?,
                content: row.get::<String>(6)?,
                content_hash: row.get::<String>(7)?,
                embedding: row.get::<Option<Vec<u8>>>(8)?,
                created_at: row.get::<i64>(9)?,
            });
        }
        Ok(out)
    }

    /// Sanitize a query string for FTS5 MATCH syntax.
    ///
    /// - `"quoted phrases"` are preserved as single FTS5 phrase tokens so the
    ///   user can run proximity/phrase searches.
    /// - Unquoted words are each wrapped in double quotes (implicit AND match).
    /// - Unterminated quotes are treated as unquoted text (defensive fallback
    ///   against malformed input / FTS5 operator injection).
    fn sanitize_fts_query(query: &str) -> String {
        let mut tokens: Vec<String> = Vec::new();
        let mut chars = query.chars().peekable();
        loop {
            while chars.peek().is_some_and(|c| c.is_whitespace()) {
                chars.next();
            }
            let Some(&c) = chars.peek() else { break };
            if c == '"' {
                chars.next();
                let mut phrase = String::new();
                let mut closed = false;
                while let Some(&pc) = chars.peek() {
                    if pc == '"' {
                        chars.next();
                        closed = true;
                        break;
                    }
                    phrase.push(pc);
                    chars.next();
                }
                if closed {
                    let trimmed = phrase.trim();
                    if !trimmed.is_empty() {
                        tokens.push(format!("\"{trimmed}\""));
                    }
                } else {
                    for w in phrase.split_whitespace() {
                        tokens.push(format!("\"{w}\""));
                    }
                }
            } else {
                let mut word = String::new();
                while let Some(&wc) = chars.peek() {
                    if wc.is_whitespace() || wc == '"' {
                        break;
                    }
                    word.push(wc);
                    chars.next();
                }
                if !word.is_empty() {
                    tokens.push(format!("\"{word}\""));
                }
            }
        }
        tokens.join(" ")
    }

    /// Full-text candidate search over chunk content within a scope.
    /// Returns `(ChunkRow, 0.0)` pairs for every row matching the sanitized
    /// query — Turso's `USING fts` index method does not expose a
    /// SQL-callable `bm25()`, so BM25 ranking moves to the caller (see
    /// [`crate::memory::bm25`]). The `f32` slot is preserved for API
    /// symmetry; the caller is expected to overwrite it.
    ///
    /// `limit` is the final post-rank result count; this method oversamples
    /// by 4× (capped at 200) so the BM25 re-rank has enough breadth.
    pub async fn fts_search(
        &self,
        scope: &str,
        query: &str,
        limit: usize,
    ) -> Result<Vec<(ChunkRow, f32)>> {
        let sanitized = Self::sanitize_fts_query(query);
        if sanitized.is_empty() {
            return Ok(Vec::new());
        }
        let candidate_limit = limit.saturating_mul(4).min(200).max(limit);
        let mut rows = self.conn.query(
            "SELECT id, scope, filename, chunk_index, start_line, end_line,
                    content, content_hash, embedding, created_at
             FROM memory_chunks
             WHERE content MATCH ?1
               AND scope = ?2
             LIMIT ?3",
            params!(sanitized, scope, candidate_limit as i64),
        ).await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push((
                ChunkRow {
                    id: row.get::<i64>(0)?,
                    scope: row.get::<String>(1)?,
                    filename: row.get::<String>(2)?,
                    chunk_index: row.get::<i64>(3)?,
                    start_line: row.get::<Option<i64>>(4)?,
                    end_line: row.get::<Option<i64>>(5)?,
                    content: row.get::<String>(6)?,
                    content_hash: row.get::<String>(7)?,
                    embedding: row.get::<Option<Vec<u8>>>(8)?,
                    created_at: row.get::<i64>(9)?,
                },
                0.0,
            ));
        }
        Ok(out)
    }

    // ── Memory Entries CRUD ──

    /// Insert or update a memory entry. Computes content_hash automatically.
    ///
    /// Also invalidates stale embeddings and chunks for the replaced entry so
    /// search results don't surface outdated content. Runs in an immediate
    /// transaction to keep the entry row and its index entries consistent.
    pub async fn upsert_memory_entry(
        &self,
        scope: &str,
        name: &str,
        content: &str,
    ) -> Result<()> {
        use sha2::{Digest, Sha256};
        let hash = format!("{:x}", Sha256::digest(content.as_bytes()));
        let now = chrono::Utc::now().timestamp();
        let tx = self.conn.unchecked_transaction().await?;
        tx.execute(
            "INSERT INTO memory_entries (scope, name, content, content_hash, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?5)
             ON CONFLICT(scope, name) DO UPDATE SET
                content = ?3, content_hash = ?4, updated_at = ?5, estimated_tokens = NULL",
            params!(scope, name, content, hash, now),
        ).await?;
        // Stale search-index cleanup: embeddings and chunks will be regenerated
        // by the caller's background embedding task after this returns.
        tx.execute(
            "DELETE FROM memory_embeddings WHERE scope = ?1 AND filename = ?2",
            params!(scope, name),
        ).await?;
        tx.execute(
            "DELETE FROM memory_chunks WHERE scope = ?1 AND filename = ?2",
            params!(scope, name),
        ).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Retrieve a memory entry by scope and name.
    pub async fn get_memory_entry(
        &self,
        scope: &str,
        name: &str,
    ) -> Result<Option<MemoryEntryRow>> {
        let mut rows = self.conn.query(
            "SELECT id, scope, name, content, content_hash, created_at, updated_at
             FROM memory_entries WHERE scope = ?1 AND name = ?2",
            params!(scope, name),
        ).await?;
        match rows.next().await? {
            Some(row) => Ok(Some(MemoryEntryRow {
                id: row.get::<i64>(0)?,
                scope: row.get::<String>(1)?,
                name: row.get::<String>(2)?,
                content: row.get::<String>(3)?,
                content_hash: row.get::<String>(4)?,
                created_at: row.get::<i64>(5)?,
                updated_at: row.get::<i64>(6)?,
            })),
            None => Ok(None),
        }
    }

    /// Delete a memory entry along with its embeddings and chunks.
    /// Returns true if the entry row was deleted.
    pub async fn delete_memory_entry(&self, scope: &str, name: &str) -> Result<bool> {
        let tx = self.conn.unchecked_transaction().await?;
        let count = tx.execute(
            "DELETE FROM memory_entries WHERE scope = ?1 AND name = ?2",
            params!(scope, name),
        ).await?;
        tx.execute(
            "DELETE FROM memory_embeddings WHERE scope = ?1 AND filename = ?2",
            params!(scope, name),
        ).await?;
        tx.execute(
            "DELETE FROM memory_chunks WHERE scope = ?1 AND filename = ?2",
            params!(scope, name),
        ).await?;
        tx.commit().await?;
        Ok(count > 0)
    }

    /// List all memory entries for a scope, ordered by updated_at DESC.
    pub async fn list_memory_entries(&self, scope: &str) -> Result<Vec<MemoryEntryRow>> {
        let mut rows = self.conn.query(
            "SELECT id, scope, name, content, content_hash, created_at, updated_at
             FROM memory_entries WHERE scope = ?1 ORDER BY updated_at DESC",
            params!(scope),
        ).await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push(MemoryEntryRow {
                id: row.get::<i64>(0)?,
                scope: row.get::<String>(1)?,
                name: row.get::<String>(2)?,
                content: row.get::<String>(3)?,
                content_hash: row.get::<String>(4)?,
                created_at: row.get::<i64>(5)?,
                updated_at: row.get::<i64>(6)?,
            });
        }
        Ok(out)
    }

    /// Append content to an existing memory entry, or create it if it doesn't exist.
    ///
    /// Runs in an immediate transaction so concurrent appenders don't interleave a
    /// read-modify-write and drop one another's writes. Also invalidates stale
    /// embeddings/chunks so the background re-embedder produces a fresh index.
    pub async fn append_memory_entry(
        &self,
        scope: &str,
        name: &str,
        content: &str,
    ) -> Result<()> {
        use sha2::{Digest, Sha256};
        let now = chrono::Utc::now().timestamp();
        let tx = self.conn.unchecked_transaction().await?;
        let existing: Option<String> = {
            let mut rows = tx
                .query(
                    "SELECT content FROM memory_entries WHERE scope = ?1 AND name = ?2",
                    params!(scope, name),
                )
                .await?;
            match rows.next().await? {
                Some(row) => Some(row.get::<String>(0)?),
                None => None,
            }
        };
        let new_content = match existing {
            Some(prev) => {
                let sep = if prev.ends_with('\n') { "" } else { "\n" };
                format!("{prev}{sep}{content}")
            }
            None => content.to_string(),
        };
        let hash = format!("{:x}", Sha256::digest(new_content.as_bytes()));
        tx.execute(
            "INSERT INTO memory_entries (scope, name, content, content_hash, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?5)
             ON CONFLICT(scope, name) DO UPDATE SET
                content = ?3, content_hash = ?4, updated_at = ?5, estimated_tokens = NULL",
            params!(scope, name, new_content, hash, now),
        ).await?;
        tx.execute(
            "DELETE FROM memory_embeddings WHERE scope = ?1 AND filename = ?2",
            params!(scope, name),
        ).await?;
        tx.execute(
            "DELETE FROM memory_chunks WHERE scope = ?1 AND filename = ?2",
            params!(scope, name),
        ).await?;
        tx.commit().await?;
        Ok(())
    }

    /// List all distinct scopes that have at least one memory entry.
    ///
    /// Used by `memory_search` to avoid hard-coding a scope list: a write to
    /// `scope="project:abc"` is visible to future searches without the caller
    /// needing to know the scope ahead of time.
    pub async fn list_memory_scopes(&self) -> Result<Vec<String>> {
        let mut rows = self
            .conn
            .query(
                "SELECT DISTINCT scope FROM memory_entries ORDER BY scope",
                (),
            )
            .await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push(row.get::<String>(0)?);
        }
        Ok(out)
    }

    /// Batch form of [`Self::list_memory_entries`]: fetch all entries across
    /// multiple scopes in a single query. Used by the agent loop on every turn,
    /// where naively looping single-scope queries was an N+1 pattern.
    pub async fn list_memory_entries_for_scopes(
        &self,
        scopes: &[String],
    ) -> Result<Vec<MemoryEntryRow>> {
        if scopes.is_empty() {
            return Ok(Vec::new());
        }
        let placeholders = in_placeholders(scopes.len());
        let sql = format!(
            "SELECT id, scope, name, content, content_hash, created_at, updated_at
             FROM memory_entries
             WHERE scope IN ({placeholders})
             ORDER BY scope, updated_at DESC"
        );
        let bound: Vec<turso::Value> = scopes
            .iter()
            .map(|s| turso::Value::Text(s.clone()))
            .collect();
        let mut rows = self.conn.query(&sql, bound).await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push(MemoryEntryRow {
                id: row.get::<i64>(0)?,
                scope: row.get::<String>(1)?,
                name: row.get::<String>(2)?,
                content: row.get::<String>(3)?,
                content_hash: row.get::<String>(4)?,
                created_at: row.get::<i64>(5)?,
                updated_at: row.get::<i64>(6)?,
            });
        }
        Ok(out)
    }

    /// Batch form of [`Self::list_memory_tokens_map`]: returns a map keyed by
    /// `(scope, name)` so callers can disambiguate identical entry names across
    /// scopes (e.g. `INDEX` exists in both `global` and `project:abc`).
    pub async fn list_memory_tokens_map_for_scopes(
        &self,
        scopes: &[String],
    ) -> Result<std::collections::HashMap<(String, String), i64>> {
        if scopes.is_empty() {
            return Ok(std::collections::HashMap::new());
        }
        let placeholders = in_placeholders(scopes.len());
        let sql = format!(
            "SELECT scope, name, estimated_tokens FROM memory_entries
             WHERE scope IN ({placeholders}) AND estimated_tokens IS NOT NULL"
        );
        let bound: Vec<turso::Value> = scopes
            .iter()
            .map(|s| turso::Value::Text(s.clone()))
            .collect();
        let mut rows = self.conn.query(&sql, bound).await?;
        let mut out = std::collections::HashMap::new();
        while let Some(row) = rows.next().await? {
            out.insert(
                (row.get::<String>(0)?, row.get::<String>(1)?),
                row.get::<i64>(2)?,
            );
        }
        Ok(out)
    }

    /// Fetch the cached estimated-token counts for all entries in a scope where
    /// the column is non-NULL. Entries missing from the map should be re-tokenized
    /// and persisted via [`Self::set_memory_tokens`].
    pub async fn list_memory_tokens_map(
        &self,
        scope: &str,
    ) -> Result<std::collections::HashMap<String, i64>> {
        let mut rows = self
            .conn
            .query(
                "SELECT name, estimated_tokens FROM memory_entries
             WHERE scope = ?1 AND estimated_tokens IS NOT NULL",
                params!(scope),
            )
            .await?;
        let mut out = std::collections::HashMap::new();
        while let Some(row) = rows.next().await? {
            out.insert(row.get::<String>(0)?, row.get::<i64>(1)?);
        }
        Ok(out)
    }

    /// Persist a freshly-computed token estimate for a memory entry.
    ///
    /// Called by the memory loader on cache miss. Any content mutation resets
    /// the column to NULL, so the value here is only valid against the entry's
    /// current `content_hash`.
    pub async fn set_memory_tokens(&self, scope: &str, name: &str, tokens: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE memory_entries SET estimated_tokens = ?1
             WHERE scope = ?2 AND name = ?3",
            params!(tokens, scope, name),
        ).await?;
        Ok(())
    }

    /// Count memory entries for a scope.
    pub async fn count_memory_entries(&self, scope: &str) -> Result<usize> {
        let mut rows = self
            .conn
            .query(
                "SELECT COUNT(*) FROM memory_entries WHERE scope = ?1",
                params!(scope),
            )
            .await?;
        let count: i64 = match rows.next().await? {
            Some(row) => row.get::<i64>(0)?,
            None => 0,
        };
        Ok(count.max(0) as usize)
    }

    // ── Embedding Cache Lifecycle ──

    /// Update last_accessed_at for a cache entry on hit.
    pub async fn touch_cache_entry(
        &self,
        provider: &str,
        model: &str,
        content_hash: &str,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE embedding_cache SET last_accessed_at = unixepoch()
             WHERE provider = ?1 AND model = ?2 AND content_hash = ?3",
            params!(provider, model, content_hash),
        ).await?;
        Ok(())
    }

    /// Prune embedding cache entries not accessed within `max_age_secs`. Returns rows deleted.
    pub async fn prune_embedding_cache(&self, max_age_secs: i64) -> Result<usize> {
        let cutoff = chrono::Utc::now().timestamp() - max_age_secs;
        let count = self.conn.execute(
            "DELETE FROM embedding_cache WHERE last_accessed_at < ?1",
            params!(cutoff),
        ).await?;
        Ok(count as usize)
    }

    /// Prune embedding cache to at most `max_entries`, removing oldest by `last_accessed_at`.
    ///
    /// Uses `NOT IN (SELECT ... ORDER BY last_accessed_at DESC LIMIT N)`. The
    /// inner subquery is driven by `idx_embedding_cache_last_accessed` so the
    /// LIMIT short-circuits without a sort, and the `id` tiebreaker means
    /// timestamp ties at the boundary still leave exactly `max_entries` rows
    /// (a `last_accessed_at <= cutoff` shape would over-delete on ties).
    pub async fn prune_embedding_cache_by_count(&self, max_entries: usize) -> Result<usize> {
        let count = self.conn.execute(
            "DELETE FROM embedding_cache WHERE id NOT IN (
                SELECT id FROM embedding_cache
                ORDER BY last_accessed_at DESC, id DESC LIMIT ?1
            )",
            params!(max_entries as i64),
        ).await?;
        Ok(count as usize)
    }
}
