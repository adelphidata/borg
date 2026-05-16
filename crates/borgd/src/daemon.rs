//! Daemon lifecycle: lock acquisition, transport setup, graceful shutdown.

use crate::auth::kek_factory::build_kek_store;
use crate::auth::{
    auth_interceptor, build_server_tls_config, build_verifier, derive_server_san, ensure_root_ca,
    ensure_server_cert, ServerCertMaterial,
};
use crate::cost_events::CostEventBus;
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
use cortex::config_watcher::ConfigWatcher;
use cortex::db::{audit_events, AuditWriter, Database};
use cortex::llm::CostEventSink;
use cortex::telemetry::BorgMetrics;
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
struct DaemonSessionFactory {
    config_rx: tokio::sync::watch::Receiver<CoreConfig>,
    cost_event_sink: Arc<dyn CostEventSink>,
    process_registry: cortex::process_registry::ProcessRegistry,
    goal_bus: Option<crate::goal_bus::GoalEventBus>,
}

impl SessionFactory for DaemonSessionFactory {
    fn open(
        &self,
        resume_id: Option<&str>,
        execution_context: Option<cortex::execution_context::ExecutionContext>,
    ) -> Result<SessionHost> {
        let config = self.config_rx.borrow().clone();
        // Inject the daemon-wide subprocess registry into the session's
        // ExecutionContext so every tool handler that spawns a child can
        // register it. Falls through to a registry-less context when the
        // request didn't carry workspace coordinates.
        let execution_context = execution_context
            .map(|ctx| ctx.with_process_registry(Some(self.process_registry.clone())));
        let backend = AgentBackend::new(
            config,
            BorgMetrics::noop(),
            resume_id,
            Some(self.config_rx.clone()),
            Some(self.cost_event_sink.clone()),
            execution_context,
            self.goal_bus.clone(),
        )?;
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

    // Tighten umask to 0o177 around bind so the socket inode is created
    // already-private. Process-global, but borgd is single-instance and
    // this only runs once at startup.
    use nix::sys::stat::{umask, Mode};
    let prev_umask = umask(Mode::from_bits_truncate(0o177));
    let bind_result = UnixListener::bind(path);
    umask(prev_umask);
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

    // Self-heal: make sure ~/.borg subdirs exist so first-start doesn't
    // race fresh-install lazy creation. Idempotent; daily maintenance
    // runs the same step. Failures log+continue.
    if let Err(e) = cortex::maintenance::ensure_data_subdirs(&cfg.borg_home) {
        tracing::warn!("borgd: ensure_data_subdirs failed: {e}");
    }

    let socket_path = paths::socket_path(&cfg.borg_home);
    let uds = bind_uds(&socket_path)?;

    tracing::info!(socket = %socket_path.display(), "borgd listening on UDS");

    let initial_core_config = CoreConfig::load_from_db().unwrap_or_default();
    let config_watcher =
        ConfigWatcher::start(initial_core_config.clone()).context("start config watcher")?;
    let metrics = BorgMetrics::noop();
    let cost_event_bus = CostEventBus::new(&initial_core_config, metrics.clone());
    let cost_event_sink: Arc<dyn CostEventSink> = Arc::new(cost_event_bus.clone());
    {
        let mut rx = config_watcher.subscribe();
        let bus = cost_event_bus.clone();
        tokio::spawn(async move {
            loop {
                if rx.changed().await.is_err() {
                    return;
                }
                bus.sync_from_config(&rx.borrow().clone());
            }
        });
    }

    let registry = SessionRegistry::new();
    // Daemon-wide subprocess registry — one handle, cloned everywhere
    // (`Arc` inside). Threaded into the session factory so tool handlers
    // see it via `ExecutionContext`, and wired into AdminSvc for RPC
    // inspection / pin toggling. Also installed as the process-global
    // handle so the scheduled-task runner's maintenance executor can
    // find it without threading parameters through the dispatcher.
    let process_registry = cortex::process_registry::ProcessRegistry::new();
    cortex::process_registry::install_global(process_registry.clone());
    // Single goal-event bus shared by the orchestrator's session executor,
    // the SessionFactory used for `Session.Open` connections, and the Admin
    // RPC layer — so every mutation path (agent loop, four goal RPCs, the
    // TUI's `/cancel`) publishes onto the same channel the TUI footer
    // subscribes to. Created once here so both `Session.Open`-created
    // backends and orchestrator-spawned backends see the same bus.
    let goal_bus = crate::goal_bus::GoalEventBus::new();
    let factory: Arc<dyn SessionFactory> = Arc::new(DaemonSessionFactory {
        config_rx: config_watcher.subscribe(),
        cost_event_sink: cost_event_sink.clone(),
        process_registry: process_registry.clone(),
        goal_bus: Some(goal_bus.clone()),
    });
    let capability_router = Arc::new(CapabilityRouter::new());

    // Single shutdown signal fanned out to every transport so SIGTERM drains
    // both UDS and TCP listeners in parallel rather than aborting the slower one.
    // Signal-handler spawn is deferred until after the AuditWriter exists so the
    // shutdown path can emit DAEMON_STOP into the same chain that bracketed
    // DAEMON_START.
    let (shutdown_tx, _) = broadcast::channel::<()>(1);

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

    // Migration for pre-existing installs: if `user.timezone` was never set
    // (onboarding ran before host-timezone auto-detection landed), detect it
    // now so heartbeat quiet hours anchor to local time. Idempotent and
    // write-once — explicit user values are preserved.
    if let Ok(db) = Database::open() {
        match db.get_setting("user.timezone") {
            Ok(None) => {
                if let Some(tz) = cortex::config::Config::detect_host_timezone() {
                    if let Err(e) = db.set_setting("user.timezone", &tz) {
                        tracing::warn!("borgd: failed to persist auto-detected timezone: {e}");
                    } else {
                        tracing::info!("borgd: auto-detected user.timezone = {tz}");
                    }
                }
            }
            Ok(Some(_)) => {}
            Err(e) => tracing::warn!("borgd: read user.timezone failed: {e}"),
        }
    }

    let extras = match CoreConfig::load_from_db() {
        Ok(core_config) => {
            let handles = orchestrator::start(
                registry.clone(),
                core_config,
                orchestrator_token.clone(),
                config_watcher.subscribe(),
                cost_event_sink.clone(),
                goal_bus.clone(),
            );
            grpc::AdminExtras {
                heartbeat_bus: Some(handles.heartbeat_bus),
                executor: Some(handles.executor),
                heartbeat_trigger_tx: Some(handles.heartbeat_trigger_tx),
                shutdown_tx: Some(shutdown_tx.clone()),
                config_notify_tx: Some(config_watcher.notify_sender()),
                process_registry: Some(process_registry.clone()),
                goal_bus: Some(handles.goal_bus),
            }
        }
        Err(e) => {
            tracing::warn!(
                "borgd: load_from_db failed ({e}); orchestrator + approval-greeting disabled"
            );
            grpc::AdminExtras {
                heartbeat_bus: None,
                executor: None,
                heartbeat_trigger_tx: None,
                shutdown_tx: Some(shutdown_tx.clone()),
                config_notify_tx: Some(config_watcher.notify_sender()),
                process_registry: Some(process_registry.clone()),
                goal_bus: Some(goal_bus.clone()),
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
    let kek = {
        // Resolve KEK backend before locking pki_db for cert ops. The
        // factory probes the OS keychain on `Auto`, falls back to the
        // passphrase-encrypted file, and persists the resolved choice so
        // posture snapshots see a concrete `kek_backend` value.
        let pki_cfg =
            CoreConfig::load_from_db().context("loading config for KEK backend selection")?;
        let g = pki_db
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        build_kek_store(&cfg.borg_home, &pki_cfg, &g).context("build KEK store")?
    };
    let (ca_cert_pem, ca_key_pem) = {
        let g = pki_db
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        ensure_root_ca(&g, kek.as_ref()).context("ensure root CA")?
    };
    // Derive the server-cert SAN from the bind address. Loopback (or no
    // `--listen`) gives `["localhost"]`; non-loopback requires the operator
    // to set `BORG_DAEMON_TLS_SAN`. This replaces the historical hard-coded
    // `["localhost"]` so binding to `--listen 0.0.0.0:PORT` fails loudly at
    // startup instead of silently minting an unusable cert.
    let san_dns = derive_server_san(cfg.listen_tcp.as_deref()).context("derive server cert SAN")?;
    let (server_cert_pem, server_key_pem, _server_serial) = {
        let g = pki_db
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        ensure_server_cert(&g, kek.as_ref(), &ca_cert_pem, &ca_key_pem, &san_dns)
            .context("ensure server cert")?
    };
    let server_tls_material = ServerCertMaterial {
        server_cert_pem: server_cert_pem.clone(),
        server_key_pem: server_key_pem.clone(),
        ca_cert_pem: ca_cert_pem.clone(),
    };
    let audit = AuditWriter::new(pki_db.clone());
    // Install the process-global audit writer so library call sites that
    // can't easily plumb an `AuditWriter` (shell-policy denial path,
    // settings resolver, …) can emit through the same chain. This must
    // happen before DAEMON_START so the install order is observable in
    // tests if it ever regresses.
    cortex::db::install_global_audit(audit.clone());

    // Install the process-global output-redaction policy. This is what
    // strips detected secrets from agent output before it reaches a
    // channel, the TUI transcript, or `~/.borg/logs/`. Gated by the
    // `security.output_redaction` setting (default true, security-first).
    // A spawned watcher reinstalls the policy whenever the setting flips
    // so /settings → toggle takes effect on the next message without a
    // daemon restart.
    {
        let core_cfg = CoreConfig::load_from_db().unwrap_or_default();
        cortex::output_redaction::install_global(
            cortex::output_redaction::RedactionPolicy::from_db(
                core_cfg.security.output_redaction,
                pki_db.clone(),
            ),
        );
        let mut rx = config_watcher.subscribe();
        let db_for_policy = pki_db.clone();
        tokio::spawn(async move {
            let mut current = core_cfg.security.output_redaction;
            loop {
                if rx.changed().await.is_err() {
                    return;
                }
                let next = rx.borrow().security.output_redaction;
                if next != current {
                    cortex::output_redaction::install_global(
                        cortex::output_redaction::RedactionPolicy::from_db(
                            next,
                            db_for_policy.clone(),
                        ),
                    );
                    current = next;
                }
            }
        });
    }

    audit.emit(
        audit_events::DAEMON_START,
        "system",
        &json!({"pid": std::process::id()}),
    );

    // Wire shutdown-signal forwarding now that the audit writer is available.
    // `forward_signals_to` blocks on the first signal, emits DAEMON_STOP, and
    // then broadcasts the shutdown so listeners drain.
    tokio::spawn(forward_signals_to(shutdown_tx.clone(), audit.clone()));

    // Reconcile security posture against live config before serving any
    // traffic. On a fresh DB this writes the bootstrap event;
    // on a restart with no config change it's a no-op. Failures are
    // logged and do NOT block daemon startup — XP writes will fall back
    // to `Default × 1.00` with `FallbackReason::PostureUndetermined`.
    {
        let g = pki_db
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let core_cfg = CoreConfig::load_from_db().unwrap_or_default();
        match cortex::evolution::security::maybe_emit_posture_change(
            &g,
            &core_cfg,
            cortex::evolution::security::PostureSource::DaemonStartReconcile,
        ) {
            Ok(cortex::evolution::security::EmitOutcome::Emitted {
                state, previous, ..
            }) => {
                // Transition is logged at INFO inside cortex::evolution::security::emit;
                // logging it again here used to produce duplicate "posture …" lines
                // back-to-back in daemon.log. Keep the broadcast; drop the dupe log.
                let text = format_posture_transition(previous, state);
                registry.broadcast_system_notice("posture", &text);
            }
            Ok(_) => {} // Unchanged or RateLimited — quiet success.
            Err(e) => tracing::warn!("posture reconcile on startup failed: {e}"),
        }
    }

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

    let services = grpc::build_services_with_extras(
        registry,
        factory,
        capability_router,
        cost_event_bus.clone(),
        extras,
    );

    let uds_stream = UnixListenerStream::new(uds);

    // UDS router: filesystem permissions (0600) provide isolation; no TLS,
    // no auth interceptor. CLI on the same machine connects here.
    //
    // INVARIANT: every service registered here MUST also be registered on the
    // TCP router below — adding a service to only one path silently exposes
    // (or hides) it depending on which transport is active.
    let make_uds_router = || {
        tonic::transport::Server::builder()
            .add_service(services.status.clone())
            .add_service(services.admin.clone())
            .add_service(services.session.clone())
            .add_service(services.capability.clone())
            .add_service(services.telemetry.clone())
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

        // Resolve verifier kind from config; fails-closed on misconfigured
        // Tailscale per build_verifier's contract. A DB load failure here
        // would silently downgrade `security.verifier` to its default, so
        // surface it instead.
        let verifier_kind = CoreConfig::load_from_db()
            .context("load config for TCP listener verifier")?
            .security
            .verifier;
        let verifier = build_verifier(verifier_kind, pki_db.clone(), None)
            .context("build verifier for TCP listener")?;
        let tls_cfg = build_server_tls_config(&server_tls_material)
            .context("build server TLS config for TCP listener")?;

        let interceptor = auth_interceptor(verifier, audit.clone());
        let tcp_pki = PkiServer::new(PkiSvc::new(
            pki_db.clone(),
            audit.clone(),
            ca_cert_pem.clone(),
            ca_key_pem.clone(),
        ));
        use tonic::service::interceptor::InterceptedService;
        let tcp_router = tonic::transport::Server::builder()
            .tls_config(tls_cfg)
            .context("install TLS config on TCP server")?
            .add_service(InterceptedService::new(
                services.status.clone(),
                interceptor.clone(),
            ))
            .add_service(InterceptedService::new(
                services.admin.clone(),
                interceptor.clone(),
            ))
            .add_service(InterceptedService::new(
                services.session.clone(),
                interceptor.clone(),
            ))
            .add_service(InterceptedService::new(
                services.capability.clone(),
                interceptor.clone(),
            ))
            .add_service(InterceptedService::new(
                services.telemetry.clone(),
                interceptor.clone(),
            ))
            .add_service(InterceptedService::new(tcp_pki, interceptor));

        tracing::info!(addr = %parsed, "TCP listener with mTLS enabled");

        let mut uds_rx = shutdown_tx.subscribe();
        let mut tcp_rx = shutdown_tx.subscribe();
        let serve_uds = make_uds_router().serve_with_incoming_shutdown(uds_stream, async move {
            let _ = uds_rx.recv().await;
        });
        let serve_tcp = tcp_router.serve_with_shutdown(parsed, async move {
            let _ = tcp_rx.recv().await;
        });

        let (uds_res, tcp_res) = tokio::join!(serve_uds, serve_tcp);
        uds_res.context("UDS server")?;
        tcp_res.context("TCP server")?;
    } else {
        let mut rx = shutdown_tx.subscribe();
        make_uds_router()
            .serve_with_incoming_shutdown(uds_stream, async move {
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
///
/// On signal: emits a `DAEMON_STOP` audit event with the triggering signal's
/// name BEFORE broadcasting shutdown, so the row lands while the audit writer
/// is still alive and the chain bookends `DAEMON_START` on every clean exit.
async fn forward_signals_to(tx: broadcast::Sender<()>, audit: AuditWriter) {
    let reason = wait_for_shutdown_signal().await;
    emit_daemon_stop(&audit, reason);
    // If all receivers were dropped (shutdown already in progress) the send
    // fails harmlessly — there's no one left to notify.
    let _ = tx.send(());
}

/// Format a posture transition for a `system_notice` toast. Pulled out so
/// runtime triggers (config change / listener bind) can reuse the exact
/// wording — the daemon-startup site is the only caller today.
pub(crate) fn format_posture_transition(
    previous: Option<cortex::evolution::security::PostureState>,
    next: cortex::evolution::security::PostureState,
) -> String {
    let prev_label = previous.map(|p| p.as_str()).unwrap_or("—");
    format!(
        "Posture: {prev_label} → {next_label} (×{mult:.2})",
        next_label = next.as_str(),
        mult = next.xp_multiplier()
    )
}

/// Block until SIGTERM or SIGINT fires; return a stable string identifying
/// which signal triggered the shutdown so it can be recorded in the audit log.
async fn wait_for_shutdown_signal() -> &'static str {
    let term = signal(SignalKind::terminate())
        .map_err(|e| tracing::warn!(error = %e, "failed to install SIGTERM handler"))
        .ok();
    let int = signal(SignalKind::interrupt())
        .map_err(|e| tracing::warn!(error = %e, "failed to install SIGINT handler"))
        .ok();

    match (term, int) {
        (Some(mut t), Some(mut i)) => tokio::select! {
            _ = t.recv() => { tracing::info!("received SIGTERM, draining"); "sigterm" }
            _ = i.recv() => { tracing::info!("received SIGINT, draining"); "sigint" }
        },
        (Some(mut t), None) => {
            let _ = t.recv().await;
            tracing::info!("received SIGTERM, draining");
            "sigterm"
        }
        (None, Some(mut i)) => {
            let _ = i.recv().await;
            tracing::info!("received SIGINT, draining");
            "sigint"
        }
        (None, None) => {
            tracing::error!("no signal handlers installed; daemon will only stop on SIGKILL");
            std::future::pending::<&'static str>().await
        }
    }
}

/// Emit the `DAEMON_STOP` audit event. Pulled into its own function so the
/// shutdown-side audit invariant is unit-testable without faking signals.
pub(crate) fn emit_daemon_stop(audit: &AuditWriter, reason: &str) {
    audit.emit(
        audit_events::DAEMON_STOP,
        "system",
        &json!({"reason": reason, "pid": std::process::id()}),
    );
}

#[cfg(test)]
mod tests {
    use super::{bind_uds, emit_daemon_stop, format_posture_transition};
    use cortex::db::{audit_events, AuditWriter, Database};
    use cortex::evolution::security::PostureState;
    use rusqlite::Connection;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::{Arc, Mutex};

    #[test]
    fn emit_daemon_stop_appends_event_with_reason_to_audit_chain() {
        // Real failure mode: graceful shutdowns that don't bookend the
        // DAEMON_START row leave the audit chain ambiguous about whether the
        // daemon exited cleanly or was killed mid-write. Spec §"Hash-chained
        // audit log" mandates daemon start/stop events on by default.
        let db =
            Database::from_connection(Connection::open_in_memory().expect("db")).expect("init");
        let writer = AuditWriter::new(Arc::new(Mutex::new(db)));
        // Seed a DAEMON_START so we can also confirm chain integrity post-stop.
        writer.emit(
            audit_events::DAEMON_START,
            "system",
            &serde_json::json!({"pid": 1}),
        );

        emit_daemon_stop(&writer, "sigterm");

        let outcome = writer.verify();
        assert!(
            matches!(outcome, cortex::db::AuditChainOutcome::Ok),
            "chain should still verify after stop, got {outcome:?}"
        );
        let dbref = writer.db();
        let g = dbref.lock().expect("lock");
        let events = g.list_audit_events().expect("list");
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].event_type, audit_events::DAEMON_START);
        assert_eq!(events[1].event_type, audit_events::DAEMON_STOP);
        let payload: serde_json::Value =
            serde_json::from_str(&events[1].event_json).expect("payload json");
        assert_eq!(payload["reason"], "sigterm");
    }

    #[test]
    fn posture_transition_toast_text_includes_arrow_and_multiplier() {
        // Real failure mode: copy drift between the toast and the
        // `posture_event_seq` it cross-references. The text must name the
        // landing state and its multiplier so users can correlate the
        // change with subsequent XP rates.
        let s = format_posture_transition(Some(PostureState::Default), PostureState::Boosted);
        assert!(s.contains("Default"));
        assert!(s.contains("Boosted"));
        assert!(s.contains("\u{2192}"));
        assert!(s.contains("\u{00d7}1.10"));

        let bootstrap = format_posture_transition(None, PostureState::Default);
        assert!(bootstrap.contains("\u{2014}"), "no prior → em-dash");
        assert!(bootstrap.contains("Default"));
    }

    #[tokio::test]
    async fn bind_uds_sets_owner_only_permissions() {
        // Real failure mode: regression to default umask (0o755 effective)
        // would let any local user connect to the agent's RPC channel. The
        // socket must be 0600 — owner-only.
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
