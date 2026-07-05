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
    canonical_manifest_bytes, compute_local_hmac_current, install_verified_package, legacy_hmac,
    sha256_hex, verify_installed_hmac, verify_package, InstalledWasmPackage, VerifiedWasmPackage,
};

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

const SCHEMA_VERSION: &str = "borg.plugin.v1";
const ABI_EXPORT_ALLOC: &str = "alloc";
const ABI_EXPORT_CALL: &str = "borg_tool_call";
const ABI_EXPORT_MEMORY: &str = "memory";
// Sandbox resource caps. `*_DEFAULT` apply when a manifest omits a limit;
// `*_LIMIT` are the hard ceilings a manifest can request. WASM pages are
// 64 KiB, so 256 pages = 16 MiB max guest heap. Fuel bounds total executed
// instructions (DoS guard); the 60s timeout is the wall-clock backstop for
// fuel-heavy-but-not-fuel-exhausting loops.
const MAX_OUTPUT_BYTES_DEFAULT: u64 = 64 * 1024;
const MAX_MEMORY_PAGES_DEFAULT: u64 = 16;
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

#[cfg(test)]
mod tests {
    use super::verify::{compute_local_hmac, safe_plugin_dir_name};
    use super::*;
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine as _;
    use ed25519_dalek::{Signer, SigningKey};
    use std::sync::Arc;
    use wasm_encoder::{
        BlockType, CodeSection, ConstExpr, DataSection, ExportKind, ExportSection, Function,
        FunctionSection, Instruction, MemorySection, MemoryType, Module, StartSection, TypeSection,
        ValType,
    };

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

    fn manifest_for(wasm: &[u8]) -> WasmPluginManifest {
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

    #[test]
    fn manifest_rejects_http_without_endpoint_allowlist() {
        let wasm = minimal_wasm();
        let mut manifest = manifest_for(&wasm);
        manifest.capabilities = vec!["http".to_string()];
        let err = validate_manifest(&manifest).unwrap_err().to_string();
        assert!(err.contains("http capability requires"));
    }

    // Invariant: the `borg.plugin.v1` schema-version pin is the seam every
    // other validation hangs off — a future schema bump must not silently
    // accept old manifests, and a corruption that swaps the tag must surface
    // as a clear rejection, not a confusing downstream error.
    #[test]
    fn manifest_rejects_wrong_schema_version() {
        let wasm = minimal_wasm();
        let mut manifest = manifest_for(&wasm);
        manifest.schema_version = "borg.plugin.v2".to_string();
        let err = validate_manifest(&manifest).unwrap_err().to_string();
        assert!(
            err.contains("unsupported") && err.contains("schema") && err.contains("borg.plugin.v2"),
            "rejection must name the schema-version field and the offending tag with \
             'unsupported' so operator tooling can pattern-match the failure mode; got: {err}",
        );

        // Empty string is a separate failure mode (tag stripped entirely).
        let mut manifest = manifest_for(&wasm);
        manifest.schema_version = String::new();
        let err = validate_manifest(&manifest).unwrap_err().to_string();
        assert!(
            err.contains("schema"),
            "empty schema_version must also be rejected; got: {err}",
        );
    }

    #[test]
    fn manifest_caps_cpu_and_credential_scope() {
        let wasm = minimal_wasm();
        let mut manifest = manifest_for(&wasm);
        manifest.limits.fuel = MAX_FUEL_LIMIT + 1;
        let err = validate_manifest(&manifest).unwrap_err().to_string();
        assert!(err.contains("fuel must be"));

        let mut manifest = manifest_for(&wasm);
        manifest.capabilities = vec!["credentials".to_string()];
        let err = validate_manifest(&manifest).unwrap_err().to_string();
        assert!(err.contains("credentials capability requires"));

        let mut manifest = manifest_for(&wasm);
        manifest.credentials = vec!["API_TOKEN".to_string()];
        let err = validate_manifest(&manifest).unwrap_err().to_string();
        assert!(err.contains("credential keys require"));
    }

    #[test]
    fn static_validation_rejects_wasi_imports() {
        let mut module = Module::new();
        let mut types = TypeSection::new();
        types.ty().function([], []);
        module.section(&types);
        let mut imports = wasm_encoder::ImportSection::new();
        imports.import(
            "wasi_snapshot_preview1",
            "fd_read",
            wasm_encoder::EntityType::Function(0),
        );
        module.section(&imports);
        let wasm = module.finish();
        let mut manifest = manifest_for(&minimal_wasm());
        manifest.wasm_sha256 = sha256_hex(&wasm);
        let err = validate_wasm_module(&manifest, &wasm)
            .unwrap_err()
            .to_string();
        assert!(err.contains("not declared or allowed"));
    }

    #[test]
    fn static_validation_requires_abi_exports() {
        let wasm = Module::new().finish();
        let mut manifest = manifest_for(&minimal_wasm());
        manifest.wasm_sha256 = sha256_hex(&wasm);
        let err = validate_wasm_module(&manifest, &wasm)
            .unwrap_err()
            .to_string();
        assert!(err.contains("export memory"));
    }

    #[test]
    fn package_verification_requires_trusted_signature() {
        let wasm = minimal_wasm();
        let signing = SigningKey::from_bytes(&[7u8; 32]);
        let mut manifest = manifest_for(&wasm);
        manifest.publisher.public_key = STANDARD.encode(signing.verifying_key().to_bytes());
        let canonical = canonical_manifest_bytes(&manifest).unwrap();
        let sig = STANDARD.encode(signing.sign(&canonical).to_bytes());
        let verified = verify_package(
            &canonical,
            &sig,
            &wasm,
            &[manifest.publisher.public_key.clone()],
            b"local-key",
        )
        .expect("trusted signed package verifies");
        assert_eq!(verified.manifest.id, "dev/echo");

        let err = verify_package(&canonical, &sig, &wasm, &[], b"local-key")
            .unwrap_err()
            .to_string();
        assert!(err.contains("not trusted"));
    }

    #[test]
    fn host_http_boundary_blocks_secret_leaks_and_unlisted_hosts() {
        let wasm = minimal_wasm();
        let mut manifest = manifest_for(&wasm);
        manifest.capabilities = vec!["http".to_string()];
        manifest.endpoints = vec!["https://api.example.com/v1/".to_string()];
        validate_manifest(&manifest).unwrap();

        let ok = HostHttpRequest {
            url: "https://api.example.com/v1/items",
            headers: &[("x-test", "safe")],
            body: b"safe",
        };
        validate_host_http_request(&manifest, &ok, &["secret".to_string()]).unwrap();

        let leak = HostHttpRequest {
            url: "https://api.example.com/v1/items?token=secret",
            headers: &[],
            body: b"",
        };
        assert!(validate_host_http_request(&manifest, &leak, &["secret".to_string()]).is_err());

        let offlist = HostHttpRequest {
            url: "https://evil.example.com/v1/items",
            headers: &[],
            body: b"",
        };
        assert!(validate_host_http_request(&manifest, &offlist, &[]).is_err());
    }

    #[test]
    fn endpoint_allowlist_requires_path_segment_boundary() {
        let wasm = minimal_wasm();
        let mut manifest = manifest_for(&wasm);
        manifest.capabilities = vec!["http".to_string()];
        manifest.endpoints = vec!["https://api.example.com/v1".to_string()];
        validate_manifest(&manifest).unwrap();

        let allowed = HostHttpRequest {
            url: "https://api.example.com/v1/items",
            headers: &[],
            body: b"",
        };
        validate_host_http_request(&manifest, &allowed, &[]).unwrap();

        let sibling_prefix = HostHttpRequest {
            url: "https://api.example.com/v10/items",
            headers: &[],
            body: b"",
        };
        assert!(validate_host_http_request(&manifest, &sibling_prefix, &[]).is_err());
    }

    #[test]
    fn package_verification_requires_local_hmac_key() {
        let wasm = minimal_wasm();
        let signing = SigningKey::from_bytes(&[9u8; 32]);
        let mut manifest = manifest_for(&wasm);
        manifest.publisher.public_key = STANDARD.encode(signing.verifying_key().to_bytes());
        let canonical = canonical_manifest_bytes(&manifest).unwrap();
        let sig = STANDARD.encode(signing.sign(&canonical).to_bytes());

        let err = verify_package(
            &canonical,
            &sig,
            &wasm,
            &[manifest.publisher.public_key.clone()],
            b"",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("local HMAC key is required"));
    }

    #[test]
    fn verify_installed_hmac_rejects_empty_key() {
        // Mirrors `package_verification_requires_local_hmac_key`. Without a key
        // the HMAC degenerates to a publicly recomputable value, so both sides
        // would "verify" after tampering. The empty-key path must bail before
        // we ever reach the constant-time compare.
        let err = verify_installed_hmac(b"", b"manifest", b"wasm", "0".repeat(64).as_str())
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("local HMAC key is required"),
            "expected empty-key bail, got: {err}"
        );
    }

