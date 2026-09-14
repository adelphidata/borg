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
//! tests because `BORG_HOME` is per-process and `Database::open()` re-reads it.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::await_holding_lock)]

use borgd::grpc::admin::AdminSvc;
use borgd::session::SessionRegistry;
use cortex::db::Database;
use cortex::settings::SettingsResolver;
use protocols::admin::admin_server::Admin;
use protocols::admin::{
    AddCredentialRequest, ListCredentialsRequest, RemoveCredentialRequest, ResetCredentialsRequest,
    SetCredentialPriorityRequest, SetCredentialStrategyRequest,
};
use std::sync::Mutex;
use tonic::{Code, Request};

static SERIAL: Mutex<()> = Mutex::new(());

const TEST_ENV_VAR: &str = "OPENROUTER_API_KEY";

async fn fresh_admin_with_provider(seed_key: &str) -> (AdminSvc, tempfile::TempDir) {
    let tmp = tempfile::tempdir().expect("tempdir");
    // SAFETY: tests in this binary are serialized via SERIAL.
    unsafe {
        std::env::set_var("BORG_HOME", tmp.path());
        std::env::set_var(TEST_ENV_VAR, seed_key);
    }
    // Materialize the DB and pick the primary provider via settings so
    // `Config::resolve_api_keys()` works for the handlers' build_primary_pool.
    let db = Database::open().await.expect("seed DB open");
    let resolver = SettingsResolver::new(db);
    resolver
        .set("provider", "openrouter")
        .await
        .expect("set provider");
    (AdminSvc::new(SessionRegistry::new()), tmp)
}

