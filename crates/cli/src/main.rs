use std::io::IsTerminal;

use anyhow::Result;
use clap::{Parser, Subcommand};
use tokio_util::sync::CancellationToken;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;
use uuid::Uuid;

mod credentials;
mod heartbeat_augmenters;
mod logo;
mod migrate_tui;
mod onboarding;
mod onboarding_tui;
mod plugins;
mod repl;
mod service;
mod tui;

/// Format a Unix timestamp for display. Returns "?" if invalid.
fn format_ts(ts: i64, fmt: &str) -> String {
    chrono::DateTime::from_timestamp(ts, 0)
        .map(|dt| dt.format(fmt).to_string())
        .unwrap_or_else(|| "?".to_string())
}

/// Return the first 8 characters of an ID for compact display.
fn short_id(id: &str) -> &str {
    &id[..8.min(id.len())]
}

#[derive(Parser)]
#[command(name = "borg", about = "AI Personal Assistant Agent", version)]
struct Cli {
    /// Resume a specific session by ID (full UUID or unique prefix).
    /// Only applies when launching the interactive TUI.
    #[arg(long, global = true, value_name = "SESSION_ID")]
    resume: Option<String>,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Start Borg — interactive TUI with auto-gateway (default)
    Start,
    /// Stop the background daemon service
    Stop,
    /// Restart the background daemon service
    Restart,
    /// Run interactive setup wizard for ~/.borg
    Init,
    /// Send a single message and exit
    Ask {
        /// The message to send
        message: String,
        /// Auto-approve all tool/shell calls (no user prompts)
        #[arg(long, short)]
        yes: bool,
        /// Output raw JSON instead of streaming text
        #[arg(long, short)]
        json: bool,
        /// Collaboration mode: default, execute, plan
        #[arg(long, short)]
        mode: Option<String>,
    },
    /// Show agent vitals and status
    Status {
        #[command(subcommand)]
        action: Option<StatusAction>,
    },
    /// Show bond status and trust metrics
    Bond {
        #[command(subcommand)]
        action: Option<BondAction>,
    },
    /// Run diagnostics to check configuration, connectivity, and dependencies
    Doctor,
    /// Run as a background daemon (executes scheduled tasks and heartbeat)
    Daemon,
    /// Manage the daemon as a system service
    Service {
        #[command(subcommand)]
        action: ServiceAction,
    },
    /// Set up an integration (e.g. borg add telegram)
    Add {
        /// Integration name
        name: String,
    },
    /// Remove an integration's credentials
    Remove {
        /// Integration name
        name: String,
    },
    /// List all available integrations and their status
    Plugins,
    /// Show or update configuration settings
    Settings {
        #[command(subcommand)]
        action: Option<SettingsAction>,
    },
    /// Show recent conversation history or activity log
    Logs {
        /// Number of log entries to show
        #[arg(long, short, default_value_t = 50)]
        count: usize,
        /// Show full untruncated content
        #[arg(long, short)]
        verbose: bool,
        /// Show structured activity log instead of conversation history
        #[arg(long, short)]
        activity: bool,
        /// Minimum level filter for activity log (error, warn, info, debug)
        #[arg(long, default_value = "info")]
        level: String,
        /// Filter activity log by category
        #[arg(long)]
        category: Option<String>,
    },
    /// Manage scheduled tasks
    Tasks {
        #[command(subcommand)]
        action: Option<TasksAction>,
    },
    /// Show token usage and cost breakdown for the current month
    Usage,
    /// Manage sender pairing and access control for messaging channels
    Pairing {
        #[command(subcommand)]
        action: Option<PairingAction>,
    },
    /// Trigger an immediate heartbeat check-in
    #[command(alias = "wake")]
    Poke,
    /// Cancel the in-progress agent turn
    ///
    /// Sends a cancel signal to the running daemon/gateway. With `--session`,
    /// cancels only that session's in-flight turn; otherwise cancels all.
    /// Note: `stop` is already taken by the daemon stop command, so only
    /// `abort` is offered as an alias here.
    #[command(alias = "abort")]
    Cancel {
        /// Session ID to target (default: cancel all in-flight turns)
        #[arg(long)]
        session: Option<String>,
    },
    /// Set the agent to away mode (auto-replies to messages)
    Away {
        /// Custom away message (uses config default if omitted)
        message: Option<String>,
    },
    /// Set the agent back to available mode (stops auto-replying)
    Available,
    /// Migrate settings from Hermes Agent or OpenClaw
    Migrate {
        #[command(subcommand)]
        action: Option<MigrateSubcommand>,
    },
    /// Update borg to the latest release
    Update {
        /// Install latest pre-release/dev build instead of stable
        #[arg(long)]
        dev: bool,
        /// Only check for updates, don't install
        #[arg(long)]
        check: bool,
    },
    /// Manage projects (group related workflows and workstreams)
    Projects {
        #[command(subcommand)]
        action: Option<ProjectsAction>,
    },
    /// Manage cron jobs (shell commands on a schedule)
    Cron {
        #[command(subcommand)]
        action: Option<CronAction>,
    },
    /// Permanently delete all Borg data and uninstall the service
    Uninstall,
}

#[derive(Subcommand)]
enum MigrateSubcommand {
    /// Migrate from Hermes Agent (~/.hermes/)
    Hermes,
    /// Migrate from OpenClaw (~/.openclaw/)
    Claw,
}

#[derive(Subcommand)]
enum BondAction {
    /// Show recent bond event history
    History {
        /// Number of events to show
        #[arg(long, short, default_value_t = 20)]
        count: usize,
    },
}

#[derive(Subcommand)]
enum StatusAction {
    /// Show evolution history timeline
    History,
    /// Show archetype score breakdown
    Archetypes,
}

