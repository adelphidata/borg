//! Runtime bridge for verified WASM plugin tools.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};

use plugins::wasm::{HostHttpFetch, HostHttpResponse, HostServices};

use crate::config::Config;
use crate::db::Database;
use crate::tool::names;

// v2 bumps the derived-key domain alongside the framing change in
// compute_local_hmac (length-prefixed over NUL-separated). Pre-v2 installs
// re-verify under a different MAC, fail integrity, and quarantine themselves —
// the migration is `borg plugin remove && borg plugin install`.
const HMAC_DOMAIN: &[u8] = b"borg-wasm-plugin-local-hmac-v2";

/// Names of built-in tools — WASM plugins may not shadow them.
///
/// Keep in sync with `crate::tool::names`. A WASM plugin advertising a tool
/// whose name collides with a built-in is filtered out at definition time and
/// rejected at dispatch time, so the LLM never sees two competing schemas for
/// the same name.
fn builtin_tool_names() -> &'static HashSet<&'static str> {
    use std::sync::OnceLock;
    static NAMES: OnceLock<HashSet<&'static str>> = OnceLock::new();
    NAMES.get_or_init(|| {
        [
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
        .into_iter()
        .collect()
    })
}

/// Cheap check for whether the WASM plugin subsystem is allowed to advertise
/// tools or dispatch calls. Falls back to `true` (the default) if config can't
/// be loaded — we don't want a transient DB hiccup to silently disable plugins.
fn wasm_enabled() -> bool {
    match Config::load_from_db() {
        Ok(cfg) => cfg.plugins.wasm.enabled,
        Err(e) => {
            tracing::warn!("WASM plugin enabled flag unreadable, defaulting on: {e}");
            true
        }
    }
}

/// Return verified WASM tool definitions stored in the DB.
pub fn tool_definitions() -> Vec<crate::types::ToolDefinition> {
    if !wasm_enabled() {
        return Vec::new();
    }
    let db = match Database::open() {
        Ok(db) => db,
        Err(e) => {
            tracing::warn!("WASM tool definitions unavailable, DB open failed: {e}");
            return Vec::new();
        }
    };
    let builtins = builtin_tool_names();
    match db.list_verified_wasm_tool_definitions() {
        Ok(rows) => rows
            .into_iter()
            .filter_map(|row| {
                if builtins.contains(row.name.as_str()) {
                    tracing::warn!(
                        tool = %row.name,
                        "Hiding WASM tool: name collides with built-in"
                    );
                    return None;
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
            .collect(),
        Err(e) => {
            tracing::warn!("Failed to load WASM tool definitions: {e}");
            Vec::new()
        }
    }
}

/// Execute `name` if it belongs to a verified WASM plugin.
///
/// Returns `Ok(None)` when the tool name is not a WASM plugin tool.
pub fn try_execute_tool(name: &str, args: &serde_json::Value) -> Result<Option<String>> {
    if !wasm_enabled() {
        return Ok(None);
    }
    if builtin_tool_names().contains(name) {
        // A built-in never reaches the unknown-tool fallthrough; if it ever did,
        // we must not let a plugin shadow it.
        return Ok(None);
    }
    let db = Database::open()?;
    let Some(row) = db.get_verified_wasm_tool_runtime(name)? else {
        return Ok(None);
    };

    let manifest_path = Path::new(&row.manifest_path);
    let wasm_path = Path::new(&row.wasm_path);
    let manifest_bytes = std::fs::read(manifest_path)
        .with_context(|| format!("failed to read {}", manifest_path.display()))?;
    let wasm = std::fs::read(wasm_path)
        .with_context(|| format!("failed to read {}", wasm_path.display()))?;
    let key = db.derive_hmac_key(HMAC_DOMAIN);

    if let Err(e) =
        plugins::wasm::verify_installed_hmac(&key, &manifest_bytes, &wasm, &row.local_hmac)
    {
        db.quarantine_wasm_plugin(&row.plugin_id, &e.to_string())?;
        return Err(e).context("WASM plugin quarantined");
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

    let output =
        plugins::wasm::execute_tool_with_services(&manifest, &wasm, &row.tool_name, args, services)
            .with_context(|| format!("WASM plugin tool '{}' failed", row.tool_name))?;
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
