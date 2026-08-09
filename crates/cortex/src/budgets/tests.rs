//! Integration tests for the budget engine + rate-card lookup.
//!
//! Every test runs against a real in-memory `Database` (no mocks) so the
//! actual SQL paths are exercised — `migrate_v43` / `migrate_v44` /
//! `migrate_v45` ran, the seeded `default` budget row exists, the
//! `provider_rate_cards` PK collision rules apply, etc.

use super::engine::{BudgetEngine, PreFlightDecision};
use super::rate_card::RateCard;
use super::usd::Usd;
use crate::db::Database;
use rusqlite::params;

fn override_default(db: &Database, hard: &str, soft: Option<&str>, period: &str) {
    db.conn()
        .execute(
            "UPDATE budgets SET hard_cap_usd = ?1, soft_cap_usd = ?2, reset_period = ?3
             WHERE scope = 'default'",
            params![hard, soft, period],
        )
        .unwrap();
}

fn set_spent(db: &Database, scope: &str, spent: &str, period_start: i64) {
    db.conn()
        .execute(
            "UPDATE budgets SET current_spent_usd = ?1, current_period_start = ?2
             WHERE scope = ?3",
            params![spent, period_start, scope],
        )
        .unwrap();
}

fn budget_hit_count(db: &Database) -> i64 {
    db.conn()
        .query_row(
            "SELECT COUNT(*) FROM provider_call_events WHERE event_type = 'budget_hit'",
            [],
            |r| r.get(0),
        )
        .unwrap()
}

fn provider_call_count(db: &Database) -> i64 {
    db.conn()
        .query_row(
            "SELECT COUNT(*) FROM provider_call_events WHERE event_type = 'provider_call'",
            [],
            |r| r.get(0),
        )
        .unwrap()
}

fn read_spent(db: &Database, scope: &str) -> Usd {
    let s: String = db
        .conn()
        .query_row(
            "SELECT current_spent_usd FROM budgets WHERE scope = ?1",
            params![scope],
            |r| r.get(0),
        )
        .unwrap();
    Usd::parse(&s).unwrap()
}

#[test]
fn pre_flight_refuses_at_hard_cap() {
    // $19.50/$20 daily, $0.80 estimate would push past $20 → HardRefuse.
    // Spend must remain unchanged (the refusal is the *gate* — the call
    // never happened) and exactly one `budget_hit` row appended.
    let db = Database::test_db();
    override_default(&db, "20.00", Some("5.00"), "daily");
    set_spent(&db, "default", "19.50", chrono::Utc::now().timestamp());

    let before_spent = read_spent(&db, "default");
    let before_hits = budget_hit_count(&db);

    let estimate = Usd::parse("0.80").unwrap();
    let decision = BudgetEngine::pre_flight(db.conn(), None, estimate).unwrap();

    match decision {
        PreFlightDecision::HardRefuse {
            scope,
            hard_cap,
            spent,
        } => {
            assert_eq!(scope, "default");
            assert_eq!(hard_cap.to_storage_string(), "20.00");
            assert_eq!(spent.to_storage_string(), "19.50");
        }
        other => panic!("expected HardRefuse, got {other:?}"),
    }

    assert_eq!(
        read_spent(&db, "default"),
        before_spent,
        "spend must not move on refusal"
    );
    assert_eq!(
        budget_hit_count(&db),
        before_hits + 1,
        "exactly one budget_hit row appended"
    );
}

#[test]
fn pre_flight_allows_under_cap() {
    // $1/$20 with no soft cap, $0.80 estimate → Allow, no budget_hit row.
    // (A soft cap would fire SoftWarn here — that path is covered by the
    // dedicated debounce test.)
    let db = Database::test_db();
    override_default(&db, "20.00", None, "daily");
    set_spent(&db, "default", "1.00", chrono::Utc::now().timestamp());

    let before_hits = budget_hit_count(&db);
    let decision = BudgetEngine::pre_flight(db.conn(), None, Usd::parse("0.80").unwrap()).unwrap();

    assert_eq!(decision, PreFlightDecision::Allow);
    assert_eq!(budget_hit_count(&db), before_hits, "no event on Allow");
}

