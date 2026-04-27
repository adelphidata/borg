//! Daemon lifecycle: lock acquisition, transport setup, graceful shutdown.

use crate::auth::{ensure_root_ca, ensure_server_cert, ServerCertMaterial};
use crate::grpc;
use crate::grpc::capability::CapabilityRouter;
use crate::grpc::session::SessionFactory;
use crate::orchestrator;
use crate::pairing::device::{BootstrapListener, BootstrapState};
use crate::paths;
use crate::pidlock::PidLock;
use crate::pki_grpc::PkiSvc;
use crate::session::backend::AgentBackend;
use crate::session::{SessionHost, SessionRegistry};
use anyhow::{Context, Result};
use cortex::config::Config as CoreConfig;
use cortex::db::{audit_events, AuditWriter, Database};
use cortex::telemetry::BorgMetrics;
use identity::KeychainStore;
use protocols::pki::pki_server::PkiServer;
use serde_json::json;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::net::UnixListener;
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::broadcast;
use tokio_stream::wrappers::UnixListenerStream;

/// Production session factory: builds an `AgentBackend` for each session.
struct DaemonSessionFactory;

impl SessionFactory for DaemonSessionFactory {
    fn open(&self, resume_id: Option<&str>) -> Result<SessionHost> {
        let config =
            CoreConfig::load_from_db().context("loading config from db for new session")?;
        let backend = AgentBackend::new(config, BorgMetrics::noop(), resume_id)?;
        Ok(SessionHost::new(backend))
    }
}

/// Runtime configuration assembled from CLI args.
pub struct Config {
    /// Resolved `$BORG_HOME`.
    pub borg_home: PathBuf,
    /// Optional loopback TCP listen address (e.g. `127.0.0.1:8009`).
    pub listen_tcp: Option<String>,
}

/// Bind a Unix Domain Socket at `path` with owner-only (mode 0600) permissions.
///
/// Closes the bind→chmod race by tightening the umask to 0o177 around the
/// `bind()` call so the socket inode is created already-private; the explicit
/// `chmod` afterwards is belt-and-braces against umask interactions on weird
/// filesystems.
pub fn bind_uds(path: &Path) -> Result<UnixListener> {
    if path.exists() {
        std::fs::remove_file(path)
            .with_context(|| format!("removing stale socket at {}", path.display()))?;
    }

    // SAFETY: setting the process umask is inherently global; we restore it
    // immediately after the bind. Acceptable because borgd is single-instance
    // and this only runs once per process at startup.
    let prev_umask = unsafe { libc::umask(0o177) };
    let bind_result = UnixListener::bind(path);
    unsafe { libc::umask(prev_umask) };
    let listener = bind_result.with_context(|| format!("binding UDS at {}", path.display()))?;

    let perms = std::fs::Permissions::from_mode(0o600);
    std::fs::set_permissions(path, perms)
        .with_context(|| format!("setting 0600 on {}", path.display()))?;

    Ok(listener)
}

