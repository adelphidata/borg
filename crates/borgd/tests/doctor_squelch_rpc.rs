//! Admin RPC coverage for `DoctorSquelchSet` / `DoctorSquelchClear`.
//!
//! Real failure modes guarded:
//! - validation: empty category/name or past-expiry leaks a no-op write or
//!   silently succeeds instead of returning `invalid_argument`
//! - happy-path: handler doesn't actually persist or doesn't actually delete
//! - clear-when-absent: returns `removed=false` rather than erroring out
//!
//! Calls the in-process `AdminSvc` directly — the BORG_DATA_DIR env points
//! at a tempdir so each test owns its DB. Serialized with a process-wide
//! mutex because `Database::open()` re-reads BORG_DATA_DIR on every call.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::await_holding_lock)]

use borgd::grpc::admin::AdminSvc;
use borgd::session::SessionRegistry;
use cortex::db::Database;
use protocols::admin::admin_server::Admin;
use protocols::admin::{DoctorSquelchClearRequest, DoctorSquelchSetRequest};
use std::sync::Mutex;
use tonic::{Code, Request};

static SERIAL: Mutex<()> = Mutex::new(());

fn fresh_admin() -> (AdminSvc, tempfile::TempDir) {
    let tmp = tempfile::tempdir().expect("tempdir");
    // SAFETY: tests in this binary are serialized via SERIAL.
    unsafe {
        std::env::set_var("BORG_DATA_DIR", tmp.path());
    }
    // Materialize the DB so subsequent handler calls reopen the same file.
    let _ = Database::open().expect("seed DB open");
    (AdminSvc::new(SessionRegistry::new()), tmp)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn doctor_squelch_set_then_clear_round_trips_through_db() {
    let _g = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (admin, _tmp) = fresh_admin();

    let now = chrono::Utc::now().timestamp();
    let expires_at = now + 30 * 86_400;

    Admin::doctor_squelch_set(
        &admin,
        Request::new(DoctorSquelchSetRequest {
            category: "Host Security".into(),
            name: "Application Firewall".into(),
            expires_at_unix: expires_at,
            reason: "ack".into(),
        }),
    )
    .await
    .expect("squelch_set");

    // Verify persisted via a readonly handle so we're not just trusting the
    // handler's own return.
    let db = Database::open_readonly().expect("readonly");
    let active = db.doctor_squelch_list_active(now).expect("list");
    assert_eq!(active.len(), 1, "set must persist exactly one row");
    assert_eq!(active[0].category, "Host Security");
    assert_eq!(active[0].name, "Application Firewall");
    assert_eq!(active[0].expires_at, expires_at);
    assert_eq!(active[0].reason.as_deref(), Some("ack"));

    let resp = Admin::doctor_squelch_clear(
        &admin,
        Request::new(DoctorSquelchClearRequest {
            category: "Host Security".into(),
            name: "Application Firewall".into(),
        }),
    )
    .await
    .expect("squelch_clear")
    .into_inner();
    assert!(
        resp.removed,
        "clear of an existing row must report removed=true"
    );

    let active_after = db.doctor_squelch_list_active(now).expect("list");
    assert!(
        active_after.is_empty(),
        "clear must actually delete the row"
    );

    // Clearing again is a clean no-op (no error, removed=false).
    let resp2 = Admin::doctor_squelch_clear(
        &admin,
        Request::new(DoctorSquelchClearRequest {
            category: "Host Security".into(),
            name: "Application Firewall".into(),
        }),
    )
    .await
    .expect("squelch_clear idempotent")
    .into_inner();
    assert!(
        !resp2.removed,
        "clearing an absent row must report removed=false, not error"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn doctor_squelch_set_rejects_empty_and_past_expiry() {
    let _g = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (admin, _tmp) = fresh_admin();

    let future = chrono::Utc::now().timestamp() + 3600;

    let empty_cat = Admin::doctor_squelch_set(
        &admin,
        Request::new(DoctorSquelchSetRequest {
            category: String::new(),
            name: "X".into(),
            expires_at_unix: future,
            reason: String::new(),
        }),
    )
    .await
    .expect_err("empty category must error");
    assert_eq!(empty_cat.code(), Code::InvalidArgument);

    let empty_name = Admin::doctor_squelch_set(
        &admin,
        Request::new(DoctorSquelchSetRequest {
            category: "X".into(),
            name: String::new(),
            expires_at_unix: future,
            reason: String::new(),
        }),
    )
    .await
    .expect_err("empty name must error");
    assert_eq!(empty_name.code(), Code::InvalidArgument);

    let past = Admin::doctor_squelch_set(
        &admin,
        Request::new(DoctorSquelchSetRequest {
            category: "Host Security".into(),
            name: "ALF".into(),
            expires_at_unix: 1, // long past
            reason: String::new(),
        }),
    )
    .await
    .expect_err("past expiry must error");
    assert_eq!(past.code(), Code::InvalidArgument);

    // None of the rejected calls should have persisted anything.
    let db = Database::open_readonly().expect("readonly");
    let active = db.doctor_squelch_list_active(0).expect("list");
    assert!(
        active.is_empty(),
        "rejected calls must not write — found {} row(s)",
        active.len()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn doctor_squelch_clear_rejects_empty_args() {
    let _g = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (admin, _tmp) = fresh_admin();

    let empty = Admin::doctor_squelch_clear(
        &admin,
        Request::new(DoctorSquelchClearRequest {
            category: String::new(),
            name: String::new(),
        }),
    )
    .await
    .expect_err("empty args must error");
    assert_eq!(empty.code(), Code::InvalidArgument);
}