#[test]
fn soft_warn_debounces_within_period() {
    // $4.00/$5 soft cap; estimate $0.10 crosses 80% threshold ($4.00).
    // First call → SoftWarn; second call same period → Allow (debounced).
    let db = Database::test_db();
    override_default(&db, "20.00", Some("5.00"), "daily");
    let period_start = chrono::Utc::now().timestamp();
    set_spent(&db, "default", "4.00", period_start);

    let est = Usd::parse("0.10").unwrap();
    let first = BudgetEngine::pre_flight(db.conn(), None, est).unwrap();
    matches!(first, PreFlightDecision::SoftWarn { .. });
    if !matches!(first, PreFlightDecision::SoftWarn { .. }) {
        panic!("first crossing must SoftWarn, got {first:?}");
    }

    let second = BudgetEngine::pre_flight(db.conn(), None, est).unwrap();
    assert_eq!(
        second,
        PreFlightDecision::Allow,
        "second pre-flight in same period must be debounced to Allow, got {second:?}"
    );

    // Advance period by ≥1 day → the soft warn fires again. Reset the spent
    // to the same level (after a real reset it would be 0; here we want to
    // assert specifically on the debounce mechanism, not the reset path).
    set_spent(&db, "default", "4.00", period_start + 86_400);
    db.conn()
        .execute(
            "UPDATE budgets SET last_soft_toast_period = NULL WHERE scope = 'default'",
            [],
        )
        .unwrap();

    let third = BudgetEngine::pre_flight(db.conn(), None, est).unwrap();
    assert!(
        matches!(third, PreFlightDecision::SoftWarn { .. }),
        "next period must SoftWarn again, got {third:?}"
    );
}

#[test]
fn post_flight_overshoot_blocks_next_call() {
    // $19.80/$20, est $0.10 (allowed), actual $0.30 → spend = $20.10.
    // Next pre-flight refuses any positive estimate.
    let db = Database::test_db();
    override_default(&db, "20.00", Some("5.00"), "daily");
    set_spent(&db, "default", "19.80", chrono::Utc::now().timestamp());

    // The pre-flight allows the call (estimate fits).
    let est = Usd::parse("0.10").unwrap();
    assert!(matches!(
        BudgetEngine::pre_flight(db.conn(), None, est).unwrap(),
        PreFlightDecision::Allow | PreFlightDecision::SoftWarn { .. }
    ));

    // Mid-stream cancellation is forbidden — post_flight always accepts.
    let actual = Usd::parse("0.30").unwrap();
    BudgetEngine::post_flight(
        db.conn(),
        None,
        "openai",
        "gpt-4o-mini",
        None,
        100,
        50,
        0,
        actual,
        Some("test-session"),
    )
    .unwrap();

    assert_eq!(read_spent(&db, "default"), Usd::parse("20.10").unwrap());
    assert_eq!(provider_call_count(&db), 1, "provider_call event written");

    // Next call: any positive estimate now exceeds hard cap.
    let next = BudgetEngine::pre_flight(db.conn(), None, Usd::parse("0.01").unwrap()).unwrap();
    assert!(
        matches!(next, PreFlightDecision::HardRefuse { .. }),
        "post-overshoot must refuse the *next* call, got {next:?}"
    );
}

