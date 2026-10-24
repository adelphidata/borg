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

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

// Action enums and their dispatch live in `commands/<name>.rs`; the `Cli`
// `Subcommand` enum below just refers to them.
use crate::commands::misc::{MigrateSubcommand, ServiceAction};
use crate::commands::pairing::PairingAction;
use crate::commands::providers::ProvidersAction;
use crate::commands::settings::SettingsAction;
use crate::commands::status::{BondAction, StatusAction};
use crate::commands::tasks::{CronAction, TasksAction};
use crate::commands::tunnel::TunnelCliAction;

mod api_key_store;
mod async_bridge;
mod commands;
// Daemon client lib lives in this crate to keep dev workflow tight; many of
// its methods are consumed only by call sites that haven't been migrated yet
// (TUI / REPL / popups). Allow dead_code on the module until those callers
// land — the lints would otherwise force placeholder usage that drifts.
mod borgd_launcher;
#[allow(dead_code)]
mod daemon_client;
mod logo;
mod migrate_tui;
mod ollama_catalog;
mod onboarding;
mod onboarding_tui;
mod openrouter_catalog;
mod plugins;
mod repl;
mod terminal_guard;
mod terminal_setup;
mod tui;

#[derive(Parser)]
#[command(name = "borg", about = "AI Personal Assistant Agent", version)]
struct Cli {
    /// Resume a specific session by ID (full UUID or unique prefix).
    /// Only applies when launching the interactive TUI.
    #[arg(long, global = true, value_name = "SESSION_ID")]
    resume: Option<String>,

    /// Resume the most recent session for this workspace (the enclosing git
    /// repo, or this directory when outside one). Falls back to the global
    /// most-recent session, and starts fresh when there are none.
    /// Only applies when launching the interactive TUI.
    /// (No `-c` short form: it would collide with subcommand flags like
    /// `logs -c`.)
    #[arg(long = "continue", global = true, conflicts_with = "resume")]
    continue_session: bool,

