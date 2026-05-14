//! Runtime bridge for verified WASM plugin tools.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};

use plugins::wasm::{
    classify_execute_error, HostHttpFetch, HostHttpResponse, HostServices, WasmExecuteErrorKind,
};

use crate::config::Config;
use crate::db::{Database, NewWasmPlugin};
use crate::tool::names;

/// Process-lifetime cache of compiled WASM modules, keyed by the row's
/// `local_hmac`. The HMAC binds (KEK-derived key, canonical manifest, wasm
/// bytes) — `try_execute_tool` re-verifies the on-disk seal before every
/// lookup, so a tampered file never reaches the cache.
pub(crate) mod compiled_cache {
    use std::num::NonZeroUsize;
    use std::sync::{Mutex, OnceLock};

    use anyhow::{anyhow, Result};
    use lru::LruCache;
    use plugins::wasm::{WasmEngine as Engine, WasmModule as Module};

    /// Cap matches the gauntlet's installable-plugin envelope. We use a true
    /// LRU so a hot module survives churn from a one-shot 65th install — the
    /// previous bulk-clear strategy would evict every hot module on overflow.
    const MAX_ENTRIES: usize = 64;

    static ENGINE: OnceLock<Engine> = OnceLock::new();
    static CACHE: OnceLock<Mutex<LruCache<String, Module>>> = OnceLock::new();

    pub(crate) fn engine() -> Result<&'static Engine> {
        if let Some(e) = ENGINE.get() {
            return Ok(e);
        }
        let built = plugins::wasm::build_engine()?;
        Ok(ENGINE.get_or_init(|| built))
    }

    fn cache() -> &'static Mutex<LruCache<String, Module>> {
        // `MAX_ENTRIES` is a non-zero const, but the cortex crate forbids
        // `.expect`/`.unwrap`. `unwrap_or(NonZeroUsize::MIN)` keeps the cache
        // functional (cap 1) even if a future edit drops MAX_ENTRIES to 0.
        CACHE.get_or_init(|| {
            Mutex::new(LruCache::new(
                NonZeroUsize::new(MAX_ENTRIES).unwrap_or(NonZeroUsize::MIN),
            ))
        })
    }

    /// Recorded outcome for the most recent `module_for` call. The agent loop
    /// uses this to emit cache-hit/miss metrics without us threading metric
    /// state through the plugins crate.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(crate) enum CacheOutcome {
        Hit,
        Miss,
    }

    pub(crate) fn module_for(local_hmac: &str, wasm: &[u8]) -> Result<(Module, CacheOutcome)> {
        let mut guard = cache()
            .lock()
            .map_err(|e| anyhow!("wasm module cache mutex poisoned: {e}"))?;
        if let Some(m) = guard.get(local_hmac) {
            return Ok((m.clone(), CacheOutcome::Hit));
        }
        // LruCache::put evicts least-recently-used when over capacity — no
        // explicit bulk-clear, hot modules survive churn.
        let module = plugins::wasm::compile_module(engine()?, wasm)?;
        guard.put(local_hmac.to_string(), module.clone());
        Ok((module, CacheOutcome::Miss))
    }

    /// Promote `local_hmac` to most-recently-used without inserting if missing.
    /// Used by the reseal path so the old key is dropped cleanly when we
    /// rewrite the seal.
    pub(crate) fn invalidate(local_hmac: &str) {
        if let Some(m) = CACHE.get() {
            if let Ok(mut g) = m.lock() {
                g.pop(local_hmac);
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn cache_len() -> usize {
        CACHE
            .get()
            .and_then(|m| m.lock().ok().map(|g| g.len()))
            .unwrap_or(0)
    }

    #[cfg(test)]
    pub(crate) fn contains(local_hmac: &str) -> bool {
        CACHE
            .get()
            .and_then(|m| m.lock().ok().map(|g| g.contains(local_hmac)))
            .unwrap_or(false)
    }

    #[cfg(test)]
    pub(crate) fn clear() {
        if let Some(m) = CACHE.get() {
            if let Ok(mut g) = m.lock() {
                g.clear();
            }
        }
    }
}

/// HMAC derived-key domain currently in use. Cortex passes the key derived
/// from this domain as the local-HMAC key for every install + verify. A
/// pre-current ("v1") install whose seal verifies under `LEGACY_HMAC_DOMAIN`
/// but not this one is auto-resealed (see `legacy_hmac::verify`); operators
/// do not have to reinstall to track a framing bump.
///
/// `pub` so cross-crate integration tests can derive the same key when
/// fabricating installs end-to-end.
pub const HMAC_DOMAIN: &[u8] = b"borg-wasm-plugin-local-hmac-v2";

/// Pre-current ("v1") domain. Verification-only — see `legacy_hmac::verify`
/// in the plugins crate. If a plugin's stored seal verifies under the legacy
/// scheme but not the current one, we reseal under the current scheme and
/// emit `wasm_plugin_resealed` so the operator sees the migration happened.
const LEGACY_HMAC_DOMAIN: &[u8] = b"borg-wasm-plugin-local-hmac";

/// Names of built-in tools — WASM plugins may not shadow them.
///
/// Keep in sync with `crate::tool::names`. A WASM plugin advertising a tool
/// whose name collides with a built-in is filtered out at definition time and
/// rejected at dispatch time, so the LLM never sees two competing schemas for
/// the same name.
fn builtin_tool_names() -> &'static [&'static str] {
    &[
        names::WRITE_MEMORY,
        names::READ_MEMORY,
        names::MEMORY_SEARCH,
        names::LIST,
        names::APPLY_PATCH,
        names::RUN_SHELL,
        names::READ_FILE,
        names::LIST_DIR,
        names::WEB_FETCH,
        names::WEB_SEARCH,
        names::SCHEDULE,
        names::MANAGE_TASKS,
        names::MANAGE_CRON,
        names::BROWSER,
        names::GENERATE_IMAGE,
        names::TEXT_TO_SPEECH,
        names::REQUEST_USER_INPUT,
        names::APPLY_SKILL_PATCH,
        names::CREATE_CHANNEL,
        names::LIST_SKILLS,
        names::LIST_CHANNELS,
        names::LIST_AGENTS,
        names::SPAWN_AGENT,
        names::SEND_TO_AGENT,
        names::WAIT_FOR_AGENT,
        names::CLOSE_AGENT,
        names::MANAGE_ROLES,
        names::GMAIL,
        names::GOOGLE_CALENDAR,
        names::NOTION,
        names::LINEAR,
    ]
}

/// True if `name` collides with a built-in, ASCII-case-insensitively.
///
/// `validate_tool_name` (plugins crate) accepts uppercase ASCII, so an exact
/// match would let `Run_Shell` slip past while still shadowing the built-in
/// anywhere downstream that normalizes case. Case-insensitive comparison
/// closes the gap without restricting plugin tool naming.
fn is_builtin_tool_name(name: &str) -> bool {
    builtin_tool_names()
        .iter()
        .any(|n| n.eq_ignore_ascii_case(name))
}

/// Snapshot of the operator-controlled WASM kill-switch state. Captured once
/// per `tool_definitions()` / `try_execute_tool()` so filtering and dispatch
/// share a consistent view even if config reloads mid-call.
#[derive(Clone, Default)]
struct WasmGuardState {
    enabled: bool,
    disabled_plugin_ids: Vec<String>,
    disabled_publisher_keys: Vec<String>,
    rate_limit_per_minute: u32,
    rate_limit_burst: u32,
}

impl WasmGuardState {
    fn from_config(cfg: &Config) -> Self {
        Self {
            enabled: cfg.plugins.wasm.enabled,
            disabled_plugin_ids: cfg.plugins.wasm.disabled_plugin_ids.clone(),
            disabled_publisher_keys: cfg.plugins.wasm.disabled_publisher_keys.clone(),
            rate_limit_per_minute: cfg.plugins.wasm.rate_limit_per_minute,
            rate_limit_burst: cfg.plugins.wasm.rate_limit_burst,
        }
    }

    fn is_plugin_disabled(&self, plugin_id: &str) -> bool {
        self.disabled_plugin_ids.iter().any(|d| d == plugin_id)
    }

    fn is_publisher_disabled(&self, publisher_key: &str) -> bool {
        self.disabled_publisher_keys
            .iter()
            .any(|d| d == publisher_key)
    }
}

/// Load config, returning a permissive default if the DB is unreachable. We
/// don't want a transient DB hiccup to silently disable plugins; an operator
/// who explicitly disabled the subsystem will have the value persisted, and
/// `Config::load_from_db` is otherwise the source of truth.
fn guard_state() -> WasmGuardState {
    match Config::load_from_db() {
        Ok(cfg) => WasmGuardState::from_config(&cfg),
        Err(e) => {
            tracing::warn!("WASM plugin config unreadable, defaulting open: {e}");
            WasmGuardState {
                enabled: true,
                disabled_plugin_ids: Vec::new(),
                disabled_publisher_keys: Vec::new(),
                rate_limit_per_minute: 60,
                rate_limit_burst: 10,
            }
        }
    }
}

/// Per-plugin token bucket. Tokens accrue at `per_minute / 60` per second up
/// to `burst` and one is consumed per dispatch. Refused calls return a
/// `WasmRateLimited` to the caller, emit an audit row, and never reach the
/// wasmtime sandbox.
struct TokenBucket {
    tokens: f64,
    last_refill: Instant,
}

impl TokenBucket {
    fn new(initial: f64) -> Self {
        Self {
            tokens: initial,
            last_refill: Instant::now(),
        }
    }

    /// Refill the bucket against wall-clock and try to consume one token.
    /// Returns `None` on success, or `Some(retry_after)` on refusal so the
    /// caller can surface a sensible message to the LLM.
    fn try_consume(&mut self, per_minute: u32, burst: u32) -> Option<Duration> {
        if per_minute == 0 || burst == 0 {
            // Misconfigured to off — fail closed: zero throughput. Operators
            // who want unlimited should leave defaults alone.
            return Some(Duration::from_secs(60));
        }
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_refill).as_secs_f64();
        let refill = elapsed * (f64::from(per_minute) / 60.0);
        self.tokens = (self.tokens + refill).min(f64::from(burst));
        self.last_refill = now;
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            None
        } else {
            let deficit = 1.0 - self.tokens;
            let seconds_to_one = deficit / (f64::from(per_minute) / 60.0);
            Some(Duration::from_secs_f64(seconds_to_one.max(0.001)))
        }
    }
}

