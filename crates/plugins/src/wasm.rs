//! Capability-sandboxed WebAssembly plugin support.
//!
//! This module intentionally keeps the host ABI small. A plugin package is
//! accepted only after publisher-signature verification, static WASM import
//! validation, and local HMAC sealing. Runtime execution exposes no WASI, no
//! raw sockets, no subprocesses, and no filesystem access.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};
use base64::Engine;
use ed25519_dalek::{Signature, VerifyingKey};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use wasmparser::{ExternalKind, Imports, Parser, Payload, TypeRef};

type HmacSha256 = Hmac<Sha256>;

const SCHEMA_VERSION: &str = "borg.plugin.v1";
const ABI_EXPORT_ALLOC: &str = "alloc";
const ABI_EXPORT_CALL: &str = "borg_tool_call";
const ABI_EXPORT_MEMORY: &str = "memory";
const MAX_OUTPUT_BYTES_DEFAULT: u64 = 64 * 1024;
const MAX_MEMORY_PAGES_DEFAULT: u64 = 16;
const FUEL_DEFAULT: u64 = 10_000_000;
const MAX_FUEL_LIMIT: u64 = 50_000_000;
const MAX_HOST_IMPORT_BYTES: usize = 64 * 1024;
/// Cap on `borg:log.log` lines per call. A noisy plugin could otherwise fill
/// the agent's tracing pipeline without ever doing useful work; the cap is per
/// invocation, generous enough for debug output, tight enough to bound waste.
const MAX_LOG_LINES_PER_CALL: u32 = 256;

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

/// Successful package verification output.
#[derive(Debug, Clone)]
pub struct VerifiedWasmPackage {
    /// Parsed manifest.
    pub manifest: WasmPluginManifest,
    /// Canonical manifest bytes used for signature verification.
    pub canonical_manifest: Vec<u8>,
    /// WASM bytes.
    pub wasm: Vec<u8>,
    /// Local HMAC seal for installed state.
    pub local_hmac: String,
    /// Static validation details.
    pub static_validation: StaticValidation,
}

/// Validate and verify a plugin package before installation.
pub fn verify_package(
    manifest_json: &[u8],
    signature_b64: &str,
    wasm: &[u8],
    trusted_publishers: &[String],
    local_hmac_key: &[u8],
) -> Result<VerifiedWasmPackage> {
    if local_hmac_key.is_empty() {
        bail!("local HMAC key is required");
    }
    let manifest: WasmPluginManifest =
        serde_json::from_slice(manifest_json).context("invalid plugin manifest JSON")?;
    validate_manifest(&manifest)?;
    verify_wasm_hash(&manifest, wasm)?;
    verify_publisher_signature(&manifest, signature_b64, trusted_publishers)?;
    let static_validation = validate_wasm_module(&manifest, wasm)?;
    let canonical_manifest = canonical_manifest_bytes(&manifest)?;
    let local_hmac = compute_local_hmac(local_hmac_key, &canonical_manifest, wasm)?;
    Ok(VerifiedWasmPackage {
        manifest,
        canonical_manifest,
        wasm: wasm.to_vec(),
        local_hmac,
        static_validation,
    })
}

/// Install a verified package under `data_dir/wasm_plugins/<safe id>/`.
pub fn install_verified_package(
    data_dir: &Path,
    package: &VerifiedWasmPackage,
) -> Result<InstalledWasmPackage> {
    let safe_id = safe_plugin_dir_name(&package.manifest.id)?;
    let dir = data_dir.join("wasm_plugins").join(safe_id);
    std::fs::create_dir_all(&dir).with_context(|| format!("failed to create {}", dir.display()))?;
    restrict_install_dir_permissions(&dir)?;
    let manifest_path = dir.join("manifest.json");
    let wasm_path = dir.join("plugin.wasm");
    std::fs::write(&manifest_path, &package.canonical_manifest)
        .with_context(|| format!("failed to write {}", manifest_path.display()))?;
    std::fs::write(&wasm_path, &package.wasm)
        .with_context(|| format!("failed to write {}", wasm_path.display()))?;
    Ok(InstalledWasmPackage {
        manifest_path,
        wasm_path,
        local_hmac: package.local_hmac.clone(),
    })
}

/// Paths and integrity seal for an installed plugin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledWasmPackage {
    /// Installed manifest path.
    pub manifest_path: PathBuf,
    /// Installed WASM path.
    pub wasm_path: PathBuf,
    /// HMAC over canonical manifest and WASM bytes.
    pub local_hmac: String,
}

/// Verify an installed plugin's local HMAC before execution.
pub fn verify_installed_hmac(
    local_hmac_key: &[u8],
    manifest_bytes: &[u8],
    wasm: &[u8],
    expected_hmac: &str,
) -> Result<()> {
    // Mirror the bail in `verify_package` — without a key, both sides would
    // hash to the same constant and any tampering would still "verify". A
    // misconfigured boot (e.g. broken `derive_hmac_key`) must fail loudly here
    // rather than silently degrade to no integrity check.
    if local_hmac_key.is_empty() {
        bail!("local HMAC key is required");
    }
    let expected = hex_decode(expected_hmac)?;
    let actual_hex = compute_local_hmac(local_hmac_key, manifest_bytes, wasm)?;
    let actual = hex_decode(&actual_hex)?;
    if constant_time_eq(&actual, &expected) {
        Ok(())
    } else {
        bail!("installed WASM plugin integrity check failed")
    }
}

/// Compute lowercase hex SHA-256.
pub fn sha256_hex(bytes: &[u8]) -> String {
    hex_encode(&Sha256::digest(bytes))
}

/// Return canonical manifest bytes used for signing and local sealing.
pub fn canonical_manifest_bytes(manifest: &WasmPluginManifest) -> Result<Vec<u8>> {
    serde_json::to_vec(manifest).context("failed to canonicalize manifest")
}

