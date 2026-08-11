//! Capability-sandboxed WebAssembly plugin support.
//!
//! This module intentionally keeps the host ABI small. A plugin package is
//! accepted only after publisher-signature verification, static WASM import
//! validation, and local HMAC sealing. Runtime execution exposes no WASI, no
//! raw sockets, no subprocesses, and no filesystem access.

mod execute;
mod host_abi;
mod limits;
mod validate;
mod verify;

pub use execute::{
    build_engine, classify_execute_error, compile_module, execute_tool, execute_tool_prepared,
    execute_tool_with_services, WasmEngine, WasmExecuteErrorKind, WasmModule,
};
pub use validate::{validate_host_http_request, validate_manifest, validate_wasm_module};
pub use verify::{
    canonical_manifest_bytes, compute_local_hmac_current, install_verified_package, sha256_hex,
    verify_installed_hmac, verify_package, InstalledWasmPackage, VerifiedWasmPackage,
};

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

const SCHEMA_VERSION: &str = "borg.plugin.v1";
const ABI_EXPORT_ALLOC: &str = "alloc";
const ABI_EXPORT_CALL: &str = "borg_tool_call";
const ABI_EXPORT_MEMORY: &str = "memory";
// Host-import module names. Shared by the static-validation allowlist
// (`validate.rs`) and the runtime linker (`execute.rs`) — if those two drift,
// a module that passed capability validation can bind a host function it never
// declared, so they must read from one source.
const ABI_IMPORT_LOG: &str = "borg:log";
const ABI_IMPORT_HTTP: &str = "borg:http";
const ABI_IMPORT_STATE: &str = "borg:state";
const ABI_IMPORT_CREDENTIALS: &str = "borg:credentials";
// Sandbox resource caps. `*_DEFAULT` apply when a manifest omits a limit;
// `*_LIMIT` are the hard ceilings a manifest can request. WASM pages are
// 64 KiB, so 256 pages = 16 MiB max guest heap. Fuel bounds total executed
// instructions (DoS guard); the 60s timeout is the wall-clock backstop for
// fuel-heavy-but-not-fuel-exhausting loops.
const MAX_OUTPUT_BYTES_DEFAULT: u64 = 64 * 1024;
const MAX_MEMORY_PAGES_DEFAULT: u64 = 16;
// 10M fuel is order-of-100ms of guest work — comfortably more than any
// request/response-shaped tool needs, while an unbounded loop trips it almost
// immediately. The 5x ceiling gives compute-heavy plugins headroom to opt into
// without letting a manifest ask for an effectively unbounded budget.
const FUEL_DEFAULT: u64 = 10_000_000;
const MAX_FUEL_LIMIT: u64 = 50_000_000;
const MAX_TIMEOUT_MS_LIMIT: u64 = 60_000;
const MAX_MEMORY_PAGES_LIMIT: u64 = 256;
const MAX_OUTPUT_BYTES_LIMIT: u64 = 1_048_576;
const MAX_HOST_IMPORT_BYTES: usize = 64 * 1024;
/// Cap on `borg:log.log` lines per call. A noisy plugin could otherwise fill
/// the agent's tracing pipeline without ever doing useful work; the cap is per
/// invocation, generous enough for debug output, tight enough to bound waste.
const MAX_LOG_LINES_PER_CALL: u32 = 256;

const CAP_LOG: &str = "log";
const CAP_HTTP: &str = "http";
const CAP_STATE: &str = "state";
const CAP_CREDENTIALS: &str = "credentials";

const SECRETS_DIR_MODE: u32 = 0o700;

