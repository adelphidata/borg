//! Daemon lifecycle: lock acquisition, transport setup, graceful shutdown.

use crate::auth::kek_factory::build_kek_store;
use crate::auth::{
    auth_interceptor, build_server_tls_config, build_verifier, ensure_root_ca, ensure_server_cert,
    san::derive_server_san_with_mdns, ServerCertMaterial,
};
use crate::cost_events::CostEventBus;
use crate::discovery::mdns::{default_instance_name, MdnsAnnouncer};
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
    metrics: BorgMetrics,
}

impl SessionFactory for DaemonSessionFactory {
    fn open<'a>(
        &'a self,
        resume_id: Option<&'a str>,
        execution_context: Option<cortex::execution_context::ExecutionContext>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<SessionHost>> + Send + 'a>> {
        Box::pin(async move {
            let config = self.config_rx.borrow().clone();
            // Inject the daemon-wide subprocess registry into the session's
            // ExecutionContext so every tool handler that spawns a child can
            // register it. Falls through to a registry-less context when the
            // request didn't carry workspace coordinates.
            let execution_context = execution_context
                .map(|ctx| ctx.with_process_registry(Some(self.process_registry.clone())));
            let backend = AgentBackend::new(
                config,
                self.metrics.clone(),
                resume_id,
                Some(self.config_rx.clone()),
                Some(self.cost_event_sink.clone()),
                execution_context,
                self.goal_bus.clone(),
            )
            .await?;
            Ok(SessionHost::new(backend))
        })
    }
}

/// Runtime configuration assembled from CLI args.
pub struct Config {
    /// Resolved `$BORG_HOME`.
    pub borg_home: PathBuf,
    /// Optional loopback TCP listen address (e.g. `127.0.0.1:8009`).
    pub listen_tcp: Option<String>,
    /// Config loaded once in `main` (for telemetry init) and threaded here to
    /// avoid a second DB open + decrypt at the start of `run`.
    pub core_config: CoreConfig,
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

    // Self-heal: make sure ~/.borg subdirs exist so first-start doesn't
    // race fresh-install lazy creation. Idempotent; daily maintenance
    // runs the same step. Failures log+continue.
    if let Err(e) = cortex::maintenance::ensure_data_subdirs(&cfg.borg_home) {
        tracing::warn!("borgd: ensure_data_subdirs failed: {e}");
    }

    let socket_path = paths::socket_path(&cfg.borg_home);
    let uds = bind_uds(&socket_path)?;

    tracing::info!(socket = %socket_path.display(), "borgd listening on UDS");

    // Loaded once in `main` and threaded through `cfg` — no second DB decrypt.
    let initial_core_config = cfg.core_config.clone();

    // Startup security posture audit — advisory warn-on-load (running as root,
    // weak SSH config, world-writable data dir). Never blocks boot; gated by
    // `security.startup_audit`. The on-demand `borg doctor` host audit remains
    // the deep check.
    if initial_core_config.security.startup_audit {
        cortex::security::startup_audit::log_startup_posture(&cfg.borg_home);
    }
    let config_watcher =
        ConfigWatcher::start(initial_core_config.clone()).context("start config watcher")?;
    // Real instruments when `telemetry.metrics_enabled` (bound to the global
    // meter provider installed in `main`); a true noop otherwise.
    let metrics = BorgMetrics::from_config(&initial_core_config);
    // RAII: records the `borgd` lifecycle start now and the shutdown on drop
    // (when `run` returns), keeping the start/shutdown counters balanced.
    let _lifecycle = cortex::telemetry::LifecycleGuard::new(metrics.clone(), "borgd");
    let cost_event_bus = CostEventBus::new(&initial_core_config, metrics.clone());
    let cost_event_sink: Arc<dyn CostEventSink> = Arc::new(cost_event_bus.clone());
    // The config→cost-bus sync bridge is started further down, once the
    // shutdown token exists, so it can run supervised — see below.

