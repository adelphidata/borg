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
use cortex::db::{audit_events, try_emit_global, Database};
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
    /// Disable a plugin: tools hidden from the LLM, dispatch refuses. Install
    /// row and files remain so re-enable is a one-flag toggle. Use this for
    /// incident response that doesn't justify a full uninstall.
    Disable {
        /// Plugin id to disable.
        plugin_id: String,
    },
    /// Re-enable a previously-disabled plugin. Quarantined rows must go
    /// through `verify` instead — enable does not silently clear a
    /// quarantine.
    Enable {
        /// Plugin id to enable.
        plugin_id: String,
    },
    /// Disable every plugin signed by `publisher_key`. Operator-curated trust
    /// in `security.trusted_publisher_keys` is unaffected — this is a paused
    /// state, not a withdrawal.
    DisablePublisher {
        /// Base64 Ed25519 verifying key.
        publisher_key: String,
    },
    /// Re-enable plugins signed by `publisher_key`.
    EnablePublisher {
        /// Base64 Ed25519 verifying key.
        publisher_key: String,
    },
}

pub async fn dispatch(action: WasmPluginAction) -> Result<()> {
    match action {
        WasmPluginAction::Install { path } => install(&path).await,
        WasmPluginAction::List => list().await,
        WasmPluginAction::Show { plugin_id } => show(&plugin_id).await,
        WasmPluginAction::Verify { plugin_id } => verify(&plugin_id).await,
        WasmPluginAction::Remove { plugin_id } => remove(&plugin_id).await,
        WasmPluginAction::Disable { plugin_id } => disable_plugin(&plugin_id).await,
        WasmPluginAction::Enable { plugin_id } => enable_plugin(&plugin_id).await,
        WasmPluginAction::DisablePublisher { publisher_key } => {
            disable_publisher(&publisher_key).await
        }
        WasmPluginAction::EnablePublisher { publisher_key } => {
            enable_publisher(&publisher_key).await
        }
    }
}

async fn install(dir: &Path) -> Result<()> {
    let summary = wasm_plugins::install_from_dir(dir).await?;
    println!(
        "installed {} ({}) — {} tool(s)",
        summary.plugin_id, summary.version, summary.tool_count
    );
    Ok(())
}

fn deprecated_sunset_for(config: &Config, publisher_key: &str) -> Option<i64> {
    config
        .security
        .deprecated_publisher_keys
        .iter()
        .find(|d| d.key == publisher_key)
        .map(|d| d.sunset_at)
}

async fn disable_plugin(plugin_id: &str) -> Result<()> {
    let db = Database::open().await.context("open db")?;
    let row = db
        .get_wasm_plugin(plugin_id)
        .await?
        .ok_or_else(|| anyhow!("plugin '{plugin_id}' not installed"))?;
    if row.status == "quarantined" {
        bail!("plugin '{plugin_id}' is quarantined; use `borg plugin verify` instead of disable");
    }
    // Update the runtime kill-switch list in addition to flipping the row
    // status — the runtime guard reads the config list, not the DB column,
    // so a TUI / CLI restart must keep the disable in force.
    let mut config = Config::load_from_db().await.context("load config")?;
    if !config
        .plugins
        .wasm
        .disabled_plugin_ids
        .iter()
        .any(|p| p == plugin_id)
    {
        config
            .plugins
            .wasm
            .disabled_plugin_ids
            .push(plugin_id.to_string());
        let json = serde_json::to_string(&config.plugins.wasm.disabled_plugin_ids)?;
        db.set_setting("plugins.wasm.disabled_plugin_ids", &json)
            .await
            .context("persist disabled_plugin_ids")?;
    }
    db.disable_wasm_plugin(plugin_id)
        .await
        .context("set status=disabled")?;
    try_emit_global(
        audit_events::WASM_PLUGIN_DISABLED,
        plugin_id,
        &serde_json::json!({ "plugin_id": plugin_id, "scope": "plugin" }),
    )
    .await;
    println!("disabled {plugin_id}");
    Ok(())
}

async fn enable_plugin(plugin_id: &str) -> Result<()> {
    let db = Database::open().await.context("open db")?;
    let row = db
        .get_wasm_plugin(plugin_id)
        .await?
        .ok_or_else(|| anyhow!("plugin '{plugin_id}' not installed"))?;
    if row.status == "quarantined" {
        bail!(
            "plugin '{plugin_id}' is quarantined; `enable` does not clear quarantine. Run `borg plugin verify` after restoring the on-disk files."
        );
    }
    let mut config = Config::load_from_db().await.context("load config")?;
    let before = config.plugins.wasm.disabled_plugin_ids.len();
    config
        .plugins
        .wasm
        .disabled_plugin_ids
        .retain(|p| p != plugin_id);
    if config.plugins.wasm.disabled_plugin_ids.len() != before {
        let json = serde_json::to_string(&config.plugins.wasm.disabled_plugin_ids)?;
        db.set_setting("plugins.wasm.disabled_plugin_ids", &json)
            .await
            .context("persist disabled_plugin_ids")?;
    }
    db.enable_wasm_plugin(plugin_id)
        .await
        .context("set status=verified")?;
    try_emit_global(
        audit_events::WASM_PLUGIN_ENABLED,
        plugin_id,
        &serde_json::json!({ "plugin_id": plugin_id, "scope": "plugin" }),
    )
    .await;
    println!("enabled {plugin_id}");
    Ok(())
}