fn rate_buckets() -> &'static Mutex<HashMap<String, TokenBucket>> {
    static BUCKETS: OnceLock<Mutex<HashMap<String, TokenBucket>>> = OnceLock::new();
    BUCKETS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn try_consume_rate_token(plugin_id: &str, guard: &WasmGuardState) -> Option<Duration> {
    let mut map = match rate_buckets().lock() {
        Ok(m) => m,
        Err(p) => p.into_inner(),
    };
    let bucket = map
        .entry(plugin_id.to_string())
        .or_insert_with(|| TokenBucket::new(f64::from(guard.rate_limit_burst)));
    bucket.try_consume(guard.rate_limit_per_minute, guard.rate_limit_burst)
}

#[cfg(test)]
pub(crate) fn reset_rate_buckets_for_test() {
    if let Ok(mut m) = rate_buckets().lock() {
        m.clear();
    }
}

/// Return verified WASM tool definitions stored in the DB.
pub fn tool_definitions() -> Vec<crate::types::ToolDefinition> {
    let guard = guard_state();
    if !guard.enabled {
        return Vec::new();
    }
    let db = match Database::open() {
        Ok(db) => db,
        Err(e) => {
            tracing::warn!("WASM tool definitions unavailable, DB open failed: {e}");
            return Vec::new();
        }
    };
    // We need publisher_key for the publisher-disable check, so iterate the
    // full plugin rows and join tool defs in-memory. Counts are tiny
    // (capped at 64 installs), so this is cheap.
    let plugin_rows = match db.list_wasm_plugins() {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!("Failed to list WASM plugins: {e}");
            return Vec::new();
        }
    };
    let tool_rows = match db.list_verified_wasm_tool_definitions() {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!("Failed to load WASM tool definitions: {e}");
            return Vec::new();
        }
    };
    let publisher_by_plugin: HashMap<&str, &str> = plugin_rows
        .iter()
        .map(|p| (p.plugin_id.as_str(), p.publisher_key.as_str()))
        .collect();
    tool_rows
        .into_iter()
        .filter_map(|row| {
            if is_builtin_tool_name(&row.name) {
                tracing::warn!(
                    tool = %row.name,
                    "Hiding WASM tool: name collides with built-in"
                );
                return None;
            }
            // list_verified_wasm_tool_definitions filters by p.status='verified',
            // so disabled rows are already excluded — but the publisher-disable
            // list still needs to be applied here.
            let plugin_id = db.plugin_id_for_tool(&row.name).ok().flatten();
            if let Some(plugin_id) = plugin_id.as_deref() {
                if guard.is_plugin_disabled(plugin_id) {
                    return None;
                }
                if let Some(pk) = publisher_by_plugin.get(plugin_id) {
                    if guard.is_publisher_disabled(pk) {
                        return None;
                    }
                }
            }
            let schema = serde_json::from_str::<serde_json::Value>(&row.input_schema_json)
                .map_err(|e| {
                    tracing::warn!(
                        "Skipping WASM tool {} with invalid schema JSON: {e}",
                        row.name
                    );
                })
                .ok()?;
            Some(crate::types::ToolDefinition::new(
                row.name,
                row.description,
                schema,
            ))
        })
        .collect()
}

