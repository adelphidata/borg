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

    /// (Deprecated) historical no-op. The UDS no longer has an "insecure"
    /// mode — the per-installation CA + paired-device flow is enforced
    /// unconditionally. Accepted silently so existing launchd / systemd
    /// unit files keep starting, but the daemon emits a deprecation
    /// warning. Remove from your unit file at your convenience; will be
    /// hard-rejected after one minor release.
    #[arg(long, hide = true)]
    insecure_uds: bool,

    /// Optional loopback TCP listener address (e.g. `127.0.0.1:8009`). Task
    /// 02 wires mTLS on this listener; clients must hold a paired-device
    /// cert.
    #[arg(long)]
    listen: Option<String>,

    /// (Legacy, deprecated) permit unauthenticated TCP. Ignored — the
    /// TCP listener always requires mTLS.
    #[arg(long)]
    insecure_tcp: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
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

    if args.insecure_tcp {
        tracing::warn!(
            "--insecure-tcp is deprecated: TCP listener now requires mTLS unconditionally"
        );
    }
    if args.insecure_uds {
        tracing::warn!(
            "--insecure-uds is deprecated and a no-op: the UDS no longer has an insecure mode. \
             Remove it from your launchd / systemd unit; the daemon will hard-reject this flag \
             in a future release."
        );
    }

    tracing::info!(borg_home = %home.display(), "starting borgd");

    daemon::run(daemon::Config {
        borg_home: home,
        listen_tcp: args.listen,
        core_config,
    })
    .await
}
