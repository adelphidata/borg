//! End-to-end exercise of the public WASM plugin pipeline:
//! `verify_package` → `install_verified_package` → re-read from disk →
//! `verify_installed_hmac` → `execute_tool`. Both happy-path and tamper-path
//! coverage; the inline unit tests in `src/wasm.rs` exercise each stage in
//! isolation but never the full integrated flow through the public surface.
//!
//! Helpers (`minimal_wasm`, `manifest_for`) intentionally duplicate the
//! crate-private versions in `src/wasm.rs` — integration tests cannot access
//! `#[cfg(test)] mod tests` items, and copying ~50 lines is cheaper than
//! widening the public surface for tests.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use anyhow::Result;
use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use ed25519_dalek::{Signer, SigningKey};
use plugins::wasm::{
    canonical_manifest_bytes, execute_tool, install_verified_package, sha256_hex,
    verify_installed_hmac, verify_package, PublisherManifest, ResourceLimits, WasmPluginManifest,
    WasmToolManifest,
};
use wasm_encoder::{
    CodeSection, ConstExpr, DataSection, ExportKind, ExportSection, Function, FunctionSection,
    Instruction, MemorySection, MemoryType, Module, TypeSection, ValType,
};

const SCHEMA_VERSION: &str = "borg.plugin.v1";
const HMAC_KEY: &[u8] = b"e2e-integration-test-hmac-key-do-not-reuse";

/// Build a minimal WASM module: `alloc` returns 1024, `borg_tool_call`
/// returns `(2048<<32) | 2` and the data section stages `"OK"` at offset
/// 2048. Mirrors `src/wasm.rs:minimal_wasm` so `execute_tool` returns "OK".
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
    call.instruction(&Instruction::I64Const(((2048u64) << 32 | 2u64) as i64));
    call.instruction(&Instruction::End);
    code.function(&call);
    module.section(&code);

    let mut data = DataSection::new();
    data.active(0, &ConstExpr::i32_const(2048), b"OK".iter().copied());
    module.section(&data);

    module.finish()
}

/// Build a manifest paired with `wasm` and signed by `signing_key`.
/// Returns (manifest, signed_canonical_manifest_bytes, signature_b64).
fn signed_manifest(wasm: &[u8], signing_key: &SigningKey) -> (WasmPluginManifest, Vec<u8>, String) {
    let manifest = WasmPluginManifest {
        schema_version: SCHEMA_VERSION.to_string(),
        id: "dev/e2e-echo".to_string(),
        name: "E2E Echo".to_string(),
        version: "1.0.0".to_string(),
        publisher: PublisherManifest {
            name: "E2E".to_string(),
            public_key: STANDARD.encode(signing_key.verifying_key().to_bytes()),
        },
        wasm_sha256: sha256_hex(wasm),
        tools: vec![WasmToolManifest {
            name: "echo_wasm".to_string(),
            description: "Echo integration test".to_string(),
            input_schema: serde_json::json!({"type": "object", "properties": {}}),
        }],
        capabilities: vec![],
        endpoints: vec![],
        credentials: vec![],
        limits: ResourceLimits::default(),
    };
    let canonical = canonical_manifest_bytes(&manifest).expect("canonical manifest");
    let sig = STANDARD.encode(signing_key.sign(&canonical).to_bytes());
    (manifest, canonical, sig)
}

/// Positive: a signed package verifies, installs to disk, re-verifies under
/// the same HMAC key after disk round-trip, and the guest's "OK" output
/// surfaces through `execute_tool`. Failure here means an end-to-end
/// regression in the public pipeline.
#[test]
fn verify_install_execute_roundtrip() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let signing = SigningKey::from_bytes(&[7u8; 32]);
    let trusted = vec![STANDARD.encode(signing.verifying_key().to_bytes())];

    let wasm = minimal_wasm();
    let (_manifest, canonical, sig) = signed_manifest(&wasm, &signing);

    let package = verify_package(&canonical, &sig, &wasm, &trusted, HMAC_KEY)?;
    let installed = install_verified_package(tmp.path(), &package)?;

    // Round-trip through the filesystem so we catch any "works in memory,
    // breaks on disk" gap (encoding, permissions, truncation).
    let on_disk_manifest = std::fs::read(&installed.manifest_path)?;
    let on_disk_wasm = std::fs::read(&installed.wasm_path)?;
    verify_installed_hmac(
        HMAC_KEY,
        &on_disk_manifest,
        &on_disk_wasm,
        &installed.local_hmac,
    )?;

    let manifest: WasmPluginManifest = serde_json::from_slice(&on_disk_manifest)?;
    let output = execute_tool(
        &manifest,
        &on_disk_wasm,
        "echo_wasm",
        &serde_json::json!({}),
    )?;
    assert_eq!(
        output, "OK",
        "round-trip must surface the guest's staged bytes"
    );
    Ok(())
}

