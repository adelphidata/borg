//! `borg backup create / verify / restore` — `BORG_HOME` backup unit.
//!
//! All three commands operate directly on the filesystem without going
//! through the daemon, so they work even when `borgd` is stopped (which
//! is the common case for `restore`). `verify` opens a read-only
//! SQLite handle on the extracted DB; it never touches the live one.

use std::path::Path;

use anyhow::{Context, Result};

use cortex::backup::{create_backup, live_schema_version, restore_backup, verify_backup};
use cortex::config::Config;

pub fn run_create() -> Result<()> {
    let home = Config::data_dir().context("locate BORG_HOME")?;
    let out_dir = home.join("backups");
    let schema_version = live_schema_version()?;
    let path = create_backup(&home, &out_dir, schema_version).context("create backup")?;
    println!("backup: {}", path.display());
    Ok(())
}

pub fn run_verify(file: &Path) -> Result<()> {
    let report = verify_backup(file).context("verify backup")?;
    println!(
        "backup OK: schema_version={}  created_at={}  messages={}  audit={}",
        report.manifest.schema_version,
        report.manifest.created_at,
        report.messages_count,
        match &report.audit_outcome {
            cortex::db::AuditChainOutcome::Ok => "ok".to_string(),
            cortex::db::AuditChainOutcome::OkWithTombstones { count } => {
                format!("ok+{count}-tombstones")
            }
            cortex::db::AuditChainOutcome::Tampered { seq } => {
                format!("TAMPERED({seq})")
            }
        }
    );
    Ok(())
}

pub fn run_restore(file: &Path, force: bool) -> Result<()> {
    let home = Config::data_dir().context("locate BORG_HOME")?;
    restore_backup(file, &home, force).context("restore backup")?;
    println!("restored {} into {}", file.display(), home.display());
    Ok(())
}
