#![cfg_attr(
    test,
    allow(
        clippy::approx_constant,
        clippy::assertions_on_constants,
        clippy::const_is_empty,
        clippy::expect_used,
        clippy::field_reassign_with_default,
        clippy::identity_op,
        clippy::items_after_test_module,
        clippy::len_zero,
        clippy::manual_range_contains,
        clippy::needless_borrow,
        clippy::needless_collect,
        clippy::redundant_clone,
        clippy::redundant_closure_for_method_calls,
        clippy::uninlined_format_args,
        clippy::unnecessary_cast,
        clippy::unnecessary_map_or,
        clippy::unwrap_used,
        clippy::useless_format,
        clippy::useless_vec
    )
)]

use std::io::IsTerminal;

use anyhow::Result;
use clap::{Parser, Subcommand};
use tokio_util::sync::CancellationToken;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

// Action enums and their dispatch live in `commands/<name>.rs`; the `Cli`
// `Subcommand` enum below just refers to them.
use crate::commands::misc::{MigrateSubcommand, ServiceAction};
use crate::commands::pairing::PairingAction;
use crate::commands::projects::ProjectsAction;
use crate::commands::settings::SettingsAction;
use crate::commands::status::{BondAction, StatusAction};
use crate::commands::tasks::{CronAction, TasksAction};

mod api_key_store;
mod commands;
mod credentials;
// Daemon client lib lives in this crate to keep dev workflow tight; many of
// its methods are consumed only by call sites that haven't been migrated yet
// (TUI / REPL / popups). Allow dead_code on the module until those callers
// land — the lints would otherwise force placeholder usage that drifts.
#[allow(dead_code)]
mod daemon_client;
mod heartbeat_augmenters;
mod logo;
mod migrate_tui;
mod onboarding;
mod onboarding_tui;
mod openrouter_catalog;
mod plugins;
mod repl;
mod service;
mod tui;

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
    /// Export a session to JSON (default), CSV, or TXT.
    ///
    /// With no `--output`, writes to stdout (pipeable into `jq`, files, etc.).
    Export {
        /// Session ID (full UUID — use `/sessions` in the TUI to find one).
        session_id: String,
        /// Output format: json (default), csv, txt.
        #[arg(long, short, default_value = "json")]
        format: String,
        /// Write to a file instead of stdout.
        #[arg(long, short)]
        output: Option<std::path::PathBuf>,
    },
}

/// Runtime worker thread stack size.
///
/// Rust futures can grow large frames along the agent-loop → tool-handler → sub-agent
/// call chain. The tokio default (2 MiB on Linux/macOS) has been sufficient so far but
/// leaves little headroom, so Borg uses 4 MiB for all worker and blocking threads.
const RUNTIME_STACK_SIZE: usize = 4 * 1024 * 1024;

fn main() -> Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(RUNTIME_STACK_SIZE)
        .thread_name("borg-worker")
        .build()?;
    runtime.block_on(async_main())
}

async fn async_main() -> Result<()> {
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
        Some(Commands::Status { action }) => commands::status::dispatch_status(action)?,
        Some(Commands::Bond { action }) => commands::status::dispatch_bond(action)?,
        Some(Commands::Doctor) => commands::misc::run_doctor()?,
        Some(Commands::Daemon) => service::run_daemon(shutdown).await?,
        Some(Commands::Service { action }) => commands::misc::dispatch_service(action)?,
        Some(Commands::Add { name }) => plugins::add_plugin(&name)?,
        Some(Commands::Remove { name }) => plugins::remove_plugin(&name)?,
        Some(Commands::Plugins) => plugins::list_plugins()?,
        Some(Commands::Settings { action }) => commands::settings::dispatch_settings(action)?,
        Some(Commands::Logs {
            count,
            verbose,
            activity,
            level,
            category,
        }) => commands::settings::dispatch_logs(
            count,
            verbose,
            activity,
            &level,
            category.as_deref(),
        )?,
        Some(Commands::Tasks { action }) => commands::tasks::dispatch_tasks(action)?,
        Some(Commands::Cron { action }) => commands::tasks::dispatch_cron(action)?,
        Some(Commands::Projects { action }) => commands::projects::dispatch_projects(action)?,
        Some(Commands::Usage) => commands::status::run_usage()?,
        Some(Commands::Pairing { action }) => commands::pairing::dispatch_pairing(action)?,
        Some(Commands::Poke) => commands::misc::run_poke().await?,
        Some(Commands::Cancel { session }) => commands::misc::run_cancel(session).await?,
        Some(Commands::Away { message }) => commands::misc::run_away(message).await?,
        Some(Commands::Available) => commands::misc::run_available().await?,
        Some(Commands::Migrate { action }) => commands::misc::dispatch_migrate(action)?,
        Some(Commands::Update { dev, check }) => commands::misc::run_update(dev, check).await?,
        Some(Commands::Uninstall) => commands::misc::run_uninstall()?,
        Some(Commands::Export {
            session_id,
            format,
            output,
        }) => commands::export::run_export(&session_id, &format, output)?,
    }

    Ok(())
}

