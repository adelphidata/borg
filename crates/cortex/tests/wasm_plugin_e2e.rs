#![allow(clippy::await_holding_lock, clippy::expect_used, clippy::unwrap_used)]
//! End-to-end integration test for the WASM plugin lifecycle.
//!
//! Covers the seam where `plugins` install-gauntlet primitives meet cortex's
//! DB-backed registry and the agent's dispatch path:
//!
//!   install_from_dir → DB insert → tool_definitions() → try_execute_tool() → output
//!
//! Plus the tamper path: a single byte flip on the installed `plugin.wasm`
//! must trip the local HMAC re-check at dispatch time, quarantine the row,
//! and cause the tool to disappear from `tool_definitions()`.
//!
//! The unit tests in `crates/plugins/src/wasm.rs` cover the wasmtime ABI,
//! signature/HMAC primitives, and host imports against in-memory bytes.
//! Nothing exercises the DB-registered dispatch path until this test runs.

use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex};

use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use ed25519_dalek::{Signer, SigningKey};
use serde_json::json;
use wasm_encoder::{
    CodeSection, ConstExpr, DataSection, ExportKind, ExportSection, Function, FunctionSection,
    Instruction, MemorySection, MemoryType, Module, TypeSection, ValType,
};

use cortex::db::{audit_events, install_global_audit, AuditWriter, Database};
use cortex::wasm_plugins;
use plugins::wasm::{
    canonical_manifest_bytes, PublisherManifest, ResourceLimits, WasmPluginManifest,
    WasmToolManifest,
};

/// Tests in this binary share the process-global `BORG_DATA_DIR` env var
/// (which `Database::open` and `Config::data_dir` consult). Run them
/// serialized so each gets a clean tempdir without racing — same pattern
/// as `tests/maintenance_sweep.rs`.
static SERIAL: Mutex<()> = Mutex::new(());

fn isolated_data_dir() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().expect("tempdir");
    // SAFETY: env mutation under multi-threaded test runners can race FFI
    // `getenv`. SERIAL guarantees no other test in this binary reads or
    // writes BORG_DATA_DIR concurrently.
    unsafe {
        std::env::set_var("BORG_DATA_DIR", tmp.path());
    }
    tmp
}

/// Build a minimal WASM module that satisfies the plugin ABI: exports
/// `memory`, `alloc(i32) -> i32`, and `borg_tool_call(i32, i32) -> i64`.
/// The packed (ptr, len) return points at bytes "OK" staged at offset 2048.
///
/// Mirrors `crates/plugins/src/wasm.rs::tests::minimal_wasm`. Lifted into
/// this test file rather than exposing the helper publicly — the helper's
/// shape is test-coupled and not part of the plugins crate's API contract.
fn minimal_wasm() -> Vec<u8> {
    let mut module = Module::new();
    let mut types = TypeSection::new();
    types.ty().function([ValType::I32], [ValType::I32]);
    types
        .ty()
        .function([ValType::I32, ValType::I32], [ValType::I64]);
    module.section(&types);

    let mut funcs = FunctionSection::new();
    funcs.function(0);
    funcs.function(1);
    module.section(&funcs);

    let mut memories = MemorySection::new();
    memories.memory(MemoryType {
        minimum: 1,
        maximum: Some(1),
        memory64: false,
        shared: false,
        page_size_log2: None,
    });
    module.section(&memories);

    let mut exports = ExportSection::new();
    exports.export("memory", ExportKind::Memory, 0);
    exports.export("alloc", ExportKind::Func, 0);
    exports.export("borg_tool_call", ExportKind::Func, 1);
    module.section(&exports);

    let mut code = CodeSection::new();
    let mut alloc = Function::new([]);
    alloc.instruction(&Instruction::I32Const(1024));
    alloc.instruction(&Instruction::End);
    code.function(&alloc);
    let mut call = Function::new([]);
    // packed (ptr=2048, len=2): high 32 bits = ptr, low 32 bits = len
    call.instruction(&Instruction::I64Const(((2048u64) << 32 | 2u64) as i64));
    call.instruction(&Instruction::End);
    code.function(&call);
    module.section(&code);

    let mut data = DataSection::new();
    data.active(0, &ConstExpr::i32_const(2048), b"OK".iter().copied());
    module.section(&data);

    module.finish()
}

