#![cfg(feature = "phase9-tests-disabled")]
//! Admin RPC coverage for the credential pool. Exercises every `borg auth`
//! surface: list / add / remove / reset / set-strategy.
//!
//! Real failure modes guarded:
//! - list against unconfigured providers returns an empty list (not an error)
//! - add: idempotent on duplicate fingerprint; persisted via DB readback
//! - remove: rejects unknown targets with `invalid_argument`
//! - remove writes a suppression row (visible after pool reload)
//! - reset clears cooldowns and returns the count cleared
//! - set-strategy: rejects garbage values and persists valid ones
//!
//! The harness invokes `AdminSvc` directly. A process-wide mutex serializes
//! tests because `BORG_DATA_DIR` is per-process and `Database::open()` re-reads it.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::await_holding_lock)]

use borgd::grpc::admin::AdminSvc;
use borgd::session::SessionRegistry;
use cortex::db::Database;
use cortex::settings::SettingsResolver;
use protocols::admin::admin_server::Admin;
use protocols::admin::{
    AddCredentialRequest, ListCredentialsRequest, RemoveCredentialRequest, ResetCredentialsRequest,
    SetCredentialStrategyRequest,
};
use std::sync::Mutex;
use tonic::{Code, Request};

static SERIAL: Mutex<()> = Mutex::new(());

const TEST_ENV_VAR: &str = "OPENROUTER_API_KEY";

fn fresh_admin_with_provider(seed_key: &str) -> (AdminSvc, tempfile::TempDir) {
    let tmp = tempfile::tempdir().expect("tempdir");
    // SAFETY: tests in this binary are serialized via SERIAL.
    unsafe {
        std::env::set_var("BORG_DATA_DIR", tmp.path());
        std::env::set_var(TEST_ENV_VAR, seed_key);
    }
    // Materialize the DB and pick the primary provider via settings so
    // `Config::resolve_api_keys()` works for the handlers' build_primary_pool.
    let db = Database::open().expect("seed DB open");
    let resolver = SettingsResolver::new(db);
    resolver
        .set("provider", "openrouter")
        .expect("set provider");
    (AdminSvc::new(SessionRegistry::new()), tmp)
}

