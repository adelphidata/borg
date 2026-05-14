//! Integration tests for the four `/goal` Admin RPCs added in the Ralph-loop
//! E2E port: `GetGoal`, `SetGoal`, `ClearGoal`, `PauseGoalOnInterrupt`.
//!
//! Each test seeds a temp BORG_DATA_DIR, spins a borgd over a UDS, and drives
//! the tonic-generated client against it. Tests share process state
//! (BORG_DATA_DIR is read at every `Database::open()`) so they are serialized
//! through `SERIAL`. We bundle positive AND negative cases per RPC into the
//! same test body — the harness setup cost dwarfs the assertions.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::type_complexity,
    clippy::await_holding_lock
)]

use borgd::daemon::bind_uds;
use borgd::goal_bus::GoalEventBus;
use borgd::grpc::admin::AdminSvc;
use borgd::session::SessionRegistry;
use cortex::db::Database;
use protocols::admin::admin_client::AdminClient;
use protocols::admin::admin_server::AdminServer;
use protocols::admin::{goal_mutation::Op as GoalOp, GoalMutation, GoalRef};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;
use tokio::net::UnixStream;
use tokio::sync::oneshot;
use tokio_stream::wrappers::UnixListenerStream;
use tonic::transport::{Channel, Endpoint, Uri};
use tower::service_fn;

static SERIAL: Mutex<()> = Mutex::new(());

struct Harness {
    _tmp: tempfile::TempDir,
    socket: PathBuf,
    _shutdown: oneshot::Sender<()>,
}