#[test]
fn min_wins_principal_cap() {
    // default $20 hard, principal:p1 $1 hard. With $0.50 estimate → Allow
    // (within both). With $1.20 → HardRefuse (principal exceeded, even
    // though default has $19+ headroom).
    let db = Database::test_db();
    override_default(&db, "20.00", Some("5.00"), "daily");
    let now = chrono::Utc::now().timestamp();
    db.conn()
        .execute(
            "INSERT INTO budgets
             (id, scope, reset_period, hard_cap_usd, soft_cap_usd,
              current_spent_usd, current_period_start, created_at, updated_at)
             VALUES ('p1', 'principal:p1', 'daily', '1.00', NULL, '0', ?1, ?1, ?1)",
            params![now],
        )
        .unwrap();

    // Allowed under both.
    let allow =
        BudgetEngine::pre_flight(db.conn(), Some("principal:p1"), Usd::parse("0.50").unwrap())
            .unwrap();
    assert_eq!(allow, PreFlightDecision::Allow);

    // Exceeds principal but not default → HardRefuse with principal scope.
    let refuse =
        BudgetEngine::pre_flight(db.conn(), Some("principal:p1"), Usd::parse("1.20").unwrap())
            .unwrap();
    match refuse {
        PreFlightDecision::HardRefuse { scope, .. } => {
            assert_eq!(
                scope, "principal:p1",
                "principal cap must bind, not default"
            );
        }
        other => panic!("expected HardRefuse, got {other:?}"),
    }
}

#[test]
fn reset_tick_advances_period_and_zeroes_spend() {
    // Daily budget, period_start = now − 25h, spent = $4.92.
    // reset_if_due should zero spend, advance period_start by exactly 86_400s,
    // and write one budget_reset event with the prior spend in metadata.
    let db = Database::test_db();
    override_default(&db, "20.00", Some("5.00"), "daily");
    let now = 2_000_000_000_i64;
    let period_start = now - 25 * 3600;
    set_spent(&db, "default", "4.92", period_start);

    let count = BudgetEngine::reset_if_due(db.conn(), now).unwrap();
    assert_eq!(
        count, 1,
        "exactly one budget reset for the expired daily row"
    );

    let (new_start, new_spent): (i64, String) = db
        .conn()
        .query_row(
            "SELECT current_period_start, current_spent_usd FROM budgets WHERE scope = 'default'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        new_start,
        period_start + 86_400,
        "advance by exactly one period"
    );
    assert_eq!(new_spent, "0", "spend zeroed");

    let (event_count, payload): (i64, String) = db
        .conn()
        .query_row(
            "SELECT COUNT(*), MAX(metadata_json) FROM provider_call_events
             WHERE event_type = 'budget_reset'",
            [],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                ))
            },
        )
        .unwrap();
    assert_eq!(event_count, 1, "one budget_reset event");
    assert!(
        payload.contains("4.92"),
        "metadata must record prior spend: {payload}"
    );

    // Subsequent call within the new period should be a no-op.
    let count2 = BudgetEngine::reset_if_due(db.conn(), now).unwrap();
    assert_eq!(count2, 0, "no reset on second call within the same period");
}

