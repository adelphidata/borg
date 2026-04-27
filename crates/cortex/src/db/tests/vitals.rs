use super::*;

// ── Vitals DB tests (event-sourced) ──

#[test]
fn vitals_state_baseline_no_events() {
    let db = test_db();
    let state = db.get_vitals_state().unwrap();
    assert_eq!(state.stability, 40);
    assert_eq!(state.focus, 40);
    assert_eq!(state.sync, 40);
    assert_eq!(state.growth, 40);
    assert_eq!(state.happiness, 40);
}

#[test]
fn record_and_replay_vitals_event() {
    let db = test_db();
    let deltas = crate::vitals::deltas_for(crate::vitals::EventCategory::Creation);
    db.record_vitals_event("creation", "create_tool", &deltas, None)
        .unwrap();
    let state = db.get_vitals_state().unwrap();
    assert_eq!(state.stability, 41); // 40 + 1
    assert_eq!(state.focus, 40); // 40 + 0
    assert_eq!(state.sync, 40); // 40 + 0
    assert_eq!(state.growth, 41); // 40 + 1
    assert_eq!(state.happiness, 41); // 40 + 1
}

#[test]
fn vitals_events_since_returns_events() {
    let db = test_db();
    let deltas = crate::vitals::deltas_for(crate::vitals::EventCategory::Interaction);
    db.record_vitals_event("interaction", "session_start", &deltas, None)
        .unwrap();
    db.record_vitals_event("interaction", "user_message", &deltas, None)
        .unwrap();
    let events = db.vitals_events_since(0).unwrap();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].category, "interaction");
    assert_eq!(events[0].source, "user_message"); // DESC order
    assert_eq!(events[1].source, "session_start");
}

#[test]
fn vitals_event_ledger_appends() {
    let db = test_db();
    let deltas = crate::vitals::deltas_for(crate::vitals::EventCategory::Success);
    for _ in 0..5 {
        db.record_vitals_event("success", "run_shell", &deltas, None)
            .unwrap();
    }
    let events = db.vitals_events_since(0).unwrap();
    assert_eq!(events.len(), 5);
    // State replayed from events with source decay (all same source "run_shell"):
    // counts 1-2: full (+1 each), count 3+: floor(1*0.5)=0, so only 2 count
    let state = db.get_vitals_state().unwrap();
    assert_eq!(state.stability, 42);
}

#[test]
fn vitals_hmac_chain_integrity() {
    let db = test_db();
    let deltas = crate::vitals::deltas_for(crate::vitals::EventCategory::Interaction);
    db.record_vitals_event("interaction", "a", &deltas, None)
        .unwrap();
    db.record_vitals_event("interaction", "b", &deltas, None)
        .unwrap();
    // Events should have valid HMAC chain
    let events = db.vitals_events_since(0).unwrap();
    assert!(!events[0].hmac.is_empty());
    assert!(!events[1].hmac.is_empty());
    // State should be valid (both events applied)
    let state = db.get_vitals_state().unwrap();
    assert_eq!(state.sync, 42); // 40 + 1 + 1
}

// ── Bond DB Tests ──

#[test]
fn bond_migration_creates_table() {
    let db = test_db();
    // Table should exist after migration
    let count: i64 = db
        .conn
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='bond_events'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 1);
}

#[test]
fn bond_no_events_returns_empty() {
    let db = test_db();
    let events = db.get_all_bond_events().unwrap();
    assert!(events.is_empty());
}

#[test]
fn bond_record_and_read_event() {
    let db = test_db();
    let now = chrono::Utc::now().timestamp();
    let hmac = crate::bond::compute_event_hmac(
        b"borg-bond-chain-v1",
        "0",
        "tool_success",
        1,
        "run_shell",
        now,
    );
    db.record_bond_event("tool_success", 1, "run_shell", &hmac, "0", now)
        .unwrap();

    let events = db.get_all_bond_events().unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].event_type, "tool_success");
    assert_eq!(events[0].score_delta, 1);
    assert_eq!(events[0].reason, "run_shell");
    assert_eq!(events[0].hmac, hmac);
    assert_eq!(events[0].prev_hmac, "0");
}