#[derive(Subcommand)]
enum SettingsAction {
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

#[derive(Subcommand)]
enum TasksAction {
    /// List all scheduled tasks
    List,
    /// Create a new scheduled task
    Create {
        /// Task name
        #[arg(long, short)]
        name: String,
        /// Prompt to send to the agent
        #[arg(long, short)]
        prompt: String,
        /// Schedule expression (cron or interval)
        #[arg(long, short)]
        schedule: String,
        /// Schedule type: cron, interval, or once
        #[arg(long, short = 't', default_value = "cron")]
        r#type: String,
        /// Max retry attempts for transient failures (default: 3)
        #[arg(long)]
        max_retries: Option<i32>,
        /// Timeout in seconds (default: 300)
        #[arg(long)]
        timeout: Option<u64>,
        /// Delivery channel for results (telegram, slack, discord)
        #[arg(long)]
        delivery_channel: Option<String>,
        /// Delivery target (chat_id or channel_id)
        #[arg(long)]
        delivery_target: Option<String>,
    },
    /// Delete a scheduled task
    Delete {
        /// Task ID (or prefix)
        id: String,
    },
    /// Pause a scheduled task
    Pause {
        /// Task ID (or prefix)
        id: String,
    },
    /// Resume a paused task
    Resume {
        /// Task ID (or prefix)
        id: String,
    },
    /// Trigger a task to run immediately
    Run {
        /// Task ID (or prefix)
        id: String,
    },
    /// Show execution history for a task
    Runs {
        /// Task ID (or prefix)
        id: String,
        /// Number of runs to show
        #[arg(long, short, default_value_t = 10)]
        count: usize,
    },
    /// Show detailed task status
    Status {
        /// Task ID (or prefix)
        id: String,
    },
}

#[derive(Subcommand)]
enum ServiceAction {
    /// Uninstall the daemon service
    Uninstall,
    /// Show the daemon service status
    Status,
}

#[derive(Subcommand)]
enum PairingAction {
    /// List pending pairing requests
    List {
        /// Filter by channel name
        channel: Option<String>,
    },
    /// Approve a pairing request by code
    Approve {
        /// Pairing code with channel prefix (e.g. TG_H4BRWMRW)
        code: String,
    },
    /// Revoke an approved sender
    Revoke {
        /// Channel name
        channel: String,
        /// Sender ID to revoke
        sender_id: String,
    },
    /// List all approved senders
    Approved {
        /// Filter by channel name
        channel: Option<String>,
    },
}

#[derive(Subcommand)]
enum CronAction {
    /// List all cron jobs
    List,
    /// Add a cron job. Use combined format: "*/5 * * * * echo hello"
    /// or separate flags: -s "*/5 * * * *" -c "echo hello"
    Add {
        /// Combined crontab line: "*/5 * * * * command args..."
        line: Option<String>,
        /// Cron schedule (5-field Linux format, e.g. "*/5 * * * *")
        #[arg(long, short)]
        schedule: Option<String>,
        /// Shell command to execute
        #[arg(long, short)]
        command: Option<String>,
        /// Job name (auto-generated from command if omitted)
        #[arg(long, short)]
        name: Option<String>,
        /// Timeout in seconds (default: 300)
        #[arg(long)]
        timeout: Option<u64>,
        /// Delivery channel for output (telegram, slack, discord)
        #[arg(long)]
        delivery_channel: Option<String>,
        /// Delivery target (chat_id or channel_id)
        #[arg(long)]
        delivery_target: Option<String>,
    },
    /// Remove a cron job
    Remove {
        /// Job ID (or prefix)
        id: String,
    },
    /// Pause a cron job
    Pause {
        /// Job ID (or prefix)
        id: String,
    },
    /// Resume a paused cron job
    Resume {
        /// Job ID (or prefix)
        id: String,
    },
    /// Trigger a cron job to run immediately
    Run {
        /// Job ID (or prefix)
        id: String,
    },
    /// Show execution history for a cron job
    Runs {
        /// Job ID (or prefix)
        id: String,
        /// Number of runs to show
        #[arg(long, short, default_value_t = 10)]
        count: usize,
    },
}

#[derive(Subcommand)]
enum ProjectsAction {
    /// List all projects
    List {
        /// Filter by status (active, archived)
        #[arg(long, short)]
        status: Option<String>,
    },
    /// Create a new project
    Create {
        /// Project name
        #[arg(long, short)]
        name: String,
        /// Project description
        #[arg(long, short)]
        description: Option<String>,
    },
    /// Show project details and associated workflows
    Get {
        /// Project ID (or prefix)
        id: String,
    },
    /// Update a project's fields
    Update {
        /// Project ID (or prefix)
        id: String,
        /// New project name
        #[arg(long, short)]
        name: Option<String>,
        /// New project description
        #[arg(long, short)]
        description: Option<String>,
        /// New status (active, archived)
        #[arg(long, short)]
        status: Option<String>,
    },
    /// Archive a project
    Archive {
        /// Project ID (or prefix)
        id: String,
    },
    /// Delete a project
    Delete {
        /// Project ID (or prefix)
        id: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let _ = dotenvy::dotenv();
    // Also load .env from the data directory (~/.borg/.env)
    if let Ok(data_dir) = borg_core::config::Config::data_dir() {
        let _ = dotenvy::from_path(data_dir.join(".env"));
        // Harden data directory permissions (owner-only access)
        harden_data_dir(&data_dir);
    }

    let cli = Cli::parse();

    let tui_mode = matches!(cli.command, Some(Commands::Start) | None);

    // _guard and _telemetry_guard must live for the program's duration to flush logs
    let _guard;
    let _telemetry_guard;

    {
        let config = borg_core::config::Config::load_from_db().unwrap_or_default();
        let (otel_layer, tg) = borg_core::telemetry::init_telemetry(&config.telemetry)
            .unwrap_or_else(|e| {
                eprintln!("Warning: OpenTelemetry init failed: {e}");
                (None, borg_core::telemetry::TelemetryGuard::noop())
            });
        _telemetry_guard = tg;

        let env_filter = EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| EnvFilter::new("warn,chromiumoxide=error,tungstenite=error"));

        if tui_mode {
            let (non_blocking, guard) = match borg_core::config::Config::logs_dir() {
                Ok(log_dir) => {
                    let _ = std::fs::create_dir_all(&log_dir);
                    let mut opts = std::fs::OpenOptions::new();
                    opts.create(true).append(true);
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::OpenOptionsExt;
                        opts.mode(0o600);
                    }
                    match opts.open(log_dir.join("tui.log")) {
                        Ok(f) => tracing_appender::non_blocking(f),
                        Err(e) => {
                            eprintln!("Warning: could not open log file: {e}");
                            tracing_appender::non_blocking(std::io::sink())
                        }
                    }
                }
                Err(_) => tracing_appender::non_blocking(std::io::sink()),
            };
            _guard = Some(guard);
            let fmt_layer = tracing_subscriber::fmt::layer()
                .with_target(false)
                .with_ansi(false)
                .with_writer(non_blocking);
            tracing_subscriber::registry()
                .with(otel_layer)
                .with(env_filter)
                .with(fmt_layer)
                .init();
        } else {
            _guard = None;
            let fmt_layer = tracing_subscriber::fmt::layer().with_target(false);
            tracing_subscriber::registry()
                .with(otel_layer)
                .with(env_filter)
                .with(fmt_layer)
                .init();
        }
    }

    // Set up a global cancellation token for graceful shutdown
    let shutdown = CancellationToken::new();
    {
        let shutdown = shutdown.clone();
        tokio::spawn(async move {
            let ctrl_c = tokio::signal::ctrl_c();
            #[cfg(unix)]
            {
                use tokio::signal::unix::{signal, SignalKind};
                let Ok(mut sigterm) = signal(SignalKind::terminate()) else {
                    // If SIGTERM handler fails, just listen for ctrl_c
                    let _ = ctrl_c.await;
                    shutdown.cancel();
                    return;
                };
                tokio::select! {
                    _ = ctrl_c => {},
                    _ = sigterm.recv() => {},
                }
            }
            #[cfg(not(unix))]
            {
                let _ = ctrl_c.await;
            }
            shutdown.cancel();
        });
    }

    // Ensure daemon service is installed and running on every command
    // (skip commands that manage the service themselves)
    if !matches!(
        cli.command,
        Some(Commands::Daemon)
            | Some(Commands::Init)
            | Some(Commands::Uninstall)
            | Some(Commands::Service { .. })
    ) {
        if let Err(e) = service::ensure_service_running() {
            tracing::warn!("Auto-start service: {e}");
        }
    }