async fn start_harness() -> Harness {
    let tmp = tempfile::tempdir().expect("tempdir");
    // SAFETY: tests in this binary are serialized via SERIAL.
    unsafe {
        std::env::set_var("BORG_DATA_DIR", tmp.path());
    }
    // Force the DB to materialize and run migrations now so the test body
    // can seed sessions/goals through the same file the handlers reopen.
    let _ = Database::open().expect("seed DB open");

    let socket = tmp.path().join("borgd.sock");
    let listener = bind_uds(&socket).expect("bind_uds");
    let stream = UnixListenerStream::new(listener);
    let registry = SessionRegistry::new();
    let bus = GoalEventBus::new();
    let admin = AdminSvc::new(registry).with_goal_bus(bus);

    let (tx, rx) = oneshot::channel::<()>();
    tokio::spawn(async move {
        let _ = tonic::transport::Server::builder()
            .add_service(AdminServer::new(admin))
            .serve_with_incoming_shutdown(stream, async {
                let _ = rx.await;
            })
            .await;
    });
    let started = std::time::Instant::now();
    while UnixStream::connect(&socket).await.is_err() {
        if started.elapsed() > Duration::from_secs(2) {
            panic!("daemon UDS never became connectable");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    Harness {
        _tmp: tmp,
        socket,
        _shutdown: tx,
    }
}

async fn channel(socket: &Path) -> Channel {
    let p = socket.to_path_buf();
    Endpoint::try_from(protocols::TONIC_UDS_PLACEHOLDER_URL)
        .unwrap()
        .connect_with_connector(service_fn(move |_: Uri| {
            let p = p.clone();
            async move {
                let s = UnixStream::connect(&p).await?;
                Ok::<_, std::io::Error>(hyper_util::rt::TokioIo::new(s))
            }
        }))
        .await
        .unwrap()
}

/// Seed a session row (`goals` rows have FK → `sessions.id`).
fn seed_session(id: &str) {
    let db = Database::open().expect("db");
    db.upsert_session(id, 0, 0, 0, "test-model", "test")
        .expect("seed session");
}

/// Positive: empty session_id resolves to the latest session and returns its
/// current goal. Negative: GetGoal on a session with no goal row returns
/// `present=false`, NOT an error.
#[tokio::test]
async fn get_goal_resolves_latest_and_reports_absence() {
    let _g = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let h = start_harness().await;
    let chan = channel(&h.socket).await;
    let mut admin = AdminClient::new(chan);

    // Negative: no sessions exist at all → present=false.
    let resp = admin
        .get_goal(GoalRef {
            session_id: String::new(),
        })
        .await
        .expect("get_goal call")
        .into_inner();
    assert!(
        !resp.present,
        "with no sessions, empty session_id must report absence (got: {resp:?})"
    );

    // Seed a session + goal, then ask for "latest" → must return that row.
    seed_session("sess-latest");
    {
        let db = Database::open().unwrap();
        db.insert_goal(
            "sess-latest",
            "ship it",
            cortex::goals::GoalStatus::Active,
            Some(5000),
        )
        .expect("insert")
        .expect("returned");
    }
    let resp = admin
        .get_goal(GoalRef {
            session_id: String::new(),
        })
        .await
        .expect("get_goal latest")
        .into_inner();
    assert!(resp.present, "latest goal must surface, got: {resp:?}");
    assert_eq!(resp.session_id, "sess-latest");
    assert_eq!(resp.objective, "ship it");
    assert_eq!(resp.status, "active");
    assert!(resp.token_budget_present);
    assert_eq!(resp.token_budget, 5000);

    // Negative: explicit session_id that has no goal row.
    seed_session("sess-without-goal");
    let resp = admin
        .get_goal(GoalRef {
            session_id: "sess-without-goal".into(),
        })
        .await
        .expect("get_goal explicit")
        .into_inner();
    assert!(
        !resp.present,
        "session with no goal row must report absence, got: {resp:?}"
    );
}

/// Positive: SetGoal{Create} inserts a new row.
/// Negative: a second SetGoal{Create} on the same session is rejected as
/// AlreadyExists so the client can branch into a Replace flow without
/// parsing the error message.
#[tokio::test]
async fn set_goal_create_rejects_duplicate() {
    let _g = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let h = start_harness().await;
    let chan = channel(&h.socket).await;
    let mut admin = AdminClient::new(chan);

    seed_session("sess-create");
    let detail = admin
        .set_goal(GoalMutation {
            session_id: "sess-create".into(),
            op: GoalOp::Create as i32,
            objective: "first".into(),
            token_budget_present: true,
            token_budget: 1000,
            status: String::new(),
        })
        .await
        .expect("create first")
        .into_inner();
    assert!(detail.present);
    assert_eq!(detail.objective, "first");
    assert_eq!(detail.status, "active");

    let err = admin
        .set_goal(GoalMutation {
            session_id: "sess-create".into(),
            op: GoalOp::Create as i32,
            objective: "second".into(),
            token_budget_present: false,
            token_budget: 0,
            status: String::new(),
        })
        .await
        .expect_err("duplicate create must fail");
    assert_eq!(
        err.code(),
        tonic::Code::AlreadyExists,
        "duplicate create must surface as AlreadyExists, got code={:?}, msg={}",
        err.code(),
        err.message()
    );
    assert!(
        err.message().to_lowercase().contains("already exists"),
        "error message should explain the duplicate, got: {}",
        err.message()
    );
}

/// Positive: SetGoal{Replace} regenerates `goal_id` even when a row already
/// exists, and resets accounting. Also tests the "create on empty" branch.
#[tokio::test]
async fn set_goal_replace_regenerates_goal_id() {
    let _g = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let h = start_harness().await;
    let chan = channel(&h.socket).await;
    let mut admin = AdminClient::new(chan);

    seed_session("sess-replace");
    let first = admin
        .set_goal(GoalMutation {
            session_id: "sess-replace".into(),
            op: GoalOp::Replace as i32,
            objective: "v1".into(),
            token_budget_present: false,
            token_budget: 0,
            status: String::new(),
        })
        .await
        .expect("replace#1")
        .into_inner();
    let first_id = first.goal_id.clone();
    assert!(!first_id.is_empty());

    let second = admin
        .set_goal(GoalMutation {
            session_id: "sess-replace".into(),
            op: GoalOp::Replace as i32,
            objective: "v2".into(),
            token_budget_present: true,
            token_budget: 7777,
            status: String::new(),
        })
        .await
        .expect("replace#2")
        .into_inner();
    assert_eq!(second.objective, "v2");
    assert_eq!(second.token_budget, 7777);
    assert_eq!(second.status, "active");
    assert_ne!(
        second.goal_id, first_id,
        "Replace must regenerate goal_id so accounting can detect the swap"
    );
    assert_eq!(second.tokens_used, 0, "Replace must reset tokens_used to 0");
}

/// Negative: `set_goal{SetStatus}` rejects an illegal transition like
/// Complete -> Active.
/// Negative: BudgetLimited cannot be set via RPC at all (system-only).
#[tokio::test]
async fn set_goal_status_rejects_illegal_transitions() {
    let _g = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let h = start_harness().await;
    let chan = channel(&h.socket).await;
    let mut admin = AdminClient::new(chan);

    seed_session("sess-status");
    admin
        .set_goal(GoalMutation {
            session_id: "sess-status".into(),
            op: GoalOp::Create as i32,
            objective: "x".into(),
            token_budget_present: false,
            token_budget: 0,
            status: String::new(),
        })
        .await
        .expect("create");
    // Active -> Complete is allowed.
    admin
        .set_goal(GoalMutation {
            session_id: "sess-status".into(),
            op: GoalOp::SetStatus as i32,
            objective: String::new(),
            token_budget_present: false,
            token_budget: 0,
            status: "complete".into(),
        })
        .await
        .expect("active->complete");

    // Negative: Complete -> Active is forbidden.
    let err = admin
        .set_goal(GoalMutation {
            session_id: "sess-status".into(),
            op: GoalOp::SetStatus as i32,
            objective: String::new(),
            token_budget_present: false,
            token_budget: 0,
            status: "active".into(),
        })
        .await
        .expect_err("complete->active must fail");
    assert_eq!(
        err.code(),
        tonic::Code::InvalidArgument,
        "illegal transition must be InvalidArgument, got code={:?}",
        err.code()
    );
    assert!(
        err.message().contains("illegal transition"),
        "expected illegal-transition error, got: {}",
        err.message()
    );

    // Negative: budget_limited is system-only.
    let err = admin
        .set_goal(GoalMutation {
            session_id: "sess-status".into(),
            op: GoalOp::SetStatus as i32,
            objective: String::new(),
            token_budget_present: false,
            token_budget: 0,
            status: "budget_limited".into(),
        })
        .await
        .expect_err("budget_limited via RPC must be rejected");
    assert_eq!(
        err.code(),
        tonic::Code::InvalidArgument,
        "system-only status must be InvalidArgument, got code={:?}",
        err.code()
    );
    assert!(
        err.message().to_lowercase().contains("system-only"),
        "expected system-only rejection, got: {}",
        err.message()
    );
}

/// Positive: PauseGoalOnInterrupt flips an Active goal to Paused.
/// Negative: when no Active goal exists, it must be a no-op (present=false),
/// NOT an error — `/cancel` must succeed even with no goal.
#[tokio::test]
async fn pause_goal_on_interrupt_is_idempotent_noop_when_absent() {
    let _g = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let h = start_harness().await;
    let chan = channel(&h.socket).await;
    let mut admin = AdminClient::new(chan);

    // Negative: no sessions at all — empty session_id → absence, no error.
    let resp = admin
        .pause_goal_on_interrupt(GoalRef {
            session_id: String::new(),
        })
        .await
        .expect("pause-when-no-sessions must succeed")
        .into_inner();
    assert!(!resp.present, "no sessions → absence, got: {resp:?}");

    // Negative: session with no goal — also absence.
    seed_session("sess-pause");
    let resp = admin
        .pause_goal_on_interrupt(GoalRef {
            session_id: "sess-pause".into(),
        })
        .await
        .expect("pause without goal must succeed")
        .into_inner();
    assert!(
        !resp.present,
        "session with no goal must report absence, got: {resp:?}"
    );

    // Positive: Active goal → pause flips it to Paused.
    admin
        .set_goal(GoalMutation {
            session_id: "sess-pause".into(),
            op: GoalOp::Create as i32,
            objective: "do thing".into(),
            token_budget_present: false,
            token_budget: 0,
            status: String::new(),
        })
        .await
        .expect("create active");
    let resp = admin
        .pause_goal_on_interrupt(GoalRef {
            session_id: "sess-pause".into(),
        })
        .await
        .expect("pause active")
        .into_inner();
    assert!(resp.present);
    assert_eq!(
        resp.status, "paused",
        "Active goal must transition to Paused on interrupt, got: {resp:?}"
    );

    // Negative: a second pause on the now-Paused goal is a no-op
    // (`pause_active_goal` only acts on Active rows).
    let resp = admin
        .pause_goal_on_interrupt(GoalRef {
            session_id: "sess-pause".into(),
        })
        .await
        .expect("pause already-paused")
        .into_inner();
    assert!(
        !resp.present,
        "already-paused goal must NOT re-emit a pause result (got: {resp:?})"
    );
}

/// Positive: ClearGoal removes the row and a subsequent GetGoal reports
/// absence. Negative: clearing a session that has no goal is a silent no-op.
#[tokio::test]
async fn clear_goal_removes_row_and_is_noop_when_absent() {
    let _g = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let h = start_harness().await;
    let chan = channel(&h.socket).await;
    let mut admin = AdminClient::new(chan);

    seed_session("sess-clear");

    // Negative: clearing with no row must succeed silently.
    admin
        .clear_goal(GoalRef {
            session_id: "sess-clear".into(),
        })
        .await
        .expect("clear-without-row must succeed");

    // Positive: create then clear → GetGoal reports absence.
    admin
        .set_goal(GoalMutation {
            session_id: "sess-clear".into(),
            op: GoalOp::Create as i32,
            objective: "delete me".into(),
            token_budget_present: false,
            token_budget: 0,
            status: String::new(),
        })
        .await
        .expect("create");
    let before = admin
        .get_goal(GoalRef {
            session_id: "sess-clear".into(),
        })
        .await
        .unwrap()
        .into_inner();
    assert!(before.present, "row must exist before clear");

    admin
        .clear_goal(GoalRef {
            session_id: "sess-clear".into(),
        })
        .await
        .expect("clear existing");
    let after = admin
        .get_goal(GoalRef {
            session_id: "sess-clear".into(),
        })
        .await
        .unwrap()
        .into_inner();
    assert!(
        !after.present,
        "ClearGoal must remove the row, got: {after:?}"
    );
}