/// Run the daemon to completion. Returns when SIGTERM/SIGINT is received or a
/// fatal error occurs.
pub async fn run(cfg: Config) -> Result<()> {
    let pid_path = paths::pid_path(&cfg.borg_home);
    let _lock = PidLock::acquire(&pid_path)
        .with_context(|| format!("acquiring single-instance lock at {}", pid_path.display()))?;

    let socket_path = paths::socket_path(&cfg.borg_home);
    let uds = bind_uds(&socket_path)?;

    tracing::info!(socket = %socket_path.display(), "borgd listening on UDS");

    let registry = SessionRegistry::new();
    let factory: Arc<dyn SessionFactory> = Arc::new(DaemonSessionFactory);
    let capability_router = Arc::new(CapabilityRouter::new());

    // Single shutdown signal fanned out to every transport so SIGTERM drains
    // both UDS and TCP listeners in parallel rather than aborting the slower one.
    let (shutdown_tx, _) = broadcast::channel::<()>(1);
    tokio::spawn(forward_signals_to(shutdown_tx.clone()));

    // Bridge the broadcast shutdown into a CancellationToken for the
    // orchestrator subsystems (gateway, heartbeat scheduler, task dispatcher).
    let orchestrator_token = tokio_util::sync::CancellationToken::new();
    {
        let token = orchestrator_token.clone();
        let mut sub = shutdown_tx.subscribe();
        tokio::spawn(async move {
            let _ = sub.recv().await;
            token.cancel();
        });
    }

    // Hourly budget reset tick — advances any budget whose period expired
    // and writes a `budget_reset` event. Failures stay isolated in the
    // task; daemon health is unaffected.
    crate::budget_tick::spawn(shutdown_tx.subscribe());

    let extras = match CoreConfig::load_from_db() {
        Ok(core_config) => {
            let handles =
                orchestrator::start(registry.clone(), core_config, orchestrator_token.clone());
            grpc::AdminExtras {
                heartbeat_bus: Some(handles.heartbeat_bus),
                executor: Some(handles.executor),
                poke_tx: Some(handles.poke_tx),
                shutdown_tx: Some(shutdown_tx.clone()),
            }
        }
        Err(e) => {
            tracing::warn!(
                "borgd: load_from_db failed ({e}); orchestrator + approval-greeting disabled"
            );
            grpc::AdminExtras {
                heartbeat_bus: None,
                executor: None,
                poke_tx: None,
                shutdown_tx: Some(shutdown_tx.clone()),
            }
        }
    };
    // ── PKI bootstrap ──
    //
    // Mint root CA + server cert on first run; load from DB+keychain on
    // subsequent starts. We open a daemon-owned `Database` here and wrap it
    // in `Arc<Mutex>` so the AuditWriter and Pki service share the single
    // writer with the audit chain.
    let db_for_pki = Database::open().context("opening DB for PKI bootstrap")?;
    let pki_db = Arc::new(Mutex::new(db_for_pki));
    let kek = Arc::new(KeychainStore::new());
    let (ca_cert_pem, ca_key_pem) = {
        let g = pki_db.lock().unwrap_or_else(|p| p.into_inner());
        ensure_root_ca(&g, kek.as_ref()).context("ensure root CA")?
    };
    let san_dns = vec!["localhost".to_string()];
    let (server_cert_pem, server_key_pem, _server_serial) = {
        let g = pki_db.lock().unwrap_or_else(|p| p.into_inner());
        ensure_server_cert(&g, kek.as_ref(), &ca_cert_pem, &ca_key_pem, &san_dns)
            .context("ensure server cert")?
    };
    let _server_tls_material = ServerCertMaterial {
        server_cert_pem: server_cert_pem.clone(),
        server_key_pem: server_key_pem.clone(),
        ca_cert_pem: ca_cert_pem.clone(),
    };
    let audit = AuditWriter::new(pki_db.clone());
    audit.emit(
        audit_events::DAEMON_START,
        "system",
        &json!({"pid": std::process::id()}),
    );

    // Bootstrap pairing socket (plaintext, on its own UDS path). Always-on
    // — code-validity gating happens inside the handshake (consume_pairing_code
    // rejects unknown/expired/used codes). Cheaper than gating socket bind on
    // active code count and no functional difference.
    let pair_sock = paths::pairing_socket_path(&cfg.borg_home);
    let bootstrap_state = BootstrapState {
        db: pki_db.clone(),
        audit: audit.clone(),
        ca_cert_pem: ca_cert_pem.clone(),
        ca_key_pem: ca_key_pem.clone(),
    };
    {
        let listener = BootstrapListener::new(pair_sock.clone());
        tokio::spawn(async move {
            if let Err(e) = listener.serve(bootstrap_state).await {
                tracing::warn!(error = %e, "pairing bootstrap listener exited");
            }
        });
    }

    let services = grpc::build_services_with_extras(registry, factory, capability_router, extras);

    let uds_stream = UnixListenerStream::new(uds);

    let make_router = || {
        tonic::transport::Server::builder()
            .add_service(services.status.clone())
            .add_service(services.admin.clone())
            .add_service(services.session.clone())
            .add_service(services.capability.clone())
            .add_service(PkiServer::new(PkiSvc::new(
                pki_db.clone(),
                audit.clone(),
                ca_cert_pem.clone(),
                ca_key_pem.clone(),
            )))
    };

    if let Some(addr) = cfg.listen_tcp {
        let parsed: std::net::SocketAddr = addr
            .parse()
            .with_context(|| format!("parsing --listen address `{addr}`"))?;
        tracing::warn!(addr = %parsed, "loopback TCP listener enabled (mTLS not yet wired on this listener)");

        let mut uds_rx = shutdown_tx.subscribe();
        let mut tcp_rx = shutdown_tx.subscribe();
        let serve_uds = make_router().serve_with_incoming_shutdown(uds_stream, async move {
            // Lagged/Closed both mean "drain now" — only one send(()) ever fires.
            let _ = uds_rx.recv().await;
        });
        let serve_tcp = make_router().serve_with_shutdown(parsed, async move {
            let _ = tcp_rx.recv().await;
        });

        let (uds_res, tcp_res) = tokio::join!(serve_uds, serve_tcp);
        uds_res.context("UDS server")?;
        tcp_res.context("TCP server")?;
    } else {
        let mut rx = shutdown_tx.subscribe();
        make_router()
            .serve_with_incoming_shutdown(uds_stream, async move {
                // Lagged/Closed both mean "drain now" — only one send(()) ever fires.
                let _ = rx.recv().await;
            })
            .await
            .context("UDS server")?;
    }

    tracing::info!("borgd shutdown complete");
    Ok(())
}