fn ensure_onboarded() -> Result<()> {
    if is_onboarded() {
        return Ok(());
    }
    init_data_dir()
}

/// Returns true if onboarding has already completed.
///
/// Config is DB-only since the V32 migration; the previous filesystem check
/// (presence of `config.toml`) was a stale signal that re-fired the wizard
/// every launch (V32 renames `config.toml` → `.bak` after import). Source
/// of truth is now the SQLite `settings` table — see `is_onboarded_from_db`.
///
/// Returns false (and logs) if the DB can't be opened. That re-runs the
/// wizard, which is intrusive on a transient lock — but it's the safer
/// default than silently skipping onboarding for a genuinely fresh install.
fn is_onboarded() -> bool {
    let Ok(data_dir) = borg_core::config::Config::data_dir() else {
        return false;
    };
    // Fresh install: no DB → never onboarded. Avoid Database::open() here
    // because it would create the file and run migrations on a fresh dir.
    if !data_dir.join("borg.db").exists() {
        return false;
    }
    let db = match borg_core::db::Database::open() {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!("is_onboarded: failed to open DB: {e}");
            return false;
        }
    };
    is_onboarded_from_db(&db)
}

/// Pure DB-side decision (extracted for testability against in-memory DBs).
///
/// Trade-offs:
/// - **Partially-aborted onboarding** (DB created by some earlier write, then
///   wizard cancelled): all settings still hold migration defaults
///   (`provider="(auto-detect)"`, `model=""`), so this returns false and the
///   user gets re-prompted. Intentional.
/// - **Legacy users clearing `model` via `/settings`** would re-trigger the
///   wizard on next launch. Users onboarded post-fix have the explicit
///   `onboarded=true` flag, so they're insulated. Acceptable: the legacy
///   cohort shrinks every time someone re-runs onboarding.
fn is_onboarded_from_db(db: &borg_core::db::Database) -> bool {
    if matches!(
        db.get_setting("onboarded").ok().flatten().as_deref(),
        Some("true")
    ) {
        return true;
    }
    // Backfill for users who completed onboarding before the `onboarded`
    // sentinel was introduced. Default-seeded settings populate `provider`
    // and `model` rows even on fresh DBs (defaults: "(auto-detect)" and "")
    // so we can't just check existence — we must check the *value*.
    // Onboarding writes a non-empty `model` (`apply_onboarding` in
    // `crates/cli/src/onboarding.rs`), so a non-empty `model` is the
    // most reliable post-onboarding signal.
    db.get_setting("model")
        .ok()
        .flatten()
        .is_some_and(|m| !m.is_empty())
}