/// Execute `name` if it belongs to a verified WASM plugin.
///
/// Returns `Ok(None)` when the tool name is not a WASM plugin tool. Surfaces
/// rate-limit denials, kill-switch refusals, and quarantine triggers as
/// `Err`s that the LLM sees as a normal tool failure.
pub fn try_execute_tool(name: &str, args: &serde_json::Value) -> Result<Option<String>> {
    let guard = guard_state();
    if !guard.enabled {
        return Ok(None);
    }
    if is_builtin_tool_name(name) {
        // A built-in never reaches the unknown-tool fallthrough; if it ever did,
        // we must not let a plugin shadow it.
        return Ok(None);
    }
    let db = Database::open()?;
    let Some(row) = db.get_verified_wasm_tool_runtime(name)? else {
        return Ok(None);
    };

    // Kill-switch checks happen *after* we resolve the runtime row so we know
    // the publisher key; before any IO so disabled plugins never read bytes.
    //
    // Note the layering: `get_verified_wasm_tool_runtime` joins on
    // `status='verified'`, so a row whose status was flipped to `'disabled'`
    // via the DB path already short-circuits above. The explicit check here
    // is the *config-only* path — an operator who sets
    // `plugins.wasm.disabled_plugin_ids` without touching the row status
    // (live setting update, programmatic toggle, future hot-reload) still
    // gets refused at dispatch. Both signals are belt-and-suspenders; the
    // CLI/TUI flip both for consistency.
    let plugin_row = db.get_wasm_plugin(&row.plugin_id)?;
    if guard.is_plugin_disabled(&row.plugin_id) {
        anyhow::bail!("WASM plugin '{}' is disabled", row.plugin_id);
    }
    if let Some(p) = plugin_row.as_ref() {
        if guard.is_publisher_disabled(&p.publisher_key) {
            anyhow::bail!("WASM plugin '{}' publisher key is disabled", row.plugin_id);
        }
    }

    // Rate-limit gate — refused calls never reach the sandbox.
    if let Some(retry_after) = try_consume_rate_token(&row.plugin_id, &guard) {
        let retry_secs = retry_after.as_secs_f64().max(0.1);
        crate::telemetry::wasm_metrics::record_rate_limited(&row.plugin_id);
        crate::db::try_emit_global(
            crate::db::audit_events::WASM_PLUGIN_RATE_LIMITED,
            &row.plugin_id,
            &serde_json::json!({
                "plugin_id": &row.plugin_id,
                "tool_name": &row.tool_name,
                "retry_after_secs": retry_secs,
                "limit_per_minute": guard.rate_limit_per_minute,
                "burst": guard.rate_limit_burst,
            }),
        );
        anyhow::bail!(
            "WASM plugin '{}' is rate-limited; retry in {:.1}s",
            row.plugin_id,
            retry_secs
        );
    }

    let manifest_path = Path::new(&row.manifest_path);
    let wasm_path = Path::new(&row.wasm_path);
    let manifest_bytes = std::fs::read(manifest_path)
        .with_context(|| format!("failed to read {}", manifest_path.display()))?;
    let wasm = std::fs::read(wasm_path)
        .with_context(|| format!("failed to read {}", wasm_path.display()))?;
    let key = db.derive_hmac_key(HMAC_DOMAIN);

    let mut effective_hmac = row.local_hmac.clone();
    if let Err(current_err) =
        plugins::wasm::verify_installed_hmac(&key, &manifest_bytes, &wasm, &row.local_hmac)
    {
        // The seal didn't verify under the current scheme. Try the legacy
        // scheme before quarantining: a plugin sealed by a previous borg
        // release should auto-migrate, not force the operator to reinstall.
        let legacy_key = db.derive_hmac_key(LEGACY_HMAC_DOMAIN);
        if plugins::wasm::legacy_hmac::verify(&legacy_key, &manifest_bytes, &wasm, &row.local_hmac)
            .is_ok()
        {
            // Recompute under the current scheme and persist. We
            // intentionally re-run the full v2 install seal so the new MAC
            // is what `verify_installed_hmac` would expect on the next call.
            match plugins::wasm::compute_local_hmac_current(&key, &manifest_bytes, &wasm) {
                Ok(new_seal) => {
                    db.update_wasm_plugin_local_hmac(&row.plugin_id, &new_seal)
                        .with_context(|| format!("persist resealed HMAC for {}", row.plugin_id))?;
                    compiled_cache::invalidate(&row.local_hmac);
                    crate::telemetry::wasm_metrics::record_reseal(&row.plugin_id);
                    crate::db::try_emit_global(
                        crate::db::audit_events::WASM_PLUGIN_RESEALED,
                        &row.plugin_id,
                        &serde_json::json!({
                            "plugin_id": &row.plugin_id,
                            "from": "legacy",
                            "to": "current",
                        }),
                    );
                    effective_hmac = new_seal;
                }
                Err(e) => {
                    crate::telemetry::wasm_metrics::record_quarantine(
                        &row.plugin_id,
                        "reseal_failed",
                    );
                    crate::db::try_emit_global(
                        crate::db::audit_events::WASM_PLUGIN_QUARANTINED,
                        &row.plugin_id,
                        &serde_json::json!({
                            "plugin_id": &row.plugin_id,
                            "reason": e.to_string(),
                            "reason_category": "reseal_failed",
                        }),
                    );
                    db.quarantine_wasm_plugin(&row.plugin_id, &e.to_string())?;
                    return Err(e).context("WASM plugin quarantined");
                }
            }
        } else {
            // Neither scheme verifies — genuine tamper / corruption.
            crate::telemetry::wasm_metrics::record_quarantine(&row.plugin_id, "integrity_failure");
            crate::db::try_emit_global(
                crate::db::audit_events::WASM_PLUGIN_QUARANTINED,
                &row.plugin_id,
                &serde_json::json!({
                    "plugin_id": &row.plugin_id,
                    "reason": current_err.to_string(),
                    "reason_category": "integrity_failure",
                }),
            );
            db.quarantine_wasm_plugin(&row.plugin_id, &current_err.to_string())?;
            return Err(current_err).context("WASM plugin quarantined");
        }
    }

    let manifest: plugins::wasm::WasmPluginManifest =
        serde_json::from_slice(&manifest_bytes).context("invalid installed WASM manifest")?;

    // Load config now so credential resolution works; if it fails, capability
    // calls will still return a clean error to the guest (-1) rather than
    // crashing the host.
    let config = Config::load_from_db().ok().map(Arc::new);
    let services: Arc<dyn HostServices> = Arc::new(BorgHostServices::new(
        &row.plugin_id,
        manifest.credentials.clone(),
        config,
    )?);

    let engine = compiled_cache::engine()?;
    let (module, cache_outcome) = compiled_cache::module_for(&effective_hmac, &wasm)?;
    crate::telemetry::wasm_metrics::record_cache(match cache_outcome {
        compiled_cache::CacheOutcome::Hit => crate::telemetry::wasm_metrics::WasmCacheOutcome::Hit,
        compiled_cache::CacheOutcome::Miss => {
            crate::telemetry::wasm_metrics::WasmCacheOutcome::Miss
        }
    });

    let start = std::time::Instant::now();
    let result = plugins::wasm::execute_tool_prepared(
        &manifest,
        engine,
        &module,
        &wasm,
        &row.tool_name,
        args,
        services,
    );
    let duration_ms = start.elapsed().as_millis() as u64;
    let (error_category, error_message) = match result.as_ref() {
        Ok(_) => (None, None),
        Err(e) => (Some(classify_execute_error(e)), Some(e.to_string())),
    };
    crate::telemetry::wasm_metrics::record_execute(
        &row.plugin_id,
        &row.tool_name,
        duration_ms,
        error_category,
    );
    crate::db::try_emit_global(
        crate::db::audit_events::WASM_PLUGIN_EXECUTED,
        &row.plugin_id,
        &serde_json::json!({
            "plugin_id": &row.plugin_id,
            "tool_name": &row.tool_name,
            "success": result.is_ok(),
            "error_message": error_message,
            "error_category": error_category.map(WasmExecuteErrorKind::as_str),
            "duration_ms": duration_ms,
        }),
    );

    let output = result.with_context(|| format!("WASM plugin tool '{}' failed", row.tool_name))?;
    Ok(Some(output))
}

