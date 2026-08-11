use super::*;
use std::sync::atomic::{AtomicI64, Ordering};

#[derive(Debug)]
struct TestClock {
    now: AtomicI64,
}
impl TestClock {
    fn new(start_ms: i64) -> Arc<Self> {
        Arc::new(Self {
            now: AtomicI64::new(start_ms),
        })
    }
    fn advance(&self, d: Duration) {
        self.now.fetch_add(d.as_millis() as i64, Ordering::SeqCst);
    }
}
impl Clock for TestClock {
    fn now_ms(&self) -> i64 {
        self.now.load(Ordering::SeqCst)
    }
}

fn open_test_db() -> (Database, std::path::PathBuf, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.db");
    let db = Database::open_at(&path).unwrap();
    (db, path, dir)
}

fn cfg_with(keys: &[(&str, &str)]) -> PoolLoadConfig {
    let entries = keys
        .iter()
        .enumerate()
        .map(|(i, (lbl, k))| ConfigEntry {
            key: (*k).into(),
            label: Some((*lbl).into()),
            config_name: format!("llm.api_keys[{i}]"),
        })
        .collect();
    PoolLoadConfig {
        strategy: CredentialStrategy::FillFirst,
        cooldown_secs: 60,
        max_concurrent_per_credential: 1,
        proactive_rate_limit_threshold_pct: 0,
        proactive_rate_limit_min_window_secs: 0,
        config_entries: entries,
        env_vars: vec![],
        oauth_home: None,
    }
}

// ── A1. Construction + load + seeding ──

#[test]
fn load_empty_config_no_env_yields_empty_pool() {
    let (db, path, _g) = open_test_db();
    let pool = CredentialPool::load(
        &db,
        &path,
        Provider::OpenRouter,
        PoolLoadConfig {
            strategy: CredentialStrategy::FillFirst,
            cooldown_secs: 60,
            max_concurrent_per_credential: 1,
            proactive_rate_limit_threshold_pct: 0,
            proactive_rate_limit_min_window_secs: 0,
            config_entries: vec![],
            env_vars: vec!["BORG_TEST_NEVER_SET_XYZ".into()],
            oauth_home: None,
        },
    )
    .unwrap();
    assert!(!pool.has_credentials());
    assert!(pool.pick().is_none());
}

#[test]
fn load_with_one_manual_key_seeds_one_entry() {
    let (db, path, _g) = open_test_db();
    let pool = CredentialPool::load(
        &db,
        &path,
        Provider::OpenRouter,
        cfg_with(&[("alpha", "k1")]),
    )
    .unwrap();
    assert_eq!(pool.entries().len(), 1);
    assert_eq!(pool.entries()[0].label, "alpha");
}

#[test]
fn load_dedupes_same_key_under_two_refs() {
    // Same plaintext key under two `SecretRef` entries collapses to one.
    let (db, path, _g) = open_test_db();
    let pool = CredentialPool::load(
        &db,
        &path,
        Provider::OpenRouter,
        cfg_with(&[("a", "samekey"), ("b", "samekey")]),
    )
    .unwrap();
    assert_eq!(pool.entries().len(), 1);
}

#[test]
fn load_keeps_distinct_keys_separate() {
    let (db, path, _g) = open_test_db();
    let pool = CredentialPool::load(
        &db,
        &path,
        Provider::OpenRouter,
        cfg_with(&[("a", "k1"), ("b", "k2")]),
    )
    .unwrap();
    assert_eq!(pool.entries().len(), 2);
}

// ── A2. peek / pick per strategy ──

#[test]
fn fill_first_sticks_to_head() {
    let (db, path, _g) = open_test_db();
    let pool = CredentialPool::load(
        &db,
        &path,
        Provider::OpenRouter,
        cfg_with(&[("a", "k1"), ("b", "k2")]),
    )
    .unwrap();
    let first = pool.pick().unwrap();
    let second = pool.pick().unwrap();
    let third = pool.pick().unwrap();
    assert_eq!(first.id, second.id);
    assert_eq!(second.id, third.id);
}

#[test]
fn round_robin_rotates() {
    let (db, path, _g) = open_test_db();
    let mut cfg = cfg_with(&[("a", "k1"), ("b", "k2"), ("c", "k3")]);
    cfg.strategy = CredentialStrategy::RoundRobin;
    let pool = CredentialPool::load(&db, &path, Provider::OpenRouter, cfg).unwrap();
    let mut counts: HashMap<String, u32> = HashMap::new();
    for _ in 0..9 {
        let c = pool.pick().unwrap();
        *counts.entry(c.id).or_insert(0) += 1;
    }
    for (_, v) in counts {
        assert_eq!(v, 3);
    }
}

#[test]
fn least_used_picks_lowest_count() {
    let (db, path, _g) = open_test_db();
    let mut cfg = cfg_with(&[("a", "k1"), ("b", "k2"), ("c", "k3")]);
    cfg.strategy = CredentialStrategy::LeastUsed;
    let pool = CredentialPool::load(&db, &path, Provider::OpenRouter, cfg).unwrap();
    let mut seen: HashSet<String> = HashSet::new();
    for _ in 0..3 {
        seen.insert(pool.pick().unwrap().id);
    }
    // First three picks must touch every key.
    assert_eq!(seen.len(), 3);
}

#[test]
fn random_spreads_picks() {
    let (db, path, _g) = open_test_db();
    let mut cfg = cfg_with(&[("a", "k1"), ("b", "k2"), ("c", "k3")]);
    cfg.strategy = CredentialStrategy::Random;
    let pool = CredentialPool::load(&db, &path, Provider::OpenRouter, cfg).unwrap();
    let mut counts: HashMap<String, u32> = HashMap::new();
    for _ in 0..600 {
        *counts.entry(pool.pick().unwrap().id).or_insert(0) += 1;
    }
    for (_, v) in counts {
        assert!(v > 100, "expected even-ish spread, got {v}");
    }
}

// ── A3. report_failure transitions ──

#[test]
fn rate_limit_cools_entry() {
    let (db, path, _g) = open_test_db();
    let clock = TestClock::new(1_000_000);
    let pool = CredentialPool::load_with_clock(
        &db,
        &path,
        Provider::OpenRouter,
        cfg_with(&[("a", "k1")]),
        clock.clone(),
    )
    .unwrap();
    let cred = pool.pick().unwrap();
    pool.report_failure(&cred.id, FailoverReason::RateLimit, Some(429), None, None);
    assert!(pool.is_exhausted());
    clock.advance(Duration::from_secs(61));
    assert!(!pool.is_exhausted());
}

#[test]
fn auth_failure_removes_permanently() {
    let (db, path, _g) = open_test_db();
    let pool = CredentialPool::load(
        &db,
        &path,
        Provider::OpenRouter,
        cfg_with(&[("a", "k1"), ("b", "k2")]),
    )
    .unwrap();
    let cred = pool.pick().unwrap();
    pool.report_failure(&cred.id, FailoverReason::Auth, Some(401), None, None);
    let entries = pool.entries();
    let removed = entries.iter().find(|c| c.id == cred.id).unwrap();
    assert!(removed.is_removed());
    // Pool still has the other key.
    assert!(pool.has_available());
}

#[test]
fn auth_permanent_also_removes() {
    let (db, path, _g) = open_test_db();
    let pool =
        CredentialPool::load(&db, &path, Provider::OpenRouter, cfg_with(&[("a", "k1")])).unwrap();
    let cred = pool.pick().unwrap();
    pool.report_failure(
        &cred.id,
        FailoverReason::AuthPermanent,
        Some(403),
        None,
        None,
    );
    assert!(pool.entries()[0].is_removed());
}

