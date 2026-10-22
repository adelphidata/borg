//! `borgd` — the Borg agent daemon.
//!
//! Hosts the agent loop (lifted from the in-process TUI/REPL) and exposes
//! Session, Capability, Status, and Admin gRPC services over a Unix Domain
//! Socket.

// The binary is its own crate, so it needs the lib's limit too: a release
// build lays out the gateway's deep async futures here and exceeds the default.
#![recursion_limit = "256"]

use anyhow::Result;
use borgd::daemon;
use clap::Parser;
use cortex::paths;
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
    borgd::auth::tls::install_crypto_provider();

    // MCP stdio watchdog trampoline (Unix): a re-exec of this binary to
    // supervise a stdio MCP server and reap it if the daemon dies ungracefully
    // (SIGKILL/crash). Handled before clap AND before the tokio runtime is
    // built so the watchdog argv never reaches the CLI, the watchdog never
    // boots the daemon, and a watchdog re-exec doesn't pay to spin up a
    // multi-thread worker pool it would only discard.
    if let Some(code) = mcp::watchdog::dispatch() {
        std::process::exit(code);
    }

    // Raise the soft open-file limit before any descriptor-hungry subsystem
    // starts. Logging is not up yet, so the result is carried into
    // `async_main` and reported once the subscriber is installed. Windows
    // has no such limit, so there is nothing to report there.
    #[cfg(unix)]
    let nofile_result = Some(borgd::runtime::raise_nofile_limit());
    #[cfg(not(unix))]
    let nofile_result: Option<std::io::Result<u64>> = None;

    // The agent loop runs on a worker thread; its deep poll chain (large tool
    // futures like the browser handler + every `#[instrument]` layer) overflows
    // tokio's 2 MiB default worker stack, and a stack overflow `abort()`s the
    // whole daemon uncatchably. `build_runtime` enlarges the worker stack to
    // give that poll chain the headroom it needs.
    borgd::runtime::build_runtime()?.block_on(async_main(nofile_result))
}

/// Load `<home>/.env` into the process environment.
///
/// The cli has always done this, and the installed service units pass the
/// file as `EnvironmentFile`; a daemon spawned by the desktop app got
/// neither, so a key kept in `.env` worked from the terminal and not from
/// the app. Loading it here makes the file one source for every launcher.
/// A missing file is the common case and stays silent.
fn load_env_file(home: &std::path::Path) {
    let path = home.join(".env");
    match dotenvy::from_path(&path) {
        Ok(()) => {}
        Err(dotenvy::Error::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => eprintln!("Warning: could not load {}: {e}", path.display()),
    }
}

async fn async_main(nofile_result: Option<std::io::Result<impl std::fmt::Display>>) -> Result<()> {
    let args = Args::parse();
    let home = paths::resolve_borg_home(args.borg_home.as_deref())?;
    // Point config/DB resolution at the resolved home so telemetry settings
    // load from the same database the daemon will use.
    std::env::set_var("BORG_HOME", &home);
    load_env_file(&home);

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

    // chromiumoxide/tungstenite warn on every CDP event their enums don't model
    // ("WS Invalid message: data did not match any variant"), which is normal
    // Chrome-version drift and not a browser-tool failure. Silenced by default
    // so `daemon.log` stays readable; `RUST_LOG` still overrides.
    let env_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,chromiumoxide=error,tungstenite=error"));
    cortex::telemetry::install_subscriber(
        otel_layers,
        env_filter,
        Box::new(tracing_subscriber::fmt::layer()),
    );

    match nofile_result {
        Some(Ok(limit)) => tracing::debug!(%limit, "open-file soft limit in effect"),
        Some(Err(e)) => {
            tracing::warn!(error = %e, "could not raise the open-file soft limit")
        }
        None => {}
    }

    tracing::info!(borg_home = %home.display(), "starting borgd");

    daemon::run(daemon::Config {
        borg_home: home,
        listen_tcp: args.listen,
        core_config,
    })
    .await
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::load_env_file;

    /// A key kept in `~/.borg/.env` has to reach a daemon the desktop app
    /// spawned, and a home without that file is the ordinary first run.
    #[test]
    fn env_file_is_loaded_when_present_and_ignored_when_not() {
        let home = tempfile::tempdir().expect("tempdir");
        let var = "BORGD_ENV_FILE_TEST_KEY";
        std::env::remove_var(var);

        // No file: nothing set, nothing printed, no panic.
        load_env_file(home.path());
        assert!(std::env::var(var).is_err(), "nothing to load");

        std::fs::write(home.path().join(".env"), format!("{var}=from-dotenv\n")).expect("write");
        load_env_file(home.path());
        let got = std::env::var(var);
        std::env::remove_var(var);
        assert_eq!(got.as_deref(), Ok("from-dotenv"));
    }
}
