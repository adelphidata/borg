use super::*;

#[test]
fn v10_migration_creates_embeddings_table() {
    let db = test_db();
    let version = db.get_meta("schema_version").unwrap().unwrap();
    assert_eq!(version, Database::CURRENT_VERSION.to_string());
    // Table should exist
    let count: i64 = db
        .conn
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='memory_embeddings'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 1);
}

#[test]
fn upsert_and_get_embedding() {
    let db = test_db();
    let embedding = vec![1.0f32, 2.0, 3.0];
    let bytes: Vec<u8> = embedding.iter().flat_map(|f| f.to_le_bytes()).collect();

    db.upsert_embedding(
        "global",
        "notes.md",
        "hash123",
        &bytes,
        3,
        "text-embedding-3-small",
    )
    .unwrap();

    let row = db.get_embedding("global", "notes.md").unwrap().unwrap();
    assert_eq!(row.filename, "notes.md");
    assert_eq!(row.scope, "global");
    assert_eq!(row.content_hash, "hash123");
    assert_eq!(row.dimension, 3);
    assert_eq!(row.model, "text-embedding-3-small");
    assert_eq!(row.embedding, bytes);
}

#[test]
fn upsert_embedding_updates_on_conflict() {
    let db = test_db();
    let bytes1 = vec![0u8; 12];
    let bytes2 = vec![1u8; 12];

    db.upsert_embedding("global", "notes.md", "hash1", &bytes1, 3, "model-a")
        .unwrap();
    db.upsert_embedding("global", "notes.md", "hash2", &bytes2, 3, "model-b")
        .unwrap();

    let row = db.get_embedding("global", "notes.md").unwrap().unwrap();
    assert_eq!(row.content_hash, "hash2");
    assert_eq!(row.embedding, bytes2);
    assert_eq!(row.model, "model-b");

    // Should still be only one row
    assert_eq!(db.count_embeddings("global").unwrap(), 1);
}

#[test]
fn get_all_embeddings_filters_by_scope() {
    let db = test_db();
    let bytes = vec![0u8; 12];

    db.upsert_embedding("global", "a.md", "h1", &bytes, 3, "m")
        .unwrap();
    db.upsert_embedding("global", "b.md", "h2", &bytes, 3, "m")
        .unwrap();
    db.upsert_embedding("local", "c.md", "h3", &bytes, 3, "m")
        .unwrap();

    let global = db.get_all_embeddings("global").unwrap();
    assert_eq!(global.len(), 2);

    let local = db.get_all_embeddings("local").unwrap();
    assert_eq!(local.len(), 1);
    assert_eq!(local[0].filename, "c.md");
}

#[test]
fn delete_embedding_works() {
    let db = test_db();
    let bytes = vec![0u8; 12];

    db.upsert_embedding("global", "notes.md", "h1", &bytes, 3, "m")
        .unwrap();
    assert_eq!(db.count_embeddings("global").unwrap(), 1);

    let deleted = db.delete_embedding("global", "notes.md").unwrap();
    assert!(deleted);
    assert_eq!(db.count_embeddings("global").unwrap(), 0);

    // Deleting again returns false
    let deleted = db.delete_embedding("global", "notes.md").unwrap();
    assert!(!deleted);
}

#[test]
fn get_embedding_returns_none_for_missing() {
    let db = test_db();
    let result = db.get_embedding("global", "nonexistent.md").unwrap();
    assert!(result.is_none());
}

#[test]
fn count_embeddings_empty() {
    let db = test_db();
    assert_eq!(db.count_embeddings("global").unwrap(), 0);
}

#[test]
fn migrate_v12_creates_memory_chunks() {
    let db = test_db();
    let version = db.schema_version().expect("get version");
    assert_eq!(version, Database::CURRENT_VERSION);
    let count: i64 = db
        .conn
        .query_row("SELECT COUNT(*) FROM memory_chunks", [], |r| r.get(0))
        .expect("memory_chunks table should exist");
    assert_eq!(count, 0);
}

#[test]
fn migrate_v12_creates_fts_table() {
    let db = test_db();
    let count: i64 = db
        .conn
        .query_row("SELECT COUNT(*) FROM memory_chunks_fts", [], |r| r.get(0))
        .expect("FTS table should exist");
    assert_eq!(count, 0);
}