/// Production HostServices for verified WASM plugins.
///
/// - `http_fetch`: blocking reqwest. The plugins crate validates URL + headers
///   against the manifest's endpoint allowlist before this runs, so we trust
///   `req.request.url` is already an allowed https endpoint.
/// - `state_read/write`: file-per-key under
///   `~/.borg/wasm_plugins/<sha256(plugin_id)>/state/<key>`. Keys are
///   pre-validated by the plugins crate to `[a-z0-9_-]{1,64}` — we
///   double-check by re-rejecting any candidate path that would escape the
///   plugin's state directory.
/// - `credentials_get`: deferred to `Config::resolve_credential_or_env`, with
///   a manifest-allowlist guard so a buggy host import cannot fetch a
///   credential the operator didn't approve when verifying the plugin.
struct BorgHostServices {
    plugin_id: String,
    declared_credentials: Vec<String>,
    state_dir: PathBuf,
    config: Option<Arc<Config>>,
    client: reqwest::blocking::Client,
}

impl BorgHostServices {
    fn new(
        plugin_id: &str,
        declared_credentials: Vec<String>,
        config: Option<Arc<Config>>,
    ) -> Result<Self> {
        let data_dir = Config::data_dir().context("borg data dir is unavailable")?;
        let safe_id = plugins::wasm::sha256_hex(plugin_id.as_bytes());
        let state_dir = data_dir.join("wasm_plugins").join(safe_id).join("state");
        // Use a reasonable per-request timeout in addition to the plugin's
        // wall-clock deadline — guards against a slow upstream wedging the
        // wasmtime thread for the full `timeout_ms` budget.
        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .context("failed to build WASM plugin HTTP client")?;
        Ok(Self {
            plugin_id: plugin_id.to_string(),
            declared_credentials,
            state_dir,
            config,
            client,
        })
    }

    /// Resolve `key` against the plugin's per-plugin state directory while
    /// refusing anything that would escape it (extra defense beyond the
    /// `[a-z0-9_-]{1,64}` validator on the plugins side).
    fn state_path_for(&self, key: &str) -> Result<PathBuf> {
        // Plugins-side already rejects '/'/'.'/empty keys; this is belt-and-braces.
        if key.is_empty() || key.contains('/') || key.contains('\\') || key.contains("..") {
            anyhow::bail!("invalid state key");
        }
        let candidate = self.state_dir.join(key);
        // The candidate must live directly under `state_dir`.
        if candidate.parent() != Some(self.state_dir.as_path()) {
            anyhow::bail!("invalid state key path");
        }
        Ok(candidate)
    }
}

impl HostServices for BorgHostServices {
    fn http_fetch(&self, req: &HostHttpFetch<'_>) -> Result<HostHttpResponse> {
        let method = reqwest::Method::from_bytes(req.method.as_bytes())
            .map_err(|e| anyhow!("invalid HTTP method '{}': {e}", req.method))?;
        let mut builder = self.client.request(method, req.request.url);
        for (k, v) in req.request.headers {
            builder = builder.header(*k, *v);
        }
        if !req.request.body.is_empty() {
            builder = builder.body(req.request.body.to_vec());
        }
        let response = builder
            .send()
            .with_context(|| format!("plugin {} HTTP request failed", self.plugin_id))?;
        let status = response.status().as_u16();
        let headers = response
            .headers()
            .iter()
            .filter_map(|(name, value)| {
                value
                    .to_str()
                    .ok()
                    .map(|s| (name.as_str().to_string(), s.to_string()))
            })
            .collect();
        let body = response.bytes().context("read plugin HTTP body")?.to_vec();
        Ok(HostHttpResponse {
            status,
            headers,
            body,
        })
    }

