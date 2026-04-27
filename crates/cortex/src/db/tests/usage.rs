use super::*;
use rusqlite::params;

#[test]
fn log_and_query_token_usage() {
    let db = test_db();
    db.log_token_usage(100, 50, 150, "openai", "gpt-4", None)
        .expect("log usage");
    db.log_token_usage(200, 100, 300, "openai", "gpt-4", None)
        .expect("log usage 2");
    let total = db.monthly_token_total().expect("query");
    assert_eq!(total, 450);
}

#[test]
fn log_token_usage_with_cache_persists_cache_columns() {
    let db = test_db();
    db.log_token_usage_with_cache(
        1000,
        200,
        1200,
        800,
        150,
        "anthropic",
        "claude-sonnet-4",
        None,
    )
    .expect("log with cache");
    let (prompt, cached, created) = db
        .cache_token_summary_since(0)
        .expect("cache token summary");
    assert_eq!(prompt, 1000);
    assert_eq!(cached, 800);
    assert_eq!(created, 150);
}

#[test]
fn log_token_usage_defaults_cache_columns_to_zero() {
    let db = test_db();
    db.log_token_usage(500, 100, 600, "openai", "gpt-4", None)
        .expect("log plain");
    let (prompt, cached, created) = db
        .cache_token_summary_since(0)
        .expect("cache token summary");
    assert_eq!(prompt, 500);
    assert_eq!(cached, 0);
    assert_eq!(created, 0);
}

#[test]
fn cache_token_summary_since_empty_returns_zeros() {
    let db = test_db();
    let (prompt, cached, created) = db
        .cache_token_summary_since(0)
        .expect("cache token summary");
    assert_eq!(prompt, 0);
    assert_eq!(cached, 0);
    assert_eq!(created, 0);
}

#[test]
fn migration_v30_adds_cache_columns() {
    // A fresh test_db() has already run all migrations. Verify the new
    // columns exist on token_usage and default to 0 for plain inserts.
    let db = test_db();
    let cached_exists: i64 = db
        .conn
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('token_usage') WHERE name = 'cached_input_tokens'",
            [],
            |r| r.get(0),
        )
        .expect("pragma");
    let created_exists: i64 = db
        .conn
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('token_usage') WHERE name = 'cache_creation_tokens'",
            [],
            |r| r.get(0),
        )
        .expect("pragma");
    assert_eq!(cached_exists, 1, "cached_input_tokens column must exist");
    assert_eq!(created_exists, 1, "cache_creation_tokens column must exist");
}

#[test]
fn monthly_token_total_empty_returns_zero() {
    let db = test_db();
    let total = db.monthly_token_total().expect("query");
    assert_eq!(total, 0);
}

#[test]
fn monthly_token_total_excludes_old_entries() {
    let db = test_db();
    // Insert a row with a very old timestamp (year 2020)
    db.conn
        .execute(
            "INSERT INTO token_usage (timestamp, prompt_tokens, completion_tokens, total_tokens)
             VALUES (?1, 500, 500, 1000)",
            params![1577836800_i64], // 2020-01-01
        )
        .expect("insert old");
    // Insert a current row
    db.log_token_usage(100, 50, 150, "openai", "gpt-4", None)
        .expect("log current");
    let total = db.monthly_token_total().expect("query");
    // Old entry should be excluded, only current entry counts
    assert_eq!(total, 150);
}

#[test]
fn monthly_usage_by_model_groups_correctly() {
    let db = test_db();
    db.log_token_usage(
        100,
        50,
        150,
        "openrouter",
        "anthropic/claude-sonnet-4",
        Some(0.00105),
    )
    .expect("log");
    db.log_token_usage(
        200,
        100,
        300,
        "openrouter",
        "anthropic/claude-sonnet-4",
        Some(0.0021),
    )
    .expect("log");
    db.log_token_usage(500, 200, 700, "openai", "gpt-4o", Some(0.00325))
        .expect("log");

    let rows = db.monthly_usage_by_model().expect("query");
    assert_eq!(rows.len(), 2);
    // Ordered by total_tokens DESC
    assert_eq!(rows[0].model, "gpt-4o");
    assert_eq!(rows[0].total_tokens, 700);
    assert_eq!(rows[1].model, "anthropic/claude-sonnet-4");
    assert_eq!(rows[1].total_tokens, 450);
    assert_eq!(rows[1].prompt_tokens, 300);
    assert_eq!(rows[1].completion_tokens, 150);
}

#[test]
fn monthly_total_cost_sums_correctly() {
    let db = test_db();
    db.log_token_usage(100, 50, 150, "openai", "gpt-4o", Some(0.001))
        .expect("log");
    db.log_token_usage(200, 100, 300, "openai", "gpt-4o", Some(0.002))
        .expect("log");

    let cost = db.monthly_total_cost().expect("query");
    assert!((cost.unwrap() - 0.003).abs() < 1e-9);
}

#[test]
fn old_rows_without_provider_handled() {
    let db = test_db();
    // Simulate pre-V11 row with no provider/model/cost
    db.conn
        .execute(
            "INSERT INTO token_usage (timestamp, prompt_tokens, completion_tokens, total_tokens)
             VALUES (?1, 100, 50, 150)",
            params![chrono::Utc::now().timestamp()],
        )
        .expect("insert old-style");
    db.log_token_usage(200, 100, 300, "openai", "gpt-4o", Some(0.002))
        .expect("log new");

    let rows = db.monthly_usage_by_model().expect("query");
    assert_eq!(rows.len(), 2);
    // One row with empty provider/model (old), one with real values
    let old_row = rows.iter().find(|r| r.model.is_empty());
    assert!(old_row.is_some());
    assert_eq!(old_row.unwrap().total_tokens, 150);
}