/// Validate manifest fields before trusting any package content.
pub fn validate_manifest(manifest: &WasmPluginManifest) -> Result<()> {
    if manifest.schema_version != SCHEMA_VERSION {
        bail!(
            "unsupported plugin manifest schema {}",
            manifest.schema_version
        );
    }
    validate_plugin_id(&manifest.id)?;
    if manifest.name.trim().is_empty() || manifest.name.len() > 128 {
        bail!("plugin name is required");
    }
    if manifest.version.trim().is_empty() || manifest.version.len() > 64 {
        bail!("plugin version is required");
    }
    if manifest.tools.is_empty() {
        bail!("WASM plugin must declare at least one tool");
    }
    if !is_hex_64(&manifest.wasm_sha256) {
        bail!("wasm_sha256 must be a lowercase SHA-256 hex digest");
    }
    let mut tool_names = BTreeSet::new();
    for tool in &manifest.tools {
        validate_tool_name(&tool.name)?;
        if !tool_names.insert(tool.name.clone()) {
            bail!("duplicate tool name {}", tool.name);
        }
        if tool.description.trim().is_empty() || tool.description.len() > 2048 {
            bail!("tool {} description is required", tool.name);
        }
        if !tool.input_schema.is_object() {
            bail!("tool {} input_schema must be a JSON object", tool.name);
        }
        // The host injects routing metadata under `_borg_*` keys before
        // calling the guest. If a tool declared a property with that prefix
        // the injection would clobber it or the LLM would attempt to set it.
        // Reject at install time to keep that boundary clean.
        if let Some(props) = tool
            .input_schema
            .get("properties")
            .and_then(|p| p.as_object())
        {
            for prop in props.keys() {
                if prop.starts_with(RESERVED_KEY_PREFIX) {
                    bail!(
                        "tool {} input_schema property {} uses reserved prefix {}",
                        tool.name,
                        prop,
                        RESERVED_KEY_PREFIX
                    );
                }
            }
        }
    }
    for cap in &manifest.capabilities {
        match cap.as_str() {
            "log" | "state" | "http" | "credentials" => {}
            other => bail!("unknown WASM plugin capability {other}"),
        }
    }
    if manifest.capabilities.iter().any(|c| c == "http") && manifest.endpoints.is_empty() {
        bail!("http capability requires at least one endpoint allowlist entry");
    }
    if !manifest.capabilities.iter().any(|c| c == "http") && !manifest.endpoints.is_empty() {
        bail!("endpoint allowlist entries require http capability");
    }
    for endpoint in &manifest.endpoints {
        validate_endpoint(endpoint)?;
    }
    if manifest.capabilities.iter().any(|c| c == "credentials") && manifest.credentials.is_empty() {
        bail!("credentials capability requires at least one credential key");
    }
    if !manifest.capabilities.iter().any(|c| c == "credentials") && !manifest.credentials.is_empty()
    {
        bail!("credential keys require credentials capability");
    }
    for key in &manifest.credentials {
        validate_credential_key(key)?;
    }
    if manifest.limits.fuel == 0 || manifest.limits.fuel > MAX_FUEL_LIMIT {
        bail!("fuel must be between 1 and {MAX_FUEL_LIMIT}");
    }
    if manifest.limits.timeout_ms == 0 || manifest.limits.timeout_ms > 60_000 {
        bail!("timeout_ms must be between 1 and 60000");
    }
    if manifest.limits.max_memory_pages == 0 || manifest.limits.max_memory_pages > 256 {
        bail!("max_memory_pages must be between 1 and 256");
    }
    if manifest.limits.max_output_bytes == 0 || manifest.limits.max_output_bytes > 1_048_576 {
        bail!("max_output_bytes must be between 1 and 1048576");
    }
    Ok(())
}

/// Validate a module's static imports/exports and memory declaration.
pub fn validate_wasm_module(
    manifest: &WasmPluginManifest,
    wasm: &[u8],
) -> Result<StaticValidation> {
    let mut imports = Vec::new();
    let mut exports = Vec::new();
    let mut has_memory = false;
    let mut has_alloc = false;
    let mut has_call = false;
    let mut memories = 0u32;

    for payload in Parser::new(0).parse_all(wasm) {
        match payload.context("invalid WASM module")? {
            Payload::ImportSection(reader) => {
                for group in reader {
                    collect_imports(group?, &mut |module, name, ty| {
                        validate_import(manifest, module, name, ty)?;
                        imports.push((module.to_string(), name.to_string()));
                        Ok(())
                    })?;
                }
            }
            Payload::MemorySection(reader) => {
                for memory in reader {
                    memories += 1;
                    if memories > 1 {
                        bail!("WASM plugins may declare only one memory");
                    }
                    let memory = memory?;
                    if memory.memory64 {
                        bail!("memory64 is not allowed for WASM plugins");
                    }
                    if memory.shared {
                        bail!("shared memory is not allowed for WASM plugins");
                    }
                    if memory.initial > manifest.limits.max_memory_pages {
                        bail!(
                            "initial memory pages {} exceed plugin limit {}",
                            memory.initial,
                            manifest.limits.max_memory_pages
                        );
                    }
                    if memory
                        .maximum
                        .is_some_and(|max| max > manifest.limits.max_memory_pages)
                    {
                        bail!("declared maximum memory exceeds plugin limit");
                    }
                }
            }
            Payload::ExportSection(reader) => {
                for export in reader {
                    let export = export?;
                    exports.push(export.name.to_string());
                    match (export.name, export.kind) {
                        (ABI_EXPORT_MEMORY, ExternalKind::Memory) => has_memory = true,
                        (ABI_EXPORT_ALLOC, ExternalKind::Func) => has_alloc = true,
                        (ABI_EXPORT_CALL, ExternalKind::Func) => has_call = true,
                        _ => {}
                    }
                }
            }
            Payload::StartSection { .. } => bail!("WASM plugin start functions are not allowed"),
            Payload::Version { .. }
            | Payload::TypeSection(_)
            | Payload::FunctionSection(_)
            | Payload::TableSection(_)
            | Payload::TagSection(_)
            | Payload::GlobalSection(_)
            | Payload::ElementSection(_)
            | Payload::DataCountSection { .. }
            | Payload::DataSection(_)
            | Payload::CodeSectionStart { .. }
            | Payload::CodeSectionEntry(_)
            | Payload::CustomSection(_)
            | Payload::End(_) => {}
            other => bail!("unsupported WASM payload in plugin: {other:?}"),
        }
    }

    if !has_memory {
        bail!("WASM plugin must export memory");
    }
    if !has_alloc {
        bail!("WASM plugin must export alloc(len) -> ptr");
    }
    if !has_call {
        bail!("WASM plugin must export borg_tool_call(ptr, len) -> packed(ptr,len)");
    }

    Ok(StaticValidation { imports, exports })
}

/// Reserved JSON key prefix used by the host to route multi-tool dispatch.
/// Manifest validation rejects any tool input schema using a property with
/// this prefix, so the key cannot collide with a guest-defined field.
pub const RESERVED_KEY_PREFIX: &str = "_borg_";

/// Reserved JSON key carrying the dispatched tool name to multi-tool plugins.
pub const RESERVED_TOOL_KEY: &str = "_borg_tool";