fn init_data_dir() -> Result<()> {
    let data_dir = borg_core::config::Config::data_dir()?;
    std::fs::create_dir_all(&data_dir)?;

    // Run interactive onboarding wizard
    match onboarding::run_onboarding()? {
        Some(result) => {
            onboarding::apply_onboarding(&result)?;

            // Auto-launch TUI for the first conversation (check before daemon
            // start to avoid printing output that flashes between terminal modes)
            let setup_path = data_dir.join("SETUP.md");
            let will_auto_launch = setup_path.exists() && std::io::stdin().is_terminal();

            // Auto-start daemon service after successful onboarding (non-fatal)
            if let Err(e) = service::ensure_service_running() {
                tracing::warn!("Auto-start service during onboarding: {e}");
            }

            if will_auto_launch {
                // The TUI will detect SETUP.md and inject it into the system prompt
                return Ok(());
            }

            println!();
            println!("You're all set! Run `borg` to start chatting.");
        }
        None => {
            // User cancelled — fall back to defaults so the directory is still usable
            init_data_dir_defaults(&data_dir)?;
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

/// Non-interactive fallback: write default config files without the wizard.
fn init_data_dir_defaults(data_dir: &std::path::Path) -> Result<()> {
    use borg_core::constants::{IDENTITY_FILE, MEMORY_INDEX_FILE};

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
    let new_identity = data_dir.join(IDENTITY_FILE);
    if old_identity.exists() && !new_identity.exists() {
        std::fs::rename(&old_identity, &new_identity)?;
    }

    let identity_path = data_dir.join(IDENTITY_FILE);
    if !identity_path.exists() {
        let identity = borg_core::identity::load_identity()?;
        borg_core::identity::save_identity(&identity)?;
        println!("  Created {}", identity_path.display());
    }

    let memory_path = data_dir.join(MEMORY_INDEX_FILE);
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
        assert!(commands::misc::confirm_uninstall("y"));
    }

    #[test]
    fn confirm_uninstall_accepts_uppercase_y() {
        assert!(commands::misc::confirm_uninstall("Y"));
    }

    #[test]
    fn confirm_uninstall_rejects_empty() {
        assert!(!commands::misc::confirm_uninstall(""));
    }

    #[test]
    fn confirm_uninstall_rejects_no() {
        assert!(!commands::misc::confirm_uninstall("n"));
        assert!(!commands::misc::confirm_uninstall("N"));
    }

    #[test]
    fn confirm_uninstall_rejects_yes_spelled_out() {
        assert!(!commands::misc::confirm_uninstall("yes"));
    }

    #[test]
    fn confirm_uninstall_rejects_arbitrary_input() {
        assert!(!commands::misc::confirm_uninstall("maybe"));
        assert!(!commands::misc::confirm_uninstall("yy"));
    }

    #[test]
    fn is_onboarded_false_on_fresh_db_with_only_defaults() {
        // Migrations seed every setting with a default, so a fresh DB has
        // `provider="(auto-detect)"` and `model=""` rows present. The gate
        // must NOT mistake these for completed onboarding.
        let conn = rusqlite::Connection::open_in_memory().expect("open in-memory sqlite");
        let db = borg_core::db::Database::from_connection(conn).expect("init db");
        assert!(!super::is_onboarded_from_db(&db));
    }

    #[test]
    fn is_onboarded_true_with_explicit_flag() {
        let conn = rusqlite::Connection::open_in_memory().expect("open in-memory sqlite");
        let db = borg_core::db::Database::from_connection(conn).expect("init db");
        db.set_setting("onboarded", "true").unwrap();
        assert!(super::is_onboarded_from_db(&db));
    }

    #[test]
    fn is_onboarded_true_with_legacy_model_only() {
        // Regression guard: users who completed onboarding before the
        // `onboarded` sentinel was introduced still have a non-empty `model`
        // written by `apply_onboarding`. They must not be re-onboarded.
        let conn = rusqlite::Connection::open_in_memory().expect("open in-memory sqlite");
        let db = borg_core::db::Database::from_connection(conn).expect("init db");
        db.set_setting("model", "anthropic/claude-sonnet-4")
            .unwrap();
        assert!(super::is_onboarded_from_db(&db));
    }

    #[test]
    fn is_onboarded_false_when_flag_is_not_true() {
        // Non-"true" values must not satisfy the gate (e.g. partially-aborted writes).
        let conn = rusqlite::Connection::open_in_memory().expect("open in-memory sqlite");
        let db = borg_core::db::Database::from_connection(conn).expect("init db");
        db.set_setting("onboarded", "false").unwrap();
        assert!(!super::is_onboarded_from_db(&db));
    }

    #[test]
    fn delete_data_dir_removes_directory() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("borg_test");
        fs::create_dir_all(dir.join("memory")).unwrap();
        fs::write(dir.join("config.toml"), "test").unwrap();

        assert!(dir.exists());
        commands::misc::delete_data_dir(&dir).unwrap();
        assert!(!dir.exists());
    }

    #[test]
    fn delete_data_dir_noop_when_missing() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("nonexistent");
        assert!(!dir.exists());
        commands::misc::delete_data_dir(&dir).unwrap();
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
        assert_eq!(commands::truncate_str("hello", 10), "hello");
    }

    #[test]
    fn test_truncate_str_exact() {
        assert_eq!(commands::truncate_str("hello", 5), "hello");
    }

    #[test]
    fn test_truncate_str_long() {
        let result = commands::truncate_str("hello world", 5);
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

    // ── Export CLI parse tests ──

    #[test]
    fn test_parse_export_defaults_to_json_and_stdout() {
        let cli = Cli::try_parse_from(["borg", "export", "sess-abc"]).unwrap();
        match cli.command {
            Some(Commands::Export {
                session_id,
                format,
                output,
            }) => {
                assert_eq!(session_id, "sess-abc");
                assert_eq!(format, "json");
                assert!(output.is_none());
            }
            _ => panic!("Expected Export"),
        }
    }

    #[test]
    fn test_parse_export_csv_with_output() {
        let cli = Cli::try_parse_from([
            "borg",
            "export",
            "sess-abc",
            "--format",
            "csv",
            "--output",
            "/tmp/s.csv",
        ])
        .unwrap();
        match cli.command {
            Some(Commands::Export {
                session_id,
                format,
                output,
            }) => {
                assert_eq!(session_id, "sess-abc");
                assert_eq!(format, "csv");
                assert_eq!(output.unwrap().to_str().unwrap(), "/tmp/s.csv");
            }
            _ => panic!("Expected Export"),
        }
    }

    #[test]
    fn test_parse_export_short_flags() {
        let cli = Cli::try_parse_from(["borg", "export", "sess-xyz", "-f", "txt", "-o", "out.txt"])
            .unwrap();
        match cli.command {
            Some(Commands::Export { format, output, .. }) => {
                assert_eq!(format, "txt");
                assert_eq!(output.unwrap().to_str().unwrap(), "out.txt");
            }
            _ => panic!("Expected Export"),
        }
    }

    #[test]
    fn test_parse_export_requires_session_id() {
        assert!(Cli::try_parse_from(["borg", "export"]).is_err());
    }
}