#[test]
fn transport_errors_dont_mutate_pool() {
    let (db, path, _g) = open_test_db();
    let pool =
        CredentialPool::load(&db, &path, Provider::OpenRouter, cfg_with(&[("a", "k1")])).unwrap();
    let cred = pool.pick().unwrap();
    let baseline_use = pool.entries()[0].use_count;
    for r in [
        FailoverReason::Overloaded,
        FailoverReason::Timeout,
        FailoverReason::Format,
        FailoverReason::ContextOverflow,
        FailoverReason::PayloadTooLarge,
        FailoverReason::ModelNotFound,
        FailoverReason::Unknown,
    ] {
        pool.report_failure(&cred.id, r, None, None, None);
    }
    let after = &pool.entries()[0];
    assert_eq!(after.use_count, baseline_use);
    assert!(!after.is_removed());
    assert!(matches!(after.last_status, CredentialStatus::Ok));
}

#[test]
fn retry_after_overrides_default_cooldown() {
    let (db, path, _g) = open_test_db();
    let clock = TestClock::new(1_000_000);
    let pool = CredentialPool::load_with_clock(
        &db,
        &path,
        Provider::OpenRouter,
        cfg_with(&[("a", "k1")]),
        clock.clone(),
    )
    .unwrap();
    let cred = pool.pick().unwrap();
    pool.report_failure(
        &cred.id,
        FailoverReason::RateLimit,
        Some(429),
        Some(Duration::from_secs(600)),
        None,
    );
    let until = pool.entries()[0].last_error_reset_at_ms.unwrap();
    // Tight equality, not `>=`: the explicit 600s retry_after must drive
    // the cooldown exactly. A regression that fell back to the configured
    // 60s default would land at 1_060_000 (caught); a runaway clamp to 24h
    // would land at 1_000_000 + 86_400_000 (also caught).
    assert_eq!(until, 1_000_000 + 600_000);
}

#[test]
fn cooldown_extends_never_shortens() {
    let (db, path, _g) = open_test_db();
    let clock = TestClock::new(1_000_000);
    let pool = CredentialPool::load_with_clock(
        &db,
        &path,
        Provider::OpenRouter,
        cfg_with(&[("a", "k1")]),
        clock.clone(),
    )
    .unwrap();
    let cred = pool.pick().unwrap();
    pool.report_failure(
        &cred.id,
        FailoverReason::RateLimit,
        None,
        Some(Duration::from_secs(600)),
        None,
    );
    let long = pool.entries()[0].last_error_reset_at_ms.unwrap();
    pool.report_failure(
        &cred.id,
        FailoverReason::RateLimit,
        None,
        Some(Duration::from_secs(10)),
        None,
    );
    let after = pool.entries()[0].last_error_reset_at_ms.unwrap();
    assert_eq!(after, long, "shorter cooldown must not win");
}

#[test]
fn report_failure_on_removed_is_noop() {
    let (db, path, _g) = open_test_db();
    let pool =
        CredentialPool::load(&db, &path, Provider::OpenRouter, cfg_with(&[("a", "k1")])).unwrap();
    let cred = pool.pick().unwrap();
    pool.report_failure(&cred.id, FailoverReason::Auth, Some(401), None, None);
    let removed_at = pool.entries()[0].removed_at_ms;
    pool.report_failure(&cred.id, FailoverReason::RateLimit, Some(429), None, None);
    // No state change after the removal.
    assert_eq!(pool.entries()[0].removed_at_ms, removed_at);
    assert!(pool.entries()[0].last_error_reset_at_ms.is_none());
}

#[test]
fn error_message_truncates_at_cap() {
    let (db, path, _g) = open_test_db();
    let pool =
        CredentialPool::load(&db, &path, Provider::OpenRouter, cfg_with(&[("a", "k1")])).unwrap();
    let cred = pool.pick().unwrap();
    let big = "x".repeat(ERROR_MESSAGE_MAX_LEN * 3);
    pool.report_failure(&cred.id, FailoverReason::RateLimit, None, None, Some(&big));
    let msg = pool.entries()[0].last_error_message.clone().unwrap();
    assert!(msg.contains("truncated"));
    assert!(msg.len() < big.len());
}

// ── A4. report_success ──

#[test]
fn success_clears_cooldown_and_error_state() {
    let (db, path, _g) = open_test_db();
    let clock = TestClock::new(1_000_000);
    let pool = CredentialPool::load_with_clock(
        &db,
        &path,
        Provider::OpenRouter,
        cfg_with(&[("a", "k1")]),
        clock,
    )
    .unwrap();
    let cred = pool.pick().unwrap();
    pool.report_failure(&cred.id, FailoverReason::RateLimit, Some(429), None, None);
    assert!(pool.is_exhausted());
    pool.report_success(&cred.id);
    let after = &pool.entries()[0];
    assert!(matches!(after.last_status, CredentialStatus::Ok));
    assert!(after.last_error_code.is_none());
    assert!(after.last_error_reset_at_ms.is_none());
    assert!(pool.has_available());
}

#[test]
fn success_on_removed_is_noop() {
    let (db, path, _g) = open_test_db();
    let pool =
        CredentialPool::load(&db, &path, Provider::OpenRouter, cfg_with(&[("a", "k1")])).unwrap();
    let cred = pool.pick().unwrap();
    pool.report_failure(&cred.id, FailoverReason::Auth, Some(401), None, None);
    pool.report_success(&cred.id);
    assert!(pool.entries()[0].is_removed());
}

// ── A5. Exhaustion / recovery ──

#[test]
fn single_key_exhausts() {
    let (db, path, _g) = open_test_db();
    let clock = TestClock::new(1_000_000);
    let pool = CredentialPool::load_with_clock(
        &db,
        &path,
        Provider::OpenRouter,
        cfg_with(&[("a", "k1")]),
        clock.clone(),
    )
    .unwrap();
    let cred = pool.pick().unwrap();
    pool.report_failure(&cred.id, FailoverReason::RateLimit, None, None, None);
    assert!(pool.is_exhausted());
    clock.advance(Duration::from_secs(61));
    assert!(!pool.is_exhausted());
}

#[test]
fn next_recovery_at_returns_earliest() {
    let (db, path, _g) = open_test_db();
    let clock = TestClock::new(1_000_000);
    let pool = CredentialPool::load_with_clock(
        &db,
        &path,
        Provider::OpenRouter,
        cfg_with(&[("a", "k1"), ("b", "k2")]),
        clock,
    )
    .unwrap();
    let entries = pool.entries();
    pool.report_failure(
        &entries[0].id,
        FailoverReason::RateLimit,
        None,
        Some(Duration::from_secs(100)),
        None,
    );
    pool.report_failure(
        &entries[1].id,
        FailoverReason::RateLimit,
        None,
        Some(Duration::from_secs(50)),
        None,
    );
    let next = pool.next_recovery_at_ms().unwrap();
    // Earliest = entry[1] at +50s.
    assert!(next < 1_000_000 + 60_000);
}

// ── A6. pick_force ──

