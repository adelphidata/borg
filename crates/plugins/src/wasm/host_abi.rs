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