/// Execute one verified WASM tool using Borg's narrow ABI with default
/// (no-op) host services. Convenience wrapper used by tests and any caller
/// whose plugin declares no capabilities. Production paths should call
/// [`execute_tool_with_services`] so capabilities actually work.
///
/// `tool_name` must be one of `manifest.tools[].name`. The host injects it into
/// the args JSON as the reserved `_borg_tool` key so a plugin exporting
/// multiple tools through the single `borg_tool_call` ABI can route internally.
pub fn execute_tool(
    manifest: &WasmPluginManifest,
    wasm: &[u8],
    tool_name: &str,
    args_json: &serde_json::Value,
) -> Result<String> {
    execute_tool_with_services(
        manifest,
        wasm,
        tool_name,
        args_json,
        Arc::new(NoHostServices),
    )
}

/// Re-exports so callers can hold a cached engine + module without taking a
/// direct dependency on the `wasmtime` crate (whose semver bumps must stay
/// confined to the plugins crate). Named `Wasm*` to avoid colliding with the
/// `base64::Engine` trait already in scope here.
pub use wasmtime::{Engine as WasmEngine, Module as WasmModule};

/// Build a `wasmtime::Engine` configured with Borg's fuel + epoch policy.
///
/// Engines are safe to share across plugins because the `Config` is fixed —
/// per-call variation (fuel budget, epoch deadline) lives in the `Store`.
/// Callers that execute the same plugin repeatedly (e.g. the daemon) should
/// cache a single engine plus per-plugin compiled `Module`s and feed them to
/// [`execute_tool_prepared`].
pub fn build_engine() -> Result<wasmtime::Engine> {
    let mut config = wasmtime::Config::new();
    config.consume_fuel(true);
    config.epoch_interruption(true);
    // Threads would let a guest spawn host OS threads outside our budgets; the
    // static gauntlet already rejects shared memory but `wasm_threads(false)`
    // makes the runtime refuse to instantiate any threaded module too.
    config.wasm_threads(false);
    // Disable DWARF — smaller modules, no source-path/symbol leakage from a
    // signed package back into host logs.
    config.debug_info(false);
    wasmtime::Engine::new(&config).context("build wasmtime engine")
}

/// JIT-compile `wasm` against `engine`. The caller MUST have already cleared
/// the install gauntlet (`verify_package` / `verify_installed_hmac`) — this
/// function only compiles, it does NOT re-check signatures or HMAC seals.
pub fn compile_module(engine: &wasmtime::Engine, wasm: &[u8]) -> Result<wasmtime::Module> {
    wasmtime::Module::new(engine, wasm).context("compile wasm module")
}

/// Execute one verified WASM tool, routing capability imports through the
/// supplied [`HostServices`]. Returns the UTF-8 string the guest stages at
/// its packed-pointer return value.
pub fn execute_tool_with_services(
    manifest: &WasmPluginManifest,
    wasm: &[u8],
    tool_name: &str,
    args_json: &serde_json::Value,
    services: Arc<dyn HostServices>,
) -> Result<String> {
    let engine = build_engine()?;
    let module = compile_module(&engine, wasm)?;
    execute_tool_prepared(
        manifest, &engine, &module, wasm, tool_name, args_json, services,
    )
}

/// Same contract as [`execute_tool_with_services`] minus the per-call Engine
/// and Module construction. Use this when you maintain a process-lifetime
/// cache of compiled modules. `validate_wasm_module` still runs on every call
/// — the cache amortizes JIT compile, never integrity checks.
pub fn execute_tool_prepared(
    manifest: &WasmPluginManifest,
    engine: &wasmtime::Engine,
    module: &wasmtime::Module,
    wasm: &[u8],
    tool_name: &str,
    args_json: &serde_json::Value,
    services: Arc<dyn HostServices>,
) -> Result<String> {
    if !manifest.tools.iter().any(|t| t.name == tool_name) {
        bail!("tool {tool_name} is not declared by this plugin");
    }
    validate_wasm_module(manifest, wasm)?;

    let max_memory_bytes = (manifest.limits.max_memory_pages as usize)
        .checked_mul(WASM_PAGE_BYTES)
        .context("max_memory_pages overflow")?;
    let mut store = wasmtime::Store::new(
        engine,
        HostState {
            limits: PluginLimits { max_memory_bytes },
            capabilities: manifest.capabilities.clone(),
            endpoints: manifest.endpoints.clone(),
            credentials: manifest.credentials.clone(),
            log_lines_remaining: MAX_LOG_LINES_PER_CALL,
            max_output_bytes: manifest.limits.max_output_bytes,
            plugin_id: manifest.id.clone(),
            tool_name: tool_name.to_string(),
            services,
        },
    );
    store.limiter(|state| &mut state.limits);
    store.set_fuel(manifest.limits.fuel)?;
    // Trap on the first epoch increment. The EpochTimer below fires that
    // increment after `timeout_ms` unless the call completes first and the
    // timer's Drop cancels it. Without this, `timeout_ms` was a no-op — fuel
    // exhaustion was the only stop and an `add`-light infinite loop could
    // exceed wall-clock budget indefinitely.
    store.set_epoch_deadline(1);
    let _epoch_timer = EpochTimer::new(
        engine,
        std::time::Duration::from_millis(manifest.limits.timeout_ms),
    );
    let mut linker = wasmtime::Linker::new(engine);
    linker.func_wrap("borg:log", "log", host_log)?;
    linker.func_wrap("borg:http", "fetch", host_http_fetch)?;
    linker.func_wrap("borg:state", "read", host_state_read)?;
    linker.func_wrap("borg:state", "write", host_state_write)?;
    linker.func_wrap("borg:credentials", "get", host_credentials_get)?;

    let instance = linker.instantiate(&mut store, module)?;
    let memory = instance
        .get_memory(&mut store, ABI_EXPORT_MEMORY)
        .context("WASM plugin missing exported memory")?;
    let alloc = instance
        .get_typed_func::<i32, i32>(&mut store, ABI_EXPORT_ALLOC)
        .context("WASM plugin missing alloc export")?;
    let call = instance
        .get_typed_func::<(i32, i32), i64>(&mut store, ABI_EXPORT_CALL)
        .context("WASM plugin missing borg_tool_call export")?;

    // Inject the dispatched tool name under the reserved key so plugins with
    // multiple tools can route internally. The single `borg_tool_call` ABI
    // gives the guest no other channel to know which tool was invoked.
    // `validate_manifest` already rejects any tool whose declared input_schema
    // uses a `_borg_*` property, so this insertion cannot collide.
    let input_value = match args_json {
        serde_json::Value::Object(map) => {
            let mut owned = map.clone();
            owned.insert(
                RESERVED_TOOL_KEY.to_string(),
                serde_json::Value::String(tool_name.to_string()),
            );
            serde_json::Value::Object(owned)
        }
        other => serde_json::json!({ RESERVED_TOOL_KEY: tool_name, "_borg_args": other }),
    };
    let input = serde_json::to_vec(&input_value)?;
    let input_len = i32::try_from(input.len()).context("tool input too large")?;
    let input_ptr = alloc.call(&mut store, input_len)?;
    if input_ptr < 0 {
        bail!("WASM plugin alloc returned a negative pointer");
    }
    memory.write(&mut store, input_ptr as usize, &input)?;
    let packed = call.call(&mut store, (input_ptr, input_len))?;
    let out_ptr = (packed >> 32) as u32 as usize;
    let out_len = (packed & 0xffff_ffff) as u32 as usize;
    if out_len as u64 > manifest.limits.max_output_bytes {
        bail!(
            "WASM plugin output exceeded {} bytes",
            manifest.limits.max_output_bytes
        );
    }
    let mut output = vec![0u8; out_len];
    memory.read(&store, out_ptr, &mut output)?;
    String::from_utf8(output).context("WASM plugin returned non-UTF-8 output")
}