/// Write a signed three-file package (`manifest.json`, `manifest.sig`,
/// `plugin.wasm`) into `dir`, with tool name `echo_wasm` declared. Returns
/// the base64-encoded publisher public key so the caller can seed
/// `security.trusted_publisher_keys`.
fn write_signed_package(dir: &Path, wasm: &[u8], signing: &SigningKey) -> String {
    write_signed_package_with(dir, wasm, signing, "dev/echo", "echo_wasm")
}

/// Parametrised variant of `write_signed_package`. Distinct `plugin_id` /
/// `tool_name` lets a test stage multiple plugins side by side without
/// re-installing on top of a stale `dev/echo` row — used by the deprecated-key
/// and kill-switch scenarios where each phase needs its own publisher keypair
/// and DB row.
fn write_signed_package_with(
    dir: &Path,
    wasm: &[u8],
    signing: &SigningKey,
    plugin_id: &str,
    tool_name: &str,
) -> String {
    let pubkey_b64 = STANDARD.encode(signing.verifying_key().to_bytes());
    let wasm_sha = {
        use sha2::{Digest, Sha256};
        let digest = Sha256::digest(wasm);
        format!("{digest:x}")
    };
    let manifest = WasmPluginManifest {
        schema_version: "borg.plugin.v1".to_string(),
        id: plugin_id.to_string(),
        name: plugin_id.to_string(),
        version: "1.0.0".to_string(),
        publisher: PublisherManifest {
            name: "Dev".to_string(),
            public_key: pubkey_b64.clone(),
        },
        wasm_sha256: wasm_sha,
        tools: vec![WasmToolManifest {
            name: tool_name.to_string(),
            description: "Echo test tool".to_string(),
            input_schema: json!({"type": "object", "properties": {}}),
        }],
        capabilities: vec![],
        endpoints: vec![],
        credentials: vec![],
        limits: ResourceLimits::default(),
    };

    let canonical = canonical_manifest_bytes(&manifest).expect("canonical manifest");
    let signature = signing.sign(&canonical);
    let sig_b64 = STANDARD.encode(signature.to_bytes());

    fs::write(dir.join("manifest.json"), &canonical).expect("write manifest.json");
    fs::write(dir.join("manifest.sig"), &sig_b64).expect("write manifest.sig");
    fs::write(dir.join("plugin.wasm"), wasm).expect("write plugin.wasm");

    pubkey_b64
}

/// Trust `pubkey_b64` for plugin installs by writing
/// `security.trusted_publisher_keys` directly to the DB so the next
/// `Config::load_from_db()` reads the seeded value.
async fn seed_trusted_publisher(db: &Database, pubkey_b64: &str) {
    let value = serde_json::to_string(&vec![pubkey_b64]).expect("encode trusted-publisher list");
    db.set_setting("security.trusted_publisher_keys", &value)
        .await
        .expect("set trusted publisher keys");
}

/// Stage a signed package on disk and install it. Returns the package
/// directory (so callers can keep it alive) and the installed plugin id.
async fn install_echo_plugin(tmp_root: &Path) -> std::path::PathBuf {
    let db = Database::open().await.expect("open db");
    let signing = SigningKey::from_bytes(&[7u8; 32]);
    let pubkey_b64 = {
        let pkg_dir = tmp_root.join("pkg");
        fs::create_dir_all(&pkg_dir).expect("create package dir");
        let wasm = minimal_wasm();
        write_signed_package(&pkg_dir, &wasm, &signing)
    };
    seed_trusted_publisher(&db, &pubkey_b64).await;
    drop(db);

    let pkg_dir = tmp_root.join("pkg");
    let summary = wasm_plugins::install_from_dir(&pkg_dir)
        .await
        .expect("install_from_dir");
    assert_eq!(summary.plugin_id, "dev/echo");
    assert_eq!(summary.version, "1.0.0");
    assert_eq!(summary.tool_count, 1);
    pkg_dir
}