    let registry = SessionRegistry::new();
    // Daemon-wide subprocess registry — one handle, cloned everywhere
    // (`Arc` inside). Threaded into the session factory so tool handlers
    // see it via `ExecutionContext`, and wired into AdminSvc for RPC
    // inspection / pin toggling. Also installed as the process-global
    // handle so the scheduled-task runner's maintenance executor can
    // find it without threading parameters through the dispatcher.
    let process_registry = cortex::process_registry::ProcessRegistry::new();
    cortex::process_registry::install_global(process_registry.clone());
    // Process-global registry for persistent interactive shell sessions
    // (`run_shell` action=start/write_stdin/poll/kill). Sessions outlive any
    // single tool call's ExecutionContext, so the handler reaches them through
    // this global; uninstalled outside the daemon, where sessions degrade to a
    // "requires daemon" message.
    cortex::shell_session::install_global(cortex::shell_session::ShellSessionRegistry::new());
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
        metrics: metrics.clone(),
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
            // Any recv outcome means shutdown: a value is the stop signal, and
            // a closed/lagged channel only happens as the daemon tears down.
            let _ = sub.recv().await;
            token.cancel();
        });
    }

    // The hourly budget reset tick is started by `orchestrator::start`, which
    // owns the supervisor's DB handle — see `budget_tick::run`.

    // Keep the cost bus in step with live config edits. Supervised rather than
    // bare-spawned: it is a daemon-lifetime loop, and a swallowed panic here
    // would silently pin cost accounting to boot-time config — every later
    // `borg settings set` for pricing or budgets would appear to apply and
    // quietly not.
    match Database::open().await {
        Ok(db) => {
            let db: crate::supervisor::DbHandle = Arc::new(db);
            let bridge_token = orchestrator_token.clone();
            // Subscribe once; the receiver is cloned per (re)spawn so a
            // restarted bridge still observes later edits.
            let bridge_rx = config_watcher.subscribe();
            let bridge_bus = cost_event_bus.clone();
            crate::supervisor::spawn_supervised(
                crate::supervisor::SupervisorConfig::new("cost-bus-config-sync"),
                orchestrator_token.clone(),
                db,
                move || {
                    let cancel = bridge_token.clone();
                    let mut rx = bridge_rx.clone();
                    let bus = bridge_bus.clone();
                    async move {
                        loop {
                            tokio::select! {
                                biased;
                                _ = cancel.cancelled() => return Ok::<(), anyhow::Error>(()),
                                changed = rx.changed() => {
                                    if changed.is_err() {
                                        // Watcher dropped: the daemon is tearing
                                        // down, nothing left to sync.
                                        return Ok(());
                                    }
                                    bus.sync_from_config(&rx.borrow().clone());
                                }
                            }
                        }
                    }
                },
            );
        }
        Err(e) => {
            // Non-fatal: cost accounting keeps working against boot-time
            // config. Loud because live pricing edits will not take effect.
            tracing::warn!(
                error = %e,
                "borgd: cost-bus config sync not started (DB unavailable); \
                 live pricing/budget edits will not reach the cost bus until restart"
            );
        }
    }

    // Migration for pre-existing installs: if `user.timezone` was never set
    // (onboarding ran before host-timezone auto-detection landed), detect it
    // now so heartbeat quiet hours anchor to local time. Idempotent and
    // write-once — explicit user values are preserved.
    if let Ok(db) = Database::open().await {
        match db.get_setting("user.timezone").await {
            Ok(None) => {
                if let Some(tz) = cortex::config::Config::detect_host_timezone() {
                    if let Err(e) = db.set_setting("user.timezone", &tz).await {
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

    let extras = match CoreConfig::load_from_db().await {
        Ok(core_config) => {
            // Tunnel supervisor — points at the gateway's bind address so
            // the tunnel forwards traffic to the right local origin. The
            // initial apply happens here so a daemon configured with a
            // non-"none" provider stands the tunnel up at startup; the
            // operator still has `borg tunnel {start,stop,status}` for
            // manual control after that.
            let tunnel_sup = crate::tunnel_supervisor::TunnelSupervisor::new(
                core_config.gateway.host.clone(),
                core_config.gateway.port,
            );
            if !matches!(
                core_config.tunnel.provider,
                cortex::config::TunnelProvider::None
            ) {
                if let Err(e) = tunnel_sup.apply(&core_config.tunnel).await {
                    tracing::warn!(
                        "borgd: initial tunnel start failed ({e}); operator can retry with `borg tunnel start`"
                    );
                }
            }
            let handles = orchestrator::start(
                registry.clone(),
                core_config,
                orchestrator_token.clone(),
                config_watcher.subscribe(),
                cost_event_sink.clone(),
                goal_bus.clone(),
            )
            .await;
            grpc::AdminExtras {
                heartbeat_bus: Some(handles.heartbeat_bus),
                executor: Some(handles.executor),
                heartbeat_trigger_tx: Some(handles.heartbeat_trigger_tx),
                shutdown_tx: Some(shutdown_tx.clone()),
                config_notify_tx: Some(config_watcher.notify_sender()),
                process_registry: Some(process_registry.clone()),
                goal_bus: Some(handles.goal_bus),
                tunnel_supervisor: Some(tunnel_sup),
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
                tunnel_supervisor: None,
            }
        }
    };
    // ── PKI bootstrap ──
    //
    // Mint root CA + server cert on first run; load from DB+keychain on
    // subsequent starts. We open a daemon-owned `Database` here and wrap it
    // in `Arc<Mutex>` so the AuditWriter and Pki service share the single
    // writer with the audit chain.
    let db_for_pki = Database::open()
        .await
        .context("opening DB for PKI bootstrap")?;
    let pki_db = Arc::new(tokio::sync::Mutex::new(db_for_pki));
    let kek = {
        // Resolve KEK backend before locking pki_db for cert ops. The
        // factory probes the OS keychain on `Auto`, falls back to the
        // passphrase-encrypted file, and persists the resolved choice so
        // posture snapshots see a concrete `kek_backend` value.
        let pki_cfg = CoreConfig::load_from_db()
            .await
            .context("loading config for KEK backend selection")?;
        let g = pki_db.lock().await;
        build_kek_store(&cfg.borg_home, &pki_cfg, &g)
            .await
            .context("build KEK store")?
    };
    let (ca_cert_pem, ca_key_pem) = {
        let g = pki_db.lock().await;
        ensure_root_ca(&g, kek.as_ref())
            .await
            .context("ensure root CA")?
    };
    // Derive the server-cert SAN from the bind address. Loopback (or no
    // `--listen`) gives `["localhost"]`; non-loopback requires the operator
    // to set `BORG_DAEMON_TLS_SAN`. When `discovery.mdns_enabled` is true,
    // the local hostname + its `.local` form are auto-appended so LAN peers
    // can pair without operator-side SAN edits.
    let mdns_enabled = CoreConfig::load_from_db()
        .await
        .map(|c| c.discovery.mdns_enabled)
        .unwrap_or(false);
    let san_dns = derive_server_san_with_mdns(cfg.listen_tcp.as_deref(), mdns_enabled, || {
        hostname::get()
            .ok()
            .and_then(|os| os.into_string().ok())
            .map(|h| h.trim_matches('.').to_string())
            .filter(|h| !h.is_empty())
    })
    .context("derive server cert SAN")?;
    let (server_cert_pem, server_key_pem, _server_serial) = {
        let g = pki_db.lock().await;
        ensure_server_cert(&g, kek.as_ref(), &ca_cert_pem, &ca_key_pem, &san_dns)
            .await
            .context("ensure server cert")?
    };
    let server_tls_material = ServerCertMaterial {
        server_cert_pem: server_cert_pem.clone(),
        server_key_pem: server_key_pem.clone(),
        ca_cert_pem: ca_cert_pem.clone(),
    };

    // Keep the stored leaf fresh while the daemon runs.
    //
    // `ensure_server_cert` renews inside a 14-day window, but it only ran here
    // at startup — so a daemon up past ~76 days (leaf validity is 90) never
    // renewed, and on day 90 every mTLS handshake began failing until someone
    // restarted it. Long-lived VPS deployments are exactly the target of the
    // file-KEK/headless path, so "it'll get restarted eventually" is not a
    // safe assumption.
    //
    // Residual limitation, deliberate: the running server serves the TLS
    // material captured above, so a renewal here does NOT hot-swap the live
    // listener — tonic's TLS config is fixed at build time and reworking it
    // would mean touching the guarded mTLS construction path. What this does
    // guarantee is that the *stored* cert is always current, so any restart
    // picks up a valid one, and that the operator is told loudly and early
    // rather than discovering it as handshake failures.
    {
        let renew_db = pki_db.clone();
        let renew_kek = kek.clone();
        let renew_ca_cert = ca_cert_pem.clone();
        let renew_ca_key = ca_key_pem.clone();
        let renew_san = san_dns.clone();
        let renew_serving_serial = _server_serial;
        let renew_token = orchestrator_token.clone();
        match Database::open().await {
            Ok(db) => {
                let sup_db: crate::supervisor::DbHandle = Arc::new(db);
                crate::supervisor::spawn_supervised(
                    crate::supervisor::SupervisorConfig::new("server-cert-renewal"),
                    orchestrator_token.clone(),
                    sup_db,
                    move || {
                        let cancel = renew_token.clone();
                        let pki_db = renew_db.clone();
                        let kek = renew_kek.clone();
                        let ca_cert = renew_ca_cert.clone();
                        let ca_key = renew_ca_key.clone();
                        let san = renew_san.clone();
                        async move {
                            const CHECK_INTERVAL: std::time::Duration =
                                std::time::Duration::from_secs(24 * 60 * 60);
                            loop {
                                tokio::select! {
                                    biased;
                                    _ = cancel.cancelled() => return Ok::<(), anyhow::Error>(()),
                                    _ = tokio::time::sleep(CHECK_INTERVAL) => {}
                                }
                                let renewed = {
                                    let g = pki_db.lock().await;
                                    ensure_server_cert(&g, kek.as_ref(), &ca_cert, &ca_key, &san)
                                        .await
                                };
                                match renewed {
                                    Ok((_, _, serial)) if serial != renew_serving_serial => {
                                        tracing::error!(
                                            new_serial = serial,
                                            serving_serial = renew_serving_serial,
                                            "server certificate was renewed, but this process is \
                                             still serving the previous one — restart borgd to \
                                             pick it up before the old cert expires"
                                        );
                                    }
                                    Ok(_) => {
                                        tracing::debug!("server certificate still current");
                                    }
                                    Err(e) => {
                                        tracing::warn!(error = %e, "server cert renewal check failed");
                                    }
                                }
                            }
                        }
                    },
                );
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "borgd: server-cert renewal check not started (DB unavailable); \
                     a daemon running past the cert's validity window will fail mTLS handshakes"
                );
            }
        }
    }
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
    // `output_redaction::RedactionPolicy::from_db` expects an
    // `Arc<std::sync::Mutex<Database>>` (its audit closure briefly snapshots
    // the libsql `Connection` under the sync lock and drops the guard before
    // awaiting). Open a dedicated handle so we don't have to thread the
    // wider `tokio::sync::Mutex<Database>` through that closure signature.
    let redaction_db = Database::open()
        .await
        .context("opening DB for redaction policy")?;
    let redaction_db_handle = Arc::new(Mutex::new(redaction_db));
    {
        let core_cfg = CoreConfig::load_from_db().await.unwrap_or_default();
        cortex::output_redaction::install_global(
            cortex::output_redaction::RedactionPolicy::from_db(
                core_cfg.security.output_redaction,
                redaction_db_handle.clone(),
            ),
        );
        let mut rx = config_watcher.subscribe();
        let db_for_policy = redaction_db_handle.clone();
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

    audit
        .emit(
            audit_events::DAEMON_START,
            "system",
            &json!({"pid": std::process::id()}),
        )
        .await;

    // Bootstrap MCP sessions before serving any traffic. `initialize`
    // returns `Ok(())` immediately when `mcp.enabled = false`, and on
    // per-server connect failures it logs + writes `mcp_server_failed`
    // audit rows without aborting daemon start — borg always boots.
    //
    // Without this call, configured MCP servers stay dark until an
    // operator triggers the explicit `reload_from_config` admin RPC.
    // That was the actual symptom: `mcp.enabled=true` + `borg mcp test`
    // succeeded standalone (test path is hermetic), but the LLM never
    // saw `mcp_*` tools because no session was ever installed in the
    // daemon process.
    //
    // Hot-reload runs in a spawned task: when the operator flips
    // `mcp.enabled` / `mcp.disabled_servers` / `mcp.servers` we
    // re-`reload_from_config` so the diff lands without a daemon
    // restart.
    {
        let mcp_cfg = initial_core_config.clone();
        if let Err(e) = cortex::mcp_clients::initialize(&mcp_cfg).await {
            tracing::warn!(error = %e, "mcp_clients::initialize failed (continuing without MCP)");
        }
        let mut rx = config_watcher.subscribe();
        tokio::spawn(async move {
            loop {
                if rx.changed().await.is_err() {
                    return;
                }
                let next = rx.borrow().clone();
                match cortex::mcp_clients::reload_from_config(&next).await {
                    Ok(report) => {
                        if !report.added.is_empty()
                            || !report.removed.is_empty()
                            || !report.failed.is_empty()
                        {
                            tracing::info!(
                                added = ?report.added,
                                removed = ?report.removed,
                                unchanged = report.unchanged,
                                failed = ?report.failed,
                                total_tools = report.total_tools,
                                "mcp_clients: hot-reload applied",
                            );
                        }
                    }
                    Err(e) => tracing::warn!(error = %e, "mcp_clients hot-reload failed"),
                }
            }
        });
    }

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
        let g = pki_db.lock().await;
        let core_cfg = CoreConfig::load_from_db().await.unwrap_or_default();
        match cortex::evolution::security::maybe_emit_posture_change(
            &g,
            &core_cfg,
            cortex::evolution::security::PostureSource::DaemonStartReconcile,
        )
        .await
        {
            Ok(cortex::evolution::security::EmitOutcome::Emitted {
                state, previous, ..
            }) => {
                // Transition is already logged at INFO inside
                // cortex::evolution::security::emit — only broadcast here.
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
            .await
            .context("load config for TCP listener verifier")?
            .security
            .verifier;
        let verifier = build_verifier(verifier_kind, pki_db.clone(), None)
            .await
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

        // mDNS announce — only when enabled AND the bind is non-loopback
        // (loopback peers can't reach a `.local` advertisement). The
        // announcer holds an mdns-sd ServiceDaemon that drains on shutdown;
        // we keep ownership in the function scope so it lives as long as
        // the listener and we can `shutdown()` it explicitly.
        let mdns_announcer = if mdns_enabled && !parsed.ip().is_loopback() {
            match maybe_announce_mdns(
                server_tls_material.server_cert_pem.as_bytes(),
                parsed.port(),
            )
            .await
            {
                Ok(a) => a,
                Err(e) => {
                    tracing::warn!(
                        "mdns: announce skipped ({e}); LAN discovery disabled for this run"
                    );
                    None
                }
            }
        } else {
            None
        };

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
        if let Some(a) = mdns_announcer {
            a.shutdown();
        }
    } else {
        let mut rx = shutdown_tx.subscribe();
        make_uds_router()
            .serve_with_incoming_shutdown(uds_stream, async move {
                let _ = rx.recv().await;
            })
            .await
            .context("UDS server")?;
    }

    // `_lifecycle` drop records the shutdown counter as this scope unwinds.
    tracing::info!("borgd shutdown complete");
    Ok(())
}

/// Resolve hostname + cert fingerprint + bind IPs and publish an mDNS
/// announcement for `_borgd._tcp.local.`. Returns `None` when the resolver
/// can't surface a usable hostname (some minimal containers); callers
/// treat that as a soft failure rather than blocking daemon start.
async fn maybe_announce_mdns(server_cert_pem: &[u8], port: u16) -> Result<Option<MdnsAnnouncer>> {
    let hostname_str = match hostname::get()
        .ok()
        .and_then(|os| os.into_string().ok())
        .map(|h| h.trim_matches('.').to_string())
        .filter(|h| !h.is_empty())
    {
        Some(h) => h,
        None => {
            tracing::debug!("mdns: hostname lookup returned none — skipping announce");
            return Ok(None);
        }
    };
    let fingerprint = identity::cert::fingerprint_of_pem(server_cert_pem)
        .map_err(|e| anyhow::anyhow!("fingerprint server cert: {e}"))?;
    let instance = default_instance_name(&hostname_str);
    let ips = local_announce_ips();
    let node_id = hostname_str.to_ascii_lowercase();
    let announcer = MdnsAnnouncer::announce(
        &instance,
        &format!("{}.local", hostname_str.to_ascii_lowercase()),
        port,
        &ips,
        &fingerprint,
        &node_id,
    )?;
    Ok(Some(announcer))
}

/// Local IPs published in the announcement. Empty by default: mdns-sd
/// will still publish the SRV/TXT records, and browsers resolve the
/// `<host>.local.` A-record via standard mDNS hostname resolution. Adding
/// explicit IPs would require an additional dep (`if-addrs`/`get_if_addrs`)
/// and silently expose every NIC the host has — opt-in behavior worth a
/// follow-up but not required for the SRV-discovery use case today.
fn local_announce_ips() -> Vec<std::net::IpAddr> {
    Vec::new()
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
    emit_daemon_stop(&audit, reason).await;
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
pub(crate) async fn emit_daemon_stop(audit: &AuditWriter, reason: &str) {
    audit
        .emit(
            audit_events::DAEMON_STOP,
            "system",
            &json!({"reason": reason, "pid": std::process::id()}),
        )
        .await;
}

#[cfg(test)]
mod tests {
    use super::{bind_uds, emit_daemon_stop, format_posture_transition};
    use cortex::db::{audit_events, AuditWriter, Database};
    use cortex::evolution::security::PostureState;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    #[tokio::test]
    async fn emit_daemon_stop_appends_event_with_reason_to_audit_chain() {
        // Real failure mode: graceful shutdowns that don't bookend the
        // DAEMON_START row leave the audit chain ambiguous about whether the
        // daemon exited cleanly or was killed mid-write. Spec §"Hash-chained
        // audit log" mandates daemon start/stop events on by default.
        let db = Database::test_db().await;
        let writer = AuditWriter::new(Arc::new(Mutex::new(db)));
        // Seed a DAEMON_START so we can also confirm chain integrity post-stop.
        writer
            .emit(
                audit_events::DAEMON_START,
                "system",
                &serde_json::json!({"pid": 1}),
            )
            .await;

        emit_daemon_stop(&writer, "sigterm").await;

        let outcome = writer.verify().await;
        assert!(
            matches!(outcome, cortex::db::AuditChainOutcome::Ok),
            "chain should still verify after stop, got {outcome:?}"
        );
        let dbref = writer.db();
        let g = dbref.lock().await;
        let events = g.list_audit_events().await.expect("list");
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
