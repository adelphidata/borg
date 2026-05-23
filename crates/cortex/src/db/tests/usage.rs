use super::*;
use libsql::params;

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
fn schema_includes_cache_columns() {
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
fn monthly_budget_pct_unlimited_limit_returns_none() {
    let db = test_db();
    db.log_token_usage(1_000_000, 0, 1_000_000, "openai", "gpt-4o", None)
        .expect("log");
    assert_eq!(db.monthly_budget_pct(0).expect("pct"), None);
}

#[test]
fn monthly_budget_pct_below_75_returns_none() {
    let db = test_db();
    db.log_token_usage(700, 0, 700, "openai", "gpt-4o", None)
        .expect("log");
    assert_eq!(db.monthly_budget_pct(1000).expect("pct"), None);
}

#[test]
fn monthly_budget_pct_buckets_at_each_threshold() {
    // Use a fresh db per case so prior usage doesn't bleed across asserts.
    for (used, expected) in [
        (750_u64, Some(75)),
        (899, Some(75)),
        (900, Some(90)),
        (999, Some(90)),
        (1_000, Some(100)),
        (5_000, Some(100)),
    ] {
        let db = test_db();
        db.log_token_usage(used, 0, used, "openai", "gpt-4o", None)
            .expect("log");
        assert_eq!(
            db.monthly_budget_pct(1000).expect("pct"),
            expected,
            "used={used}"
        );
    }
}

// ── token_summary_since / usage_by_model_since ────────────────────────────
// These back the `/usage` rolling-window view. Each test pins a real-world
// failure mode the formatter relies on (cutoff filters, NULL-vs-zero cost,
// ordering invariants).

const DAY: i64 = 86_400;

#[test]
fn token_summary_since_excludes_rows_outside_window() {
    let db = test_db();
    let now = chrono::Utc::now().timestamp();
    // In window: 100/50/150
    db.conn
        .execute(
            "INSERT INTO token_usage (timestamp, prompt_tokens, completion_tokens, total_tokens,
                                       provider, model, cost_usd, cached_input_tokens, cache_creation_tokens)
             VALUES (?1, 100, 50, 150, 'openai', 'gpt-4o', NULL, 0, 0)",
            params![now],
        )
        .expect("insert recent");
    // Outside window (100 days old): 999/999/9999 — must NOT be summed.
    db.conn
        .execute(
            "INSERT INTO token_usage (timestamp, prompt_tokens, completion_tokens, total_tokens,
                                       provider, model, cost_usd, cached_input_tokens, cache_creation_tokens)
             VALUES (?1, 999, 999, 9999, 'openai', 'gpt-4o', NULL, 0, 0)",
            params![now - 100 * DAY],
        )
        .expect("insert ancient");

    let summary = db
        .token_summary_since(now - 30 * DAY)
        .expect("token_summary_since");
    // Cutoff regression mode: someone uses `<= since_ts` instead of `>=`,
    // or applies the filter to the wrong column. Either bug doubles the
    // total — these exact numbers prove only the recent row is summed.
    assert_eq!(summary.prompt, 100);
    assert_eq!(summary.completion, 50);
    assert_eq!(summary.total, 150);
}

#[test]
fn token_summary_since_cost_present_false_when_no_cost_rows() {
    let db = test_db();
    let now = chrono::Utc::now().timestamp();
    // Two rows, neither with a cost — `cost_present` MUST be false so the
    // formatter hides the Cost section. A regression returning `Some(0.0)`
    // with `cost_present = true` would render "$0.00 estimated", which is
    // the "pricing unreliable" failure mode this test guards against.
    db.conn
        .execute(
            "INSERT INTO token_usage (timestamp, prompt_tokens, completion_tokens, total_tokens,
                                       provider, model, cost_usd, cached_input_tokens, cache_creation_tokens)
             VALUES (?1, 50, 25, 75, 'ollama', 'llama3', NULL, 0, 0)",
            params![now],
        )
        .expect("insert");
    db.conn
        .execute(
            "INSERT INTO token_usage (timestamp, prompt_tokens, completion_tokens, total_tokens,
                                       provider, model, cost_usd, cached_input_tokens, cache_creation_tokens)
             VALUES (?1, 50, 25, 75, 'ollama', 'llama3', NULL, 0, 0)",
            params![now],
        )
        .expect("insert");

    let summary = db.token_summary_since(now - DAY).expect("summary");
    assert!(
        !summary.cost_present,
        "no cost rows must surface as cost_present=false"
    );
    assert!(
        summary.cost.is_none(),
        "no cost rows must surface as cost=None, not Some(0.0)"
    );
}

#[test]
fn token_summary_since_cost_present_true_when_any_cost_row() {
    let db = test_db();
    let now = chrono::Utc::now().timestamp();
    db.conn
        .execute(
            "INSERT INTO token_usage (timestamp, prompt_tokens, completion_tokens, total_tokens,
                                       provider, model, cost_usd, cached_input_tokens, cache_creation_tokens)
             VALUES (?1, 100, 50, 150, 'openai', 'gpt-4o', 0.005, 0, 0)",
            params![now],
        )
        .expect("insert priced");
    db.conn
        .execute(
            "INSERT INTO token_usage (timestamp, prompt_tokens, completion_tokens, total_tokens,
                                       provider, model, cost_usd, cached_input_tokens, cache_creation_tokens)
             VALUES (?1, 100, 50, 150, 'ollama', 'llama3', NULL, 0, 0)",
            params![now],
        )
        .expect("insert unpriced");

    let summary = db.token_summary_since(now - DAY).expect("summary");
    assert!(
        summary.cost_present,
        "any cost_usd row triggers cost_present=true"
    );
    let cost = summary.cost.expect("cost should be Some");
    assert!(
        (cost - 0.005).abs() < 1e-9,
        "cost summed only over priced rows: {cost}"
    );
}

#[test]
fn usage_by_model_since_orders_by_total_tokens_desc() {
    let db = test_db();
    let now = chrono::Utc::now().timestamp();
    // Three groups; the test asserts the explicit ordering invariant the
    // formatter's "top N" depends on. A regression that drops `ORDER BY
    // SUM(total_tokens) DESC` in a refactor would silently render rows
    // out of order — this test fails loudly.
    db.log_token_usage(100, 50, 150, "openai", "gpt-4o-mini", None)
        .expect("log small");
    db.log_token_usage(800, 400, 1200, "anthropic", "sonnet", None)
        .expect("log big");
    db.log_token_usage(200, 100, 300, "openai", "gpt-4o", None)
        .expect("log mid");

    let rows = db
        .usage_by_model_since(now - DAY)
        .expect("usage_by_model_since");
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0].total_tokens, 1200, "biggest group first");
    assert_eq!(rows[1].total_tokens, 300, "middle group second");
    assert_eq!(rows[2].total_tokens, 150, "smallest group last");
}

#[test]
fn old_rows_without_provider_handled() {
    let db = test_db();
    // Simulate legacy row with no provider/model/cost
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
