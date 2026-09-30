//! Integration tests for the admin/status RPCs added in sub-stage 1 of the
//! popup-migration work. Each test seeds a temp BORG_HOME with realistic
//! fixtures, spins a borgd over a UDS, and asserts on a SPECIFIC observable
//! outcome that would fail if proto wiring or the underlying reader broke.
//!
//! Tests share process state (BORG_HOME env, `Database::open()` resolves
//! it at request time) so they're serialized through a single mutex. We
//! intentionally use ONE big test per group — splitting into N tiny tests
//! would just multiply the env-mutation surface for no extra signal.
//!
//! `borg-cli` is a binary crate without a public lib, so its `DaemonClient`
//! wrapper isn't reachable from here; tests use the tonic-generated clients
//! directly. Same wire format, same handlers — the wrapper is a thin
//! ergonomic layer (covered by the cli's own compile.)

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::type_complexity,
    clippy::await_holding_lock
)]

use borgd::daemon::bind_uds;
use borgd::grpc::admin::AdminSvc;
use borgd::grpc::status::StatusSvc;
use borgd::session::backend::{InlineImage, SessionBackend};
use borgd::session::{SessionHost, SessionRegistry};
use cortex::db::{Database, NewTask, UpdateTask};
use protocols::admin::admin_client::AdminClient;
use protocols::admin::admin_server::AdminServer;
use protocols::admin::{
    api_key_value::Body as ApiKeyBody, ApiKeyRef, ApiKeyValue, Empty as AdminEmpty, KeychainRef,
    ListSessionsRequest, ScheduledTaskCreate, ScheduledTaskUpdate, SessionArchiveMutation,
    SessionRef, SessionTitleMutation, SettingMutation,
};
use protocols::status::status_client::StatusClient;
use protocols::status::status_server::StatusServer;
use protocols::status::{Empty as StatusEmpty, Int64Cursor};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::net::UnixStream;
use tokio::sync::oneshot;
use tokio_stream::wrappers::UnixListenerStream;
use tonic::transport::{Channel, Endpoint, Uri};
use tower::service_fn;

// Serialize tests in this binary: BORG_HOME is process-global and
// `Database::open()` re-reads it at every request. Without this lock, two
// tests running concurrently would each open the OTHER test's tempdir DB.
static SERIAL: Mutex<()> = Mutex::new(());

struct Harness {
    _tmp: tempfile::TempDir,
    socket: PathBuf,
    /// The same registry the served `AdminSvc` holds, so a test can seed a
    /// live session host and observe what a handler does to it.
    registry: SessionRegistry,
    _shutdown: oneshot::Sender<()>,
}