#[tokio::test]
async fn install_register_dispatch_uninstall_roundtrip() {
    let _g = SERIAL.lock().expect("serial lock");
    let tmp = isolated_data_dir();

    let _pkg_dir = install_echo_plugin(tmp.path()).await;

    // DB registration must surface the tool to the LLM.
    let defs = wasm_plugins::tool_definitions().await;
    assert!(
        defs.iter().any(|d| d.function.name == "echo_wasm"),
        "echo_wasm must surface in tool_definitions after install; got: {:?}",
        defs.iter().map(|d| &d.function.name).collect::<Vec<_>>()
    );

    // The installed package files must exist under data_dir/wasm_plugins/.
    let db = Database::open().await.expect("open db");
    let row = db
        .get_wasm_plugin("dev/echo")
        .await
        .expect("get_wasm_plugin")
        .expect("plugin row");
    assert!(
        Path::new(&row.manifest_path).exists(),
        "manifest.json on disk"
    );
    assert!(Path::new(&row.wasm_path).exists(), "plugin.wasm on disk");
    assert_eq!(row.status, "verified");
    drop(db);

    // End-to-end dispatch: the guest's "OK" bytes must round-trip back through
    // DB lookup, HMAC re-verify, wasmtime execution, and packed-(ptr,len) decode.
    let out = wasm_plugins::try_execute_tool("echo_wasm", &json!({}))
        .await
        .expect("try_execute_tool ok");
    assert_eq!(
        out.as_deref(),
        Some("OK"),
        "guest's staged bytes must round-trip through DB-resolved dispatch"
    );

    // Uninstall: DB row gone, install dir gone, dispatch returns Ok(None).
    let outcome = wasm_plugins::remove("dev/echo").await.expect("remove ok");
    assert!(outcome.existed, "remove must report the row existed");
    assert!(
        outcome.orphaned_dir.is_none(),
        "clean uninstall must not leave on-disk dir behind, got: {:?}",
        outcome.orphaned_dir
    );

    let db = Database::open().await.expect("open db");
    assert!(
        db.get_wasm_plugin("dev/echo")
            .await
            .expect("get_wasm_plugin")
            .is_none(),
        "DB row must be gone after remove"
    );
    drop(db);

    assert!(
        !Path::new(&row.wasm_path).exists(),
        "on-disk plugin.wasm must be deleted after remove"
    );
    assert!(
        !Path::new(&row.manifest_path).exists(),
        "on-disk manifest.json must be deleted after remove"
    );

    let defs_after = wasm_plugins::tool_definitions();
    assert!(
        defs_after
            .await
            .iter()
            .all(|d| d.function.name != "echo_wasm"),
        "echo_wasm must not appear in tool_definitions after remove"
    );

    let after = wasm_plugins::try_execute_tool("echo_wasm", &json!({}))
        .await
        .expect("dispatch after remove must be Ok");
    assert!(
        after.is_none(),
        "after remove, try_execute_tool must return Ok(None), not error — \
         the agent loop relies on this to fall through to other tools"
    );
}