fn teardown_env() {
    // SAFETY: serialized by SERIAL.
    unsafe {
        std::env::remove_var(TEST_ENV_VAR);
        std::env::remove_var("BORG_HOME");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_then_add_then_remove_round_trips() {
    let _g = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (admin, _tmp) = fresh_admin_with_provider("seed-env-key").await;

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
    // The suppression row is written by a fire-and-forget persist task, so it
    // is not necessarily visible on the very next rebuild. Poll rather than
    // assert once: this still fails if the row never lands, but it does not
    // depend on winning a race with the background writer.
    let mut statuses: Vec<String> = list_after
        .providers
        .first()
        .map(|p| p.entries.iter().map(|e| e.status.clone()).collect())
        .unwrap_or_default();
    let mut tomb = statuses.iter().filter(|s| *s == "removed").count();
    for _ in 0..600 {
        if tomb >= 1 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let again =
            Admin::list_credentials(&admin, Request::new(ListCredentialsRequest::default()))
                .await
                .expect("re-list")
                .into_inner();
        statuses = again
            .providers
            .first()
            .map(|p| p.entries.iter().map(|e| e.status.clone()).collect())
            .unwrap_or_default();
        tomb = statuses.iter().filter(|s| *s == "removed").count();
    }
    assert!(
        tomb >= 1,
        "tombstone must persist across pool rebuilds — statuses={statuses:?}"
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
    let (admin, _tmp) = fresh_admin_with_provider("seed-env-runtime-only").await;

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
    let (admin, _tmp) = fresh_admin_with_provider("seed-env-key-2").await;

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
    let (admin, _tmp) = fresh_admin_with_provider("seed-env-key-3").await;

    // Build a pool directly via the production builder and cool one entry,
    // then reset via RPC and verify the count.
    {
        let _db = Database::open().await.expect("db");
        let config = cortex::config::Config::load_from_db()
            .await
            .expect("config");
        // The pool builder now takes the whole `LlmConfig` rather than seven
        // positional knobs — see `try_build_from_keys`.
        let pool = cortex::llm::credential_pool::try_build_from_keys(
            cortex::llm::provider::Provider::OpenRouter,
            config.resolve_api_keys().expect("keys").1,
            &config.llm,
        )
        .await
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

    // `report_failure` persists the cooldown through a fire-and-forget task,
    // and `reset_credentials` rebuilds the pool from the DB — so wait for the
    // cooldown to be durably visible before resetting. The probe must be
    // read-only: polling with `reset_credentials` itself mutates state, and a
    // persist landing *after* an early reset re-introduces the cooldown, which
    // then breaks the idempotency assertion below.
    //
    // Budget is deliberately generous (~12s): under a full `cargo test
    // --workspace` many test binaries compete and the background persist can
    // be starved well past a second. It still fails rather than hangs.
    let mut saw_cooling = false;
    for _ in 0..600 {
        let listed =
            Admin::list_credentials(&admin, Request::new(ListCredentialsRequest::default()))
                .await
                .expect("list while waiting for cooldown to persist")
                .into_inner();
        if listed
            .providers
            .first()
            .is_some_and(|p| p.entries.iter().any(|e| e.status == "exhausted"))
        {
            saw_cooling = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(
        saw_cooling,
        "the RateLimit cooldown must become visible to a pool rebuild"
    );

    let cleared = Admin::reset_credentials(
        &admin,
        Request::new(ResetCredentialsRequest {
            provider: "openrouter".into(),
            target: String::new(),
        }),
    )
    .await
    .expect("reset")
    .into_inner()
    .cleared;
    assert_eq!(cleared, 1, "exactly one cooldown should be cleared");

    // Idempotent: second reset clears nothing.
    let resp2 = Admin::reset_credentials(
        &admin,
        Request::new(ResetCredentialsRequest {
            provider: "openrouter".into(),
            target: String::new(),
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
    let (admin, _tmp) = fresh_admin_with_provider("seed-env-key-4").await;

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
    let db = Database::open().await.expect("db");
    let resolver = SettingsResolver::new(db);
    let cfg = resolver.resolve().await.expect("resolve 1");
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
        let cfg = resolver.resolve().await.expect("resolve loop");
        assert_eq!(cfg.llm.credential_strategy.to_string(), *s);
    }

    teardown_env();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn add_rejects_empty_or_whitespace_key() {
    let _g = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (admin, _tmp) = fresh_admin_with_provider("seed-env-key-5").await;

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
    let (admin, _tmp) = fresh_admin_with_provider("seed-env-key-6").await;

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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn targeted_reset_and_priority_rpcs_validate_targets_and_positions() {
    let _g = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (admin, _tmp) = fresh_admin_with_provider("seed-env-key-7").await;

    // A targeted reset of an unknown entry must fail loudly and tell the
    // operator what *would* have resolved — never fall back to "reset all".
    let err = Admin::reset_credentials(
        &admin,
        Request::new(ResetCredentialsRequest {
            provider: "openrouter".into(),
            target: "no-such-key".into(),
        }),
    )
    .await
    .expect_err("unknown reset target must error");
    assert_eq!(err.code(), Code::InvalidArgument);
    assert!(
        err.message().contains("no-such-key") && err.message().contains("valid targets"),
        "error must name the bad target and the valid ones: {}",
        err.message()
    );

    // A targeted reset of a healthy entry resolves and clears nothing.
    let cleared = Admin::reset_credentials(
        &admin,
        Request::new(ResetCredentialsRequest {
            provider: "openrouter".into(),
            target: "1".into(),
        }),
    )
    .await
    .expect("targeted reset of a healthy entry")
    .into_inner()
    .cleared;
    assert_eq!(cleared, 0);

    // Priority: the pool has one live entry, so position 2 is out of range.
    let err = Admin::set_credential_priority(
        &admin,
        Request::new(SetCredentialPriorityRequest {
            provider: "openrouter".into(),
            target: "1".into(),
            position: 2,
        }),
    )
    .await
    .expect_err("position past the live count must error");
    assert_eq!(err.code(), Code::InvalidArgument);
    assert!(
        err.message().contains("between 1 and 1"),
        "error must state the valid range: {}",
        err.message()
    );

    let resp = Admin::set_credential_priority(
        &admin,
        Request::new(SetCredentialPriorityRequest {
            provider: "openrouter".into(),
            target: "1".into(),
            position: 1,
        }),
    )
    .await
    .expect("in-range move")
    .into_inner();
    assert_eq!(
        resp.strategy, "fill_first",
        "response carries the active strategy so the CLI can warn on round_robin"
    );
    assert_eq!(resp.entries.len(), 1);
    assert_eq!(resp.entries[0].priority, 0);

    teardown_env();
}

/// A key saved through `SetApiKey` (the only write the desktop and browser
/// onboarding make) must be the key the LLM resolver hands out. Before the
/// `credentials.{provider}` fold in `Config::load_from_db`, this returned
/// "no API key" and every GUI install was a dead end after setup.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gui_saved_literal_key_reaches_the_llm_resolver() {
    let _g = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let tmp = tempfile::tempdir().expect("tempdir");
    // SAFETY: serialized by SERIAL. Both env vars the resolver would consult
    // (the provider's own, and the compiled default `llm.api_key_env`) are
    // cleared so the stored row is the only place a key could come from.
    unsafe {
        std::env::set_var("BORG_HOME", tmp.path());
        std::env::remove_var("OPENAI_API_KEY");
        std::env::remove_var(TEST_ENV_VAR);
    }
    let db = Database::open().await.expect("seed DB open");
    SettingsResolver::new(db)
        .set("provider", "openai")
        .await
        .expect("set provider");
    let svc = AdminSvc::new(SessionRegistry::new());

    let before = cortex::config::Config::load_from_db()
        .await
        .expect("config")
        .resolve_provider();
    assert!(
        before.is_err(),
        "no key yet must be an error, got {before:?}"
    );

    svc.set_api_key(Request::new(protocols::admin::ApiKeyValue {
        provider: "openai".into(),
        body: Some(protocols::admin::api_key_value::Body::Literal(
            "sk-gui-literal".into(),
        )),
    }))
    .await
    .expect("set_api_key");

    let (provider, key) = cortex::config::Config::load_from_db()
        .await
        .expect("config")
        .resolve_provider()
        .expect("stored key resolves");
    assert_eq!(provider, cortex::llm::provider::Provider::OpenAi);
    assert_eq!(key, "sk-gui-literal");

    // And the pool the daemon actually sends with is built from it.
    let listed = svc
        .list_credentials(Request::new(ListCredentialsRequest {
            provider: "openai".into(),
        }))
        .await
        .expect("list")
        .into_inner();
    let pooled = listed.providers.iter().flat_map(|p| &p.entries).count();
    assert_eq!(pooled, 1, "one pooled key from the stored row: {listed:?}");

    teardown_env();
}
