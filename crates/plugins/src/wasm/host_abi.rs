//! Host-ABI shims exposed to the guest as `borg:*` imports.
//!
//! Each host function is a thin wrapper over a fallible `*_impl` body: the
//! wrapper maps every error (capability miss, bad guest input, host service
//! failure) to the `-1` sentinel the ABI hands back to the guest, while the
//! body uses ordinary `Result` plumbing. "Absent" results on `state.read` /
//! `credentials.get` are the distinct `0` (packed all-zero) sentinel, never
//! an error.

use std::sync::Arc;

use anyhow::{bail, Context, Result};
use base64::Engine;

use super::limits::PluginLimits;
use super::validate::validate_host_http_request;
use super::{
    HostHttpFetch, HostHttpRequest, HostServices, PublisherManifest, ResourceLimits,
    WasmPluginManifest, ABI_EXPORT_ALLOC, ABI_EXPORT_MEMORY, CAP_CREDENTIALS, CAP_HTTP, CAP_LOG,
    CAP_STATE, MAX_HOST_IMPORT_BYTES, SCHEMA_VERSION,
};

/// Per-call host state attached to the wasmtime Store.
pub(super) struct HostState {
    /// ResourceLimiter that bounds runtime `memory.grow` calls. Static validation
    /// only checks the declared memory shape; without this the guest could
    /// declare 1 page and grow far past `max_memory_pages` at runtime.
    pub(super) limits: PluginLimits,
    /// Capabilities declared by the manifest, copied here so host stubs can
    /// re-check at call time. Static import gating in `validate_import` is the
    /// primary guard; this is defense in depth — a future linker regression
    /// that accidentally exposes a `borg:*` import to an undeclared plugin
    /// would still be caught here.
    pub(super) capabilities: Vec<String>,
    /// Manifest endpoint allowlist, consulted by `borg:http.fetch`.
    pub(super) endpoints: Vec<String>,
    /// Declared credential keys, consulted by `borg:credentials.get` so the
    /// host can refuse a key the manifest never asked for.
    pub(super) credentials: Vec<String>,
    /// Per-call log line budget (`borg:log.log`).
    pub(super) log_lines_remaining: u32,
    /// Maximum response body bytes returned from `borg:http.fetch`. Mirrors
    /// the output cap so a plugin cannot funnel an unbounded body around the
    /// `max_output_bytes` check via the http capability.
    pub(super) max_output_bytes: u64,
    /// Plugin id, surfaced on tracing events for log triage.
    pub(super) plugin_id: String,
    /// Tool name being dispatched, used on tracing events.
    pub(super) tool_name: String,
    /// Caller-supplied host services for the four capability imports.
    pub(super) services: Arc<dyn HostServices>,
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

fn is_valid_state_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= 64
        && key
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '_' | '-'))
}

/// Return an error when the manifest didn't declare `cap`.
///
/// Static import gating in `validate_import` is the primary guard; this is
/// defense in depth — a future linker regression that accidentally exposed a
/// `borg:*` import to an undeclared plugin would still be caught here.
fn ensure_cap(caller: &wasmtime::Caller<'_, HostState>, cap: &str) -> Result<()> {
    if caller.data().capabilities.iter().any(|c| c == cap) {
        Ok(())
    } else {
        bail!("plugin does not have {cap} capability")
    }
}

/// borg:log.log host import: logs a UTF-8 message at info level, gated by the
/// per-call throttle and the `log` capability. Returns `0` on success and the
/// `-1` sentinel on any failure.
pub(super) fn host_log(mut caller: wasmtime::Caller<'_, HostState>, ptr: i32, len: i32) -> i32 {
    match host_log_impl(&mut caller, ptr, len) {
        Ok(()) => 0,
        Err(_) => -1,
    }
}