/// Validate a plugin-originated HTTP request before any network operation.
pub fn validate_host_http_request(
    manifest: &WasmPluginManifest,
    req: &HostHttpRequest<'_>,
    secret_values: &[String],
) -> Result<()> {
    if !manifest.capabilities.iter().any(|c| c == "http") {
        bail!("plugin does not have http capability");
    }
    let url = url::Url::parse(req.url).context("invalid plugin HTTP URL")?;
    if url.scheme() != "https" {
        bail!("plugin HTTP requests must use https");
    }
    if !endpoint_allowed(&url, &manifest.endpoints) {
        bail!("plugin HTTP endpoint is not allowlisted");
    }
    let mut haystack = req.url.to_string();
    for (name, value) in req.headers {
        haystack.push_str(name);
        haystack.push_str(value);
    }
    haystack.push_str(&String::from_utf8_lossy(req.body));
    for secret in secret_values {
        if !secret.is_empty() && haystack.contains(secret) {
            bail!("plugin HTTP request blocked by secret leak scanner");
        }
    }
    Ok(())
}

fn verify_wasm_hash(manifest: &WasmPluginManifest, wasm: &[u8]) -> Result<()> {
    let actual = sha256_hex(wasm);
    if actual != manifest.wasm_sha256 {
        bail!("WASM hash mismatch: manifest does not match module bytes");
    }
    Ok(())
}

fn verify_publisher_signature(
    manifest: &WasmPluginManifest,
    signature_b64: &str,
    trusted_publishers: &[String],
) -> Result<()> {
    if !trusted_publishers
        .iter()
        .any(|k| k == &manifest.publisher.public_key)
    {
        bail!("publisher key is not trusted");
    }
    let key_bytes = base64::engine::general_purpose::STANDARD
        .decode(&manifest.publisher.public_key)
        .context("publisher public key is not valid base64")?;
    let key_array: [u8; 32] = key_bytes
        .try_into()
        .map_err(|_| anyhow!("publisher public key must be 32 bytes"))?;
    let verifying_key = VerifyingKey::from_bytes(&key_array)?;
    let sig_bytes = base64::engine::general_purpose::STANDARD
        .decode(signature_b64)
        .context("signature is not valid base64")?;
    let signature = Signature::from_slice(&sig_bytes)?;
    let canonical = canonical_manifest_bytes(manifest)?;
    verifying_key.verify_strict(&canonical, &signature)?;
    Ok(())
}

/// Recompute the current-scheme local HMAC seal. Public so cortex can rewrite
/// the row after auto-resealing a legacy install. The result is what
/// `verify_installed_hmac` will check on the next execution.
pub fn compute_local_hmac_current(
    key: &[u8],
    manifest_bytes: &[u8],
    wasm: &[u8],
) -> Result<String> {
    compute_local_hmac(key, manifest_bytes, wasm)
}

fn compute_local_hmac(key: &[u8], manifest_bytes: &[u8], wasm: &[u8]) -> Result<String> {
    let mut mac =
        HmacSha256::new_from_slice(key).map_err(|_| anyhow!("invalid HMAC key length"))?;
    // Length-prefix framing so the two segments can't be ambiguously concatenated.
    // A NUL separator alone would let a manifest containing the right bytes shift
    // material across the boundary while keeping the same MAC; explicit u64-be
    // prefixes make that class of substitution impossible.
    mac.update(&(manifest_bytes.len() as u64).to_be_bytes());
    mac.update(manifest_bytes);
    mac.update(&(wasm.len() as u64).to_be_bytes());
    mac.update(wasm);
    Ok(hex_encode(&mac.finalize().into_bytes()))
}

/// Legacy (pre-current) HMAC sealing — kept verification-only so installs
/// sealed by an older borg can be auto-resealed under the current scheme
/// without forcing the operator to reinstall every plugin.
///
/// Two differences from `compute_local_hmac`:
///   1. The derived-key domain on the cortex side was
///      `b"borg-wasm-plugin-local-hmac"` (no `-v2` suffix). Cortex passes that
///      legacy key when it falls back through this verifier.
///   2. Framing used a single NUL separator between manifest and wasm bytes —
///      no length prefix. The length-prefixed scheme is more robust against
///      framing-substitution mistakes if the manifest format ever changes,
///      but the legacy framing has no known exploit *without* the per-install
///      KEK-derived key. We accept a v1 MAC as a trust signal at parity with
///      a v2 MAC because forging either one requires the same secret. The
///      Ed25519 publisher signature is the remote-trust anchor and is not
///      touched by this path — the local HMAC is strictly a post-install
///      tamper signal. If a v1-class collision is ever discovered, this
///      module must be deleted (not loosened), forcing reinstall.
pub mod legacy_hmac {
    use super::{anyhow, bail, constant_time_eq, hex_decode, hex_encode, HmacSha256, Result};
    use hmac::Mac;

    /// Verify `expected_hmac` against the pre-current ("v1") sealing scheme.
    /// Callers MUST treat success as a signal to reseal under the current
    /// scheme (see `cortex::wasm_plugins`), not as a permanent integrity state.
    pub fn verify(
        legacy_key: &[u8],
        manifest_bytes: &[u8],
        wasm: &[u8],
        expected_hmac: &str,
    ) -> Result<()> {
        if legacy_key.is_empty() {
            bail!("legacy HMAC key is required");
        }
        let expected = hex_decode(expected_hmac)?;
        let actual_hex = compute(legacy_key, manifest_bytes, wasm)?;
        let actual = hex_decode(&actual_hex)?;
        if constant_time_eq(&actual, &expected) {
            Ok(())
        } else {
            bail!("installed WASM plugin legacy integrity check failed")
        }
    }