#[test]
fn bond_get_last_hmac() {
    let db = test_db();
    // No events — should return "0"
    let hmac = db.get_last_bond_event_hmac().unwrap();
    assert_eq!(hmac, "0");

    // Add an event
    let now = chrono::Utc::now().timestamp();
    let h1 =
        crate::bond::compute_event_hmac(b"borg-bond-chain-v1", "0", "tool_success", 1, "test", now);
    db.record_bond_event("tool_success", 1, "test", &h1, "0", now)
        .unwrap();
    assert_eq!(db.get_last_bond_event_hmac().unwrap(), h1);

    // Add another
    let h2 = crate::bond::compute_event_hmac(
        b"borg-bond-chain-v1",
        &h1,
        "creation",
        2,
        "write_memory",
        now + 1,
    );
    db.record_bond_event("creation", 2, "write_memory", &h2, &h1, now + 1)
        .unwrap();
    assert_eq!(db.get_last_bond_event_hmac().unwrap(), h2);
}

#[test]
fn bond_events_since_filters() {
    let db = test_db();
    let now = chrono::Utc::now().timestamp();
    let h1 =
        crate::bond::compute_event_hmac(b"borg-bond-chain-v1", "0", "tool_success", 1, "a", now);
    db.record_bond_event("tool_success", 1, "a", &h1, "0", now)
        .unwrap();

    // Events are timestamped at now(), so "since 0" should include everything
    let events = db.bond_events_since(0).unwrap();
    assert_eq!(events.len(), 1);

    // Far future should return nothing
    let events = db
        .bond_events_since(chrono::Utc::now().timestamp() + 9999)
        .unwrap();
    assert!(events.is_empty());
}

#[test]
fn bond_events_recent_limits() {
    let db = test_db();
    let base = chrono::Utc::now().timestamp();
    for i in 0..5 {
        let prev = if i == 0 {
            "0".to_string()
        } else {
            db.get_last_bond_event_hmac().unwrap()
        };
        let ts = base + i;
        let h = crate::bond::compute_event_hmac(
            b"borg-bond-chain-v1",
            &prev,
            "tool_success",
            1,
            "t",
            ts,
        );
        db.record_bond_event("tool_success", 1, "t", &h, &prev, ts)
            .unwrap();
    }

    let events = db.bond_events_recent(3).unwrap();
    assert_eq!(events.len(), 3);
}

#[test]
fn bond_count_events_since() {
    let db = test_db();
    let base = chrono::Utc::now().timestamp();
    let h1 =
        crate::bond::compute_event_hmac(b"borg-bond-chain-v1", "0", "tool_success", 1, "a", base);
    db.record_bond_event("tool_success", 1, "a", &h1, "0", base)
        .unwrap();
    let h2 =
        crate::bond::compute_event_hmac(b"borg-bond-chain-v1", &h1, "creation", 2, "b", base + 1);
    db.record_bond_event("creation", 2, "b", &h2, &h1, base + 1)
        .unwrap();
    let h3 = crate::bond::compute_event_hmac(
        b"borg-bond-chain-v1",
        &h2,
        "tool_success",
        1,
        "c",
        base + 2,
    );
    db.record_bond_event("tool_success", 1, "c", &h3, &h2, base + 2)
        .unwrap();

    // All events (empty type = all)
    let total = db.count_bond_events_since(0, "").unwrap();
    assert_eq!(total, 3);

    // Filter by type
    let ts = db.count_bond_events_since(0, "tool_success").unwrap();
    assert_eq!(ts, 2);

    let cr = db.count_bond_events_since(0, "creation").unwrap();
    assert_eq!(cr, 1);
}

#[test]
fn bond_replay_with_db() {
    let db = test_db();
    let base = chrono::Utc::now().timestamp();
    // Record a chain of events
    let h1 = crate::bond::compute_event_hmac(
        b"borg-bond-chain-v1",
        "0",
        "tool_success",
        1,
        "read_file",
        base,
    );
    db.record_bond_event("tool_success", 1, "read_file", &h1, "0", base)
        .unwrap();
    let h2 = crate::bond::compute_event_hmac(
        b"borg-bond-chain-v1",
        &h1,
        "creation",
        1,
        "write_memory",
        base + 1,
    );
    db.record_bond_event("creation", 1, "write_memory", &h2, &h1, base + 1)
        .unwrap();

    let events = db.get_all_bond_events().unwrap();
    let state = crate::bond::replay_events(&events);
    assert!(state.chain_valid);
    // 25 + 1 + 1 = 27
    assert_eq!(state.score, 27);
    assert_eq!(state.level, crate::bond::BondLevel::Emerging);
}