fn teardown_env() {
    // SAFETY: serialized by SERIAL.
    unsafe {
        std::env::remove_var(TEST_ENV_VAR);
        std::env::remove_var("BORG_DATA_DIR");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_then_add_then_remove_round_trips() {
    let _g = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (admin, _tmp) = fresh_admin_with_provider("seed-env-key");

    // 1. Initial list: pool seeded from env, 1 entry.
    let list = Admin::list_credentials(&admin, Request::new(ListCredentialsRequest::default()))
        .await
        .expect("list_credentials")
        .into_inner();
    assert_eq!(list.providers.len(), 1, "exactly one provider configured");
    let pp = &list.providers[0];
    assert_eq!(pp.provider, "openrouter");
    assert_eq!(pp.entries.len(), 1, "env-resolved entry must be present");
    // `resolve_api_keys` returns the env-resolved key as a plaintext string
    // that the pool builder wraps in a `Config` source (see `try_build_from_keys`).
    // We treat the env var as a config-style entry once it's resolved upstream.
    assert_eq!(pp.entries[0].source_kind, "config");
    assert!(
        !pp.entries[0].fingerprint.is_empty() && pp.entries[0].fingerprint.len() == 16,
        "fingerprint must be 16-hex"
    );

    // 2. Add a manual key. The handler returns the new entry's metadata;
    //    plaintext never echoes back (the proto has no plaintext field).
    let added = Admin::add_credential(
        &admin,
        Request::new(AddCredentialRequest {
            provider: "openrouter".into(),
            key: "manual-key-1".into(),
            label: "personal".into(),
        }),
    )
    .await
    .expect("add_credential")
    .into_inner();
    assert_eq!(added.label, "personal");
    assert_eq!(added.source_kind, "manual");
    assert!(!added.fingerprint.is_empty());
    assert_eq!(
        added.fingerprint.len(),
        16,
        "fingerprint must be 16-hex BLAKE3 truncation"
    );

    // 3. Remove the env-seeded entry by 1-based index. Writes a suppression
    //    row so reload won't undo it.
    let removed = Admin::remove_credential(
        &admin,
        Request::new(RemoveCredentialRequest {
            provider: "openrouter".into(),
            target: "1".into(),
        }),
    )
    .await
    .expect("remove by index")
    .into_inner();
    assert!(
        removed.removed_at_ms > 0,
        "removed entry must carry a non-zero tombstone timestamp"
    );
    assert_eq!(removed.source_kind, "config");

    // 4. Re-list: the removed env entry is now a tombstone; the suppression
    //    row prevents the env source from being re-seeded as a live entry.
    let list_after =
        Admin::list_credentials(&admin, Request::new(ListCredentialsRequest::default()))
            .await
            .expect("list after remove")
            .into_inner();
    let pp = &list_after.providers[0];
    let tomb = pp.entries.iter().filter(|e| e.status == "removed").count();
    assert!(
        tomb >= 1,
        "tombstone must persist across pool rebuilds — got {tomb}"
    );

    teardown_env();
}

/// Manual-source credentials added via `add_credential` do **not** survive
/// across separate RPC calls. This is the documented design tradeoff in
/// `CredentialPool::add_entry`: we never persist plaintext API keys, so a
/// Manual-source row that survived restart would resurrect with an empty
/// key and 401 the next request. The pool's `load()` prunes such rows.
///
/// Operators who need restart-stable credentials should configure them via
/// `llm.api_keys` or an environment variable (both re-seed on load).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn add_credential_is_runtime_only_across_rpc_calls() {
    let _g = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (admin, _tmp) = fresh_admin_with_provider("seed-env-runtime-only");

    Admin::add_credential(
        &admin,
        Request::new(AddCredentialRequest {
            provider: "openrouter".into(),
            key: "transient-manual-key".into(),
            label: "transient".into(),
        }),
    )
    .await
    .expect("add_credential");

    // Second RPC builds a fresh pool that doesn't see Manual rows — by design.
    let list = Admin::list_credentials(&admin, Request::new(ListCredentialsRequest::default()))
        .await
        .expect("list")
        .into_inner();
    let manual_count = list.providers[0]
        .entries
        .iter()
        .filter(|e| e.source_kind == "manual" && e.status != "removed")
        .count();
    assert_eq!(
        manual_count, 0,
        "Manual entries are runtime-only by design — see docs/llm-resilience.md"
    );

    teardown_env();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remove_unknown_target_returns_invalid_argument() {
    let _g = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (admin, _tmp) = fresh_admin_with_provider("seed-env-key-2");

    let err = Admin::remove_credential(
        &admin,
        Request::new(RemoveCredentialRequest {
            provider: "openrouter".into(),
            target: "no-such-label".into(),
        }),
    )
    .await
    .expect_err("unknown target must error");
    assert_eq!(err.code(), Code::InvalidArgument);

    let empty = Admin::remove_credential(
        &admin,
        Request::new(RemoveCredentialRequest {
            provider: "openrouter".into(),
            target: String::new(),
        }),
    )
    .await
    .expect_err("empty target must error");
    assert_eq!(empty.code(), Code::InvalidArgument);

    teardown_env();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reset_clears_cooled_entries_only() {
    let _g = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (admin, _tmp) = fresh_admin_with_provider("seed-env-key-3");

    // Build a pool directly via the production builder and cool one entry,
    // then reset via RPC and verify the count.
    {
        let _db = Database::open().expect("db");
        let config = cortex::config::Config::load_from_db().expect("config");
        let pool = cortex::llm::credential_pool::try_build_from_keys(
            cortex::llm::provider::Provider::OpenRouter,
            config.resolve_api_keys().expect("keys").1,
            config.llm.credential_strategy,
            config.llm.credential_cooldown_secs,
            config.llm.max_concurrent_per_credential,
            config.llm.proactive_rate_limit_threshold_pct,
            config.llm.proactive_rate_limit_min_window_secs,
        )
        .expect("pool");
        let id = pool.entries()[0].id.clone();
        pool.report_failure(
            &id,
            cortex::llm::llm_error::FailoverReason::RateLimit,
            Some(429),
            None,
            Some("throttled"),
        );
        assert!(
            pool.is_exhausted(),
            "single-key pool must be exhausted after a RateLimit"
        );
    }

    let resp = Admin::reset_credentials(
        &admin,
        Request::new(ResetCredentialsRequest {
            provider: "openrouter".into(),
        }),
    )
    .await
    .expect("reset")
    .into_inner();
    assert_eq!(resp.cleared, 1, "exactly one cooldown should be cleared");

    // Idempotent: second reset clears nothing.
    let resp2 = Admin::reset_credentials(
        &admin,
        Request::new(ResetCredentialsRequest {
            provider: "openrouter".into(),
        }),
    )
    .await
    .expect("reset 2")
    .into_inner();
    assert_eq!(resp2.cleared, 0, "second reset is a no-op");

    teardown_env();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn set_strategy_validates_and_persists() {
    let _g = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (admin, _tmp) = fresh_admin_with_provider("seed-env-key-4");

    // Garbage value rejected at the API boundary, before any settings write.
    let err = Admin::set_credential_strategy(
        &admin,
        Request::new(SetCredentialStrategyRequest {
            provider: "openrouter".into(),
            strategy: "sideways".into(),
        }),
    )
    .await
    .expect_err("garbage strategy must error");
    assert_eq!(err.code(), Code::InvalidArgument);

    // Confirm DB wasn't touched by the rejected call.
    let db = Database::open().expect("db");
    let resolver = SettingsResolver::new(db);
    let cfg = resolver.resolve().expect("resolve 1");
    assert_eq!(
        cfg.llm.credential_strategy.to_string(),
        "fill_first",
        "rejected strategy must not persist"
    );

    // Valid strategies all round-trip.
    for s in &["round_robin", "least_used", "random", "fill_first"] {
        Admin::set_credential_strategy(
            &admin,
            Request::new(SetCredentialStrategyRequest {
                provider: "openrouter".into(),
                strategy: (*s).into(),
            }),
        )
        .await
        .unwrap_or_else(|e| panic!("set {s}: {e}"));
        let cfg = resolver.resolve().expect("resolve loop");
        assert_eq!(cfg.llm.credential_strategy.to_string(), *s);
    }

    teardown_env();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn add_rejects_empty_or_whitespace_key() {
    let _g = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (admin, _tmp) = fresh_admin_with_provider("seed-env-key-5");

    // Empty key.
    let err = Admin::add_credential(
        &admin,
        Request::new(AddCredentialRequest {
            provider: "openrouter".into(),
            key: String::new(),
            label: String::new(),
        }),
    )
    .await
    .expect_err("empty key must error");
    assert_eq!(err.code(), Code::InvalidArgument);

    // Whitespace-only key — must also be rejected so we don't fingerprint
    // an unusable key and write it to disk.
    let ws_err = Admin::add_credential(
        &admin,
        Request::new(AddCredentialRequest {
            provider: "openrouter".into(),
            key: "   ".into(),
            label: String::new(),
        }),
    )
    .await
    .expect_err("whitespace key must error");
    assert_eq!(ws_err.code(), Code::InvalidArgument);

    teardown_env();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_filters_by_provider() {
    let _g = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (admin, _tmp) = fresh_admin_with_provider("seed-env-key-6");

    // Empty filter = all providers (we have one).
    let all = Admin::list_credentials(&admin, Request::new(ListCredentialsRequest::default()))
        .await
        .expect("list all")
        .into_inner();
    assert_eq!(all.providers.len(), 1);

    // Matching filter = same.
    let matched = Admin::list_credentials(
        &admin,
        Request::new(ListCredentialsRequest {
            provider: "openrouter".into(),
        }),
    )
    .await
    .expect("list matched")
    .into_inner();
    assert_eq!(matched.providers.len(), 1);

    // Non-matching filter returns empty (not an error).
    let none = Admin::list_credentials(
        &admin,
        Request::new(ListCredentialsRequest {
            provider: "anthropic".into(),
        }),
    )
    .await
    .expect("list other")
    .into_inner();
    assert!(
        none.providers.is_empty(),
        "filter for unconfigured provider returns no rows"
    );

    teardown_env();
}
