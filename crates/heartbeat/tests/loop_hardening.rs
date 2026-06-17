//! Integration tests for the heartbeat scheduler's resilience properties.
//!
//! Integration tests compile as a separate crate, so the `cfg(test)` allow
//! list in `crates/heartbeat/src/lib.rs` doesn't apply here. Mirror the
//! relevant lint relaxations explicitly so `cargo clippy --all-targets`
//! stays green.
//!
//! - **Liveness counter** advances on every iteration, including the
//!   forced 60-second wakes that exist to keep the watchdog signal
//!   fresh during legitimately long sleeps.
//! - **Bad cron expressions** never kill the loop — they fall back to
//!   interval mode the same way an unparseable cron at startup would.
//! - **Cron sleeps are capped at LIVENESS_TICK_MAX** so the liveness
//!   counter advances even when the next fire is far in the future.
//!
//! Each test must be able to fail for a real-world reason: a regression
//! in the structural hardening that allows a single bad iteration to
//! freeze the loop, or a future refactor that drops the liveness ticker.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::field_reassign_with_default
)]

use std::sync::atomic::Ordering;
use std::time::Duration;

use heartbeat::scheduler::{
    HeartbeatEvent, HeartbeatResult, HeartbeatScheduler, SkipReason, LIVENESS_TICK_MAX,
};
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

use cortex::config::{Config, HeartbeatConfig};

fn hb_config(interval: &str) -> HeartbeatConfig {
    HeartbeatConfig {
        interval: interval.to_string(),
        min_interval_secs: 1,
        quiet_hours_start: None,
        quiet_hours_end: None,
        cron: None,
        channels: Vec::new(),
        recipients: std::collections::BTreeMap::new(),
        ..Default::default()
    }
}

/// Liveness counter must advance on EVERY iteration, including the
/// dedicated 60s liveness ticker that exists for the watchdog. We use
/// virtual time so the test runs in milliseconds — we explicitly advance
/// past LIVENESS_TICK_MAX twice and assert the counter moved both times.
#[tokio::test(start_paused = true)]
async fn liveness_advances_during_long_idle_interval() {
    // 1h interval — the loop would otherwise sleep for an hour. The
    // forced LIVENESS_TICK_MAX wake is the only thing that can make the
    // counter advance during that time.
    let config = hb_config("3600s");

    let (_poke_tx, poke_rx) = mpsc::channel(1);
    let (fire_tx, mut fire_rx) = mpsc::channel(64);
    let cancel = CancellationToken::new();

    let scheduler = HeartbeatScheduler::new(config, chrono_tz::UTC, poke_rx);
    let liveness = scheduler.liveness_handle();

    let cancel_clone = cancel.clone();
    tokio::spawn(async move { scheduler.run(fire_tx, cancel_clone).await });

    tokio::task::yield_now().await;
    // Drain SchedulerStarted.
    let _ = tokio::time::timeout(Duration::from_secs(2), fire_rx.recv())
        .await
        .expect("started event")
        .expect("channel open");
    // Snapshot AFTER startup so the initial boot iteration is excluded
    // from the assertion — we want to prove the *liveness ticker* makes
    // the loop wake during a long sleep.
    let after_boot = liveness.load(Ordering::Relaxed);

    // Advance just past one LIVENESS_TICK_MAX wake. The counter must
    // have advanced — if it hadn't, the watchdog would be unable to
    // distinguish "sleeping for an hour normally" from "stalled."
    tokio::time::advance(LIVENESS_TICK_MAX + Duration::from_secs(2)).await;
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    let after_one_tick = liveness.load(Ordering::Relaxed);
    assert!(
        after_one_tick > after_boot,
        "liveness must advance past after_boot after one LIVENESS_TICK_MAX wake (after_boot={after_boot}, after={after_one_tick})"
    );

    // Advance another LIVENESS_TICK_MAX — must advance again.
    tokio::time::advance(LIVENESS_TICK_MAX + Duration::from_secs(2)).await;
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    let after_two_ticks = liveness.load(Ordering::Relaxed);
    assert!(
        after_two_ticks > after_one_tick,
        "liveness must keep advancing across multiple liveness ticks (got after_one={after_one_tick}, after_two={after_two_ticks})"
    );

    // No Fire events — only liveness ticks.
    while let Ok(ev) = fire_rx.try_recv() {
        if let HeartbeatEvent::Fire = ev {
            panic!("liveness ticks must not produce Fire events");
        }
    }

    cancel.cancel();
}

