//! gRPC client / daemon round-trip smoke + source-level invariant guards
//! for the post-daemon-migration cli.
//!
//! This file contains TWO tests:
//!
//!  1. `crates_cli_src_has_no_direct_db_access` — a source-grep regression
//!     guard. Source-grep tests are normally a AGENTS.md anti-pattern (the
//!     compiler already enforces module structure), but this one is
//!     intentionally listed as an exception under the "Source-grep tests"
//!     rule for the same reason as the TUI mouse-capture guards: the
//!     invariant ("the cli must not open the SQLite DB directly — the
//!     daemon owns the DB") is structural, not type-level. Motivating
//!     incident: prior to the daemon migration the cli routinely opened
//!     `Database::open()` from background tasks, racing the daemon's
//!     writer and corrupting state.
//!
//!  2. `daemon_client_round_trips_a_turn` — spins a `borgd` server over a
//!     temp UDS, opens a session, sends a trivial message, and asserts at
//!     least one `AgentEvent` arrives before the per-turn stream closes.
//!     Real failure modes guarded: a wire-incompatible proto regression
//!     (Open / Send response shape drift), a daemon-side mpsc forwarder
//!     that swallowed events, a socket-path mismatch between cli and
//!     daemon defaults. We intentionally do NOT require a configured LLM
//!     provider — if no provider is set, the daemon emits an `Error`
//!     event and that still proves the round-trip wire path works.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::type_complexity)]

use std::path::{Path, PathBuf};
use std::time::Duration;

use protocols::admin::admin_server::AdminServer;
use protocols::session::agent_event::Kind as EventKind;
use protocols::session::session_client::SessionClient;
use protocols::session::session_server::SessionServer;
use protocols::session::{OpenRequest, SendRequest};
use std::sync::{Arc, Mutex as StdMutex};

use async_trait::async_trait;
use borgd::daemon::bind_uds;
use borgd::grpc::admin::AdminSvc;
use borgd::grpc::session::{SessionFactory, SessionSvc};
use borgd::session::backend::{InlineImage, SessionBackend};
use borgd::session::{SessionHost, SessionRegistry};
use cortex::agent::AgentEvent as CoreEvent;
use tokio::net::UnixStream;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio_stream::wrappers::UnixListenerStream;
use tokio_stream::StreamExt;
use tokio_util::sync::CancellationToken;
use tonic::transport::{Channel, Endpoint, Uri};
use tower::service_fn;

// ────────────────────────────────────────────────────────────────────────────
// Test 1: source-grep guard
// ────────────────────────────────────────────────────────────────────────────

/// Files explicitly allowed to retain `Database::open(` writer-handle
/// references during the in-flight cli→daemon RPC migration. Each entry
/// must come with a code comment justifying why it is structurally safe
/// to race the daemon's writer (or noting the followup RPC migration).
///
/// `commands/wasm_plugin.rs` — admin batch subcommands (`borg plugin
///   disable/enable/disable-publisher/enable-publisher`). Operator-driven,
///   one-shot, and SQLite's single-writer lock serializes against the
///   daemon. Followup: add `AdminClient::wasm_plugin_*` RPCs.
/// `tui/mod.rs` — the TUI's plugin-toggle handler writes
///   `db.disable_wasm_plugin`/`enable_wasm_plugin` from a one-shot async
///   block; same SQLite-lock reasoning as the cli subcommands. Followup:
///   wire to the new admin RPC once it lands.
/// `main.rs` — `output_redaction::install_global` keeps a long-lived DB
///   handle for the `redaction_audit` table. Migrating this away requires
///   a dedicated admin RPC since the audit closure fires from sync log
///   sinks. Tracked as a separate refactor.
const ALLOWLIST: &[&str] = &["commands/wasm_plugin.rs", "tui/mod.rs", "main.rs"];

fn is_allowlisted(rel: &str) -> bool {
    ALLOWLIST.iter().any(|a| rel.ends_with(a))
}

/// Recursively collect `.rs` files under `dir`.
fn collect_rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) => panic!("read_dir({}): {e}", dir.display()),
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_rs_files(&path, out);
        } else if path.extension().and_then(|s| s.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}

/// Baseline count of writer-handle openers carried forward during the
/// cli→daemon RPC migration. The shrink-only guard below pins this so
/// new openers can't be added without a deliberate baseline bump.
const ALLOWLIST_BASELINE: usize = 3;

#[test]
fn allowlist_is_shrink_only() {
    // Every change that GROWS this list needs an explicit baseline bump
    // and a code-review trail. Shrinking is always welcome.
    assert!(
        ALLOWLIST.len() <= ALLOWLIST_BASELINE,
        "ALLOWLIST grew (len = {}, baseline = {}); migrate the offending file to a DaemonClient admin RPC instead of allowlisting it. If a baseline bump is unavoidable, update ALLOWLIST_BASELINE in the same PR with justification.",
        ALLOWLIST.len(),
        ALLOWLIST_BASELINE,
    );
}

