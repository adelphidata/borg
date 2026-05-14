#![allow(clippy::expect_used, clippy::unwrap_used)]
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
use std::sync::Mutex;

use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use ed25519_dalek::{Signer, SigningKey};
use serde_json::json;
use wasm_encoder::{
    CodeSection, ConstExpr, DataSection, ExportKind, ExportSection, Function, FunctionSection,
    Instruction, MemorySection, MemoryType, Module, TypeSection, ValType,
};

use cortex::db::Database;
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
    let pubkey_b64 = STANDARD.encode(signing.verifying_key().to_bytes());
    let wasm_sha = {
        use sha2::{Digest, Sha256};
        let digest = Sha256::digest(wasm);
        format!("{digest:x}")
    };
    let manifest = WasmPluginManifest {
        schema_version: "borg.plugin.v1".to_string(),
        id: "dev/echo".to_string(),
        name: "Echo".to_string(),
        version: "1.0.0".to_string(),
        publisher: PublisherManifest {
            name: "Dev".to_string(),
            public_key: pubkey_b64.clone(),
        },
        wasm_sha256: wasm_sha,
        tools: vec![WasmToolManifest {
            name: "echo_wasm".to_string(),
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
fn seed_trusted_publisher(db: &Database, pubkey_b64: &str) {
    let value = serde_json::to_string(&vec![pubkey_b64]).expect("encode trusted-publisher list");
    db.set_setting("security.trusted_publisher_keys", &value)
        .expect("set trusted publisher keys");
}

/// Stage a signed package on disk and install it. Returns the package
/// directory (so callers can keep it alive) and the installed plugin id.
fn install_echo_plugin(tmp_root: &Path) -> std::path::PathBuf {
    let db = Database::open().expect("open db");
    let signing = SigningKey::from_bytes(&[7u8; 32]);
    let pubkey_b64 = {
        let pkg_dir = tmp_root.join("pkg");
        fs::create_dir_all(&pkg_dir).expect("create package dir");
        let wasm = minimal_wasm();
        write_signed_package(&pkg_dir, &wasm, &signing)
    };
    seed_trusted_publisher(&db, &pubkey_b64);
    drop(db);

    let pkg_dir = tmp_root.join("pkg");
    let summary = wasm_plugins::install_from_dir(&pkg_dir).expect("install_from_dir");
    assert_eq!(summary.plugin_id, "dev/echo");
    assert_eq!(summary.version, "1.0.0");
    assert_eq!(summary.tool_count, 1);
    pkg_dir
}

#[test]
fn install_register_dispatch_uninstall_roundtrip() {
    let _g = SERIAL.lock().expect("serial lock");
    let tmp = isolated_data_dir();

    let _pkg_dir = install_echo_plugin(tmp.path());

    // DB registration must surface the tool to the LLM.
    let defs = wasm_plugins::tool_definitions();
    assert!(
        defs.iter().any(|d| d.function.name == "echo_wasm"),
        "echo_wasm must surface in tool_definitions after install; got: {:?}",
        defs.iter().map(|d| &d.function.name).collect::<Vec<_>>()
    );

    // The installed package files must exist under data_dir/wasm_plugins/.
    let db = Database::open().expect("open db");
    let row = db
        .get_wasm_plugin("dev/echo")
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
    let out = wasm_plugins::try_execute_tool("echo_wasm", &json!({})).expect("try_execute_tool ok");
    assert_eq!(
        out.as_deref(),
        Some("OK"),
        "guest's staged bytes must round-trip through DB-resolved dispatch"
    );

    // Uninstall: DB row gone, install dir gone, dispatch returns Ok(None).
    let outcome = wasm_plugins::remove("dev/echo").expect("remove ok");
    assert!(outcome.existed, "remove must report the row existed");
    assert!(
        outcome.orphaned_dir.is_none(),
        "clean uninstall must not leave on-disk dir behind, got: {:?}",
        outcome.orphaned_dir
    );

    let db = Database::open().expect("open db");
    assert!(
        db.get_wasm_plugin("dev/echo")
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
        defs_after.iter().all(|d| d.function.name != "echo_wasm"),
        "echo_wasm must not appear in tool_definitions after remove"
    );

    let after = wasm_plugins::try_execute_tool("echo_wasm", &json!({}))
        .expect("dispatch after remove must be Ok");
    assert!(
        after.is_none(),
        "after remove, try_execute_tool must return Ok(None), not error — \
         the agent loop relies on this to fall through to other tools"
    );
}

#[test]
fn tampered_wasm_quarantines_on_dispatch() {
    let _g = SERIAL.lock().expect("serial lock");
    let tmp = isolated_data_dir();

    let _pkg_dir = install_echo_plugin(tmp.path());

    // Tamper: flip one byte in the installed plugin.wasm. This must trip the
    // HMAC re-check at dispatch time — if it doesn't, the seal isn't being
    // enforced and an attacker who can write to ~/.borg/wasm_plugins/ could
    // swap in arbitrary code.
    let db = Database::open().expect("open db");
    let row = db
        .get_verified_wasm_tool_runtime("echo_wasm")
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
        .expect_err("tampered wasm must fail dispatch");

    // Quarantined row must drop out of tool_definitions so the LLM stops
    // being offered a tool that can no longer execute.
    let defs = wasm_plugins::tool_definitions();
    assert!(
        defs.iter().all(|d| d.function.name != "echo_wasm"),
        "quarantined plugin must not appear in tool_definitions"
    );

    // And the row's status field must reflect the quarantine — used by
    // `borg plugin show` / `borg plugin verify` to surface the reason.
    let db = Database::open().expect("open db");
    let row = db
        .get_wasm_plugin("dev/echo")
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