/// An invalid cron expression at startup falls back to interval mode —
/// existing behavior preserved. Asserts that the iteration body's
/// `Schedule::from_str` failure path doesn't suddenly start panicking
/// (e.g. if a future refactor moved the validation inside the iteration
/// without a panic guard).
#[tokio::test]
async fn bad_cron_at_startup_falls_back_without_panic() {
    let mut config = hb_config("30m");
    config.cron = Some("absolutely not a cron expression".to_string());

    let (poke_tx, poke_rx) = mpsc::channel(8);
    let (fire_tx, mut fire_rx) = mpsc::channel(32);
    let cancel = CancellationToken::new();

    let scheduler = HeartbeatScheduler::new(config, chrono_tz::UTC, poke_rx);
    let liveness = scheduler.liveness_handle();
    let cancel_clone = cancel.clone();
    let handle = tokio::spawn(async move { scheduler.run(fire_tx, cancel_clone).await });

    // Drain SchedulerStarted (interval-mode fallback).
    let started = tokio::time::timeout(Duration::from_secs(2), fire_rx.recv())
        .await
        .expect("started event")
        .expect("channel open");
    match started {
        HeartbeatEvent::SchedulerStarted { mode } => {
            assert!(
                mode.starts_with("interval"),
                "bad cron must fall back to interval mode, got: {mode}"
            );
        }
        other => panic!("expected SchedulerStarted, got: {other:?}"),
    }

    // Wait briefly so at least one iteration completes.
    for _ in 0..50 {
        tokio::task::yield_now().await;
        tokio::time::sleep(Duration::from_millis(20)).await;
        if liveness.load(Ordering::Relaxed) > 0 {
            break;
        }
    }
    assert!(
        liveness.load(Ordering::Relaxed) > 0,
        "loop must have iterated at least once after startup"
    );

    // Loop is still alive: a poke produces a Fire.
    poke_tx.send(()).await.unwrap();
    let event = tokio::time::timeout(Duration::from_secs(2), fire_rx.recv())
        .await
        .expect("poke fire")
        .expect("channel open");
    assert!(matches!(event, HeartbeatEvent::Fire));

    cancel.cancel();
    let _ = handle.await;
}

/// In cron mode, when the next fire is far in the future, the inner
/// sleep is capped at LIVENESS_TICK_MAX so the liveness counter
/// advances. Without this cap, a 1-hour cron schedule would let the
/// watchdog falsely flag the loop as stalled after 5min.
#[tokio::test(start_paused = true)]
async fn cron_long_wait_is_capped_for_liveness() {
    // Cron firing once on 2099-01-01 — `chrono::Local::now()` ignores
    // `tokio::time::pause`, so any periodic expression (e.g. `0 0/10 *
    // * * *`) makes the test flaky when wall time happens to sit
    // within LIVENESS_TICK_MAX of a fire boundary: `did_clip` would
    // be false, virtual sleep would complete, and a real Fire would
    // be emitted. A single fire decades away guarantees the cap
    // clips every iteration regardless of when the test runs.
    let mut config = hb_config("30m");
    config.cron = Some("0 0 0 1 1 * 2099".to_string());
    config.min_interval_secs = 1;

    let (_poke_tx, poke_rx) = mpsc::channel(1);
    let (fire_tx, mut fire_rx) = mpsc::channel(32);
    let cancel = CancellationToken::new();

    let scheduler = HeartbeatScheduler::new(config, chrono_tz::UTC, poke_rx);
    let liveness = scheduler.liveness_handle();
    let cancel_clone = cancel.clone();
    tokio::spawn(async move { scheduler.run(fire_tx, cancel_clone).await });

    tokio::task::yield_now().await;
    // Drain SchedulerStarted.
    let _ = tokio::time::timeout(Duration::from_secs(2), fire_rx.recv())
        .await
        .expect("started")
        .expect("open");

    let after_boot = liveness.load(Ordering::Relaxed);
    // Advance just past LIVENESS_TICK_MAX. With the cap, the cron loop
    // must wake on the cap (not after 10 minutes) and refresh liveness.
    tokio::time::advance(LIVENESS_TICK_MAX + Duration::from_secs(2)).await;
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    let after = liveness.load(Ordering::Relaxed);
    assert!(
        after > after_boot,
        "cron loop must touch liveness within LIVENESS_TICK_MAX even with no fire due (after_boot={after_boot}, after={after})"
    );

    // The cap-induced wake must NOT emit Fire (no real fire was due).
    while let Ok(ev) = fire_rx.try_recv() {
        if let HeartbeatEvent::Fire = ev {
            panic!("liveness cap wake must not emit Fire — no cron fire was due");
        }
    }

    cancel.cancel();
}