    /// Run inside an isolated git worktree instead of this checkout.
    /// Creates it on the first use and reuses it afterwards, so the same
    /// name always lands in the same place. Manage them with `borg worktree`.
    #[arg(long, short = 'w', global = true, value_name = "NAME")]
    worktree: Option<String>,

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
        /// Switch the collaboration mode (execute or plan) before the message
        /// is sent. Like /mode, it applies to every session.
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
        /// Apply detected auto-safe fixes (recreate runtime dirs, seed the
        /// memory index, backfill embeddings, clear an orphaned daemon lock).
        /// Takes a full backup under ~/.borg/backups/ before mutating.
        #[arg(long, visible_alias = "fix")]
        repair: bool,
        /// With --repair: proceed even if the pre-repair backup can't be
        /// written. Without it, a backup failure aborts before any mutation.
        #[arg(long)]
        force: bool,
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
    /// Manage the public-internet tunnel (cloudflared/ngrok/tailscale/custom)
    Tunnel {
        #[command(subcommand)]
        action: TunnelCliAction,
    },
    /// List or show stored sessions
    Sessions {
        #[command(subcommand)]
        action: Option<commands::sessions::SessionsAction>,
    },
    /// Send a message to a running session and print its reply
    Send {
        /// Session id (or a unique prefix) or title
        target: String,
        /// The message. Bare words are joined, so quoting is optional.
        #[arg(required = true, num_args = 1..)]
        message: Vec<String>,
    },
    /// Manage isolated git worktrees to code in
    #[command(alias = "wt")]
    Worktree {
        #[command(subcommand)]
        action: Option<commands::worktree::WorktreeAction>,
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
    /// Emergency stop: halt all NEW agent work (in-flight turns finish)
    ///
    /// Scheduled runs are deferred (not lost), new gateway/interactive turns
    /// get a "paused" notice, and heartbeat injection is skipped until
    /// `borg resume`. Internal maintenance keeps running.
    Pause {
        /// Reason shown in `borg status` and in paused-turn notices
        #[arg(long)]
        reason: Option<String>,
    },
    /// Release the emergency stop and resume normal operation
    Resume,
    /// Set the agent to away mode (auto-replies to messages)
    Away {
        /// Custom away message (uses config default if omitted)
        message: Option<String>,
    },
    /// Set the agent back to available mode (stops auto-replying)
    Available,
    /// Import settings from a supported upstream agent
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
    /// Open the browser UI, already signed in
    Web {
        /// Print the sign-in URL instead of opening a browser
        #[arg(long)]
        no_open: bool,
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
    /// Manage MCP (Model Context Protocol) — add/list/test external MCP servers, or run Borg as one over stdio
    Mcp {
        #[command(subcommand)]
        action: Option<commands::mcp::McpAction>,
    },
    /// Manage the browser CDP connection (connect|disconnect|status). Parity
    /// with the in-TUI `/browser` slash command.
    Browser {
        #[command(subcommand)]
        action: Option<commands::browser::BrowserAction>,
    },
    /// Inspect and control spawned sub-agents (list|tree|history|snapshot|kill|pause|resume).
    /// Parity with the in-TUI `/agents` slash command.
    Agents {
        #[command(subcommand)]
        action: commands::agents::AgentsAction,
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
    /// Export a session to JSON (default), CSV, TXT, or Markdown.
    ///
    /// With no `--output`, writes to stdout (pipeable into `jq`, files, etc.).
    /// Secrets are redacted by default (fail-closed); `--no-redact` opts out.
    Export {
        /// Session ID (full UUID — use `/sessions` in the TUI to find one).
        session_id: String,
        /// Output format: json (default), csv, txt, md.
        #[arg(long, short, default_value = "json")]
        format: String,
        /// Write to a file instead of stdout.
        #[arg(long, short)]
        output: Option<std::path::PathBuf>,
        /// Skip secret redaction and emit raw session content.
        #[arg(long)]
        no_redact: bool,
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
/// call chain, and the tokio default (2 MiB on Linux/macOS) overflows that chain (a
/// stack overflow `abort()`s the process — see `borgd::runtime`). The in-process agent
/// loop needs the same headroom the daemon does, so this reuses the daemon's single
/// source of truth rather than keeping a second 4 MiB constant in sync by hand.
const RUNTIME_STACK_SIZE: usize = borgd::runtime::WORKER_STACK_SIZE;

fn main() -> Result<()> {
    borgd::auth::tls::install_crypto_provider();

    // Windows sandbox trampoline. Intercepted before tokio + clap so a
    // policy violation that re-execs `borg.exe __windows-sandbox-spawn ...`
    // cannot accidentally spin up the agent, open the audit DB, or honor
    // a misbehaving `--resume` flag. The dispatcher exits via
    // `process::exit` so no Drop on later resources runs against an
    // unexpected state.
    #[cfg(target_os = "windows")]
    {
        let argv: Vec<String> = std::env::args().collect();
        if argv.get(1).map(String::as_str) == Some(sandbox::windows::SPAWN_SUBCOMMAND) {
            let exit_code = run_windows_sandbox_trampoline(&argv[2..]);
            std::process::exit(exit_code as i32);
        }
        // Tier 2 elevated runner: launched as the sandbox account via
        // CreateProcessWithLogonW. Same pre-tokio/clap short-circuit so
        // the runner never spins up the agent.
        if argv.get(1).map(String::as_str) == Some(sandbox::windows::RUNNER_SUBCOMMAND) {
            let exit_code = run_windows_sandbox_runner(&argv[2..]);
            std::process::exit(exit_code as i32);
        }
    }

    // MCP stdio watchdog trampoline (Unix): a re-exec of this binary to
    // supervise a stdio MCP server spawned by `borg mcp …` tooling and reap it
    // if this process dies ungracefully. Handled before tokio/clap so the
    // watchdog argv never reaches the CLI and never boots the agent.
    if let Some(code) = mcp::watchdog::dispatch() {
        std::process::exit(code);
    }

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(RUNTIME_STACK_SIZE)
        .thread_name("borg-worker")
        .build()?;
    runtime.block_on(async_main())
}

/// Body of the `__windows-sandbox-spawn` subcommand. Parses argv,
/// reads the framed `SpawnRequest` from the inherited pipe, applies
/// the policy via the Tier 1 runtime, and returns the wrapped child's
/// exit code. Any failure path emits a stderr diagnostic and returns
/// a non-zero exit — never falls through to running the wrapped
/// command without sandboxing.
#[cfg(target_os = "windows")]
fn run_windows_sandbox_trampoline(argv_after_subcommand: &[String]) -> u32 {
    let invocation = match sandbox::windows::parse_trampoline_args(argv_after_subcommand) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("borg sandbox trampoline: {e}");
            return 1;
        }
    };
    match sandbox::windows::runtime::run(&invocation) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("borg sandbox trampoline: {e}");
            1
        }
    }
}

/// Body of the `__windows-sandbox-runner` subcommand (Tier 2). Parses
/// `--pipe <name>`, connects back to the operator's named pipe, brokers
/// the sandboxed child, and returns its exit code. Fails closed on any
/// argv or pipe error.
#[cfg(target_os = "windows")]
fn run_windows_sandbox_runner(argv_after_subcommand: &[String]) -> u32 {
    let pipe = match sandbox::windows::extract_runner_pipe_arg(argv_after_subcommand) {
        Ok(p) => p.to_string(),
        Err(e) => {
            eprintln!("borg sandbox runner: {e}");
            return 1;
        }
    };
    match sandbox::windows::runtime::run_elevated_runner(&pipe) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("borg sandbox runner: {e}");
            1
        }
    }
}

/// Decide what `--worktree` means for this invocation.
///
/// Three outcomes, and the two non-obvious ones are the point:
///
/// * `borg worktree ...` ignores the flag. Resolving a worktree in order to
///   manage worktrees would create the very thing the operator may be about to
///   remove.
/// * `borg acp` **refuses** it. ACP sessions take their directory from the
///   editor, per session, so the flag cannot be honored — and an ignored flag
///   would leave the editor patching the main checkout while the operator
///   believed the run was isolated. Silence is the dangerous outcome here, so
///   this is the one case that errors rather than skipping.
fn resolve_worktree_request(cli: &Cli) -> Result<Option<&str>> {
    let Some(name) = cli.worktree.as_deref() else {
        return Ok(None);
    };
    match cli.command {
        Some(Commands::Worktree { .. }) => Ok(None),
        Some(Commands::Acp) => anyhow::bail!(
            "--worktree does not apply to `borg acp` — the editor supplies each \
             session's directory. Point the editor at the worktree instead \
             (`borg worktree list` prints its path)."
        ),
        _ => Ok(Some(name)),
    }
}