    fn state_read(&self, key: &str) -> Result<Option<Vec<u8>>> {
        let path = self.state_path_for(key)?;
        match std::fs::read(&path) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("read plugin state {}", path.display())),
        }
    }

    fn state_write(&self, key: &str, value: &[u8]) -> Result<()> {
        let path = self.state_path_for(key)?;
        std::fs::create_dir_all(&self.state_dir)
            .with_context(|| format!("create plugin state dir {}", self.state_dir.display()))?;
        // Write-then-rename so a crash mid-write leaves the old value intact.
        let mut tmp = self.state_dir.clone();
        tmp.push(format!("{key}.tmp"));
        std::fs::write(&tmp, value)
            .with_context(|| format!("write plugin state tmp {}", tmp.display()))?;
        std::fs::rename(&tmp, &path).with_context(|| {
            format!(
                "rename plugin state {} -> {}",
                tmp.display(),
                path.display()
            )
        })?;
        Ok(())
    }

    fn credentials_get(&self, key: &str) -> Result<Option<String>> {
        if !self.declared_credentials.iter().any(|k| k == key) {
            // Plugins-side already enforces this against `HostState.credentials`,
            // but a future linker change could let an undeclared key through.
            anyhow::bail!(
                "plugin {} requested undeclared credential {}",
                self.plugin_id,
                key
            );
        }
        match &self.config {
            Some(cfg) => Ok(cfg.resolve_credential_or_env(key)),
            None => Ok(std::env::var(key).ok().filter(|v| !v.is_empty())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_tool_name_match_is_case_insensitive() {
        // `plugins::wasm::validate_tool_name` accepts uppercase ASCII, so a
        // plugin advertising `Run_Shell` would otherwise slip past the
        // built-in collision check (HashSet exact match) and reach the LLM
        // prompt as a second tool definition. Sanity-check both anchors:
        //
        // - canonical lowercase still matches (would fail if the list went
        //   stale or `is_builtin_tool_name` started normalizing the wrong
        //   direction)
        // - upper/mixed case match (the regression guard)
        // - non-builtin still rejects (would fail if `eq_ignore_ascii_case`
        //   was applied to the wrong argument and accidentally matched
        //   everything)
        assert!(is_builtin_tool_name("run_shell"));
        assert!(is_builtin_tool_name("RUN_SHELL"));
        assert!(is_builtin_tool_name("Run_Shell"));
        assert!(is_builtin_tool_name("rUn_ShElL"));
        assert!(!is_builtin_tool_name("plugin_specific_tool"));
        assert!(!is_builtin_tool_name(""));
    }

    #[test]
    fn token_bucket_refuses_after_burst_and_refills() {
        // Reasonable defaults: 60/min = 1/sec, burst 2. First two consume
        // succeed, third refuses. This locks the math down so an off-by-one
        // in the refill formula surfaces immediately.
        let mut bucket = TokenBucket::new(2.0);
        assert!(bucket.try_consume(60, 2).is_none(), "first take succeeds");
        assert!(bucket.try_consume(60, 2).is_none(), "second take succeeds");
        let denial = bucket
            .try_consume(60, 2)
            .expect("third take must be refused");
        // Refill at 1/sec, deficit ≈ 1 token, so retry_after ≈ 1s. Allow
        // wide slack — clock noise can shave a few ms either way.
        assert!(
            denial.as_secs_f64() > 0.5 && denial.as_secs_f64() < 2.0,
            "retry_after should be ~1s, got {denial:?}",
        );
    }

    #[test]
    fn token_bucket_zero_config_fails_closed() {
        // Setting per_minute=0 must NOT silently mean "unlimited" — that would
        // turn a clumsy operator into a denial-of-budget. Burst=0 likewise.
        let mut bucket = TokenBucket::new(10.0);
        assert!(bucket.try_consume(0, 10).is_some());
        let mut bucket = TokenBucket::new(10.0);
        assert!(bucket.try_consume(60, 0).is_some());
    }

    #[test]
    fn guard_state_filters_by_plugin_id_and_publisher_key() {
        // The kill-switch path branches on these two checks at dispatch time;
        // a typo that lowercased one side or used `contains()` substring match
        // would silently let a disabled publisher's plugins through.
        let guard = WasmGuardState {
            enabled: true,
            disabled_plugin_ids: vec!["dev/bad".to_string(), "vendor/x".to_string()],
            disabled_publisher_keys: vec!["AAAA_pubkey_base64".to_string()],
            rate_limit_per_minute: 60,
            rate_limit_burst: 10,
        };
        assert!(guard.is_plugin_disabled("dev/bad"));
        assert!(guard.is_plugin_disabled("vendor/x"));
        assert!(!guard.is_plugin_disabled("dev/good"));
        // Substring of a disabled id must not match — exact equality only.
        assert!(!guard.is_plugin_disabled("dev/ba"));
        assert!(!guard.is_plugin_disabled("dev/bad/extra"));

        assert!(guard.is_publisher_disabled("AAAA_pubkey_base64"));
        assert!(!guard.is_publisher_disabled("AAAA_pubkey_base6"));
        assert!(!guard.is_publisher_disabled("aaaa_pubkey_base64"));
    }

    /// LRU eviction regression: insert MAX_ENTRIES+1 distinct modules,
    /// "touching" entry 0 between inserts to mark it most-recently-used,
    /// and assert that entry 1 (the actual LRU at overflow) was evicted
    /// while entry 0 survived. The previous bulk-`clear()` strategy would
    /// drop EVERY entry on overflow, which is what this test guards against.
    #[test]
    fn compiled_cache_evicts_lru_not_bulk_clear() {
        // The simplest valid wasm: magic + version + an empty type section.
        // wasmtime will compile this without complaint.
        let wasm: Vec<u8> = vec![
            0x00, 0x61, 0x73, 0x6d, // \0asm
            0x01, 0x00, 0x00, 0x00, // version 1
        ];
        compiled_cache::clear();

        // Fill the cache to capacity. Keys are unique strings so the LruCache
        // sees 64 distinct entries, all compiling the same wasm.
        for i in 0..64 {
            let key = format!("k{i:02}");
            compiled_cache::module_for(&key, &wasm).expect("compile module");
        }
        assert_eq!(compiled_cache::cache_len(), 64);
        assert!(compiled_cache::contains("k00"));
        assert!(compiled_cache::contains("k01"));

        // Touch k00 so it becomes most-recently-used. k01 is now the LRU.
        compiled_cache::module_for("k00", &wasm).expect("touch k00");

        // Insert one more — must evict the LRU (k01), not k00.
        compiled_cache::module_for("k64", &wasm).expect("insert k64");
        assert_eq!(
            compiled_cache::cache_len(),
            64,
            "cap must hold at MAX_ENTRIES, not grow",
        );
        assert!(
            compiled_cache::contains("k00"),
            "touched entry must survive overflow — this is the regression guard \
             against the old bulk-clear behavior",
        );
        assert!(
            !compiled_cache::contains("k01"),
            "true LRU (k01) must be evicted on overflow",
        );
        assert!(
            compiled_cache::contains("k64"),
            "newly inserted entry must be present",
        );

        compiled_cache::clear();
    }

    /// Invalidate must remove a specific key without touching the rest —
    /// the reseal path relies on this to drop the stale-keyed cache entry
    /// when it rewrites the local_hmac. A bulk-clear under the hood would
    /// thrash unrelated plugins on every reseal.
    #[test]
    fn compiled_cache_invalidate_is_surgical() {
        let wasm: Vec<u8> = vec![0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00];
        compiled_cache::clear();
        for i in 0..3 {
            compiled_cache::module_for(&format!("inv{i}"), &wasm).expect("compile");
        }
        assert_eq!(compiled_cache::cache_len(), 3);
        compiled_cache::invalidate("inv1");
        assert!(compiled_cache::contains("inv0"));
        assert!(!compiled_cache::contains("inv1"));
        assert!(compiled_cache::contains("inv2"));
        assert_eq!(compiled_cache::cache_len(), 2);
        compiled_cache::clear();
    }
}

/// End-to-end coverage of `try_execute_tool`: the compiled-module cache, the
/// audit-event emissions for install/execute/quarantine, and the on-disk
/// tamper → quarantine path. These have to be exercised through the public
/// entry point because the cache and audit code only meet at that seam.
///
/// All assertions live in one test function so the `OnceLock`-installed audit
/// writer is set up exactly once and `BORG_DATA_DIR` stays pinned to a single
/// tempdir for the whole sequence. Splitting these would race on global state.
#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod e2e_tests {
    use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

    use base64::engine::general_purpose::STANDARD;
    use base64::Engine as _;
    use ed25519_dalek::{Signer, SigningKey};
    use wasm_encoder::{
        CodeSection, ConstExpr, DataSection, ExportKind, ExportSection, Function, FunctionSection,
        Instruction, MemorySection, MemoryType, Module, TypeSection, ValType,
    };

    use crate::db::{audit_events, install_global_audit, AuditWriter, Database, NewWasmPlugin};

    use super::{compiled_cache, try_execute_tool, HMAC_DOMAIN};

    /// Serializes E2E setup against any other test that mutates
    /// `$BORG_DATA_DIR` in the same test binary (see
    /// `config/tests/credentials.rs` for the canonical pattern).
    fn env_lock() -> MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|p| p.into_inner())
    }

    struct DataDirGuard {
        prev: Option<String>,
        _tmp: tempfile::TempDir,
        path: std::path::PathBuf,
    }

    impl DataDirGuard {
        fn new() -> Self {
            let tmp = tempfile::TempDir::new().expect("tempdir");
            let path = tmp.path().to_path_buf();
            let prev = std::env::var("BORG_DATA_DIR").ok();
            // SAFETY: env mutation guarded by env_lock; restore on Drop.
            unsafe { std::env::set_var("BORG_DATA_DIR", &path) };
            Self {
                prev,
                _tmp: tmp,
                path,
            }
        }
    }

    impl Drop for DataDirGuard {
        fn drop(&mut self) {
            // SAFETY: env mutation guarded by env_lock.
            unsafe {
                match &self.prev {
                    Some(v) => std::env::set_var("BORG_DATA_DIR", v),
                    None => std::env::remove_var("BORG_DATA_DIR"),
                }
            }
        }
    }

    /// Mirrors `plugins::wasm::tests::minimal_wasm` — the guest stages
    /// "OK" at offset 2048 and `borg_tool_call` returns `(2048<<32) | 2`.
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

    #[test]
    fn cache_audit_and_quarantine_e2e() {
        let _lock = env_lock();
        let dir = DataDirGuard::new();

        // Open the DB once for our test driver. Migrations run on open.
        let db = Database::open().expect("open db");

        // Install a process-global audit writer pointing at the same DB so
        // the production `try_emit_global` calls land in the chain we can
        // inspect. `install_global_audit` is `OnceLock::set` — if some other
        // test in this binary beat us to it, we cannot run the audit
        // assertions reliably.
        let writer = AuditWriter::new(Arc::new(Mutex::new(
            Database::open().expect("audit writer db handle"),
        )));
        assert!(
            install_global_audit(writer),
            "audit writer was already installed by another test — \
             run `cargo test -p cortex --lib cache_audit_and_quarantine_e2e`",
        );

        // Sign a manifest with an ephemeral keypair the operator trusts.
        let signing = SigningKey::from_bytes(&[23u8; 32]);
        let trusted = vec![STANDARD.encode(signing.verifying_key().to_bytes())];
        let wasm = minimal_wasm();
        let manifest = plugins::wasm::WasmPluginManifest {
            schema_version: "borg.plugin.v1".to_string(),
            id: "dev/e2e-echo".to_string(),
            name: "E2E Echo".to_string(),
            version: "1.0.0".to_string(),
            publisher: plugins::wasm::PublisherManifest {
                name: "E2E".to_string(),
                public_key: trusted[0].clone(),
            },
            wasm_sha256: plugins::wasm::sha256_hex(&wasm),
            tools: vec![plugins::wasm::WasmToolManifest {
                name: "e2e_echo".to_string(),
                description: "Echo E2E test".to_string(),
                input_schema: serde_json::json!({"type":"object","properties":{}}),
            }],
            capabilities: vec![],
            endpoints: vec![],
            credentials: vec![],
            limits: plugins::wasm::ResourceLimits::default(),
        };
        let canonical = plugins::wasm::canonical_manifest_bytes(&manifest).expect("canonical");
        let sig = STANDARD.encode(signing.sign(&canonical).to_bytes());
        let hmac_key = db.derive_hmac_key(HMAC_DOMAIN);

        // Verify + install + persist DB row (production-equivalent pipeline,
        // minus the CLI wrapper).
        let pkg = plugins::wasm::verify_package(&canonical, &sig, &wasm, &trusted, &hmac_key)
            .expect("verify");
        let installed = plugins::wasm::install_verified_package(&dir.path, &pkg).expect("install");
        let manifest_path_str = installed.manifest_path.to_string_lossy().into_owned();
        let wasm_path_str = installed.wasm_path.to_string_lossy().into_owned();
        let canonical_str =
            String::from_utf8(pkg.canonical_manifest.clone()).expect("canonical manifest is utf-8");
        let manifest_sha = plugins::wasm::sha256_hex(&pkg.canonical_manifest);
        db.insert_wasm_plugin(
            &NewWasmPlugin {
                plugin_id: &manifest.id,
                manifest_json: &canonical_str,
                manifest_sha256: &manifest_sha,
                wasm_sha256: &manifest.wasm_sha256,
                publisher_key: &manifest.publisher.public_key,
                signature: &sig,
                local_hmac: &installed.local_hmac,
                manifest_path: &manifest_path_str,
                wasm_path: &wasm_path_str,
            },
            &[(
                &manifest.tools[0].name,
                &manifest.tools[0].description,
                &serde_json::to_string(&manifest.tools[0].input_schema).unwrap(),
            )],
        )
        .expect("persist plugin");

        // ── Positive: cache miss → hit ──
        compiled_cache::clear();
        super::reset_rate_buckets_for_test();
        assert_eq!(
            compiled_cache::cache_len(),
            0,
            "cache must start empty after clear()"
        );

        let out1 = try_execute_tool("e2e_echo", &serde_json::json!({}))
            .expect("first execute Ok")
            .expect("first execute returned Some");
        assert_eq!(out1, "OK", "guest must surface staged bytes on first call");
        assert_eq!(
            compiled_cache::cache_len(),
            1,
            "first call must populate the cache"
        );

        let out2 = try_execute_tool("e2e_echo", &serde_json::json!({}))
            .expect("second execute Ok")
            .expect("second execute returned Some");
        assert_eq!(out2, "OK", "second call must hit the cache and still work");
        assert_eq!(
            compiled_cache::cache_len(),
            1,
            "second call must reuse the cached Module, not insert a duplicate",
        );

        // ── Positive: audit chain records both executes ──
        let after_exec = db.list_audit_events().expect("list audit events");
        let exec_rows: Vec<_> = after_exec
            .iter()
            .filter(|e| e.event_type == audit_events::WASM_PLUGIN_EXECUTED)
            .collect();
        assert_eq!(
            exec_rows.len(),
            2,
            "two executes must produce two audit rows: {:#?}",
            after_exec
        );
        for row in &exec_rows {
            assert_eq!(row.principal_id, "dev/e2e-echo");
            let payload: serde_json::Value =
                serde_json::from_str(&row.event_json).expect("payload json");
            assert_eq!(payload["success"], serde_json::Value::Bool(true));
            assert_eq!(payload["tool_name"], "e2e_echo");
            assert_eq!(
                payload["error_category"],
                serde_json::Value::Null,
                "successful execute must carry null error_category, not a stale string",
            );
        }

        // ── Negative: tamper → quarantine + audit ──
        let mut bytes = std::fs::read(&installed.wasm_path).expect("read wasm");
        let last = bytes.len() - 1;
        bytes[last] ^= 0xFF;
        std::fs::write(&installed.wasm_path, &bytes).expect("write tampered wasm");

        let err = try_execute_tool("e2e_echo", &serde_json::json!({}))
            .expect_err("tampered plugin must fail execution");
        assert!(
            err.to_string().contains("quarantined"),
            "operator must see the quarantine signal: {err}",
        );

        let after_tamper = db.list_audit_events().expect("list after tamper");
        let quarantine_rows: Vec<_> = after_tamper
            .iter()
            .filter(|e| e.event_type == audit_events::WASM_PLUGIN_QUARANTINED)
            .collect();
        assert_eq!(
            quarantine_rows.len(),
            1,
            "exactly one quarantine row expected: {:#?}",
            after_tamper
        );
        assert_eq!(quarantine_rows[0].principal_id, "dev/e2e-echo");
        let q_payload: serde_json::Value =
            serde_json::from_str(&quarantine_rows[0].event_json).expect("quarantine payload");
        assert_eq!(
            q_payload["reason_category"], "integrity_failure",
            "tamper quarantine must carry a category, not a free-form string only",
        );

        // Plugin row should be marked quarantined and hidden from the runtime.
        let row = db
            .get_wasm_plugin("dev/e2e-echo")
            .expect("get plugin")
            .expect("plugin row exists");
        assert_eq!(row.status, "quarantined");
        assert!(
            db.get_verified_wasm_tool_runtime("e2e_echo")
                .expect("runtime")
                .is_none(),
            "quarantined plugin must not surface to dispatch",
        );

        // Audit chain must still verify end-to-end.
        assert_eq!(
            db.verify_audit_chain().expect("verify chain"),
            crate::db::AuditChainOutcome::Ok,
        );

        compiled_cache::clear();
    }
}