#[test]
fn pick_force_returns_soonest_recovery() {
    let (db, path, _g) = open_test_db();
    let clock = TestClock::new(1_000_000);
    let pool = CredentialPool::load_with_clock(
        &db,
        &path,
        Provider::OpenRouter,
        cfg_with(&[("a", "k1"), ("b", "k2"), ("c", "k3")]),
        clock,
    )
    .unwrap();
    let entries = pool.entries();
    pool.report_failure(
        &entries[0].id,
        FailoverReason::RateLimit,
        None,
        Some(Duration::from_secs(100)),
        None,
    );
    pool.report_failure(
        &entries[1].id,
        FailoverReason::RateLimit,
        None,
        Some(Duration::from_secs(50)),
        None,
    );
    pool.report_failure(
        &entries[2].id,
        FailoverReason::RateLimit,
        None,
        Some(Duration::from_secs(200)),
        None,
    );
    let chosen = pool.pick_force().unwrap();
    assert_eq!(chosen.id, entries[1].id);
}

#[test]
fn pick_force_bumps_use_count() {
    // Regression for the "salvage shows used 0×" bug: pick_force is the
    // last-gasp path against a cooled key, and its accounting feeds both
    // least_used selection and operator-visible `borg auth list` columns.
    let (db, path, _g) = open_test_db();
    let pool =
        CredentialPool::load(&db, &path, Provider::OpenRouter, cfg_with(&[("a", "k1")])).unwrap();
    let cred = pool.pick().unwrap();
    pool.report_failure(&cred.id, FailoverReason::RateLimit, None, None, None);
    let before = pool.entries()[0].use_count;
    let _ = pool.pick_force().unwrap();
    let after = pool.entries()[0].use_count;
    assert_eq!(after, before + 1);
}

#[test]
fn pick_force_returns_none_when_all_removed() {
    let (db, path, _g) = open_test_db();
    let pool = CredentialPool::load(
        &db,
        &path,
        Provider::OpenRouter,
        cfg_with(&[("a", "k1"), ("b", "k2")]),
    )
    .unwrap();
    for e in pool.entries() {
        pool.report_failure(&e.id, FailoverReason::Auth, Some(401), None, None);
    }
    assert!(pool.pick_force().is_none());
}

// ── A8. reset_cooldowns ──

#[test]
fn reset_cooldowns_clears_exhausted_only() {
    let (db, path, _g) = open_test_db();
    let clock = TestClock::new(1_000_000);
    let pool = CredentialPool::load_with_clock(
        &db,
        &path,
        Provider::OpenRouter,
        cfg_with(&[("a", "k1"), ("b", "k2"), ("c", "k3")]),
        clock,
    )
    .unwrap();
    let entries = pool.entries();
    pool.report_failure(&entries[0].id, FailoverReason::RateLimit, None, None, None);
    pool.report_failure(&entries[1].id, FailoverReason::Auth, Some(401), None, None);
    let cleared = pool.reset_cooldowns();
    assert_eq!(cleared, 1);
    // The removed entry stays removed.
    let after = pool.entries();
    assert!(after
        .iter()
        .find(|c| c.id == entries[1].id)
        .unwrap()
        .is_removed());
    // The cooled entry is now Ok.
    let zero = after.iter().find(|c| c.id == entries[0].id).unwrap();
    assert!(matches!(zero.last_status, CredentialStatus::Ok));
}

#[test]
fn reset_cooldowns_idempotent() {
    let (db, path, _g) = open_test_db();
    let pool =
        CredentialPool::load(&db, &path, Provider::OpenRouter, cfg_with(&[("a", "k1")])).unwrap();
    let cred = pool.pick().unwrap();
    pool.report_failure(&cred.id, FailoverReason::RateLimit, None, None, None);
    let n1 = pool.reset_cooldowns();
    let n2 = pool.reset_cooldowns();
    assert_eq!(n1, 1);
    assert_eq!(n2, 0);
}

// ── A9. Leasing ──

#[test]
fn acquire_lease_returns_below_cap_keys() {
    let (db, path, _g) = open_test_db();
    let pool = CredentialPool::load(
        &db,
        &path,
        Provider::OpenRouter,
        cfg_with(&[("a", "k1"), ("b", "k2"), ("c", "k3")]),
    )
    .unwrap();
    let l1 = pool.acquire_lease(None).unwrap();
    let l2 = pool.acquire_lease(None).unwrap();
    let l3 = pool.acquire_lease(None).unwrap();
    let ids: HashSet<String> = [&l1, &l2, &l3].iter().map(|l| l.id().to_string()).collect();
    assert_eq!(ids.len(), 3);
}

#[test]
fn lease_drop_decrements_counter() {
    let (db, path, _g) = open_test_db();
    let pool =
        CredentialPool::load(&db, &path, Provider::OpenRouter, cfg_with(&[("a", "k1")])).unwrap();
    {
        let _l = pool.acquire_lease(None).unwrap();
    }
    // After drop, a fresh lease still succeeds (counter decremented).
    let _l2 = pool.acquire_lease(None).unwrap();
}

#[test]
fn acquire_lease_when_all_capped_still_returns_one() {
    let (db, path, _g) = open_test_db();
    let mut cfg = cfg_with(&[("a", "k1")]);
    cfg.max_concurrent_per_credential = 1;
    let pool = CredentialPool::load(&db, &path, Provider::OpenRouter, cfg).unwrap();
    let _l1 = pool.acquire_lease(None).unwrap();
    let l2 = pool.acquire_lease(None);
    // Single key + cap=1 still returns a lease (least-leased fallback),
    // letting concurrent requests degrade gracefully rather than block.
    assert!(l2.is_some());
}

#[test]
fn acquire_lease_unknown_id_returns_none() {
    let (db, path, _g) = open_test_db();
    let pool =
        CredentialPool::load(&db, &path, Provider::OpenRouter, cfg_with(&[("a", "k1")])).unwrap();
    assert!(pool.acquire_lease(Some("bogus")).is_none());
}

#[test]
fn credential_source_round_trips_every_variant_through_serde() {
    // Schema contract: every CredentialSource variant must serialize and
    // deserialize losslessly through the same `source_json` column used by
    // `upsert_row` and the `load` SELECT. A typo in `#[serde(rename)]` for
    // any new variant would silently rewrite the persistence format,
    // stranding existing rows.
    let cases = vec![
        CredentialSource::Manual,
        CredentialSource::Env {
            var: "OPENROUTER_API_KEY".into(),
        },
        CredentialSource::Config {
            name: "llm.api_keys[0]".into(),
        },
        CredentialSource::OAuth {
            name: "claude_code".into(),
        },
        CredentialSource::Custom {
            name: "local-vllm".into(),
            base_url: "http://127.0.0.1:8000".into(),
        },
    ];
    for src in cases {
        let s = serde_json::to_string(&src).expect("serialize");
        let back: CredentialSource = serde_json::from_str(&s).expect("deserialize");
        assert_eq!(back, src, "round-trip failed for {src:?}: {s}");
    }
}

#[test]
fn credential_pool_table_has_phase_one_columns() {
    // Migration contract: a fresh DB must expose the five new columns added
    // for OAuth + custom-endpoint parity. Catches the case where someone
    // adds a Rust field but forgets the SQL — load would then fail at
    // SELECT-time and silently fall through to the poolless rotation path.
    let (db, _path, _g) = open_test_db();
    let conn = db.conn();
    let mut stmt = conn.prepare("PRAGMA table_info(credential_pool)").unwrap();
    let cols: HashSet<String> = stmt
        .query_map([], |r| r.get::<_, String>(1))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    for expected in [
        "auth_type",
        "expires_at_ms",
        "base_url",
        "extra_json",
        // sanity-check one preexisting column to confirm the PRAGMA path
        // actually returned schema and not an empty set.
        "fingerprint",
    ] {
        assert!(
            cols.contains(expected),
            "missing column `{expected}` in credential_pool: have {cols:?}"
        );
    }
}

