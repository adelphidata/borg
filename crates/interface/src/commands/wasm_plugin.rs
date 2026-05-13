//! `borg plugin` — install, list, show, remove, and verify WASM plugin packages.
//!
//! A package directory must contain three files:
//!   - `manifest.json` — canonical signed manifest
//!   - `manifest.sig` — base64 Ed25519 detached signature over manifest.json
//!   - `plugin.wasm` — the WASM module whose SHA-256 matches the manifest
//!
//! Install rejects any package whose publisher key isn't in
//! `security.trusted_publisher_keys`. Remove deletes both DB rows and the
//! on-disk install dir under `~/.borg/wasm_plugins/<sha-id>/`.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use clap::Subcommand;

use cortex::config::Config;
use cortex::db::{Database, NewWasmPlugin};

#[derive(Subcommand)]
pub enum WasmPluginAction {
    /// Install a WASM plugin from a directory containing manifest.json,
    /// manifest.sig, and plugin.wasm.
    Install {
        /// Directory holding the package files.
        path: PathBuf,
    },
    /// List all installed WASM plugins.
    List,
    /// Show full details for a single installed plugin.
    Show {
        /// Plugin id (e.g. `dev/echo`).
        plugin_id: String,
    },
    /// Re-run integrity verification against the on-disk install. Clears
    /// quarantine on success.
    Verify {
        /// Plugin id to verify.
        plugin_id: String,
    },
    /// Delete a plugin's DB rows and on-disk install directory.
    Remove {
        /// Plugin id to remove.
        plugin_id: String,
    },
}

pub fn dispatch(action: WasmPluginAction) -> Result<()> {
    match action {
        WasmPluginAction::Install { path } => install(&path),
        WasmPluginAction::List => list(),
        WasmPluginAction::Show { plugin_id } => show(&plugin_id),
        WasmPluginAction::Verify { plugin_id } => verify(&plugin_id),
        WasmPluginAction::Remove { plugin_id } => remove(&plugin_id),
    }
}

const HMAC_DOMAIN: &[u8] = b"borg-wasm-plugin-local-hmac-v2";

fn install(dir: &Path) -> Result<()> {
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

    println!(
        "installed {} ({}) — {} tool(s)",
        package.manifest.id,
        package.manifest.version,
        package.manifest.tools.len()
    );
    Ok(())
}

fn list() -> Result<()> {
    let db = Database::open().context("open db")?;
    let rows = db.list_wasm_plugins().context("list wasm plugins")?;
    if rows.is_empty() {
        println!("(no WASM plugins installed)");
        return Ok(());
    }
    println!(
        "{:<32}  {:<10}  {:<5}  {}",
        "ID", "STATUS", "TOOLS", "INSTALLED"
    );
    for row in rows {
        let tools = db.count_wasm_plugin_tools(&row.plugin_id).unwrap_or(0);
        let installed = chrono::DateTime::from_timestamp(row.installed_at, 0)
            .map(|dt| dt.format("%Y-%m-%d").to_string())
            .unwrap_or_else(|| "?".into());
        println!(
            "{:<32}  {:<10}  {:<5}  {}",
            truncate(&row.plugin_id, 32),
            row.status,
            tools,
            installed
        );
    }
    Ok(())
}

fn show(plugin_id: &str) -> Result<()> {
    let db = Database::open().context("open db")?;
    let row = db
        .get_wasm_plugin(plugin_id)?
        .ok_or_else(|| anyhow!("plugin '{plugin_id}' not installed"))?;
    println!("plugin_id:       {}", row.plugin_id);
    println!("status:          {}", row.status);
    if let Some(reason) = &row.quarantine_reason {
        println!("quarantine_reason: {reason}");
    }
    println!("publisher_key:   {}", row.publisher_key);
    println!("manifest_sha256: {}", row.manifest_sha256);
    println!("wasm_sha256:     {}", row.wasm_sha256);
    println!("manifest_path:   {}", row.manifest_path);
    println!("wasm_path:       {}", row.wasm_path);
    println!(
        "installed_at:    {}",
        chrono::DateTime::from_timestamp(row.installed_at, 0)
            .map(|d| d.to_rfc3339())
            .unwrap_or_else(|| "?".into())
    );
    if let Some(ts) = row.verified_at {
        println!(
            "verified_at:     {}",
            chrono::DateTime::from_timestamp(ts, 0)
                .map(|d| d.to_rfc3339())
                .unwrap_or_else(|| "?".into())
        );
    }
    println!("\nmanifest_json:");
    let pretty: serde_json::Value = serde_json::from_str(&row.manifest_json)
        .unwrap_or(serde_json::Value::String(row.manifest_json.clone()));
    println!(
        "{}",
        serde_json::to_string_pretty(&pretty).unwrap_or(row.manifest_json)
    );
    Ok(())
}

fn verify(plugin_id: &str) -> Result<()> {
    let db = Database::open().context("open db")?;
    let row = db
        .get_wasm_plugin(plugin_id)?
        .ok_or_else(|| anyhow!("plugin '{plugin_id}' not installed"))?;
    let manifest_bytes =
        std::fs::read(&row.manifest_path).with_context(|| format!("read {}", row.manifest_path))?;
    let wasm = std::fs::read(&row.wasm_path).with_context(|| format!("read {}", row.wasm_path))?;
    let key = db.derive_hmac_key(HMAC_DOMAIN);
    plugins::wasm::verify_installed_hmac(&key, &manifest_bytes, &wasm, &row.local_hmac)
        .context("local HMAC verification failed")?;
    db.restore_wasm_plugin(plugin_id)
        .context("clear quarantine flag")?;
    println!("verified {plugin_id}");
    Ok(())
}

fn remove(plugin_id: &str) -> Result<()> {
    let db = Database::open().context("open db")?;
    let row = db.get_wasm_plugin(plugin_id)?;
    let existed = db
        .delete_wasm_plugin(plugin_id)
        .context("delete plugin rows")?;
    if !existed {
        bail!("plugin '{plugin_id}' not found");
    }
    if let Some(row) = row {
        // Delete the install dir if it sits under data_dir/wasm_plugins.
        let data_dir = Config::data_dir().ok();
        let wasm_path = PathBuf::from(&row.wasm_path);
        if let (Some(data_dir), Some(parent)) = (data_dir, wasm_path.parent()) {
            let expected_root = data_dir.join("wasm_plugins");
            if parent.starts_with(&expected_root) {
                if let Err(e) = std::fs::remove_dir_all(parent) {
                    tracing::warn!("Failed to remove {}: {e}", parent.display());
                    eprintln!("warning: could not remove {}: {e}", parent.display());
                }
            } else {
                tracing::warn!(
                    "Refusing to delete plugin dir {} outside data dir",
                    parent.display()
                );
            }
        }
    }
    println!("removed {plugin_id}");
    Ok(())
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}…", &s[..max - 1])
    }
}
