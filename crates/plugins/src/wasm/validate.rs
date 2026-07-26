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
    ABI_EXPORT_MEMORY, ABI_IMPORT_CREDENTIALS, ABI_IMPORT_HTTP, ABI_IMPORT_LOG, ABI_IMPORT_STATE,
    CAP_CREDENTIALS, CAP_HTTP, CAP_LOG, CAP_STATE, MAX_FUEL_LIMIT,
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
        (ABI_IMPORT_LOG, "log") => has_cap(CAP_LOG),
        (ABI_IMPORT_HTTP, "fetch") => has_cap(CAP_HTTP),
        (ABI_IMPORT_STATE, "read" | "write") => has_cap(CAP_STATE),
        (ABI_IMPORT_CREDENTIALS, "get") => has_cap(CAP_CREDENTIALS),
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

#[cfg(test)]
mod tests {
    use wasm_encoder::{
        CodeSection, ExportKind, ExportSection, Function, FunctionSection, Instruction,
        MemorySection, MemoryType, Module, StartSection, TypeSection, ValType,
    };

    use super::super::test_util::{manifest_for, minimal_wasm};
    use super::super::{sha256_hex, WasmToolManifest, MAX_MEMORY_PAGES_DEFAULT};
    use super::*;

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
}
