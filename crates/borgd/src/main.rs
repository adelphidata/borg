//! `borgd` — the Borg agent daemon.
//!
//! v1 entrypoint per `docs/redesign/tasks/01-daemon-grpc-kernel.md`. Hosts the
//! agent loop (lifted from the in-process TUI/REPL) and exposes Session,
//! Capability, Status, and Admin gRPC services over a Unix Domain Socket.

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

    /// Permit unauthenticated connections over the UDS. Required in v1
    /// until Task 02 wires up mTLS. Without this flag, the daemon refuses
    /// to start in non-dev builds.
    #[arg(long)]
    insecure_uds: bool,

    /// Optional loopback TCP listener address (e.g. `127.0.0.1:8009`).
    /// Requires `--insecure-tcp` until Task 02 lands mTLS.
    #[arg(long)]
    listen: Option<String>,

    /// Permit unauthenticated TCP connections (paired with `--listen`).
    #[arg(long)]
    insecure_tcp: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();

    if !args.insecure_uds {
        anyhow::bail!("refusing to start without --insecure-uds (mTLS auth lands in Task 02)");
    }
    if args.listen.is_some() && !args.insecure_tcp {
        anyhow::bail!("--listen requires --insecure-tcp until mTLS lands in Task 02");
    }

    let home = paths::resolve_borg_home(args.borg_home.as_deref())?;
    tracing::info!(borg_home = %home.display(), "starting borgd");

    daemon::run(daemon::Config {
        borg_home: home,
        listen_tcp: args.listen,
    })
    .await
}