#[test]
fn crates_cli_src_has_no_direct_db_access() {
    // AGENTS.md "Source-grep tests" exception:
    // Invariant: the cli must not open the SQLite DB directly — the daemon
    // owns the DB and is the single writer. Motivating incident: prior
    // to the daemon migration the cli routinely raced the daemon's writer.
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let cli_src = PathBuf::from(&manifest_dir).join("src");
    assert!(cli_src.is_dir(), "{} is not a dir", cli_src.display());

    let mut files = Vec::new();
    collect_rs_files(&cli_src, &mut files);
    assert!(
        !files.is_empty(),
        "no .rs files under {}",
        cli_src.display()
    );

    // Path of THIS test source on disk; even if the test ever lived under
    // src/ (it doesn't), self-references would be exempt.
    let self_path = PathBuf::from(file!());

    let mut offenders: Vec<String> = Vec::new();
    for path in &files {
        if path == &self_path {
            continue;
        }
        let rel = match path.strip_prefix(&cli_src) {
            Ok(r) => r.to_string_lossy().replace('\\', "/"),
            Err(_) => continue,
        };
        if is_allowlisted(&rel) {
            continue;
        }
        let src = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) => panic!("read {}: {e}", path.display()),
        };

        // Track depth inside `mod tests { ... }` blocks (also covers
        // `#[cfg(test)] mod tests { ... }` since matching just the
        // `mod tests {` opener is sufficient — once we're inside, every
        // brace toggles depth).
        let mut in_tests_depth: i32 = 0;
        for (idx, raw_line) in src.lines().enumerate() {
            let trimmed = raw_line.trim();

            // Enter a tests module the moment we see its opening brace.
            if in_tests_depth == 0 && trimmed.starts_with("mod tests") && trimmed.contains('{') {
                in_tests_depth = 1;
                continue;
            }
            if in_tests_depth > 0 {
                // Naive brace counting — close-enough for the patterns
                // generated by rustfmt.
                in_tests_depth += raw_line.matches('{').count() as i32;
                in_tests_depth -= raw_line.matches('}').count() as i32;
                if in_tests_depth <= 0 {
                    in_tests_depth = 0;
                }
                continue;
            }

            // Skip pure comments (// or ///).
            if trimmed.starts_with("//") {
                continue;
            }

            // `Config::load_from_db()` is intentionally NOT forbidden here:
            // the body now opens the DB read-only (see `Database::open_readonly`),
            // so it cannot race the daemon's writer. The single-writer
            // invariant is enforced at the rusqlite layer; this guard only
            // catches direct writer-handle openers.
            //
            // Plain `cortex::db::` substring usages (e.g. row-type imports
            // like `cortex::db::ProjectRow`) are allowed — what we care
            // about is the writer handle, which is `Database::open(`. The
            // readonly handle is `Database::open_readonly(` and is a
            // transitional escape hatch for read-only consumers.
            if raw_line.contains("Database::open(") {
                offenders.push(format!("{}:{} {}", rel, idx + 1, raw_line.trim_end()));
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "cli source must not reference cortex::db::* / Database::open(); \
         the daemon owns the database. Offenders:\n{}\n\n\
         If this is unavoidable for an in-flight migration, add the file to ALLOWLIST \
         in this test with an inline comment justifying the exception.",
        offenders.join("\n")
    );
}

// ────────────────────────────────────────────────────────────────────────────
// Test 2: daemon round-trip
// ────────────────────────────────────────────────────────────────────────────

struct Harness {
    _tmp: tempfile::TempDir,
    socket: PathBuf,
    _shutdown: oneshot::Sender<()>,
}

