use anyhow::Result;
use clap::Subcommand;

#[derive(Subcommand)]
pub(crate) enum MigrateSubcommand {
    /// Migrate from Hermes Agent (~/.hermes/)
    Hermes,
    /// Migrate from OpenClaw (~/.openclaw/)
    Claw,
}

#[derive(Subcommand)]
pub(crate) enum ServiceAction {
    /// Uninstall the daemon service
    Uninstall,
    /// Show the daemon service status
    Status,
}

/// Dispatch for `borg migrate ...` — either the interactive TUI or a direct
/// migration from the named source.
pub(crate) fn dispatch_migrate(action: Option<MigrateSubcommand>) -> Result<()> {
    match action {
        None => crate::migrate_tui::run(),
        Some(MigrateSubcommand::Hermes) => {
            run_migrate_direct(borg_core::migrate::MigrationSource::Hermes)
        }
        Some(MigrateSubcommand::Claw) => {
            run_migrate_direct(borg_core::migrate::MigrationSource::OpenClaw)
        }
    }
}

/// Dispatch for `borg service ...`.
pub(crate) fn dispatch_service(action: ServiceAction) -> Result<()> {
    match action {
        ServiceAction::Uninstall => crate::service::uninstall_service(),
        ServiceAction::Status => crate::service::service_status(),
    }
}

pub(crate) fn run_doctor() -> Result<()> {
    let config = borg_core::config::Config::load_from_db().unwrap_or_default();
    let report = borg_core::doctor::run_diagnostics(&config);
    println!("{}", report.format());
    let (_pass, _warn, fail) = report.counts();
    if fail > 0 {
        std::process::exit(1);
    }
    Ok(())
}

pub(crate) async fn run_update(dev: bool, check: bool) -> Result<()> {
    let current = borg_core::update::current_version();
    println!("Current version: {current}");
    println!(
        "Checking for updates{}...",
        if dev { " (including pre-releases)" } else { "" }
    );

    if check {
        let release = borg_core::update::fetch_latest_release(dev).await?;
        let latest = release
            .tag_name
            .strip_prefix('v')
            .unwrap_or(&release.tag_name);
        if !borg_core::update::is_newer(current, latest) {
            println!("Already up to date ({current})");
        } else {
            println!("Update available: {current} → {latest}");
        }
        return Ok(());
    }

    match borg_core::update::perform_update(dev).await? {
        borg_core::update::UpdateResult {
            status: borg_core::update::UpdateStatus::AlreadyUpToDate,
            current_version,
            ..
        } => {
            println!("Already up to date ({current_version})");
        }
        borg_core::update::UpdateResult {
            status: borg_core::update::UpdateStatus::Updated { from, to },
            ..
        } => {
            println!("Updated borg: {from} → {to}");
            println!("Restart borg to use the new version.");
        }
    }
    Ok(())
}

pub(crate) async fn run_poke() -> Result<()> {
    let config = borg_core::config::Config::load_from_db()?;
    let url = format!(
        "http://{}:{}/internal/poke",
        config.gateway.host, config.gateway.port
    );
    let client = reqwest::Client::new();
    match client
        .post(&url)
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await
    {
        Ok(r) if r.status().is_success() => println!("Poke signal sent."),
        Ok(r) => println!("Poke failed: {}", r.status()),
        Err(_) => {
            println!("Could not reach daemon. Is it running?");
            println!("Start it with: borg service start");
        }
    }
    Ok(())
}

pub(crate) async fn run_cancel(session: Option<String>) -> Result<()> {
    let config = borg_core::config::Config::load_from_db()?;
    let url = format!(
        "http://{}:{}/internal/cancel",
        config.gateway.host, config.gateway.port
    );
    let client = reqwest::Client::new();
    let mut req = client.post(&url).timeout(std::time::Duration::from_secs(5));
    if let Some(ref sid) = session {
        req = req.query(&[("session", sid)]);
    }
    match req.send().await {
        Ok(r) if r.status().is_success() => {
            let body: serde_json::Value = r.json().await.unwrap_or_default();
            let count = body
                .get("cancelled")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0);
            if count == 0 {
                println!("Nothing to cancel.");
            } else if count == 1 {
                println!("Cancelled 1 in-flight turn.");
            } else {
                println!("Cancelled {count} in-flight turns.");
            }
        }
        Ok(r) => println!("Cancel failed: {}", r.status()),
        Err(_) => {
            println!("Could not reach daemon. Is it running?");
            println!("Start it with: borg service start");
        }
    }
    Ok(())
}