#[tokio::test]
async fn tampered_wasm_quarantines_on_dispatch() {
    let _g = SERIAL.lock().expect("serial lock");
    let tmp = isolated_data_dir();

    let _pkg_dir = install_echo_plugin(tmp.path()).await;

    // Tamper: flip one byte in the installed plugin.wasm. This must trip the
    // HMAC re-check at dispatch time — if it doesn't, the seal isn't being
    // enforced and an attacker who can write to ~/.borg/wasm_plugins/ could
    // swap in arbitrary code.
    let db = Database::open().await.expect("open db");
    let row = db
        .get_verified_wasm_tool_runtime("echo_wasm")
        .await
        .expect("runtime row")
        .expect("echo_wasm runtime row exists pre-tamper");
    let wasm_path = row.wasm_path;
    drop(db);

    let mut bytes = fs::read(&wasm_path).expect("read installed wasm");
    let last = bytes.len() - 1;
    bytes[last] ^= 0x01;
    fs::write(&wasm_path, &bytes).expect("write tampered wasm");

    // Dispatch must error. We deliberately do NOT assert on the error string —
    // wording is not the contract; the structural side effect is. The
    // post-conditions below (status="quarantined", non-empty reason, tool
    // dropped from tool_definitions) are what an attacker who flips one byte
    // on disk must trigger. A regression where dispatch returns Ok would
    // bypass them all.
    let _ = wasm_plugins::try_execute_tool("echo_wasm", &json!({}))
        .await
        .expect_err("tampered wasm must fail dispatch");

    // Quarantined row must drop out of tool_definitions so the LLM stops
    // being offered a tool that can no longer execute.
    let defs = wasm_plugins::tool_definitions().await;
    assert!(
        defs.iter().all(|d| d.function.name != "echo_wasm"),
        "quarantined plugin must not appear in tool_definitions"
    );

    // And the row's status field must reflect the quarantine — used by
    // `borg plugin show` / `borg plugin verify` to surface the reason.
    let db = Database::open().await.expect("open db");
    let row = db
        .get_wasm_plugin("dev/echo")
        .await
        .expect("get_wasm_plugin")
        .expect("plugin row still present (quarantined, not deleted)");
    assert_eq!(row.status, "quarantined");
    assert!(
        row.quarantine_reason
            .as_deref()
            .map(|r| !r.is_empty())
            .unwrap_or(false),
        "quarantine_reason must be populated, got: {:?}",
        row.quarantine_reason
    );
}

