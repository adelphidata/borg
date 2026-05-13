//! Capability-sandboxed WebAssembly plugin support.
//!
//! This module intentionally keeps the host ABI small. A plugin package is
//! accepted only after publisher-signature verification, static WASM import
//! validation, and local HMAC sealing. Runtime execution exposes no WASI, no
//! raw sockets, no subprocesses, and no filesystem access.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

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

/// Manifest for a signed WASM plugin package.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
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
pub struct PublisherManifest {
    /// Display name.
    pub name: String,
    /// Base64-encoded Ed25519 verifying key.
    pub public_key: String,
}

/// One callable tool exposed by a WASM plugin.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
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
#[derive(Debug, Clone)]
pub struct HostHttpRequest<'a> {
    /// Request URL.
    pub url: &'a str,
    /// Header key/value pairs.
    pub headers: &'a [(&'a str, &'a str)],
    /// Request body bytes.
    pub body: &'a [u8],
}

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

/// Execute one verified WASM tool using Borg's narrow ABI.
pub fn execute_tool(
    manifest: &WasmPluginManifest,
    wasm: &[u8],
    args_json: &serde_json::Value,
) -> Result<String> {
    validate_wasm_module(manifest, wasm)?;

    let mut config = wasmtime::Config::new();
    config.consume_fuel(true);
    let engine = wasmtime::Engine::new(&config)?;
    let module = wasmtime::Module::new(&engine, wasm)?;
    let mut store = wasmtime::Store::new(&engine, HostState);
    store.set_fuel(manifest.limits.fuel)?;
    let mut linker = wasmtime::Linker::new(&engine);
    linker.func_wrap(
        "borg:log",
        "log",
        |mut caller: wasmtime::Caller<'_, HostState>, ptr: i32, len: i32| -> i32 {
            read_guest_string(&mut caller, ptr, len)
                .map(|msg| {
                    tracing::info!(target: "borg.wasm_plugin", "{msg}");
                    0
                })
                .unwrap_or(-1)
        },
    )?;
    linker.func_wrap("borg:http", "fetch", |_ptr: i32, _len: i32| -> i32 { -1 })?;
    linker.func_wrap("borg:state", "read", |_ptr: i32, _len: i32| -> i32 { -1 })?;
    linker.func_wrap("borg:state", "write", |_ptr: i32, _len: i32| -> i32 { -1 })?;
    linker.func_wrap("borg:credentials", "get", |_ptr: i32, _len: i32| -> i32 {
        -1
    })?;

    let mut store_deadline = std::time::Instant::now();
    store_deadline += std::time::Duration::from_millis(manifest.limits.timeout_ms);
    let instance = linker.instantiate(&mut store, &module)?;
    let memory = instance
        .get_memory(&mut store, ABI_EXPORT_MEMORY)
        .context("WASM plugin missing exported memory")?;
    let alloc = instance
        .get_typed_func::<i32, i32>(&mut store, ABI_EXPORT_ALLOC)
        .context("WASM plugin missing alloc export")?;
    let call = instance
        .get_typed_func::<(i32, i32), i64>(&mut store, ABI_EXPORT_CALL)
        .context("WASM plugin missing borg_tool_call export")?;

    let input = serde_json::to_vec(args_json)?;
    let input_len = i32::try_from(input.len()).context("tool input too large")?;
    let input_ptr = alloc.call(&mut store, input_len)?;
    if input_ptr < 0 {
        bail!("WASM plugin alloc returned a negative pointer");
    }
    memory.write(&mut store, input_ptr as usize, &input)?;
    let packed = call.call(&mut store, (input_ptr, input_len))?;
    if std::time::Instant::now() > store_deadline {
        bail!("WASM plugin execution exceeded timeout");
    }
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

fn compute_local_hmac(key: &[u8], manifest_bytes: &[u8], wasm: &[u8]) -> Result<String> {
    let mut mac =
        HmacSha256::new_from_slice(key).map_err(|_| anyhow!("invalid HMAC key length"))?;
    mac.update(manifest_bytes);
    mac.update(b"\0");
    mac.update(wasm);
    Ok(hex_encode(&mac.finalize().into_bytes()))
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

struct HostState;

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
        let output = execute_tool(&manifest, &wasm, &serde_json::json!({}))
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
            let r = execute_tool(&manifest_thread, &wasm_thread, &serde_json::json!({}));
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

        let err = execute_tool(&manifest, &wasm, &serde_json::json!({}))
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
        let err = execute_tool(&manifest, &wasm, &serde_json::json!({}))
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
        let err = execute_tool(&manifest, &wasm, &serde_json::json!({}))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("non-UTF-8"),
            "expected non-UTF-8 rejection at host boundary, got: {err}"
        );
    }
}
