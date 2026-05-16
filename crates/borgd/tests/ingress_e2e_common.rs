#![allow(clippy::expect_used, clippy::unwrap_used)]
//! Shared helpers for the ingress-surface e2e tests.
//!
//! Each test file (`openai_compat_e2e.rs`, `generic_webhook_e2e.rs`,
//! `mcp_http_e2e.rs`) boots the real `gateway::GatewayServer` against
//! a tempdir-backed `BORG_HOME` on a free TCP port, then issues live
//! HTTP requests against it. The shared helpers live here so the
//! three tests don't duplicate the boot/teardown dance.
//!
//! Cargo treats each `tests/*.rs` file as a separate binary; this
//! module is included by `#[path = "ingress_e2e_common.rs"] mod common;`
//! so it isn't compiled as a standalone test binary.

#![allow(dead_code)]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use borgd::session::backend::{InlineImage, SessionBackend};
use borgd::session::{DaemonSessionExecutor, SessionHost, SessionRegistry};
use cortex::agent::AgentEvent;
use cortex::config::Config;
use cortex::telemetry::BorgMetrics;
use gateway::session_executor;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// One booted gateway, ready for HTTP smoke. Drop the harness to
/// trigger shutdown — the listener task observes the cancellation
/// token and exits.
pub struct GatewayHarness {
    pub base_url: String,
    pub borg_home: PathBuf,
    pub shutdown: CancellationToken,
    // Held for the lifetime of the harness so concurrent tests in the
    // same binary serialise on the shared `BORG_DATA_DIR` and SQLite
    // file. Dropped (and the lock released) when the harness drops.
    _serial: tokio::sync::OwnedMutexGuard<()>,
}

impl GatewayHarness {
    pub fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }

    pub async fn shutdown(self) {
        self.shutdown.cancel();
        // Brief grace period so the gateway closes its socket before
        // the next test (which may pick the same port) tries to bind.
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Boot the gateway server against a free TCP port and a freshly
/// initialised `BORG_HOME`. Returns once the server has answered a
/// `/healthz` probe.
///
/// `with_executor` controls whether a stub `SessionExecutor` is
/// installed in `gateway::session_executor::set_global`. Tests that
/// exercise the agent dispatch path (OpenAI-compat, webhook) pass
/// `true`; tests that only need an HTTP surface (MCP HTTP, which
/// never hits the executor) pass `false`. Per-session scripted
/// output is registered via [`register_stub_for_session`].
pub async fn boot_gateway(with_executor: bool) -> GatewayHarness {
    // Serialise test bodies *within* this test binary — `BORG_DATA_DIR`
    // and the SQLite file under it don't tolerate parallel migrations +
    // opens. Cross-binary isolation (cargo runs each `tests/*.rs` as a
    // separate process) is automatic: each process has its own env, its
    // own `OnceLock<TempDir>`, its own DB. The mutex is held by the
    // returned harness until it drops.
    let serial = serial_lock().clone().lock_owned().await;

    let borg_home = ensure_shared_borg_home();

    // Force a database init so the gateway can open ~/.borg/borg.db
    // without racing — `Database::open()` runs migrations.
    {
        let _db = cortex::db::Database::open().await.expect("init borg.db");
    }

    let port = pick_free_port();
    let base_url = format!("http://127.0.0.1:{port}");

    let mut config = Config::default();
    config.gateway.host = "127.0.0.1".into();
    config.gateway.port = port;
    // Generous rate limit so multiple test cases in the same process
    // don't trip per-principal throttling.
    config.gateway.rate_limit_per_minute = 1_000;

    if with_executor {
        install_global_executor();
    }

    let shutdown = CancellationToken::new();
    let server_shutdown = shutdown.clone();
    let server_config = config.clone();
    tokio::spawn(async move {
        let metrics = BorgMetrics::noop();
        let server = match gateway::GatewayServer::new(
            server_config,
            server_shutdown,
            metrics,
            None,
            None,
        ) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("test gateway construction failed: {e:#}");
                return;
            }
        };
        if let Err(e) = server.run().await {
            eprintln!("test gateway exited with error: {e:#}");
        }
    });

    wait_for_health(&base_url).await;

    GatewayHarness {
        base_url,
        borg_home,
        shutdown,
        _serial: serial,
    }
}

fn serial_lock() -> &'static Arc<tokio::sync::Mutex<()>> {
    use std::sync::OnceLock;
    static M: OnceLock<Arc<tokio::sync::Mutex<()>>> = OnceLock::new();
    M.get_or_init(|| Arc::new(tokio::sync::Mutex::new(())))
}

/// Per-session scripted event queue. Tests call
/// [`register_stub_for_session`] with the session key the dispatcher
/// will resolve (typically `"<token-label>:"` for OpenAI-compat,
/// `"<extracted-sender>:"` for generic webhooks) and a one-shot list
/// of events to emit on the next turn. The shared backend registered
/// for that session key drains the queue one batch per `run_turn`.
type ScriptedQueue = std::sync::Mutex<std::collections::VecDeque<Vec<AgentEvent>>>;

