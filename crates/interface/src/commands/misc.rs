use anyhow::{Context, Result};
use clap::Subcommand;

#[derive(Subcommand)]
pub(crate) enum MigrateSubcommand {
    /// Migrate from Hermes Agent (~/.hermes/)
    Hermes,
    /// Migrate from OpenClaw (~/.openclaw/)
    Claw,
    /// Import memory from Claude Code (~/.claude/)
    Claude,
    /// Import rules from Cursor (~/.cursor/)
    Cursor,
    /// Import instructions from an AGENTS.md-based CLI (~/.codex/)
    Agents,
}

#[derive(Subcommand)]
pub(crate) enum ServiceAction {
    /// Install the daemon as a launchd/systemd service
    Install {
        /// Linux only: install gnome-keyring + dbus-user-session so the
        /// daemon stores its KEK in the OS secret-service backend rather
        /// than a passphrase-encrypted file. Requires apt-get; on
        /// non-Debian systems, falls back to the file backend without
        /// failing — the agent always boots.
        #[clap(long)]
        secure: bool,
        /// Windows only: provision the Tier 2 elevated sandbox (dedicated
        /// `BorgSandboxOnline`/`BorgSandboxOffline` accounts + WFP
        /// outbound block for the offline account). Must run from an
        /// elevated (Administrator) terminal. On other platforms this
        /// flag errors with a clear message.
        #[clap(long)]
        windows_sandbox: bool,
    },
    /// Uninstall the daemon service
    Uninstall {
        /// Windows only: also remove the Tier 2 elevated sandbox
        /// (accounts, WFP rules, sealed credentials, setup.json).
        #[clap(long)]
        windows_sandbox: bool,
    },
    /// Show the daemon service status
    Status,
    /// Start the daemon service (via the installed unit, or direct spawn).
    Start,
    /// Stop the daemon service.
    Stop,
    /// Restart the daemon service
    Restart,
}

/// Dispatch for `borg migrate ...` — either the interactive TUI or a direct
/// migration from the named source.
pub(crate) async fn dispatch_migrate(action: Option<MigrateSubcommand>) -> Result<()> {
    match action {
        None => crate::migrate_tui::run(),
        Some(MigrateSubcommand::Hermes) => {
            run_migrate_direct(cortex::migrate::MigrationSource::Hermes).await
        }
        Some(MigrateSubcommand::Claw) => {
            run_migrate_direct(cortex::migrate::MigrationSource::OpenClaw).await
        }
        Some(MigrateSubcommand::Claude) => {
            run_migrate_direct(cortex::migrate::MigrationSource::ClaudeCode).await
        }
        Some(MigrateSubcommand::Cursor) => {
            run_migrate_direct(cortex::migrate::MigrationSource::Cursor).await
        }
        Some(MigrateSubcommand::Agents) => {
            run_migrate_direct(cortex::migrate::MigrationSource::AgentsMd).await
        }
    }
}

/// Dispatch for `borg service ...`.
pub(crate) async fn dispatch_service(action: ServiceAction) -> Result<()> {
    match action {
        ServiceAction::Install {
            secure,
            windows_sandbox,
        } => {
            if windows_sandbox {
                crate::borgd_launcher::install_windows_sandbox()?;
            }
            crate::borgd_launcher::install_service(secure)
        }
        ServiceAction::Uninstall { windows_sandbox } => {
            if windows_sandbox {
                crate::borgd_launcher::uninstall_windows_sandbox()?;
            }
            crate::borgd_launcher::uninstall_service()
        }
        ServiceAction::Status => crate::borgd_launcher::service_status().await,
        ServiceAction::Start => crate::borgd_launcher::start_service().await,
        ServiceAction::Stop => crate::borgd_launcher::stop_service().await,
        ServiceAction::Restart => crate::borgd_launcher::restart_service().await,
    }
}

pub(crate) async fn run_doctor() -> Result<()> {
    let config = cortex::config::Config::load_from_db()
        .await
        .unwrap_or_default();
    let report = cortex::doctor::run_diagnostics(&config).await;
    println!("{}", report.format());
    let (_pass, _warn, fail) = report.counts();
    if fail > 0 {
        std::process::exit(1);
    }
    Ok(())
}