#[test]
fn max_concurrent_cap_round_trips_through_config_and_clamps() {
    // The cap must be read from PoolLoadConfig (not hard-coded) and the
    // `.max(1)` clamp at load time keeps cap=0 from bricking the pool —
    // every available credential would otherwise be over-cap forever.
    let (db, path, _g) = open_test_db();
    let mut cfg = cfg_with(&[("a", "k1")]);
    cfg.max_concurrent_per_credential = 5;
    let pool = CredentialPool::load(&db, &path, Provider::OpenRouter, cfg).unwrap();
    assert_eq!(pool.max_concurrent_per_credential(), 5);

    let (db2, path2, _g2) = open_test_db();
    let mut cfg2 = cfg_with(&[("a", "k1")]);
    cfg2.max_concurrent_per_credential = 0;
    let pool2 = CredentialPool::load(&db2, &path2, Provider::OpenRouter, cfg2).unwrap();
    assert_eq!(
        pool2.max_concurrent_per_credential(),
        1,
        "cap=0 must clamp to 1"
    );
}

#[test]
fn max_concurrent_cap_allows_multiple_leases_per_key_when_under_cap() {
    // cap=2 + single key: two concurrent leases must both succeed against
    // the same id (cap-aware path stays in the below_cap branch). If the
    // cap were silently 1, the second lease would still succeed via the
    // graceful-degradation fallback — but a third lease would not be
    // distinguishable. So we also verify the per-key counter decrements
    // on drop by re-leasing after one drop.
    let (db, path, _g) = open_test_db();
    let mut cfg = cfg_with(&[("a", "k1")]);
    cfg.max_concurrent_per_credential = 2;
    let pool = CredentialPool::load(&db, &path, Provider::OpenRouter, cfg).unwrap();
    let l1 = pool.acquire_lease(None).unwrap();
    let l2 = pool.acquire_lease(None).unwrap();
    assert_eq!(l1.id(), l2.id(), "single key — both leases land on it");
    let id1 = l1.id().to_string();
    drop(l1);
    let l3 = pool.acquire_lease(None).unwrap();
    assert_eq!(l3.id(), id1, "post-drop lease re-uses the same key");
}

#[test]
fn max_concurrent_cap_distributes_across_keys_under_cap() {
    // cap=2 + two keys: first two leases must land on *different* keys.
    // The tiebreak `(active_leases, priority)` prefers the unleased key
    // over re-leasing the first even though both are below cap. Four
    // leases distribute evenly (2 per key). If anyone breaks the
    // below-cap preference, the first key gets all leases.
    let (db, path, _g) = open_test_db();
    let mut cfg = cfg_with(&[("a", "k1"), ("b", "k2")]);
    cfg.max_concurrent_per_credential = 2;
    let pool = CredentialPool::load(&db, &path, Provider::OpenRouter, cfg).unwrap();
    let l1 = pool.acquire_lease(None).unwrap();
    let l2 = pool.acquire_lease(None).unwrap();
    assert_ne!(
        l1.id(),
        l2.id(),
        "below-cap preference must spread before doubling up"
    );
    let l3 = pool.acquire_lease(None).unwrap();
    let l4 = pool.acquire_lease(None).unwrap();
    let mut counts: HashMap<String, usize> = HashMap::new();
    for id in [l1.id(), l2.id(), l3.id(), l4.id()] {
        *counts.entry(id.to_string()).or_insert(0) += 1;
    }
    assert_eq!(counts.len(), 2, "both keys leased");
    assert!(
        counts.values().all(|&n| n == 2),
        "even distribution under cap: got {counts:?}"
    );
}

// ── A10. add_entry / remove_index / resolve_target ──

#[test]
fn add_entry_appends_and_returns_idempotent_on_dup() {
    let (db, path, _g) = open_test_db();
    let pool =
        CredentialPool::load(&db, &path, Provider::OpenRouter, cfg_with(&[("a", "k1")])).unwrap();
    let added = pool.add_entry("k2".into(), "added".into(), CredentialSource::Manual);
    assert_eq!(pool.entries().len(), 2);
    let again = pool.add_entry("k2".into(), "ignored".into(), CredentialSource::Manual);
    assert_eq!(again.id, added.id);
    assert_eq!(pool.entries().len(), 2);
}

#[test]
fn add_entry_full_preserves_oauth_metadata_across_reload() {
    // Regression for the bug where `pair_credential` used `add_entry` and
    // silently downgraded OAuth credentials to API keys (no auth_type=oauth,
    // no refresh_token, no expires_at, no base_url). Refresh would never
    // fire and the credential would die when the access token expired.
    //
    // Asserts on the reloaded pool, not the in-memory return value — the
    // real win is that auth_type/expires_at survive SQLite round-trip.
    // (refresh_token is intentionally NOT persisted to DB; the rotated
    // value lives in the source file via `write_back`.)
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home.path().join(".claude")).unwrap();
    std::fs::write(
        home.path().join(".claude/.credentials.json"),
        r#"{"claudeAiOauth":{"accessToken":"access-token","refreshToken":"refresh-token","expiresAt":9999999999999}}"#,
    )
    .unwrap();
    let (db, path, _g) = open_test_db();
    let pool = CredentialPool::load(
        &db,
        &path,
        Provider::ClaudeCodeOAuth,
        oauth_cfg(home.path()),
    )
    .unwrap();
    let cred = pool.add_entry_full(
        "access-token".into(),
        "claude-code:abc123".into(),
        CredentialSource::OAuth {
            name: "claude_code".into(),
        },
        AuthType::OAuth,
        Some("refresh-token".into()),
        Some(9_999_999_999_999),
        None,
        None,
    );
    assert_eq!(cred.auth_type, AuthType::OAuth);
    assert_eq!(cred.refresh_token.as_deref(), Some("refresh-token"));

    // Drop the live pool and reload — proves the OAuth metadata
    // round-trips through SQLite. Without this assertion the test would
    // pass even if `upsert_row` silently dropped auth_type=oauth.
    drop(pool);
    let pool2 = CredentialPool::load(
        &db,
        &path,
        Provider::ClaudeCodeOAuth,
        oauth_cfg(home.path()),
    )
    .unwrap();
    let entries = pool2.entries();
    let reloaded = entries
        .iter()
        .find(|c| c.id == cred.id)
        .expect("entry must survive reload");
    assert_eq!(
        reloaded.auth_type,
        AuthType::OAuth,
        "auth_type must round-trip — otherwise refresh path takes the API-key fast-path"
    );
    assert_eq!(reloaded.expires_at_ms, Some(9_999_999_999_999));
    assert!(matches!(
        reloaded.source,
        CredentialSource::OAuth { ref name } if name == "claude_code"
    ));
}

#[test]
fn add_entry_full_with_custom_base_url_round_trips() {
    // Custom-endpoint credentials carry a per-credential `base_url` that
    // the HTTP path consults at request time. The pool must preserve it
    // through `add_entry_full` so `LeaseHandle::base_url()` returns it.
    let (db, path, _g) = open_test_db();
    let pool = CredentialPool::load(
        &db,
        &path,
        Provider::Custom,
        oauth_cfg(std::path::Path::new("/nonexistent")),
    )
    .unwrap();
    let cred = pool.add_entry_full(
        "qwen-api-key".into(),
        "qwen-cli".into(),
        CredentialSource::Custom {
            name: "qwen_cli".into(),
            base_url: "https://dashscope.aliyuncs.com/api/v1".into(),
        },
        AuthType::ApiKey,
        None,
        None,
        Some("https://dashscope.aliyuncs.com/api/v1".into()),
        None,
    );
    assert_eq!(
        cred.base_url.as_deref(),
        Some("https://dashscope.aliyuncs.com/api/v1")
    );
    // Lease should surface the override.
    let lease = pool.acquire_lease(Some(&cred.id)).unwrap();
    assert_eq!(
        lease.base_url().as_deref(),
        Some("https://dashscope.aliyuncs.com/api/v1")
    );
}