    match cli.command {
        Some(Commands::Start) | None => {
            ensure_onboarded()?;
            if let Some(hint) = repl::run(cli.resume.clone()).await? {
                println!();
                println!("Session: {} ({})", hint.title, hint.short_id);
                println!("Resume with: borg --resume {}", hint.short_id);
            }
        }
        Some(Commands::Stop) => service::stop_service()?,
        Some(Commands::Restart) => service::restart_service()?,
        Some(Commands::Init) => init_data_dir()?,
        Some(Commands::Ask {
            message,
            yes,
            json,
            mode,
        }) => repl::one_shot(&message, yes, json, mode.as_deref()).await?,
        Some(Commands::Status { action }) => match action {
            None => run_status()?,
            Some(StatusAction::History) => run_status_history()?,
            Some(StatusAction::Archetypes) => run_status_archetypes()?,
        },
        Some(Commands::Bond { action }) => match action {
            Some(BondAction::History { count }) => run_bond_history(count)?,
            None => run_bond_status()?,
        },
        Some(Commands::Doctor) => run_doctor()?,
        Some(Commands::Daemon) => service::run_daemon(shutdown).await?,
        Some(Commands::Service { action }) => match action {
            ServiceAction::Uninstall => service::uninstall_service()?,
            ServiceAction::Status => service::service_status()?,
        },
        Some(Commands::Add { name }) => plugins::add_plugin(&name)?,
        Some(Commands::Remove { name }) => plugins::remove_plugin(&name)?,
        Some(Commands::Plugins) => plugins::list_plugins()?,
        Some(Commands::Settings { action }) => match action {
            Some(SettingsAction::Set { key, value }) => run_settings_set(&key, &value)?,
            Some(SettingsAction::Get { key }) => run_settings_get(&key)?,
            Some(SettingsAction::Unset { key }) => run_settings_unset(&key)?,
            None => run_settings_show()?,
        },
        Some(Commands::Logs {
            count,
            verbose,
            activity,
            level,
            category,
        }) => {
            if activity {
                run_activity_logs(count, &level, category.as_deref())?;
            } else {
                run_logs(count, verbose)?;
            }
        }
        Some(Commands::Tasks { action }) => match action {
            Some(TasksAction::List) | None => run_tasks_list()?,
            Some(TasksAction::Create {
                name,
                prompt,
                schedule,
                r#type,
                max_retries,
                timeout,
                delivery_channel,
                delivery_target,
            }) => run_tasks_create(
                &name,
                &prompt,
                &schedule,
                &r#type,
                max_retries,
                timeout.map(|s| s as i64 * 1000),
                delivery_channel.as_deref(),
                delivery_target.as_deref(),
            )?,
            Some(TasksAction::Delete { id }) => run_tasks_delete(&id)?,
            Some(TasksAction::Pause { id }) => run_tasks_update_status(&id, "paused")?,
            Some(TasksAction::Resume { id }) => run_tasks_update_status(&id, "active")?,
            Some(TasksAction::Run { id }) => run_tasks_run(&id)?,
            Some(TasksAction::Runs { id, count }) => run_tasks_runs(&id, count)?,
            Some(TasksAction::Status { id }) => run_tasks_status(&id)?,
        },
        Some(Commands::Cron { action }) => match action {
            Some(CronAction::List) | None => run_cron_list()?,
            Some(CronAction::Add {
                line,
                schedule,
                command,
                name,
                timeout,
                delivery_channel,
                delivery_target,
            }) => run_cron_add(
                line.as_deref(),
                schedule.as_deref(),
                command.as_deref(),
                name.as_deref(),
                timeout.map(|s| s as i64 * 1000),
                delivery_channel.as_deref(),
                delivery_target.as_deref(),
            )?,
            Some(CronAction::Remove { id }) => run_cron_mutate(&id, "delete")?,
            Some(CronAction::Pause { id }) => run_cron_mutate(&id, "pause")?,
            Some(CronAction::Resume { id }) => run_cron_mutate(&id, "resume")?,
            Some(CronAction::Run { id }) => run_cron_mutate(&id, "run")?,
            Some(CronAction::Runs { id, count }) => run_tasks_runs(&id, count)?,
        },
        Some(Commands::Projects { action }) => match action {
            Some(ProjectsAction::List { status }) => run_projects_list(status.as_deref())?,
            None => run_projects_list(None)?,
            Some(ProjectsAction::Create { name, description }) => {
                run_projects_create(&name, description.as_deref())?
            }
            Some(ProjectsAction::Get { id }) => run_projects_get(&id)?,
            Some(ProjectsAction::Update {
                id,
                name,
                description,
                status,
            }) => run_projects_update(
                &id,
                name.as_deref(),
                description.as_deref(),
                status.as_deref(),
            )?,
            Some(ProjectsAction::Archive { id }) => run_projects_archive(&id)?,
            Some(ProjectsAction::Delete { id }) => run_projects_delete(&id)?,
        },
        Some(Commands::Usage) => run_usage()?,
        Some(Commands::Pairing { action }) => match action {
            Some(PairingAction::List { channel }) => run_pairing_list(channel.as_deref())?,
            Some(PairingAction::Approve { code }) => run_pairing_approve(&code)?,
            Some(PairingAction::Revoke { channel, sender_id }) => {
                run_pairing_revoke(&channel, &sender_id)?
            }
            Some(PairingAction::Approved { channel }) => run_pairing_approved(channel.as_deref())?,
            None => run_pairing_list(None)?,
        },
        Some(Commands::Poke) => run_poke().await?,
        Some(Commands::Cancel { session }) => run_cancel(session).await?,
        Some(Commands::Away { message }) => run_away(message).await?,
        Some(Commands::Available) => run_available().await?,
        Some(Commands::Migrate { action }) => match action {
            None => migrate_tui::run()?,
            Some(MigrateSubcommand::Hermes) => {
                run_migrate_direct(borg_core::migrate::MigrationSource::Hermes)?
            }
            Some(MigrateSubcommand::Claw) => {
                run_migrate_direct(borg_core::migrate::MigrationSource::OpenClaw)?
            }
        },
        Some(Commands::Update { dev, check }) => run_update(dev, check).await?,
        Some(Commands::Uninstall) => run_uninstall()?,
    }

    Ok(())
}