async fn async_main() -> Result<()> {
    load_env_files();

    let cli = Cli::parse();

    let requested_worktree = resolve_worktree_request(&cli)?.map(str::to_string);

    if matches!(cli.command, Some(Commands::Acp)) {
        return commands::acp::run_acp_stdio().await;
    }

    // A restore writes a whole `BORG_HOME`, and refuses one that already holds
    // state. The diagnostics setup below opens (creating) `borg.db` and the
    // auto-start launches a daemon on the home, so either would make a fresh
    // target look occupied and the restore could never run without `--force`.
    if let Some(Commands::Backup {
        action: BackupCmd::Restore { file, force },
    }) = &cli.command
    {
        return commands::backup::run_restore(file, *force).await;
    }

    let worktree_dir = enter_worktree(requested_worktree.as_deref()).await?;

    let tui_mode = matches!(cli.command, Some(Commands::Start) | None);

    // _guard and _telemetry_guard must live for the program's duration to flush logs
    let (_guard, _telemetry_guard) = init_diagnostics(tui_mode).await;

    spawn_shutdown_listener(tui_mode);

    autostart_daemon(&cli).await;

    dispatch_command(cli, worktree_dir).await
}

/// Best-effort .env loading plus data-dir permission hardening. Runs before
/// clap so credentials from either .env are visible to every subcommand.
fn load_env_files() {
    // Missing .env is the common case — silent best-effort load.
    let _ = dotenvy::dotenv();
    // Also load .env from the data directory (~/.borg/.env)
    if let Ok(data_dir) = cortex::config::Config::data_dir() {
        let _ = dotenvy::from_path(data_dir.join(".env"));
        // Harden data directory permissions (owner-only access)
        harden_data_dir(&data_dir);
    }
}

/// Apply `--worktree NAME` by moving the process into the worktree before
/// anything reads the current directory. Everything downstream — session
/// workspace identity, the agent's cwd, git context, project docs — derives
/// from it, so one chdir here is what makes the whole run isolated.
/// Deliberately skipped for `borg worktree ...` itself (the caller passes
/// `None` then): resolving a worktree in order to manage worktrees would make
/// `--worktree` create the very thing the operator may be about to remove.
///
/// Returns the worktree path for the session-open calls downstream: moving
/// this process is not enough, because the agent runs inside the long-lived
/// `borgd`.
async fn enter_worktree(name: Option<&str>) -> Result<Option<std::path::PathBuf>> {
    let Some(name) = name else {
        return Ok(None);
    };
    let dir = commands::worktree::resolve_or_create(name).await?;
    std::env::set_current_dir(&dir)
        .with_context(|| format!("entering worktree at {}", dir.display()))?;
    Ok(Some(dir))
}

