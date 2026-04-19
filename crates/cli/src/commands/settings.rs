use anyhow::Result;
use clap::Subcommand;

#[derive(Subcommand)]
pub(crate) enum SettingsAction {
    /// Update a configuration setting (writes to DB, not config.toml)
    Set {
        /// Setting key (e.g. temperature, model, sandbox.enabled)
        key: String,
        /// New value
        value: String,
    },
    /// Show the effective value and source for a setting
    Get {
        /// Setting key
        key: String,
    },
    /// Remove a DB override, reverting to config.toml or default
    Unset {
        /// Setting key
        key: String,
    },
}

/// Dispatch for `borg settings ...`.
pub(crate) fn dispatch_settings(action: Option<SettingsAction>) -> Result<()> {
    match action {
        Some(SettingsAction::Set { key, value }) => run_settings_set(&key, &value),
        Some(SettingsAction::Get { key }) => run_settings_get(&key),
        Some(SettingsAction::Unset { key }) => run_settings_unset(&key),
        None => run_settings_show(),
    }
}

/// Dispatch for `borg logs ...` — activity log vs. conversation history.
pub(crate) fn dispatch_logs(
    count: usize,
    verbose: bool,
    activity: bool,
    level: &str,
    category: Option<&str>,
) -> Result<()> {
    if activity {
        run_activity_logs(count, level, category)
    } else {
        run_logs(count, verbose)
    }
}

pub(crate) fn run_settings_show() -> Result<()> {
    let resolver = borg_core::settings::SettingsResolver::load()?;
    let all = resolver.list_all()?;
    println!("Settings:");
    for info in &all {
        println!("  {:40} = {:20} [{}]", info.key, info.value, info.source);
    }
    Ok(())
}

pub(crate) fn run_settings_set(key: &str, value: &str) -> Result<()> {
    let resolver = borg_core::settings::SettingsResolver::load()?;
    let confirmation = resolver.set(key, value)?;
    println!("Updated: {confirmation}");
    Ok(())
}

pub(crate) fn run_settings_get(key: &str) -> Result<()> {
    let resolver = borg_core::settings::SettingsResolver::load()?;
    let (value, source) = resolver.get_with_source(key)?;
    println!("{key} = {value} [{source}]");
    Ok(())
}

pub(crate) fn run_settings_unset(key: &str) -> Result<()> {
    let resolver = borg_core::settings::SettingsResolver::load()?;
    resolver.unset(key)?;
    let (value, source) = resolver.get_with_source(key)?;
    println!("Unset {key} — effective value: {value} [{source}]");
    Ok(())
}

pub(crate) fn run_logs(count: usize, verbose: bool) -> Result<()> {
    let lines = borg_core::logging::read_history_formatted(count, verbose)?;
    if lines.is_empty() {
        println!("No conversation history.");
    } else {
        for line in &lines {
            println!("{line}");
        }
    }
    Ok(())
}

pub(crate) fn run_activity_logs(count: usize, level: &str, category: Option<&str>) -> Result<()> {
    let db = borg_core::db::Database::open()?;
    let entries = db.query_activity(count, Some(level), category)?;
    if entries.is_empty() {
        println!("No activity log entries.");
    } else {
        for entry in entries.iter().rev() {
            println!("{}", borg_core::activity_log::format_activity_entry(entry));
        }
    }
    Ok(())
}