#[test]
fn bond_record_chained_produces_valid_chain() {
    let db = test_db();
    db.record_bond_event_chained("tool_success", 1, "read_file")
        .unwrap();
    db.record_bond_event_chained("creation", 1, "write_memory")
        .unwrap();
    db.record_bond_event_chained("tool_failure", -1, "run_shell")
        .unwrap();

    let events = db.get_all_bond_events().unwrap();
    assert_eq!(events.len(), 3);

    // Replay should verify the chain is valid (use derived key matching record_bond_event_chained)
    let key = db.derive_hmac_key(crate::bond::BOND_HMAC_DOMAIN);
    let state = crate::bond::replay_events_with_key(&key, &events);
    assert!(state.chain_valid);
    // 25 + 1 + 1 - 1 = 26
    assert_eq!(state.score, 26);

    // Verify chain linking
    assert_eq!(events[0].prev_hmac, "0");
    assert_eq!(events[1].prev_hmac, events[0].hmac);
    assert_eq!(events[2].prev_hmac, events[1].hmac);
}

#[test]
fn bond_record_rejects_invalid_event_type() {
    let db = test_db();
    let result = db.record_bond_event_chained("custom_exploit", 1, "test");
    assert!(result.is_err());
}

#[test]
fn bond_record_rejects_wrong_delta() {
    let db = test_db();
    let result = db.record_bond_event_chained("tool_success", 99, "test");
    assert!(result.is_err());
    let result = db.record_bond_event_chained("tool_success", 1, "test");
    assert!(result.is_ok());
}

#[test]
fn bond_record_total_hourly_cap() {
    let db = test_db();
    for i in 0..15 {
        let event_type = match i % 6 {
            0 => "tool_success",
            1 => "tool_failure",
            2 => "creation",
            3 => "correction",
            4 => "suggestion_accepted",
            _ => "suggestion_rejected",
        };
        let delta = match event_type {
            "tool_success" | "suggestion_accepted" => 1,
            "tool_failure" | "suggestion_rejected" => -1,
            "creation" => 1,
            "correction" => -2,
            _ => unreachable!(),
        };
        db.record_bond_event_chained(event_type, delta, "test")
            .unwrap();
    }
    // 16th event should be silently dropped (total cap = 15)
    db.record_bond_event_chained("tool_failure", -1, "test")
        .unwrap();
    let events = db.get_all_bond_events().unwrap();
    assert_eq!(events.len(), 15);
}

#[test]
fn bond_record_positive_delta_hourly_cap() {
    let db = test_db();
    for _ in 0..8 {
        db.record_bond_event_chained("tool_success", 1, "test")
            .unwrap();
    }
    // 9th positive event should be dropped
    db.record_bond_event_chained("suggestion_accepted", 1, "test")
        .unwrap();
    let events = db.get_all_bond_events().unwrap();
    assert_eq!(events.len(), 8);
    // Negative event should still work
    db.record_bond_event_chained("tool_failure", -1, "test")
        .unwrap();
    let events = db.get_all_bond_events().unwrap();
    assert_eq!(events.len(), 9);
}

#[test]
fn bond_count_vitals_events_by_category() {
    let db = test_db();
    let deltas = crate::vitals::deltas_for(crate::vitals::EventCategory::Interaction);
    db.record_vitals_event("interaction", "session_start", &deltas, None)
        .unwrap();
    db.record_vitals_event("interaction", "user_message", &deltas, None)
        .unwrap();
    let corr_deltas = crate::vitals::deltas_for(crate::vitals::EventCategory::Correction);
    db.record_vitals_event("correction", "user_message", &corr_deltas, None)
        .unwrap();

    let (corrections, total) = db
        .count_vitals_events_by_category_since(0, "correction")
        .unwrap();
    assert_eq!(corrections, 1);
    assert_eq!(total, 3);

    let (interactions, _) = db
        .count_vitals_events_by_category_since(0, "interaction")
        .unwrap();
    assert_eq!(interactions, 2);
}

// ── Tamper-Proof Hardening Tests ──

#[test]
fn vitals_record_time_rate_limiting() {
    let db = test_db();
    let deltas = crate::vitals::deltas_for(crate::vitals::EventCategory::Correction);
    // Correction cap is 3/hour
    for _ in 0..10 {
        db.record_vitals_event("correction", "test", &deltas, None)
            .unwrap();
    }
    // Only 3 should actually be recorded
    let events = db.vitals_events_since(0).unwrap();
    assert_eq!(
        events.len(),
        3,
        "record-time rate limiting should cap at 3 correction events/hour"
    );
}

