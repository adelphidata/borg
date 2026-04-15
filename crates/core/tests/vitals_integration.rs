//! Vitals event-sourcing integration tests.
//!
//! Tests the full vitals pipeline: recording events via DB, replaying with
//! HMAC verification, decay over time, and drift detection.

use borg_core::vitals::{self, EventCategory, StatDeltas};

mod common;
use common::test_db;

// ── Test: record and retrieve vitals events ──

#[test]
fn record_and_retrieve_events() {
    let db = test_db();

    let deltas = vitals::deltas_for(EventCategory::Interaction);
    db.record_vitals_event("interaction", "test", &deltas, None)
        .expect("record event");

    let events = db.vitals_events_since(0).expect("get events");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].category, "interaction");
    assert_eq!(events[0].source, "test");
}

// ── Test: HMAC chain links events ──

#[test]
fn hmac_chain_links_events() {
    let db = test_db();

    // Record two events
    let d1 = vitals::deltas_for(EventCategory::Interaction);
    let d2 = vitals::deltas_for(EventCategory::Success);
    db.record_vitals_event("interaction", "test", &d1, None)
        .expect("record 1");
    db.record_vitals_event("success", "test", &d2, None)
        .expect("record 2");

    let events = db.vitals_events_since(0).expect("get events");
    assert_eq!(events.len(), 2);

    // Events are returned DESC order, so events[0] is newest
    // The newest event's prev_hmac should equal the oldest event's hmac
    let newest = &events[0];
    let oldest = &events[1];
    assert_eq!(
        newest.prev_hmac, oldest.hmac,
        "HMAC chain should link events"
    );
}

// ── Test: first event chains from "0" ──

#[test]
fn first_event_chains_from_zero() {
    let db = test_db();

    let deltas = vitals::deltas_for(EventCategory::Creation);
    db.record_vitals_event("creation", "test", &deltas, None)
        .expect("record");

    let events = db.vitals_events_since(0).expect("get events");
    assert_eq!(
        events[0].prev_hmac, "0",
        "First event should chain from '0'"
    );
}

// ── Test: multiple event categories recorded independently ──

#[test]
fn multiple_categories_recorded() {
    let db = test_db();

    let categories = [
        "interaction",
        "success",
        "failure",
        "correction",
        "creation",
    ];
    for cat in &categories {
        let ec = EventCategory::parse(cat).expect("parse category");
        let deltas = vitals::deltas_for(ec);
        db.record_vitals_event(cat, "test", &deltas, None)
            .expect("record");
    }

    let events = db.vitals_events_since(0).expect("get events");
    assert_eq!(events.len(), 5);

    let cats: Vec<&str> = events.iter().map(|e| e.category.as_str()).collect();
    for cat in &categories {
        assert!(cats.contains(cat), "Should have {cat} event");
    }
}

// ── Test: baseline values are mid-range ──

#[test]
fn baseline_values_mid_range() {
    let state = vitals::baseline();
    assert_eq!(state.stability, state.focus);
    assert_eq!(state.focus, state.sync);
    assert_eq!(state.sync, state.growth);
    assert_eq!(state.growth, state.happiness);
    assert!(state.stability >= 40 && state.stability <= 60);
}

// ── Test: apply_deltas clamps to 0..100 ──

#[test]
fn apply_deltas_clamps() {
    let mut state = vitals::baseline();

    // Large positive deltas should clamp at 100
    let big_positive = StatDeltas {
        stability: 127,
        focus: 127,
        sync: 127,
        growth: 127,
        happiness: 127,
    };
    vitals::apply_deltas(&mut state, &big_positive);
    assert_eq!(state.stability, 100);
    assert_eq!(state.focus, 100);

    // Large negative deltas should clamp at 0
    let big_negative = StatDeltas {
        stability: -128,
        focus: -128,
        sync: -128,
        growth: -128,
        happiness: -128,
    };
    vitals::apply_deltas(&mut state, &big_negative);
    assert_eq!(state.stability, 0);
    assert_eq!(state.focus, 0);
}

