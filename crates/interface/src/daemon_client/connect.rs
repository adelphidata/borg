//! Connection helpers for [`DaemonClient`].
//!
//! Two flavors:
//! - [`connect`] — fail fast if the socket is missing. Use in production.
//! - [`connect_or_spawn`] — auto-launch `borgd` and wait for the socket.
//!   Use in dev / single-user setups where the daemon's lifecycle is the
//!   user's CLI session.

use super::DaemonClient;
use anyhow::{anyhow, Context, Result};
use identity::{KekStore, KeychainStore, PkiError};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tokio::net::UnixStream;
use tonic::transport::{Certificate, ClientTlsConfig, Endpoint, Identity, Uri};
use tower::service_fn;

/// Server name validated against the daemon's server cert SAN.
/// `borgd` mints its server cert with `localhost` in the SAN
/// (`crates/borgd/src/daemon.rs:237`); changing one side requires changing the
/// other or the TLS handshake fails with `BadCertificate`.
const TLS_SERVER_NAME: &str = "localhost";

/// Connect to `borgd` over TCP with mutual TLS. Loads the client cert + key +
/// CA PEM stashed by `borg device pair` from the OS keychain (or an injected
/// `KekStore` in tests). Fails fast if any of the three PEMs is missing —
/// presents a clear "run `borg device pair` first" message rather than a
/// generic TLS handshake error.
pub async fn connect_tcp(addr: SocketAddr) -> Result<DaemonClient> {
    connect_tcp_with_kek(addr, &KeychainStore::new()).await
}

/// Like [`connect_tcp`] but with a caller-provided `KekStore` — used by the
/// mTLS handshake e2e suite to inject an in-memory keychain without touching
/// the user's OS keychain.
pub async fn connect_tcp_with_kek<K: KekStore>(addr: SocketAddr, kek: &K) -> Result<DaemonClient> {
    let cert_pem = require_kek_pem(kek, crate::commands::device_pair::CLIENT_CERT_KEY_ACCOUNT)?;
    let key_pem = require_kek_pem(kek, crate::commands::device_pair::CLIENT_KEY_KEY_ACCOUNT)?;
    let ca_pem = require_kek_pem(kek, crate::commands::device_pair::CA_CERT_ACCOUNT)?;

    let identity = Identity::from_pem(cert_pem.as_bytes(), key_pem.as_bytes());
    let ca = Certificate::from_pem(ca_pem.as_bytes());
    let tls = ClientTlsConfig::new()
        .ca_certificate(ca)
        .identity(identity)
        .domain_name(TLS_SERVER_NAME);

    let channel = Endpoint::from_shared(format!("https://{addr}"))
        .context("constructing TCP endpoint")?
        .tls_config(tls)
        .context("install client TLS config")?
        .connect()
        .await
        .with_context(|| format!("mTLS connect to borgd at {addr}"))?;
    Ok(DaemonClient::from_channel(channel))
}

fn require_kek_pem<K: KekStore>(kek: &K, account: &str) -> Result<String> {
    match kek.get(account) {
        Ok(bytes) => String::from_utf8(bytes)
            .with_context(|| format!("keychain account `{account}` is not utf-8 PEM")),
        Err(PkiError::NotFound(_)) => Err(anyhow!(
            "no client cert in keychain (account `{account}` missing) — run `borg device pair` first"
        )),
        Err(e) => Err(anyhow!("load `{account}` from keychain: {e}")),
    }
}

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
    let channel = Endpoint::try_from(protocols::TONIC_UDS_PLACEHOLDER_URL)
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

    #[tokio::test]
    async fn tcp_connect_errors_clearly_when_no_client_cert_in_keychain() {
        // Real failure mode: a fresh install (or wiped keychain) attempting
        // `borg --connect 127.0.0.1:7777` must surface a recoverable error
        // pointing the user at `borg device pair`, not a low-level TLS
        // handshake failure that obscures the missing-cert root cause.
        use identity::MemoryKekStore;
        let kek = MemoryKekStore::new();
        let addr: SocketAddr = "127.0.0.1:65535".parse().unwrap();
        let err = super::connect_tcp_with_kek(addr, &kek)
            .await
            .expect_err("must fail with empty keychain");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("no client cert"),
            "error must signal missing cert; got: {msg}"
        );
        assert!(
            msg.contains("borg device pair"),
            "error must point at the recovery command; got: {msg}"
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