/// Install SIGTERM and SIGINT handlers; broadcast on either. If a handler
/// can't be installed we log and continue with whichever one we got — the
/// daemon is never left un-killable.
async fn forward_signals_to(tx: broadcast::Sender<()>) {
    let term = signal(SignalKind::terminate())
        .map_err(|e| tracing::warn!(error = %e, "failed to install SIGTERM handler"))
        .ok();
    let int = signal(SignalKind::interrupt())
        .map_err(|e| tracing::warn!(error = %e, "failed to install SIGINT handler"))
        .ok();

    match (term, int) {
        (Some(mut t), Some(mut i)) => tokio::select! {
            _ = t.recv() => tracing::info!("received SIGTERM, draining"),
            _ = i.recv() => tracing::info!("received SIGINT, draining"),
        },
        (Some(mut t), None) => {
            let _ = t.recv().await;
            tracing::info!("received SIGTERM, draining");
        }
        (None, Some(mut i)) => {
            let _ = i.recv().await;
            tracing::info!("received SIGINT, draining");
        }
        (None, None) => {
            tracing::error!("no signal handlers installed; daemon will only stop on SIGKILL");
            std::future::pending::<()>().await;
        }
    }

    // If all receivers were dropped (shutdown already in progress) the send
    // fails harmlessly — there's no one left to notify.
    let _ = tx.send(());
}

#[cfg(test)]
mod tests {
    use super::bind_uds;
    use std::os::unix::fs::PermissionsExt;

    #[tokio::test]
    async fn bind_uds_sets_owner_only_permissions() {
        // Real failure mode: regression to default umask (0o755 effective)
        // would let any local user connect to the agent's RPC channel before
        // mTLS lands. Architecture-redesign §Layer 4a mandates 0600.
        let tmp = tempfile::tempdir().expect("tempdir");
        let socket = tmp.path().join("borgd.sock");
        let _listener = bind_uds(&socket).expect("bind");
        let mode = std::fs::metadata(&socket)
            .expect("stat")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "got mode {mode:o}");
    }

    #[tokio::test]
    async fn bind_uds_replaces_stale_socket_file() {
        // Real failure mode: a prior daemon crashed without cleanup; the
        // socket inode still exists and `bind()` would EADDRINUSE without
        // the unlink-first step.
        let tmp = tempfile::tempdir().expect("tempdir");
        let socket = tmp.path().join("borgd.sock");
        // Drop a stale file at the path.
        std::fs::write(&socket, b"stale").expect("seed stale");
        let _listener = bind_uds(&socket).expect("bind despite stale file");
        // After bind, the path is a socket, not a regular file.
        let meta = std::fs::metadata(&socket).expect("stat");
        assert!(
            !meta.is_file(),
            "stale regular file should have been replaced by a socket inode"
        );
    }
}