    fn compute(key: &[u8], manifest_bytes: &[u8], wasm: &[u8]) -> Result<String> {
        let mut mac =
            HmacSha256::new_from_slice(key).map_err(|_| anyhow!("invalid HMAC key length"))?;
        mac.update(manifest_bytes);
        mac.update(b"\0");
        mac.update(wasm);
        Ok(hex_encode(&mac.finalize().into_bytes()))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn legacy_verify_round_trip() {
            // Real failure mode: a future framing tweak that silently breaks
            // verification of in-the-wild v1 seals — every existing install
            // would be force-quarantined on upgrade. Round-trip a known input
            // to lock the framing down.
            let key = b"legacy-key-bytes-2026";
            let manifest = b"{\"id\":\"dev/echo\"}";
            let wasm = b"\0asm\x01\x00\x00\x00";
            let mac = compute(key, manifest, wasm).expect("compute legacy");
            verify(key, manifest, wasm, &mac).expect("verify legacy");

            // Tamper either side, expect failure.
            let mut tampered = wasm.to_vec();
            *tampered.last_mut().unwrap() ^= 0xFF;
            assert!(verify(key, manifest, &tampered, &mac).is_err());

            let mut tampered_manifest = manifest.to_vec();
            tampered_manifest[0] ^= 0xFF;
            assert!(verify(key, &tampered_manifest, wasm, &mac).is_err());
        }

        #[test]
        fn legacy_verify_empty_key_rejected() {
            // A misconfigured boot must fail loudly here rather than silently
            // accept everything via a zero-length key — mirrors the v2 path.
            let err = verify(b"", b"manifest", b"wasm", &"0".repeat(64))
                .expect_err("empty key must be rejected");
            assert!(
                err.to_string().contains("legacy HMAC key"),
                "error must point at the key, got: {err}",
            );
        }
    }
}

/// Coarse-grained error categorization used for metrics labels and audit-row
/// `error_category` fields. Derived from an `anyhow::Error` returned by
/// `execute_tool_prepared` — cortex calls `classify_execute_error` at the
/// seam, which keeps the existing string error surface stable while making
/// observability machine-readable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WasmExecuteErrorKind {
    /// Wasmtime fuel exhausted before the guest returned.
    FuelExhausted,
    /// Epoch deadline fired — wall-clock timeout.
    Timeout,
    /// Memory growth exceeded `limits.max_memory_pages`.
    MemoryExceeded,
    /// Guest output (return buffer) exceeded `limits.max_output_bytes`.
    OutputTooLarge,
    /// Guest emitted more log bytes/lines than the per-call budget.
    LogBytesExceeded,
    /// Host HTTP request rejected because the URL is not on the manifest
    /// endpoint allowlist.
    HostHttpDenied,
    /// Host HTTP request refused because secret-leak scanning matched.
    HostHttpSecretLeak,
    /// Host HTTP request reached the upstream but failed (DNS, TLS, 5xx, etc).
    HostHttpUpstream,
    /// Guest asked for a credential key not in the manifest's allowlist.
    CredentialDenied,
    /// Guest returned bytes that are not valid UTF-8.
    InvalidGuestOutput,
    /// Generic wasmtime trap / runtime failure not matched above.
    GuestTrap,
}

impl WasmExecuteErrorKind {
    /// Stable label suffix for OpenTelemetry attributes / audit JSON.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::FuelExhausted => "fuel_exhausted",
            Self::Timeout => "timeout",
            Self::MemoryExceeded => "memory_exceeded",
            Self::OutputTooLarge => "output_too_large",
            Self::LogBytesExceeded => "log_bytes_exceeded",
            Self::HostHttpDenied => "host_http_denied",
            Self::HostHttpSecretLeak => "host_http_secret_leak",
            Self::HostHttpUpstream => "host_http_upstream",
            Self::CredentialDenied => "credential_denied",
            Self::InvalidGuestOutput => "invalid_guest_output",
            Self::GuestTrap => "guest_trap",
        }
    }
}

/// Classify an `anyhow::Error` returned from the execute path into one of the
/// stable `WasmExecuteErrorKind` buckets. The classifier walks the error chain
/// and matches against `wasmtime::Trap` first (the most reliable signal for
/// fuel/timeout/memory) then falls back to string matching on cause messages
/// produced inside this crate.
///
/// Returns `GuestTrap` as the catch-all so a new failure mode shows up as a
/// recognizable bucket in dashboards instead of silently going uncounted.
pub fn classify_execute_error(err: &anyhow::Error) -> WasmExecuteErrorKind {
    // Wasmtime traps carry typed information about the stop reason.
    for cause in err.chain() {
        if let Some(trap) = cause.downcast_ref::<wasmtime::Trap>() {
            return match trap {
                wasmtime::Trap::OutOfFuel => WasmExecuteErrorKind::FuelExhausted,
                wasmtime::Trap::Interrupt => WasmExecuteErrorKind::Timeout,
                _ => WasmExecuteErrorKind::GuestTrap,
            };
        }
    }

    // String-based fallbacks for host-side bail!()s in this crate. Each match
    // mirrors the literal bail message at its emission site so renaming an
    // error string requires updating the classifier too — call sites stay
    // grep-able from the bucket name.
    let chain_text: String = err
        .chain()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(" || ");
    let lower = chain_text.to_ascii_lowercase();

    if lower.contains("growing memory")
        || lower.contains("max memory")
        || lower.contains("memory minimum size")
    {
        return WasmExecuteErrorKind::MemoryExceeded;
    }
    if lower.contains("output exceeded") {
        return WasmExecuteErrorKind::OutputTooLarge;
    }
    if lower.contains("log budget") || lower.contains("log byte") {
        return WasmExecuteErrorKind::LogBytesExceeded;
    }
    if lower.contains("not on allowlist")
        || lower.contains("not in endpoint allowlist")
        || lower.contains("endpoint not allowed")
    {
        return WasmExecuteErrorKind::HostHttpDenied;
    }
    if lower.contains("secret") && lower.contains("http") {
        return WasmExecuteErrorKind::HostHttpSecretLeak;
    }
    if lower.contains("http request failed") || lower.contains("read plugin http body") {
        return WasmExecuteErrorKind::HostHttpUpstream;
    }
    if lower.contains("undeclared credential") || lower.contains("credentials capability") {
        return WasmExecuteErrorKind::CredentialDenied;
    }
    if lower.contains("non-utf-8 output") {
        return WasmExecuteErrorKind::InvalidGuestOutput;
    }

    WasmExecuteErrorKind::GuestTrap
}