fn scripted_for(session_key: &str) -> Arc<ScriptedQueue> {
    use std::sync::OnceLock;
    // Leaks per-session queues until process exit — that's fine for a
    // test binary with a small number of sessions. Don't reuse this
    // pattern in long-running production code.
    static MAP: OnceLock<std::sync::Mutex<std::collections::HashMap<String, Arc<ScriptedQueue>>>> =
        OnceLock::new();
    let map = MAP.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()));
    let mut g = map.lock().expect("lock");
    g.entry(session_key.to_string())
        .or_insert_with(|| Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new())))
        .clone()
}

fn shared_registry() -> &'static SessionRegistry {
    use std::sync::OnceLock;
    static R: OnceLock<SessionRegistry> = OnceLock::new();
    R.get_or_init(SessionRegistry::new)
}

/// Register a stub session host for `session_key` and queue one batch
/// of agent events to emit on the next `run_turn`. Subsequent calls
/// for the same `session_key` push more batches onto the FIFO so
/// multi-turn tests work as expected.
pub fn register_stub_for_session(session_key: &str, events: Vec<AgentEvent>) {
    let queue = scripted_for(session_key);
    queue.lock().expect("lock").push_back(events);

    // Backends are shared by session_id. If we already inserted one
    // for this key, reuse it — the queue is the only state that
    // changes per turn.
    if shared_registry().get(session_key).is_some() {
        return;
    }

    struct QueueBackend {
        id: String,
        queue: Arc<ScriptedQueue>,
    }

    #[async_trait::async_trait]
    impl SessionBackend for QueueBackend {
        async fn run_turn(
            &self,
            _text: String,
            _images: Vec<InlineImage>,
            event_tx: mpsc::Sender<AgentEvent>,
            _cancel: CancellationToken,
            _text_file_bridge: Option<
                std::sync::Arc<dyn cortex::execution_context::TextFileBridge>,
            >,
        ) {
            let batch = self.queue.lock().expect("lock").pop_front();
            let mut emitted_terminal = false;
            if let Some(batch) = batch {
                for e in batch {
                    let is_terminal = matches!(e, AgentEvent::TurnComplete | AgentEvent::Error(_));
                    if event_tx.send(e).await.is_err() {
                        return;
                    }
                    if is_terminal {
                        emitted_terminal = true;
                        break;
                    }
                }
            }
            if !emitted_terminal {
                let _ = event_tx.send(AgentEvent::TurnComplete).await;
            }
        }

        fn session_id(&self) -> &str {
            &self.id
        }
    }

    let backend: Arc<dyn SessionBackend> = Arc::new(QueueBackend {
        id: session_key.to_string(),
        queue,
    });
    shared_registry().insert(SessionHost::new(backend));
}

/// Install a `DaemonSessionExecutor` over [`shared_registry`] into the
/// gateway's process-global slot. `set_global` is `OnceLock`-backed,
/// so repeat calls (across multiple test files in the same binary)
/// silently no-op — that's fine; the registry is shared regardless.
fn install_global_executor() {
    let exec = Arc::new(DaemonSessionExecutor::new(
        shared_registry().clone(),
        Config::default(),
        BorgMetrics::noop(),
        None,
        None,
        None,
    ));
    session_executor::set_global(exec);
}

/// Process-shared `BORG_DATA_DIR`. Created on first call, kept alive
/// by a `OnceLock<TempDir>` so the directory stays present for the
/// lifetime of the test binary regardless of individual harness drops.
fn ensure_shared_borg_home() -> PathBuf {
    use std::sync::OnceLock;
    static SHARED: OnceLock<tempfile::TempDir> = OnceLock::new();
    let dir = SHARED.get_or_init(|| {
        let td = tempfile::tempdir().expect("tempdir for BORG_DATA_DIR");
        std::env::set_var("BORG_DATA_DIR", td.path());
        td
    });
    dir.path().to_path_buf()
}

fn pick_free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind 0");
    listener.local_addr().expect("local_addr").port()
}

async fn wait_for_health(base_url: &str) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let client = reqwest::Client::new();
    loop {
        if let Ok(resp) = client.get(format!("{base_url}/healthz")).send().await {
            if resp.status().is_success() {
                return;
            }
        }
        if std::time::Instant::now() > deadline {
            panic!("gateway never came up at {base_url}");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Issue a fresh client token via the same code path the
/// `borg client-token issue` CLI uses, and return the plaintext value
/// the bearer header should carry.
pub async fn issue_token(label: &str) -> String {
    let db = cortex::db::Database::open().await.expect("open db");
    let issued = cortex::auth::tokens::issue(&db, label, "e2e_test")
        .await
        .expect("issue token");
    issued.plaintext
}

/// Convenience: full BORG_HOME path on disk for the active harness.
pub fn current_borg_home() -> PathBuf {
    cortex::config::Config::data_dir().expect("data_dir")
}