#[test]
fn bond_record_time_rate_limiting() {
    let db = test_db();
    // creation cap is 3/hour
    for _ in 0..10 {
        db.record_bond_event_chained("creation", 1, "test").unwrap();
    }
    let events = db.get_all_bond_events().unwrap();
    assert_eq!(
        events.len(),
        3,
        "record-time rate limiting should cap at 3 creation events/hour"
    );
}

#[test]
fn evolution_record_time_rate_limiting() {
    let db = test_db();
    // Per-source cap is 5/hour, per-type cap is 15/hour, total cap is 20/hour.
    // With the same source, per-source (5) kicks in first.
    for _ in 0..35 {
        db.record_evolution_event("xp_gain", 3, Some("builder"), "test", None)
            .unwrap();
    }
    let events = db.load_all_evolution_events().unwrap();
    assert_eq!(
        events.len(),
        5,
        "record-time per-source rate limiting should cap at 5 events/hour from same source"
    );
}

#[test]
fn append_only_triggers_block_update() {
    let db = test_db();
    let deltas = crate::vitals::deltas_for(crate::vitals::EventCategory::Interaction);
    db.record_vitals_event("interaction", "test", &deltas, None)
        .unwrap();

    // UPDATE should be blocked by trigger
    let result = db.conn.execute(
        "UPDATE vitals_events SET category = 'hacked' WHERE id = 1",
        [],
    );
    assert!(result.is_err(), "append-only trigger should prevent UPDATE");
    assert!(
        result.unwrap_err().to_string().contains("append-only"),
        "error message should mention append-only"
    );
}

#[test]
fn append_only_triggers_block_delete() {
    let db = test_db();
    let deltas = crate::vitals::deltas_for(crate::vitals::EventCategory::Interaction);
    db.record_vitals_event("interaction", "test", &deltas, None)
        .unwrap();

    // DELETE should be blocked by trigger
    let result = db
        .conn
        .execute("DELETE FROM vitals_events WHERE id = 1", []);
    assert!(result.is_err(), "append-only trigger should prevent DELETE");
}

#[test]
fn bond_append_only_triggers() {
    let db = test_db();
    db.record_bond_event_chained("tool_success", 1, "test")
        .unwrap();

    let update = db
        .conn
        .execute("UPDATE bond_events SET score_delta = 100 WHERE id = 1", []);
    assert!(update.is_err(), "bond trigger should prevent UPDATE");

    let delete = db.conn.execute("DELETE FROM bond_events WHERE id = 1", []);
    assert!(delete.is_err(), "bond trigger should prevent DELETE");
}

#[test]
fn evolution_append_only_triggers() {
    let db = test_db();
    db.record_evolution_event("xp_gain", 3, Some("builder"), "test", None)
        .unwrap();

    let update = db.conn.execute(
        "UPDATE evolution_events SET xp_delta = 99999 WHERE id = 1",
        [],
    );
    assert!(update.is_err(), "evolution trigger should prevent UPDATE");

    let delete = db
        .conn
        .execute("DELETE FROM evolution_events WHERE id = 1", []);
    assert!(delete.is_err(), "evolution trigger should prevent DELETE");
}

#[test]
fn chain_integrity_verification_healthy() {
    let db = test_db();
    let deltas = crate::vitals::deltas_for(crate::vitals::EventCategory::Success);
    db.record_vitals_event("success", "test", &deltas, None)
        .unwrap();
    db.record_bond_event_chained("tool_success", 1, "test")
        .unwrap();
    db.record_evolution_event("xp_gain", 3, Some("ops"), "test", None)
        .unwrap();

    let health = db.verify_event_chains();
    assert!(health.vitals_valid, "vitals chain should be valid");
    assert!(health.bond_valid, "bond chain should be valid");
    assert!(health.evolution_valid, "evolution chain should be valid");
    assert_eq!(health.vitals_count, 1);
    assert_eq!(health.bond_count, 1);
    assert_eq!(health.evolution_count, 1);
}

#[test]
fn chain_integrity_empty_db() {
    let db = test_db();
    let health = db.verify_event_chains();
    assert!(health.vitals_valid, "empty chain should be valid");
    assert!(health.bond_valid, "empty chain should be valid");
    assert!(health.evolution_valid, "empty chain should be valid");
    assert_eq!(health.vitals_count, 0);
}

