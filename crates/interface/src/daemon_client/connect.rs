//! Connection helpers for [`DaemonClient`](super::DaemonClient).
//!
//! Two flavors:
//! - [`connect`] — fail fast if the socket is missing. Use in production.
//! - [`connect_or_spawn`] — auto-launch `borgd --insecure-uds` and wait for
//!   the socket. Use in dev / single-user setups where the daemon's lifecycle
//!   is the user's CLI session.

use super::DaemonClient;
use anyhow::{anyhow, Context, Result};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tokio::net::UnixStream;
use tonic::transport::{Endpoint, Uri};
use tower::service_fn;

/// Default UDS path: `$BORG_HOME/borgd.sock`, falling back to
/// `~/.borg/borgd.sock`. Mirrors `borgd::paths::resolve_borg_home`.
pub fn default_socket_path() -> PathBuf {
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
            "no borgd socket at {}. Start the daemon (`borgd --insecure-uds`) or use connect_or_spawn for auto-launch.",
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
    Ok(DaemonClient::from_channel(channel))
}

/// Like [`connect`] but launches `borgd --insecure-uds` if the socket isn't
/// present. The spawned daemon is detached — it lives on after this CLI
/// process exits, so subsequent `borg` invocations reuse it. If you don't
/// want that lifecycle, prefer [`connect`] + an externally supervised daemon.
pub async fn connect_or_spawn() -> Result<DaemonClient> {
    let path = default_socket_path();
    if !path.exists() {
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
        .arg("--insecure-uds")
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
}