/// Summary of a successful install — returned to the CLI for its print line
/// and to tests for assertions.
#[derive(Debug, Clone)]
pub struct InstalledSummary {
    /// Manifest's declared plugin id (e.g. `dev/echo`).
    pub plugin_id: String,
    /// Manifest version string.
    pub version: String,
    /// Number of tools registered for this plugin.
    pub tool_count: usize,
}

/// Verify and install a signed WASM plugin package directory.
///
/// `dir` must contain `manifest.json`, `manifest.sig`, and `plugin.wasm`. The
/// publisher key in the manifest must appear in `security.trusted_publisher_keys`.
/// On success: the canonical manifest + wasm bytes are written under
/// `data_dir/wasm_plugins/<sha-id>/`, sealed with the per-DB HMAC key, and the
/// metadata + tool rows are persisted so `tool_definitions()` will surface them.
///
/// Shared by the `borg plugin install` CLI command and the integration test
/// suite so both walk the same code path.
pub fn install_from_dir(dir: &Path) -> Result<InstalledSummary> {
    if !dir.is_dir() {
        bail!("{} is not a directory", dir.display());
    }
    let manifest_path = dir.join("manifest.json");
    let sig_path = dir.join("manifest.sig");
    let wasm_path = dir.join("plugin.wasm");

    let manifest_bytes = std::fs::read(&manifest_path)
        .with_context(|| format!("read {}", manifest_path.display()))?;
    let signature_b64 = std::fs::read_to_string(&sig_path)
        .with_context(|| format!("read {}", sig_path.display()))?;
    let wasm =
        std::fs::read(&wasm_path).with_context(|| format!("read {}", wasm_path.display()))?;

    let config = Config::load_from_db().context("load config")?;
    if config.security.trusted_publisher_keys.is_empty() {
        bail!(
            "security.trusted_publisher_keys is empty; add at least one key with `borg settings set security.trusted_publisher_keys '[\"<base64-ed25519>\"]'`"
        );
    }

    let db = Database::open().context("open db")?;
    let local_hmac_key = db.derive_hmac_key(HMAC_DOMAIN);

    let package = plugins::wasm::verify_package(
        &manifest_bytes,
        signature_b64.trim(),
        &wasm,
        &config.security.trusted_publisher_keys,
        &local_hmac_key,
    )
    .context("verify plugin package")?;

    // After signature verification we know which publisher actually signed —
    // check the sunset list. Sunsetted keys reject hard; not-yet-sunsetted
    // deprecated keys warn but still install (operators need a window to
    // migrate to a fresh signing key).
    let mut deprecated_warning: Option<String> = None;
    if let Some(dep) = config
        .security
        .deprecated_publisher_keys
        .iter()
        .find(|d| d.key == package.manifest.publisher.public_key)
    {
        let now = chrono::Utc::now().timestamp();
        if now >= dep.sunset_at {
            anyhow::bail!(
                "publisher key {} reached its sunset_at ({}); install rejected{}",
                dep.key,
                chrono::DateTime::from_timestamp(dep.sunset_at, 0)
                    .map(|d| d.to_rfc3339())
                    .unwrap_or_else(|| dep.sunset_at.to_string()),
                dep.reason
                    .as_deref()
                    .map(|r| format!(" — {r}"))
                    .unwrap_or_default()
            );
        }
        let sunset_display = chrono::DateTime::from_timestamp(dep.sunset_at, 0)
            .map(|d| d.to_rfc3339())
            .unwrap_or_else(|| dep.sunset_at.to_string());
        eprintln!(
            "warning: publisher key {} is deprecated and sunsets at {}{}",
            dep.key,
            sunset_display,
            dep.reason
                .as_deref()
                .map(|r| format!(" — {r}"))
                .unwrap_or_default()
        );
        deprecated_warning = Some(sunset_display);
    }

    let data_dir = Config::data_dir()?;
    let installed = plugins::wasm::install_verified_package(&data_dir, &package)
        .context("write installed package files")?;

    let manifest_sha = plugins::wasm::sha256_hex(&package.canonical_manifest);
    let tool_rows: Vec<(&str, &str, String)> = package
        .manifest
        .tools
        .iter()
        .map(|t| {
            (
                t.name.as_str(),
                t.description.as_str(),
                serde_json::to_string(&t.input_schema).unwrap_or_else(|_| "{}".to_string()),
            )
        })
        .collect();
    let tool_refs: Vec<(&str, &str, &str)> = tool_rows
        .iter()
        .map(|(n, d, s)| (*n, *d, s.as_str()))
        .collect();

    let manifest_path_str = installed.manifest_path.to_string_lossy().into_owned();
    let wasm_path_str = installed.wasm_path.to_string_lossy().into_owned();
    let canonical_manifest_str = String::from_utf8(package.canonical_manifest.clone())
        .context("canonical manifest is not utf-8")?;
    db.insert_wasm_plugin(
        &NewWasmPlugin {
            plugin_id: &package.manifest.id,
            manifest_json: &canonical_manifest_str,
            manifest_sha256: &manifest_sha,
            wasm_sha256: &package.manifest.wasm_sha256,
            publisher_key: &package.manifest.publisher.public_key,
            signature: signature_b64.trim(),
            local_hmac: &installed.local_hmac,
            manifest_path: &manifest_path_str,
            wasm_path: &wasm_path_str,
        },
        &tool_refs,
    )
    .context("persist plugin metadata")?;

    // Match the audit pattern established for WASM_PLUGIN_EXECUTED /
    // _QUARANTINED: principal "system" because the install was operator-driven,
    // not plugin-driven. Lives in cortex (not the CLI) so daemon-side install
    // paths get the same audit trail without re-implementing the emit.
    crate::db::try_emit_global(
        crate::db::audit_events::WASM_PLUGIN_INSTALLED,
        "system",
        &serde_json::json!({
            "plugin_id": &package.manifest.id,
            "version": &package.manifest.version,
            "tool_count": package.manifest.tools.len(),
            "manifest_sha": manifest_sha,
            "deprecated_key": deprecated_warning.is_some(),
            "sunset_at": deprecated_warning,
        }),
    );
    crate::telemetry::wasm_metrics::record_install(&package.manifest.publisher.public_key);

    Ok(InstalledSummary {
        plugin_id: package.manifest.id,
        version: package.manifest.version,
        tool_count: package.manifest.tools.len(),
    })
}