#[test]
fn per_install_hmac_salt_persists() {
    let db = test_db();
    let salt1 = db.get_meta("hmac_salt").unwrap().unwrap();
    // Create another DB from same connection — salt should be the same
    let salt2 = db.get_meta("hmac_salt").unwrap().unwrap();
    assert_eq!(salt1, salt2, "HMAC salt should persist across reads");
    assert_eq!(salt1.len(), 64, "salt should be 64 hex chars (32 bytes)");
}

#[test]
fn derived_keys_differ_by_domain() {
    let db = test_db();
    let vitals_key = db.derive_hmac_key(crate::vitals::VITALS_HMAC_DOMAIN);
    let bond_key = db.derive_hmac_key(crate::bond::BOND_HMAC_DOMAIN);
    let evo_key = db.derive_hmac_key(crate::evolution::EVOLUTION_HMAC_DOMAIN);
    assert_ne!(vitals_key, bond_key, "vitals and bond keys should differ");
    assert_ne!(bond_key, evo_key, "bond and evolution keys should differ");
    assert_ne!(
        vitals_key, evo_key,
        "vitals and evolution keys should differ"
    );
}

#[test]
fn vitals_transaction_atomicity() {
    let db = test_db();
    let deltas = crate::vitals::deltas_for(crate::vitals::EventCategory::Success);
    // Record two events and verify HMAC chain is valid (proves atomic transactions)
    db.record_vitals_event("success", "a", &deltas, None)
        .unwrap();
    db.record_vitals_event("success", "b", &deltas, None)
        .unwrap();

    let state = db.get_vitals_state().unwrap();
    assert!(
        state.chain_valid,
        "HMAC chain should be valid with transactional writes"
    );
}

#[test]
fn hmac_checkpoint_write_and_read() {
    let db = test_db();
    // No checkpoint initially
    let cp = db.load_latest_hmac_checkpoint("vitals").unwrap();
    assert!(cp.is_none());

    // Save a checkpoint
    db.save_hmac_checkpoint("vitals", 42, "prev_abc", "state_hash_123")
        .unwrap();
    let cp = db.load_latest_hmac_checkpoint("vitals").unwrap().unwrap();
    assert_eq!(cp.domain, "vitals");
    assert_eq!(cp.event_id, 42);
    assert_eq!(cp.prev_hmac, "prev_abc");
    assert_eq!(cp.state_hash, "state_hash_123");

    // Save another checkpoint — latest should win
    db.save_hmac_checkpoint("vitals", 100, "prev_xyz", "state_hash_456")
        .unwrap();
    let cp = db.load_latest_hmac_checkpoint("vitals").unwrap().unwrap();
    assert_eq!(cp.event_id, 100);

    // Different domain should have its own checkpoints
    let cp_bond = db.load_latest_hmac_checkpoint("bond").unwrap();
    assert!(cp_bond.is_none());
}

#[test]
fn bond_to_evolution_gate_integration() {
    let db = test_db();
    // Record bond events to build score
    for _ in 0..20 {
        db.record_bond_event_chained("tool_success", 1, "test")
            .unwrap();
    }
    // Verify bond state replays correctly
    let bond_events = db.get_all_bond_events().unwrap();
    let bond_key = db.derive_hmac_key(crate::bond::BOND_HMAC_DOMAIN);
    let bond_state = crate::bond::replay_events_with_key(&bond_key, &bond_events);
    assert!(bond_state.chain_valid);
    // Baseline 40 + 15 (capped per hour) = 55
    assert!(
        bond_state.score >= 30,
        "bond score {} should be >= 30 for stage1 gate",
        bond_state.score
    );

    // Verify evolution state can replay correctly after bond + evolution events
    let evo_state = db.get_evolution_state().unwrap();
    assert!(evo_state.chain_valid);
    assert_eq!(evo_state.stage, crate::evolution::Stage::Base);
}

#[test]
fn evolution_rejects_inflated_xp_delta() {
    let db = test_db();
    let result = db.record_evolution_event("xp_gain", 1000, Some("ops"), "test", None);
    assert!(result.is_err(), "should reject xp_delta > MAX_XP_DELTA");
    assert!(
        result.unwrap_err().to_string().contains("invalid xp_delta"),
        "error should mention invalid xp_delta"
    );
}