/// Combined coverage for three doc invariants that all rely on the
/// process-global audit writer:
///
/// 1. Pre-sunset deprecated publisher key: install succeeds, the
///    `WASM_PLUGIN_INSTALLED` audit row carries `deprecated_key=true` and a
///    `sunset_at` timestamp so the operator can see the warning landed.
/// 2. Post-sunset deprecated publisher key: install is rejected hard, no
///    plugin row is persisted.
/// 3. A seal that does not verify under the current scheme quarantines the
///    plugin on execute, refuses to run the guest, and records a
///    `WASM_PLUGIN_QUARANTINED` audit row. There is no second sealing scheme
///    to fall back to, so an unverifiable seal is always tamper.
///
/// All three phases live in one `#[test]` because `install_global_audit`
/// installs a process-singleton writer (OnceLock). Splitting them into
/// separate test functions would leave the writer pointing at a dropped
/// tempdir, and `try_emit_global` would silently write to an orphaned DB
/// the next test cannot inspect. One test, one tempdir, one writer.
#[tokio::test]
async fn deprecated_key_and_seal_quarantine_e2e() {
    let _g = SERIAL.lock().expect("serial lock");
    let tmp = isolated_data_dir();

    // Install the process-global audit writer pointing at the DB rooted at
    // this test's BORG_DATA_DIR. `install_global_audit` is `OnceLock::set` —
    // if a sibling test in this binary already installed one we cannot run
    // the audit-row assertions, so flag the run order explicitly.
    let audit_db = Arc::new(tokio::sync::Mutex::new(
        Database::open().await.expect("audit writer db"),
    ));
    assert!(
        install_global_audit(AuditWriter::new(audit_db)),
        "audit writer was already installed by another test in this binary — \
         run `cargo test -p cortex --test wasm_plugin_e2e -- deprecated_key_and_auto_reseal_e2e`",
    );

    // ── Phase 1: pre-sunset deprecated key — install succeeds with audit flag ──
    //
    // Invariant: a deprecated-but-not-yet-sunset publisher key installs the
    // plugin (operators need the window to rotate) AND surfaces the deprecation
    // in the audit chain so it isn't silent. A regression that dropped
    // `deprecated_key` from the audit payload would make the migration window
    // invisible to the operator.
    let pre_signing = SigningKey::from_bytes(&[11u8; 32]);
    let pre_pkg = tmp.path().join("pkg-pre");
    fs::create_dir_all(&pre_pkg).expect("mkdir pre pkg");
    let pre_pubkey = write_signed_package_with(
        &pre_pkg,
        &minimal_wasm(),
        &pre_signing,
        "dev/pre",
        "echo_pre",
    );
    {
        let db = Database::open().await.expect("open db");
        // trusted publisher list contains only the pre-sunset key for this phase
        db.set_setting(
            "security.trusted_publisher_keys",
            &serde_json::to_string(&vec![&pre_pubkey]).expect("encode trusted"),
        )
        .await
        .expect("set trusted");
        let future = (chrono::Utc::now() + chrono::Duration::hours(24)).timestamp();
        let dep_list = serde_json::json!([{
            "key": pre_pubkey,
            "sunset_at": future,
            "reason": "rotating to new signing key",
        }])
        .to_string();
        db.set_setting("security.deprecated_publisher_keys", &dep_list)
            .await
            .expect("set deprecated");
    }
    let summary = wasm_plugins::install_from_dir(&pre_pkg)
        .await
        .expect("pre-sunset install must succeed");
    assert_eq!(summary.plugin_id, "dev/pre");

    let db = Database::open().await.expect("open db");
    let events = db.list_audit_events().await.expect("list audit events");
    let pre_row = events
        .iter()
        .find(|e| {
            e.event_type == audit_events::WASM_PLUGIN_INSTALLED
                && e.principal_id == "system"
                && e.event_json.contains("\"dev/pre\"")
        })
        .expect("WASM_PLUGIN_INSTALLED audit row for dev/pre exists");
    let pre_payload: serde_json::Value =
        serde_json::from_str(&pre_row.event_json).expect("payload");
    assert_eq!(pre_payload["plugin_id"], "dev/pre");
    assert_eq!(
        pre_payload["deprecated_key"],
        serde_json::Value::Bool(true),
        "pre-sunset install must flag deprecated_key=true so operator sees the warning landed; \
         payload was {pre_payload}",
    );
    assert!(
        pre_payload["sunset_at"].is_string(),
        "sunset_at must be a string timestamp in the audit payload; got {pre_payload}",
    );

    // ── Phase 2: post-sunset deprecated key — install rejected, no row ──
    //
    // Invariant: once `sunset_at` is in the past, install must fail hard. A
    // regression that downgraded the rejection to a warning would silently
    // accept code signed by a key the operator already retired.
    let post_signing = SigningKey::from_bytes(&[13u8; 32]);
    let post_pkg = tmp.path().join("pkg-post");
    fs::create_dir_all(&post_pkg).expect("mkdir post pkg");
    let post_pubkey = write_signed_package_with(
        &post_pkg,
        &minimal_wasm(),
        &post_signing,
        "dev/post",
        "echo_post",
    );
    db.set_setting(
        "security.trusted_publisher_keys",
        &serde_json::to_string(&vec![&post_pubkey]).expect("encode trusted"),
    )
    .await
    .expect("set trusted post");
    let past = (chrono::Utc::now() - chrono::Duration::hours(24)).timestamp();
    let dep_list = serde_json::json!([{
        "key": post_pubkey,
        "sunset_at": past,
        "reason": "key retired",
    }])
    .to_string();
    db.set_setting("security.deprecated_publisher_keys", &dep_list)
        .await
        .expect("set deprecated post");
    drop(db);

    let err = wasm_plugins::install_from_dir(&post_pkg)
        .await
        .expect_err("post-sunset install must reject");
    let err_msg = format!("{err:#}");
    assert!(
        err_msg.contains("sunset"),
        "rejection must mention sunset so the operator knows why; got: {err_msg}",
    );
    let db = Database::open().await.expect("open db");
    assert!(
        db.get_wasm_plugin("dev/post")
            .await
            .expect("get post")
            .is_none(),
        "post-sunset rejection must not leave a plugin row behind",
    );
    db.delete_setting("security.deprecated_publisher_keys")
        .await
        .expect("clear deprecated");
    drop(db);

    // ── Phase 3: an unverifiable seal quarantines instead of running ──
    //
    // Invariant: the local HMAC is the post-install tamper signal. A stored
    // seal that does not verify under the current key must stop the guest
    // from executing, flip the row to quarantined, and leave an audit trail.
    // Silently repairing the seal here would turn the tamper detector off.
    let _pkg_dir = install_echo_plugin(tmp.path()).await;
    let db = Database::open().await.expect("open db");
    let row = db
        .get_wasm_plugin("dev/echo")
        .await
        .expect("get echo")
        .expect("echo row present after install");
    let manifest_bytes = fs::read(&row.manifest_path).expect("read installed manifest");
    let wasm_bytes = fs::read(&row.wasm_path).expect("read installed wasm");
    // Overwrite the stored seal with one computed under a different key —
    // indistinguishable from an attacker who edited the row.
    let wrong_key = db.derive_hmac_key(b"not-the-real-hmac-domain");
    let wrong_seal =
        plugins::wasm::compute_local_hmac_current(&wrong_key, &manifest_bytes, &wasm_bytes)
            .expect("compute wrong-key seal");
    assert_ne!(
        wrong_seal, row.local_hmac,
        "wrong-key seal must differ from the real one, or this test proves nothing",
    );
    db.update_wasm_plugin_local_hmac("dev/echo", &wrong_seal)
        .await
        .expect("install wrong seal");
    drop(db);

    let err = wasm_plugins::try_execute_tool("echo_wasm", &json!({}))
        .await
        .expect_err("a bad seal must refuse to execute");
    assert!(
        err.to_string().contains("quarantined"),
        "error must name the quarantine, got: {err}",
    );

    let db = Database::open().await.expect("open db");
    let row_after = db
        .get_wasm_plugin("dev/echo")
        .await
        .expect("get echo after quarantine")
        .expect("echo row still present");
    assert_eq!(
        row_after.status, "quarantined",
        "row must be quarantined, got status {}",
        row_after.status,
    );

    let events = db
        .list_audit_events()
        .await
        .expect("list audit after quarantine");
    let row = events
        .iter()
        .find(|e| {
            e.event_type == audit_events::WASM_PLUGIN_QUARANTINED && e.principal_id == "dev/echo"
        })
        .expect("WASM_PLUGIN_QUARANTINED audit row for dev/echo exists");
    let payload: serde_json::Value =
        serde_json::from_str(&row.event_json).expect("quarantine payload");
    assert_eq!(
        payload["reason_category"], "integrity_failure",
        "quarantine must be categorized as an integrity failure; got {payload}",
    );
}