// `is_safe_base_url` invariants live alongside the helper in
// `credential_pool::helpers::tests::is_safe_base_url_blocks_dangerous_schemes`.

#[test]
fn add_entry_resurrects_tombstoned_fingerprint() {
    // Regression: re-adding a previously-removed key used to silently
    // return the tombstone — operator saw "Added credential…" but the
    // pool never used the key. The resurrection path must clear the
    // tombstone, restore the plaintext, and delete the suppression row.
    let (db, path, _g) = open_test_db();
    let pool =
        CredentialPool::load(&db, &path, Provider::OpenRouter, cfg_with(&[("a", "k1")])).unwrap();
    let added = pool.add_entry("k2".into(), "personal".into(), CredentialSource::Manual);
    let _removed = pool.remove_index(2).unwrap();
    assert!(pool.entries()[1].is_removed());

    let resurrected = pool.add_entry("k2".into(), "ignored".into(), CredentialSource::Manual);
    assert_eq!(resurrected.id, added.id, "same entry resurrected, not new");
    assert!(!resurrected.is_removed());
    assert_eq!(resurrected.key, "k2");
    assert!(pool.has_available());
    // Reload from DB to confirm the suppression row was deleted —
    // otherwise the next start would re-tombstone it via the seed path.
    let pool2 = CredentialPool::load(
        &db,
        &path,
        Provider::OpenRouter,
        cfg_with(&[("a", "k1"), ("personal", "k2")]),
    )
    .unwrap();
    let live: Vec<_> = pool2
        .entries()
        .into_iter()
        .filter(|c| !c.is_removed())
        .collect();
    assert!(
        live.iter().any(|c| c.key == "k2"),
        "resurrected key survives reload, suppression cleared"
    );
}

#[test]
fn remove_index_soft_deletes() {
    let (db, path, _g) = open_test_db();
    let pool = CredentialPool::load(
        &db,
        &path,
        Provider::OpenRouter,
        cfg_with(&[("a", "k1"), ("b", "k2")]),
    )
    .unwrap();
    let removed = pool.remove_index(1).unwrap();
    assert!(removed.is_removed());
    // At least one other entry remains pickable.
    let live = pool
        .entries()
        .into_iter()
        .filter(|c| !c.is_removed())
        .count();
    assert_eq!(live, 1);
    assert!(pool.has_available());
}

#[test]
fn remove_index_out_of_range_returns_none() {
    let (db, path, _g) = open_test_db();
    let pool =
        CredentialPool::load(&db, &path, Provider::OpenRouter, cfg_with(&[("a", "k1")])).unwrap();
    assert!(pool.remove_index(99).is_none());
    assert!(pool.remove_index(0).is_none());
}

#[test]
fn resolve_target_by_label_index_and_id() {
    let (db, path, _g) = open_test_db();
    let pool = CredentialPool::load(
        &db,
        &path,
        Provider::OpenRouter,
        cfg_with(&[("primary", "k1"), ("backup", "k2")]),
    )
    .unwrap();
    // By label.
    let (_, c) = pool.resolve_target("backup").unwrap();
    assert_eq!(c.label, "backup");
    // By 1-based index.
    let (_, c) = pool.resolve_target("1").unwrap();
    assert_eq!(c.label, "primary");
    // By id.
    let id = pool.entries()[0].id.clone();
    let (_, c) = pool.resolve_target(&id).unwrap();
    assert_eq!(c.id, id);
    // Unknown.
    assert!(matches!(
        pool.resolve_target("nope"),
        Err(ResolveError::NotFound(_))
    ));
}

#[test]
fn resolve_target_ambiguous_label() {
    let (db, path, _g) = open_test_db();
    let pool = CredentialPool::load(
        &db,
        &path,
        Provider::OpenRouter,
        cfg_with(&[("same", "k1"), ("same", "k2")]),
    )
    .unwrap();
    assert!(matches!(
        pool.resolve_target("same"),
        Err(ResolveError::Ambiguous(_, _))
    ));
}

// ── A11. Persistence round-trip ──

#[test]
fn persistence_survives_reload() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("p.db");
    let clock = TestClock::new(1_000_000);
    {
        let db = Database::open_at(&path).unwrap();
        let pool = CredentialPool::load_with_clock(
            &db,
            &path,
            Provider::OpenRouter,
            cfg_with(&[("a", "k1"), ("b", "k2")]),
            clock.clone(),
        )
        .unwrap();
        // Pick once so 'a' gets a use_count bump that must round-trip.
        let picked = pool.pick().unwrap();
        assert_eq!(picked.label, "a");
        let entries = pool.entries();
        let other_id = entries.iter().find(|c| c.label == "b").unwrap().id.clone();
        pool.report_failure(&other_id, FailoverReason::Auth, Some(401), None, None);
        pool.report_success(&picked.id);
    }
    // Re-open; state should persist.
    let db = Database::open_at(&path).unwrap();
    let pool = CredentialPool::load_with_clock(
        &db,
        &path,
        Provider::OpenRouter,
        cfg_with(&[("a", "k1"), ("b", "k2")]),
        clock,
    )
    .unwrap();
    let entries = pool.entries();
    let removed = entries.iter().find(|c| c.label == "b").unwrap();
    assert!(removed.is_removed());
    let first = entries.iter().find(|c| c.label == "a").unwrap();
    // use_count was bumped at pick() time.
    assert!(first.use_count >= 1);
}

#[test]
fn remove_index_writes_suppression_row() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("p.db");
    let var = "BORG_TEST_SUPPRESS_VAR";
    std::env::set_var(var, "envkey-secret");
    {
        let db = Database::open_at(&path).unwrap();
        let mut cfg = cfg_with(&[]);
        cfg.env_vars = vec![var.to_string()];
        let pool = CredentialPool::load(&db, &path, Provider::OpenRouter, cfg).unwrap();
        assert_eq!(pool.entries().len(), 1);
        pool.remove_index(1).unwrap();
    }
    // Re-load with the env var still set — suppression should win.
    {
        let db = Database::open_at(&path).unwrap();
        let mut cfg = cfg_with(&[]);
        cfg.env_vars = vec![var.to_string()];
        let pool = CredentialPool::load(&db, &path, Provider::OpenRouter, cfg).unwrap();
        let live = pool
            .entries()
            .into_iter()
            .filter(|c| !c.is_removed())
            .count();
        assert_eq!(live, 0, "suppression should prevent re-seed");
    }
    std::env::remove_var(var);
}

// ── Misc ──

// `fingerprint` invariants live alongside the helper in
// `credential_pool::helpers::tests::fingerprint_is_stable_and_short`.

#[test]
fn strategy_parses_and_roundtrips() {
    use std::str::FromStr;
    for s in ["fill_first", "round_robin", "least_used", "random"] {
        let parsed = CredentialStrategy::from_str(s).unwrap();
        assert_eq!(parsed.to_string(), s);
    }
    assert!(CredentialStrategy::from_str("nope").is_err());
}