async fn disable_publisher(publisher_key: &str) -> Result<()> {
    if publisher_key.trim().is_empty() {
        bail!("publisher_key is required");
    }
    let db = Database::open().await.context("open db")?;
    let mut config = Config::load_from_db().await.context("load config")?;
    if !config
        .plugins
        .wasm
        .disabled_publisher_keys
        .iter()
        .any(|k| k == publisher_key)
    {
        config
            .plugins
            .wasm
            .disabled_publisher_keys
            .push(publisher_key.to_string());
        let json = serde_json::to_string(&config.plugins.wasm.disabled_publisher_keys)?;
        db.set_setting("plugins.wasm.disabled_publisher_keys", &json)
            .await
            .context("persist disabled_publisher_keys")?;
    }
    try_emit_global(
        audit_events::WASM_PLUGIN_DISABLED,
        "system",
        &serde_json::json!({
            "publisher_key": publisher_key,
            "scope": "publisher",
        }),
    )
    .await;
    println!("disabled all plugins signed by {publisher_key}");
    Ok(())
}

async fn enable_publisher(publisher_key: &str) -> Result<()> {
    let db = Database::open().await.context("open db")?;
    let mut config = Config::load_from_db().await.context("load config")?;
    let before = config.plugins.wasm.disabled_publisher_keys.len();
    config
        .plugins
        .wasm
        .disabled_publisher_keys
        .retain(|k| k != publisher_key);
    if config.plugins.wasm.disabled_publisher_keys.len() != before {
        let json = serde_json::to_string(&config.plugins.wasm.disabled_publisher_keys)?;
        db.set_setting("plugins.wasm.disabled_publisher_keys", &json)
            .await
            .context("persist disabled_publisher_keys")?;
    }
    try_emit_global(
        audit_events::WASM_PLUGIN_ENABLED,
        "system",
        &serde_json::json!({
            "publisher_key": publisher_key,
            "scope": "publisher",
        }),
    )
    .await;
    println!("enabled plugins signed by {publisher_key}");
    Ok(())
}

async fn list() -> Result<()> {
    let db = Database::open_readonly().await.context("open db")?;
    let rows = db.list_wasm_plugins().await.context("list wasm plugins")?;
    if rows.is_empty() {
        println!("(no WASM plugins installed)");
        return Ok(());
    }
    let config = Config::load_from_db().await.ok();
    let show_sunset = config
        .as_ref()
        .is_some_and(|c| !c.security.deprecated_publisher_keys.is_empty());
    if show_sunset {
        println!(
            "{:<32}  {:<10}  {:<5}  {:<12}  KEY_SUNSET",
            "ID", "STATUS", "TOOLS", "INSTALLED"
        );
    } else {
        println!("{:<32}  {:<10}  {:<5}  INSTALLED", "ID", "STATUS", "TOOLS");
    }
    for row in rows {
        let tools = db
            .count_wasm_plugin_tools(&row.plugin_id)
            .await
            .unwrap_or(0);
        let installed = chrono::DateTime::from_timestamp(row.installed_at, 0)
            .map(|dt| dt.format("%Y-%m-%d").to_string())
            .unwrap_or_else(|| "?".into());
        if show_sunset {
            let sunset = config
                .as_ref()
                .and_then(|c| deprecated_sunset_for(c, &row.publisher_key))
                .and_then(|ts| chrono::DateTime::from_timestamp(ts, 0))
                .map(|d| d.format("%Y-%m-%d").to_string())
                .unwrap_or_else(|| "-".into());
            println!(
                "{:<32}  {:<10}  {:<5}  {:<12}  {}",
                truncate(&row.plugin_id, 32),
                row.status,
                tools,
                installed,
                sunset,
            );
        } else {
            println!(
                "{:<32}  {:<10}  {:<5}  {}",
                truncate(&row.plugin_id, 32),
                row.status,
                tools,
                installed
            );
        }
    }
    Ok(())
}

async fn show(plugin_id: &str) -> Result<()> {
    let db = Database::open_readonly().await.context("open db")?;
    let row = db
        .get_wasm_plugin(plugin_id)
        .await?
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

async fn verify(plugin_id: &str) -> Result<()> {
    let db = Database::open().await.context("open db")?;
    let row = db
        .get_wasm_plugin(plugin_id)
        .await?
        .ok_or_else(|| anyhow!("plugin '{plugin_id}' not installed"))?;
    let manifest_bytes =
        std::fs::read(&row.manifest_path).with_context(|| format!("read {}", row.manifest_path))?;
    let wasm = std::fs::read(&row.wasm_path).with_context(|| format!("read {}", row.wasm_path))?;
    let key = db.derive_hmac_key(wasm_plugins::HMAC_DOMAIN);
    plugins::wasm::verify_installed_hmac(&key, &manifest_bytes, &wasm, &row.local_hmac)
        .context("local HMAC verification failed")?;
    db.restore_wasm_plugin(plugin_id)
        .await
        .context("clear quarantine flag")?;
    println!("verified {plugin_id}");
    Ok(())
}

async fn remove(plugin_id: &str) -> Result<()> {
    let outcome = wasm_plugins::remove(plugin_id).await?;
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