/// Outcome of a `remove` call, surfaced to the CLI so it can warn the user
/// when DB cleanup succeeded but the on-disk install directory was left
/// behind. The CLAUDE.md invariant — "If we report success to the user, it
/// MUST actually succeed" — means the CLI must not print a plain "removed"
/// when `orphaned_dir` is `Some`.
#[derive(Debug, Clone)]
pub struct RemoveOutcome {
    /// True when a DB row existed for `plugin_id` (and was deleted).
    pub existed: bool,
    /// Set when the DB row pointed at a wasm path under `data_dir/wasm_plugins/`
    /// but `remove_dir_all` failed; the directory still lives on disk.
    pub orphaned_dir: Option<PathBuf>,
}

/// Remove a WASM plugin's DB rows and its on-disk install directory.
///
/// The on-disk directory is only removed when it sits under
/// `data_dir/wasm_plugins/` — a paranoia guard against a tampered DB row
/// pointing at a path outside the borg data dir.
pub fn remove(plugin_id: &str) -> Result<RemoveOutcome> {
    let db = Database::open().context("open db")?;
    let row = db.get_wasm_plugin(plugin_id)?;
    let existed = db
        .delete_wasm_plugin(plugin_id)
        .context("delete plugin rows")?;
    if !existed {
        return Ok(RemoveOutcome {
            existed: false,
            orphaned_dir: None,
        });
    }
    let mut orphaned_dir = None;
    let Some(row) = row else {
        // Race: DELETE succeeded but the prior SELECT didn't see the row.
        // We can't clean the directory without its path, so flag the
        // inconsistency rather than silently claiming a clean removal.
        tracing::warn!(
            plugin_id,
            "DB row deleted but lookup returned None; on-disk dir (if any) not cleaned"
        );
        return Ok(RemoveOutcome {
            existed: true,
            orphaned_dir: None,
        });
    };
    let data_dir = Config::data_dir().ok();
    let wasm_path = PathBuf::from(&row.wasm_path);
    if let (Some(data_dir), Some(parent)) = (data_dir, wasm_path.parent()) {
        let expected_root = data_dir.join("wasm_plugins");
        if parent.starts_with(&expected_root) {
            if let Err(e) = std::fs::remove_dir_all(parent) {
                tracing::warn!("Failed to remove {}: {e}", parent.display());
                orphaned_dir = Some(parent.to_path_buf());
            }
        } else {
            tracing::warn!(
                "Refusing to delete plugin dir {} outside data dir",
                parent.display()
            );
            orphaned_dir = Some(parent.to_path_buf());
        }
    }
    Ok(RemoveOutcome {
        existed: true,
        orphaned_dir,
    })
}