/// Manifest for a signed WASM plugin package.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct WasmPluginManifest {
    /// Manifest schema version. Must be `borg.plugin.v1`.
    pub schema_version: String,
    /// Stable plugin id. May contain `/` for catalog-style grouping.
    pub id: String,
    /// Human-readable plugin name.
    pub name: String,
    /// Plugin version string.
    pub version: String,
    /// Publisher identity and public key.
    pub publisher: PublisherManifest,
    /// Lowercase hex SHA-256 of the WASM module bytes.
    pub wasm_sha256: String,
    /// Tools exported by this plugin.
    pub tools: Vec<WasmToolManifest>,
    /// Declared host capabilities.
    #[serde(default)]
    pub capabilities: Vec<String>,
    /// HTTPS endpoint allowlist used by host-boundary network checks.
    #[serde(default)]
    pub endpoints: Vec<String>,
    /// Credential keys this plugin may request from the host.
    #[serde(default)]
    pub credentials: Vec<String>,
    /// Resource limits applied during runtime execution.
    #[serde(default)]
    pub limits: ResourceLimits,
}

/// Publisher identity embedded in a manifest.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PublisherManifest {
    /// Display name.
    pub name: String,
    /// Base64-encoded Ed25519 verifying key.
    pub public_key: String,
}

/// One callable tool exposed by a WASM plugin.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct WasmToolManifest {
    /// Tool name emitted by the LLM.
    pub name: String,
    /// Tool description sent to the LLM.
    pub description: String,
    /// JSON schema for tool input.
    pub input_schema: serde_json::Value,
}

/// Runtime limits for a WASM plugin.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ResourceLimits {
    /// Execution timeout in milliseconds.
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
    /// Wasmtime fuel budget.
    #[serde(default = "default_fuel")]
    pub fuel: u64,
    /// Maximum initial/declared memory pages.
    #[serde(default = "default_max_memory_pages")]
    pub max_memory_pages: u64,
    /// Maximum bytes read back from a plugin call.
    #[serde(default = "default_max_output_bytes")]
    pub max_output_bytes: u64,
}

impl Default for ResourceLimits {
    fn default() -> Self {
        Self {
            timeout_ms: default_timeout_ms(),
            fuel: default_fuel(),
            max_memory_pages: default_max_memory_pages(),
            max_output_bytes: default_max_output_bytes(),
        }
    }
}

/// Result of static WASM validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaticValidation {
    /// Imports observed in the module as `(module, name)`.
    pub imports: Vec<(String, String)>,
    /// Exports observed in the module.
    pub exports: Vec<String>,
}

/// Host-boundary request checked before a plugin-originated HTTP call.
///
/// HTTP method (`GET`, `POST`, etc.) lives on [`HostHttpFetch`] rather than
/// here — the boundary validator only cares about URL/headers/body, while the
/// fetcher needs the method too.
#[derive(Debug, Clone)]
pub struct HostHttpRequest<'a> {
    /// Request URL.
    pub url: &'a str,
    /// Header key/value pairs.
    pub headers: &'a [(&'a str, &'a str)],
    /// Request body bytes.
    pub body: &'a [u8],
}

/// Full HTTP request passed to [`HostServices::http_fetch`] after the boundary
/// validator has approved a [`HostHttpRequest`].
#[derive(Debug, Clone)]
pub struct HostHttpFetch<'a> {
    /// HTTP method (e.g. `GET`, `POST`).
    pub method: &'a str,
    /// Approved request URL/headers/body.
    pub request: HostHttpRequest<'a>,
}

/// HTTP response surfaced back to the guest after a successful host fetch.
#[derive(Debug, Clone, Default)]
pub struct HostHttpResponse {
    /// HTTP status code.
    pub status: u16,
    /// Response headers in the order received.
    pub headers: Vec<(String, String)>,
    /// Response body bytes.
    pub body: Vec<u8>,
}