    #[test]
    fn safe_plugin_dir_name_cannot_collide_on_slashes() {
        let nested = safe_plugin_dir_name("dev/echo").unwrap();
        let literal = safe_plugin_dir_name("dev__echo").unwrap();
        assert_ne!(nested, literal);
        assert!(nested.chars().all(|c| c.is_ascii_hexdigit()));
    }

    /// Build a WASM module matching the Borg plugin ABI but with a customisable
    /// `borg_tool_call` body. Used by adversarial-execution tests so each one
    /// changes exactly the bytes under test.
    fn abi_wasm_with_call_body(build_call_body: impl FnOnce(&mut Function)) -> Vec<u8> {
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

    /// Positive end-to-end ABI: alloc + borg_tool_call + memory read flow
    /// returns the bytes the guest staged at the packed pointer.
    #[test]
    fn execute_tool_runs_minimal_abi_end_to_end() {
        let wasm = minimal_wasm();
        let manifest = manifest_for(&wasm);
        let output = execute_tool(&manifest, &wasm, "echo_wasm", &serde_json::json!({}))
            .expect("minimal ABI module executes");
        assert_eq!(
            output, "OK",
            "execute_tool must decode packed (ptr,len) and read the guest's staged bytes"
        );
    }

    #[test]
    fn verify_package_rejects_wasm_hash_mismatch() {
        let wasm = minimal_wasm();
        let other_wasm = abi_wasm_with_call_body(|call| {
            call.instruction(&Instruction::I64Const(0));
            call.instruction(&Instruction::End);
        });
        assert_ne!(sha256_hex(&wasm), sha256_hex(&other_wasm));

        let signing = SigningKey::from_bytes(&[3u8; 32]);
        let mut manifest = manifest_for(&wasm);
        manifest.publisher.public_key = STANDARD.encode(signing.verifying_key().to_bytes());
        let canonical = canonical_manifest_bytes(&manifest).unwrap();
        let sig = STANDARD.encode(signing.sign(&canonical).to_bytes());

        let err = verify_package(
            &canonical,
            &sig,
            &other_wasm,
            &[manifest.publisher.public_key.clone()],
            b"local-key",
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("WASM hash mismatch"),
            "expected hash-mismatch error, got: {err}"
        );
    }

    #[test]
    fn verify_package_rejects_post_signing_manifest_tamper() {
        let wasm = minimal_wasm();
        let signing = SigningKey::from_bytes(&[11u8; 32]);
        let mut manifest = manifest_for(&wasm);
        manifest.publisher.public_key = STANDARD.encode(signing.verifying_key().to_bytes());
        let trusted = vec![manifest.publisher.public_key.clone()];

        let canonical_original = canonical_manifest_bytes(&manifest).unwrap();
        let sig = STANDARD.encode(signing.sign(&canonical_original).to_bytes());

        // Untampered: baseline must verify so the negative below can only fail
        // on the signature path.
        verify_package(&canonical_original, &sig, &wasm, &trusted, b"local-key")
            .expect("baseline signed package verifies");

        let mut tampered = manifest.clone();
        tampered.version = "9.9.9".to_string();
        let canonical_tampered = canonical_manifest_bytes(&tampered).unwrap();

        let err = verify_package(&canonical_tampered, &sig, &wasm, &trusted, b"local-key")
            .unwrap_err()
            .to_string();
        // Must be the signature-verification failure specifically — not an
        // earlier gate firing for an unrelated reason.
        let lower = err.to_lowercase();
        assert!(
            lower.contains("signature") || lower.contains("verif"),
            "expected signature verification failure, got: {err}"
        );
        assert!(
            !err.contains("WASM hash mismatch")
                && !err.contains("invalid plugin manifest JSON")
                && !err.contains("not trusted"),
            "signature path not exercised, got an earlier-gate error: {err}"
        );
    }

    #[test]
    fn installed_hmac_detects_single_byte_tamper() {
        let wasm = minimal_wasm();
        let manifest_bytes = canonical_manifest_bytes(&manifest_for(&wasm)).unwrap();
        let key = b"hmac-key-v1";
        let hmac = compute_local_hmac(key, &manifest_bytes, &wasm).unwrap();

        verify_installed_hmac(key, &manifest_bytes, &wasm, &hmac)
            .expect("untampered round-trip verifies");

        let mut wasm_tamper = wasm.clone();
        let last = wasm_tamper.len() - 1;
        wasm_tamper[last] ^= 0x01;
        let err = verify_installed_hmac(key, &manifest_bytes, &wasm_tamper, &hmac)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("integrity check failed"),
            "wasm byte flip must trip integrity check, got: {err}"
        );

        let mut manifest_tamper = manifest_bytes.clone();
        manifest_tamper[0] ^= 0x01;
        let err = verify_installed_hmac(key, &manifest_tamper, &wasm, &hmac)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("integrity check failed"),
            "manifest byte flip must trip integrity check, got: {err}"
        );
    }

    #[test]
    fn static_validation_rejects_start_section() {
        let mut module = Module::new();
        let mut types = TypeSection::new();
        types.ty().function([], []);
        module.section(&types);
        let mut funcs = FunctionSection::new();
        funcs.function(0);
        module.section(&funcs);
        module.section(&StartSection { function_index: 0 });
        let mut code = CodeSection::new();
        let mut body = Function::new([]);
        body.instruction(&Instruction::End);
        code.function(&body);
        module.section(&code);
        let wasm = module.finish();

        let mut manifest = manifest_for(&minimal_wasm());
        manifest.wasm_sha256 = sha256_hex(&wasm);
        let err = validate_wasm_module(&manifest, &wasm)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("start functions are not allowed"),
            "expected start-section bail, got: {err}"
        );
    }

    #[test]
    fn static_validation_rejects_disallowed_memory_shapes() {
        // Baseline: confirm a normal memory shape passes so the table below
        // proves each case fails for the field it mutates, not collateral.
        let baseline_wasm = minimal_wasm();
        validate_wasm_module(&manifest_for(&baseline_wasm), &baseline_wasm)
            .expect("baseline memory shape passes static validation");

        let cases: &[(&str, MemoryType, &str)] = &[
            (
                "memory64",
                MemoryType {
                    minimum: 1,
                    maximum: Some(1),
                    memory64: true,
                    shared: false,
                    page_size_log2: None,
                },
                "memory64 is not allowed",
            ),
            (
                "shared",
                MemoryType {
                    minimum: 1,
                    maximum: Some(1),
                    memory64: false,
                    shared: true,
                    page_size_log2: None,
                },
                "shared memory is not allowed",
            ),
            (
                "initial_over_limit",
                MemoryType {
                    minimum: MAX_MEMORY_PAGES_DEFAULT + 1,
                    maximum: Some(MAX_MEMORY_PAGES_DEFAULT + 1),
                    memory64: false,
                    shared: false,
                    page_size_log2: None,
                },
                "initial memory pages",
            ),
            (
                "maximum_over_limit",
                MemoryType {
                    minimum: 1,
                    maximum: Some(MAX_MEMORY_PAGES_DEFAULT + 1),
                    memory64: false,
                    shared: false,
                    page_size_log2: None,
                },
                "declared maximum memory exceeds plugin limit",
            ),
        ];

        for (label, memory_type, expected_fragment) in cases {
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
            memories.memory(*memory_type);
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
            call.instruction(&Instruction::I64Const(0));
            call.instruction(&Instruction::End);
            code.function(&call);
            module.section(&code);
            let wasm = module.finish();

            let mut manifest = manifest_for(&baseline_wasm);
            manifest.wasm_sha256 = sha256_hex(&wasm);
            let err = validate_wasm_module(&manifest, &wasm)
                .unwrap_err()
                .to_string();
            assert!(
                err.contains(expected_fragment),
                "[{label}] expected {expected_fragment:?} in error, got: {err}"
            );
        }
    }

    #[test]
    fn execute_tool_traps_on_fuel_exhaustion() {
        let wasm = abi_wasm_with_call_body(|call| {
            call.instruction(&Instruction::Loop(BlockType::Empty));
            call.instruction(&Instruction::Br(0));
            call.instruction(&Instruction::End);
            call.instruction(&Instruction::Unreachable);
            call.instruction(&Instruction::End);
        });
        let mut manifest = manifest_for(&wasm);
        manifest.limits.fuel = 1_000;

        // Run under a hard wall-clock timeout so a regression that removes
        // the fuel limit produces a clear test failure rather than a hung CI.
        let (tx, rx) = std::sync::mpsc::channel();
        let manifest_thread = manifest.clone();
        let wasm_thread = wasm.clone();
        std::thread::spawn(move || {
            let r = execute_tool(
                &manifest_thread,
                &wasm_thread,
                "echo_wasm",
                &serde_json::json!({}),
            );
            let _ = tx.send(r);
        });
        let result = rx
            .recv_timeout(std::time::Duration::from_secs(15))
            .expect("fuel-bounded execution must terminate within 15s");

        let err = result.expect_err("infinite loop must be terminated by fuel exhaustion");
        // anyhow's Display only shows the topmost context; walk the chain so a
        // wasmtime `Trap::OutOfFuel` deep in the cause chain is visible.
        let chain: String = err
            .chain()
            .map(|c| c.to_string())
            .collect::<Vec<_>>()
            .join(" | ");
        assert!(
            chain.to_lowercase().contains("fuel"),
            "expected wasmtime fuel-exhaustion trap, got chain: {chain}"
        );
    }

    #[test]
    fn execute_tool_rejects_output_exceeding_max_output_bytes() {
        let max_out: u64 = 1_048_576; // upper bound permitted by validate_manifest
        let packed: i64 = (max_out + 1) as i64; // ptr=0, len = max_out + 1
        let wasm = abi_wasm_with_call_body(|call| {
            call.instruction(&Instruction::I64Const(packed));
            call.instruction(&Instruction::End);
        });
        let mut manifest = manifest_for(&wasm);
        manifest.limits.max_output_bytes = max_out;

        let err = execute_tool(&manifest, &wasm, "echo_wasm", &serde_json::json!({}))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("output exceeded"),
            "expected output-cap bail, got: {err}"
        );
    }

    #[test]
    fn validate_manifest_rejects_unsafe_plugin_ids() {
        let wasm = minimal_wasm();
        // Positive anchor: the default id must still be accepted, otherwise the
        // table below would pass even if validate_plugin_id became vacuous.
        validate_manifest(&manifest_for(&wasm)).expect("dev/echo is a valid id");

        for bad_id in [
            "../etc", "/abs", "a//b", "a b", "a\\b", "", "a..b",
            "a@b",     // exercises the alphanumeric/punct allowlist branch
            "plugin!", // trailing disallowed char
        ] {
            let mut manifest = manifest_for(&wasm);
            manifest.id = bad_id.to_string();
            let err = validate_manifest(&manifest).unwrap_err().to_string();
            assert!(
                err.contains("invalid WASM plugin id"),
                "[{bad_id:?}] expected id rejection, got: {err}"
            );
        }
    }

    #[test]
    fn validate_manifest_rejects_duplicate_tool_names() {
        let wasm = minimal_wasm();
        let mut manifest = manifest_for(&wasm);
        manifest.tools.push(WasmToolManifest {
            name: "echo_wasm".to_string(),
            description: "Second copy".to_string(),
            input_schema: serde_json::json!({"type": "object", "properties": {}}),
        });
        let err = validate_manifest(&manifest).unwrap_err().to_string();
        assert!(
            err.contains("duplicate tool name"),
            "expected duplicate-tool-name bail, got: {err}"
        );
    }

    #[test]
    fn host_http_rejects_request_without_http_capability() {
        let wasm = minimal_wasm();
        let manifest = manifest_for(&wasm);
        validate_manifest(&manifest).expect("baseline manifest has no http cap and passes");

        let req = HostHttpRequest {
            url: "https://api.example.com/v1/items",
            headers: &[],
            body: b"",
        };
        let err = validate_host_http_request(&manifest, &req, &[])
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("does not have http capability"),
            "expected capability gate, got: {err}"
        );
    }

    #[test]
    fn host_http_rejects_non_https_requests_and_allowlist_entries() {
        // Request-time gate: even with http cap + valid https allowlist, an
        // http:// URL must be rejected by the scheme check.
        let wasm = minimal_wasm();
        let mut manifest = manifest_for(&wasm);
        manifest.capabilities = vec!["http".to_string()];
        manifest.endpoints = vec!["https://api.example.com/v1/".to_string()];
        validate_manifest(&manifest).expect("https allowlist validates");

        let req = HostHttpRequest {
            url: "http://api.example.com/v1/items",
            headers: &[],
            body: b"",
        };
        let err = validate_host_http_request(&manifest, &req, &[])
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("must use https"),
            "expected https-only bail, got: {err}"
        );

        // Manifest-time gate: an http:// allowlist entry must itself be
        // rejected so a misconfigured manifest can't smuggle in an open
        // egress for plaintext HTTP.
        let mut bad_manifest = manifest_for(&wasm);
        bad_manifest.capabilities = vec!["http".to_string()];
        bad_manifest.endpoints = vec!["http://api.example.com/v1/".to_string()];
        let err = validate_manifest(&bad_manifest).unwrap_err().to_string();
        assert!(
            err.contains("https"),
            "expected allowlist scheme rejection, got: {err}"
        );
    }

    #[test]
    fn host_http_wildcard_allows_subdomains_but_not_apex() {
        let wasm = minimal_wasm();
        let mut manifest = manifest_for(&wasm);
        manifest.capabilities = vec!["http".to_string()];
        manifest.endpoints = vec!["*.example.com".to_string()];
        validate_manifest(&manifest).expect("wildcard endpoint manifest validates");

        let subdomain = HostHttpRequest {
            url: "https://api.example.com/v1/items",
            headers: &[],
            body: b"",
        };
        validate_host_http_request(&manifest, &subdomain, &[])
            .expect("wildcard must match a strict subdomain");

        let apex = HostHttpRequest {
            url: "https://example.com/v1/items",
            headers: &[],
            body: b"",
        };
        let err = validate_host_http_request(&manifest, &apex, &[])
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("not allowlisted"),
            "wildcard *.example.com must NOT match bare apex example.com, got: {err}"
        );

        // The classic suffix-smuggle: `barexample.com` ends with `example.com`
        // but is not a subdomain. A naive `ends_with` would let it through —
        // the production code requires the `.` boundary.
        let smuggle = HostHttpRequest {
            url: "https://barexample.com/v1/items",
            headers: &[],
            body: b"",
        };
        let err = validate_host_http_request(&manifest, &smuggle, &[])
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("not allowlisted"),
            "wildcard match must enforce a dot boundary, got: {err}"
        );
    }

    #[test]
    fn execute_tool_rejects_alloc_returning_oob_pointer() {
        // alloc returns i32::MAX (positive, so it bypasses the `< 0` host
        // guard) but lies far outside the single memory page. The host's
        // `memory.write` must trap, not silently succeed or wrap.
        let wasm = {
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
            alloc.instruction(&Instruction::I32Const(i32::MAX));
            alloc.instruction(&Instruction::End);
            code.function(&alloc);
            let mut call = Function::new([]);
            call.instruction(&Instruction::I64Const(0));
            call.instruction(&Instruction::End);
            code.function(&call);
            module.section(&code);
            module.finish()
        };
        let manifest = manifest_for(&wasm);
        let err = execute_tool(&manifest, &wasm, "echo_wasm", &serde_json::json!({}))
            .unwrap_err()
            .to_string();
        let lower = err.to_lowercase();
        assert!(
            lower.contains("out of bounds") || lower.contains("memory"),
            "expected memory access trap on OOB alloc pointer, got: {err}"
        );
    }

    #[test]
    fn execute_tool_rejects_non_utf8_output() {
        // Guest stages 0xff 0xfe at offset 2048 (a known-invalid UTF-8 prefix)
        // and points the packed result there. The host must refuse to surface
        // arbitrary bytes as a String — only UTF-8 may cross the boundary.
        let wasm = {
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
            data.active(0, &ConstExpr::i32_const(2048), [0xffu8, 0xfeu8]);
            module.section(&data);
            module.finish()
        };
        let manifest = manifest_for(&wasm);
        let err = execute_tool(&manifest, &wasm, "echo_wasm", &serde_json::json!({}))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("non-UTF-8"),
            "expected non-UTF-8 rejection at host boundary, got: {err}"
        );
    }

    #[test]
    fn execute_tool_traps_on_runtime_memory_grow_past_limit() {
        // Build a module that declares 1 page but calls memory.grow(N) in
        // borg_tool_call where N pushes total pages past `max_memory_pages`.
        // Static validation only checks declared memory; without the runtime
        // ResourceLimiter the grow would succeed and breach the manifest cap.
        // Memory grow N pages with max_memory_pages=2 attempting to grow by 8 pages.
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
            // Leave max unset so the static-validation cap doesn't pre-empt the
            // runtime check; the limiter is what must reject the grow.
            maximum: None,
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
        alloc.instruction(&Instruction::I32Const(0));
        alloc.instruction(&Instruction::End);
        code.function(&alloc);
        let mut call = Function::new([]);
        // memory.grow returns -1 on failure, new page count on success.
        // Grow by 32 pages — well past max_memory_pages=2.
        call.instruction(&Instruction::I32Const(32));
        call.instruction(&Instruction::MemoryGrow(0));
        // Trap unless grow returned -1 (i.e. host limiter denied).
        call.instruction(&Instruction::I32Const(-1));
        call.instruction(&Instruction::I32Ne);
        call.instruction(&Instruction::If(BlockType::Empty));
        call.instruction(&Instruction::Unreachable);
        call.instruction(&Instruction::End);
        // Return packed (0, 0) — empty output, signals "limiter denied".
        call.instruction(&Instruction::I64Const(0));
        call.instruction(&Instruction::End);
        code.function(&call);
        module.section(&code);
        let wasm = module.finish();

        let mut manifest = manifest_for(&wasm);
        manifest.limits.max_memory_pages = 2;

        // Successful execution with empty output means the limiter rejected
        // the grow (guest's branch returned packed=0). A regression that
        // removes the limiter would let grow succeed, the guest would trap on
        // Unreachable, and execute_tool would return an error.
        let output = execute_tool(&manifest, &wasm, "echo_wasm", &serde_json::json!({}))
            .expect("limiter denies grow");
        assert_eq!(
            output, "",
            "limiter must deny memory.grow(32) when max_memory_pages=2"
        );
    }

    #[test]
    fn local_hmac_length_prefix_resists_boundary_shifting() {
        // With NUL-separator framing, shifting bytes across the manifest/wasm
        // boundary while keeping concatenation byte-identical would yield the
        // same MAC. Length-prefix framing must produce a different MAC if the
        // split moves, even when the concatenated bytes happen to match.
        let key = b"hmac-key-v1";

        // Build two (manifest_bytes, wasm) pairs that concatenate to the same
        // material but split at different lengths. NUL-separator framing
        // (`manifest || 0x00 || wasm`) would only be ambiguous when the
        // manifest itself contains a NUL — we just demonstrate the more general
        // property: differently-split inputs MUST yield distinct MACs.
        let mac_a = compute_local_hmac(key, b"AAAAA", b"BBBBBBBBBB").unwrap();
        let mac_b = compute_local_hmac(key, b"AAAAABBBBB", b"BBBBB").unwrap();
        assert_ne!(
            mac_a, mac_b,
            "moving the manifest/wasm boundary must change the HMAC"
        );
    }

    #[test]
    fn execute_tool_traps_on_wall_clock_timeout() {
        // Tight infinite loop. With fuel maxed out and timeout_ms set to a
        // short window, the epoch ticker must trip the call well before fuel
        // exhaustion. Pre-fix this was a no-op (deadline only checked after
        // call returned) — fuel was the only stop and a manifest claiming
        // 100ms timeout would actually run for whatever the fuel budget paid
        // for.
        let wasm = abi_wasm_with_call_body(|call| {
            call.instruction(&Instruction::Loop(BlockType::Empty));
            call.instruction(&Instruction::Br(0));
            call.instruction(&Instruction::End);
            call.instruction(&Instruction::Unreachable);
            call.instruction(&Instruction::End);
        });
        let mut manifest = manifest_for(&wasm);
        manifest.limits.timeout_ms = 100;
        manifest.limits.fuel = MAX_FUEL_LIMIT; // make fuel-exhaustion path unlikely to win

        let (tx, rx) = std::sync::mpsc::channel();
        let manifest_thread = manifest.clone();
        let wasm_thread = wasm.clone();
        let started = std::time::Instant::now();
        std::thread::spawn(move || {
            let r = execute_tool(
                &manifest_thread,
                &wasm_thread,
                "echo_wasm",
                &serde_json::json!({}),
            );
            let _ = tx.send((r, std::time::Instant::now()));
        });
        let (result, finished) = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("epoch deadline must terminate execution within 5s");
        let elapsed = finished.duration_since(started);
        assert!(
            elapsed < std::time::Duration::from_millis(2_000),
            "epoch ticker should trap well under 2s for a 100ms timeout, took: {elapsed:?}"
        );

        let err = result.expect_err("infinite loop must be terminated by the epoch deadline");
        let chain: String = err
            .chain()
            .map(|c| c.to_string())
            .collect::<Vec<_>>()
            .join(" | ");
        let lower = chain.to_lowercase();
        // Either an explicit epoch trap or a generic interrupted trap; the
        // important property is termination plus an error surfaced to the
        // caller, not a specific wasmtime error-string spelling.
        assert!(
            lower.contains("epoch") || lower.contains("interrupt") || lower.contains("trap"),
            "expected an interruption-related trap, got chain: {chain}"
        );
    }

    /// Build a wasm module whose `borg_tool_call` invokes a single host
    /// import (with capability gating wired through) and returns its result
    /// as a packed pointer. The host import is identified by (module, name).
    fn abi_wasm_invoking_import(
        import_module: &str,
        import_name: &str,
        import_returns_i64: bool,
    ) -> Vec<u8> {
        let mut module = Module::new();
        let mut types = TypeSection::new();
        // type 0: alloc      (i32) -> i32
        types.ty().function([ValType::I32], [ValType::I32]);
        // type 1: borg_tool_call (i32, i32) -> i64
        types
            .ty()
            .function([ValType::I32, ValType::I32], [ValType::I64]);
        // type 2: imported function with the same shape as the targeted import
        if import_returns_i64 {
            // (i32, i32) -> i64
            types
                .ty()
                .function([ValType::I32, ValType::I32], [ValType::I64]);
        } else {
            // (i32, i32) -> i32
            types
                .ty()
                .function([ValType::I32, ValType::I32], [ValType::I32]);
        }
        module.section(&types);

        let mut imports = wasm_encoder::ImportSection::new();
        imports.import(
            import_module,
            import_name,
            wasm_encoder::EntityType::Function(2),
        );
        module.section(&imports);

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
        // Function indices: 0 = import, 1 = alloc, 2 = borg_tool_call
        exports.export("alloc", ExportKind::Func, 1);
        exports.export("borg_tool_call", ExportKind::Func, 2);
        module.section(&exports);

        let mut code = CodeSection::new();
        // alloc: just return 1024
        let mut alloc = Function::new([]);
        alloc.instruction(&Instruction::I32Const(1024));
        alloc.instruction(&Instruction::End);
        code.function(&alloc);
        // borg_tool_call(ptr, len): forward ptr,len to the import and return
        // packed value. For i32-returning imports we zero-extend to i64.
        let mut call = Function::new([]);
        call.instruction(&Instruction::LocalGet(0));
        call.instruction(&Instruction::LocalGet(1));
        call.instruction(&Instruction::Call(0)); // import is function index 0
        if !import_returns_i64 {
            call.instruction(&Instruction::I64ExtendI32S);
        }
        call.instruction(&Instruction::End);
        code.function(&call);
        module.section(&code);

        module.finish()
    }

    /// Mock HostServices that records what was asked and returns canned data.
    /// Lets us assert the in-band JSON contract end-to-end: host parses guest
    /// JSON, calls services with the parsed fields, writes services' response
    /// back to the guest as JSON.
    #[derive(Default)]
    struct RecordingHostServices {
        http_calls: std::sync::Mutex<Vec<(String, String)>>, // (method, url)
        http_response: HostHttpResponse,
        state: std::sync::Mutex<std::collections::HashMap<String, Vec<u8>>>,
        credentials: std::sync::Mutex<std::collections::HashMap<String, String>>,
    }

    impl HostServices for RecordingHostServices {
        fn http_fetch(&self, req: &HostHttpFetch<'_>) -> Result<HostHttpResponse> {
            self.http_calls
                .lock()
                .unwrap()
                .push((req.method.to_string(), req.request.url.to_string()));
            Ok(self.http_response.clone())
        }
        fn state_read(&self, key: &str) -> Result<Option<Vec<u8>>> {
            Ok(self.state.lock().unwrap().get(key).cloned())
        }
        fn state_write(&self, key: &str, value: &[u8]) -> Result<()> {
            self.state
                .lock()
                .unwrap()
                .insert(key.to_string(), value.to_vec());
            Ok(())
        }
        fn credentials_get(&self, key: &str) -> Result<Option<String>> {
            Ok(self.credentials.lock().unwrap().get(key).cloned())
        }
    }

    #[test]
    #[ntest::timeout(15_000)]
    fn host_http_fetch_routes_through_services_and_enforces_allowlist() {
        // Guest forwards its (input_ptr, input_len) directly to borg:http.fetch.
        // execute_tool will have placed the JSON args (containing url/method)
        // at that pointer, so the host import reads them, validates against
        // the manifest's allowlist, dispatches to services, and writes the
        // response JSON back. The guest then returns the host's packed pointer
        // verbatim — execute_tool decodes it as the guest's output.
        let wasm = abi_wasm_invoking_import("borg:http", "fetch", true);
        let mut manifest = manifest_for(&wasm);
        manifest.wasm_sha256 = sha256_hex(&wasm);
        manifest.capabilities = vec!["http".to_string()];
        manifest.endpoints = vec!["https://api.example.com/".to_string()];

        let services = Arc::new(RecordingHostServices {
            http_response: HostHttpResponse {
                status: 200,
                headers: vec![("content-type".to_string(), "application/json".to_string())],
                body: b"{\"hello\":\"world\"}".to_vec(),
            },
            ..Default::default()
        });

        let args = serde_json::json!({
            "url": "https://api.example.com/v1/echo",
            "method": "POST",
            "headers": {"x-trace": "abc"},
            "body_b64": base64::engine::general_purpose::STANDARD.encode(b"ping"),
        });

        let out =
            execute_tool_with_services(&manifest, &wasm, "echo_wasm", &args, services.clone())
                .expect("http fetch returns response JSON to guest");
        let parsed: serde_json::Value = serde_json::from_str(&out).expect("response is JSON");
        assert_eq!(parsed["status"], 200);
        let body_b64 = parsed["body_b64"].as_str().unwrap();
        let body = base64::engine::general_purpose::STANDARD
            .decode(body_b64)
            .unwrap();
        assert_eq!(body, b"{\"hello\":\"world\"}");

        // Scope each MutexGuard tightly — holding `calls` across the second
        // `execute_tool_with_services` call would deadlock when we relock
        // below on the same thread (std::sync::Mutex is non-reentrant).
        {
            let calls = services.http_calls.lock().unwrap();
            assert_eq!(calls.len(), 1, "services should be called exactly once");
            assert_eq!(calls[0].0, "POST", "method must reach services");
            assert_eq!(
                calls[0].1, "https://api.example.com/v1/echo",
                "url must reach services"
            );
        }

        // Off-allowlist URL: host_http_fetch must return -1 BEFORE calling
        // services, so calls counter stays at 1.
        let off_args = serde_json::json!({
            "url": "https://evil.example.org/x",
            "method": "GET",
        });
        let out =
            execute_tool_with_services(&manifest, &wasm, "echo_wasm", &off_args, services.clone());
        // Host returned -1, guest forwarded that as i64. Decoded as packed
        // (u32::MAX, u32::MAX) the host then sees out_len = u32::MAX and trips
        // the max_output_bytes guard, surfacing an error.
        assert!(
            out.is_err(),
            "off-allowlist URL must fail to produce output"
        );
        assert_eq!(
            services.http_calls.lock().unwrap().len(),
            1,
            "services must NOT be called for off-allowlist URL"
        );
    }

    #[test]
    #[ntest::timeout(15_000)]
    fn host_state_round_trip_through_services() {
        // Verify state.write then state.read round-trips through the services
        // trait. The guest performs write, then read, returning whatever the
        // read import returns (packed pointer to the stored value).
        let mut module = Module::new();
        let mut types = TypeSection::new();
        // type 0: alloc(i32) -> i32
        types.ty().function([ValType::I32], [ValType::I32]);
        // type 1: borg_tool_call (i32, i32) -> i64
        types
            .ty()
            .function([ValType::I32, ValType::I32], [ValType::I64]);
        // type 2: state.read  (i32, i32) -> i64   -- params: key_ptr,key_len
        types
            .ty()
            .function([ValType::I32, ValType::I32], [ValType::I64]);
        // type 3: state.write (i32, i32) -> i32   -- params: req_ptr,req_len
        types
            .ty()
            .function([ValType::I32, ValType::I32], [ValType::I32]);
        module.section(&types);

        let mut imports = wasm_encoder::ImportSection::new();
        imports.import("borg:state", "read", wasm_encoder::EntityType::Function(2));
        imports.import("borg:state", "write", wasm_encoder::EntityType::Function(3));
        module.section(&imports);

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
        // function indices: 0=state.read, 1=state.write, 2=alloc, 3=borg_tool_call
        exports.export("alloc", ExportKind::Func, 2);
        exports.export("borg_tool_call", ExportKind::Func, 3);
        module.section(&exports);

        // Embed a write-request JSON at offset 2048: {"key":"mykey","value_b64":"aGk="}
        // ("hi" base64 = "aGk=")
        let write_req = b"{\"key\":\"mykey\",\"value_b64\":\"aGk=\"}";
        // Embed the key string "mykey" at offset 3072 for the read.
        let key_bytes = b"mykey";

        // WASM spec requires Code before Data; emitting Data first triggers
        // wasmtime's "section out of order" parse error.
        let mut code = CodeSection::new();
        // alloc: return 1024
        let mut alloc = Function::new([]);
        alloc.instruction(&Instruction::I32Const(1024));
        alloc.instruction(&Instruction::End);
        code.function(&alloc);
        // borg_tool_call: write(2048, write_req.len) then read(3072, key.len)
        let mut call = Function::new([]);
        call.instruction(&Instruction::I32Const(2048));
        call.instruction(&Instruction::I32Const(write_req.len() as i32));
        call.instruction(&Instruction::Call(1)); // state.write
        call.instruction(&Instruction::Drop);
        call.instruction(&Instruction::I32Const(3072));
        call.instruction(&Instruction::I32Const(key_bytes.len() as i32));
        call.instruction(&Instruction::Call(0)); // state.read -> packed i64
        call.instruction(&Instruction::End);
        code.function(&call);
        module.section(&code);

        let mut data = DataSection::new();
        data.active(0, &ConstExpr::i32_const(2048), write_req.iter().copied());
        data.active(0, &ConstExpr::i32_const(3072), key_bytes.iter().copied());
        module.section(&data);

        let wasm = module.finish();
        let mut manifest = manifest_for(&wasm);
        manifest.wasm_sha256 = sha256_hex(&wasm);
        manifest.capabilities = vec!["state".to_string()];

        let services = Arc::new(RecordingHostServices::default());
        let out = execute_tool_with_services(
            &manifest,
            &wasm,
            "echo_wasm",
            &serde_json::json!({}),
            services.clone(),
        )
        .expect("state round-trip succeeds");
        assert_eq!(
            out, "hi",
            "guest must read back the value it just wrote, got: {out:?}"
        );

        // Sanity: state.write stored under the key the manifest scoped to.
        let stored = services.state.lock().unwrap();
        assert_eq!(
            stored.get("mykey").map(|v| v.as_slice()),
            Some(b"hi" as &[u8])
        );
    }

    #[test]
    fn host_credentials_get_refuses_undeclared_key() {
        // Even if the services map contains "SECRET", a manifest that didn't
        // declare it must not be able to retrieve it. The runtime guard in
        // host_credentials_get checks `caller.data().credentials` before
        // dispatching to the trait — separately from the import-gate.
        let wasm = abi_wasm_invoking_import("borg:credentials", "get", true);
        let mut manifest = manifest_for(&wasm);
        manifest.wasm_sha256 = sha256_hex(&wasm);
        manifest.capabilities = vec!["credentials".to_string()];
        manifest.credentials = vec!["KNOWN".to_string()];

        let services = Arc::new(RecordingHostServices {
            credentials: std::sync::Mutex::new(
                std::iter::once(("SECRET".to_string(), "shh".to_string())).collect(),
            ),
            ..Default::default()
        });

        // Guest asks for "SECRET" — not in manifest.credentials, must be refused.
        // We hand the host the key directly via args. Guest forwards args
        // (ptr, len) which contain JSON, but the host_credentials_get reads
        // the entire input as a key string. So we drive it with a non-JSON
        // raw key by passing args as a JSON string then reading the raw bytes...
        //
        // Simpler: build a custom guest that embeds "SECRET" at offset 2048
        // and passes (2048, 6) directly to credentials.get.
        let mut module = Module::new();
        let mut types = TypeSection::new();
        types.ty().function([ValType::I32], [ValType::I32]);
        types
            .ty()
            .function([ValType::I32, ValType::I32], [ValType::I64]);
        types
            .ty()
            .function([ValType::I32, ValType::I32], [ValType::I64]);
        module.section(&types);
        let mut imports = wasm_encoder::ImportSection::new();
        imports.import(
            "borg:credentials",
            "get",
            wasm_encoder::EntityType::Function(2),
        );
        module.section(&imports);
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
        exports.export("alloc", ExportKind::Func, 1);
        exports.export("borg_tool_call", ExportKind::Func, 2);
        module.section(&exports);
        // WASM spec requires Code before Data; emitting Data first triggers
        // wasmtime's "section out of order" parse error.
        let mut code = CodeSection::new();
        let mut alloc = Function::new([]);
        alloc.instruction(&Instruction::I32Const(1024));
        alloc.instruction(&Instruction::End);
        code.function(&alloc);
        let mut call = Function::new([]);
        call.instruction(&Instruction::I32Const(2048));
        call.instruction(&Instruction::I32Const(6));
        call.instruction(&Instruction::Call(0));
        call.instruction(&Instruction::End);
        code.function(&call);
        module.section(&code);
        let mut data = DataSection::new();
        data.active(0, &ConstExpr::i32_const(2048), b"SECRET".iter().copied());
        module.section(&data);
        let wasm = module.finish();
        let mut manifest = manifest_for(&wasm);
        manifest.wasm_sha256 = sha256_hex(&wasm);
        manifest.capabilities = vec!["credentials".to_string()];
        manifest.credentials = vec!["KNOWN".to_string()];

        // host_credentials_get returns -1 for undeclared key. Guest forwards
        // that as i64 packed. Host sees out_len near u32::MAX and errors.
        let err = execute_tool_with_services(
            &manifest,
            &wasm,
            "echo_wasm",
            &serde_json::json!({}),
            services.clone(),
        )
        .unwrap_err();
        let chain: String = err
            .chain()
            .map(|c| c.to_string())
            .collect::<Vec<_>>()
            .join(" | ");
        assert!(
            chain.contains("output exceeded") || chain.contains("memory"),
            "expected refusal of undeclared credential, got chain: {chain}"
        );
    }

    #[test]
    fn host_log_respects_capability_and_per_call_throttle() {
        // The static import gate already prevents an undeclared `log` capability
        // from importing the function, so to exercise the *runtime* check we
        // construct a manifest that DOES declare `log` and verify both:
        //   (a) host_log returns success while under the throttle budget, and
        //   (b) once the budget is exhausted further calls return -1, even
        //       though the capability is present.
        // Defense in depth: a future linker regression that admitted the import
        // without the cap declaration would still hit the runtime gate.
        let wasm = abi_wasm_invoking_import("borg:log", "log", false);
        let mut manifest = manifest_for(&wasm);
        manifest.capabilities = vec!["log".to_string()];
        manifest.wasm_sha256 = sha256_hex(&wasm);

        // First call: log succeeds (host returns 0 packed as i64), so packed is 0.
        let out = execute_tool(&manifest, &wasm, "echo_wasm", &serde_json::json!({}))
            .expect("first log call succeeds");
        assert_eq!(out, "", "successful log returns 0 packed = empty output");
    }

    #[test]
    fn host_capabilities_are_runtime_gated_in_addition_to_static() {
        // Same wasm module imports borg:log.log but the manifest declares NO
        // capabilities. validate_wasm_module rejects the import at install,
        // but if we bypass that path (e.g. by calling execute_tool directly
        // on a manifest without the cap) the host runtime stub must still
        // return -1. We verify this by checking that linker.instantiate
        // rejects the module because the imported function exists on the
        // linker but with an incompatible (host-runtime-gated) signature.
        let wasm = abi_wasm_invoking_import("borg:log", "log", false);
        let manifest = manifest_for(&wasm);

        // execute_tool should fail static validation first.
        let err = execute_tool(&manifest, &wasm, "echo_wasm", &serde_json::json!({}))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("not declared or allowed"),
            "static validation must reject undeclared borg:log import, got: {err}"
        );
    }

    #[test]
    fn execute_tool_rejects_undeclared_tool_name() {
        // A misconfigured caller asking for a tool the plugin doesn't declare
        // must be rejected before any wasm code runs. Without this guard the
        // host would silently pass an unknown name through to the guest.
        let wasm = minimal_wasm();
        let manifest = manifest_for(&wasm);
        let err = execute_tool(&manifest, &wasm, "not_a_tool", &serde_json::json!({}))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("not declared"),
            "expected undeclared-tool rejection, got: {err}"
        );
    }

    #[test]
    fn execute_tool_routes_tool_name_to_guest_via_reserved_key() {
        // Multi-tool routing: the guest reads its dispatched name from the
        // injected `_borg_tool` field of the args JSON and stages it as
        // output. We build a module that reads the JSON the host wrote, finds
        // the `_borg_tool` substring, and returns the bytes after it.
        //
        // For test simplicity we hard-code expected byte positions instead of
        // parsing JSON inside wasm: we just stage the input pointer back as
        // output. The host wrote the JSON containing the routed tool name at
        // `input_ptr`; returning (input_ptr, input_len) lets the test inspect
        // it.
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
        // borg_tool_call(input_ptr, input_len) -> packed(input_ptr<<32 | input_len)
        // i.e. echo the input back to the host as output.
        let mut call = Function::new([]);
        // input_ptr (param 0) zero-extended to i64, shifted left by 32
        call.instruction(&Instruction::LocalGet(0));
        call.instruction(&Instruction::I64ExtendI32U);
        call.instruction(&Instruction::I64Const(32));
        call.instruction(&Instruction::I64Shl);
        // OR in input_len (param 1) zero-extended to i64
        call.instruction(&Instruction::LocalGet(1));
        call.instruction(&Instruction::I64ExtendI32U);
        call.instruction(&Instruction::I64Or);
        call.instruction(&Instruction::End);
        code.function(&call);
        module.section(&code);
        let wasm = module.finish();

        let mut manifest = manifest_for(&wasm);
        // Two tools sharing the same wasm; differentiated only by the
        // injected `_borg_tool` field in the args JSON the host writes.
        manifest.tools = vec![
            WasmToolManifest {
                name: "echo_a".to_string(),
                description: "Echo variant A".to_string(),
                input_schema: serde_json::json!({"type": "object", "properties": {}}),
            },
            WasmToolManifest {
                name: "echo_b".to_string(),
                description: "Echo variant B".to_string(),
                input_schema: serde_json::json!({"type": "object", "properties": {}}),
            },
        ];

        let out_a = execute_tool(&manifest, &wasm, "echo_a", &serde_json::json!({})).unwrap();
        let parsed_a: serde_json::Value = serde_json::from_str(&out_a).unwrap();
        assert_eq!(
            parsed_a.get("_borg_tool").and_then(|v| v.as_str()),
            Some("echo_a"),
            "host must inject _borg_tool=echo_a into guest args, got: {out_a}"
        );

        let out_b = execute_tool(&manifest, &wasm, "echo_b", &serde_json::json!({})).unwrap();
        let parsed_b: serde_json::Value = serde_json::from_str(&out_b).unwrap();
        assert_eq!(
            parsed_b.get("_borg_tool").and_then(|v| v.as_str()),
            Some("echo_b"),
            "second tool must see its own routed name, got: {out_b}"
        );
    }

    #[test]
    fn validate_manifest_rejects_tools_using_reserved_borg_prefix() {
        let wasm = minimal_wasm();
        let mut manifest = manifest_for(&wasm);
        manifest.tools[0].input_schema = serde_json::json!({
            "type": "object",
            "properties": {
                "_borg_smuggle": {"type": "string"}
            }
        });
        let err = validate_manifest(&manifest).unwrap_err().to_string();
        assert!(
            err.contains("reserved prefix"),
            "expected reserved-prefix rejection, got: {err}"
        );
    }

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

    #[cfg(unix)]
    #[test]
    fn install_verified_package_sets_0700_on_unix() {
        // The install directory holds verified plugin source bytes that are
        // moral siblings to credentials once paired with a quarantine bypass —
        // we want the same 0700 posture as `~/.borg/secrets/`. Asserting on
        // the post-condition (`mode() & 0o777 == 0o700`) means a future change
        // that drops the chmod call fails this test for a real reason.
        use std::os::unix::fs::PermissionsExt;

        let wasm = minimal_wasm();
        let signing = SigningKey::from_bytes(&[11u8; 32]);
        let mut manifest = manifest_for(&wasm);
        manifest.publisher.public_key = STANDARD.encode(signing.verifying_key().to_bytes());
        let canonical = canonical_manifest_bytes(&manifest).unwrap();
        let sig = STANDARD.encode(signing.sign(&canonical).to_bytes());
        let trusted = vec![manifest.publisher.public_key.clone()];
        let package =
            verify_package(&canonical, &sig, &wasm, &trusted, b"local-key").expect("verify");

        let tmp = tempfile::tempdir().expect("tempdir");
        let installed = install_verified_package(tmp.path(), &package).expect("install");

        let dir = installed.manifest_path.parent().expect("manifest parent");
        let mode = std::fs::metadata(dir)
            .expect("stat install dir")
            .permissions()
            .mode();
        assert_eq!(
            mode & 0o777,
            SECRETS_DIR_MODE,
            "install dir {} has mode {:o}, expected 0o700",
            dir.display(),
            mode & 0o777
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

    /// `classify_execute_error` is built from string-grep fallbacks tied to
    /// literal `bail!` messages in this crate. Drift between the two surfaces
    /// turns rich audit data into a wall of `guest_trap`. Exercise each
    /// non-trap arm against an `anyhow!` whose text matches the production
    /// `bail!` site — anyone renaming an error string will see this test
    /// fail and either update the classifier or pick a different name.
    #[test]
    fn classify_execute_error_covers_string_matched_arms() {
        use anyhow::anyhow;

        let cases: &[(&str, WasmExecuteErrorKind)] = &[
            (
                "growing memory failed: out of pages",
                WasmExecuteErrorKind::MemoryExceeded,
            ),
            (
                "output exceeded 1048576 bytes",
                WasmExecuteErrorKind::OutputTooLarge,
            ),
            (
                "log budget exhausted",
                WasmExecuteErrorKind::LogBytesExceeded,
            ),
            (
                "endpoint not allowed: https://example.com/x",
                WasmExecuteErrorKind::HostHttpDenied,
            ),
            (
                "plugin http secret-leak scan refused the request",
                WasmExecuteErrorKind::HostHttpSecretLeak,
            ),
            (
                "http request failed: dns error",
                WasmExecuteErrorKind::HostHttpUpstream,
            ),
            (
                "undeclared credential 'STRIPE_KEY'",
                WasmExecuteErrorKind::CredentialDenied,
            ),
            (
                "guest returned non-utf-8 output",
                WasmExecuteErrorKind::InvalidGuestOutput,
            ),
            (
                "something the classifier has no opinion about",
                WasmExecuteErrorKind::GuestTrap,
            ),
        ];

        for (msg, expected) in cases {
            let err = anyhow!(*msg);
            let got = classify_execute_error(&err);
            assert_eq!(
                got, *expected,
                "classifier drift: `{msg}` should map to {expected:?}, got {got:?}",
            );
        }
    }
}
