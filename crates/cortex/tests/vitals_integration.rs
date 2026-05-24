//! Vitals event-sourcing integration tests.
//!
//! Scope: end-to-end DB persistence and HMAC chain linkage. Pure-function
//! coverage (apply_deltas, decay, drift, classify_tool, looks_like_correction,
//! format_compact, event-category round-trip, baseline shape) lives in the
//! crate's inline `cortex::vitals::tests` module — duplicating those at the
//! integration layer adds no real-world failure mode and just inflates the
//! `tee` log. Tests below must drive the DB path: record → read back →
//! assert on the HMAC chain or stored metadata.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::uninlined_format_args
)]

use cortex::vitals::{self, EventCategory};

mod common;
use common::test_db;

// ── DB write/read round-trip ──

#[tokio::test]
async fn record_and_retrieve_events() {
    let db = test_db().await;

    let deltas = vitals::deltas_for(EventCategory::Interaction);
    db.record_vitals_event("interaction", "test", &deltas, None)
        .await
        .expect("record event");

    let events = db.vitals_events_since(0).await.expect("get events");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].category, "interaction");
    assert_eq!(events[0].source, "test");
}

// HMAC chain is a load-bearing invariant.

#[tokio::test]
async fn hmac_chain_links_events() {
    let db = test_db().await;

    // Record two events
    let d1 = vitals::deltas_for(EventCategory::Interaction);
    let d2 = vitals::deltas_for(EventCategory::Success);
    db.record_vitals_event("interaction", "test", &d1, None)
        .await
        .expect("record 1");
    db.record_vitals_event("success", "test", &d2, None)
        .await
        .expect("record 2");

    let events = db.vitals_events_since(0).await.expect("get events");
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

#[tokio::test]
async fn first_event_chains_from_zero() {
    let db = test_db().await;

    let deltas = vitals::deltas_for(EventCategory::Creation);
    db.record_vitals_event("creation", "test", &deltas, None)
        .await
        .expect("record");

    let events = db.vitals_events_since(0).await.expect("get events");
    assert_eq!(
        events[0].prev_hmac, "0",
        "First event should chain from '0'"
    );
}

// ── All categories survive a DB write/read round-trip ──
//
// The pure parse/Display round-trip is covered by unit tests; the DB layer
// adds the extra failure mode that the rusqlite mapping for `category` could
// silently drop or rename values. Asserting bidirectional set equality
// catches both missing inputs and unexpected extras.
#[tokio::test]
async fn multiple_categories_recorded() {
    let db = test_db().await;

    let categories = [
        "interaction",
        "success",
        "failure",
        "correction",
        "creation",
        "negative_sentiment",
    ];
    for cat in &categories {
        let ec = EventCategory::parse(cat).expect("parse category");
        let deltas = vitals::deltas_for(ec);
        db.record_vitals_event(cat, "test", &deltas, None)
            .await
            .expect("record");
    }

    let events = db.vitals_events_since(0).await.expect("get events");
    assert_eq!(
        events.len(),
        categories.len(),
        "every distinct-category insert should persist (none share an hourly cap with each other in a fresh DB)"
    );

    let recorded: std::collections::HashSet<&str> =
        events.iter().map(|e| e.category.as_str()).collect();
    let expected: std::collections::HashSet<&str> = categories.iter().copied().collect();
    assert_eq!(recorded, expected, "recorded set differs from inserted set");
}

// ── Metadata survives the DB column round-trip ──

#[tokio::test]
async fn metadata_stored_and_retrieved() {
    let db = test_db().await;

    let deltas = vitals::deltas_for(EventCategory::Interaction);
    db.record_vitals_event(
        "interaction",
        "test",
        &deltas,
        Some("{\"tool\":\"run_shell\"}"),
    )
    .await
    .expect("record with metadata");

    let events = db.vitals_events_since(0).await.expect("get events");
    assert_eq!(events.len(), 1);
    assert_eq!(
        events[0].metadata_json.as_deref(),
        Some("{\"tool\":\"run_shell\"}")
    );
}