#[test]
fn upsert_and_get_chunks() {
    let db = test_db();
    let chunks = vec![
        ChunkData {
            chunk_index: 0,
            content: "First chunk about Rust programming".into(),
            content_hash: "hash0".into(),
            embedding: Some(vec![0u8; 12]),
            dimension: Some(3),
            model: Some("test-model".into()),
            start_line: Some(1),
            end_line: Some(10),
        },
        ChunkData {
            chunk_index: 1,
            content: "Second chunk about memory systems".into(),
            content_hash: "hash1".into(),
            embedding: None,
            dimension: None,
            model: None,
            start_line: Some(11),
            end_line: Some(20),
        },
    ];
    db.upsert_chunks("global", "notes.md", &chunks)
        .expect("upsert");
    let loaded = db.get_all_chunks("global", None).expect("get all");
    assert_eq!(loaded.len(), 2);
    assert_eq!(loaded[0].filename, "notes.md");
    assert_eq!(loaded[0].chunk_index, 0);
    assert_eq!(loaded[1].chunk_index, 1);
}

#[test]
fn upsert_chunks_replaces_existing() {
    let db = test_db();
    let chunks_v1 = vec![ChunkData {
        chunk_index: 0,
        content: "Old content".into(),
        content_hash: "old_hash".into(),
        embedding: None,
        dimension: None,
        model: None,
        start_line: Some(1),
        end_line: Some(5),
    }];
    db.upsert_chunks("global", "notes.md", &chunks_v1)
        .expect("v1");

    let chunks_v2 = vec![ChunkData {
        chunk_index: 0,
        content: "New content".into(),
        content_hash: "new_hash".into(),
        embedding: None,
        dimension: None,
        model: None,
        start_line: Some(1),
        end_line: Some(8),
    }];
    db.upsert_chunks("global", "notes.md", &chunks_v2)
        .expect("v2");

    let loaded = db.get_all_chunks("global", None).expect("get");
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].content, "New content");
}

#[test]
fn fts_search_returns_matching_chunks() {
    let db = test_db();
    let chunks = vec![
        ChunkData {
            chunk_index: 0,
            content: "The quick brown fox jumps over the lazy dog".into(),
            content_hash: "h0".into(),
            embedding: None,
            dimension: None,
            model: None,
            start_line: Some(1),
            end_line: Some(1),
        },
        ChunkData {
            chunk_index: 1,
            content: "Rust programming language is fast and safe".into(),
            content_hash: "h1".into(),
            embedding: None,
            dimension: None,
            model: None,
            start_line: Some(2),
            end_line: Some(2),
        },
    ];
    db.upsert_chunks("global", "test.md", &chunks)
        .expect("upsert");

    let results = db.fts_search("global", "fox", 10).expect("fts search");
    assert_eq!(results.len(), 1);
    assert!(results[0].0.content.contains("fox"));

    let results2 = db
        .fts_search("global", "Rust programming", 10)
        .expect("fts");
    assert_eq!(results2.len(), 1);
    assert!(results2[0].0.content.contains("Rust"));
}

#[test]
fn fts_search_no_results() {
    let db = test_db();
    let chunks = vec![ChunkData {
        chunk_index: 0,
        content: "Hello world".into(),
        content_hash: "h".into(),
        embedding: None,
        dimension: None,
        model: None,
        start_line: Some(1),
        end_line: Some(1),
    }];
    db.upsert_chunks("global", "test.md", &chunks)
        .expect("upsert");
    let results = db.fts_search("global", "nonexistent", 10).expect("fts");
    assert!(results.is_empty());
}

#[test]
fn delete_chunks_for_file_works() {
    let db = test_db();
    let chunks = vec![ChunkData {
        chunk_index: 0,
        content: "content".into(),
        content_hash: "h".into(),
        embedding: None,
        dimension: None,
        model: None,
        start_line: Some(1),
        end_line: Some(1),
    }];
    db.upsert_chunks("global", "a.md", &chunks)
        .expect("upsert a");
    db.upsert_chunks("global", "b.md", &chunks)
        .expect("upsert b");
    assert_eq!(db.get_all_chunks("global", None).unwrap().len(), 2);

    db.delete_chunks_for_file("global", "a.md").expect("delete");
    let remaining = db.get_all_chunks("global", None).unwrap();
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].filename, "b.md");
}

#[test]
fn chunks_scoped_isolation() {
    let db = test_db();
    let chunk = vec![ChunkData {
        chunk_index: 0,
        content: "scoped content".into(),
        content_hash: "h".into(),
        embedding: None,
        dimension: None,
        model: None,
        start_line: Some(1),
        end_line: Some(1),
    }];
    db.upsert_chunks("global", "g.md", &chunk).expect("global");
    db.upsert_chunks("local", "l.md", &chunk).expect("local");

    assert_eq!(db.get_all_chunks("global", None).unwrap().len(), 1);
    assert_eq!(db.get_all_chunks("local", None).unwrap().len(), 1);
}