#[test]
fn entries_includes_removed_tombstones() {
    let (db, path, _g) = open_test_db();
    let pool = CredentialPool::load(
        &db,
        &path,
        Provider::OpenRouter,
        cfg_with(&[("a", "k1"), ("b", "k2")]),
    )
    .unwrap();
    pool.remove_index(1);
    let entries = pool.entries();
    assert_eq!(entries.len(), 2);
    assert!(entries.iter().any(|c| c.is_removed()));
}

// ── Code-review regressions ──

/// `peek` must not bump use_count or set `current_id`. The spec contract
/// is "non-mutating preview"; tests pin it so a future refactor that
/// inlines `peek -> pick` doesn't sneak past review.
#[test]
fn peek_is_non_mutating() {
    let (db, path, _g) = open_test_db();
    let pool = CredentialPool::load(
        &db,
        &path,
        Provider::OpenRouter,
        cfg_with(&[("a", "k1"), ("b", "k2")]),
    )
    .unwrap();
    for _ in 0..5 {
        let _ = pool.peek();
    }
    assert!(
        pool.entries().iter().all(|c| c.use_count == 0),
        "peek must not bump use_count"
    );
    assert!(pool.current().is_none(), "peek must not set current_id");
}

/// `acquire_lease(None)` must bump `use_count` — `LeastUsed` strategy
/// relies on it. Regression for the bug where the lease path silently
/// degraded LeastUsed to priority-only ordering.
#[test]
fn acquire_lease_bumps_use_count() {
    let (db, path, _g) = open_test_db();
    let pool =
        CredentialPool::load(&db, &path, Provider::OpenRouter, cfg_with(&[("a", "k1")])).unwrap();
    let lease = pool.acquire_lease(None).expect("lease");
    drop(lease);
    let count = pool.entries()[0].use_count;
    assert_eq!(count, 1, "acquire_lease must bump use_count");
}

/// A credential removed by 401/403 must stay removed across restart even
/// when its env var is still set. Regression for the bug where
/// `reconcile_or_new` dropped `removed_at_ms` on re-seed.
#[test]
fn auth_removal_survives_reload_with_env_still_set() {
    let (_db, path, _g) = open_test_db();
    let var = "BORG_TEST_AUTH_STICKY";
    std::env::set_var(var, "stickyk");
    let cred_id = {
        let db = Database::open_at(&path).unwrap();
        let mut cfg = cfg_with(&[]);
        cfg.env_vars = vec![var.to_string()];
        let pool = CredentialPool::load(&db, &path, Provider::OpenRouter, cfg).unwrap();
        let live = pool.entries().into_iter().next().unwrap();
        pool.report_failure(
            &live.id,
            FailoverReason::Auth,
            Some(401),
            None,
            Some("revoked"),
        );
        assert!(pool.entries()[0].is_removed());
        live.id
    };
    // Re-load with the env var still present — removal must persist.
    {
        let db = Database::open_at(&path).unwrap();
        let mut cfg = cfg_with(&[]);
        cfg.env_vars = vec![var.to_string()];
        let pool = CredentialPool::load(&db, &path, Provider::OpenRouter, cfg).unwrap();
        let reloaded = pool
            .entries()
            .into_iter()
            .find(|c| c.id == cred_id)
            .expect("removed cred should still appear in entries");
        assert!(
            reloaded.is_removed(),
            "401-removed cred must not resurrect when env var still set"
        );
    }
    std::env::remove_var(var);
}

/// Plaintext `key` must never appear in `{:?}` output — accidental
/// tracing of a `PooledCredential` (or a parent `PoolInner`) cannot
/// leak the secret.
#[test]
fn debug_redacts_plaintext_key() {
    let (db, path, _g) = open_test_db();
    let pool = CredentialPool::load(
        &db,
        &path,
        Provider::OpenRouter,
        cfg_with(&[("a", "supersecret-key-do-not-log")]),
    )
    .unwrap();
    let cred = &pool.entries()[0];
    let dbg_str = format!("{cred:?}");
    assert!(
        !dbg_str.contains("supersecret-key-do-not-log"),
        "Debug must not include plaintext key: {dbg_str}"
    );
    assert!(dbg_str.contains("redacted"), "Debug must mark redaction");
    // Also exercise the parent CredentialPool Debug (transitive through inner).
    let parent_dbg = format!("{pool:?}");
    assert!(
        !parent_dbg.contains("supersecret-key-do-not-log"),
        "CredentialPool Debug must not leak key transitively"
    );
}

// ── OAuth source seeding ──

fn oauth_cfg(home: &std::path::Path) -> PoolLoadConfig {
    PoolLoadConfig {
        strategy: CredentialStrategy::FillFirst,
        cooldown_secs: 60,
        max_concurrent_per_credential: 1,
        proactive_rate_limit_threshold_pct: 0,
        proactive_rate_limit_min_window_secs: 0,
        config_entries: vec![],
        env_vars: vec![],
        oauth_home: Some(home.to_path_buf()),
    }
}

#[test]
fn load_seeds_claude_code_oauth_from_disk() {
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home.path().join(".claude")).unwrap();
    std::fs::write(
        home.path().join(".claude/.credentials.json"),
        r#"{"claudeAiOauth":{"accessToken":"sk-ant-oat-abc","refreshToken":"sk-ant-ort-xyz","expiresAt":9999999999000,"subscriptionType":"pro","emailAddress":"alice@example.com"}}"#,
    )
    .unwrap();

    let (db, path, _g) = open_test_db();
    let pool = CredentialPool::load(
        &db,
        &path,
        Provider::ClaudeCodeOAuth,
        oauth_cfg(home.path()),
    )
    .unwrap();
    let entries = pool.entries();
    assert_eq!(entries.len(), 1, "one OAuth credential seeded");
    let c = &entries[0];
    assert_eq!(c.key, "sk-ant-oat-abc");
    assert_eq!(c.refresh_token.as_deref(), Some("sk-ant-ort-xyz"));
    assert_eq!(c.expires_at_ms, Some(9999999999000));
    assert!(matches!(c.auth_type, AuthType::OAuth));
    assert!(matches!(
        c.source,
        CredentialSource::OAuth { ref name } if name == "claude_code"
    ));
}

#[test]
fn malformed_oauth_file_does_not_crash_load() {
    // Parser error must log + continue — never panic the daemon's startup
    // path because a single source file is corrupt.
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home.path().join(".claude")).unwrap();
    std::fs::write(
        home.path().join(".claude/.credentials.json"),
        "not json {{ at all",
    )
    .unwrap();

    let (db, path, _g) = open_test_db();
    let pool = CredentialPool::load(
        &db,
        &path,
        Provider::ClaudeCodeOAuth,
        oauth_cfg(home.path()),
    )
    .unwrap();
    assert!(pool.entries().is_empty());
}

#[tokio::test]
async fn acquire_lease_refreshing_renews_expired_oauth_token() {
    // Expired OAuth credential → refresh fires → access_token rotated,
    // refresh_token rotated, expires_at_ms moved forward.
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/oauth/token"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"{"access_token":"refreshed-access","refresh_token":"refreshed-r","expires_in":3600}"#,
        ))
        .expect(1)
        .mount(&server)
        .await;

    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home.path().join(".claude")).unwrap();
    // expiresAt in the past — forces refresh on next lease.
    std::fs::write(
        home.path().join(".claude/.credentials.json"),
        r#"{"claudeAiOauth":{"accessToken":"old-access","refreshToken":"old-refresh","expiresAt":1}}"#,
    )
    .unwrap();

    let (db, path, _g) = open_test_db();
    let pool = CredentialPool::load(
        &db,
        &path,
        Provider::ClaudeCodeOAuth,
        oauth_cfg(home.path()),
    )
    .unwrap();
    pool.set_refresh_endpoint_for_tests(Some(format!("{}/v1/oauth/token", server.uri())));

    let lease = pool.acquire_lease_refreshing(None).await.unwrap();
    let cred = lease.credential().unwrap();
    assert_eq!(cred.key, "refreshed-access", "access token rotated");
    assert_eq!(
        cred.refresh_token.as_deref(),
        Some("refreshed-r"),
        "refresh token rotated"
    );
    assert!(
        cred.expires_at_ms.unwrap() > pool.clock.now_ms(),
        "expiry pushed into the future"
    );
}