/// Negative: flipping a single byte on disk after install must invalidate
/// the HMAC seal. This is the on-disk tamper invariant that drives the
/// quarantine path in cortex.
#[test]
fn installed_wasm_byte_flip_fails_hmac() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let signing = SigningKey::from_bytes(&[11u8; 32]);
    let trusted = vec![STANDARD.encode(signing.verifying_key().to_bytes())];

    let wasm = minimal_wasm();
    let (_manifest, canonical, sig) = signed_manifest(&wasm, &signing);

    let package = verify_package(&canonical, &sig, &wasm, &trusted, HMAC_KEY)?;
    let installed = install_verified_package(tmp.path(), &package)?;

    // Flip the last byte of plugin.wasm on disk. Any byte inside the module
    // would do, but the trailing byte is guaranteed not to be in a header
    // that wasm-parse would reject outright (we want HMAC failure, not a
    // parser short-circuit).
    let mut tampered = std::fs::read(&installed.wasm_path)?;
    let last = tampered.len() - 1;
    tampered[last] ^= 0xFF;
    std::fs::write(&installed.wasm_path, &tampered)?;

    let on_disk_manifest = std::fs::read(&installed.manifest_path)?;
    let on_disk_wasm = std::fs::read(&installed.wasm_path)?;
    let err = verify_installed_hmac(
        HMAC_KEY,
        &on_disk_manifest,
        &on_disk_wasm,
        &installed.local_hmac,
    )
    .expect_err("tampered wasm must fail HMAC verification");
    assert!(
        err.to_string().contains("integrity check failed"),
        "operator must see an integrity error, not a generic mismatch: {err}",
    );
    Ok(())
}

/// Negative: executing a tool name not in the manifest must bail with a
/// specific error, not a generic wasmtime trap. Guards the dispatch check
/// at the top of `execute_tool_prepared`.
#[test]
fn execute_unknown_tool_name_bails() -> Result<()> {
    let signing = SigningKey::from_bytes(&[13u8; 32]);
    let wasm = minimal_wasm();
    let (manifest, _canonical, _sig) = signed_manifest(&wasm, &signing);

    let err = execute_tool(&manifest, &wasm, "not_in_manifest", &serde_json::json!({}))
        .expect_err("unknown tool must be refused");
    assert!(
        err.to_string()
            .contains("not_in_manifest is not declared by this plugin"),
        "error must name the missing tool: {err}",
    );
    Ok(())
}

/// Negative: a foreign publisher key (not in the operator-trusted list)
/// must be rejected even when the signature is otherwise valid. Guards the
/// "trust boundary" of the install gauntlet.
#[test]
fn verify_package_rejects_untrusted_publisher() -> Result<()> {
    let signing = SigningKey::from_bytes(&[17u8; 32]);
    // Operator's trusted set lists a DIFFERENT key.
    let other = SigningKey::from_bytes(&[18u8; 32]);
    let trusted = vec![STANDARD.encode(other.verifying_key().to_bytes())];

    let wasm = minimal_wasm();
    let (_manifest, canonical, sig) = signed_manifest(&wasm, &signing);

    let err = verify_package(&canonical, &sig, &wasm, &trusted, HMAC_KEY)
        .expect_err("untrusted publisher must be refused");
    let msg = err.to_string().to_lowercase();
    assert!(
        msg.contains("publisher") || msg.contains("trust") || msg.contains("signature"),
        "error must point at the trust/signature failure: {err}",
    );
    Ok(())
}