// ── Test: decay reduces stats over time ──

#[test]
fn decay_reduces_stats_over_time() {
    let mut state = vitals::baseline();
    // baseline() anchors timestamps at epoch for replay determinism; simulate
    // a recent interaction so the "no decay within 24h" assertion is meaningful.
    let now = chrono::Utc::now();
    state.last_interaction_at = now;

    // No decay within 24h
    let no_decay = vitals::apply_decay(&state, now);
    assert_eq!(no_decay.stability, state.stability);

    // 72h of inactivity should cause decay
    let future = now + chrono::Duration::hours(72);
    let decayed = vitals::apply_decay(&state, future);
    assert!(
        decayed.stability < state.stability || decayed.sync < state.sync,
        "72h inactivity should cause decay"
    );
}

// ── Test: drift detection flags inactivity ──

#[test]
fn drift_detection_flags_inactivity() {
    let mut state = vitals::baseline();
    // Simulate old interaction
    state.last_interaction_at = chrono::Utc::now() - chrono::Duration::days(7);

    let now = chrono::Utc::now();
    let drift = vitals::detect_drift(&state, now);
    assert!(
        !drift.is_empty(),
        "7 days of inactivity should trigger drift flags"
    );
}

// ── Test: drift detection passes for healthy state ──

#[test]
fn no_drift_for_healthy_state() {
    let mut state = vitals::baseline();
    // baseline() anchors timestamps at the Unix epoch for replay determinism;
    // for a "healthy" check we simulate that the user just interacted.
    let now = chrono::Utc::now();
    state.last_interaction_at = now;
    let drift = vitals::detect_drift(&state, now);
    assert!(drift.is_empty(), "Baseline state should not have drift");
}

// ── Test: classify_tool maps tool names correctly ──

#[test]
fn classify_tool_correct() {
    // Successful tool calls → Success
    let cat = vitals::classify_tool("run_shell", false);
    assert!(matches!(cat, EventCategory::Success));

    // Failed tool calls → Failure
    let cat = vitals::classify_tool("run_shell", true);
    assert!(matches!(cat, EventCategory::Failure));
}

// ── Test: looks_like_correction detects corrections ──

#[test]
fn looks_like_correction_patterns() {
    assert!(vitals::looks_like_correction("no, that's wrong"));
    assert!(vitals::looks_like_correction("don't do that"));
    assert!(!vitals::looks_like_correction("great work, thanks!"));
}

// ── Test: format_compact produces valid output ──

#[test]
fn format_compact_valid() {
    let state = vitals::baseline();
    let output = vitals::format_compact(&state);
    assert!(!output.is_empty());
    // Should contain stat names or values
    assert!(
        output.contains("stab") || output.contains("focus") || output.len() > 10,
        "Compact format should contain stat info"
    );
}

// ── Test: event category parse round-trip ──

#[test]
fn event_category_parse_round_trip() {
    let categories = [
        "interaction",
        "success",
        "failure",
        "correction",
        "creation",
    ];
    for name in &categories {
        let cat = EventCategory::parse(name).expect("parse");
        assert_eq!(cat.to_string().to_lowercase(), *name);
    }
}

// ── Test: event category parse unknown returns None ──

#[test]
fn event_category_parse_unknown() {
    assert!(EventCategory::parse("nonexistent").is_none());
    assert!(EventCategory::parse("").is_none());
}

// ── Test: metadata stored and retrieved ──

#[test]
fn metadata_stored_and_retrieved() {
    let db = test_db();

    let deltas = vitals::deltas_for(EventCategory::Interaction);
    db.record_vitals_event(
        "interaction",
        "test",
        &deltas,
        Some("{\"tool\":\"run_shell\"}"),
    )
    .expect("record with metadata");

    let events = db.vitals_events_since(0).expect("get events");
    assert_eq!(events.len(), 1);
    assert_eq!(
        events[0].metadata_json.as_deref(),
        Some("{\"tool\":\"run_shell\"}")
    );
}