/// Three operator kill switches must each (a) hide the plugin from
/// `tool_definitions()` so the LLM stops seeing it, and (b) refuse dispatch
/// through `try_execute_tool` so a stale tool-call in flight can't sneak by.
/// Listing-time-only filtering or dispatch-time-only filtering would each
/// leave a real-world escape: a cached LLM context with the stale tool, or a
/// hot-reload window where listing rebuilt before dispatch did.
///
/// The three knobs:
/// - `plugins.wasm.enabled = false` — global kill (dispatch returns `Ok(None)`
///   because the whole subsystem is off, indistinguishable from "not a wasm tool")
/// - `plugins.wasm.disabled_plugin_ids = [id]` — per-plugin kill (`Err`)
/// - `plugins.wasm.disabled_publisher_keys = [pubkey]` — per-publisher kill (`Err`)
///
/// Table-driven so a future fourth knob slots in as one row, not a fourth
/// near-duplicate test function.
#[tokio::test]
async fn kill_switch_hides_and_refuses_dispatch() {
    let _g = SERIAL.lock().expect("serial lock");
    let tmp = isolated_data_dir();

    let _pkg_dir = install_echo_plugin(tmp.path()).await;
    let pubkey = {
        let db = Database::open().await.expect("open db");
        db.get_wasm_plugin("dev/echo")
            .await
            .expect("get echo")
            .expect("echo row present")
            .publisher_key
    };

    // Baseline: tool is visible AND dispatch succeeds. If THIS fails, the test
    // setup is broken and the kill-switch assertions below would falsely pass.
    assert!(
        wasm_plugins::tool_definitions()
            .await
            .iter()
            .any(|d| d.function.name == "echo_wasm"),
        "baseline: echo_wasm must be visible before any kill switch is engaged",
    );
    let baseline = wasm_plugins::try_execute_tool("echo_wasm", &json!({}))
        .await
        .expect("baseline execute Ok");
    assert_eq!(
        baseline.as_deref(),
        Some("OK"),
        "baseline: dispatch must succeed before any kill switch is engaged",
    );

    struct Case<'a> {
        name: &'a str,
        setting_key: &'a str,
        setting_value: String,
        // For the global kill switch the whole subsystem is off, so dispatch
        // returns Ok(None) (the agent loop treats it as "not a wasm tool").
        // For per-plugin / per-publisher kills the subsystem is live, so
        // dispatch returns Err (the agent loop surfaces the refusal).
        dispatch_returns_err: bool,
    }

    let cases = [
        Case {
            name: "plugins.wasm.enabled = false",
            setting_key: "plugins.wasm.enabled",
            setting_value: "false".to_string(),
            dispatch_returns_err: false,
        },
        Case {
            name: "plugins.wasm.disabled_plugin_ids = [dev/echo]",
            setting_key: "plugins.wasm.disabled_plugin_ids",
            setting_value: serde_json::to_string(&vec!["dev/echo"]).expect("encode"),
            dispatch_returns_err: true,
        },
        Case {
            name: "plugins.wasm.disabled_publisher_keys = [pubkey]",
            setting_key: "plugins.wasm.disabled_publisher_keys",
            setting_value: serde_json::to_string(&vec![&pubkey]).expect("encode"),
            dispatch_returns_err: true,
        },
    ];

    for case in &cases {
        {
            let db = Database::open().await.expect("open db");
            db.set_setting(case.setting_key, &case.setting_value)
                .await
                .unwrap_or_else(|e| panic!("set {} for {}: {e}", case.setting_key, case.name));
        }

        let defs = wasm_plugins::tool_definitions().await;
        assert!(
            defs.iter().all(|d| d.function.name != "echo_wasm"),
            "{}: echo_wasm must be hidden from tool_definitions; got: {:?}",
            case.name,
            defs.iter().map(|d| &d.function.name).collect::<Vec<_>>(),
        );

        match wasm_plugins::try_execute_tool("echo_wasm", &json!({})).await {
            Ok(out) if case.dispatch_returns_err => panic!(
                "{}: dispatch must return Err to refuse the in-flight call; got Ok({:?})",
                case.name, out,
            ),
            Err(e) if !case.dispatch_returns_err => panic!(
                "{}: dispatch must return Ok(None) when subsystem is globally off; got Err: {e:#}",
                case.name,
            ),
            Ok(Some(out)) => panic!(
                "{}: globally-disabled subsystem must return Ok(None), not Ok(Some({:?}))",
                case.name, out,
            ),
            Ok(None) => {
                // Expected for the global-disable case.
            }
            Err(e) => {
                // Expected for per-plugin / per-publisher kill switches. The
                // refusal must name the disabled state so the LLM (and ops
                // tooling reading the error) can disambiguate from a generic
                // runtime failure.
                let msg = format!("{e:#}");
                assert!(
                    msg.contains("disabled"),
                    "{}: refusal must name the kill-switch state; got: {msg}",
                    case.name,
                );
            }
        }

        // Restore the setting before the next case so each case starts clean.
        {
            let db = Database::open().await.expect("open db");
            db.delete_setting(case.setting_key)
                .await
                .unwrap_or_else(|e| panic!("clear {} after {}: {e}", case.setting_key, case.name));
        }

        // Sanity: after clearing the knob the plugin reappears. If this fails
        // for any case, a kill-switch path is persisting state it shouldn't —
        // a bug worth its own test.
        assert!(
            wasm_plugins::tool_definitions()
                .await
                .iter()
                .any(|d| d.function.name == "echo_wasm"),
            "{}: plugin must reappear after clearing the kill switch",
            case.name,
        );
    }
}