/// Host-side wiring for the four capability imports. The default
/// [`NoHostServices`] implementation rejects every capability; production
/// callers (`cortex::wasm_plugins::try_execute_tool`) construct a real
/// implementation backed by the agent's HTTP client, DB, and filesystem.
///
/// Implementations are called from inside wasmtime host functions on a
/// blocking thread (the agent already runs WASM plugins inside
/// `tokio::task::spawn_blocking`), so synchronous I/O is fine.
pub trait HostServices: Send + Sync {
    /// Issue a plugin-originated HTTP request. The host must call
    /// [`validate_host_http_request`] before any network operation.
    fn http_fetch(&self, _req: &HostHttpFetch<'_>) -> Result<HostHttpResponse> {
        bail!("borg:http.fetch is not wired in this host")
    }
    /// Read a value from the plugin's scoped state store.
    fn state_read(&self, _key: &str) -> Result<Option<Vec<u8>>> {
        bail!("borg:state.read is not wired in this host")
    }
    /// Write a value to the plugin's scoped state store.
    fn state_write(&self, _key: &str, _value: &[u8]) -> Result<()> {
        bail!("borg:state.write is not wired in this host")
    }
    /// Resolve a credential key declared in the manifest. Implementations
    /// must refuse keys not in `manifest.credentials`.
    fn credentials_get(&self, _key: &str) -> Result<Option<String>> {
        bail!("borg:credentials.get is not wired in this host")
    }
}

/// Host services that reject every capability. Used by tests and as the
/// default when [`execute_tool`] is called without an explicit `services`.
pub struct NoHostServices;

impl HostServices for NoHostServices {}

/// Reserved JSON key prefix used by the host to route multi-tool dispatch.
/// Manifest validation rejects any tool input schema using a property with
/// this prefix, so the key cannot collide with a guest-defined field.
pub const RESERVED_KEY_PREFIX: &str = "_borg_";

/// Reserved JSON key carrying the dispatched tool name to multi-tool plugins.
pub const RESERVED_TOOL_KEY: &str = "_borg_tool";

fn default_timeout_ms() -> u64 {
    5_000
}

fn default_fuel() -> u64 {
    FUEL_DEFAULT
}

fn default_max_memory_pages() -> u64 {
    MAX_MEMORY_PAGES_DEFAULT
}

fn default_max_output_bytes() -> u64 {
    MAX_OUTPUT_BYTES_DEFAULT
}

/// Shared fixtures for the wasm submodule test suites: a minimal ABI-complete
/// module, a manifest builder pinned to its hash, and a customisable-body
/// variant for adversarial-execution tests.
#[cfg(test)]
pub(super) mod test_util {
    use wasm_encoder::{
        CodeSection, ConstExpr, DataSection, ExportKind, ExportSection, Function, FunctionSection,
        Instruction, MemorySection, MemoryType, Module, TypeSection, ValType,
    };

    use super::{
        sha256_hex, PublisherManifest, ResourceLimits, WasmPluginManifest, WasmToolManifest,
        SCHEMA_VERSION,
    };

    pub(crate) fn minimal_wasm() -> Vec<u8> {
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

    pub(crate) fn manifest_for(wasm: &[u8]) -> WasmPluginManifest {
        WasmPluginManifest {
            schema_version: SCHEMA_VERSION.to_string(),
            id: "dev/echo".to_string(),
            name: "Echo".to_string(),
            version: "1.0.0".to_string(),
            publisher: PublisherManifest {
                name: "Dev".to_string(),
                public_key: String::new(),
            },
            wasm_sha256: sha256_hex(wasm),
            tools: vec![WasmToolManifest {
                name: "echo_wasm".to_string(),
                description: "Echo test".to_string(),
                input_schema: serde_json::json!({"type": "object", "properties": {}}),
            }],
            capabilities: vec![],
            endpoints: vec![],
            credentials: vec![],
            limits: ResourceLimits::default(),
        }
    }

    /// Build a WASM module matching the Borg plugin ABI but with a customisable
    /// `borg_tool_call` body. Used by adversarial-execution tests so each one
    /// changes exactly the bytes under test.
    pub(crate) fn abi_wasm_with_call_body(build_call_body: impl FnOnce(&mut Function)) -> Vec<u8> {
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
        build_call_body(&mut call);
        code.function(&call);
        module.section(&code);

        module.finish()
    }
}

#[cfg(test)]
mod tests {
    use super::test_util::{manifest_for, minimal_wasm};
    use super::*;