#[tokio::test]
async fn acquire_lease_refreshing_skips_when_token_still_valid() {
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0) // must NOT be called
        .mount(&server)
        .await;

    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home.path().join(".claude")).unwrap();
    std::fs::write(
        home.path().join(".claude/.credentials.json"),
        r#"{"claudeAiOauth":{"accessToken":"still-good","refreshToken":"r","expiresAt":9999999999000}}"#,
    )
    .unwrap();
    let (db, path, _g) = open_test_db();
    let pool = CredentialPool::load(
        &db,
        &path,
        Provider::ClaudeCodeOAuth,
        oauth_cfg(home.path()),
    )
    .unwrap();
    pool.set_refresh_endpoint_for_tests(Some(format!("{}/v1/oauth/token", server.uri())));

    let lease = pool.acquire_lease_refreshing(None).await.unwrap();
    assert_eq!(lease.credential().unwrap().key, "still-good");
    // wiremock's `expect(0)` validates on drop.
}

#[tokio::test]
async fn refresh_failure_cools_credential_without_removing_it() {
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500))
        // `.expect(1)` proves the cooldown was set BY the failed refresh,
        // not just by the credential happening to already be exhausted.
        // Without this, an inert fast-path (e.g. refresh skipped) would
        // silently pass — the "is not a tombstone" assertion alone is
        // also satisfied by a healthy credential.
        .expect(1)
        .mount(&server)
        .await;

    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home.path().join(".claude")).unwrap();
    std::fs::write(
        home.path().join(".claude/.credentials.json"),
        r#"{"claudeAiOauth":{"accessToken":"a","refreshToken":"r","expiresAt":1}}"#,
    )
    .unwrap();
    let (db, path, _g) = open_test_db();
    let pool = CredentialPool::load(
        &db,
        &path,
        Provider::ClaudeCodeOAuth,
        oauth_cfg(home.path()),
    )
    .unwrap();
    pool.set_refresh_endpoint_for_tests(Some(format!("{}/v1/oauth/token", server.uri())));

    let lease = pool.acquire_lease_refreshing(None).await;
    assert!(
        lease.is_none(),
        "single expired OAuth credential with failed refresh should fall through to fallback, not reuse stale token"
    );
    let entries = pool.entries();
    assert!(
        !entries[0].is_removed(),
        "transient refresh failure is not a tombstone"
    );
    assert_eq!(
        entries[0].last_status,
        CredentialStatus::Exhausted,
        "refresh failure cools the credential"
    );
    assert!(
        entries[0].last_error_reset_at_ms.is_some(),
        "cooldown set so retries are spaced"
    );
    assert_eq!(
        entries[0].last_error_message.as_deref(),
        Some("oauth_refresh_failed"),
        "error message tags the cooldown as a refresh failure, distinguishing it from a true 429"
    );
    // wiremock's `.expect(1)` is verified on server drop.
    drop(server);
}

#[tokio::test]
async fn refresh_failure_reacquires_a_healthy_sibling_credential() {
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500))
        .expect(1)
        .mount(&server)
        .await;

    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home.path().join(".claude")).unwrap();
    std::fs::write(
        home.path().join(".claude/.credentials.json"),
        r#"{"claudeAiOauth":{"accessToken":"expired-access","refreshToken":"refresh-r","expiresAt":1}}"#,
    )
    .unwrap();
    let (db, path, _g) = open_test_db();
    let pool = CredentialPool::load(
        &db,
        &path,
        Provider::ClaudeCodeOAuth,
        oauth_cfg(home.path()),
    )
    .unwrap();
    pool.set_refresh_endpoint_for_tests(Some(format!("{}/v1/oauth/token", server.uri())));
    let sibling = pool.add_entry(
        "healthy-api-key".into(),
        "manual-sibling".into(),
        CredentialSource::Manual,
    );

    let lease = pool
        .acquire_lease_refreshing(None)
        .await
        .expect("healthy sibling should be re-acquired");
    assert_eq!(
        lease.id(),
        sibling.id,
        "failed refresh must rotate to another available credential instead of keeping the stale one"
    );
}

#[test]
fn may_recover_within_false_for_single_key_pool() {
    // Single key, 60s cooldown: there is nothing to rotate to, so
    // fallback should fire instead of waiting on the same exhausted key.
    let (db, path, _g) = open_test_db();
    let pool =
        CredentialPool::load(&db, &path, Provider::OpenRouter, cfg_with(&[("a", "k1")])).unwrap();
    let cred = pool.pick().unwrap();
    pool.report_failure(
        &cred.id,
        FailoverReason::RateLimit,
        None,
        Some(Duration::from_secs(60)),
        None,
    );
    assert!(
        !pool.may_recover_within(Duration::from_secs(300)),
        "single-key pool should not defer provider fallback"
    );
    assert!(
        !pool.may_recover_within(Duration::from_secs(30)),
        "single-key pool should never report recoverable-by-rotation"
    );
}

#[test]
fn may_recover_within_true_for_multi_key_imminent_recovery() {
    let (db, path, _g) = open_test_db();
    let pool = CredentialPool::load(
        &db,
        &path,
        Provider::OpenRouter,
        cfg_with(&[("a", "k1"), ("b", "k2")]),
    )
    .unwrap();
    let entries = pool.entries();
    pool.report_failure(
        &entries[0].id,
        FailoverReason::RateLimit,
        None,
        Some(Duration::from_secs(60)),
        None,
    );
    assert!(
        pool.may_recover_within(Duration::from_secs(300)),
        "multi-key pool with an imminent recovery should defer fallback"
    );
    assert!(
        !pool.may_recover_within(Duration::from_secs(30)),
        "recovery outside the horizon should not defer fallback"
    );
}

#[test]
fn may_recover_within_false_for_tombstoned_pool() {
    // Auth failure permanently tombstones — no recovery possible, so
    // fallback must engage.
    let (db, path, _g) = open_test_db();
    let pool =
        CredentialPool::load(&db, &path, Provider::OpenRouter, cfg_with(&[("a", "k1")])).unwrap();
    let cred = pool.pick().unwrap();
    pool.report_failure(&cred.id, FailoverReason::Auth, Some(401), None, None);
    assert!(
        !pool.may_recover_within(Duration::from_secs(3600)),
        "tombstoned credential never recovers, no matter how long the window"
    );
}

#[test]
fn may_recover_within_false_for_long_cooldown_beyond_horizon() {
    // 2-hour Retry-After exceeds the 5-min recovery window → fallback fires.
    let (db, path, _g) = open_test_db();
    let pool =
        CredentialPool::load(&db, &path, Provider::OpenRouter, cfg_with(&[("a", "k1")])).unwrap();
    let cred = pool.pick().unwrap();
    pool.report_failure(
        &cred.id,
        FailoverReason::RateLimit,
        None,
        Some(Duration::from_secs(7200)),
        None,
    );
    assert!(
        !pool.may_recover_within(Duration::from_secs(300)),
        "long cooldown beyond horizon → fallback should fire"
    );
}