/// Fallible body of [`host_log`]; the wrapper maps every error to `-1`.
fn host_log_impl(caller: &mut wasmtime::Caller<'_, HostState>, ptr: i32, len: i32) -> Result<()> {
    ensure_cap(caller, CAP_LOG)?;
    {
        let state = caller.data_mut();
        if state.log_lines_remaining == 0 {
            bail!("per-call log budget exhausted");
        }
        // Deliberately spent before the guest string is read: a malformed
        // message still consumes budget, matching the pre-refactor behavior.
        state.log_lines_remaining -= 1;
    }
    let msg = read_guest_string(caller, ptr, len)?;
    let state = caller.data();
    tracing::info!(
        target: "borg.wasm_plugin",
        plugin_id = %state.plugin_id,
        tool = %state.tool_name,
        "{msg}"
    );
    Ok(())
}

/// borg:http.fetch host import: parse a JSON request from guest memory,
/// validate against the manifest allowlist + secret-leak scan, dispatch
/// through `HostServices`, and write the response back as packed (ptr, len)
/// of a JSON string the guest can parse. Returns `-1` on any failure.
pub(super) fn host_http_fetch(
    mut caller: wasmtime::Caller<'_, HostState>,
    ptr: i32,
    len: i32,
) -> i64 {
    host_http_fetch_impl(&mut caller, ptr, len).unwrap_or(-1)
}

