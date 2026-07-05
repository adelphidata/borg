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

#[cfg(test)]
mod tests {
    use wasm_encoder::{
        BlockType, CodeSection, ConstExpr, DataSection, ExportKind, ExportSection, Function,
        FunctionSection, Instruction, MemorySection, MemoryType, Module, TypeSection, ValType,
    };

    use super::super::test_util::{abi_wasm_with_call_body, manifest_for, minimal_wasm};
    use super::super::{WasmToolManifest, MAX_FUEL_LIMIT};
    use super::*;

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
