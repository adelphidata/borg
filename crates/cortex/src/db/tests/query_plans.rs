//! `EXPLAIN QUERY PLAN` regression guards for queries that depend on a
//! specific index. Each test asserts the *invariant* (this query uses index X)
//! rather than just running the query and checking the result — a result-only
//! test would silently pass even if a future schema change dropped the index
//! and forced a table scan.
//!
//! Source-grep tests are otherwise discouraged (see `CLAUDE.md` — "no fluff
//! tests"); these are listed as one of the explicit exceptions because the
//! index-usage invariant cannot be expressed in the type system.

use super::*;

/// Run `EXPLAIN QUERY PLAN <sql>` and return the joined `detail` column for
/// every plan row. The combined string is what we pattern-match in assertions.
async fn explain(db: &Database, sql: &str, params: Vec<libsql::Value>) -> String {
    let explain_sql = format!("EXPLAIN QUERY PLAN {sql}");
    let mut stmt = db.conn.prepare(&explain_sql).await.unwrap();
    let mut rows = stmt.query(params).await.unwrap();
    let mut details: Vec<String> = Vec::new();
    while let Some(row) = rows.next().await.unwrap() {
        details.push(row.get::<String>(3).unwrap());
    }
    details.join("\n")
}

#[tokio::test]
async fn get_unindexed_sessions_uses_indexed_anti_join_and_ordering() {
    // Guard: rewriting `LEFT JOIN ... IS NULL` to `NOT EXISTS` plus the new
    // idx_sessions_updated_at index must let SQLite (a) skip a full
    // sessions-table scan for the order-by, and (b) probe
    // session_index_status by primary key, not scan it.
    let db = test_db().await;
    let plan = explain(
        &db,
        "SELECT s.id FROM sessions s
         WHERE NOT EXISTS (
             SELECT 1 FROM session_index_status si WHERE si.session_id = s.id
         )
         ORDER BY s.updated_at DESC
         LIMIT ?1",
        vec![libsql::Value::Integer(10)],
    )
    .await;
    assert!(
        plan.contains("USING INDEX idx_sessions_updated_at")
            || plan.contains("USING COVERING INDEX idx_sessions_updated_at"),
        "expected idx_sessions_updated_at to drive ORDER BY, plan was:\n{plan}"
    );
    assert!(
        !plan.contains("USE TEMP B-TREE"),
        "ORDER BY should use the index, not a temp btree; plan was:\n{plan}"
    );
    // The anti-join probes session_index_status's PK index — never a SCAN.
    assert!(
        !plan.contains("SCAN session_index_status"),
        "anti-join must not scan session_index_status; plan was:\n{plan}"
    );
}

#[tokio::test]
async fn prune_embedding_cache_inner_keeps_set_uses_last_accessed_index() {
    // Guard: `prune_embedding_cache_by_count` uses NOT IN against an inner
    // `ORDER BY last_accessed_at DESC, id DESC LIMIT N` subquery. The inner
    // sort must be served by idx_embedding_cache_last_accessed so the LIMIT
    // short-circuits — otherwise SQLite would walk the whole table and sort
    // it just to compute the keep-set every prune cycle.
    let db = test_db().await;
    let plan = explain(
        &db,
        "SELECT id FROM embedding_cache
         ORDER BY last_accessed_at DESC, id DESC LIMIT ?1",
        vec![libsql::Value::Integer(100)],
    )
    .await;
    assert!(
        plan.contains("USING INDEX idx_embedding_cache_last_accessed")
            || plan.contains("USING COVERING INDEX idx_embedding_cache_last_accessed"),
        "keep-set lookup must use idx_embedding_cache_last_accessed; plan was:\n{plan}"
    );
}

#[tokio::test]
async fn list_memory_entries_for_scopes_uses_scope_index() {
    // Guard: the batch helper relies on idx_memory_entries_scope to seek by
    // scope. If that index is dropped or the IN-clause is rewritten in a way
    // that defeats the index (e.g. function on the column), we fall back to
    // a full memory_entries scan — bad for an every-turn query.
    let db = test_db().await;
    let plan = explain(
        &db,
        "SELECT id, scope, name, content, content_hash, created_at, updated_at
         FROM memory_entries
         WHERE scope IN (?, ?, ?)
         ORDER BY scope, updated_at DESC",
        vec![
            libsql::Value::Text("global".to_string()),
            libsql::Value::Text("project:abc".to_string()),
            libsql::Value::Text("extra".to_string()),
        ],
    )
    .await;
    assert!(
        plan.contains("USING INDEX idx_memory_entries_scope"),
        "scoped fetch must use idx_memory_entries_scope; plan was:\n{plan}"
    );
    assert!(
        !plan.contains("SCAN memory_entries"),
        "scoped fetch must not scan memory_entries; plan was:\n{plan}"
    );
}

#[tokio::test]
async fn sessions_since_uses_updated_at_index() {
    // Guard: heartbeat calls sessions_since(0) every tick. With
    // idx_sessions_updated_at SQLite walks the index in DESC order and stops
    // at LIMIT, no temp btree, no full scan.
    let db = test_db().await;
    let plan = explain(
        &db,
        "SELECT id, created_at, updated_at, total_tokens, model, title,
                parent_session_id, forked_from_message_id
         FROM sessions WHERE updated_at >= ?1 ORDER BY updated_at DESC
         LIMIT ?2",
        vec![libsql::Value::Integer(0), libsql::Value::Integer(10_000)],
    )
    .await;
    assert!(
        plan.contains("USING INDEX idx_sessions_updated_at")
            || plan.contains("USING COVERING INDEX idx_sessions_updated_at"),
        "sessions_since must use idx_sessions_updated_at; plan was:\n{plan}"
    );
    assert!(
        !plan.contains("USE TEMP B-TREE"),
        "ORDER BY should use the index, not a temp btree; plan was:\n{plan}"
    );
}