// `needs_refresh` invariants live alongside the helper in
// `credential_pool::helpers::tests::needs_refresh_*`.

#[test]
fn oauth_seeded_credential_round_trips_through_persistence() {
    // Use-count and cooldown state from a prior run must survive a reload
    // even though the refresh_token is re-supplied by the discoverer (not
    // persisted to DB).
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home.path().join(".claude")).unwrap();
    std::fs::write(
        home.path().join(".claude/.credentials.json"),
        r#"{"claudeAiOauth":{"accessToken":"tok-1","refreshToken":"ref-1","expiresAt":1234567890000}}"#,
    )
    .unwrap();
    let (db, path, _g) = open_test_db();
    {
        let pool = CredentialPool::load(
            &db,
            &path,
            Provider::ClaudeCodeOAuth,
            oauth_cfg(home.path()),
        )
        .unwrap();
        let cred = pool.pick().unwrap();
        pool.report_failure(
            &cred.id,
            FailoverReason::RateLimit,
            None,
            Some(Duration::from_secs(60)),
            None,
        );
    }
    let pool2 = CredentialPool::load(
        &db,
        &path,
        Provider::ClaudeCodeOAuth,
        oauth_cfg(home.path()),
    )
    .unwrap();
    let entries = pool2.entries();
    assert_eq!(entries.len(), 1);
    let c = &entries[0];
    assert_eq!(
        c.refresh_token.as_deref(),
        Some("ref-1"),
        "refresh re-seeded from disk"
    );
    assert!(
        c.last_error_reset_at_ms.is_some(),
        "cooldown state survived reload"
    );
}

// ── Proactive rate-limit avoidance ──

use crate::llm::rate_limit_headers::{Bucket, RateLimitSnapshot};

fn snap_with_pct(now_ms: i64, remaining_pct: u8, reset_offset_ms: i64) -> RateLimitSnapshot {
    let limit = 1000u64;
    let remaining = (limit * remaining_pct as u64) / 100;
    RateLimitSnapshot {
        captured_at_ms: now_ms,
        requests: Some(Bucket {
            limit,
            remaining,
            reset_at_ms: now_ms + reset_offset_ms,
        }),
        tokens: None,
    }
}

fn proactive_cfg(threshold_pct: u8, min_window_secs: u64) -> PoolLoadConfig {
    let entries = vec![
        ConfigEntry {
            key: "key-a".into(),
            label: Some("A".into()),
            config_name: "llm.api_keys[0]".into(),
        },
        ConfigEntry {
            key: "key-b".into(),
            label: Some("B".into()),
            config_name: "llm.api_keys[1]".into(),
        },
    ];
    PoolLoadConfig {
        strategy: CredentialStrategy::FillFirst,
        cooldown_secs: 60,
        max_concurrent_per_credential: 1,
        proactive_rate_limit_threshold_pct: threshold_pct,
        proactive_rate_limit_min_window_secs: min_window_secs,
        config_entries: entries,
        env_vars: vec![],
        oauth_home: None,
    }
}

#[test]
fn proactive_throttle_triggers_below_threshold() {
    let now = 1_000_000;
    let mut c = PooledCredential {
        id: "x".into(),
        label: "x".into(),
        priority: 0,
        source: CredentialSource::Manual,
        key: "k".into(),
        fingerprint: "fp".into(),
        use_count: 0,
        last_status: CredentialStatus::Ok,
        last_status_at_ms: Some(now),
        last_error_code: None,
        last_error_reason: None,
        last_error_message: None,
        last_error_reset_at_ms: None,
        removed_at_ms: None,
        auth_type: AuthType::ApiKey,
        refresh_token: None,
        expires_at_ms: None,
        base_url: None,
        extra_json: None,
        rate_limit: Some(snap_with_pct(now, 2, 60_000)),
    };
    // 2% remaining < 5% threshold, reset 60s out > 5s min window → throttled
    assert!(c.is_proactively_throttled(now, 5, 5));
    // Reset already passed → not throttled (snapshot is stale).
    assert!(!c.is_proactively_throttled(now + 70_000, 5, 5));
    // Within the min-window → waiting is cheaper, don't rotate.
    c.rate_limit = Some(snap_with_pct(now, 2, 3_000));
    assert!(!c.is_proactively_throttled(now, 5, 5));
    // Threshold disabled (0) → never throttle, even at 0% remaining.
    c.rate_limit = Some(snap_with_pct(now, 0, 60_000));
    assert!(!c.is_proactively_throttled(now, 0, 5));
    // No snapshot → not throttled.
    c.rate_limit = None;
    assert!(!c.is_proactively_throttled(now, 5, 5));
}

#[test]
fn pick_skips_proactively_throttled_credential() {
    let (db, path, _g) = open_test_db();
    let pool =
        CredentialPool::load(&db, &path, Provider::OpenRouter, proactive_cfg(5, 5)).expect("pool");

    let entries = pool.entries();
    let id_a = entries[0].id.clone();
    let id_b = entries[1].id.clone();

    // Mark A near-empty with a far-out reset; B stays healthy.
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    pool.report_rate_limit_headers(&id_a, snap_with_pct(now, 1, 120_000));

    // FillFirst would normally pick A (lower priority). With the proactive
    // filter it should skip to B.
    let picked = pool.pick().expect("pick");
    assert_eq!(
        picked.id, id_b,
        "pool should skip near-empty credential A in favor of healthy B"
    );

    // Now mark B near-empty too. Every credential is throttled, but the
    // fallback pass must still pick *something* — otherwise the caller
    // would synthesize an exhaustion error for a key that's actually live.
    pool.report_rate_limit_headers(&id_b, snap_with_pct(now, 1, 120_000));
    let picked = pool.pick().expect("pick under all-throttled");
    assert!(
        picked.id == id_a || picked.id == id_b,
        "fallback pick must still return a live credential"
    );
}

#[test]
fn report_rate_limit_headers_overwrites_prior_snapshot() {
    // Regression guard: the latest provider observation always wins. If we
    // ever changed `report_rate_limit_headers` to merge / max / preserve
    // an older snapshot, a recovered bucket would never re-enable a
    // credential until something else cleared the field.
    let (db, path, _g) = open_test_db();
    let pool = CredentialPool::load(&db, &path, Provider::OpenRouter, proactive_cfg(5, 5)).unwrap();
    let id = pool.entries()[0].id.clone();
    let now = 2_000_000;
    pool.report_rate_limit_headers(&id, snap_with_pct(now, 1, 60_000));
    pool.report_rate_limit_headers(&id, snap_with_pct(now, 90, 60_000));
    let snap = pool.entries()[0].rate_limit.clone().expect("snapshot");
    assert_eq!(
        snap.requests.unwrap().remaining,
        900,
        "latest snapshot must replace the prior one, not merge"
    );
}

#[test]
fn report_rate_limit_headers_on_removed_is_noop() {
    let (db, path, _g) = open_test_db();
    let pool = CredentialPool::load(&db, &path, Provider::OpenRouter, proactive_cfg(5, 5)).unwrap();
    let id = pool.entries()[0].id.clone();
    // Tombstone it.
    pool.report_failure(&id, FailoverReason::Auth, Some(401), None, None);
    let now = 3_000_000;
    pool.report_rate_limit_headers(&id, snap_with_pct(now, 1, 60_000));
    // Snapshot must not have been attached to a tombstoned credential —
    // otherwise `borg auth list` shows phantom bucket state on a key that
    // can never be used again.
    assert!(pool.entries()[0].rate_limit.is_none());
}