/// Liveness handle returned by the scheduler is the same Arc as the one
/// it updates internally. Without this guarantee, an external watchdog
/// could read a stale value forever even while the loop iterates.
#[tokio::test]
async fn liveness_handle_observes_updates_made_by_running_loop() {
    let config = hb_config("60s");

    let (_poke_tx, poke_rx) = mpsc::channel(1);
    let (fire_tx, _fire_rx) = mpsc::channel(32);
    let cancel = CancellationToken::new();

    let scheduler = HeartbeatScheduler::new(config, chrono_tz::UTC, poke_rx);
    let liveness = scheduler.liveness_handle();
    // New schedulers start at 0; any positive value means the running
    // loop touched it.
    assert_eq!(liveness.load(Ordering::Relaxed), 0);

    let cancel_clone = cancel.clone();
    tokio::spawn(async move { scheduler.run(fire_tx, cancel_clone).await });

    // Wait briefly: the scheduler boots and the first iteration return
    // increments the counter we hold.
    for _ in 0..50 {
        tokio::task::yield_now().await;
        tokio::time::sleep(Duration::from_millis(20)).await;
        if liveness.load(Ordering::Relaxed) > 0 {
            break;
        }
    }

    let later = liveness.load(Ordering::Relaxed);
    assert!(
        later > 0,
        "the externally-held liveness handle must observe updates from the running loop (got {later})"
    );

    cancel.cancel();
}

/// Source-grep guard for an invariant types cannot express: each
/// `run_interval` / `run_cron` iteration body is wrapped in
/// `AssertUnwindSafe(...).catch_unwind()` so a panic inside the
/// iteration becomes a logged backoff instead of a task death. This
/// is layer 1 of the three-layer safety net documented in
/// `docs/daemon-supervision.md`; without it, every iteration panic
/// would escape to the supervisor, burn a restart, and risk tripping
/// the degraded-state cap on transient bugs.
///
/// Precedent for the source-grep pattern: the TUI mouse-capture
/// guards in `crates/interface/src/tui/mod.rs` and `app.rs` use the
/// same `include_str!` approach for a structurally-equivalent
/// invariant. Do not weaken — if the wrapper is renamed/refactored,
/// update this guard with the new pattern rather than deleting it.
#[test]
fn run_loops_wrap_each_iteration_in_catch_unwind() {
    let src = include_str!("../src/scheduler.rs");
    assert!(
        src.contains("AssertUnwindSafe(self.interval_iteration(")
            && src.contains(".catch_unwind()"),
        "run_interval must wrap interval_iteration in AssertUnwindSafe().catch_unwind() — defense layer 1 of docs/daemon-supervision.md"
    );
    assert!(
        src.contains("AssertUnwindSafe(self.cron_iteration("),
        "run_cron must wrap cron_iteration in AssertUnwindSafe().catch_unwind() — defense layer 1 of docs/daemon-supervision.md"
    );
    // The matching panic-arm in run_interval/run_cron consumes the
    // catch_unwind payload, logs it, and falls through to the next
    // iteration. Removing this arm turns a caught panic into a Stop.
    // Require BOTH loops to keep it — a single substring match would
    // pass if only one of run_interval/run_cron had the arm.
    let payload_arms = src.matches("Err(payload) =>").count();
    assert!(
        payload_arms >= 2 && src.contains("panic_msg(&payload)"),
        "both run_interval and run_cron must consume the catch_unwind payload via Err(payload) => panic_msg(&payload); found {payload_arms} arms"
    );
}

/// Reload-induced quiet-hours change still applies once the per-iteration
/// panic isolation wraps the body — guards against that wrapping breaking
/// the apply_config path.
#[tokio::test(start_paused = true)]
async fn config_reload_still_applies_after_hardening() {
    let hb = hb_config("60s");
    let mut cfg = Config::default();
    cfg.heartbeat = hb.clone();
    let (cfg_tx, cfg_rx) = watch::channel(cfg);

    let (_poke_tx, poke_rx) = mpsc::channel(1);
    let (fire_tx, mut fire_rx) = mpsc::channel(64);
    let cancel = CancellationToken::new();

    let mut scheduler = HeartbeatScheduler::new(hb.clone(), chrono_tz::UTC, poke_rx);
    scheduler.set_config_rx(cfg_rx);
    let cancel_clone = cancel.clone();
    tokio::spawn(async move { scheduler.run(fire_tx, cancel_clone).await });

    tokio::task::yield_now().await;
    // Drain SchedulerStarted.
    let _ = tokio::time::timeout(Duration::from_secs(2), fire_rx.recv())
        .await
        .expect("started")
        .expect("open");

    // Push update enabling all-day quiet hours.
    let mut updated_hb = hb;
    updated_hb.quiet_hours_start = Some("00:00".to_string());
    updated_hb.quiet_hours_end = Some("23:59".to_string());
    let mut updated_cfg = Config::default();
    updated_cfg.heartbeat = updated_hb;
    cfg_tx.send(updated_cfg).expect("send");

    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    tokio::time::advance(Duration::from_secs(62)).await;
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }

    let mut saw_skip = false;
    while let Ok(ev) = fire_rx.try_recv() {
        if let HeartbeatEvent::Result(HeartbeatResult::Skipped {
            reason: SkipReason::QuietHours,
        }) = ev
        {
            saw_skip = true;
            break;
        }
    }
    cancel.cancel();
    assert!(
        saw_skip,
        "reload-induced quiet-hours change must still suppress the next fire"
    );
}