#[test]
fn fts_triggers_stay_in_sync_after_upsert() {
    let db = test_db();
    let v1 = vec![ChunkData {
        chunk_index: 0,
        content: "alpha beta gamma".into(),
        content_hash: "h1".into(),
        embedding: None,
        dimension: None,
        model: None,
        start_line: Some(1),
        end_line: Some(1),
    }];
    db.upsert_chunks("global", "test.md", &v1).expect("v1");
    assert_eq!(db.fts_search("global", "alpha", 10).unwrap().len(), 1);

    let v2 = vec![ChunkData {
        chunk_index: 0,
        content: "delta epsilon zeta".into(),
        content_hash: "h2".into(),
        embedding: None,
        dimension: None,
        model: None,
        start_line: Some(1),
        end_line: Some(1),
    }];
    db.upsert_chunks("global", "test.md", &v2).expect("v2");

    assert!(db.fts_search("global", "alpha", 10).unwrap().is_empty());
    assert_eq!(db.fts_search("global", "delta", 10).unwrap().len(), 1);
}

#[test]
fn get_all_chunks_with_limit() {
    let db = test_db();
    let chunks: Vec<ChunkData> = (0..20)
        .map(|i| ChunkData {
            chunk_index: i,
            content: format!("Chunk number {i}"),
            content_hash: format!("hash_{i}"),
            embedding: None,
            dimension: None,
            model: None,
            start_line: Some((i as i64) * 10 + 1),
            end_line: Some((i as i64 + 1) * 10),
        })
        .collect();
    db.upsert_chunks("global", "big.md", &chunks)
        .expect("upsert");

    // Without limit
    let all = db.get_all_chunks("global", None).expect("get all");
    assert_eq!(all.len(), 20);

    // With limit
    let limited = db.get_all_chunks("global", Some(5)).expect("get limited");
    assert_eq!(limited.len(), 5);

    // Limit larger than actual count
    let over = db.get_all_chunks("global", Some(100)).expect("get over");
    assert_eq!(over.len(), 20);
}

#[test]
fn fts_search_empty_query() {
    let db = test_db();
    let chunks = vec![ChunkData {
        chunk_index: 0,
        content: "Hello world of programming".into(),
        content_hash: "h".into(),
        embedding: None,
        dimension: None,
        model: None,
        start_line: Some(1),
        end_line: Some(1),
    }];
    db.upsert_chunks("global", "test.md", &chunks)
        .expect("upsert");
    // Empty query after sanitization should return empty
    let results = db.fts_search("global", "", 10).expect("fts");
    assert!(results.is_empty());
}

// ── Embedding Cache Tests ──

#[test]
fn cache_embedding_round_trip() {
    let db = test_db();
    let data = vec![1u8, 2, 3, 4];
    db.cache_embedding("openai", "text-embedding-3-small", "hash1", &data, 4)
        .unwrap();
    let result = db
        .get_cached_embedding("openai", "text-embedding-3-small", "hash1")
        .unwrap();
    assert!(result.is_some());
    let (embedding, dimension) = result.unwrap();
    assert_eq!(embedding, data);
    assert_eq!(dimension, 4);
}

#[test]
fn get_cached_embedding_returns_none_for_missing() {
    let db = test_db();
    let result = db
        .get_cached_embedding("openai", "text-embedding-3-small", "nonexistent")
        .unwrap();
    assert!(result.is_none());
}

#[test]
fn cache_embedding_upsert_overwrites() {
    let db = test_db();
    db.cache_embedding("openai", "model", "hash1", &[1, 2], 2)
        .unwrap();
    db.cache_embedding("openai", "model", "hash1", &[3, 4, 5], 3)
        .unwrap();
    let (embedding, dimension) = db
        .get_cached_embedding("openai", "model", "hash1")
        .unwrap()
        .unwrap();
    assert_eq!(embedding, vec![3, 4, 5]);
    assert_eq!(dimension, 3);
}

#[test]
fn clear_embedding_cache_deletes_all() {
    let db = test_db();
    db.cache_embedding("p1", "m1", "h1", &[1], 1).unwrap();
    db.cache_embedding("p2", "m2", "h2", &[2], 1).unwrap();
    let deleted = db.clear_embedding_cache().unwrap();
    assert_eq!(deleted, 2);
    assert!(db.get_cached_embedding("p1", "m1", "h1").unwrap().is_none());
}
