//! Execution entrypoints for verified WASM tools.
//!
//! Engine construction, module compilation, the store/linker wiring that
//! binds the `borg:*` host imports, and error classification for the
//! execute path.

use std::sync::Arc;

use anyhow::{bail, Context, Result};

use super::host_abi::{
    host_credentials_get, host_http_fetch, host_log, host_state_read, host_state_write, HostState,
};
use super::limits::{EpochTimer, PluginLimits, WASM_PAGE_BYTES};
use super::validate::validate_wasm_module;
use super::{
    HostServices, NoHostServices, WasmPluginManifest, ABI_EXPORT_ALLOC, ABI_EXPORT_CALL,
    ABI_EXPORT_MEMORY, MAX_LOG_LINES_PER_CALL, RESERVED_TOOL_KEY,
};

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
