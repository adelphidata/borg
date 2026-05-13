//! Runtime bridge for verified WASM plugin tools.

use std::collections::HashSet;
use std::path::Path;

use anyhow::{Context, Result};

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

/// Return verified WASM tool definitions stored in the DB.
pub fn tool_definitions() -> Vec<crate::types::ToolDefinition> {
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
    let output = plugins::wasm::execute_tool(&manifest, &wasm, &row.tool_name, args)
        .with_context(|| format!("WASM plugin tool '{}' failed", row.tool_name))?;
    Ok(Some(output))
}