#[test]
fn evolution_rejects_negative_xp_delta() {
    let db = test_db();
    let result = db.record_evolution_event("xp_gain", -1, Some("ops"), "test", None);
    assert!(result.is_err(), "should reject negative xp_delta");
}

#[test]
fn evolution_rejects_unknown_event_type() {
    let db = test_db();
    let result = db.record_evolution_event("fake_type", 0, None, "test", None);
    assert!(result.is_err(), "should reject unknown event_type");
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("invalid evolution event_type"),
        "error should mention invalid event_type"
    );
}

#[test]
fn evolution_rejects_nonzero_delta_for_non_xp_types() {
    let db = test_db();
    // evolution events must have xp_delta = 0
    let result = db.record_evolution_event("evolution", 5, None, "test", None);
    assert!(
        result.is_err(),
        "evolution event should reject nonzero xp_delta"
    );
    // classification events must have xp_delta = 0
    let result = db.record_evolution_event("classification", 1, None, "test", None);
    assert!(
        result.is_err(),
        "classification event should reject nonzero xp_delta"
    );
}

#[test]
fn evolution_accepts_valid_xp_deltas() {
    let db = test_db();
    // xp_gain with 1, 2, 3 should all succeed (up to source rate limit)
    for delta in 1..=3 {
        db.record_evolution_event("xp_gain", delta, Some("ops"), &format!("src{delta}"), None)
            .unwrap();
    }
    // evolution with 0 should succeed (gates_verified metadata is required)
    let meta = serde_json::json!({ "gates_verified": true }).to_string();
    db.record_evolution_event("evolution", 0, None, "gate_check", Some(&meta))
        .unwrap();
    let events = db.load_all_evolution_events().unwrap();
    assert_eq!(events.len(), 4);
}

#[test]
fn evolution_source_rate_limiting_at_write_time() {
    let db = test_db();
    // Per-source cap is 5/hour
    for _ in 0..10 {
        db.record_evolution_event("xp_gain", 1, Some("ops"), "same_source", None)
            .unwrap();
    }
    let events = db.load_all_evolution_events().unwrap();
    assert_eq!(
        events.len(),
        5,
        "per-source write-time cap should limit to 5 events"
    );
}

#[test]
fn evolution_event_requires_gates_verified_metadata() {
    let db = test_db();
    // None metadata → reject
    let r = db.record_evolution_event("evolution", 0, None, "gate_check", None);
    assert!(r.is_err(), "None metadata should fail");
    // Invalid JSON → reject
    let r = db.record_evolution_event("evolution", 0, None, "gate_check", Some("not json"));
    assert!(r.is_err(), "invalid JSON metadata should fail");
    // gates_verified=false → reject
    let meta_false = serde_json::json!({ "gates_verified": false }).to_string();
    let r = db.record_evolution_event("evolution", 0, None, "gate_check", Some(&meta_false));
    assert!(r.is_err(), "gates_verified=false should fail");
    // Missing gates_verified key → reject
    let meta_missing = serde_json::json!({ "other": "data" }).to_string();
    let r = db.record_evolution_event("evolution", 0, None, "gate_check", Some(&meta_missing));
    assert!(r.is_err(), "missing gates_verified should fail");
    // gates_verified=true → accept
    let meta_ok = serde_json::json!({ "gates_verified": true }).to_string();
    db.record_evolution_event("evolution", 0, None, "gate_check", Some(&meta_ok))
        .unwrap();
    // Non-evolution types are not subject to this rule
    db.record_evolution_event("xp_gain", 1, Some("ops"), "run_shell", None)
        .unwrap();
    let class_meta = serde_json::json!({ "name": "Test", "description": "d" }).to_string();
    db.record_evolution_event(
        "classification",
        0,
        Some("ops"),
        "llm_naming",
        Some(&class_meta),
    )
    .unwrap();
}