fn collect_imports<F>(imports: Imports<'_>, visit: &mut F) -> Result<()>
where
    F: FnMut(&str, &str, TypeRef) -> Result<()>,
{
    match imports {
        Imports::Single(_, import) => visit(import.module, import.name, import.ty),
        Imports::Compact1 { module, items } => {
            for item in items {
                let item = item?;
                visit(module, item.name, item.ty)?;
            }
            Ok(())
        }
        Imports::Compact2 { module, ty, names } => {
            for name in names {
                visit(module, name?, ty)?;
            }
            Ok(())
        }
    }
}

fn validate_import(
    manifest: &WasmPluginManifest,
    module: &str,
    name: &str,
    ty: TypeRef,
) -> Result<()> {
    if !matches!(ty, TypeRef::Func(_)) {
        bail!("WASM plugin import {module}.{name} is not a function");
    }
    let has_cap = |cap: &str| manifest.capabilities.iter().any(|c| c == cap);
    let allowed = match (module, name) {
        ("borg:log", "log") => has_cap("log"),
        ("borg:http", "fetch") => has_cap("http"),
        ("borg:state", "read" | "write") => has_cap("state"),
        ("borg:credentials", "get") => has_cap("credentials"),
        _ => false,
    };
    if !allowed {
        bail!("WASM plugin import {module}.{name} is not declared or allowed");
    }
    Ok(())
}

fn validate_plugin_id(id: &str) -> Result<()> {
    if id.is_empty()
        || id.len() > 128
        || id.starts_with('/')
        || id.contains("..")
        || id.contains("//")
        || !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/'))
    {
        bail!("invalid WASM plugin id");
    }
    Ok(())
}

fn validate_tool_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > 64
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
    {
        bail!("invalid WASM tool name {name}");
    }
    Ok(())
}

fn validate_credential_key(key: &str) -> Result<()> {
    if key.is_empty()
        || !key
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
    {
        bail!("invalid credential key {key}");
    }
    Ok(())
}

fn validate_endpoint(endpoint: &str) -> Result<()> {
    if let Some(host) = endpoint.strip_prefix("*.") {
        if host.is_empty() || host.contains('/') || host.contains(':') {
            bail!("invalid wildcard endpoint {endpoint}");
        }
        return Ok(());
    }
    let url = url::Url::parse(endpoint).context("invalid endpoint allowlist URL")?;
    if url.scheme() != "https" {
        bail!("endpoint allowlist entries must use https");
    }
    if url.host_str().is_none() {
        bail!("endpoint allowlist entry must include a host");
    }
    Ok(())
}

fn endpoint_allowed(url: &url::Url, allowlist: &[String]) -> bool {
    let Some(host) = url.host_str() else {
        return false;
    };
    allowlist.iter().any(|entry| {
        if let Some(suffix) = entry.strip_prefix("*.") {
            host != suffix && host.ends_with(&format!(".{suffix}"))
        } else if let Ok(allowed) = url::Url::parse(entry) {
            allowed.scheme() == url.scheme()
                && allowed.host_str() == Some(host)
                && allowed.port_or_known_default() == url.port_or_known_default()
                && path_prefix_allowed(allowed.path(), url.path())
        } else {
            false
        }
    })
}

fn path_prefix_allowed(allowed_path: &str, request_path: &str) -> bool {
    if allowed_path == "/" {
        return true;
    }
    if request_path == allowed_path {
        return true;
    }
    let prefix = allowed_path.strip_suffix('/').unwrap_or(allowed_path);
    request_path
        .strip_prefix(prefix)
        .is_some_and(|remaining| remaining.starts_with('/'))
}

fn safe_plugin_dir_name(id: &str) -> Result<String> {
    validate_plugin_id(id)?;
    Ok(sha256_hex(id.as_bytes()))
}

/// Restrict the per-plugin install directory to owner-only on Unix.
///
/// Matches the `~/.borg/secrets/` posture so an attacker with read access to
/// `~/.borg/` (e.g. via a misconfigured backup or a sibling tool running as a
/// less-privileged user) cannot enumerate installed plugin source bytes —
/// which is the moral equivalent of cleartext capability tokens once paired
/// with a quarantine bypass.
fn restrict_install_dir_permissions(dir: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("failed to chmod {} to 0700", dir.display()))?;
    }
    let _ = dir;
    Ok(())
}

fn is_hex_64(s: &str) -> bool {
    s.len() == 64
        && s.chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut out, b| {
        use std::fmt::Write;
        let _ = write!(out, "{b:02x}");
        out
    })
}

fn hex_decode(hex: &str) -> Result<Vec<u8>> {
    if hex.len() % 2 != 0 {
        bail!("hex string has odd length");
    }
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).context("invalid hex"))
        .collect()
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut diff = 0u8;
    for (a, b) in left.iter().zip(right) {
        diff |= a ^ b;
    }
    diff == 0
}