pub(crate) async fn run_update(prerelease: bool, check: bool) -> Result<()> {
    let current = cortex::update::current_version();
    println!("Current version: {current}");
    println!(
        "Checking for updates{}...",
        if prerelease {
            " (including pre-releases)"
        } else {
            ""
        }
    );

    if check {
        let release = cortex::update::fetch_latest_release(prerelease).await?;
        let latest = release
            .tag_name
            .strip_prefix('v')
            .unwrap_or(&release.tag_name);
        if !cortex::update::is_newer(current, latest) {
            println!("Already up to date ({current})");
        } else {
            println!("Update available: {current} → {latest}");
        }
        return Ok(());
    }

    match cortex::update::perform_update(prerelease).await? {
        cortex::update::UpdateResult {
            status: cortex::update::UpdateStatus::AlreadyUpToDate,
            current_version,
            ..
        } => {
            println!("Already up to date ({current_version})");
        }
        cortex::update::UpdateResult {
            status: cortex::update::UpdateStatus::Updated { from, to },
            ..
        } => {
            println!("Updated borg: {from} → {to}");
            println!("Restart borg to use the new version.");
        }
    }
    Ok(())
}

pub(crate) async fn run_poke() -> Result<()> {
    // borgd is the only way to reach the heartbeat runner. There used to be an
    // HTTP fallback here for installs where borgd had not taken over yet; no
    // such install exists, and the fallback only served to turn "the daemon is
    // not running" into a success-shaped message.
    let socket = crate::daemon_client::default_socket_path();
    if !socket.exists() {
        anyhow::bail!("borg is not running. Start it with: borg service start");
    }
    // Propagate errors rather than print-and-return-Ok — the CLI exit code is
    // the user's only signal that the poke didn't actually fire.
    let mut client = crate::daemon_client::connect(Some(&socket))
        .await
        .context("connect to borgd")?;
    client.poke().await.context("Admin.Poke via borgd")?;
    println!("Poke signal sent.");
    Ok(())
}

pub(crate) async fn run_dream() -> Result<()> {
    let mut client = crate::daemon_client::connect_default().await?;
    let report = client.dream().await.context("Admin.Dream via borgd")?;
    println!("{}", format_dream_report(&report));
    Ok(())
}

pub(crate) fn format_dream_report(report: &protocols::admin::DreamReport) -> String {
    let mut text = format!(
        "Dream complete\n──────────────\nEntries extracted: {}\nDupes merged: {}\nTokens used: {}",
        report.entries_extracted, report.dupes_merged, report.tokens_used
    );
    if !report.nightly_summary.is_empty() {
        text.push_str("\nNightly: ");
        text.push_str(&report.nightly_summary);
    }
    if !report.weekly_summary.is_empty() {
        text.push_str("\nWeekly: ");
        text.push_str(&report.weekly_summary);
    }
    if !report.warnings.is_empty() {
        text.push_str("\nWarnings:");
        for warning in &report.warnings {
            text.push_str("\n  - ");
            text.push_str(warning);
        }
    }
    text
}

pub(crate) async fn run_cancel(session: Option<String>) -> Result<()> {
    let config = cortex::config::Config::load_from_db().await?;
    let url = format!(
        "http://{}:{}/internal/cancel",
        config.gateway.host, config.gateway.port
    );
    let client = cortex::http::shared();
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
    let config = cortex::config::Config::load_from_db().await?;
    let url = format!(
        "http://{}:{}/internal/away",
        config.gateway.host, config.gateway.port
    );
    let client = cortex::http::shared();
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
    let config = cortex::config::Config::load_from_db().await?;
    let url = format!(
        "http://{}:{}/internal/available",
        config.gateway.host, config.gateway.port
    );
    let client = cortex::http::shared();
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

pub(crate) async fn run_migrate_direct(source: cortex::migrate::MigrationSource) -> Result<()> {
    use cortex::migrate::{self, MigrationCategories};

    if !source.is_installed() {
        anyhow::bail!(
            "{} not found at {}",
            source.label(),
            source.data_dir().display()
        );
    }

    let categories = MigrationCategories::default();
    let data = migrate::parse_source(source, &categories)?;
    let config = cortex::config::Config::load_from_db()
        .await
        .unwrap_or_default();
    let borg_dir = cortex::config::Config::data_dir()?;
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

    let result = migrate::apply::apply_plan(&plan, &data, &borg_dir).await?;
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
    let data_dir = cortex::config::Config::data_dir()?;

    eprintln!(
        "WARNING: This will permanently delete all Borg data at {}\n\
         including config, memory, tools, skills, channels, and database.\n",
        data_dir.display()
    );
    eprint!("Continue? [y/N] ");

    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;

    if confirm_uninstall(input.trim()) {
        if let Err(e) = crate::borgd_launcher::uninstall_service() {
            tracing::debug!("Service uninstall skipped: {e}");
        }
        crate::borgd_launcher::kill_other_borg_processes();

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