/// Install the process-global output-redaction policy for standalone `borg`
/// (TUI / REPL without `borgd`). When `borgd` is the host the daemon installs
/// its own policy with a shared DB handle; here the TUI opens its own
/// connection just for audit writes. Gated by `security.output_redaction`
/// (default true). The policy will still strip secrets even if the audit-DB
/// open fails — failing closed (i.e. skipping redaction) would be the wrong
/// choice security-wise.
async fn install_redaction_policy(config: &cortex::config::Config) {
    match cortex::db::Database::open().await {
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

    // If the database was unreadable on open, recovery preserved it aside
    // and rebuilt an empty one so the agent could still boot. Tell the user
    // once, here, where the original went.
    if let Some(notice) = cortex::db::Database::take_recovery_notice() {
        eprintln!(
            "Notice: your database was unreadable and has been rebuilt empty. \
             The original was preserved at {} — remove it once you've confirmed \
             you don't need it.",
            notice.backup_path.display()
        );
    }
}

/// Open the append-only TUI log file as a non-blocking tracing writer,
/// falling back to a sink when the logs dir or file is unavailable.
fn tui_log_writer() -> (
    tracing_appender::non_blocking::NonBlocking,
    tracing_appender::non_blocking::WorkerGuard,
) {
    match cortex::config::Config::logs_dir() {
        Ok(log_dir) => {
            // The OpenOptions::open below surfaces any real dir-creation failure.
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
    }
}

/// Set up redaction, telemetry, and the tracing subscriber. Returns the two
/// guards that must live for the program's duration so logs flush on exit.
async fn init_diagnostics(
    tui_mode: bool,
) -> (
    Option<tracing_appender::non_blocking::WorkerGuard>,
    cortex::telemetry::TelemetryGuard,
) {
    let config = cortex::config::Config::load_from_db()
        .await
        .unwrap_or_default();

    install_redaction_policy(&config).await;

    let (otel_layers, telemetry_guard) = cortex::telemetry::init_telemetry(&config.telemetry)
        .unwrap_or_else(|e| {
            eprintln!("Warning: OpenTelemetry init failed: {e}");
            (Vec::new(), cortex::telemetry::TelemetryGuard::noop())
        });

    let env_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("warn,chromiumoxide=error,tungstenite=error"));

    let guard = if tui_mode {
        let (non_blocking, guard) = tui_log_writer();
        let fmt_layer = tracing_subscriber::fmt::layer()
            .with_target(false)
            .with_ansi(false)
            .with_writer(non_blocking);
        cortex::telemetry::install_subscriber(otel_layers, env_filter, Box::new(fmt_layer));
        Some(guard)
    } else {
        // Diagnostics go to stderr: stdout is the data channel for
        // `borg ask --json` and other machine-read output, and a stray warn
        // line there corrupts the consumer's parse.
        let fmt_layer = tracing_subscriber::fmt::layer()
            .with_target(false)
            .with_writer(std::io::stderr);
        cortex::telemetry::install_subscriber(otel_layers, env_filter, Box::new(fmt_layer));
        None
    };

    (guard, telemetry_guard)
}

/// Exit on SIGTERM, and on ctrl-c outside the TUI.
///
/// Installing the handlers replaces the default "terminate" action, so the
/// listener must end the process itself. The TUI reads ctrl-c as a key in raw
/// mode; a SIGINT reaches it only while a `!` shell command or an editor holds
/// the terminal, and that one belongs to the child, so the TUI ignores it.
fn spawn_shutdown_listener(tui_mode: bool) {
    tokio::spawn(async move {
        let code = wait_for_shutdown_signal(tui_mode).await;
        // A signal exit skips every drop guard, so undo raw mode here.
        if crossterm::terminal::is_raw_mode_enabled().unwrap_or(false) {
            tui::restore_terminal();
        }
        std::process::exit(code);
    });
}

/// Wait for the signal that should end the process and return the shell's
/// exit code for it (128 + signal number).
async fn wait_for_shutdown_signal(tui_mode: bool) -> i32 {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let (Ok(mut interrupt), Ok(mut terminate)) = (
            signal(SignalKind::interrupt()),
            signal(SignalKind::terminate()),
        ) else {
            // A handler that did not install leaves the default action, which
            // already ends the process.
            return std::future::pending().await;
        };
        loop {
            tokio::select! {
                _ = interrupt.recv() => if !tui_mode { return 130 },
                _ = terminate.recv() => return 143,
            }
        }
    }
    #[cfg(not(unix))]
    loop {
        if tokio::signal::ctrl_c().await.is_err() {
            return std::future::pending().await;
        }
        if !tui_mode {
            return 130;
        }
    }
}

/// Ensure daemon service is installed and running on every command
/// (skip commands that manage the service themselves, and `worktree`, which
/// is pure local git work — waiting seconds on a daemon that has nothing to
/// do would make listing worktrees feel broken)
async fn autostart_daemon(cli: &Cli) {
    if !matches!(
        cli.command,
        Some(Commands::Init)
            | Some(Commands::Uninstall)
            | Some(Commands::Service { .. })
            | Some(Commands::Worktree { .. })
    ) {
        if let Err(e) = borgd_launcher::ensure_running().await {
            tracing::warn!("Auto-start borgd: {e}");
        }
    }
}

/// Body of the default `borg` / `borg start` invocation: gate on onboarding,
/// resolve `--resume` / `--continue`, run the TUI, and print the resume hint.
async fn run_start(
    resume_flag: Option<String>,
    continue_session: bool,
    worktree_dir: Option<std::path::PathBuf>,
) -> Result<()> {
    ensure_onboarded()?;
    let resume = if let Some(prefix) = resume_flag {
        // Checked before the TUI starts: an id that matches nothing is a
        // failed command, not a fresh session.
        if let Err(e) = cortex::session::resolve_session_id(&prefix) {
            anyhow::bail!("{e}");
        }
        Some(prefix)
    } else if continue_session {
        resolve_continue_session().await
    } else {
        None
    };
    if let Some(hint) = repl::run(resume, worktree_dir).await? {
        println!();
        println!("Session: {} ({})", hint.title, hint.short_id);
        println!("Resume with: borg --resume {}", hint.short_id);
    }
    Ok(())
}

