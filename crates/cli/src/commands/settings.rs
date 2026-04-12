use anyhow::Result;

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