#[test]
fn rate_card_replay_is_deterministic_across_effective_at_boundary() {
    // Two rate-card rows: T0 at $2.50/$10, T1 = T0+30d at $3.00/$12.
    // A call at T0+5d must price against the older row; T0+60d picks the
    // newer. Repeat 100x — exact bit-equal results (decimal strings, not
    // floats) prove the rate-card → USD path is deterministic for replay.
    let db = Database::test_db();
    let t0: i64 = 1_700_000_000;
    let t1 = t0 + 30 * 86_400;
    RateCard::append(
        db.conn(),
        "openai",
        "gpt-4o",
        t0,
        Usd::parse("2.50").unwrap(),
        Usd::parse("10.00").unwrap(),
        None,
        "test-1",
    )
    .unwrap();
    RateCard::append(
        db.conn(),
        "openai",
        "gpt-4o",
        t1,
        Usd::parse("3.00").unwrap(),
        Usd::parse("12.00").unwrap(),
        None,
        "test-2",
    )
    .unwrap();

    let early = RateCard::lookup(db.conn(), "openai", "gpt-4o", t0 + 5 * 86_400)
        .unwrap()
        .unwrap();
    let late = RateCard::lookup(db.conn(), "openai", "gpt-4o", t0 + 60 * 86_400)
        .unwrap()
        .unwrap();

    assert_eq!(early.effective_at, t0);
    assert_eq!(late.effective_at, t1);
    assert_eq!(early.prompt_per_mtok.to_storage_string(), "2.50");
    assert_eq!(late.prompt_per_mtok.to_storage_string(), "3.00");

    // 100k prompt + 50k completion priced at each row → exact equality
    // across 100 repeats.
    let early_priced = RateCard::price(&early, 100_000, 50_000);
    let late_priced = RateCard::price(&late, 100_000, 50_000);
    for _ in 0..100 {
        assert_eq!(
            RateCard::price(&early, 100_000, 50_000),
            early_priced,
            "deterministic across repeats"
        );
        assert_eq!(RateCard::price(&late, 100_000, 50_000), late_priced);
    }
    // 100k * $2.50/M + 50k * $10/M = $0.25 + $0.50 = $0.75
    assert_eq!(early_priced, Usd::parse("0.75").unwrap());
    // 100k * $3.00/M + 50k * $12/M = $0.30 + $0.60 = $0.90
    assert_eq!(late_priced, Usd::parse("0.90").unwrap());
}

#[test]
fn rate_card_refresh_appends_never_mutates() {
    // After a "refresh" at a new effective_at, the original row must
    // still be retrievable — historical replays of audit-chain events
    // that pinned the old `effective_at` resolve identically. Use a
    // synthetic (provider, model) pair so the bundled-snapshot rows
    // don't collide with the test fixture.
    let db = Database::test_db();
    let t0: i64 = 1_700_000_000;
    let t1 = t0 + 30 * 86_400;
    RateCard::append(
        db.conn(),
        "test-provider",
        "test-model",
        t0,
        Usd::parse("2.50").unwrap(),
        Usd::parse("10.00").unwrap(),
        None,
        "bundled",
    )
    .unwrap();
    RateCard::append(
        db.conn(),
        "test-provider",
        "test-model",
        t1,
        Usd::parse("3.00").unwrap(),
        Usd::parse("12.00").unwrap(),
        None,
        "manifest_2026-04",
    )
    .unwrap();
    let count: i64 = db
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM provider_rate_cards
             WHERE provider_id='test-provider' AND model='test-model'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 2, "refresh appends, never updates");

    // A historical lookup pinned to t0+5d still resolves to the bundled row.
    let row = RateCard::lookup(db.conn(), "test-provider", "test-model", t0 + 5 * 86_400)
        .unwrap()
        .unwrap();
    assert_eq!(row.source, "bundled");
    assert_eq!(row.prompt_per_mtok, Usd::parse("2.50").unwrap());
}

#[test]
fn migrations_seed_default_budget_and_rate_cards() {
    // Schema must seed exactly one `default` budget row and at least one
    // rate-card row from the bundled snapshot. If either is missing,
    // post-flight reconciliation silently records zero spend (the
    // regression this guards against).
    let db = Database::test_db();
    let budget_count: i64 = db
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM budgets WHERE scope = 'default'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        budget_count, 1,
        "schema must seed exactly one default budget row"
    );

    let rate_count: i64 = db
        .conn()
        .query_row("SELECT COUNT(*) FROM provider_rate_cards", [], |r| r.get(0))
        .unwrap();
    assert!(rate_count > 0, "schema must seed bundled rate-card rows");

    // A representative seed row exists with the price we set in
    // bundled_rate_card_entries (anthropic/claude-sonnet-4 at $3 / $15).
    let row = RateCard::lookup(db.conn(), "anthropic", "claude-sonnet-4", i64::MAX)
        .unwrap()
        .unwrap();
    assert_eq!(row.prompt_per_mtok, Usd::parse("3.00").unwrap());
    assert_eq!(row.completion_per_mtok, Usd::parse("15.00").unwrap());
}