/// Flat subcommand dispatch: every arm delegates to a named handler.
async fn dispatch_command(cli: Cli, worktree_dir: Option<std::path::PathBuf>) -> Result<()> {
    let Cli {
        resume,
        continue_session,
        worktree: _,
        command,
    } = cli;
    match command {
        Some(Commands::Start) | None => {
            run_start(resume, continue_session, worktree_dir.clone()).await?
        }
        Some(Commands::Stop) => borgd_launcher::stop_service().await?,
        Some(Commands::Restart) => borgd_launcher::restart_service().await?,
        Some(Commands::Init) => init_data_dir()?,
        Some(Commands::Ask {
            message,
            json,
            mode,
        }) => repl::one_shot(&message, json, mode.as_deref(), worktree_dir.clone()).await?,
        Some(Commands::Status { action }) => commands::status::dispatch_status(action).await?,
        Some(Commands::Bond { action }) => commands::status::dispatch_bond(action).await?,
        Some(Commands::Doctor {
            action,
            repair,
            force,
        }) => commands::doctor::dispatch_doctor(action, repair, force).await?,
        Some(Commands::Skills { action }) => commands::skills::dispatch_skills(action).await?,
        Some(Commands::Memory { action }) => commands::memory::dispatch_memory(action).await?,
        Some(Commands::Heal) => commands::heal::run_heal().await?,
        Some(Commands::Tunnel { action }) => commands::tunnel::run_tunnel(action).await?,
        Some(Commands::Sessions { action }) => {
            commands::sessions::dispatch_sessions(action).await?
        }
        Some(Commands::Send { target, message }) => {
            commands::send::run(&target, &message.join(" ")).await?
        }
        Some(Commands::Worktree { action }) => commands::worktree::dispatch(action).await?,
        Some(Commands::Service { action }) => commands::misc::dispatch_service(action).await?,
        Some(Commands::Add { name }) => plugins::add_plugin(&name)?,
        Some(Commands::Remove { name }) => plugins::remove_plugin(&name)?,
        Some(Commands::Plugins) => plugins::list_plugins()?,
        Some(Commands::Plugin { action }) => commands::wasm_plugin::dispatch(action).await?,
        Some(Commands::Settings { action }) => {
            commands::settings::dispatch_settings(action).await?
        }
        Some(Commands::Logs {
            count,
            verbose,
            activity,
            level,
            category,
        }) => {
            commands::settings::dispatch_logs(count, verbose, activity, &level, category.as_deref())
                .await?
        }
        Some(Commands::Tasks { action }) => commands::tasks::dispatch_tasks(action)?,
        Some(Commands::Cron { action }) => commands::tasks::dispatch_cron(action)?,
        Some(Commands::Providers { action }) => commands::providers::dispatch(action).await?,
        Some(Commands::Usage) => commands::status::run_usage().await?,
        Some(Commands::Pairing { action }) => commands::pairing::dispatch_pairing(action)?,
        Some(Commands::Poke) => commands::misc::run_poke().await?,
        Some(Commands::Dream) => commands::misc::run_dream().await?,
        Some(Commands::Cancel { session }) => commands::misc::run_cancel(session).await?,
        Some(Commands::Pause { reason }) => commands::pause::run_pause(reason).await?,
        Some(Commands::Resume) => commands::pause::run_resume().await?,
        Some(Commands::Away { message }) => commands::misc::run_away(message).await?,
        Some(Commands::Available) => commands::misc::run_available().await?,
        Some(Commands::Migrate { action }) => commands::misc::dispatch_migrate(action).await?,
        Some(Commands::Update(args)) => {
            commands::misc::run_update(args.prerelease, args.check).await?
        }
        Some(Commands::Web { no_open }) => commands::web::run_web(no_open).await?,
        Some(Commands::ClientToken { action }) => {
            commands::client_token::dispatch_client_token(action).await?
        }
        Some(Commands::Auth { action }) => commands::auth::dispatch_auth(action)?,
        Some(Commands::Webhook { action }) => commands::webhook::dispatch_webhook(action).await?,
        Some(Commands::Mcp { action }) => commands::mcp::dispatch_mcp(action).await?,
        Some(Commands::Browser { action }) => commands::browser::dispatch_browser(action).await?,
        Some(Commands::Agents { action }) => commands::agents::dispatch_agents(action).await?,
        Some(Commands::Acp) => unreachable!("ACP handled before interactive bootstrap"),
        Some(Commands::Uninstall) => commands::misc::run_uninstall()?,
        Some(Commands::Export {
            session_id,
            format,
            output,
            no_redact,
        }) => commands::export::run_export(&session_id, &format, output, no_redact)?,
        Some(Commands::Pair { action }) => dispatch_pair(action).await?,
        Some(Commands::Revoke { action }) => dispatch_revoke(action).await?,
        Some(Commands::Audit { action }) => match action {
            AuditCmd::Verify => commands::audit::run_verify().await?,
        },
        Some(Commands::Backup { action }) => dispatch_backup(action).await?,
    }

    Ok(())
}

async fn dispatch_pair(action: PairCmd) -> Result<()> {
    match action {
        PairCmd::Show => commands::device_pair::run_show().await,
        PairCmd::Complete {
            code,
            label,
            socket,
        } => commands::device_pair::run_complete(&code, &label, socket).await,
    }
}

async fn dispatch_revoke(action: Option<RevokeCmd>) -> Result<()> {
    match action {
        None | Some(RevokeCmd::List) => commands::revoke::run_list().await,
        Some(RevokeCmd::Device {
            fingerprint,
            reason,
        }) => commands::revoke::run(&fingerprint, reason.as_deref()).await,
    }
}

async fn dispatch_backup(action: BackupCmd) -> Result<()> {
    match action {
        BackupCmd::Create => commands::backup::run_create().await,
        BackupCmd::Verify { file } => commands::backup::run_verify(&file).await,
        BackupCmd::Restore { file, force } => commands::backup::run_restore(&file, force).await,
    }
}

