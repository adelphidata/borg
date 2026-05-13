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
use crate::commands::providers::ProvidersAction;
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
mod borgd_launcher;
#[allow(dead_code)]
mod daemon_client;
mod logo;
mod migrate_tui;
mod onboarding;
mod onboarding_tui;
mod openrouter_catalog;
mod plugins;
mod repl;
mod terminal_guard;
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
    /// Run diagnostics, or manage squelches with subcommands
    Doctor {
        #[command(subcommand)]
        action: Option<commands::doctor::DoctorAction>,
    },
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
    /// Manage WASM plugin packages (install/list/show/remove/verify)
    Plugin {
        #[command(subcommand)]
        action: commands::wasm_plugin::WasmPluginAction,
    },
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
    /// List, show, enable, or disable agent skills
    Skills {
        #[command(subcommand)]
        action: Option<commands::skills::SkillsAction>,
    },
    /// Read, write, search, and delete long-term memory entries
    Memory {
        #[command(subcommand)]
        action: commands::memory::MemoryAction,
    },
    /// Run a one-shot self-heal sweep (logs, embeddings, stalled tasks)
    Heal,
    /// List or show stored sessions
    Sessions {
        #[command(subcommand)]
        action: Option<commands::sessions::SessionsAction>,
    },
    /// Trigger an immediate heartbeat check-in
    #[command(alias = "wake")]
    Poke,
    /// Run memory consolidation immediately
    Dream,
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
    /// Update borg to the latest released version
    Update(commands::update_args::UpdateArgs),
    /// Manage cron jobs (shell commands on a schedule)
    Cron {
        #[command(subcommand)]
        action: Option<CronAction>,
    },
    /// Manage bearer tokens for OpenAI-compat HTTP and MCP HTTP surfaces
    #[command(name = "client-token")]
    ClientToken {
        #[command(subcommand)]
        action: Option<commands::client_token::ClientTokenAction>,
    },
    /// Manage the per-provider LLM credential pool (see docs/llm-resilience.md)
    Auth {
        #[command(subcommand)]
        action: Option<commands::auth::AuthAction>,
    },
    /// Manage signed webhook definitions for /v1/webhook/{name}
    Webhook {
        #[command(subcommand)]
        action: Option<commands::webhook::WebhookAction>,
    },
    /// Run Borg as a Model Context Protocol (MCP) server
    Mcp {
        #[command(subcommand)]
        action: Option<commands::mcp::McpAction>,
    },
    /// Run Borg as an Agent Client Protocol (ACP) server over stdio
    Acp,
    /// Manage LLM providers (BYO endpoints, rate-card refresh)
    Providers {
        #[command(subcommand)]
        action: ProvidersAction,
    },
    /// Permanently delete all Borg data and uninstall the service
    Uninstall,
    /// Pair a device with the daemon over the mTLS bootstrap socket.
    ///
    /// Run on the daemon host: `borg pair show` mints + prints a code.
    /// Run on the client device: `borg pair <code>` drives the SPAKE2
    /// handshake and stashes the issued cert in the OS keychain.
    Pair {
        #[command(subcommand)]
        action: PairCmd,
    },
    /// Revoke or list paired devices.
    Revoke {
        #[command(subcommand)]
        action: Option<RevokeCmd>,
    },
    /// Verify the integrity of the audit chain.
    Audit {
        #[command(subcommand)]
        action: AuditCmd,
    },
    /// Create, verify, or restore a `BORG_HOME` backup tarball.
    Backup {
        #[command(subcommand)]
        action: BackupCmd,
    },
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

#[derive(Subcommand)]
enum PairCmd {
    /// Mint a pairing code and print it (run on the daemon host).
    Show,
    /// Complete pairing using a code (run on the client device).
    Complete {
        /// The pairing code shown by `borg pair show` on the daemon host.
        code: String,
        /// Human-readable label for this device.
        #[arg(long, default_value = "borg-device")]
        label: String,
        /// Override the bootstrap socket path.
        #[arg(long)]
        socket: Option<std::path::PathBuf>,
    },
}

#[derive(Subcommand)]
enum RevokeCmd {
    /// Revoke a paired device by fingerprint.
    Device {
        /// SHA-256 cert fingerprint (lowercase hex, no colons).
        fingerprint: String,
        /// Reason recorded in the audit log.
        #[arg(long)]
        reason: Option<String>,
    },
    /// List all paired devices.
    List,
}

