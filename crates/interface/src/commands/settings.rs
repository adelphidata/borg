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
pub(crate) async fn dispatch_settings(action: Option<SettingsAction>) -> Result<()> {
    match action {
        Some(SettingsAction::Set { key, value }) => run_settings_set(&key, &value).await,
        Some(SettingsAction::Get { key }) => run_settings_get(&key).await,
        Some(SettingsAction::Unset { key }) => run_settings_unset(&key).await,
        None => run_settings_show().await,
    }
}

/// Dispatch for `borg logs ...` — activity log vs. conversation history.
pub(crate) async fn dispatch_logs(
    count: usize,
    verbose: bool,
    activity: bool,
    level: &str,
    category: Option<&str>,
) -> Result<()> {
    if activity {
        run_activity_logs(count, level, category).await
    } else {
        run_logs(count, verbose)
    }
}

pub(crate) async fn run_settings_show() -> Result<()> {
    let resolver = cortex::settings::SettingsResolver::load().await?;
    let all = resolver.list_all().await?;
    println!("Settings:");
    for info in &all {
        println!("  {:40} = {:20} [{}]", info.key, info.value, info.source);
    }
    Ok(())
}

pub(crate) async fn run_settings_set(key: &str, value: &str) -> Result<()> {
    let resolver = cortex::settings::SettingsResolver::load().await?;
    let confirmation = resolver.set(key, value).await?;
    println!("Updated: {confirmation}");
    Ok(())
}

pub(crate) async fn run_settings_get(key: &str) -> Result<()> {
    let resolver = cortex::settings::SettingsResolver::load().await?;
    let (value, source) = resolver.get_with_source(key).await?;
    println!("{key} = {value} [{source}]");
    Ok(())
}

pub(crate) async fn run_settings_unset(key: &str) -> Result<()> {
    let resolver = cortex::settings::SettingsResolver::load().await?;
    resolver.unset(key).await?;
    let (value, source) = resolver.get_with_source(key).await?;
    println!("Unset {key} — effective value: {value} [{source}]");
    Ok(())
}

pub(crate) fn run_logs(count: usize, verbose: bool) -> Result<()> {
    let lines = cortex::logging::read_history_formatted(count, verbose)?;
    if lines.is_empty() {
        println!("No conversation history.");
    } else {
        for line in &lines {
            println!("{line}");
        }
    }
    Ok(())
}

pub(crate) async fn run_activity_logs(
    count: usize,
    level: &str,
    category: Option<&str>,
) -> Result<()> {
    let db = cortex::db::Database::open_readonly().await?;
    let entries = db.query_activity(count, Some(level), category).await?;
    if entries.is_empty() {
        println!("No activity log entries.");
    } else {
        for entry in entries.iter().rev() {
            println!("{}", cortex::activity_log::format_activity_entry(entry));
        }
    }
    Ok(())
}