/// Resolve `--continue` to a session id: the most recent non-archived session
/// stamped with this workspace (enclosing git repo toplevel, else exact cwd
/// match), falling back to the global most-recent session with a printed
/// note. Never errors — any failure path prints/logs and returns `None`, so
/// the TUI just starts a fresh session.
async fn resolve_continue_session() -> Option<String> {
    let cwd = match std::env::current_dir() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("borg: --continue: cannot determine working directory ({e}); starting fresh");
            return None;
        }
    };
    let (cwd, repo_root) = cortex::session::workspace_identity(&cwd);
    let db = match cortex::db::Database::open().await {
        Ok(db) => db,
        Err(e) => {
            eprintln!("borg: --continue: database unavailable ({e}); starting fresh");
            return None;
        }
    };
    let workspace_hit = db
        .most_recent_session_in_workspace(
            repo_root.as_ref().map(|p| p.to_string_lossy()).as_deref(),
            &cwd.to_string_lossy(),
        )
        .await
        .unwrap_or_else(|e| {
            tracing::warn!(error = %e, "--continue workspace lookup failed");
            None
        });
    let row = match workspace_hit {
        Some(row) => row,
        None => {
            // Global fallback: newest resumable session anywhere.
            let global = match db.most_recent_resumable_session().await {
                Ok(row) => row,
                Err(e) => {
                    tracing::warn!(error = %e, "--continue global lookup failed");
                    None
                }
            };
            match global {
                Some(row) => {
                    eprintln!(
                        "borg: no previous session for this workspace; \
                         continuing the most recent session instead"
                    );
                    row
                }
                None => return None, // empty DB — start fresh, never error
            }
        }
    };
    // The TUI resume path resolves ids against the on-disk session files and
    // hard-exits on a miss; verify here so a DB row whose file is gone
    // degrades to a fresh session instead.
    match cortex::session::resolve_session_id(&row.id) {
        Ok(_) => Some(row.id),
        // The store itself could not be read: say so, rather than claiming the
        // file is gone. Starting fresh is still the only safe move.
        Err(e @ cortex::session::ResolveSessionError::Listing(_)) => {
            eprintln!("borg: {e}; starting fresh");
            None
        }
        Err(_) => {
            eprintln!(
                "borg: most recent workspace session {} has no session file; starting fresh",
                &row.id[..row.id.len().min(8)]
            );
            None
        }
    }
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
    let db_path = cortex::paths::db_path(&data_dir);
    if !db_path.exists() {
        return false;
    }
    if let Ok(mut client) = daemon_client::connect(None).await {
        if let Ok(state) = client.get_onboarding_state().await {
            return state.onboarded;
        }
    }
    // Daemon unreachable (crash loop, never started, or just slow). The DB
    // is encrypted at rest, so we can't peek at the `onboarded` settings
    // row without going through the full libsql open path (KEK + migrations)
    // — which races the daemon's writer. The conservative call when a DB
    // file exists is to assume onboarded; re-prompting the wizard during a
    // transient daemon outage would clobber existing state, which is worse
    // than the rare case of skipping onboarding on a half-initialized DB.
    tracing::warn!("is_onboarded: daemon unreachable but DB file present; assuming onboarded");
    true
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

    match onboarding::run_onboarding()? {
        Some(result) => {
            onboarding::apply_onboarding(&result)?;

            // Auto-launch TUI for the first conversation (check before daemon
            // start to avoid printing output that flashes between terminal modes)
            let setup_path = data_dir.join(cortex::constants::SETUP_FILE);
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
        // Best-effort: chmod failure (e.g. read-only FS) is not fatal to startup.
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

    /// `--help` is rendered from `///` doc comments, so a code note written
    /// there (why a variant is boxed, how an enum is laid out) is printed to
    /// the user as if it were usage text. Settings live in the DB only, so a
    /// mention of `config.toml` points at a file that does not exist.
    #[test]
    fn help_text_carries_no_code_notes() {
        use clap::CommandFactory;
        fn walk(cmd: &clap::Command, path: &str, found: &mut Vec<String>) {
            let texts = [cmd.get_about(), cmd.get_long_about()]
                .into_iter()
                .flatten()
                .map(|t| t.to_string())
                .chain(cmd.get_arguments().filter_map(|a| {
                    a.get_long_help()
                        .or_else(|| a.get_help())
                        .map(|t| t.to_string())
                }));
            for text in texts {
                if [
                    "this enum",
                    "Boxed:",
                    "Box<",
                    "See `PRESETS`",
                    "config.toml",
                ]
                .iter()
                .any(|n| text.contains(n))
                {
                    found.push(format!("{path}: {text}"));
                }
            }
            for sub in cmd.get_subcommands() {
                walk(sub, &format!("{path} {}", sub.get_name()), found);
            }
        }
        let mut found = Vec::new();
        walk(&Cli::command(), "borg", &mut found);
        assert!(found.is_empty(), "code notes in --help: {found:#?}");
    }

    /// Uninstall confirmation is intentionally narrower than a typical y/N
    /// prompt: only exactly `y` or `Y` proceeds. "yes" / empty / anything
    /// else is rejected so an accidental return key never wipes the data dir.
    #[test]
    fn confirm_uninstall_accepts_only_single_y_case_insensitive() {
        for accept in ["y", "Y"] {
            assert!(
                commands::misc::confirm_uninstall(accept),
                "should accept {accept:?}"
            );
        }
        for reject in ["", "n", "N", "yes", "maybe", "yy"] {
            assert!(
                !commands::misc::confirm_uninstall(reject),
                "should reject {reject:?}"
            );
        }
    }

    // Onboarding-sentinel round-trip is covered in `borgd`'s
    // `admin_rpc_smoke::onboarding_state_round_trips_through_settings_table`,
    // which exercises the live daemon path the cli actually uses.

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

    /// `borg update` flags: each name/alias/short maps to the same field, and
    /// `-pc` must combine. The old `--dev` spelling was dropped with the rest
    /// of the back-compat surface; [`parse_update_no_longer_accepts_dev`]
    /// pins that it is gone.
    #[test]
    fn parse_update_flag_aliases_and_combinations() {
        // (argv, expected prerelease, expected check)
        let cases: &[(&[&str], bool, bool)] = &[
            (&[], false, false),
            (&["--prerelease"], true, false),
            (&["--pre"], true, false),
            (&["-p"], true, false),
            (&["--check"], false, true),
            (&["-c"], false, true),
            (&["-pc"], true, true),
        ];
        for (args, prerelease, check) in cases {
            let a = parse_update(args);
            assert_eq!(a.prerelease, *prerelease, "args: {args:?}");
            assert_eq!(a.check, *check, "args: {args:?}");
        }
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

    /// The back-compat alias is gone, so it must fail like any other unknown
    /// flag rather than silently doing nothing.
    #[test]
    fn parse_update_no_longer_accepts_dev() {
        let err = match Cli::try_parse_from(["borg", "update", "--dev"]) {
            Err(e) => e,
            Ok(_) => panic!("--dev was removed and must not parse"),
        };
        assert!(err.to_string().contains("dev"), "error must name it: {err}");
    }

    #[test]
    fn update_help_lists_the_supported_flags_only() {
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
        assert!(!help.contains("--dev"), "--dev is gone: {help}");
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

    /// `--worktree` has to be reachable both bare (`borg -w x`, the TUI) and
    /// after a subcommand, because it is declared `global`. If it stopped being
    /// global, `borg ask -w fix "..."` would fail to parse rather than run in
    /// the worktree.
    #[test]
    fn test_parse_global_worktree_flag() {
        let cli = Cli::try_parse_from(["borg", "--worktree", "fix-login"]).unwrap();
        assert_eq!(cli.worktree.as_deref(), Some("fix-login"));
        assert!(
            cli.command.is_none(),
            "the bare form still launches the TUI"
        );

        let cli = Cli::try_parse_from(["borg", "ask", "hello", "-w", "fix-login"]).unwrap();
        assert_eq!(cli.worktree.as_deref(), Some("fix-login"));

        let cli = Cli::try_parse_from(["borg", "status"]).unwrap();
        assert_eq!(cli.worktree, None, "no flag means no worktree redirection");
    }

    /// The two exceptions to `--worktree` are the whole risk surface: skipping
    /// it for `borg worktree` keeps a management command from creating what it
    /// is about to remove, and refusing it for `borg acp` keeps an editor from
    /// silently patching the main checkout.
    #[test]
    fn worktree_request_skips_management_and_refuses_acp() {
        let cli = Cli::try_parse_from(["borg", "--worktree", "fix"]).unwrap();
        assert_eq!(resolve_worktree_request(&cli).unwrap(), Some("fix"));

        let cli = Cli::try_parse_from(["borg", "ask", "hi", "-w", "fix"]).unwrap();
        assert_eq!(resolve_worktree_request(&cli).unwrap(), Some("fix"));

        let cli = Cli::try_parse_from(["borg", "worktree", "list", "-w", "fix"]).unwrap();
        assert_eq!(
            resolve_worktree_request(&cli).unwrap(),
            None,
            "managing worktrees must not resolve one"
        );

        let cli = Cli::try_parse_from(["borg", "acp", "-w", "fix"]).unwrap();
        let err = resolve_worktree_request(&cli)
            .expect_err("acp must refuse rather than silently ignore the flag");
        assert!(
            err.to_string().contains("does not apply to `borg acp`"),
            "the error must say why: {err}"
        );

        let cli = Cli::try_parse_from(["borg", "acp"]).unwrap();
        assert_eq!(
            resolve_worktree_request(&cli).unwrap(),
            None,
            "plain acp is unaffected"
        );
    }

    /// `borg worktree` with no action must list rather than error, and `wt`
    /// must stay an alias — both are the shapes documented to operators.
    #[test]
    fn test_parse_worktree_subcommands() {
        let cli = Cli::try_parse_from(["borg", "worktree"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Commands::Worktree { action: None })
        ));

        let cli = Cli::try_parse_from(["borg", "wt", "list"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Commands::Worktree {
                action: Some(commands::worktree::WorktreeAction::List)
            })
        ));

        let cli = Cli::try_parse_from([
            "borg", "worktree", "add", "spike", "-b", "feat/x", "--from", "HEAD~2",
        ])
        .unwrap();
        match cli.command {
            Some(Commands::Worktree {
                action:
                    Some(commands::worktree::WorktreeAction::Add {
                        name,
                        branch,
                        from,
                        existing,
                        print_path,
                        ..
                    }),
            }) => {
                assert_eq!(name, "spike");
                assert_eq!(branch.as_deref(), Some("feat/x"));
                assert_eq!(from.as_deref(), Some("HEAD~2"));
                assert!(!existing);
                assert!(!print_path);
            }
            other => panic!("expected worktree add, got {:?}", other.is_some()),
        }

        // `--from` names a commit to fork a NEW branch from; `--existing`
        // checks a branch out at its own tip. Accepting both would report
        // success with the tree on a revision the operator did not ask for.
        assert!(
            Cli::try_parse_from(["borg", "worktree", "add", "x", "--existing", "--from", "v1"])
                .is_err(),
            "--from and --existing must not be accepted together"
        );

        let cli = Cli::try_parse_from(["borg", "worktree", "rm", "spike", "-f"]).unwrap();
        match cli.command {
            Some(Commands::Worktree {
                action: Some(commands::worktree::WorktreeAction::Remove { target, force }),
            }) => {
                assert_eq!(target, "spike");
                assert!(force);
            }
            other => panic!("expected worktree remove, got {:?}", other.is_some()),
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

    #[test]
    fn truncate_str_passes_through_or_appends_ellipsis() {
        assert_eq!(commands::truncate_str("hello", 10), "hello");
        assert_eq!(commands::truncate_str("hello", 5), "hello"); // exact
        assert_eq!(commands::truncate_str("hello world", 5), "hell…");
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

    // -- --continue flag parsing --

    #[test]
    fn test_parse_continue_flag() {
        let cli = Cli::try_parse_from(["borg", "--continue"]).unwrap();
        assert!(cli.continue_session);
        assert!(cli.command.is_none());

        let cli = Cli::try_parse_from(["borg", "start", "--continue"]).unwrap();
        assert!(cli.continue_session);
        assert!(matches!(cli.command, Some(Commands::Start)));

        let cli = Cli::try_parse_from(["borg"]).unwrap();
        assert!(!cli.continue_session);
    }

    #[test]
    fn test_continue_conflicts_with_resume() {
        // Two different answers to "which session?" must be rejected, not
        // silently prioritized.
        assert!(Cli::try_parse_from(["borg", "--continue", "--resume", "abcd1234"]).is_err());
    }

    #[test]
    fn test_continue_has_no_short_flag() {
        // `-c` is claimed by subcommand flags (e.g. `logs -c`); a global short
        // would collide, so `--continue` must stay long-only at the top level.
        assert!(Cli::try_parse_from(["borg", "-c"]).is_err());
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
                no_redact,
            }) => {
                assert_eq!(session_id, "sess-abc");
                assert_eq!(format, "json");
                assert!(output.is_none());
                assert!(!no_redact, "redaction must default ON");
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
                ..
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

    #[test]
    fn test_parse_export_markdown_with_no_redact() {
        let cli =
            Cli::try_parse_from(["borg", "export", "sess-md", "-f", "md", "--no-redact"]).unwrap();
        match cli.command {
            Some(Commands::Export {
                format, no_redact, ..
            }) => {
                assert_eq!(format, "md");
                assert!(no_redact);
            }
            _ => panic!("Expected Export"),
        }
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
    fn send_joins_bare_words_so_quoting_is_optional() {
        // `borg send work check the build log` has to mean what it reads as.
        // Requiring quotes here would be a papercut on the command people
        // reach for from a shell script, where the message is often built up
        // from variables.
        let cli =
            Cli::try_parse_from(["borg", "send", "my-session", "check", "the", "build", "log"])
                .expect("bare words must parse");
        match cli.command {
            Some(Commands::Send { target, message }) => {
                assert_eq!(target, "my-session");
                assert_eq!(message.join(" "), "check the build log");
            }
            _ => panic!("expected Send"),
        }
    }

    #[test]
    fn send_requires_both_a_target_and_a_message() {
        // A bare `borg send <target>` would otherwise start an empty turn on
        // somebody's live session.
        assert!(
            Cli::try_parse_from(["borg", "send", "my-session"]).is_err(),
            "a target with no message must be rejected at parse time"
        );
        assert!(
            Cli::try_parse_from(["borg", "send"]).is_err(),
            "no target at all must be rejected"
        );
    }

    #[test]
    fn test_parse_sessions_list_with_limit() {
        let cli = Cli::try_parse_from(["borg", "sessions", "list", "--limit", "10"]).unwrap();
        match cli.command {
            Some(Commands::Sessions {
                action: Some(commands::sessions::SessionsAction::List { limit, archived }),
            }) => {
                assert_eq!(limit, 10);
                assert!(!archived, "archived listing must be opt-in");
            }
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
                ..
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
                action: Some(commands::doctor::DoctorAction::Unsquelch { .. }),
                ..
            })
        ));
        let cli = Cli::try_parse_from(["borg", "doctor", "squelches"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Commands::Doctor {
                action: Some(commands::doctor::DoctorAction::Squelches),
                ..
            })
        ));
    }

    #[test]
    fn test_parse_doctor_repair_flags() {
        // --repair and its --fix alias must both set repair; --force is opt-in.
        let cli = Cli::try_parse_from(["borg", "doctor", "--repair"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Commands::Doctor {
                action: None,
                repair: true,
                force: false,
            })
        ));
        let cli = Cli::try_parse_from(["borg", "doctor", "--fix", "--force"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Commands::Doctor {
                action: None,
                repair: true,
                force: true,
            })
        ));
        // Bare `doctor` stays read-only.
        let cli = Cli::try_parse_from(["borg", "doctor"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Commands::Doctor {
                action: None,
                repair: false,
                force: false,
            })
        ));
    }
}
