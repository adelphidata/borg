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

    /// (Legacy) permit unauthenticated UDS connections. Task 02 now ships
    /// the per-installation CA + paired-device flow, so the UDS no longer
    /// requires this gate. Flag retained for backwards-compat invocation.
    #[arg(long)]
    insecure_uds: bool,

    /// Optional loopback TCP listener address (e.g. `127.0.0.1:8009`). Task
    /// 02 wires mTLS on this listener; clients must hold a paired-device
    /// cert.
    #[arg(long)]
    listen: Option<String>,

    /// (Legacy, deprecated) permit unauthenticated TCP. Ignored — Task 02
    /// always requires mTLS on the TCP listener.
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
    if args.insecure_tcp {
        tracing::warn!(
            "--insecure-tcp is deprecated: TCP listener now requires mTLS unconditionally"
        );
    }

    let home = paths::resolve_borg_home(args.borg_home.as_deref())?;
    tracing::info!(borg_home = %home.display(), "starting borgd");

    daemon::run(daemon::Config {
        borg_home: home,
        listen_tcp: args.listen,
    })
    .await
}