async fn start_harness() -> Harness {
    let tmp = tempfile::tempdir().expect("tempdir");
    // SAFETY: tests in this binary are serialized via SERIAL.
    unsafe {
        std::env::set_var("BORG_HOME", tmp.path());
    }
    // Force the DB to materialize NOW so seeding code in the test body
    // sees the same file the handlers will reopen.
    let _ = Database::open().await.expect("seed DB open");

    let socket = tmp.path().join("borgd.sock");
    let listener = bind_uds(&socket).expect("bind_uds");
    let stream = UnixListenerStream::new(listener);
    let registry = SessionRegistry::new();
    let admin = AdminSvc::new(registry.clone());
    let status = StatusSvc::new();

    let (tx, rx) = oneshot::channel::<()>();
    tokio::spawn(async move {
        let _ = tonic::transport::Server::builder()
            .add_service(AdminServer::new(admin))
            .add_service(StatusServer::new(status))
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
        registry,
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

#[tokio::test]
async fn admin_rpcs_round_trip_warnings_heal_and_scheduled_tasks() {
    let _g = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let h = start_harness().await;
    let chan = channel(&h.socket).await;
    let mut admin = AdminClient::new(chan);

    // GetPersistentWarnings: empty when no doctor_runs row exists, and
    // round-trips a seeded report's persistent_warnings vector verbatim.
    // Real failure mode: latest_doctor_run returns the JSON blob unparsed
    // or the handler reads `current_issues` instead of `persistent_warnings`.
    let warnings = admin
        .get_persistent_warnings(AdminEmpty {})
        .await
        .expect("warnings")
        .into_inner();
    assert!(warnings.warnings.is_empty(), "no doctor_runs → empty list");
    {
        let db = Database::open().await.expect("db");
        let report = cortex::maintenance::MaintenanceReport {
            ran_at: 1,
            persistent_warnings: vec!["A:check".into(), "B:check".into()],
            current_issues: vec!["other:thing".into()],
            ..Default::default()
        };
        db.record_doctor_run(&report).await.expect("record run");
    }
    let warnings = admin
        .get_persistent_warnings(AdminEmpty {})
        .await
        .expect("warnings")
        .into_inner();
    assert_eq!(
        warnings.warnings,
        vec!["A:check".to_string(), "B:check".to_string()],
        "must surface persistent_warnings, NOT current_issues"
    );

    // Heal: returns the expanded maintenance projection, including
    // wedged-run recovery and explicit skip=false for the default config.
    // Real failure mode: adding fields to MaintenanceReport but forgetting
    // to map them across the gRPC boundary, so the TUI silently renders
    // zeros despite the daemon having done real work.
    {
        let db = Database::open().await.expect("db");
        db.create_task(&NewTask {
            id: "wedged-task",
            name: "wedged",
            prompt: "p",
            schedule_type: "cron",
            schedule_expr: "0 0 * * * *",
            timezone: "local",
            next_run: None,
            max_retries: None,
            timeout_ms: Some(60_000),
            delivery_channel: None,
            delivery_target: None,
            allowed_tools: None,
            task_type: "prompt",
            pre_run_command: None,
            pre_run_cwd: None,
            pre_run_timeout_ms: None,
            min_interval_secs: None,
            continuity: false,
        })
        .await
        .expect("create wedged task");
        let started_at = chrono::Utc::now().timestamp() - 3600;
        db.conn()
            .execute(
                "INSERT INTO task_runs (task_id, started_at, duration_ms, status)
                 VALUES ('wedged-task', ?1, 0, 'running')",
                [started_at],
            )
            .await
            .expect("insert wedged run");
    }
    let heal = admin.heal(AdminEmpty {}).await.expect("heal").into_inner();
    assert!(!heal.skipped, "default config should run maintenance");
    assert_eq!(
        heal.wedged_runs_recovered, 1,
        "healed report must surface recovered wedged runs"
    );
    assert_eq!(
        heal.log_bytes_truncated, 0,
        "no oversized logs were seeded in this harness"
    );

    // Scheduled task UpdateScheduledTask round-trip.
    // Real failure mode: a refactor that made all-empty UpdateTask fields
    // fail validation instead of being a no-op (regressed historically:
    // the popup edits one field at a time, so an empty body MUST succeed).
    {
        let db = Database::open().await.expect("db");
        db.create_task(&NewTask {
            id: "t1",
            name: "orig",
            prompt: "p",
            schedule_type: "interval",
            schedule_expr: "1h",
            timezone: "UTC",
            next_run: None,
            max_retries: None,
            timeout_ms: None,
            delivery_channel: None,
            delivery_target: None,
            allowed_tools: None,
            task_type: "prompt",
            pre_run_command: None,
            pre_run_cwd: None,
            pre_run_timeout_ms: None,
            min_interval_secs: None,
            continuity: false,
        })
        .await
        .expect("create t1");
    }

    // All-empty optionals → leave row unchanged. Confirms the
    // `Option<&str>` -> "leave alone" semantics survive at the DB layer.
    {
        let db = Database::open().await.expect("db");
        let baseline = db
            .list_tasks()
            .await
            .expect("list")
            .into_iter()
            .find(|t| t.id == "t1")
            .expect("t1 baseline");
        db.update_task(
            "t1",
            &UpdateTask {
                name: None,
                prompt: None,
                schedule_type: None,
                schedule_expr: None,
                timezone: None,
            },
        )
        .await
        .expect("noop update");
        let after = db
            .list_tasks()
            .await
            .expect("list")
            .into_iter()
            .find(|t| t.id == "t1")
            .expect("t1 after");
        assert_eq!(
            baseline.name, after.name,
            "noop UpdateTask must not change name"
        );
    }

    // Now via the wire RPC: empty strings ≡ "leave unchanged".
    let updated = admin
        .update_scheduled_task(ScheduledTaskUpdate {
            id: "t1".into(),
            name: "renamed".into(),
            prompt: String::new(),
            schedule_type: String::new(),
            schedule_expr: String::new(),
            timezone: String::new(),
        })
        .await
        .expect("update wire")
        .into_inner();
    assert_eq!(updated.name, "renamed", "name must apply");
    assert_eq!(updated.prompt, "p", "empty prompt must NOT clobber");

    let err = admin
        .update_scheduled_task(ScheduledTaskUpdate {
            id: "missing".into(),
            name: "x".into(),
            prompt: String::new(),
            schedule_type: String::new(),
            schedule_expr: String::new(),
            timezone: String::new(),
        })
        .await
        .expect_err("missing id rejected");
    assert_eq!(err.code(), tonic::Code::NotFound);
}

#[tokio::test]
async fn api_key_admin_rpcs_round_trip_literal_keychain_and_unset() {
    // Covers all four api-key RPCs: a bug in any one of them (oneof
    // serialization, settings-table key naming drift, the
    // unset-then-list off-by-one) would surface here.
    let _g = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let h = start_harness().await;
    let chan = channel(&h.socket).await;
    let mut admin = AdminClient::new(chan);

    // 1. Literal: round-trip through Set → Get and assert the body variant
    //    AND value survive the wire. A regression in the oneof tag numbers
    //    would surface here as a wrong variant or empty string.
    admin
        .set_api_key(ApiKeyValue {
            provider: "openrouter".into(),
            body: Some(ApiKeyBody::Literal("sk-literal-test".into())),
        })
        .await
        .expect("set literal");
    let got = admin
        .get_api_key(ApiKeyRef {
            provider: "openrouter".into(),
        })
        .await
        .expect("get literal")
        .into_inner();
    assert_eq!(got.provider, "openrouter");
    match got.body {
        Some(ApiKeyBody::Literal(v)) => assert_eq!(v, "sk-literal-test"),
        other => panic!("expected literal body, got {other:?}"),
    }

    // 2. Keychain ref: set via Set, read back via Get; both fields must
    //    survive the round-trip. The settings-table key naming
    //    (`credentials.{provider}`) is implicit here — a drift would
    //    cause Get to read empty and return Unset.
    admin
        .set_api_key(ApiKeyValue {
            provider: "anthropic".into(),
            body: Some(ApiKeyBody::Keychain(KeychainRef {
                service: "borg-anthropic".into(),
                account: "borg".into(),
            })),
        })
        .await
        .expect("set keychain");
    let got = admin
        .get_api_key(ApiKeyRef {
            provider: "anthropic".into(),
        })
        .await
        .expect("get keychain")
        .into_inner();
    match got.body {
        Some(ApiKeyBody::Keychain(kr)) => {
            assert_eq!(kr.service, "borg-anthropic");
            assert_eq!(kr.account, "borg");
        }
        other => panic!("expected keychain body, got {other:?}"),
    }

    // 3. List must reflect both backends with their tags. Done before
    //    unset so we have a non-trivial list to assert on.
    let list = admin
        .list_api_keys(AdminEmpty {})
        .await
        .expect("list before unset")
        .into_inner();
    let by_provider: std::collections::HashMap<_, _> = list
        .entries
        .iter()
        .map(|e| (e.provider.clone(), e.backend.clone()))
        .collect();
    assert_eq!(
        by_provider.get("openrouter").map(String::as_str),
        Some("literal")
    );
    assert_eq!(
        by_provider.get("anthropic").map(String::as_str),
        Some("keychain")
    );

    // 4. Unset → Get must report the unset variant (NOT a literal with an
    //    empty string — that's the off-by-one we're guarding against).
    admin
        .unset_api_key(ApiKeyRef {
            provider: "openrouter".into(),
        })
        .await
        .expect("unset");
    let got = admin
        .get_api_key(ApiKeyRef {
            provider: "openrouter".into(),
        })
        .await
        .expect("get after unset")
        .into_inner();
    assert!(
        matches!(got.body, Some(ApiKeyBody::Unset(_))),
        "after unset, body must be Unset; got {:?}",
        got.body
    );

    // List after the unset must NOT contain the openrouter row anymore but
    // MUST still contain anthropic.
    let list = admin
        .list_api_keys(AdminEmpty {})
        .await
        .expect("list after unset")
        .into_inner();
    let providers: std::collections::HashSet<_> =
        list.entries.iter().map(|e| e.provider.clone()).collect();
    assert!(
        !providers.contains("openrouter"),
        "openrouter must drop from list after unset; got {providers:?}"
    );
    assert!(
        providers.contains("anthropic"),
        "anthropic must remain after openrouter unset; got {providers:?}"
    );

    // Defensive: empty provider is rejected at the boundary (so a
    // misrouted popup state can't silently overwrite an unrelated row).
    let err = admin
        .set_api_key(ApiKeyValue {
            provider: String::new(),
            body: Some(ApiKeyBody::Literal("x".into())),
        })
        .await
        .expect_err("empty provider rejected");
    assert_eq!(err.code(), tonic::Code::InvalidArgument);
}

#[tokio::test]
async fn status_rpcs_round_trip_evolution_bond_vitals_and_xp() {
    let _g = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let h = start_harness().await;
    let chan = channel(&h.socket).await;
    let mut status = StatusClient::new(chan.clone());
    let mut admin = AdminClient::new(chan);

    // Seed bond + vitals + evolution events through the canonical writers
    // so HMAC chains match what the readers will replay.
    {
        let db = Database::open().await.expect("db");
        db.record_bond_event_chained("tool_success", 1, "did a thing")
            .await
            .expect("bond 1");
        db.record_bond_event_chained("creation", 1, "made a thing")
            .await
            .expect("bond 2");
        db.record_vitals_event(
            "interaction",
            "session_start",
            // Must match the canonical delta for the "interaction" category;
            // record_vitals_event validates these to prevent gaming.
            &cortex::vitals::deltas_for(cortex::vitals::EventCategory::Interaction),
            None,
        )
        .await
        .expect("vitals 1");
        db.record_evolution_event("xp_gain", 2, Some("builder"), "tool:apply_patch", None)
            .await
            .expect("evo 1");
        db.record_evolution_event("xp_gain", 1, Some("builder"), "tool:read_file", None)
            .await
            .expect("evo 2");
    }

    // GetEvolution: must include the archetype_scores map (proto3 map<>
    // serialization is a real failure mode — a wire regression that drops
    // the map field would surface here as an empty map even though the
    // underlying state has scores).
    let evo = status
        .get_evolution(StatusEmpty {})
        .await
        .expect("evolution")
        .into_inner();
    assert!(
        evo.total_xp >= 3,
        "expected >=3 xp from two xp_gain events, got {}",
        evo.total_xp
    );
    // Replay rejects all events when the chain is broken; if the handler
    // forgot to fetch via the canonical reader the totals would be 0 here.
    assert!(
        evo.archetype_scores.values().any(|v| *v > 0.0),
        "archetype_scores map must round-trip non-zero values; got {:?}",
        evo.archetype_scores
    );
    assert_eq!(evo.stage, "base");

    // GetBondState — replayed score AND the rolling rate fields.
    // Real failure mode: handler returns BondState replay only and skips the
    // three rolling metrics, leaving the popup's correction/routine bars
    // permanently zero.
    let bond = status
        .get_bond_state(StatusEmpty {})
        .await
        .expect("bond")
        .into_inner();
    // Two +1 events from baseline 25 → 27.
    assert_eq!(
        bond.score as u32, 27,
        "bond replay score off; got {}",
        bond.score
    );
    assert_eq!(bond.level, "emerging");

    // GetBondEventsSince(0): must surface both seeded events. since=now+1h
    // must surface zero (off-by-one regression test for `since_unix` filter).
    let events = status
        .get_bond_events_since(Int64Cursor { since_unix: 0 })
        .await
        .expect("bond events")
        .into_inner();
    assert_eq!(events.events.len(), 2, "expected 2 bond events since 0");
    let kinds: Vec<_> = events.events.iter().map(|e| e.kind.clone()).collect();
    assert!(kinds.contains(&"tool_success".to_string()));
    assert!(kinds.contains(&"creation".to_string()));
    let future = chrono::Utc::now().timestamp() + 3600;
    let none = status
        .get_bond_events_since(Int64Cursor { since_unix: future })
        .await
        .expect("bond future")
        .into_inner();
    assert_eq!(none.events.len(), 0, "since>now must filter to empty");

    let evo_events = status
        .get_evolution_events_since(Int64Cursor { since_unix: 0 })
        .await
        .expect("evo events")
        .into_inner();
    assert_eq!(evo_events.events.len(), 2);
    for e in &evo_events.events {
        assert_eq!(e.kind, "xp_gain");
        assert!(e.delta > 0.0);
    }

    let v_events = status
        .get_vitals_events_since(Int64Cursor { since_unix: 0 })
        .await
        .expect("vitals events")
        .into_inner();
    assert_eq!(v_events.events.len(), 1);
    let v = &v_events.events[0];
    assert_eq!(v.category, "interaction");
    // Interaction category's canonical sync delta is +2; this asserts the
    // proto field translation didn't get crossed with another delta column.
    assert_eq!(v.sync_delta, 2);

    // GetXp: total_xp must match GetEvolution (single source of truth).
    let xp = status
        .get_xp(StatusEmpty {})
        .await
        .expect("xp")
        .into_inner();
    assert_eq!(xp.total_xp, evo.total_xp);
    assert_eq!(xp.level, evo.level);

    // GetAmbientStatus: at minimum the level must match evolution.level.
    // mood/archetype may legitimately vary based on bond+vitals state at
    // replay time, so we only pin the load-bearing field.
    let ambient = admin
        .get_ambient_status(AdminEmpty {})
        .await
        .expect("ambient")
        .into_inner();
    assert_eq!(ambient.level, evo.level);
    assert!(!ambient.mood.is_empty(), "mood must always render a value");
}

#[tokio::test]
async fn onboarding_state_round_trips_through_settings_table() {
    // Real failure modes guarded:
    // 1. Handler reads/writes a misspelled key (e.g. "onboarding.complete")
    //    while the cli's `is_onboarded_from_db` reads "onboarded" — would
    //    silently re-run the wizard on every launch.
    // 2. Handler stores the flag somewhere other than the `settings` table
    //    (or under a wrong category) — breaks the cross-process source of
    //    truth shared with main.rs's startup probe.
    let _g = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let h = start_harness().await;
    let chan = channel(&h.socket).await;
    let mut admin = AdminClient::new(chan);

    // Fresh DB → not onboarded.
    let state = admin
        .get_onboarding_state(AdminEmpty {})
        .await
        .expect("get state")
        .into_inner();
    assert!(
        !state.onboarded,
        "fresh DB must report onboarded=false; got {state:?}"
    );

    // Mark complete.
    admin
        .set_onboarding_complete(AdminEmpty {})
        .await
        .expect("set complete");

    // Round-trip via the same RPC.
    let state = admin
        .get_onboarding_state(AdminEmpty {})
        .await
        .expect("get state after set")
        .into_inner();
    assert!(
        state.onboarded,
        "after set_onboarding_complete, onboarded must be true; got {state:?}"
    );

    // Cross-check the actual settings-table key — guards against a refactor
    // that quietly renames the sentinel and breaks main.rs's probe path.
    {
        let db = Database::open().await.expect("db");
        assert_eq!(
            db.get_setting("onboarded").await.expect("get").as_deref(),
            Some("true"),
            "settings.onboarded must be persisted as the literal string \"true\""
        );
    }

    // Direct settings-table mutation (bypassing the registry-validated
    // resolver) must round-trip through the read RPC — guards against a
    // handler that caches the flag in-memory and ignores subsequent DB
    // state changes (e.g. a future "reset onboarding" maintenance task).
    {
        let db = Database::open().await.expect("db");
        db.set_setting("onboarded", "false")
            .await
            .expect("flip false");
    }
    let state = admin
        .get_onboarding_state(AdminEmpty {})
        .await
        .expect("get after flip")
        .into_inner();
    assert!(
        !state.onboarded,
        "RPC must read fresh DB state, not a cached `true`; got {state:?}"
    );
}

#[tokio::test]
async fn monthly_aggregates_distinguish_no_rows_from_zero_cost() {
    // Real failure modes guarded:
    //  - cost SQL aggregator returning 0.0 on no-rows instead of None →
    //    we'd render "Estimated cost: $0.0000" on a brand-new install
    //    that has never made an LLM call, which is dishonest.
    //  - token_total returning a wrong sum (e.g. summing only completion
    //    tokens, or wrong column) — verified against an explicit seed sum.
    //  - oneof field-tag drift on MonthlyTotalCost.present (proto3 bool
    //    field 2 silently dropping → cli would always treat as not-present).
    let _g = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let h = start_harness().await;
    let chan = channel(&h.socket).await;
    let mut admin = AdminClient::new(chan);

    // Empty DB: total = 0, cost = present:false (NOT 0.0 with present:true).
    let total = admin
        .get_monthly_token_total(AdminEmpty {})
        .await
        .expect("token total empty")
        .into_inner();
    assert_eq!(total.total, 0, "fresh DB monthly token total must be 0");

    let cost = admin
        .get_monthly_total_cost(AdminEmpty {})
        .await
        .expect("cost empty")
        .into_inner();
    assert!(
        !cost.present,
        "no rows must report present=false; got present=true cost={}",
        cost.cost
    );

    // Seed: two rows, totals 100 + 250 = 350; costs 0.10 + 0.25 = 0.35.
    {
        let db = Database::open().await.expect("db");
        db.log_token_usage(40, 60, 100, "openrouter", "model-a", Some(0.10))
            .await
            .expect("seed row 1");
        db.log_token_usage(50, 200, 250, "openrouter", "model-a", Some(0.25))
            .await
            .expect("seed row 2");
    }

    let total = admin
        .get_monthly_token_total(AdminEmpty {})
        .await
        .expect("token total seeded")
        .into_inner();
    assert_eq!(
        total.total, 350,
        "monthly token total must sum total_tokens across rows; got {}",
        total.total
    );

    let cost = admin
        .get_monthly_total_cost(AdminEmpty {})
        .await
        .expect("cost seeded")
        .into_inner();
    assert!(
        cost.present,
        "after seeding non-null cost rows, present must be true"
    );
    assert!(
        (cost.cost - 0.35).abs() < 1e-9,
        "cost must be 0.35; got {}",
        cost.cost
    );
}

#[tokio::test]
async fn daemon_status_reports_elapsed_uptime() {
    let _g = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let h = start_harness().await;
    let chan = channel(&h.socket).await;
    let mut admin = AdminClient::new(chan);

    // The service was constructed moments ago; after a real one-second wait
    // the reported uptime must have advanced past zero. Regression guard for
    // the hardcoded `uptime_secs: 0` this replaced.
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let status = admin
        .get_daemon_status(AdminEmpty {})
        .await
        .expect("daemon status")
        .into_inner();
    assert!(
        status.uptime_secs >= 1,
        "uptime must track elapsed wall-clock; got {}",
        status.uptime_secs
    );
    assert_eq!(status.version, env!("CARGO_PKG_VERSION"));
}

#[tokio::test]
async fn set_session_archived_hides_a_session_from_the_listing_and_restores_it() {
    let _g = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let h = start_harness().await;
    let chan = channel(&h.socket).await;
    let mut admin = AdminClient::new(chan);

    {
        let db = Database::open().await.expect("db");
        db.upsert_session("keep-me", 1, 20, 0, "m", "Keep")
            .await
            .expect("seed keep");
        db.upsert_session("close-me", 1, 10, 0, "m", "Close")
            .await
            .expect("seed close");
    }
    let ids = |list: protocols::admin::SessionList| {
        list.sessions.into_iter().map(|s| s.id).collect::<Vec<_>>()
    };
    let before = ids(admin
        .list_sessions(ListSessionsRequest::default())
        .await
        .expect("list")
        .into_inner());
    assert_eq!(before, vec!["keep-me".to_string(), "close-me".to_string()]);

    // Archiving is the sidebar's "close this conversation". The observable
    // contract is the listing the sidebar renders — a handler that stamped the
    // row but left it in ListSessions would look like a dead button.
    admin
        .set_session_archived(SessionArchiveMutation {
            id: "close-me".into(),
            archived: true,
        })
        .await
        .expect("archive");
    let after = ids(admin
        .list_sessions(ListSessionsRequest::default())
        .await
        .expect("list")
        .into_inner());
    assert_eq!(
        after,
        vec!["keep-me".to_string()],
        "an archived session must drop out of the listing, its neighbour must not"
    );

    // Soft hide, not delete: the row survives with a real timestamp so
    // `borg sessions list --archived` can still find it.
    {
        let db = Database::open().await.expect("db");
        let row = db
            .session_by_id("close-me")
            .await
            .expect("query")
            .expect("archived session must still exist");
        assert!(
            row.archived_at.unwrap_or(0) > 0,
            "archiving must stamp a real time, not delete the row"
        );
    }

    admin
        .set_session_archived(SessionArchiveMutation {
            id: "close-me".into(),
            archived: false,
        })
        .await
        .expect("unarchive");
    let restored = ids(admin
        .list_sessions(ListSessionsRequest::default())
        .await
        .expect("list")
        .into_inner());
    assert_eq!(restored, before, "restoring must bring the session back");

    let missed = admin
        .set_session_archived(SessionArchiveMutation {
            id: "no-such-session".into(),
            archived: true,
        })
        .await
        .expect_err("archiving an unknown session must fail");
    assert_eq!(
        missed.code(),
        tonic::Code::NotFound,
        "a typo'd id must report NotFound, not a silent success"
    );
}

#[tokio::test]
async fn list_sessions_surfaces_archived_rows_only_when_asked() {
    let _g = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let h = start_harness().await;
    let chan = channel(&h.socket).await;
    let mut admin = AdminClient::new(chan);

    {
        let db = Database::open().await.expect("db");
        db.upsert_session("active-one", 1, 20, 0, "m", "Active")
            .await
            .expect("seed active");
        db.upsert_session("put-away", 1, 10, 0, "m", "Put away")
            .await
            .expect("seed archived");
    }
    admin
        .set_session_archived(SessionArchiveMutation {
            id: "put-away".into(),
            archived: true,
        })
        .await
        .expect("archive");

    // The default listing is unchanged: this is what the CLI and the TUI ask
    // for, and archiving has to keep hiding from it.
    let default = admin
        .list_sessions(ListSessionsRequest::default())
        .await
        .expect("default list")
        .into_inner();
    assert!(
        !default.sessions.iter().any(|s| s.id == "put-away"),
        "an archived session must stay out of the default listing"
    );

    // Asking for them is what makes an unarchive possible at all: without the
    // row there is no id for a client to restore.
    let with_archived = admin
        .list_sessions(ListSessionsRequest {
            include_archived: true,
        })
        .await
        .expect("archived list")
        .into_inner();
    let archived_row = with_archived
        .sessions
        .iter()
        .find(|s| s.id == "put-away")
        .expect("archived session must be listed when asked for");
    assert!(
        archived_row.archived,
        "an archived row must say so, or a client cannot offer Restore instead of Archive"
    );
    assert!(
        with_archived
            .sessions
            .iter()
            .any(|s| s.id == "active-one" && !s.archived),
        "asking for archived rows must not flag the active ones"
    );
}

#[tokio::test]
async fn list_sessions_previews_the_last_thing_said() {
    let _g = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let h = start_harness().await;
    let chan = channel(&h.socket).await;
    let mut admin = AdminClient::new(chan);

    {
        let db = Database::open().await.expect("db");
        db.upsert_session("chatty", 1, 30, 0, "m", "Chatty")
            .await
            .expect("seed");
        db.insert_message(
            "chatty",
            "user",
            Some("deploy the thing"),
            None,
            None,
            None,
            None,
        )
        .await
        .expect("user row");
        db.insert_message(
            "chatty",
            "assistant",
            Some("Deployed.\n\nEverything is green."),
            None,
            None,
            None,
            None,
        )
        .await
        .expect("assistant row");
    }

    let list = admin
        .list_sessions(ListSessionsRequest::default())
        .await
        .expect("list")
        .into_inner();
    let row = list
        .sessions
        .iter()
        .find(|s| s.id == "chatty")
        .expect("seeded session must be listed");

    assert_eq!(
        row.last_message_preview, "Deployed. Everything is green.",
        "the preview must be one flat line, so a list row cannot grow to three"
    );
    assert_eq!(row.last_message_role, "assistant");
    assert_eq!(
        row.message_count, 2,
        "the batched count must still be right"
    );
}

/// Minimal backend for the live-session case: it never runs a turn, it only
/// records that the daemon finalized it.
struct CloseCountingBackend {
    id: String,
    close_calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl SessionBackend for CloseCountingBackend {
    async fn run_turn(
        &self,
        _text: String,
        _images: Vec<InlineImage>,
        _event_tx: tokio::sync::mpsc::Sender<cortex::agent::AgentEvent>,
        _cancel: tokio_util::sync::CancellationToken,
        _text_file_bridge: Option<Arc<dyn cortex::execution_context::TextFileBridge>>,
    ) {
    }

    fn session_id(&self) -> &str {
        &self.id
    }

    async fn close_session(&self) -> anyhow::Result<()> {
        self.close_calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[tokio::test]
async fn archiving_a_live_session_closes_its_host_and_a_missing_one_closes_nothing() {
    let _g = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let h = start_harness().await;
    let chan = channel(&h.socket).await;
    let mut admin = AdminClient::new(chan);

    {
        let db = Database::open().await.expect("db");
        db.upsert_session("live-one", 1, 10, 0, "m", "Live")
            .await
            .expect("seed live");
    }
    let close_calls = Arc::new(AtomicUsize::new(0));
    h.registry
        .insert(SessionHost::new(Arc::new(CloseCountingBackend {
            id: "live-one".into(),
            close_calls: Arc::clone(&close_calls),
        })));

    // ListSessions deliberately exempts live sessions from the archived
    // filter, so stamping the row alone would leave it on screen. Archiving
    // has to end the session too — this is the branch that makes the sidebar's
    // × actually remove the row.
    admin
        .set_session_archived(SessionArchiveMutation {
            id: "live-one".into(),
            archived: true,
        })
        .await
        .expect("archive live session");

    assert!(
        h.registry.peek("live-one").is_none(),
        "archiving a live session must remove its host from the registry"
    );
    assert_eq!(
        close_calls.load(Ordering::SeqCst),
        1,
        "the host must be finalized once, not dropped or closed twice"
    );
    let listed = admin
        .list_sessions(ListSessionsRequest::default())
        .await
        .expect("list")
        .into_inner()
        .sessions;
    assert!(
        listed.is_empty(),
        "an archived session must leave the listing even though it was live; got {:?}",
        listed.into_iter().map(|s| s.id).collect::<Vec<_>>()
    );

    // The unknown-id path must not have side effects: a request answered with
    // NotFound must not cancel a live turn on its way out.
    let other_calls = Arc::new(AtomicUsize::new(0));
    h.registry
        .insert(SessionHost::new(Arc::new(CloseCountingBackend {
            id: "bystander".into(),
            close_calls: Arc::clone(&other_calls),
        })));
    let missed = admin
        .set_session_archived(SessionArchiveMutation {
            id: "bystander".into(),
            archived: true,
        })
        .await
        .expect_err("a session with no DB row must not archive");
    assert_eq!(missed.code(), tonic::Code::NotFound);
    assert!(
        h.registry.peek("bystander").is_some(),
        "a rejected archive must leave the live session running"
    );
    assert_eq!(
        other_calls.load(Ordering::SeqCst),
        0,
        "a rejected archive must not have closed anything"
    );
}

/// A scheduled task whose expression the scheduler cannot parse must be
/// rejected outright.
///
/// Before this, `create_scheduled_task` stored whatever it was handed and left
/// `next_run` NULL when the caller passed none. The row landed as `active`, the
/// scheduler — which only claims rows with a due `next_run` — never picked it
/// up, and the task silently never ran. Nothing surfaced the mistake, so the
/// only symptom was a schedule that quietly did nothing forever.
#[tokio::test]
async fn creating_a_scheduled_task_resolves_its_first_run_and_rejects_a_bad_expression() {
    let _g = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let h = start_harness().await;
    let chan = channel(&h.socket).await;
    let mut admin = AdminClient::new(chan);

    let create = |name: &str, expr: &str| ScheduledTaskCreate {
        name: name.into(),
        prompt: "say hello".into(),
        schedule_type: "cron".into(),
        schedule_expr: expr.into(),
        timezone: "UTC".into(),
        task_type: "prompt".into(),
        delivery_channel: String::new(),
        delivery_target: String::new(),
        next_run_unix: 0,
        max_retries: -1,
        timeout_ms: 0,
        allowed_tools: String::new(),
        pre_run_command: String::new(),
        pre_run_cwd: String::new(),
        pre_run_timeout_ms: 0,
    };

    // Six fields, seconds first — the dialect the `cron` crate actually parses.
    let good = admin
        .create_scheduled_task(create("good", "0 0 8 * * *"))
        .await
        .expect("valid cron accepted")
        .into_inner();
    assert!(
        good.next_run_unix > chrono::Utc::now().timestamp(),
        "a created task must know when it next fires, or the scheduler skips it forever"
    );

    // The five-field crontab dialect is not what the parser accepts.
    let status = admin
        .create_scheduled_task(create("bad", "0 8 * * *"))
        .await
        .expect_err("unparseable cron must be refused");
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert!(
        status.message().contains("cron"),
        "the error must name the offending expression, got: {}",
        status.message()
    );

    let names = admin
        .list_scheduled_tasks(AdminEmpty {})
        .await
        .expect("list")
        .into_inner()
        .tasks
        .into_iter()
        .map(|t| t.name)
        .collect::<Vec<_>>();
    assert!(names.contains(&"good".to_string()));
    assert!(
        !names.contains(&"bad".to_string()),
        "a refused task must not be left behind in the table"
    );
}

/// A zone the scheduler cannot resolve must be refused on create and on
/// update. A stored unknown zone makes the schedule evaluate in UTC with no
/// message to the user, so the task fires at the wrong hour.
#[tokio::test]
async fn scheduled_task_rpcs_refuse_an_unknown_timezone_and_store_nothing() {
    let _g = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let h = start_harness().await;
    let chan = channel(&h.socket).await;
    let mut admin = AdminClient::new(chan);

    let create = |name: &str, timezone: &str| ScheduledTaskCreate {
        name: name.into(),
        prompt: "say hello".into(),
        schedule_type: "cron".into(),
        schedule_expr: "0 0 8 * * *".into(),
        timezone: timezone.into(),
        task_type: "prompt".into(),
        delivery_channel: String::new(),
        delivery_target: String::new(),
        next_run_unix: 0,
        max_retries: -1,
        timeout_ms: 0,
        allowed_tools: String::new(),
        pre_run_command: String::new(),
        pre_run_cwd: String::new(),
        pre_run_timeout_ms: 0,
    };
    let assert_refusal = |status: tonic::Status, bad: &str| {
        assert_eq!(status.code(), tonic::Code::InvalidArgument, "{bad}");
        assert!(
            status.message().contains(bad) && status.message().contains("America/New_York"),
            "the error must name the bad value and the accepted form, got: {}",
            status.message()
        );
    };

    // A numeric offset is the value an old CLI sent. It is not a zone name.
    for bad in ["+02:00", "Mars/Base"] {
        let status = admin
            .create_scheduled_task(create("refused", bad))
            .await
            .expect_err("an unknown zone must be refused");
        assert_refusal(status, bad);
    }

    let kept = admin
        .create_scheduled_task(create("kept", "Europe/Berlin"))
        .await
        .expect("an IANA zone is accepted")
        .into_inner();
    admin
        .create_scheduled_task(create("local-zone", "Local"))
        .await
        .expect("'Local' is accepted in any letter case");

    let status = admin
        .update_scheduled_task(ScheduledTaskUpdate {
            id: kept.id.clone(),
            name: "renamed".into(),
            prompt: String::new(),
            schedule_type: String::new(),
            schedule_expr: String::new(),
            timezone: "Mars/Base".into(),
        })
        .await
        .expect_err("an unknown zone must be refused on update");
    assert_refusal(status, "Mars/Base");

    let tasks = admin
        .list_scheduled_tasks(AdminEmpty {})
        .await
        .expect("list")
        .into_inner()
        .tasks;
    assert!(
        tasks.iter().all(|t| t.name != "refused"),
        "a refused task must not be left behind in the table"
    );
    let row = tasks
        .iter()
        .find(|t| t.id == kept.id)
        .expect("the accepted task is stored");
    assert_eq!(
        row.name, "kept",
        "a refused update must not apply its other fields"
    );
    assert_eq!(row.timezone, "Europe/Berlin");
}

/// Renaming is what the conversation list's Rename action calls.
///
/// `update_session_title` is a no-op for an unknown id, so the handler has to
/// check first — a rename that silently did nothing is indistinguishable from
/// one that worked, and the row would snap back on the next refetch.
#[tokio::test]
async fn setting_a_session_title_persists_and_rejects_blank_or_unknown() {
    let _g = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let h = start_harness().await;
    let chan = channel(&h.socket).await;
    let mut admin = AdminClient::new(chan);

    {
        let db = Database::open().await.expect("db");
        db.upsert_session("s1", 1, 10, 0, "m", "Original")
            .await
            .expect("seed");
    }

    admin
        .set_session_title(SessionTitleMutation {
            id: "s1".into(),
            title: "  Renamed  ".into(),
        })
        .await
        .expect("rename");
    let detail = admin
        .get_session(SessionRef { id: "s1".into() })
        .await
        .expect("get")
        .into_inner();
    assert_eq!(detail.title, "Renamed", "the stored title must be trimmed");

    let blank = admin
        .set_session_title(SessionTitleMutation {
            id: "s1".into(),
            title: "   ".into(),
        })
        .await
        .expect_err("a blank title must be refused");
    assert_eq!(blank.code(), tonic::Code::InvalidArgument);

    let missing = admin
        .set_session_title(SessionTitleMutation {
            id: "nope".into(),
            title: "Whatever".into(),
        })
        .await
        .expect_err("an unknown session must be refused");
    assert_eq!(missing.code(), tonic::Code::NotFound);

    let after = admin
        .get_session(SessionRef { id: "s1".into() })
        .await
        .expect("get")
        .into_inner();
    assert_eq!(
        after.title, "Renamed",
        "a refused rename must change nothing"
    );
}

/// Backend for the turn-activity case: `run_turn` parks until the test
/// releases it, so the host stays observably busy for the whole assertion.
struct BlockingBackend {
    id: String,
    release: Arc<tokio::sync::Notify>,
}

#[async_trait::async_trait]
impl SessionBackend for BlockingBackend {
    async fn run_turn(
        &self,
        _text: String,
        _images: Vec<InlineImage>,
        _event_tx: tokio::sync::mpsc::Sender<cortex::agent::AgentEvent>,
        _cancel: tokio_util::sync::CancellationToken,
        _text_file_bridge: Option<Arc<dyn cortex::execution_context::TextFileBridge>>,
    ) {
        self.release.notified().await;
    }

    fn session_id(&self) -> &str {
        &self.id
    }
}

/// A session list has to answer "which conversation is working right now"
/// two ways: as a snapshot on every row, and as a live feed. Both are asserted
/// here because a client trusts the feed only as far as the snapshot can
/// correct it — if the snapshot lied, a dropped stream would strand the dot.
#[tokio::test]
async fn turn_activity_is_reported_by_both_the_listing_and_the_feed() {
    let _g = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let h = start_harness().await;
    let chan = channel(&h.socket).await;
    let mut admin = AdminClient::new(chan);

    {
        let db = Database::open().await.expect("db");
        db.upsert_session("busy-one", 1, 10, 0, "m", "Working")
            .await
            .expect("seed session");
    }

    let mut feed = admin
        .subscribe_session_activity(AdminEmpty {})
        .await
        .expect("subscribe")
        .into_inner();

    let release = Arc::new(tokio::sync::Notify::new());
    let host = SessionHost::new(Arc::new(BlockingBackend {
        id: "busy-one".into(),
        release: Arc::clone(&release),
    }));
    h.registry.insert(host.clone());

    let turn_active_in_listing = |admin: &mut AdminClient<tonic::transport::Channel>| {
        let mut admin = admin.clone();
        async move {
            admin
                .list_sessions(ListSessionsRequest::default())
                .await
                .expect("list")
                .into_inner()
                .sessions
                .into_iter()
                .find(|s| s.id == "busy-one")
                .expect("the seeded session must be listed")
                .turn_active
        }
    };

    assert!(
        !turn_active_in_listing(&mut admin).await,
        "an idle session must not be reported as working"
    );

    host.start_turn("go".into(), Vec::new(), None, None).await;

    let started = tokio::time::timeout(Duration::from_secs(2), feed.message())
        .await
        .expect("a transition within 2s")
        .expect("stream open")
        .expect("an event");
    assert_eq!(started.session_id, "busy-one");
    assert!(started.turn_active, "the feed must announce the start");
    assert!(
        turn_active_in_listing(&mut admin).await,
        "the listing must agree with the feed while the turn runs"
    );

    release.notify_waiters();

    let settled = tokio::time::timeout(Duration::from_secs(2), feed.message())
        .await
        .expect("a transition within 2s")
        .expect("stream open")
        .expect("an event");
    assert!(!settled.turn_active, "the feed must announce the settle");
    assert!(
        !turn_active_in_listing(&mut admin).await,
        "a settled session must drop out of the working set"
    );
}

/// The first-run screen branches on `GetSetupStatus`. Each stage below is a
/// state a real install passes through; a wrong answer at any of them is a
/// wizard that asks for a key it already has, or skips one it lacks.
#[tokio::test]
async fn setup_status_reflects_provider_credentials_and_catalog() {
    let _g = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let h = start_harness().await;
    let chan = channel(&h.socket).await;
    let mut admin = AdminClient::new(chan);
    async fn status(admin: &mut AdminClient<Channel>) -> protocols::admin::SetupStatus {
        admin
            .get_setup_status(AdminEmpty {})
            .await
            .expect("get_setup_status")
            .into_inner()
    }

    // Fresh install: nothing chosen, but a full catalog to choose from.
    let fresh = status(&mut admin).await;
    assert!(!fresh.configured, "fresh DB must not report configured");
    assert_eq!(fresh.provider, "");
    let openrouter = fresh
        .providers
        .iter()
        .find(|p| p.id == "openrouter")
        .expect("openrouter in catalog");
    assert!(openrouter.recommended && openrouter.needs_api_key);
    assert_eq!(openrouter.key_url, "https://openrouter.ai/keys");
    assert_eq!(
        Some(openrouter.default_model.as_str()),
        cortex::llm::model_catalog::embedded_manifest()
            .default_model("openrouter")
            .as_deref()
    );
    let ollama = fresh
        .providers
        .iter()
        .find(|p| p.id == "ollama")
        .expect("ollama in catalog");
    assert!(!ollama.needs_api_key && ollama.key_url.is_empty());
    for p in &fresh.providers {
        assert!(!p.models.is_empty(), "{} offers no models", p.id);
    }

    // A key in ~/.borg/.env is found, named by file, never by value.
    std::fs::write(
        h._tmp.path().join(".env"),
        "ANTHROPIC_API_KEY=\"sk-ant-dotenv-secret\"\n",
    )
    .expect("write .env");
    let found = status(&mut admin).await;
    let dotenv = found
        .detected
        .iter()
        .find(|d| d.provider == "anthropic" && d.source == "dotenv")
        .expect("dotenv detection for anthropic");
    assert!(
        dotenv.detail.ends_with(".env"),
        "detail names the file: {dotenv:?}"
    );
    for d in &found.detected {
        assert!(!d.detail.contains("sk-ant"), "secret leaked: {d:?}");
    }
    assert!(!found.configured, "a found key is not a chosen provider");

    // Choosing that provider makes the install configured.
    admin
        .set_setting(SettingMutation {
            key: "provider".into(),
            value: "anthropic".into(),
        })
        .await
        .expect("set provider");
    let chosen = status(&mut admin).await;
    assert_eq!(chosen.provider, "anthropic");
    assert!(chosen.configured, "provider with a .env key is configured");

    // A key saved through the GUI counts too, and the resolver sees it.
    admin
        .set_api_key(ApiKeyValue {
            provider: "openai".into(),
            body: Some(ApiKeyBody::Literal("sk-openai-stored".into())),
        })
        .await
        .expect("set_api_key");
    admin
        .set_setting(SettingMutation {
            key: "provider".into(),
            value: "openai".into(),
        })
        .await
        .expect("set provider");
    let stored = status(&mut admin).await;
    assert!(
        stored
            .detected
            .iter()
            .any(|d| d.provider == "openai" && d.source == "stored"),
        "stored key not detected: {:?}",
        stored.detected
    );
    assert!(stored.configured);

    // A keyless provider needs its runtime, not a key lying around. The
    // provider goes first: a change of provider clears the endpoint of the
    // previous one.
    admin
        .set_setting(SettingMutation {
            key: "provider".into(),
            value: "ollama".into(),
        })
        .await
        .expect("set provider");
    admin
        .set_setting(SettingMutation {
            key: "llm.base_url".into(),
            value: "http://127.0.0.1:1".into(),
        })
        .await
        .expect("set base_url");
    let ollama = status(&mut admin).await;
    assert!(
        !ollama.configured,
        "ollama on a closed port must not be configured: {:?}",
        ollama.detected
    );
}
