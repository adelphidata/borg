//! `borgd` — the Borg agent daemon.
//!
//! Hosts the agent loop (lifted from the in-process TUI/REPL) and exposes
//! Session, Capability, Status, and Admin gRPC services over a Unix Domain
//! Socket.

use anyhow::Result;
use borgd::{daemon, paths};
use clap::Parser;
use std::path::PathBuf;
use tracing_subscriber::EnvFilter;

/// Command-line arguments for `borgd`.
#[derive(Debug, Parser)]
#[command(name = "borgd", version, about = "Borg agent daemon")]
struct Args {
    /// Override `$BORG_HOME`. Defaults to `~/.borg`.
    #[arg(long, env = "BORG_HOME")]
    borg_home: Option<PathBuf>,

    /// Optional loopback TCP listener address (e.g. `127.0.0.1:8009`). The
    /// listener enforces mTLS; clients must hold a paired-device cert.
    #[arg(long)]
    listen: Option<String>,
}

fn main() -> Result<()> {
    // MCP stdio watchdog trampoline (Unix): a re-exec of this binary to
    // supervise a stdio MCP server and reap it if the daemon dies ungracefully
    // (SIGKILL/crash). Handled before clap AND before the tokio runtime is
    // built so the watchdog argv never reaches the CLI, the watchdog never
    // boots the daemon, and a watchdog re-exec doesn't pay to spin up a
    // multi-thread worker pool it would only discard.
    if let Some(code) = mcp::watchdog::dispatch() {
        std::process::exit(code);
    }

    // The agent loop runs on a worker thread; its deep poll chain (large tool
    // futures like the browser handler + every `#[instrument]` layer) overflows
    // tokio's 2 MiB default worker stack, and a stack overflow `abort()`s the
    // whole daemon uncatchably. `build_runtime` enlarges the worker stack to
    // give that poll chain the headroom it needs.
    borgd::runtime::build_runtime()?.block_on(async_main())
}

async fn async_main() -> Result<()> {
    let args = Args::parse();
    let home = paths::resolve_borg_home(args.borg_home.as_deref())?;
    // Point config/DB resolution at the resolved home so telemetry settings
    // load from the same database the daemon will use.
    std::env::set_var("BORG_HOME", &home);

    // Load config once here (for telemetry init) and thread it into the daemon
    // so it isn't re-decrypted from the DB a second time. Off by default;
    // `telemetry.{tracing,metrics,logs}_enabled` opt in. The guard must outlive
    // the daemon so exporters flush on exit.
    let core_config = cortex::config::Config::load_from_db()
        .await
        .unwrap_or_default();
    let (otel_layers, _telemetry_guard) = cortex::telemetry::init_telemetry(&core_config.telemetry)
        .unwrap_or_else(|e| {
            eprintln!("Warning: OpenTelemetry init failed: {e}");
            (Vec::new(), cortex::telemetry::TelemetryGuard::noop())
        });

    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    cortex::telemetry::install_subscriber(
        otel_layers,
        env_filter,
        Box::new(tracing_subscriber::fmt::layer()),
    );

    tracing::info!(borg_home = %home.display(), "starting borgd");

    daemon::run(daemon::Config {
        borg_home: home,
        listen_tcp: args.listen,
        core_config,
    })
    .await
}
