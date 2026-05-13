//! Runtime bridge for verified WASM plugin tools.

use std::path::Path;

use anyhow::{Context, Result};

use crate::db::Database;

const HMAC_DOMAIN: &[u8] = b"borg-wasm-plugin-local-hmac-v1";

/// Return verified WASM tool definitions stored in the DB.
pub fn tool_definitions() -> Vec<crate::types::ToolDefinition> {
    let Ok(db) = Database::open() else {
        return Vec::new();
    };
    match db.list_verified_wasm_tool_definitions() {
        Ok(rows) => rows
            .into_iter()
            .filter_map(|row| {
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
    let output = plugins::wasm::execute_tool(&manifest, &wasm, args)
        .with_context(|| format!("WASM plugin tool '{}' failed", row.tool_name))?;
    Ok(Some(output))
}
