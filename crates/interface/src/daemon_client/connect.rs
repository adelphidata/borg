//! Connection helpers for [`DaemonClient`].
//!
//! Two flavors:
//! - [`connect`] — fail fast if the socket is missing. Use in production.
//! - [`connect_or_spawn`] — auto-launch `borgd` and wait for the socket.
//!   Use in dev / single-user setups where the daemon's lifecycle is the
//!   user's CLI session.

use super::DaemonClient;
use anyhow::{anyhow, Context, Result};
use identity::KeychainStore;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tokio::net::UnixStream;
use tonic::transport::{Endpoint, Uri};
use tower::service_fn;

/// Default UDS path resolution:
///
/// 1. `$BORG_DAEMON_UDS` if set — explicit override, used by the ACP
///    integration test harness to pin a subprocess at a per-test socket
///    without auto-spawning a real daemon.
/// 2. `$BORG_HOME/borgd.sock` if `$BORG_HOME` is set.
/// 3. `~/.borg/borgd.sock` otherwise.
///
/// Mirrors `borgd::paths::resolve_borg_home` for cases 2 and 3.
pub fn default_socket_path() -> PathBuf {
    if let Ok(p) = std::env::var("BORG_DAEMON_UDS") {
        return PathBuf::from(p);
    }
    let home = std::env::var("BORG_HOME")
        .ok()
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".borg")))
        .unwrap_or_else(|| PathBuf::from(".borg"));
    home.join("borgd.sock")
}

/// Connect over `socket_path` (defaults to [`default_socket_path`] if `None`).
/// Returns a clean error if the socket doesn't exist or isn't connectable.
pub async fn connect(socket_path: Option<&Path>) -> Result<DaemonClient> {
    let owned;
    let path = match socket_path {
        Some(p) => p,
        None => {
            owned = default_socket_path();
            owned.as_path()
        }
    };
    if !path.exists() {
        return Err(anyhow!(
            "no borgd socket at {}. Start the daemon (`borgd`) or use connect_or_spawn for auto-launch.",
            path.display()
        ));
    }
    let path_owned = path.to_path_buf();
    let channel = Endpoint::try_from("http://[::]:50051")
        .context("constructing endpoint")?
        .connect_with_connector(service_fn(move |_: Uri| {
            let p = path_owned.clone();
            async move {
                let stream = UnixStream::connect(&p).await?;
                Ok::<_, std::io::Error>(hyper_util::rt::TokioIo::new(stream))
            }
        }))
        .await
        .with_context(|| format!("connecting to UDS {}", path.display()))?;
    let mut client = DaemonClient::from_channel(channel);
    // Best-effort silent cert renewal: if the daemon says we're inside the
    // 14-day window, rotate now. Failures here MUST NOT block the connect —
    // unauthenticated connections (bootstrap UDS, tests) intentionally fall
    // through. Real failures are surfaced via tracing::warn! per the
    // CLAUDE.md "log + continue" rule for fire-and-forget paths.
    let kek = KeychainStore::new();
    match client.ensure_cert_fresh(&kek).await {
        Ok(true) => tracing::info!("client cert silently renewed"),
        Ok(false) => {}
        Err(e) => tracing::warn!(error = %e, "silent cert renewal failed"),
    }
    Ok(client)
}

/// Like [`connect`] but launches `borgd` if the socket isn't present. The spawned daemon is detached — it lives on after this CLI
/// process exits, so subsequent `borg` invocations reuse it. If you don't
/// want that lifecycle, prefer [`connect`] + an externally supervised daemon.
///
/// Setting `BORG_ACP_NO_AUTOSPAWN=1` (or any non-empty value) disables the
/// auto-spawn — used by the ACP integration test harness, which pins the
/// subprocess to a test-managed UDS via `BORG_DAEMON_UDS` and must not have
/// the subprocess race-spawn a second real daemon if the test daemon is
/// slow to come up. With auto-spawn disabled and the socket missing,
/// returns the same clean error as [`connect`].
pub async fn connect_or_spawn() -> Result<DaemonClient> {
    let path = default_socket_path();
    let no_autospawn = std::env::var("BORG_ACP_NO_AUTOSPAWN")
        .map(|v| !v.is_empty())
        .unwrap_or(false);
    if !path.exists() && !no_autospawn {
        spawn_detached_daemon()?;
        wait_for_socket(&path, Duration::from_secs(5))
            .await
            .context("waiting for borgd to come up after auto-spawn")?;
    }
    connect(Some(&path)).await
}

fn spawn_detached_daemon() -> Result<()> {
    use std::process::{Command, Stdio};
    // Find `borgd` next to the current binary so dev installs (cargo run -p
    // borg) and release installs both work without PATH gymnastics.
    let exe = std::env::current_exe().context("locating current exe")?;
    let mut borgd = exe.with_file_name("borgd");
    if !borgd.exists() {
        // Fallback: trust PATH.
        borgd = PathBuf::from("borgd");
    }
    Command::new(&borgd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("spawning {}", borgd.display()))?;
    Ok(())
}

async fn wait_for_socket(path: &Path, timeout: Duration) -> Result<()> {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if path.exists() && UnixStream::connect(path).await.is_ok() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Err(anyhow!(
        "timed out after {:?} waiting for borgd UDS at {}",
        timeout,
        path.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn connect_returns_clean_error_when_socket_missing() {
        // Real failure mode: a regression to a generic "connection refused"
        // error would obscure that the daemon simply isn't running. The
        // message should name the socket path so operators can recover.
        let tmp = tempfile::tempdir().expect("tempdir");
        let missing = tmp.path().join("nope.sock");
        let err = connect(Some(&missing)).await.expect_err("must fail");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("no borgd socket"),
            "error must say no socket; got: {msg}"
        );
        assert!(
            msg.contains("nope.sock"),
            "error must name the path; got: {msg}"
        );
    }

    #[test]
    fn default_socket_path_honors_borg_daemon_uds_override() {
        // Real failure mode: the ACP test harness pins the subprocess to a
        // per-test UDS via BORG_DAEMON_UDS. If this env knob silently
        // ignored, the subprocess would either auto-spawn a real daemon or
        // hit the user's `~/.borg/borgd.sock` and corrupt state.
        //
        // Mutating env mid-test is racy with parallel tests; serialize via
        // a static mutex so this case is deterministic regardless of
        // concurrent connect_returns_clean_error_when_socket_missing runs.
        use std::sync::Mutex;
        static ENV_LOCK: Mutex<()> = Mutex::new(());
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        let prior_uds = std::env::var_os("BORG_DAEMON_UDS");
        let prior_home = std::env::var_os("BORG_HOME");
        std::env::set_var("BORG_DAEMON_UDS", "/tmp/explicit.sock");
        let resolved = default_socket_path();
        match prior_uds {
            Some(v) => std::env::set_var("BORG_DAEMON_UDS", v),
            None => std::env::remove_var("BORG_DAEMON_UDS"),
        }
        if let Some(v) = prior_home {
            std::env::set_var("BORG_HOME", v);
        }
        assert_eq!(resolved, PathBuf::from("/tmp/explicit.sock"));
    }
}