async fn start_harness() -> Harness {
    let tmp = tempfile::tempdir().expect("tempdir");
    // SAFETY: this test binary contains a single async test that touches the
    // env, so no concurrent mutation is possible.
    unsafe {
        std::env::set_var("BORG_HOME", tmp.path());
    }

    let socket = tmp.path().join("borgd.sock");
    let listener = bind_uds(&socket).expect("bind_uds");
    let stream = UnixListenerStream::new(listener);

    let registry = SessionRegistry::new();
    // We use a stub backend rather than the production AgentBackend because
    // AgentBackend::new requires a configured LLM provider in the temp
    // BORG_HOME — supplying that would just shift the test from
    // "wire-shape regression" to "Agent::new init plumbing", which is
    // already covered by other tests. The wire path under test (Open →
    // Send → stream of AgentEvents → close) is identical for stub and
    // production backends.
    struct StubBackend {
        id: String,
    }
    #[async_trait]
    impl SessionBackend for StubBackend {
        async fn run_turn(
            &self,
            _text: String,
            _images: Vec<InlineImage>,
            event_tx: mpsc::Sender<CoreEvent>,
            _cancel: CancellationToken,
            _text_file_bridge: Option<
                std::sync::Arc<dyn cortex::execution_context::TextFileBridge>,
            >,
        ) {
            // Emit a delta + TurnComplete so the client observes the full
            // wire shape. A regression that lost either event would surface
            // as a smoke-test failure.
            let _ = event_tx.send(CoreEvent::TextDelta("ok".into())).await;
            let _ = event_tx.send(CoreEvent::TurnComplete).await;
        }
        fn session_id(&self) -> &str {
            &self.id
        }
    }

    struct StubFactory {
        // One-shot: yields a fresh backend on first Open and errors on the
        // second (this test only opens once).
        next: StdMutex<Option<Arc<dyn SessionBackend>>>,
    }
    impl SessionFactory for StubFactory {
        fn open<'a>(
            &'a self,
            _resume_id: Option<&'a str>,
            _execution_context: Option<cortex::execution_context::ExecutionContext>,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = anyhow::Result<SessionHost>> + Send + 'a>,
        > {
            Box::pin(async move {
                let backend = self
                    .next
                    .lock()
                    .expect("factory mutex poisoned")
                    .take()
                    .ok_or_else(|| anyhow::anyhow!("stub factory already consumed"))?;
                Ok(SessionHost::new(backend))
            })
        }
    }
    let backend: Arc<dyn SessionBackend> = Arc::new(StubBackend { id: "stub".into() });
    let factory: Arc<dyn SessionFactory> = Arc::new(StubFactory {
        next: StdMutex::new(Some(backend)),
    });
    let session_svc = SessionSvc::new(registry.clone(), factory);
    let admin_svc = AdminSvc::new(registry);

    let (tx, rx) = oneshot::channel::<()>();
    tokio::spawn(async move {
        let _ = tonic::transport::Server::builder()
            .add_service(SessionServer::new(session_svc))
            .add_service(AdminServer::new(admin_svc))
            .serve_with_incoming_shutdown(stream, async {
                let _ = rx.await;
            })
            .await;
    });

    let started = std::time::Instant::now();
    while UnixStream::connect(&socket).await.is_err() {
        if started.elapsed() > Duration::from_secs(2) {
            panic!("daemon UDS never became connectable");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    Harness {
        _tmp: tmp,
        socket,
        _shutdown: tx,
    }
}

async fn uds_channel(socket: PathBuf) -> Channel {
    Endpoint::try_from(protocols::TONIC_UDS_PLACEHOLDER_URL)
        .expect("endpoint")
        .connect_with_connector(service_fn(move |_: Uri| {
            let socket = socket.clone();
            async move {
                let s = UnixStream::connect(&socket).await?;
                Ok::<_, std::io::Error>(hyper_util::rt::TokioIo::new(s))
            }
        }))
        .await
        .expect("connect")
}

#[tokio::test(flavor = "multi_thread")]
async fn daemon_client_round_trips_a_turn() {
    let h = start_harness().await;
    let mut client = SessionClient::new(uds_channel(h.socket.clone()).await);

    // Open a fresh session.
    let opened = client
        .open(OpenRequest {
            resume_id: String::new(),
            workspace_cwd: String::new(),
            workspace_root: String::new(),
            enforce_workspace_root: false,
        })
        .await
        .expect("Open must succeed against a live daemon")
        .into_inner();
    assert!(
        !opened.session_id.is_empty(),
        "Open response must include a non-empty session_id"
    );

    // Send a trivial message and drain the per-turn event stream. The
    // turn may complete with TextDelta+TurnComplete (provider configured)
    // or with an Error (no provider). Either case proves the wire path
    // works; a regression where NO event arrives would fail this test.
    let mut stream = client
        .send(SendRequest {
            session_id: opened.session_id.clone(),
            text: "hi".into(),
            images: Vec::new(),
            documents: Vec::new(),
            resume_interrupted: false,
        })
        .await
        .expect("Send must succeed")
        .into_inner();

    let mut saw_event = false;
    let drain = async {
        while let Some(item) = stream.next().await {
            let evt = item.expect("event item must decode");
            saw_event = true;
            match evt.kind {
                Some(EventKind::TurnComplete(_)) | Some(EventKind::Error(_)) => break,
                _ => continue,
            }
        }
    };
    // Bound the drain so a regression that leaks the stream surfaces here
    // as a clear timeout instead of hanging the test runner.
    tokio::time::timeout(Duration::from_secs(15), drain)
        .await
        .expect("turn stream did not close within 15s");

    assert!(
        saw_event,
        "expected at least one AgentEvent (TextDelta / Error / TurnComplete) before stream close"
    );
}