#[test]
fn evolution_persistence_round_trip() {
    // Insert a mix of events via record_evolution_event, then reload and
    // confirm EvolutionState matches what replay_events_with_key produces
    // directly on the loaded events. This catches serialization / schema
    // drift between write and replay paths.
    let db = test_db();
    db.record_evolution_event("xp_gain", 3, Some("ops"), "run_shell", None)
        .unwrap();
    db.record_evolution_event("xp_gain", 3, Some("builder"), "apply_patch", None)
        .unwrap();
    let evo_meta = serde_json::json!({ "gates_verified": true }).to_string();
    db.record_evolution_event("evolution", 0, None, "gate_check", Some(&evo_meta))
        .unwrap();
    let class_meta =
        serde_json::json!({ "name": "Tool Forgemaster", "description": "d" }).to_string();
    db.record_evolution_event(
        "classification",
        0,
        Some("builder"),
        "llm_naming",
        Some(&class_meta),
    )
    .unwrap();

    let state_via_get = db.get_evolution_state().unwrap();
    // State should reflect that Stage transitioned and naming landed
    assert_eq!(state_via_get.stage, crate::evolution::Stage::Evolved);
    assert_eq!(
        state_via_get.evolution_name.as_deref(),
        Some("Tool Forgemaster")
    );
    assert!(state_via_get.chain_valid);
}

#[test]
fn evolution_total_rate_limiting_at_write_time() {
    let db = test_db();
    // Total cap is 20/hour. Use unique sources to avoid per-source cap (5).
    for i in 0..30 {
        db.record_evolution_event("xp_gain", 1, Some("ops"), &format!("src{i}"), None)
            .unwrap();
    }
    let events = db.load_all_evolution_events().unwrap();
    // Per-type cap is 15 for xp_gain, and total cap is 20. Per-type (15) kicks in first.
    assert_eq!(
        events.len(),
        15,
        "per-type cap (15) should kick in before total cap (20)"
    );
}

#[test]
fn vitals_rejects_inflated_deltas() {
    let db = test_db();
    let bad_deltas = crate::vitals::StatDeltas {
        stability: 100,
        focus: 0,
        sync: 0,
        growth: 0,
        happiness: 0,
    };
    let result = db.record_vitals_event("interaction", "test", &bad_deltas, None);
    assert!(result.is_err(), "should reject inflated deltas");
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("delta validation failed"),
        "error should mention delta validation"
    );
}

#[test]
fn vitals_rejects_unknown_category() {
    let db = test_db();
    let deltas = crate::vitals::StatDeltas::default();
    let result = db.record_vitals_event("hacked", "test", &deltas, None);
    assert!(result.is_err(), "should reject unknown category");
}

#[test]
fn vitals_accepts_correct_deltas_for_each_category() {
    let db = test_db();
    let categories = [
        "interaction",
        "success",
        "failure",
        "correction",
        "creation",
    ];
    for cat in &categories {
        let deltas = crate::vitals::deltas_for(match *cat {
            "interaction" => crate::vitals::EventCategory::Interaction,
            "success" => crate::vitals::EventCategory::Success,
            "failure" => crate::vitals::EventCategory::Failure,
            "correction" => crate::vitals::EventCategory::Correction,
            "creation" => crate::vitals::EventCategory::Creation,
            _ => unreachable!(),
        });
        db.record_vitals_event(cat, "test", &deltas, None).unwrap();
    }
    let events = db.vitals_events_since(0).unwrap();
    assert_eq!(events.len(), 5, "all 5 valid categories should persist");
}

// ── Pending Celebrations ──

#[test]
fn pending_celebrations_round_trip() {
    let db = test_db();
    let payload = r#"{"from_stage":"base","to_stage":"evolved","evolution_name":"Test Borg"}"#;
    db.insert_pending_celebration("evolution", payload).unwrap();

    let pending = db.get_pending_celebrations().unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].celebration_type, "evolution");
    assert_eq!(pending[0].payload_json, payload);

    db.mark_celebration_delivered(pending[0].id).unwrap();

    let after = db.get_pending_celebrations().unwrap();
    assert!(after.is_empty(), "should be empty after marking delivered");
}

#[test]
fn no_pending_celebrations_initially() {
    let db = test_db();
    let pending = db.get_pending_celebrations().unwrap();
    assert!(pending.is_empty());
}

#[test]
fn multiple_pending_celebrations_ordered() {
    let db = test_db();
    db.insert_pending_celebration("evolution", r#"{"id":1}"#)
        .unwrap();
    db.insert_pending_celebration("evolution", r#"{"id":2}"#)
        .unwrap();

    let pending = db.get_pending_celebrations().unwrap();
    assert_eq!(pending.len(), 2);
    // Should be ordered by created_at ASC
    assert!(pending[0].created_at <= pending[1].created_at);

    // Mark first delivered, second should remain
    db.mark_celebration_delivered(pending[0].id).unwrap();
    let remaining = db.get_pending_celebrations().unwrap();
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].payload_json, r#"{"id":2}"#);
}