pub(crate) async fn run_away(message: Option<String>) -> Result<()> {
    let config = borg_core::config::Config::load_from_db()?;
    let url = format!(
        "http://{}:{}/internal/away",
        config.gateway.host, config.gateway.port
    );
    let client = reqwest::Client::new();
    let mut req = client.post(&url).timeout(std::time::Duration::from_secs(5));
    if let Some(msg) = &message {
        req = req
            .header("Content-Type", "application/json")
            .body(serde_json::json!({"message": msg}).to_string());
    }
    match req.send().await {
        Ok(r) if r.status().is_success() => {
            let body: serde_json::Value = r.json().await.unwrap_or_default();
            let msg = body
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("(default)");
            println!("Agent set to away: {msg}");
        }
        Ok(r) => println!("Away failed: {}", r.status()),
        Err(_) => {
            println!("Could not reach gateway. Is it running?");
        }
    }
    Ok(())
}

pub(crate) async fn run_available() -> Result<()> {
    let config = borg_core::config::Config::load_from_db()?;
    let url = format!(
        "http://{}:{}/internal/available",
        config.gateway.host, config.gateway.port
    );
    let client = reqwest::Client::new();
    match client
        .post(&url)
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await
    {
        Ok(r) if r.status().is_success() => println!("Agent set to available."),
        Ok(r) => println!("Available failed: {}", r.status()),
        Err(_) => {
            println!("Could not reach gateway. Is it running?");
        }
    }
    Ok(())
}

pub(crate) fn run_migrate_direct(source: borg_core::migrate::MigrationSource) -> Result<()> {
    use borg_core::migrate::{self, MigrationCategories};

    if !source.is_installed() {
        anyhow::bail!(
            "{} not found at {}",
            source.label(),
            source.data_dir().display()
        );
    }

    let categories = MigrationCategories::default();
    let data = migrate::parse_source(source, &categories)?;
    let config = borg_core::config::Config::load_from_db().unwrap_or_default();
    let borg_dir = borg_core::config::Config::data_dir()?;
    let plan = migrate::plan::build_plan(source, &data, &config, &borg_dir);

    if plan.is_empty() {
        eprintln!("Nothing to migrate from {}.", source.label());
        return Ok(());
    }

    eprintln!("Migration plan from {}:", source.label());
    for line in plan.summary_lines() {
        eprintln!("  {line}");
    }
    eprintln!();

    eprint!("Apply migration? [y/N] ");
    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;
    if !input.trim().eq_ignore_ascii_case("y") {
        eprintln!("Cancelled.");
        return Ok(());
    }

    let result = migrate::apply::apply_plan(&plan, &data, &borg_dir)?;
    eprintln!("Migration complete:");
    if result.config_changes_applied > 0 {
        eprintln!(
            "  {} config change(s) applied",
            result.config_changes_applied
        );
    }
    if result.credentials_added > 0 {
        eprintln!("  {} credential(s) added", result.credentials_added);
    }
    if result.memory_files_copied > 0 {
        eprintln!("  {} memory file(s) copied", result.memory_files_copied);
    }
    if result.persona_copied {
        eprintln!("  Persona copied to IDENTITY.md");
    }
    if result.skills_copied > 0 {
        eprintln!("  {} skill(s) copied", result.skills_copied);
    }
    for warning in &result.warnings {
        eprintln!("  Warning: {warning}");
    }

    Ok(())
}

pub(crate) fn run_uninstall() -> Result<()> {
    let data_dir = borg_core::config::Config::data_dir()?;

    eprintln!(
        "WARNING: This will permanently delete all Borg data at {}\n\
         including config, memory, tools, skills, channels, and database.\n",
        data_dir.display()
    );
    eprint!("Continue? [y/N] ");

    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;

    if confirm_uninstall(input.trim()) {
        if let Err(e) = crate::service::uninstall_service() {
            tracing::debug!("Service uninstall skipped: {e}");
        }
        crate::service::kill_other_borg_processes();

        delete_data_dir(&data_dir)?;

        if let Ok(exe) = std::env::current_exe() {
            let exe = exe.canonicalize().unwrap_or(exe);
            if let Err(e) = std::fs::remove_file(&exe) {
                tracing::debug!("Could not remove binary: {e}");
            }
        }

        println!("Borg data deleted. Goodbye!");
    } else {
        println!("Aborted.");
    }

    Ok(())
}

pub(crate) fn confirm_uninstall(input: &str) -> bool {
    input.eq_ignore_ascii_case("y")
}

pub(crate) fn delete_data_dir(data_dir: &std::path::Path) -> Result<()> {
    if data_dir.exists() {
        std::fs::remove_dir_all(data_dir)?;
    }
    Ok(())
}
