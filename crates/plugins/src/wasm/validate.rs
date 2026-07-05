//! Manifest and WASM-module static validation.
//!
//! Everything here runs before a byte of guest code executes: manifest field
//! checks, capability/import gating, endpoint-allowlist syntax, the static
//! walk over the module's sections, and the host-boundary HTTP validator.

use std::collections::BTreeSet;

use anyhow::{bail, Context, Result};
use wasmparser::{ExternalKind, Imports, Parser, Payload, TypeRef};

use super::verify::is_hex_64;
use super::{
    HostHttpRequest, StaticValidation, WasmPluginManifest, ABI_EXPORT_ALLOC, ABI_EXPORT_CALL,
    ABI_EXPORT_MEMORY, CAP_CREDENTIALS, CAP_HTTP, CAP_LOG, CAP_STATE, MAX_FUEL_LIMIT,
    MAX_MEMORY_PAGES_LIMIT, MAX_OUTPUT_BYTES_LIMIT, MAX_TIMEOUT_MS_LIMIT, RESERVED_KEY_PREFIX,
    SCHEMA_VERSION,
};

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
            CAP_LOG | CAP_STATE | CAP_HTTP | CAP_CREDENTIALS => {}
            other => bail!("unknown WASM plugin capability {other}"),
        }
    }
    let has = |c: &str| manifest.capabilities.iter().any(|x| x == c);
    if has(CAP_HTTP) && manifest.endpoints.is_empty() {
        bail!("http capability requires at least one endpoint allowlist entry");
    }
    if !has(CAP_HTTP) && !manifest.endpoints.is_empty() {
        bail!("endpoint allowlist entries require http capability");
    }
    for endpoint in &manifest.endpoints {
        validate_endpoint(endpoint)?;
    }
    if has(CAP_CREDENTIALS) && manifest.credentials.is_empty() {
        bail!("credentials capability requires at least one credential key");
    }
    if !has(CAP_CREDENTIALS) && !manifest.credentials.is_empty() {
        bail!("credential keys require credentials capability");
    }
    for key in &manifest.credentials {
        validate_credential_key(key)?;
    }
    if manifest.limits.fuel == 0 || manifest.limits.fuel > MAX_FUEL_LIMIT {
        bail!("fuel must be between 1 and {MAX_FUEL_LIMIT}");
    }
    if manifest.limits.timeout_ms == 0 || manifest.limits.timeout_ms > MAX_TIMEOUT_MS_LIMIT {
        bail!("timeout_ms must be between 1 and {MAX_TIMEOUT_MS_LIMIT}");
    }
    if manifest.limits.max_memory_pages == 0
        || manifest.limits.max_memory_pages > MAX_MEMORY_PAGES_LIMIT
    {
        bail!("max_memory_pages must be between 1 and {MAX_MEMORY_PAGES_LIMIT}");
    }
    if manifest.limits.max_output_bytes == 0
        || manifest.limits.max_output_bytes > MAX_OUTPUT_BYTES_LIMIT
    {
        bail!("max_output_bytes must be between 1 and {MAX_OUTPUT_BYTES_LIMIT}");
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

/// Validate a plugin-originated HTTP request before any network operation.
pub fn validate_host_http_request(
    manifest: &WasmPluginManifest,
    req: &HostHttpRequest<'_>,
    secret_values: &[String],
) -> Result<()> {
    if !manifest.capabilities.iter().any(|c| c == CAP_HTTP) {
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
        ("borg:log", "log") => has_cap(CAP_LOG),
        ("borg:http", "fetch") => has_cap(CAP_HTTP),
        ("borg:state", "read" | "write") => has_cap(CAP_STATE),
        ("borg:credentials", "get") => has_cap(CAP_CREDENTIALS),
        _ => false,
    };
    if !allowed {
        bail!("WASM plugin import {module}.{name} is not declared or allowed");
    }
    Ok(())
}

pub(super) fn validate_plugin_id(id: &str) -> Result<()> {
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
