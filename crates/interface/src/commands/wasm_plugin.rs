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

use cortex::db::Database;
use cortex::wasm_plugins;

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

fn install(dir: &Path) -> Result<()> {
    let summary = wasm_plugins::install_from_dir(dir)?;
    println!(
        "installed {} ({}) — {} tool(s)",
        summary.plugin_id, summary.version, summary.tool_count
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
    println!("{:<32}  {:<10}  {:<5}  INSTALLED", "ID", "STATUS", "TOOLS");
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
    let key = db.derive_hmac_key(wasm_plugins::HMAC_DOMAIN);
    plugins::wasm::verify_installed_hmac(&key, &manifest_bytes, &wasm, &row.local_hmac)
        .context("local HMAC verification failed")?;
    db.restore_wasm_plugin(plugin_id)
        .context("clear quarantine flag")?;
    println!("verified {plugin_id}");
    Ok(())
}

fn remove(plugin_id: &str) -> Result<()> {
    let outcome = wasm_plugins::remove(plugin_id)?;
    if !outcome.existed {
        bail!("plugin '{plugin_id}' not found");
    }
    if let Some(dir) = outcome.orphaned_dir {
        // DB rows are gone but the on-disk install dir was left behind. The
        // user needs to see this on stderr — they typed `borg plugin remove`
        // expecting a clean state, and tracing logs aren't always tailed.
        eprintln!(
            "warning: removed DB rows but could not delete {} — see logs",
            dir.display()
        );
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