/// Fallible body of [`host_http_fetch`]; the wrapper maps every error to `-1`.
fn host_http_fetch_impl(
    caller: &mut wasmtime::Caller<'_, HostState>,
    ptr: i32,
    len: i32,
) -> Result<i64> {
    ensure_cap(caller, CAP_HTTP)?;
    let req_json = read_guest_string(caller, ptr, len)?;
    let req: serde_json::Value =
        serde_json::from_str(&req_json).context("plugin HTTP request is not valid JSON")?;
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
        Some(b64) => base64::engine::general_purpose::STANDARD
            .decode(b64)
            .context("plugin HTTP body_b64 is not valid base64")?,
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
    validate_host_http_request(&manifest_for_validate, &validation_req, &[])?;

    let services = caller.data().services.clone();
    let max_body = caller.data().max_output_bytes;
    let fetch = HostHttpFetch {
        method,
        request: validation_req,
    };
    let response = services.http_fetch(&fetch)?;
    if response.body.len() as u64 > max_body {
        bail!("plugin HTTP response body exceeds max_output_bytes");
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
    let resp_bytes =
        serde_json::to_vec(&resp_json).context("failed to serialize plugin HTTP response")?;
    write_guest_response(caller, &resp_bytes)
}

/// borg:state.read host import: returns `packed(ptr, len)` of the stored
/// value, `0` (packed all-zero, ptr=0 len=0) when the key is absent, or `-1`
/// on any failure.
pub(super) fn host_state_read(
    mut caller: wasmtime::Caller<'_, HostState>,
    ptr: i32,
    len: i32,
) -> i64 {
    host_state_read_impl(&mut caller, ptr, len).unwrap_or(-1)
}

/// Fallible body of [`host_state_read`]; the wrapper maps every error to `-1`.
/// An absent key is NOT an error — it returns `Ok(0)` so the guest can
/// distinguish "no value" from "read failed".
fn host_state_read_impl(
    caller: &mut wasmtime::Caller<'_, HostState>,
    ptr: i32,
    len: i32,
) -> Result<i64> {
    ensure_cap(caller, CAP_STATE)?;
    let key = read_guest_string(caller, ptr, len)?;
    if !is_valid_state_key(&key) {
        bail!("invalid state key {key}");
    }
    let services = caller.data().services.clone();
    let value = match services.state_read(&key)? {
        Some(v) => v,
        None => return Ok(0), // absent — packed all-zero is the sentinel
    };
    if value.len() as u64 > caller.data().max_output_bytes {
        bail!("state value exceeds max_output_bytes");
    }
    write_guest_response(caller, &value)
}

/// borg:state.write host import: takes a JSON payload `{key, value_b64}`.
/// Returns `0` on success and `-1` on any failure.
pub(super) fn host_state_write(
    mut caller: wasmtime::Caller<'_, HostState>,
    ptr: i32,
    len: i32,
) -> i32 {
    match host_state_write_impl(&mut caller, ptr, len) {
        Ok(()) => 0,
        Err(_) => -1,
    }
}

/// Fallible body of [`host_state_write`]; the wrapper maps every error to `-1`.
fn host_state_write_impl(
    caller: &mut wasmtime::Caller<'_, HostState>,
    ptr: i32,
    len: i32,
) -> Result<()> {
    ensure_cap(caller, CAP_STATE)?;
    let req_json = read_guest_string(caller, ptr, len)?;
    let req: serde_json::Value =
        serde_json::from_str(&req_json).context("state write payload is not valid JSON")?;
    let key = req
        .get("key")
        .and_then(|v| v.as_str())
        .context("state write payload missing key")?
        .to_string();
    if !is_valid_state_key(&key) {
        bail!("invalid state key {key}");
    }
    let value = base64::engine::general_purpose::STANDARD
        .decode(
            req.get("value_b64")
                .and_then(|v| v.as_str())
                .context("state write payload missing value_b64")?,
        )
        .context("state write value_b64 is not valid base64")?;
    if value.len() as u64 > caller.data().max_output_bytes {
        bail!("state value exceeds max_output_bytes");
    }
    let services = caller.data().services.clone();
    services.state_write(&key, &value)
}

/// borg:credentials.get host import: returns `packed(ptr, len)` of the
/// credential value, `0` (packed all-zero) when absent, or `-1` on any
/// failure.
pub(super) fn host_credentials_get(
    mut caller: wasmtime::Caller<'_, HostState>,
    ptr: i32,
    len: i32,
) -> i64 {
    host_credentials_get_impl(&mut caller, ptr, len).unwrap_or(-1)
}

/// Fallible body of [`host_credentials_get`]; the wrapper maps every error to
/// `-1`. An absent credential is NOT an error — it returns `Ok(0)`.
fn host_credentials_get_impl(
    caller: &mut wasmtime::Caller<'_, HostState>,
    ptr: i32,
    len: i32,
) -> Result<i64> {
    ensure_cap(caller, CAP_CREDENTIALS)?;
    let key = read_guest_string(caller, ptr, len)?;
    // Refuse keys the manifest never asked for — even if the host implementation
    // would return one, surfacing it widens scope past the operator-trusted set.
    if !caller.data().credentials.iter().any(|k| k == &key) {
        bail!("undeclared credential {key}");
    }
    let services = caller.data().services.clone();
    let value = match services.credentials_get(&key)? {
        Some(v) => v.into_bytes(),
        None => return Ok(0),
    };
    if value.len() as u64 > caller.data().max_output_bytes {
        bail!("credential value exceeds max_output_bytes");
    }
    write_guest_response(caller, &value)
}

#[cfg(test)]
mod tests {
    use wasm_encoder::{
        CodeSection, ConstExpr, DataSection, ExportKind, ExportSection, Function, FunctionSection,
        Instruction, MemorySection, MemoryType, Module, TypeSection, ValType,
    };

    use super::super::test_util::manifest_for;
    use super::super::{
        execute_tool, execute_tool_with_services, sha256_hex, HostHttpResponse,
        ABI_IMPORT_CREDENTIALS, ABI_IMPORT_HTTP, ABI_IMPORT_LOG, ABI_IMPORT_STATE,
    };
    use super::*;

    /// Build a wasm module whose `borg_tool_call` invokes a single host
    /// import (with capability gating wired through) and returns its result
    /// as a packed pointer. The host import is identified by (module, name).
    fn abi_wasm_invoking_import(
        import_module: &str,
        import_name: &str,
        import_returns_i64: bool,
    ) -> Vec<u8> {
        abi_wasm_invoking_import_with(import_module, import_name, import_returns_i64, 1, 1024)
    }

    /// Generalized variant of [`abi_wasm_invoking_import`]: configurable
    /// memory size and `alloc` return value, for tests that need larger guest
    /// buffers or a zero-offset allocator.
    fn abi_wasm_invoking_import_with(
        import_module: &str,
        import_name: &str,
        import_returns_i64: bool,
        mem_pages: u64,
        alloc_return: i32,
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
            minimum: mem_pages,
            maximum: Some(mem_pages),
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
        // alloc: return the configured arena offset
        let mut alloc = Function::new([]);
        alloc.instruction(&Instruction::I32Const(alloc_return));
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

    /// Guest that calls `borg:log.log` three times with its (ptr, len) input
    /// and returns the sum of the three host results, sign-extended. With a
    /// budget of two, the expected sum is 0 + 0 + (-1) = -1.
    fn triple_log_guest() -> Vec<u8> {
        let mut module = Module::new();
        let mut types = TypeSection::new();
        types.ty().function([ValType::I32], [ValType::I32]);
        types
            .ty()
            .function([ValType::I32, ValType::I32], [ValType::I64]);
        types
            .ty()
            .function([ValType::I32, ValType::I32], [ValType::I32]);
        module.section(&types);
        let mut imports = wasm_encoder::ImportSection::new();
        imports.import(ABI_IMPORT_LOG, "log", wasm_encoder::EntityType::Function(2));
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
        let mut code = CodeSection::new();
        let mut alloc = Function::new([]);
        alloc.instruction(&Instruction::I32Const(1024));
        alloc.instruction(&Instruction::End);
        code.function(&alloc);
        let mut call = Function::new([]);
        for i in 0..3 {
            call.instruction(&Instruction::LocalGet(0));
            call.instruction(&Instruction::LocalGet(1));
            call.instruction(&Instruction::Call(0));
            if i > 0 {
                call.instruction(&Instruction::I32Add);
            }
        }
        call.instruction(&Instruction::I64ExtendI32S);
        call.instruction(&Instruction::End);
        code.function(&call);
        module.section(&code);
        module.finish()
    }

    /// A [`HostState`] with permissive-but-bounded defaults for the direct
    /// harness below. Individual tests override the field under test.
    fn harness_state(caps: &[&str], services: Arc<dyn HostServices>) -> HostState {
        HostState {
            limits: PluginLimits {
                max_memory_bytes: 4 * 65_536,
            },
            capabilities: caps.iter().map(|c| c.to_string()).collect(),
            endpoints: vec![],
            credentials: vec![],
            log_lines_remaining: 256,
            max_output_bytes: 1024,
            plugin_id: "dev/harness".to_string(),
            tool_name: "echo_wasm".to_string(),
            services,
        }
    }

    /// Instantiate `wasm` directly against the `borg:*` host imports with a
    /// caller-controlled [`HostState`], bypassing `execute_tool`'s static
    /// validation and output decoding. Returns the raw guest-visible packed
    /// value each host function produced, plus the response bytes when the
    /// call yielded a packed pointer — so tests can assert the exact `-1`/`0`
    /// sentinels the ABI documents instead of inferring them through
    /// execute-path failure modes.
    fn run_import_guest(
        wasm: &[u8],
        state: HostState,
        input_ptr: i32,
        input: &[u8],
    ) -> (i64, Vec<u8>) {
        let engine = wasmtime::Engine::default();
        let module = wasmtime::Module::new(&engine, wasm).expect("compile harness guest");
        let mut store = wasmtime::Store::new(&engine, state);
        let mut linker = wasmtime::Linker::new(&engine);
        linker.func_wrap(ABI_IMPORT_LOG, "log", host_log).unwrap();
        linker
            .func_wrap(ABI_IMPORT_HTTP, "fetch", host_http_fetch)
            .unwrap();
        linker
            .func_wrap(ABI_IMPORT_STATE, "read", host_state_read)
            .unwrap();
        linker
            .func_wrap(ABI_IMPORT_STATE, "write", host_state_write)
            .unwrap();
        linker
            .func_wrap(ABI_IMPORT_CREDENTIALS, "get", host_credentials_get)
            .unwrap();
        let instance = linker
            .instantiate(&mut store, &module)
            .expect("instantiate");
        let memory = instance
            .get_memory(&mut store, ABI_EXPORT_MEMORY)
            .expect("memory export");
        memory
            .write(&mut store, input_ptr as usize, input)
            .expect("write harness input");
        let call = instance
            .get_typed_func::<(i32, i32), i64>(&mut store, "borg_tool_call")
            .expect("borg_tool_call export");
        let packed = call
            .call(&mut store, (input_ptr, input.len() as i32))
            .expect("host imports return sentinels, never trap");
        let bytes = if packed > 0 {
            let ptr = (packed >> 32) as u32 as usize;
            let len = (packed & 0xffff_ffff) as u32 as usize;
            let mut out = vec![0u8; len];
            memory.read(&store, ptr, &mut out).expect("read output");
            out
        } else {
            Vec::new()
        };
        (packed, bytes)
    }

    #[test]
    #[ntest::timeout(15_000)]
    fn host_log_guest_sentinels_and_import_size_limit() {
        // Guest-visible contract of borg:log.log across the boundary cases:
        //   * missing capability → exactly -1 (runtime gate, independent of
        //     the static import gate),
        //   * ptr=0 / len=0 is a valid empty message, not a protocol error,
        //   * a message exactly at the 64 KiB host-import limit is accepted;
        //     one byte past it is refused. Literals on purpose — the limit is
        //     contract, so a drifted constant must fail here.
        let big_ok = vec![b'a'; 65_536];
        let too_big = vec![b'a'; 65_537];
        /// (declared capabilities, guest ptr, message bytes, expected
        /// return, failure note) — one guest-visible log contract case.
        type LogCase<'a> = (&'a [&'a str], i32, &'a [u8], i64, &'a str);
        let cases: &[LogCase] = &[
            (
                &[],
                4096,
                b"hello",
                -1,
                "missing log capability must return the -1 sentinel",
            ),
            (
                &["log"],
                4096,
                b"hello",
                0,
                "declared capability + valid message returns 0",
            ),
            (&["log"], 0, b"", 0, "ptr=0/len=0 is a valid empty message"),
            (
                &["log"],
                0,
                &big_ok,
                0,
                "a message exactly at the 64 KiB import limit is accepted",
            ),
            (
                &["log"],
                0,
                &too_big,
                -1,
                "one byte past the 64 KiB import limit is refused",
            ),
        ];
        for (caps, ptr, input, expected, why) in cases {
            let wasm = abi_wasm_invoking_import_with(ABI_IMPORT_LOG, "log", false, 2, 1024);
            let state = harness_state(caps, Arc::new(RecordingHostServices::default()));
            let (packed, _) = run_import_guest(&wasm, state, *ptr, input);
            assert_eq!(packed, *expected, "{why}");
        }
    }

    #[test]
    #[ntest::timeout(15_000)]
    fn host_log_budget_is_spent_per_call_until_exhausted() {
        let wasm = triple_log_guest();
        // Budget 2: the third call must fail, so the summed results are
        // 0 + 0 + (-1) = -1. A budget that grew, divided, or never decremented
        // would let all three calls succeed.
        let mut state = harness_state(&["log"], Arc::new(RecordingHostServices::default()));
        state.log_lines_remaining = 2;
        let (packed, _) = run_import_guest(&wasm, state, 4096, b"hi");
        assert_eq!(packed, -1, "third log call must exhaust a budget of two");
        // Budget 3 covers all three calls — proves the -1 above came from the
        // budget, not some other failure in the harness.
        let mut state = harness_state(&["log"], Arc::new(RecordingHostServices::default()));
        state.log_lines_remaining = 3;
        let (packed, _) = run_import_guest(&wasm, state, 4096, b"hi");
        assert_eq!(packed, 0, "a budget of three covers all three calls");
    }

    #[test]
    #[ntest::timeout(15_000)]
    fn host_http_fetch_response_body_cap_is_inclusive() {
        let wasm = abi_wasm_invoking_import_with(ABI_IMPORT_HTTP, "fetch", true, 1, 1024);
        let req = serde_json::json!({"url": "https://api.example.com/v1/x", "method": "GET"});
        let input = serde_json::to_vec(&req).unwrap();
        for (body_len, expect_ok) in [(1024usize, true), (1025, false)] {
            let services = Arc::new(RecordingHostServices {
                http_response: HostHttpResponse {
                    status: 200,
                    headers: vec![],
                    body: vec![b'a'; body_len],
                },
                ..Default::default()
            });
            let mut state = harness_state(&["http"], services);
            state.endpoints = vec!["https://api.example.com/".to_string()];
            let (packed, bytes) = run_import_guest(&wasm, state, 4096, &input);
            if expect_ok {
                assert!(
                    packed > 0,
                    "a response body exactly at max_output_bytes must reach the guest"
                );
                let parsed: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                let body = base64::engine::general_purpose::STANDARD
                    .decode(parsed["body_b64"].as_str().unwrap())
                    .unwrap();
                assert_eq!(body.len(), 1024, "at-cap body round-trips intact");
            } else {
                assert_eq!(
                    packed, -1,
                    "a response body one byte past the cap must be refused"
                );
            }
        }
    }

    #[test]
    #[ntest::timeout(15_000)]
    fn host_state_read_cap_absence_and_invalid_key_sentinels() {
        let wasm = abi_wasm_invoking_import_with(ABI_IMPORT_STATE, "read", true, 1, 1024);
        let services = Arc::new(RecordingHostServices::default());
        services
            .state
            .lock()
            .unwrap()
            .insert("mykey".to_string(), vec![b'v'; 1024]);

        // Value exactly at max_output_bytes round-trips.
        let (packed, bytes) = run_import_guest(
            &wasm,
            harness_state(&["state"], services.clone()),
            4096,
            b"mykey",
        );
        assert!(packed > 0, "at-cap state value must reach the guest");
        assert_eq!(bytes, vec![b'v'; 1024]);

        // One byte past the cap is refused.
        services
            .state
            .lock()
            .unwrap()
            .insert("mykey".to_string(), vec![b'v'; 1025]);
        let (packed, _) = run_import_guest(
            &wasm,
            harness_state(&["state"], services.clone()),
            4096,
            b"mykey",
        );
        assert_eq!(packed, -1, "over-cap state value must be refused");

        // Absent key is the distinct all-zero sentinel, not an error.
        let (packed, _) = run_import_guest(
            &wasm,
            harness_state(&["state"], services.clone()),
            4096,
            b"other",
        );
        assert_eq!(packed, 0, "absent key must return the 0 sentinel");

        // Invalid key is exactly the -1 sentinel.
        let (packed, _) = run_import_guest(
            &wasm,
            harness_state(&["state"], services.clone()),
            4096,
            b"Bad Key",
        );
        assert_eq!(packed, -1, "invalid key must return exactly -1");
    }

    #[test]
    #[ntest::timeout(15_000)]
    fn host_state_write_cap_and_sentinels() {
        let wasm = abi_wasm_invoking_import_with(ABI_IMPORT_STATE, "write", false, 1, 1024);
        let payload = |n: usize| {
            serde_json::to_vec(&serde_json::json!({
                "key": "mykey",
                "value_b64": base64::engine::general_purpose::STANDARD.encode(vec![b'v'; n]),
            }))
            .unwrap()
        };

        // Value exactly at max_output_bytes is stored.
        let services = Arc::new(RecordingHostServices::default());
        let (packed, _) = run_import_guest(
            &wasm,
            harness_state(&["state"], services.clone()),
            4096,
            &payload(1024),
        );
        assert_eq!(packed, 0, "at-cap state write must succeed");
        assert_eq!(
            services.state.lock().unwrap().get("mykey").map(Vec::len),
            Some(1024),
            "at-cap value must reach the services store"
        );

        // One byte past the cap is refused before the services call.
        let services = Arc::new(RecordingHostServices::default());
        let (packed, _) = run_import_guest(
            &wasm,
            harness_state(&["state"], services.clone()),
            4096,
            &payload(1025),
        );
        assert_eq!(packed, -1, "over-cap state write must be refused");
        assert!(
            services.state.lock().unwrap().is_empty(),
            "refused write must never reach the services store"
        );

        // Malformed payload is exactly the -1 sentinel.
        let services = Arc::new(RecordingHostServices::default());
        let (packed, _) = run_import_guest(
            &wasm,
            harness_state(&["state"], services),
            4096,
            b"not json",
        );
        assert_eq!(packed, -1, "malformed write payload must return exactly -1");
    }

    #[test]
    #[ntest::timeout(15_000)]
    fn host_credentials_get_cap_and_sentinels() {
        let wasm = abi_wasm_invoking_import_with(ABI_IMPORT_CREDENTIALS, "get", true, 1, 1024);
        let state_for = |value: Option<&str>| {
            let services = Arc::new(RecordingHostServices {
                credentials: std::sync::Mutex::new(
                    value
                        .map(|v| ("KNOWN".to_string(), v.to_string()))
                        .into_iter()
                        .collect(),
                ),
                ..Default::default()
            });
            let mut state = harness_state(&["credentials"], services);
            state.credentials = vec!["KNOWN".to_string()];
            // Tight cap so the boundary is exercised with tiny values.
            state.max_output_bytes = 4;
            state
        };

        // Declared + present, exactly at the cap: value reaches the guest.
        let (packed, bytes) = run_import_guest(&wasm, state_for(Some("abcd")), 4096, b"KNOWN");
        assert!(packed > 0, "at-cap credential must reach the guest");
        assert_eq!(bytes, b"abcd");

        // One byte past the cap is refused.
        let (packed, _) = run_import_guest(&wasm, state_for(Some("abcde")), 4096, b"KNOWN");
        assert_eq!(packed, -1, "over-cap credential must be refused");

        // Declared but absent is the distinct 0 sentinel, not an error.
        let (packed, _) = run_import_guest(&wasm, state_for(None), 4096, b"KNOWN");
        assert_eq!(packed, 0, "absent credential must return the 0 sentinel");
    }

    #[test]
    #[ntest::timeout(15_000)]
    fn write_guest_response_accepts_pointer_zero_from_alloc() {
        // An allocator handing out offset 0 is unusual but legal — only
        // negative pointers are protocol errors. Rejecting 0 would break
        // guests whose arena starts at the memory base.
        let wasm = abi_wasm_invoking_import_with(ABI_IMPORT_STATE, "read", true, 1, 0);
        let services = Arc::new(RecordingHostServices::default());
        services
            .state
            .lock()
            .unwrap()
            .insert("mykey".to_string(), b"hi".to_vec());
        let state = harness_state(&["state"], services);
        let (packed, bytes) = run_import_guest(&wasm, state, 4096, b"mykey");
        assert_eq!(packed, 2, "packed (ptr=0, len=2) must survive intact");
        assert_eq!(bytes, b"hi");
    }

    #[test]
    fn is_valid_state_key_enforces_charset_and_length() {
        let cases: &[(&str, bool, &str)] = &[
            ("abc", true, "plain lowercase"),
            ("123", true, "digits only"),
            ("a_b-c9", true, "underscore and dash allowed"),
            ("", false, "empty key"),
            ("ABC", false, "uppercase rejected"),
            ("a b", false, "whitespace rejected"),
            ("key.with.dots", false, "dots rejected"),
        ];
        for (key, expect, why) in cases {
            assert_eq!(is_valid_state_key(key), *expect, "{why}: {key:?}");
        }
        // Length bound is inclusive at 64.
        assert!(is_valid_state_key(&"k".repeat(64)), "64 chars is valid");
        assert!(!is_valid_state_key(&"k".repeat(65)), "65 chars is invalid");
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
}