#[test]
fn rate_card_wildcard_fallback_for_local_providers() {
    // Local providers seed model = '*' so post_flight has *something* to
    // price against (zero) instead of returning None and silently skipping
    // the spend record.
    let db = Database::test_db();
    let row = RateCard::lookup(db.conn(), "ollama", "llama3:8b", i64::MAX)
        .unwrap()
        .expect("wildcard rate-card row must resolve unknown ollama models");
    assert_eq!(row.prompt_per_mtok, Usd::parse("0").unwrap());
    assert_eq!(row.completion_per_mtok, Usd::parse("0").unwrap());
    assert_eq!(RateCard::price(&row, 1_000_000, 1_000_000), Usd::ZERO);
}

#[test]
fn hook_uses_prompt_tokens_estimate_to_refuse_at_hard_cap() {
    // Regression for the BudgetHook estimate fix: spec §Pipeline step 1 says
    // pre-flight USD comes from `count_tokens` × rate card. The previous
    // implementation used `message_count * 50` and missed cap crossings on
    // long prompts. With anthropic/claude-sonnet-4 priced at $3/Mtok,
    // 200k prompt tokens = $0.60 — at $19.50/$20 spent that trips the cap,
    // whereas a 5-message × 50-token heuristic ($0.00075) would have let
    // it through.
    use crate::budgets::hook::BudgetHook;
    use crate::hooks::{Hook, HookAction, HookContext, HookData, HookPoint};

    let db = Database::test_db();
    db.conn()
        .execute(
            "INSERT OR REPLACE INTO settings (key, value) VALUES ('providers.default_id', 'anthropic')",
            [],
        )
        .unwrap();
    db.conn()
        .execute(
            "INSERT OR REPLACE INTO settings (key, value) VALUES ('model', 'claude-sonnet-4')",
            [],
        )
        .unwrap();
    override_default(&db, "20.00", Some("5.00"), "daily");
    set_spent(&db, "default", "19.50", chrono::Utc::now().timestamp());

    let hook = BudgetHook::from_database(db);
    let ctx = HookContext {
        point: HookPoint::BeforeLlmCall,
        session_id: "test".to_string(),
        turn_count: 1,
        data: HookData::LlmCall {
            message_count: 5,
            prompt_tokens: Some(200_000),
        },
    };

    assert!(
        matches!(hook.execute(&ctx), HookAction::Skip { .. }),
        "fat prompt must trip the hard cap via prompt_tokens"
    );
}

#[test]
fn hook_falls_back_to_message_count_heuristic_when_prompt_tokens_absent() {
    // When prompt_tokens is None (legacy callers / tests), the hook falls
    // back to message_count × 50 so the budget gate keeps working — just
    // less precise. 3 × 50 × $3/Mtok ≈ $0.00045 → well under cap → Continue.
    use crate::budgets::hook::BudgetHook;
    use crate::hooks::{Hook, HookAction, HookContext, HookData, HookPoint};

    let db = Database::test_db();
    db.conn()
        .execute(
            "INSERT OR REPLACE INTO settings (key, value) VALUES ('providers.default_id', 'anthropic')",
            [],
        )
        .unwrap();
    db.conn()
        .execute(
            "INSERT OR REPLACE INTO settings (key, value) VALUES ('model', 'claude-sonnet-4')",
            [],
        )
        .unwrap();
    override_default(&db, "20.00", None, "daily");
    set_spent(&db, "default", "1.00", chrono::Utc::now().timestamp());

    let hook = BudgetHook::from_database(db);
    let ctx = HookContext {
        point: HookPoint::BeforeLlmCall,
        session_id: "test".to_string(),
        turn_count: 1,
        data: HookData::LlmCall {
            message_count: 3,
            prompt_tokens: None,
        },
    };

    assert!(matches!(hook.execute(&ctx), HookAction::Continue));
}