fn run_migrate_direct(source: borg_core::migrate::MigrationSource) -> Result<()> {
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

fn ensure_onboarded() -> Result<()> {
    let data_dir = borg_core::config::Config::data_dir()?;
    let config_path = data_dir.join("config.toml");
    if !config_path.exists() {
        init_data_dir()?;
    }
    Ok(())
}

async fn run_poke() -> Result<()> {
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

async fn run_cancel(session: Option<String>) -> Result<()> {
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

async fn run_away(message: Option<String>) -> Result<()> {
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

async fn run_available() -> Result<()> {
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

fn run_pairing_list(channel: Option<&str>) -> Result<()> {
    let db = borg_core::db::Database::open()?;
    let requests = db.list_pairings(channel)?;
    if requests.is_empty() {
        println!("No pending pairing requests.");
        return Ok(());
    }
    println!(
        "{:<12} {:<20} {:<10} {:<20}",
        "Channel", "Sender ID", "Code", "Expires"
    );
    println!("{}", "─".repeat(64));
    for r in &requests {
        let expires = format_ts(r.expires_at, "%Y-%m-%d %H:%M UTC");
        println!(
            "{:<12} {:<20} {:<10} {:<20}",
            r.channel_name, r.sender_id, r.code, expires
        );
    }
    println!();
    println!("Approve with: borg pairing approve <code>");
    Ok(())
}

fn run_pairing_approve(code: &str) -> Result<()> {
    let db = borg_core::db::Database::open()?;

    // Extract channel from prefix, or fall back to cross-channel lookup
    let (channel_name, request) =
        if let Some((channel, _)) = borg_core::pairing::parse_prefixed_code(code) {
            let req = db.approve_pairing(channel, code)?;
            (channel.to_string(), req)
        } else {
            match db.find_pending_by_code(code)? {
                Some(row) => {
                    let ch = row.channel_name;
                    let req = db.approve_pairing(&ch, code)?;
                    (ch, req)
                }
                None => anyhow::bail!("No pending pairing request found for code '{code}'"),
            }
        };

    println!(
        "Approved {} sender {}.",
        request.channel_name, request.sender_id
    );

    // Send approval notification to the user's channel
    let config = match borg_core::config::Config::load_from_db() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("Failed to load config for approval notification: {e}");
            borg_core::config::Config::default()
        }
    };
    let name = config
        .user
        .agent_name
        .clone()
        .unwrap_or_else(|| "Borg".to_string());
    let sid = request.sender_id;
    let ch = channel_name;
    tokio::runtime::Handle::current().block_on(async {
        borg_core::pairing::send_approval_notification(&config, &ch, &sid, &name).await;
    });

    Ok(())
}

fn run_pairing_revoke(channel: &str, sender_id: &str) -> Result<()> {
    let db = borg_core::db::Database::open()?;
    if db.revoke_sender(channel, sender_id)? {
        println!("Revoked {channel} sender {sender_id}.");
    } else {
        println!("No approved sender found for {channel} with ID {sender_id}.");
    }
    Ok(())
}

fn run_pairing_approved(channel: Option<&str>) -> Result<()> {
    let db = borg_core::db::Database::open()?;
    let senders = db.list_approved_senders(channel)?;
    if senders.is_empty() {
        println!("No approved senders.");
        return Ok(());
    }
    println!(
        "{:<12} {:<20} {:<16} {:<20}",
        "Channel", "Sender ID", "Display Name", "Approved At"
    );
    println!("{}", "─".repeat(70));
    for s in &senders {
        let approved = format_ts(s.approved_at, "%Y-%m-%d %H:%M UTC");
        let name = s.display_name.as_deref().unwrap_or("—");
        println!(
            "{:<12} {:<20} {:<16} {:<20}",
            s.channel_name, s.sender_id, name, approved
        );
    }
    Ok(())
}

fn run_usage() -> Result<()> {
    let db = borg_core::db::Database::open()?;
    let total_tokens = db.monthly_token_total()?;
    let total_cost = db.monthly_total_cost()?;
    let by_model = db.monthly_usage_by_model()?;

    println!("Token usage for current month");
    println!("────────────────────────────────────────");
    println!("Total tokens: {total_tokens}");
    if let Some(cost) = total_cost {
        println!("Estimated cost: ${cost:.4}");
    }

    // Prompt cache hit ratio (current month).
    if let Ok((prompt_sum, cached_sum, created_sum)) = {
        use chrono::Datelike;
        let now = chrono::Utc::now();
        let month_start = now
            .date_naive()
            .with_day(1)
            .unwrap_or_else(|| now.date_naive())
            .and_hms_opt(0, 0, 0)
            .map(|dt| dt.and_utc().timestamp())
            .unwrap_or(0);
        db.cache_token_summary_since(month_start)
    } {
        if prompt_sum > 0 && (cached_sum > 0 || created_sum > 0) {
            let pct = cached_sum as f64 / prompt_sum as f64 * 100.0;
            println!(
                "Prompt cache: {cached_sum}/{prompt_sum} hit ({pct:.1}%), {created_sum} created"
            );
        }
    }

    if !by_model.is_empty() {
        println!();
        println!(
            "{:<40} {:>10} {:>10} {:>10} {:>10}",
            "Model", "Input", "Output", "Total", "Cost"
        );
        println!("{}", "─".repeat(84));
        for row in &by_model {
            let label = if row.model.is_empty() {
                "(unknown)".to_string()
            } else {
                row.model.clone()
            };
            let cost_str = match row.total_cost_usd {
                Some(c) => format!("${c:.4}"),
                None => "—".to_string(),
            };
            println!(
                "{:<40} {:>10} {:>10} {:>10} {:>10}",
                label, row.prompt_tokens, row.completion_tokens, row.total_tokens, cost_str
            );
        }
    }

    let config = borg_core::config::Config::load_from_db().unwrap_or_default();
    let budget_limit = config.budget.monthly_token_limit;
    if budget_limit > 0 {
        let pct = total_tokens as f64 / budget_limit as f64 * 100.0;
        println!();
        println!("Budget: {total_tokens}/{budget_limit} tokens ({pct:.1}%) used");
    }

    Ok(())
}

fn run_status() -> Result<()> {
    let now = chrono::Utc::now();
    let config = borg_core::config::Config::load_from_db()?;
    let db = borg_core::db::Database::open()?;

    // Evolution header
    if config.evolution.enabled {
        if let Ok(evo_state) = db.get_evolution_state() {
            println!("Borg Status");
            println!("\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}");
            println!();
            print!(
                "{}",
                borg_core::evolution::format_status_section(&evo_state)
            );
            println!();
        }
    }

    // Vitals
    let state = db.get_vitals_state()?;
    let state = borg_core::vitals::apply_decay(&state, now);
    let mut drift = borg_core::vitals::detect_drift(&state, now);
    let since = (now - chrono::Duration::days(7)).timestamp();
    let events = db.vitals_events_since(since)?;
    if borg_core::vitals::detect_failure_drift(&events) {
        drift.push(borg_core::vitals::DriftFlag::RepeatedFailures);
    }
    println!(
        "{}",
        borg_core::vitals::format_status(&state, &events, &drift)
    );

    // Bond
    if let Ok(bond_events) = db.get_all_bond_events() {
        let bond_key = db.derive_hmac_key(borg_core::bond::BOND_HMAC_DOMAIN);
        let bond_state = borg_core::bond::replay_events_with_key(&bond_key, &bond_events);
        println!("Bond");
        println!(
            "  score        {}  {} ({})",
            format_bar(bond_state.score as usize, 10),
            bond_state.score,
            bond_state.level
        );
        println!();
    }

    Ok(())
}

fn format_bar(value: usize, width: usize) -> String {
    let filled = (value * width) / 100;
    format!(
        "{}{}",
        "\u{2588}".repeat(filled),
        "\u{2591}".repeat(width.saturating_sub(filled))
    )
}

fn run_status_history() -> Result<()> {
    let config = borg_core::config::Config::load_from_db()?;
    if !config.evolution.enabled {
        println!(
            "Evolution system is disabled. Enable with: borg settings set evolution.enabled true"
        );
        return Ok(());
    }
    let db = borg_core::db::Database::open()?;
    let events = db.evolution_events_since(0)?;
    // Reverse to chronological for display
    let mut events = events;
    events.reverse();
    println!("{}", borg_core::evolution::format_history(&events));
    Ok(())
}

fn run_status_archetypes() -> Result<()> {
    let config = borg_core::config::Config::load_from_db()?;
    if !config.evolution.enabled {
        println!(
            "Evolution system is disabled. Enable with: borg settings set evolution.enabled true"
        );
        return Ok(());
    }
    let db = borg_core::db::Database::open()?;
    let state = db.get_evolution_state()?;
    println!("{}", borg_core::evolution::format_archetype_scores(&state));
    Ok(())
}

fn run_bond_status() -> Result<()> {
    let db = borg_core::db::Database::open()?;
    let events = db.get_all_bond_events()?;
    let bond_key = db.derive_hmac_key(borg_core::bond::BOND_HMAC_DOMAIN);
    let state = borg_core::bond::replay_events_with_key(&bond_key, &events);
    let correction_rate = borg_core::bond::compute_correction_rate(&db);
    let routine_rate = borg_core::bond::compute_routine_success_rate(&db);
    let pref_count = borg_core::bond::compute_preference_learning_count(&db);
    let since = (chrono::Utc::now() - chrono::Duration::days(7)).timestamp();
    let recent = db.bond_events_since(since)?;
    println!(
        "{}",
        borg_core::bond::format_status(&state, correction_rate, routine_rate, pref_count, &recent)
    );
    Ok(())
}

fn run_bond_history(count: usize) -> Result<()> {
    let db = borg_core::db::Database::open()?;
    let events = db.bond_events_recent(count)?;
    println!("{}", borg_core::bond::format_history(&events));
    Ok(())
}

fn run_doctor() -> Result<()> {
    let config = borg_core::config::Config::load_from_db().unwrap_or_default();
    let report = borg_core::doctor::run_diagnostics(&config);
    println!("{}", report.format());
    let (_pass, _warn, fail) = report.counts();
    if fail > 0 {
        std::process::exit(1);
    }
    Ok(())
}

async fn run_update(dev: bool, check: bool) -> Result<()> {
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

fn init_data_dir() -> Result<()> {
    let data_dir = borg_core::config::Config::data_dir()?;
    std::fs::create_dir_all(&data_dir)?;

    // Run interactive onboarding wizard
    match onboarding::run_onboarding()? {
        Some(result) => {
            onboarding::apply_onboarding(&result)?;

            // Auto-start daemon service after successful onboarding (non-fatal)
            if let Err(e) = service::ensure_service_running() {
                tracing::warn!("Auto-start service during onboarding: {e}");
            }

            println!();
            println!("Initialized {}", data_dir.display());

            // Auto-launch TUI for the first conversation
            let setup_path = data_dir.join("SETUP.md");
            if setup_path.exists() && std::io::stdin().is_terminal() {
                println!();
                println!("Launching your agent for the first time...");
                // The TUI will detect SETUP.md and inject it into the system prompt
                return Ok(());
            }

            println!();
            println!("You're all set! Run `borg` to start chatting.");
        }
        None => {
            // User cancelled — fall back to defaults so the directory is still usable
            init_data_dir_defaults(&data_dir)?;
            println!("Initialized {} with defaults", data_dir.display());
        }
    }

    Ok(())
}

/// Set `~/.borg/` to mode 0700 so only the owner can access it.
/// This protects IDENTITY.md, memory, config, conversation logs, and API key env files.
#[cfg(unix)]
fn harden_data_dir(data_dir: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    if data_dir.exists() {
        let perms = std::fs::Permissions::from_mode(0o700);
        let _ = std::fs::set_permissions(data_dir, perms);
    }
}

#[cfg(not(unix))]
fn harden_data_dir(_data_dir: &std::path::Path) {
    // No-op on non-Unix platforms
}

fn run_settings_show() -> Result<()> {
    let resolver = borg_core::settings::SettingsResolver::load()?;
    let all = resolver.list_all()?;
    println!("Settings:");
    for info in &all {
        println!("  {:40} = {:20} [{}]", info.key, info.value, info.source);
    }
    Ok(())
}

fn run_settings_set(key: &str, value: &str) -> Result<()> {
    let resolver = borg_core::settings::SettingsResolver::load()?;
    let confirmation = resolver.set(key, value)?;
    println!("Updated: {confirmation}");
    Ok(())
}

fn run_settings_get(key: &str) -> Result<()> {
    let resolver = borg_core::settings::SettingsResolver::load()?;
    let (value, source) = resolver.get_with_source(key)?;
    println!("{key} = {value} [{source}]");
    Ok(())
}

fn run_settings_unset(key: &str) -> Result<()> {
    let resolver = borg_core::settings::SettingsResolver::load()?;
    resolver.unset(key)?;
    let (value, source) = resolver.get_with_source(key)?;
    println!("Unset {key} — effective value: {value} [{source}]");
    Ok(())
}

fn run_logs(count: usize, verbose: bool) -> Result<()> {
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

fn run_activity_logs(count: usize, level: &str, category: Option<&str>) -> Result<()> {
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

fn run_tasks_list() -> Result<()> {
    let db = borg_core::db::Database::open()?;
    let tasks = db.list_tasks()?;

    if tasks.is_empty() {
        println!("No scheduled tasks.");
    } else {
        println!(
            "{:8}  {:20}  {:8}  {:8}  {:20}  NEXT RUN",
            "ID", "NAME", "TYPE", "STATUS", "SCHEDULE"
        );
        for task in &tasks {
            let next_run = task
                .next_run
                .map(|ts| format_ts(ts, "%Y-%m-%d %H:%M:%S"))
                .unwrap_or_else(|| "-".to_string());
            println!(
                "{:8}  {:20}  {:8}  {:8}  {:20}  {}",
                short_id(&task.id),
                truncate_str(&task.name, 20),
                task.schedule_type,
                task.status,
                truncate_str(&task.schedule_expr, 20),
                next_run,
            );
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn run_tasks_create(
    name: &str,
    prompt: &str,
    schedule: &str,
    schedule_type: &str,
    max_retries: Option<i32>,
    timeout_ms: Option<i64>,
    delivery_channel: Option<&str>,
    delivery_target: Option<&str>,
) -> Result<()> {
    borg_core::tasks::validate_schedule(schedule_type, schedule)?;
    let next_run = borg_core::tasks::calculate_next_run(schedule_type, schedule)?;
    let id = Uuid::new_v4().to_string();
    let tz = chrono::Local::now().offset().to_string();

    let db = borg_core::db::Database::open()?;
    db.create_task(&borg_core::db::NewTask {
        id: &id,
        name,
        prompt,
        schedule_type,
        schedule_expr: schedule,
        timezone: &tz,
        next_run,
        max_retries,
        timeout_ms,
        delivery_channel,
        delivery_target,
        allowed_tools: None, // TODO: Add --tools CLI flag
        task_type: "prompt",
    })?;

    println!("Created task {} ({})", short_id(&id), name);
    Ok(())
}

fn run_tasks_run(id: &str) -> Result<()> {
    let db = borg_core::db::Database::open()?;
    match db.get_task_by_id(id)? {
        Some(_task) => {
            let now = chrono::Utc::now().timestamp();
            db.update_task_next_run(id, Some(now))?;
            db.clear_task_retry(id)?;
            println!("Task {} queued for immediate execution.", short_id(id));
        }
        None => println!("Task not found: {id}"),
    }
    Ok(())
}

fn run_tasks_runs(id: &str, count: usize) -> Result<()> {
    let db = borg_core::db::Database::open()?;
    let runs = db.task_run_history(id, count)?;
    if runs.is_empty() {
        println!("No runs recorded for task {}", short_id(id));
        return Ok(());
    }
    println!("{:<20} {:<8} {:<10} Details", "Time", "Status", "Duration");
    println!("{}", "-".repeat(70));
    for run in &runs {
        let when = format_ts(run.started_at, "%Y-%m-%d %H:%M");
        let status = borg_core::tasks::format_run_status(&run.status);
        let duration = format!("{}ms", run.duration_ms);
        let details = run
            .error
            .as_deref()
            .or(run.result.as_deref())
            .unwrap_or("")
            .chars()
            .take(40)
            .collect::<String>();
        println!("{when:<20} {status:<8} {duration:<10} {details}");
    }
    Ok(())
}

fn run_tasks_status(id: &str) -> Result<()> {
    let db = borg_core::db::Database::open()?;
    match db.get_task_by_id(id)? {
        Some(task) => {
            println!("{}", borg_core::tasks::format_task(&task));
            println!("    Max retries: {}", task.max_retries);
            println!("    Timeout: {}ms", task.timeout_ms);
            if let Some(ref ch) = task.delivery_channel {
                println!(
                    "    Delivery: {} -> {}",
                    ch,
                    task.delivery_target.as_deref().unwrap_or("?")
                );
            }
            if task.retry_count > 0 {
                println!(
                    "    Retry state: attempt {}/{}",
                    task.retry_count, task.max_retries
                );
                if let Some(ref err) = task.last_error {
                    println!("    Last error: {}", &err[..err.len().min(100)]);
                }
                if let Some(retry_at) = task.retry_after {
                    let when = format_ts(retry_at, "%Y-%m-%d %H:%M UTC");
                    println!("    Next retry: {when}");
                }
            }
            if let Ok(Some(run)) = db.last_task_run(id) {
                let when = format_ts(run.started_at, "%Y-%m-%d %H:%M UTC");
                println!(
                    "    Last run: {} at {when} ({}ms)",
                    run.status, run.duration_ms
                );
            }
        }
        None => println!("Task not found: {id}"),
    }
    Ok(())
}

fn run_tasks_delete(id: &str) -> Result<()> {
    let db = borg_core::db::Database::open()?;
    if db.delete_task(id)? {
        println!("Deleted task {}", short_id(id));
    } else {
        println!("Task not found: {id}");
    }
    Ok(())
}

fn run_tasks_update_status(id: &str, status: &str) -> Result<()> {
    let db = borg_core::db::Database::open()?;
    if db.update_task_status(id, status)? {
        println!("Task {} status: {status}", short_id(id));
    } else {
        println!("Task not found: {id}");
    }
    Ok(())
}

// ── Project handlers ──

fn run_projects_list(status_filter: Option<&str>) -> Result<()> {
    let db = borg_core::db::Database::open()?;
    let projects = db.list_projects(status_filter)?;

    if projects.is_empty() {
        println!("No projects.");
    } else {
        println!(
            "{:8}  {:30}  {:10}  {:19}  DESCRIPTION",
            "ID", "NAME", "STATUS", "CREATED"
        );
        for p in &projects {
            let created = format_ts(p.created_at, "%Y-%m-%d %H:%M:%S");
            let desc = truncate_str(&p.description, 40);
            println!(
                "{:8}  {:30}  {:10}  {:19}  {}",
                short_id(&p.id),
                truncate_str(&p.name, 30),
                p.status,
                created,
                desc,
            );
        }
    }
    Ok(())
}

fn run_projects_create(name: &str, description: Option<&str>) -> Result<()> {
    let id = Uuid::new_v4().to_string();
    let db = borg_core::db::Database::open()?;
    db.create_project(&id, name, description.unwrap_or(""))?;
    println!("Created project {} ({})", short_id(&id), name);
    Ok(())
}

fn run_projects_get(id: &str) -> Result<()> {
    let db = borg_core::db::Database::open()?;
    match db.get_project(id)? {
        Some(p) => {
            println!("Project: {}", p.name);
            println!("  ID:          {}", p.id);
            println!("  Status:      {}", p.status);
            println!(
                "  Description: {}",
                if p.description.is_empty() {
                    "(none)"
                } else {
                    &p.description
                }
            );
            println!(
                "  Created:     {}",
                format_ts(p.created_at, "%Y-%m-%d %H:%M:%S")
            );
            println!(
                "  Updated:     {}",
                format_ts(p.updated_at, "%Y-%m-%d %H:%M:%S")
            );

            match db.list_workflows_by_project(&p.id) {
                Ok(wfs) if wfs.is_empty() => println!("  Workflows:   none"),
                Ok(wfs) => {
                    println!("  Workflows ({}):", wfs.len());
                    for wf in &wfs {
                        println!("    [{}] {} ({})", wf.status, wf.title, short_id(&wf.id),);
                    }
                }
                Err(e) => println!("  Workflows:   error ({e})"),
            }
        }
        None => println!("Project not found: {id}"),
    }
    Ok(())
}

fn run_projects_update(
    id: &str,
    name: Option<&str>,
    description: Option<&str>,
    status: Option<&str>,
) -> Result<()> {
    if name.is_none() && description.is_none() && status.is_none() {
        anyhow::bail!("Nothing to update. Provide --name, --description, or --status.");
    }
    let db = borg_core::db::Database::open()?;
    if db.update_project(id, name, description, status)? {
        println!("Updated project {}", short_id(id));
    } else {
        println!("Project not found: {id}");
    }
    Ok(())
}

fn run_projects_archive(id: &str) -> Result<()> {
    let db = borg_core::db::Database::open()?;
    if db.archive_project(id)? {
        println!("Archived project {}", short_id(id));
    } else {
        println!("Project not found or already archived: {id}");
    }
    Ok(())
}

fn run_projects_delete(id: &str) -> Result<()> {
    let db = borg_core::db::Database::open()?;
    if db.delete_project(id)? {
        println!("Deleted project {}", short_id(id));
    } else {
        println!("Project not found: {id}");
    }
    Ok(())
}

// ── Cron job handlers ──

fn run_cron_list() -> Result<()> {
    let db = borg_core::db::Database::open()?;
    let tasks = db.list_tasks()?;
    let cron_jobs: Vec<_> = tasks.iter().filter(|t| t.task_type == "command").collect();
    if cron_jobs.is_empty() {
        println!("No cron jobs. Use `borg cron add` to create one.");
        return Ok(());
    }
    println!(
        "{:<10} {:<8} {:<20} {:<30} NEXT RUN",
        "ID", "STATUS", "SCHEDULE", "COMMAND"
    );
    println!("{}", "-".repeat(90));
    for job in &cron_jobs {
        let next = job
            .next_run
            .map(|ts| format_ts(ts, "%Y-%m-%d %H:%M"))
            .unwrap_or_else(|| "—".to_string());
        // Convert 7-field back to 5-field for display
        let sched_display = display_cron_5field(&job.schedule_expr);
        println!(
            "{:<10} {:<8} {:<20} {:<30} {}",
            short_id(&job.id),
            job.status,
            sched_display,
            truncate_str(&job.prompt, 28),
            next,
        );
    }
    Ok(())
}

/// Convert a 7-field cron expression back to 5-field Linux format for display.
fn display_cron_5field(expr: &str) -> String {
    let fields: Vec<&str> = expr.split_whitespace().collect();
    if fields.len() == 7 {
        // Drop first (seconds) and last (year) fields
        fields[1..6].join(" ")
    } else {
        expr.to_string()
    }
}

fn run_cron_add(
    line: Option<&str>,
    schedule: Option<&str>,
    command: Option<&str>,
    name: Option<&str>,
    timeout_ms: Option<i64>,
    delivery_channel: Option<&str>,
    delivery_target: Option<&str>,
) -> Result<()> {
    let (cron_7, cmd) = if let Some(line) = line {
        borg_core::tasks::parse_cron_line(line)?
    } else {
        match (schedule, command) {
            (Some(sched), Some(cmd)) => {
                let cron_7 = borg_core::tasks::convert_5_to_7_field(sched);
                borg_core::tasks::validate_schedule("cron", &cron_7)?;
                (cron_7, cmd.to_string())
            }
            _ => anyhow::bail!(
                "Provide either a combined crontab line or both --schedule and --command.\n\
                 Examples:\n  borg cron add \"*/5 * * * * echo hello\"\n  \
                 borg cron add -s \"*/5 * * * *\" -c \"echo hello\""
            ),
        }
    };

    let job_name = name
        .map(std::string::ToString::to_string)
        .unwrap_or_else(|| {
            let short_cmd: String = cmd.chars().take(30).collect();
            format!("cron: {short_cmd}")
        });

    let id = uuid::Uuid::new_v4().to_string();
    let next_run = borg_core::tasks::calculate_next_run("cron", &cron_7)?;

    let db = borg_core::db::Database::open()?;
    db.create_task(&borg_core::db::NewTask {
        id: &id,
        name: &job_name,
        prompt: &cmd,
        schedule_type: "cron",
        schedule_expr: &cron_7,
        timezone: &chrono::Local::now().offset().to_string(),
        next_run,
        max_retries: Some(0), // cron jobs don't retry by default
        timeout_ms,
        delivery_channel,
        delivery_target,
        allowed_tools: None,
        task_type: "command",
    })?;

    let next_str = next_run
        .map(|ts| format_ts(ts, "%Y-%m-%d %H:%M"))
        .unwrap_or_else(|| "?".to_string());
    println!(
        "Created cron job {} ({})\n  Schedule: {}\n  Command: {}\n  Next run: {}",
        short_id(&id),
        job_name,
        display_cron_5field(&cron_7),
        cmd,
        next_str,
    );
    Ok(())
}

/// Type-guarded cron job mutation. Verifies the task is a command-type before mutating.
fn run_cron_mutate(id: &str, action: &str) -> Result<()> {
    let db = borg_core::db::Database::open()?;
    match db.get_task_by_id(id)? {
        Some(task) if task.task_type == "command" => match action {
            "delete" => {
                db.delete_task(id)?;
                println!("Deleted cron job {}", short_id(id));
            }
            "pause" => {
                db.update_task_status(id, "paused")?;
                println!("Cron job {} paused", short_id(id));
            }
            "resume" => {
                db.update_task_status(id, "active")?;
                println!("Cron job {} resumed", short_id(id));
            }
            "run" => {
                let now = chrono::Utc::now().timestamp();
                db.update_task_next_run(id, Some(now))?;
                db.clear_task_retry(id)?;
                println!("Cron job {} queued for immediate execution", short_id(id));
            }
            _ => unreachable!(),
        },
        Some(_) => println!("Not a cron job: {id} (use `borg tasks` for prompt tasks)"),
        None => println!("Cron job not found: {id}"),
    }
    Ok(())
}

fn truncate_str(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max - 1;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

fn run_uninstall() -> Result<()> {
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
        if let Err(e) = service::uninstall_service() {
            tracing::debug!("Service uninstall skipped: {e}");
        }

        delete_data_dir(&data_dir)?;

        println!("Borg data deleted. Goodbye!");
    } else {
        println!("Aborted.");
    }

    Ok(())
}

fn confirm_uninstall(input: &str) -> bool {
    input.eq_ignore_ascii_case("y")
}

fn delete_data_dir(data_dir: &std::path::Path) -> Result<()> {
    if data_dir.exists() {
        std::fs::remove_dir_all(data_dir)?;
    }
    Ok(())
}

/// Non-interactive fallback: write default config files without the wizard.
fn init_data_dir_defaults(data_dir: &std::path::Path) -> Result<()> {
    for sub in crate::onboarding::BORG_SUBDIRS {
        std::fs::create_dir_all(data_dir.join(sub))?;
    }

    let config_path = data_dir.join("config.toml");
    if !config_path.exists() {
        let config_content =
            onboarding::generate_config("anthropic/claude-sonnet-4", "openrouter", "", "", false)?;
        std::fs::write(&config_path, config_content)?;
        println!("  Created {}", config_path.display());
    }

    // Migrate SOUL.md → IDENTITY.md for existing users
    let old_identity = data_dir.join("SOUL.md");
    let new_identity = data_dir.join("IDENTITY.md");
    if old_identity.exists() && !new_identity.exists() {
        std::fs::rename(&old_identity, &new_identity)?;
    }

    let identity_path = data_dir.join("IDENTITY.md");
    if !identity_path.exists() {
        let identity = borg_core::identity::load_identity()?;
        borg_core::identity::save_identity(&identity)?;
        println!("  Created {}", identity_path.display());
    }

    let memory_path = data_dir.join("MEMORY.md");
    if !memory_path.exists() {
        std::fs::write(&memory_path, "# Memory Index\n\nNo memories yet.\n")?;
        println!("  Created {}", memory_path.display());
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn confirm_uninstall_accepts_y() {
        assert!(confirm_uninstall("y"));
    }

    #[test]
    fn confirm_uninstall_accepts_uppercase_y() {
        assert!(confirm_uninstall("Y"));
    }

    #[test]
    fn confirm_uninstall_rejects_empty() {
        assert!(!confirm_uninstall(""));
    }

    #[test]
    fn confirm_uninstall_rejects_no() {
        assert!(!confirm_uninstall("n"));
        assert!(!confirm_uninstall("N"));
    }

    #[test]
    fn confirm_uninstall_rejects_yes_spelled_out() {
        assert!(!confirm_uninstall("yes"));
    }

    #[test]
    fn confirm_uninstall_rejects_arbitrary_input() {
        assert!(!confirm_uninstall("maybe"));
        assert!(!confirm_uninstall("yy"));
    }

    #[test]
    fn delete_data_dir_removes_directory() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("borg_test");
        fs::create_dir_all(dir.join("memory")).unwrap();
        fs::write(dir.join("config.toml"), "test").unwrap();

        assert!(dir.exists());
        delete_data_dir(&dir).unwrap();
        assert!(!dir.exists());
    }

    #[test]
    fn delete_data_dir_noop_when_missing() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("nonexistent");
        assert!(!dir.exists());
        delete_data_dir(&dir).unwrap();
    }

    // -- Clap parsing tests --

    #[test]
    fn test_parse_settings_show() {
        let cli = Cli::try_parse_from(["borg", "settings"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Commands::Settings { action: None })
        ));
    }

    #[test]
    fn test_parse_settings_set() {
        let cli = Cli::try_parse_from(["borg", "settings", "set", "temperature", "0.5"]).unwrap();
        match cli.command {
            Some(Commands::Settings {
                action: Some(SettingsAction::Set { key, value }),
            }) => {
                assert_eq!(key, "temperature");
                assert_eq!(value, "0.5");
            }
            _ => panic!("Expected Settings Set"),
        }
    }

    #[test]
    fn test_parse_settings_get() {
        let cli = Cli::try_parse_from(["borg", "settings", "get", "temperature"]).unwrap();
        match cli.command {
            Some(Commands::Settings {
                action: Some(SettingsAction::Get { key }),
            }) => {
                assert_eq!(key, "temperature");
            }
            _ => panic!("Expected Settings Get"),
        }
    }

    #[test]
    fn test_parse_settings_unset() {
        let cli = Cli::try_parse_from(["borg", "settings", "unset", "temperature"]).unwrap();
        match cli.command {
            Some(Commands::Settings {
                action: Some(SettingsAction::Unset { key }),
            }) => {
                assert_eq!(key, "temperature");
            }
            _ => panic!("Expected Settings Unset"),
        }
    }

    #[test]
    fn test_parse_logs_default() {
        let cli = Cli::try_parse_from(["borg", "logs"]).unwrap();
        match cli.command {
            Some(Commands::Logs {
                count,
                verbose,
                activity,
                ..
            }) => {
                assert_eq!(count, 50);
                assert!(!verbose);
                assert!(!activity);
            }
            _ => panic!("Expected Logs"),
        }
    }

    #[test]
    fn test_parse_logs_custom_count() {
        let cli = Cli::try_parse_from(["borg", "logs", "--count", "10"]).unwrap();
        match cli.command {
            Some(Commands::Logs { count, .. }) => assert_eq!(count, 10),
            _ => panic!("Expected Logs"),
        }
    }

    #[test]
    fn test_parse_logs_verbose() {
        let cli = Cli::try_parse_from(["borg", "logs", "--verbose"]).unwrap();
        match cli.command {
            Some(Commands::Logs { count, verbose, .. }) => {
                assert_eq!(count, 50);
                assert!(verbose);
            }
            _ => panic!("Expected Logs"),
        }
    }

    #[test]
    fn test_parse_logs_verbose_short() {
        let cli = Cli::try_parse_from(["borg", "logs", "-v"]).unwrap();
        match cli.command {
            Some(Commands::Logs { verbose, .. }) => assert!(verbose),
            _ => panic!("Expected Logs"),
        }
    }

    #[test]
    fn test_parse_logs_verbose_with_count() {
        let cli = Cli::try_parse_from(["borg", "logs", "-v", "-c", "20"]).unwrap();
        match cli.command {
            Some(Commands::Logs { count, verbose, .. }) => {
                assert!(verbose);
                assert_eq!(count, 20);
            }
            _ => panic!("Expected Logs"),
        }
    }

    #[test]
    fn test_parse_logs_activity() {
        let cli = Cli::try_parse_from(["borg", "logs", "--activity", "--level", "warn"]).unwrap();
        match cli.command {
            Some(Commands::Logs {
                activity,
                level,
                category,
                ..
            }) => {
                assert!(activity);
                assert_eq!(level, "warn");
                assert!(category.is_none());
            }
            _ => panic!("Expected Logs"),
        }
    }

    #[test]
    fn test_parse_tasks_list() {
        let cli = Cli::try_parse_from(["borg", "tasks"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Commands::Tasks { action: None })
        ));

        let cli = Cli::try_parse_from(["borg", "tasks", "list"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Commands::Tasks {
                action: Some(TasksAction::List)
            })
        ));
    }

    #[test]
    fn test_parse_tasks_create() {
        let cli = Cli::try_parse_from([
            "borg",
            "tasks",
            "create",
            "--name",
            "test",
            "--prompt",
            "hello",
            "--schedule",
            "*/5 * * * *",
        ])
        .unwrap();
        match cli.command {
            Some(Commands::Tasks {
                action:
                    Some(TasksAction::Create {
                        name,
                        prompt,
                        schedule,
                        r#type,
                        ..
                    }),
            }) => {
                assert_eq!(name, "test");
                assert_eq!(prompt, "hello");
                assert_eq!(schedule, "*/5 * * * *");
                assert_eq!(r#type, "cron");
            }
            _ => panic!("Expected Tasks Create"),
        }
    }

    #[test]
    fn test_parse_tasks_create_short_flags() {
        let cli = Cli::try_parse_from([
            "borg",
            "tasks",
            "create",
            "-n",
            "test",
            "-p",
            "hello",
            "-s",
            "*/5 * * * *",
            "-t",
            "interval",
        ])
        .unwrap();
        match cli.command {
            Some(Commands::Tasks {
                action:
                    Some(TasksAction::Create {
                        name,
                        prompt,
                        schedule,
                        r#type,
                        ..
                    }),
            }) => {
                assert_eq!(name, "test");
                assert_eq!(prompt, "hello");
                assert_eq!(schedule, "*/5 * * * *");
                assert_eq!(r#type, "interval");
            }
            _ => panic!("Expected Tasks Create"),
        }
    }

    #[test]
    fn test_parse_ask_short_flags() {
        let cli = Cli::try_parse_from(["borg", "ask", "-y", "-j", "hello"]).unwrap();
        match cli.command {
            Some(Commands::Ask {
                message,
                yes,
                json,
                mode,
            }) => {
                assert_eq!(message, "hello");
                assert!(yes);
                assert!(json);
                assert!(mode.is_none());
            }
            _ => panic!("Expected Ask"),
        }
    }

    #[test]
    fn test_parse_tasks_delete() {
        let cli = Cli::try_parse_from(["borg", "tasks", "delete", "abc123"]).unwrap();
        match cli.command {
            Some(Commands::Tasks {
                action: Some(TasksAction::Delete { id }),
            }) => assert_eq!(id, "abc123"),
            _ => panic!("Expected Tasks Delete"),
        }
    }

    #[test]
    fn test_parse_tasks_pause_resume() {
        let cli = Cli::try_parse_from(["borg", "tasks", "pause", "abc123"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Commands::Tasks {
                action: Some(TasksAction::Pause { .. })
            })
        ));

        let cli = Cli::try_parse_from(["borg", "tasks", "resume", "abc123"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Commands::Tasks {
                action: Some(TasksAction::Resume { .. })
            })
        ));
    }

    // -- Settings tests --

    #[test]
    fn test_settings_show_with_default_config() {
        let config = borg_core::config::Config::default();
        let output = config.display_settings();
        assert!(!output.is_empty());
        assert!(output.contains("temperature"));
    }

    #[test]
    fn test_settings_apply_valid_key() {
        let mut config = borg_core::config::Config::default();
        let result = config.apply_setting("temperature", "0.5");
        assert!(result.is_ok());
        assert!((config.llm.temperature - 0.5).abs() < f32::EPSILON);
    }

    #[test]
    fn test_settings_apply_invalid_key() {
        let mut config = borg_core::config::Config::default();
        let result = config.apply_setting("nonexistent", "x");
        assert!(result.is_err());
    }

    #[test]
    fn test_settings_apply_invalid_value() {
        let mut config = borg_core::config::Config::default();
        let result = config.apply_setting("temperature", "not_a_number");
        assert!(result.is_err());
    }

    // -- Tasks tests (using temp DB) --

    #[test]
    fn test_tasks_validate_schedule_cron() {
        assert!(borg_core::tasks::validate_schedule("cron", "0 */5 * * * *").is_ok());
    }

    #[test]
    fn test_tasks_validate_schedule_interval() {
        assert!(borg_core::tasks::validate_schedule("interval", "30m").is_ok());
    }

    #[test]
    fn test_tasks_validate_schedule_invalid() {
        assert!(borg_core::tasks::validate_schedule("cron", "not valid").is_err());
    }

    // -- Helper tests --

    #[test]
    fn test_truncate_str_short() {
        assert_eq!(truncate_str("hello", 10), "hello");
    }

    #[test]
    fn test_truncate_str_exact() {
        assert_eq!(truncate_str("hello", 5), "hello");
    }

    #[test]
    fn test_truncate_str_long() {
        let result = truncate_str("hello world", 5);
        assert_eq!(result, "hell…");
    }

    // -- --resume flag parsing --

    #[test]
    fn test_parse_resume_flag_no_subcommand() {
        let cli = Cli::try_parse_from(["borg", "--resume", "abcd1234"]).unwrap();
        assert_eq!(cli.resume.as_deref(), Some("abcd1234"));
        assert!(cli.command.is_none());
    }

    #[test]
    fn test_parse_resume_flag_with_start() {
        let cli = Cli::try_parse_from(["borg", "start", "--resume", "abcd1234"]).unwrap();
        assert_eq!(cli.resume.as_deref(), Some("abcd1234"));
        assert!(matches!(cli.command, Some(Commands::Start)));
    }

    #[test]
    fn test_parse_without_resume_flag() {
        let cli = Cli::try_parse_from(["borg"]).unwrap();
        assert!(cli.resume.is_none());
        assert!(cli.command.is_none());
    }

    #[test]
    fn test_parse_resume_requires_value() {
        // --resume with no value should fail to parse
        assert!(Cli::try_parse_from(["borg", "--resume"]).is_err());
    }

    // ── Projects CLI parse tests ──

    #[test]
    fn test_parse_projects_list() {
        let cli = Cli::try_parse_from(["borg", "projects"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Commands::Projects { action: None })
        ));

        let cli = Cli::try_parse_from(["borg", "projects", "list"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Commands::Projects {
                action: Some(ProjectsAction::List { .. })
            })
        ));
    }

    #[test]
    fn test_parse_projects_create() {
        let cli =
            Cli::try_parse_from(["borg", "projects", "create", "--name", "My Project"]).unwrap();
        match cli.command {
            Some(Commands::Projects {
                action: Some(ProjectsAction::Create { name, description }),
            }) => {
                assert_eq!(name, "My Project");
                assert!(description.is_none());
            }
            _ => panic!("Expected Projects Create"),
        }
    }

    #[test]
    fn test_parse_projects_create_with_description() {
        let cli = Cli::try_parse_from([
            "borg",
            "projects",
            "create",
            "--name",
            "My Project",
            "--description",
            "A test project",
        ])
        .unwrap();
        match cli.command {
            Some(Commands::Projects {
                action: Some(ProjectsAction::Create { name, description }),
            }) => {
                assert_eq!(name, "My Project");
                assert_eq!(description.as_deref(), Some("A test project"));
            }
            _ => panic!("Expected Projects Create"),
        }
    }

    #[test]
    fn test_parse_projects_get() {
        let cli = Cli::try_parse_from(["borg", "projects", "get", "abc123"]).unwrap();
        match cli.command {
            Some(Commands::Projects {
                action: Some(ProjectsAction::Get { id }),
            }) => assert_eq!(id, "abc123"),
            _ => panic!("Expected Projects Get"),
        }
    }

    #[test]
    fn test_parse_projects_update() {
        let cli = Cli::try_parse_from([
            "borg", "projects", "update", "abc123", "--name", "New Name", "--status", "archived",
        ])
        .unwrap();
        match cli.command {
            Some(Commands::Projects {
                action:
                    Some(ProjectsAction::Update {
                        id,
                        name,
                        description,
                        status,
                    }),
            }) => {
                assert_eq!(id, "abc123");
                assert_eq!(name.as_deref(), Some("New Name"));
                assert!(description.is_none());
                assert_eq!(status.as_deref(), Some("archived"));
            }
            _ => panic!("Expected Projects Update"),
        }
    }

    #[test]
    fn test_parse_projects_delete() {
        let cli = Cli::try_parse_from(["borg", "projects", "delete", "abc123"]).unwrap();
        match cli.command {
            Some(Commands::Projects {
                action: Some(ProjectsAction::Delete { id }),
            }) => assert_eq!(id, "abc123"),
            _ => panic!("Expected Projects Delete"),
        }
    }

    #[test]
    fn test_parse_projects_archive() {
        let cli = Cli::try_parse_from(["borg", "projects", "archive", "abc123"]).unwrap();
        match cli.command {
            Some(Commands::Projects {
                action: Some(ProjectsAction::Archive { id }),
            }) => assert_eq!(id, "abc123"),
            _ => panic!("Expected Projects Archive"),
        }
    }
}