    #[test]
    fn manifest_rejects_unknown_top_level_fields() {
        // A distributor must not be able to smuggle un-vetted fields past the
        // verifier by tacking them onto the JSON. The signature would still
        // verify (we re-canonicalize the parsed struct), but operators
        // inspecting manifest.json on disk would see fields the host never
        // considered. deny_unknown_fields forces the rejection at parse.
        let wasm = minimal_wasm();
        let manifest = manifest_for(&wasm);
        let mut value = serde_json::to_value(&manifest).unwrap();
        value
            .as_object_mut()
            .unwrap()
            .insert("evil_field".to_string(), serde_json::json!("trojan"));
        let bytes = serde_json::to_vec(&value).unwrap();
        let err = serde_json::from_slice::<WasmPluginManifest>(&bytes)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("unknown field") && err.contains("evil_field"),
            "deny_unknown_fields must reject extra top-level keys, got: {err}"
        );
    }

    #[test]
    fn manifest_rejects_unknown_nested_fields() {
        // Same guard at depth: a trojan key inside publisher/tool/limits must
        // also fail to parse, otherwise the trojan could ride into a section
        // the top-level guard alone wouldn't see.
        let wasm = minimal_wasm();
        let manifest = manifest_for(&wasm);

        for nested_path in ["publisher", "limits"] {
            let mut value = serde_json::to_value(&manifest).unwrap();
            value[nested_path]
                .as_object_mut()
                .unwrap()
                .insert("evil_field".to_string(), serde_json::json!(1));
            let bytes = serde_json::to_vec(&value).unwrap();
            let err = serde_json::from_slice::<WasmPluginManifest>(&bytes)
                .unwrap_err()
                .to_string();
            assert!(
                err.contains("unknown field") && err.contains("evil_field"),
                "[{nested_path}] expected nested unknown-field rejection, got: {err}"
            );
        }

        let mut value = serde_json::to_value(&manifest).unwrap();
        value["tools"][0]
            .as_object_mut()
            .unwrap()
            .insert("evil_field".to_string(), serde_json::json!(1));
        let bytes = serde_json::to_vec(&value).unwrap();
        let err = serde_json::from_slice::<WasmPluginManifest>(&bytes)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("unknown field") && err.contains("evil_field"),
            "[tools[0]] expected nested unknown-field rejection, got: {err}"
        );
    }

    #[test]
    fn wasmtime_config_has_required_setters() {
        // Source-grep guard. `wasmtime::Config` has no getters, so this
        // invariant can't be expressed in the type system. The setters below
        // are the minimum sandbox posture we ship — losing any one of them is
        // a real-world regression:
        //
        //   * `consume_fuel(true)` — without it, no instruction budget.
        //   * `epoch_interruption(true)` — without it, the per-call watchdog
        //     thread that arms `Engine::increment_epoch` has nothing to
        //     interrupt, and the documented wall-clock cap is unenforced.
        //   * `wasm_threads(false)` — keeps a guest from spawning host OS
        //     threads outside our budgets; complements the static gauntlet's
        //     shared-memory rejection.
        //   * `debug_info(false)` — smaller modules; no source-path leakage
        //     from a signed package back into host log lines.
        //
        // The check runs against `execute.rs`, the production-only file that
        // owns `build_engine`. This test lives in `mod.rs`, so a setter
        // mentioned in this test body itself cannot satisfy the assertion.
        let production = include_str!("execute.rs");
        for setter in [
            "consume_fuel(true)",
            "epoch_interruption(true)",
            "wasm_threads(false)",
            "debug_info(false)",
        ] {
            assert!(
                production.contains(setter),
                "wasmtime Config production code must call `{setter}` to preserve the v1 sandbox posture"
            );
        }
    }
}