#[derive(Subcommand)]
enum AuditCmd {
    /// Replay the audit chain and report Ok or Tampered(seq).
    Verify,
}

#[derive(Subcommand)]
enum BackupCmd {
    /// Write a tarball of the current `BORG_HOME` to `~/.borg/backups/`.
    Create,
    /// Verify a backup tarball: digest, manifest, and embedded audit chain.
    Verify {
        /// Path to the `borg-backup-*.tar.zst` file.
        file: std::path::PathBuf,
    },
    /// Restore a backup into `BORG_HOME`. Refuses to clobber existing
    /// state without `--force` (which renames the existing dir to
    /// `<BORG_HOME>.bak.<unix_ts>` first).
    Restore {
        /// Path to the `borg-backup-*.tar.zst` file.
        file: std::path::PathBuf,
        #[arg(long)]
        force: bool,
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
    if let Ok(data_dir) = cortex::config::Config::data_dir() {
        let _ = dotenvy::from_path(data_dir.join(".env"));
        // Harden data directory permissions (owner-only access)
        harden_data_dir(&data_dir);
    }

    let cli = Cli::parse();

    if matches!(cli.command, Some(Commands::Acp)) {
        return commands::acp::run_acp_stdio().await;
    }

    let tui_mode = matches!(cli.command, Some(Commands::Start) | None);

    // _guard and _telemetry_guard must live for the program's duration to flush logs
    let _guard;
    let _telemetry_guard;

    {
        let config = cortex::config::Config::load_from_db().unwrap_or_default();

        // Install the process-global output-redaction policy for standalone
        // `borg` (TUI / REPL without `borgd`). When `borgd` is the host the
        // daemon installs its own policy with a shared DB handle; here the
        // TUI opens its own connection just for audit writes. Gated by
        // `security.output_redaction` (default true). The policy will still
        // strip secrets even if the audit-DB open fails — failing closed
        // (i.e. skipping redaction) would be the wrong choice security-wise.
        match cortex::db::Database::open() {
            Ok(db) => {
                use std::sync::{Arc, Mutex};
                cortex::output_redaction::install_global(
                    cortex::output_redaction::RedactionPolicy::from_db(
                        config.security.output_redaction,
                        Arc::new(Mutex::new(db)),
                    ),
                );
            }
            Err(e) => {
                eprintln!("Warning: audit-DB open failed ({e}); output redaction will run without audit logging");
                cortex::output_redaction::install_global(if config.security.output_redaction {
                    cortex::output_redaction::RedactionPolicy::enabled_no_audit()
                } else {
                    cortex::output_redaction::RedactionPolicy::disabled()
                });
            }
        }

        let (otel_layer, tg) =
            cortex::telemetry::init_telemetry(&config.telemetry).unwrap_or_else(|e| {
                eprintln!("Warning: OpenTelemetry init failed: {e}");
                (None, cortex::telemetry::TelemetryGuard::noop())
            });
        _telemetry_guard = tg;

        let env_filter = EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| EnvFilter::new("warn,chromiumoxide=error,tungstenite=error"));

        if tui_mode {
            let (non_blocking, guard) = match cortex::config::Config::logs_dir() {
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
        Some(Commands::Init) | Some(Commands::Uninstall) | Some(Commands::Service { .. })
    ) {
        if let Err(e) = borgd_launcher::ensure_running().await {
            tracing::warn!("Auto-start borgd: {e}");
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
        Some(Commands::Stop) => borgd_launcher::stop_service().await?,
        Some(Commands::Restart) => borgd_launcher::restart_service().await?,
        Some(Commands::Init) => init_data_dir()?,
        Some(Commands::Ask {
            message,
            json,
            mode,
        }) => repl::one_shot(&message, json, mode.as_deref()).await?,
        Some(Commands::Status { action }) => commands::status::dispatch_status(action)?,
        Some(Commands::Bond { action }) => commands::status::dispatch_bond(action)?,
        Some(Commands::Doctor { action }) => commands::doctor::dispatch_doctor(action)?,
        Some(Commands::Skills { action }) => commands::skills::dispatch_skills(action)?,
        Some(Commands::Memory { action }) => commands::memory::dispatch_memory(action)?,
        Some(Commands::Heal) => commands::heal::run_heal().await?,
        Some(Commands::Sessions { action }) => {
            commands::sessions::dispatch_sessions(action).await?
        }
        Some(Commands::Service { action }) => commands::misc::dispatch_service(action).await?,
        Some(Commands::Add { name }) => plugins::add_plugin(&name)?,
        Some(Commands::Remove { name }) => plugins::remove_plugin(&name)?,
        Some(Commands::Plugins) => plugins::list_plugins()?,
        Some(Commands::Plugin { action }) => commands::wasm_plugin::dispatch(action)?,
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
        Some(Commands::Providers { action }) => commands::providers::dispatch(action)?,
        Some(Commands::Usage) => commands::status::run_usage()?,
        Some(Commands::Pairing { action }) => commands::pairing::dispatch_pairing(action)?,
        Some(Commands::Poke) => commands::misc::run_poke().await?,
        Some(Commands::Dream) => commands::misc::run_dream().await?,
        Some(Commands::Cancel { session }) => commands::misc::run_cancel(session).await?,
        Some(Commands::Away { message }) => commands::misc::run_away(message).await?,
        Some(Commands::Available) => commands::misc::run_available().await?,
        Some(Commands::Migrate { action }) => commands::misc::dispatch_migrate(action)?,
        Some(Commands::Update(args)) => {
            commands::misc::run_update(args.prerelease, args.check).await?
        }
        Some(Commands::ClientToken { action }) => {
            commands::client_token::dispatch_client_token(action)?
        }
        Some(Commands::Auth { action }) => commands::auth::dispatch_auth(action)?,
        Some(Commands::Webhook { action }) => commands::webhook::dispatch_webhook(action)?,
        Some(Commands::Mcp { action }) => commands::mcp::dispatch_mcp(action).await?,
        Some(Commands::Acp) => unreachable!("ACP handled before interactive bootstrap"),
        Some(Commands::Uninstall) => commands::misc::run_uninstall()?,
        Some(Commands::Export {
            session_id,
            format,
            output,
        }) => commands::export::run_export(&session_id, &format, output)?,
        Some(Commands::Pair { action }) => match action {
            PairCmd::Show => commands::device_pair::run_show().await?,
            PairCmd::Complete {
                code,
                label,
                socket,
            } => commands::device_pair::run_complete(&code, &label, socket).await?,
        },
        Some(Commands::Revoke { action }) => match action {
            None | Some(RevokeCmd::List) => commands::revoke::run_list().await?,
            Some(RevokeCmd::Device {
                fingerprint,
                reason,
            }) => commands::revoke::run(&fingerprint, reason.as_deref()).await?,
        },
        Some(Commands::Audit { action }) => match action {
            AuditCmd::Verify => commands::audit::run_verify().await?,
        },
        Some(Commands::Backup { action }) => match action {
            BackupCmd::Create => commands::backup::run_create()?,
            BackupCmd::Verify { file } => commands::backup::run_verify(&file)?,
            BackupCmd::Restore { file, force } => commands::backup::run_restore(&file, force)?,
        },
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
/// Routed through `borgd` over UDS — the daemon is the single source of
/// truth for the `onboarded` settings-table sentinel. The cli no longer
/// opens the DB itself.
///
/// Returns false (and logs) on:
/// - missing data dir / missing `borg.db` (genuinely fresh install)
/// - daemon connect / RPC failure (intrusive but safer than silently
///   skipping onboarding).
async fn is_onboarded_async() -> bool {
    let Ok(data_dir) = cortex::config::Config::data_dir() else {
        return false;
    };
    // Fresh install: no DB → never onboarded. Skip the daemon round-trip
    // (which would otherwise spin up borgd and create the file) so the
    // wizard runs cleanly on a brand-new system.
    let db_path = data_dir.join("borg.db");
    if !db_path.exists() {
        return false;
    }
    if let Ok(mut client) = daemon_client::connect(None).await {
        if let Ok(state) = client.get_onboarding_state().await {
            return state.onboarded;
        }
    }
    // Daemon unreachable (crash loop, never started, or just slow). Without
    // this fallback every transient daemon outage re-prompts onboarding,
    // which clobbers existing state. The sentinel is a single read of a
    // boolean settings row — safe to do directly without migrations.
    match read_onboarded_sentinel(&db_path) {
        Ok(v) => v,
        Err(SentinelError::Transient(e)) => {
            // Lock contention / IO blip: assume onboarded so a transient
            // daemon-write race doesn't re-prompt the wizard and clobber
            // existing state.
            tracing::warn!("is_onboarded: transient DB read failure ({e}); assuming onboarded");
            true
        }
        Err(SentinelError::Fatal(e)) => {
            // Corrupt DB / missing schema / not-a-database: there's nothing
            // to clobber, and re-running the wizard is the recovery path.
            tracing::warn!("is_onboarded: DB unusable ({e}); will re-run onboarding");
            false
        }
    }
}

enum SentinelError {
    /// SQLITE_BUSY / SQLITE_LOCKED / IO — likely recovers on next run.
    Transient(anyhow::Error),
    /// SQLITE_CORRUPT / SQLITE_NOTADB / missing `settings` table — DB is
    /// effectively empty for our purposes; safe to re-onboard.
    Fatal(anyhow::Error),
}

impl std::fmt::Display for SentinelError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SentinelError::Transient(e) | SentinelError::Fatal(e) => write!(f, "{e}"),
        }
    }
}

/// Best-effort read of the `onboarded` settings row without running
/// migrations or contending with the daemon's writer connection.
///
/// Distinguishes three cases:
/// - row present and "true" → onboarded
/// - row absent or != "true" → not onboarded
/// - DB locked / query error → returns `Err` (caller decides; see above)
///
/// Sets a busy_timeout so a mid-write daemon (BEGIN IMMEDIATE / checkpoint)
/// doesn't immediately surface as `SQLITE_BUSY`.
fn read_onboarded_sentinel(db_path: &std::path::Path) -> Result<bool, SentinelError> {
    use rusqlite::ffi::ErrorCode;
    let classify = |e: rusqlite::Error| -> SentinelError {
        match &e {
            rusqlite::Error::SqliteFailure(err, _) => match err.code {
                ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked => {
                    SentinelError::Transient(e.into())
                }
                ErrorCode::DatabaseCorrupt | ErrorCode::NotADatabase => {
                    SentinelError::Fatal(e.into())
                }
                _ => SentinelError::Transient(e.into()),
            },
            // Missing `settings` table on a partially-initialized DB.
            rusqlite::Error::SqlInputError { .. } => SentinelError::Fatal(e.into()),
            _ => SentinelError::Transient(e.into()),
        }
    };

    let conn = rusqlite::Connection::open_with_flags(
        db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(classify)?;
    conn.busy_timeout(std::time::Duration::from_secs(2))
        .map_err(classify)?;
    let value: Option<String> = match conn.query_row(
        "SELECT value FROM settings WHERE key = 'onboarded'",
        [],
        |r| r.get::<_, String>(0),
    ) {
        Ok(v) => Some(v),
        Err(rusqlite::Error::QueryReturnedNoRows) => None,
        Err(e) => return Err(classify(e)),
    };
    Ok(matches!(value.as_deref(), Some("true")))
}

/// Sync wrapper used by `ensure_onboarded` (which is itself called from a
/// sync clap dispatch).
fn is_onboarded() -> bool {
    match tokio::runtime::Handle::try_current() {
        Ok(h) => tokio::task::block_in_place(|| h.block_on(is_onboarded_async())),
        Err(_) => {
            // No tokio runtime — fall back to a fresh one. Should not
            // happen in practice because main is `#[tokio::main]`.
            tracing::warn!("is_onboarded: no tokio runtime; falling back to false");
            false
        }
    }
}

fn init_data_dir() -> Result<()> {
    let data_dir = cortex::config::Config::data_dir()?;
    std::fs::create_dir_all(&data_dir)?;

    // Run interactive onboarding wizard
    match onboarding::run_onboarding()? {
        Some(result) => {
            onboarding::apply_onboarding(&result)?;

            // Auto-launch TUI for the first conversation (check before daemon
            // start to avoid printing output that flashes between terminal modes)
            let setup_path = data_dir.join("SETUP.md");
            let will_auto_launch = setup_path.exists() && std::io::stdin().is_terminal();

            // Auto-start daemon service after successful onboarding (non-fatal).
            // `init_data_dir` is sync, so block on the async ensure_running via
            // the current tokio handle.
            let ensure_result: Result<()> = match tokio::runtime::Handle::try_current() {
                Ok(h) => tokio::task::block_in_place(|| {
                    h.block_on(async { borgd_launcher::ensure_running().await })
                }),
                Err(_) => Err(anyhow::anyhow!("no tokio runtime available")),
            };
            if let Err(e) = ensure_result {
                tracing::warn!("Auto-start service during onboarding: {e}");
            }

            if will_auto_launch {
                // The TUI will detect SETUP.md and inject it into the system prompt
                return Ok(());
            }

            println!();
            println!("You're all set! Run `borg` to start chatting.");

            // Headless Linux servers ride the file-encrypted KEK fallback by
            // default. Surface the secure-upgrade path so the user sees it
            // exactly once at onboarding completion rather than discovering
            // the Reduced posture multiplier mid-session.
            if borgd_launcher::is_headless_linux() {
                borgd_launcher::print_headless_setup_hint();
            }
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
    use cortex::constants::{IDENTITY_FILE, MEMORY_INDEX_FILE};

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
        let identity = cortex::identity::load_identity()?;
        cortex::identity::save_identity(&identity)?;
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

    // Onboarding-sentinel round-trip is covered in `borgd`'s
    // `admin_rpc_smoke::onboarding_state_round_trips_through_settings_table`
    // — that exercises the live daemon path the cli now uses, which is more
    // honest than testing a since-removed `is_onboarded_from_db` helper
    // against an in-memory DB.

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

    // -- `borg update` parsing --

    fn parse_update(args: &[&str]) -> commands::update_args::UpdateArgs {
        let mut full = vec!["borg", "update"];
        full.extend_from_slice(args);
        let cli = match Cli::try_parse_from(full) {
            Ok(c) => c,
            Err(e) => panic!("parse failed: {e}"),
        };
        match cli.command {
            Some(Commands::Update(a)) => a,
            _ => panic!("expected Update subcommand"),
        }
    }

    #[test]
    fn parse_update_no_flags_is_default() {
        let a = parse_update(&[]);
        assert!(!a.prerelease);
        assert!(!a.check);
    }

    #[test]
    fn parse_update_prerelease_flag() {
        assert!(parse_update(&["--prerelease"]).prerelease);
        assert!(parse_update(&["--pre"]).prerelease);
        assert!(parse_update(&["-p"]).prerelease);
    }

    #[test]
    fn parse_update_dev_alias_back_compat() {
        // --dev must still work for users with old muscle memory.
        let a = parse_update(&["--dev"]);
        assert!(a.prerelease);
        assert!(!a.check);
    }

    #[test]
    fn parse_update_check_flag() {
        assert!(parse_update(&["--check"]).check);
        assert!(parse_update(&["-c"]).check);
    }

    #[test]
    fn parse_update_combined_short_flags() {
        let a = parse_update(&["-pc"]);
        assert!(a.prerelease);
        assert!(a.check);
    }

    #[test]
    fn parse_update_rejects_unknown_flag() {
        let err = match Cli::try_parse_from(["borg", "update", "--bogus"]) {
            Err(e) => e,
            Ok(_) => panic!("--bogus must error"),
        };
        assert!(
            err.to_string().contains("bogus"),
            "error must name token: {err}"
        );
    }

    #[test]
    fn update_help_does_not_advertise_dev_alias() {
        // --help text for the subcommand must not show --dev (it's a hidden alias).
        let err = match Cli::try_parse_from(["borg", "update", "--help"]) {
            Err(e) => e,
            Ok(_) => panic!("--help should short-circuit with help output"),
        };
        let help = err.to_string();
        assert!(
            help.contains("--prerelease"),
            "missing --prerelease: {help}"
        );
        assert!(help.contains("--check"), "missing --check: {help}");
        assert!(!help.contains("--dev"), "--dev should be hidden: {help}");
    }

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
        let cli = Cli::try_parse_from(["borg", "ask", "-j", "hello"]).unwrap();
        match cli.command {
            Some(Commands::Ask {
                message,
                json,
                mode,
            }) => {
                assert_eq!(message, "hello");
                assert!(json);
                assert!(mode.is_none());
            }
            _ => panic!("Expected Ask"),
        }
    }

    #[test]
    fn test_parse_ask_rejects_removed_yes_flag() {
        let err = match Cli::try_parse_from(["borg", "ask", "-y", "hello"]) {
            Ok(_) => panic!("expected -y to be rejected"),
            Err(err) => err,
        };
        let msg = err.to_string();
        assert!(msg.contains("unexpected argument '-y'"), "got: {msg}");
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
        let config = cortex::config::Config::default();
        let output = config.display_settings();
        assert!(!output.is_empty());
        assert!(output.contains("temperature"));
    }

    #[test]
    fn test_settings_apply_valid_key() {
        let mut config = cortex::config::Config::default();
        let result = config.apply_setting("temperature", "0.5");
        assert!(result.is_ok());
        assert!((config.llm.temperature - 0.5).abs() < f32::EPSILON);
    }

    #[test]
    fn test_settings_apply_invalid_key() {
        let mut config = cortex::config::Config::default();
        let result = config.apply_setting("nonexistent", "x");
        assert!(result.is_err());
    }

    #[test]
    fn test_settings_apply_invalid_value() {
        let mut config = cortex::config::Config::default();
        let result = config.apply_setting("temperature", "not_a_number");
        assert!(result.is_err());
    }

    // -- Tasks tests (using temp DB) --

    #[test]
    fn test_tasks_validate_schedule_cron() {
        assert!(cortex::tasks::validate_schedule("cron", "0 */5 * * * *").is_ok());
    }

    #[test]
    fn test_tasks_validate_schedule_interval() {
        assert!(cortex::tasks::validate_schedule("interval", "30m").is_ok());
    }

    #[test]
    fn test_tasks_validate_schedule_invalid() {
        assert!(cortex::tasks::validate_schedule("cron", "not valid").is_err());
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

    /// Table-driven check that argv-less subcommands wired through clap derive
    /// resolve to the right `Commands::*` variant. Pins the user-facing CLI
    /// surface — renaming a `#[command(name = ...)]` would silently break
    /// any external scripts. `test_parse_heal` and `test_parse_acp` below
    /// fold into this once we touch them again.
    #[test]
    fn parses_no_arg_subcommands_to_correct_variant() {
        type Check = fn(&Cli);
        let cases: &[(&[&str], Check)] = &[
            (&["borg", "dream"], |cli| {
                assert!(
                    matches!(cli.command, Some(Commands::Dream)),
                    "dream did not parse to Commands::Dream"
                )
            }),
            (&["borg", "heal"], |cli| {
                assert!(
                    matches!(cli.command, Some(Commands::Heal)),
                    "heal did not parse to Commands::Heal"
                )
            }),
            (&["borg", "acp"], |cli| {
                assert!(
                    matches!(cli.command, Some(Commands::Acp)),
                    "acp did not parse to Commands::Acp"
                )
            }),
        ];
        for (args, check) in cases {
            let cli = Cli::try_parse_from(*args).unwrap_or_else(|e| panic!("parse {args:?}: {e}"));
            check(&cli);
        }
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

    // ── Skills CLI parse tests ──

    #[test]
    fn test_parse_skills_list_default() {
        let cli = Cli::try_parse_from(["borg", "skills"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Commands::Skills { action: None })
        ));
    }

    #[test]
    fn test_parse_skills_enable_disable() {
        let cli = Cli::try_parse_from(["borg", "skills", "enable", "git"]).unwrap();
        match cli.command {
            Some(Commands::Skills {
                action: Some(commands::skills::SkillsAction::Enable { name }),
            }) => assert_eq!(name, "git"),
            _ => panic!("Expected Skills Enable"),
        }
        let cli = Cli::try_parse_from(["borg", "skills", "disable", "docker"]).unwrap();
        match cli.command {
            Some(Commands::Skills {
                action: Some(commands::skills::SkillsAction::Disable { name }),
            }) => assert_eq!(name, "docker"),
            _ => panic!("Expected Skills Disable"),
        }
    }

    #[test]
    fn test_parse_skills_show() {
        let cli = Cli::try_parse_from(["borg", "skills", "show", "borg-cli"]).unwrap();
        match cli.command {
            Some(Commands::Skills {
                action: Some(commands::skills::SkillsAction::Show { name }),
            }) => assert_eq!(name, "borg-cli"),
            _ => panic!("Expected Skills Show"),
        }
    }

    // ── Memory CLI parse tests ──

    #[test]
    fn test_parse_memory_list_default_scope() {
        let cli = Cli::try_parse_from(["borg", "memory", "list"]).unwrap();
        match cli.command {
            Some(Commands::Memory {
                action: commands::memory::MemoryAction::List { scope },
            }) => assert_eq!(scope, "global"),
            _ => panic!("Expected Memory List"),
        }
    }

    #[test]
    fn test_parse_memory_write_requires_content_or_file() {
        // Both omitted: clap accepts (mutually-exclusive args are optional);
        // the runtime check rejects. This test pins the parse-level shape.
        let cli = Cli::try_parse_from([
            "borg",
            "memory",
            "write",
            "global",
            "notes",
            "--content",
            "hi",
        ])
        .unwrap();
        match cli.command {
            Some(Commands::Memory {
                action:
                    commands::memory::MemoryAction::Write {
                        scope,
                        name,
                        content,
                        file,
                        append,
                    },
            }) => {
                assert_eq!(scope, "global");
                assert_eq!(name, "notes");
                assert_eq!(content.as_deref(), Some("hi"));
                assert!(file.is_none());
                assert!(!append);
            }
            _ => panic!("Expected Memory Write"),
        }
    }

    #[test]
    fn test_parse_memory_search() {
        let cli =
            Cli::try_parse_from(["borg", "memory", "search", "rust patterns", "--top-k", "5"])
                .unwrap();
        match cli.command {
            Some(Commands::Memory {
                action:
                    commands::memory::MemoryAction::Search {
                        query,
                        scope,
                        top_k,
                    },
            }) => {
                assert_eq!(query, "rust patterns");
                assert_eq!(scope, "global");
                assert_eq!(top_k, 5);
            }
            _ => panic!("Expected Memory Search"),
        }
    }

    #[test]
    fn test_parse_memory_content_and_file_conflict() {
        let err = Cli::try_parse_from([
            "borg",
            "memory",
            "write",
            "global",
            "n",
            "--content",
            "x",
            "--file",
            "/tmp/x",
        ]);
        assert!(err.is_err(), "--content + --file must conflict");
    }

    // ── Sessions parse tests ──
    // heal / acp parsing is covered by `parses_no_arg_subcommands_to_correct_variant` above.

    #[test]
    fn test_parse_sessions_list_with_limit() {
        let cli = Cli::try_parse_from(["borg", "sessions", "list", "--limit", "10"]).unwrap();
        match cli.command {
            Some(Commands::Sessions {
                action: Some(commands::sessions::SessionsAction::List { limit }),
            }) => assert_eq!(limit, 10),
            _ => panic!("Expected Sessions List"),
        }
    }

    #[test]
    fn test_parse_sessions_show() {
        let cli = Cli::try_parse_from(["borg", "sessions", "show", "abc123"]).unwrap();
        match cli.command {
            Some(Commands::Sessions {
                action: Some(commands::sessions::SessionsAction::Show { id }),
            }) => assert_eq!(id, "abc123"),
            _ => panic!("Expected Sessions Show"),
        }
    }

    // ── Doctor squelch parse tests ──

    #[test]
    fn test_parse_doctor_squelch_with_ttl() {
        let cli = Cli::try_parse_from([
            "borg",
            "doctor",
            "squelch",
            "Host Security",
            "ALF",
            "--ttl-days",
            "7",
        ])
        .unwrap();
        match cli.command {
            Some(Commands::Doctor {
                action:
                    Some(commands::doctor::DoctorAction::Squelch {
                        category,
                        name,
                        ttl_days,
                        reason,
                    }),
            }) => {
                assert_eq!(category, "Host Security");
                assert_eq!(name, "ALF");
                assert_eq!(ttl_days, 7);
                assert!(reason.is_none());
            }
            _ => panic!("Expected Doctor Squelch"),
        }
    }

    #[test]
    fn test_parse_doctor_unsquelch_and_squelches() {
        let cli =
            Cli::try_parse_from(["borg", "doctor", "unsquelch", "Host Security", "PF"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Commands::Doctor {
                action: Some(commands::doctor::DoctorAction::Unsquelch { .. })
            })
        ));
        let cli = Cli::try_parse_from(["borg", "doctor", "squelches"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Commands::Doctor {
                action: Some(commands::doctor::DoctorAction::Squelches)
            })
        ));
    }
}