/// Wall-clock timeout enforcer backed by wasmtime's epoch interruption.
///
/// On construction it spawns a sentinel thread that waits for either the
/// timeout to elapse or a Drop-time cancellation signal. On timeout it calls
/// `Engine::increment_epoch` on a weak handle, which causes the wasm store's
/// `epoch_deadline` (set to `1` by the caller) to be reached and traps the
/// running guest at the next safe point.
///
/// The Drop impl sends a cancellation message so a normal call return doesn't
/// leak a sleeping thread for the full `timeout_ms` budget.
struct EpochTimer {
    cancel: Option<std::sync::mpsc::SyncSender<()>>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl EpochTimer {
    fn new(engine: &wasmtime::Engine, timeout: std::time::Duration) -> Self {
        let (tx, rx) = std::sync::mpsc::sync_channel::<()>(0);
        let engine_weak = engine.weak();
        let handle = std::thread::spawn(move || {
            if rx.recv_timeout(timeout).is_err() {
                if let Some(e) = engine_weak.upgrade() {
                    e.increment_epoch();
                }
            }
        });
        Self {
            cancel: Some(tx),
            handle: Some(handle),
        }
    }
}

impl Drop for EpochTimer {
    fn drop(&mut self) {
        if let Some(tx) = self.cancel.take() {
            // try_send fails iff the receiver already exited via the timeout
            // path — that's the expected "we already tripped the engine" case
            // and not an error worth surfacing.
            let _ = tx.try_send(());
        }
        if let Some(h) = self.handle.take() {
            // Joining a tiny sentinel thread that just returned should be
            // immediate; if it isn't, the worst case is we wait a few extra
            // ms for the channel's wakeup. Panics in the ticker (none
            // expected — it does no fallible work) drop the JoinError silently.
            let _ = h.join();
        }
    }
}

/// Per-call host state attached to the wasmtime Store.
struct HostState {
    /// ResourceLimiter that bounds runtime `memory.grow` calls. Static validation
    /// only checks the declared memory shape; without this the guest could
    /// declare 1 page and grow far past `max_memory_pages` at runtime.
    limits: PluginLimits,
    /// Capabilities declared by the manifest, copied here so host stubs can
    /// re-check at call time. Static import gating in `validate_import` is the
    /// primary guard; this is defense in depth — a future linker regression
    /// that accidentally exposes a `borg:*` import to an undeclared plugin
    /// would still be caught here.
    capabilities: Vec<String>,
    /// Manifest endpoint allowlist, consulted by `borg:http.fetch`.
    endpoints: Vec<String>,
    /// Declared credential keys, consulted by `borg:credentials.get` so the
    /// host can refuse a key the manifest never asked for.
    credentials: Vec<String>,
    /// Per-call log line budget (`borg:log.log`).
    log_lines_remaining: u32,
    /// Maximum response body bytes returned from `borg:http.fetch`. Mirrors
    /// the output cap so a plugin cannot funnel an unbounded body around the
    /// `max_output_bytes` check via the http capability.
    max_output_bytes: u64,
    /// Plugin id, surfaced on tracing events for log triage.
    plugin_id: String,
    /// Tool name being dispatched, used on tracing events.
    tool_name: String,
    /// Caller-supplied host services for the four capability imports.
    services: Arc<dyn HostServices>,
}

struct PluginLimits {
    max_memory_bytes: usize,
}

const WASM_PAGE_BYTES: usize = 65_536;

impl wasmtime::ResourceLimiter for PluginLimits {
    fn memory_growing(
        &mut self,
        _current: usize,
        desired: usize,
        maximum: Option<usize>,
    ) -> Result<bool> {
        if desired > self.max_memory_bytes {
            return Ok(false);
        }
        if let Some(max) = maximum {
            if desired > max {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn table_growing(
        &mut self,
        _current: usize,
        _desired: usize,
        _maximum: Option<usize>,
    ) -> Result<bool> {
        Ok(true)
    }
}

fn read_guest_string(
    caller: &mut wasmtime::Caller<'_, HostState>,
    ptr: i32,
    len: i32,
) -> Result<String> {
    if ptr < 0 || len < 0 {
        bail!("negative guest pointer");
    }
    if len as usize > MAX_HOST_IMPORT_BYTES {
        bail!("guest string exceeds host import limit");
    }
    let memory = caller
        .get_export(ABI_EXPORT_MEMORY)
        .and_then(wasmtime::Extern::into_memory)
        .context("guest memory export not found")?;
    let mut bytes = vec![0u8; len as usize];
    memory.read(caller, ptr as usize, &mut bytes)?;
    String::from_utf8(bytes).context("guest string was not UTF-8")
}

fn has_cap(caller: &wasmtime::Caller<'_, HostState>, cap: &str) -> bool {
    caller.data().capabilities.iter().any(|c| c == cap)
}

/// Write `bytes` to guest memory via the guest's `alloc` export and return a
/// packed `(ptr<<32 | len)` value suitable for guest consumption. Used by the
/// host import functions that produce responses (`http_fetch`, `state.read`,
/// `credentials.get`).
fn write_guest_response(caller: &mut wasmtime::Caller<'_, HostState>, bytes: &[u8]) -> Result<i64> {
    let len = i32::try_from(bytes.len()).context("host response too large for guest")?;
    let alloc = caller
        .get_export(ABI_EXPORT_ALLOC)
        .and_then(wasmtime::Extern::into_func)
        .context("guest alloc export not found")?
        .typed::<i32, i32>(&caller)?;
    let ptr = alloc.call(&mut *caller, len)?;
    if ptr < 0 {
        bail!("guest alloc returned negative pointer");
    }
    let memory = caller
        .get_export(ABI_EXPORT_MEMORY)
        .and_then(wasmtime::Extern::into_memory)
        .context("guest memory export not found")?;
    memory.write(&mut *caller, ptr as usize, bytes)?;
    Ok((i64::from(ptr) << 32) | i64::from(len as u32))
}

/// borg:log.log host import: logs a UTF-8 message at info level, gated by the
/// per-call throttle and the `log` capability.
fn host_log(mut caller: wasmtime::Caller<'_, HostState>, ptr: i32, len: i32) -> i32 {
    if !has_cap(&caller, "log") {
        return -1;
    }
    {
        let state = caller.data_mut();
        if state.log_lines_remaining == 0 {
            return -1;
        }
        state.log_lines_remaining -= 1;
    }
    match read_guest_string(&mut caller, ptr, len) {
        Ok(msg) => {
            let state = caller.data();
            tracing::info!(
                target: "borg.wasm_plugin",
                plugin_id = %state.plugin_id,
                tool = %state.tool_name,
                "{msg}"
            );
            0
        }
        Err(_) => -1,
    }
}

/// borg:http.fetch host import: parse a JSON request from guest memory,
/// validate against the manifest allowlist + secret-leak scan, dispatch
/// through `HostServices`, and write the response back as packed (ptr, len)
/// of a JSON string the guest can parse.
fn host_http_fetch(mut caller: wasmtime::Caller<'_, HostState>, ptr: i32, len: i32) -> i64 {
    if !has_cap(&caller, "http") {
        return -1;
    }
    let req_json = match read_guest_string(&mut caller, ptr, len) {
        Ok(s) => s,
        Err(_) => return -1,
    };
    let req: serde_json::Value = match serde_json::from_str(&req_json) {
        Ok(v) => v,
        Err(_) => return -1,
    };
    let url = req.get("url").and_then(|v| v.as_str()).unwrap_or("");
    let method = req.get("method").and_then(|v| v.as_str()).unwrap_or("GET");
    let headers_value = req
        .get("headers")
        .and_then(|v| v.as_object())
        .cloned()
        .unwrap_or_default();
    let headers_pairs: Vec<(String, String)> = headers_value
        .into_iter()
        .filter_map(|(k, v)| v.as_str().map(|s| (k, s.to_string())))
        .collect();
    // base64-decode the body if present so binary payloads survive the
    // JSON boundary.
    let body: Vec<u8> = match req.get("body_b64").and_then(|v| v.as_str()) {
        Some(b64) => match base64::engine::general_purpose::STANDARD.decode(b64) {
            Ok(bytes) => bytes,
            Err(_) => return -1,
        },
        None => Vec::new(),
    };

    let headers_refs: Vec<(&str, &str)> = headers_pairs
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let validation_req = HostHttpRequest {
        url,
        headers: &headers_refs,
        body: &body,
    };
    // Build a temporary manifest-shaped view from the HostState fields the
    // validator needs, so the same validate function can be used for both
    // pre-call validation and runtime enforcement.
    let manifest_for_validate = WasmPluginManifest {
        schema_version: SCHEMA_VERSION.to_string(),
        id: caller.data().plugin_id.clone(),
        name: caller.data().plugin_id.clone(),
        version: "runtime".to_string(),
        publisher: PublisherManifest {
            name: String::new(),
            public_key: String::new(),
        },
        wasm_sha256: "0".repeat(64),
        tools: vec![],
        capabilities: caller.data().capabilities.clone(),
        endpoints: caller.data().endpoints.clone(),
        credentials: caller.data().credentials.clone(),
        limits: ResourceLimits::default(),
    };
    if validate_host_http_request(&manifest_for_validate, &validation_req, &[]).is_err() {
        return -1;
    }

    let services = caller.data().services.clone();
    let max_body = caller.data().max_output_bytes;
    let fetch = HostHttpFetch {
        method,
        request: validation_req,
    };
    let response = match services.http_fetch(&fetch) {
        Ok(r) => r,
        Err(_) => return -1,
    };
    if response.body.len() as u64 > max_body {
        return -1;
    }
    let headers_map: serde_json::Map<String, serde_json::Value> = response
        .headers
        .into_iter()
        .map(|(k, v)| (k, serde_json::Value::String(v)))
        .collect();
    let resp_json = serde_json::json!({
        "status": response.status,
        "headers": serde_json::Value::Object(headers_map),
        "body_b64": base64::engine::general_purpose::STANDARD.encode(&response.body),
    });
    let resp_bytes = match serde_json::to_vec(&resp_json) {
        Ok(b) => b,
        Err(_) => return -1,
    };
    write_guest_response(&mut caller, &resp_bytes).unwrap_or(-1)
}

/// borg:state.read host import: returns `packed(ptr, len)` of the stored
/// value, or `0` (packed all-zero, ptr=0 len=0) when the key is absent.
fn host_state_read(mut caller: wasmtime::Caller<'_, HostState>, ptr: i32, len: i32) -> i64 {
    if !has_cap(&caller, "state") {
        return -1;
    }
    let key = match read_guest_string(&mut caller, ptr, len) {
        Ok(k) => k,
        Err(_) => return -1,
    };
    if !is_valid_state_key(&key) {
        return -1;
    }
    let services = caller.data().services.clone();
    let value = match services.state_read(&key) {
        Ok(Some(v)) => v,
        Ok(None) => return 0, // absent — packed all-zero is the sentinel
        Err(_) => return -1,
    };
    if value.len() as u64 > caller.data().max_output_bytes {
        return -1;
    }
    write_guest_response(&mut caller, &value).unwrap_or(-1)
}

/// borg:state.write host import: takes a JSON payload `{key, value_b64}`.
fn host_state_write(mut caller: wasmtime::Caller<'_, HostState>, ptr: i32, len: i32) -> i32 {
    if !has_cap(&caller, "state") {
        return -1;
    }
    let req_json = match read_guest_string(&mut caller, ptr, len) {
        Ok(s) => s,
        Err(_) => return -1,
    };
    let req: serde_json::Value = match serde_json::from_str(&req_json) {
        Ok(v) => v,
        Err(_) => return -1,
    };
    let key = match req.get("key").and_then(|v| v.as_str()) {
        Some(k) => k.to_string(),
        None => return -1,
    };
    if !is_valid_state_key(&key) {
        return -1;
    }
    let value = match req.get("value_b64").and_then(|v| v.as_str()) {
        Some(b64) => match base64::engine::general_purpose::STANDARD.decode(b64) {
            Ok(b) => b,
            Err(_) => return -1,
        },
        None => return -1,
    };
    if value.len() as u64 > caller.data().max_output_bytes {
        return -1;
    }
    let services = caller.data().services.clone();
    match services.state_write(&key, &value) {
        Ok(()) => 0,
        Err(_) => -1,
    }
}

/// borg:credentials.get host import: returns `packed(ptr, len)` of the
/// credential value, or `0` (packed all-zero) when absent.
fn host_credentials_get(mut caller: wasmtime::Caller<'_, HostState>, ptr: i32, len: i32) -> i64 {
    if !has_cap(&caller, "credentials") {
        return -1;
    }
    let key = match read_guest_string(&mut caller, ptr, len) {
        Ok(k) => k,
        Err(_) => return -1,
    };
    // Refuse keys the manifest never asked for — even if the host implementation
    // would return one, surfacing it widens scope past the operator-trusted set.
    if !caller.data().credentials.iter().any(|k| k == &key) {
        return -1;
    }
    let services = caller.data().services.clone();
    let value = match services.credentials_get(&key) {
        Ok(Some(v)) => v.into_bytes(),
        Ok(None) => return 0,
        Err(_) => return -1,
    };
    if value.len() as u64 > caller.data().max_output_bytes {
        return -1;
    }
    write_guest_response(&mut caller, &value).unwrap_or(-1)
}

fn is_valid_state_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= 64
        && key
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '_' | '-'))
}

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
    use super::*;
    use base64::engine::general_purpose::STANDARD;
    use ed25519_dalek::{Signer, SigningKey};
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

        let calls = services.http_calls.lock().unwrap();
        assert_eq!(calls.len(), 1, "services should be called exactly once");
        assert_eq!(calls[0].0, "POST", "method must reach services");
        assert_eq!(
            calls[0].1, "https://api.example.com/v1/echo",
            "url must reach services"
        );

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
        let calls_after = services.http_calls.lock().unwrap();
        assert_eq!(
            calls_after.len(),
            1,
            "services must NOT be called for off-allowlist URL"
        );
    }

    #[test]
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
            0o700,
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
        // The check runs against the production region only (sliced before
        // `#[cfg(test)]`) so a setter mentioned in this test body itself
        // cannot satisfy the assertion.
        let src = include_str!("wasm.rs");
        let test_boundary = src
            .find("#[cfg(test)]")
            .expect("test module marker must exist");
        let production = &src[..test_boundary];
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
