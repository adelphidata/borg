//! `Session.Rollback` end to end below the wire: a session backend with a
//! real agent, a real checkpoint store under an isolated data dir, and a
//! workspace on disk.
//!
//! What these pin is the part only the daemon decides: which session's undo
//! points are offered, that a rollback is refused while the agent is busy
//! (it would race the very files a turn is writing), and that the words in
//! `report` are ones a surface can show as they are.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use borgd::session::backend::{
    default_execution_context, AgentBackend, RollbackRequest, SessionBackend,
};
use cortex::config::Config;
use cortex::db::{Database, NewSessionSnapshot};
use cortex::execution_context::ExecutionContext;
use cortex::git::checkpoints::{CheckpointStore, SnapshotOutcome};
use cortex::telemetry::BorgMetrics;

/// Serializes the file: `BORG_HOME` is process-global.
static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct Fixture {
    backend: Arc<AgentBackend>,
    store: CheckpointStore,
    ws: PathBuf,
    _tmp: tempfile::TempDir,
    _guard: tokio::sync::MutexGuard<'static, ()>,
}

impl Fixture {
    /// A session confined to its workspace, as an editor or `--worktree`
    /// opens one.
    async fn new() -> Self {
        Self::open(|ws| {
            let ctx = ExecutionContext::for_workspace(ws.to_path_buf(), ws.to_path_buf())
                .expect("workspace context");
            async move { (Config::default(), Some(ctx)) }
        })
        .await
    }

    /// A session whose client named no folder — the web and desktop apps —
    /// on a daemon with the workspace as its configured default.
    async fn in_the_default_folder() -> Self {
        Self::open(|ws| {
            let mut config = Config::default();
            config.conversation.default_folder = Some(ws.to_string_lossy().into_owned());
            async move {
                let ctx = default_execution_context(&config, None).await;
                (config, ctx)
            }
        })
        .await
    }

    async fn open<F, Fut>(session: F) -> Self
    where
        F: FnOnce(&Path) -> Fut,
        Fut: std::future::Future<Output = (Config, Option<ExecutionContext>)>,
    {
        let guard = ENV_LOCK.lock().await;
        let tmp = tempfile::tempdir().expect("tempdir");
        let base = tmp.path().canonicalize().expect("canonicalize");
        let home = base.join("home");
        let ws = base.join("project");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&ws).unwrap();
        // SAFETY: serialized by `ENV_LOCK`; read while the backend is built
        // and on every data-dir lookup after it.
        unsafe { std::env::set_var("BORG_HOME", &home) };

        let (config, ctx) = session(&ws).await;
        let backend =
            AgentBackend::new(config, BorgMetrics::default(), None, None, None, ctx, None)
                .await
                .expect("backend builds without an LLM provider");
        Self {
            backend,
            store: CheckpointStore::new(&home),
            ws,
            _tmp: tmp,
            _guard: guard,
        }
    }

    fn write(&self, rel: &str, content: &str) {
        std::fs::write(self.ws.join(rel), content).unwrap();
    }

    fn read(&self, rel: &str) -> Option<String> {
        std::fs::read_to_string(self.ws.join(rel)).ok()
    }

    /// What a turn does before its first file-changing tool call: record
    /// the workspace and file the checkpoint against `session`.
    async fn undo_point_for(&self, session: &str) {
        let SnapshotOutcome::Taken(checkpoint) =
            self.store.snapshot(&self.ws).await.expect("snapshot")
        else {
            panic!("the workspace was not snapshotted");
        };
        let db = Database::open().await.expect("open db");
        db.record_session_snapshot(&NewSessionSnapshot {
            session_id: session,
            user_message_seq: None,
            snapshot_root: Path::new(&checkpoint.root),
            commit_id: &checkpoint.commit_id,
            uncovered: &checkpoint.uncovered,
            before_rollback: false,
        })
        .await
        .expect("record");
    }

    async fn undo_point(&self) {
        let session = self
            .backend
            .snapshot_state()
            .expect("state")
            .agent_session_id;
        self.undo_point_for(&session).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rollback_puts_the_files_back_and_a_second_one_brings_the_edits_back() {
    let fx = Fixture::new().await;
    fx.write("notes.txt", "before\n");
    fx.undo_point().await;
    fx.write("notes.txt", "after\n");
    fx.write("new.txt", "made by the agent\n");

    let list = fx
        .backend
        .rollback(RollbackRequest::List)
        .await
        .expect("list");
    assert!(!list.changed);
    assert_eq!(list.points.len(), 1);
    assert_eq!(list.points[0].number, 1);
    assert_eq!(list.points[0].differing_files, 2);
    assert!(list.report.contains("1. before a turn · 2 files differ"));
    assert_eq!(
        fx.read("notes.txt").as_deref(),
        Some("after\n"),
        "a list changes nothing"
    );

    let done = fx
        .backend
        .rollback(RollbackRequest::Restore(None))
        .await
        .expect("rollback");
    assert!(done.changed);
    assert_eq!(
        done.report,
        "Files are back to how they were before a turn: 1 file restored, 1 file removed. \
         Use /rollback again to undo this."
    );
    assert_eq!(fx.read("notes.txt").as_deref(), Some("before\n"));
    assert_eq!(fx.read("new.txt"), None);

    let undone = fx
        .backend
        .rollback(RollbackRequest::Restore(None))
        .await
        .expect("rollback again");
    assert!(undone.changed);
    assert!(
        undone.report.contains("before a rollback"),
        "{}",
        undone.report
    );
    assert_eq!(fx.read("notes.txt").as_deref(), Some("after\n"));
    assert_eq!(fx.read("new.txt").as_deref(), Some("made by the agent\n"));
}

/// A session is offered its own undo points only. Another conversation's
/// checkpoint of the same folder is not this one's to roll back to.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn another_conversations_undo_points_are_not_offered() {
    let fx = Fixture::new().await;
    fx.write("notes.txt", "before\n");
    fx.undo_point_for("some-other-conversation").await;
    fx.write("notes.txt", "after\n");

    let list = fx
        .backend
        .rollback(RollbackRequest::List)
        .await
        .expect("list");
    assert_eq!(list.points.len(), 0);
    assert!(list.report.contains("no undo points"), "{}", list.report);

    for request in [
        RollbackRequest::Restore(None),
        RollbackRequest::Restore(Some(1)),
    ] {
        let reply = fx.backend.rollback(request).await.expect("rollback");
        assert!(!reply.changed, "{request:?}: {}", reply.report);
    }
    assert_eq!(fx.read("notes.txt").as_deref(), Some("after\n"));
}

/// While the agent is busy a rollback is refused after a moment, in words
/// that say what to do, and no file moves. Waiting the turn out would look
/// like a hang; running would race the turn's own writes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rollback_is_refused_while_the_agent_is_busy() {
    let fx = Fixture::new().await;
    fx.write("notes.txt", "before\n");
    fx.undo_point().await;
    fx.write("notes.txt", "after\n");

    let holder = fx.backend.clone();
    let busy = tokio::spawn(async move { holder.exec_shell("sleep 3", 10_000).await });
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;

    let started = std::time::Instant::now();
    let reply = fx
        .backend
        .rollback(RollbackRequest::Restore(None))
        .await
        .expect("rollback");
    assert!(!busy.is_finished(), "the command must still hold the agent");
    assert!(started.elapsed() < std::time::Duration::from_secs(2));
    assert!(!reply.changed);
    assert_eq!(
        reply.report,
        "A turn is still running. Wait for it to finish, or stop it first."
    );
    assert_eq!(fx.read("notes.txt").as_deref(), Some("after\n"));

    busy.await.expect("join").expect("exec_shell");
    let reply = fx
        .backend
        .rollback(RollbackRequest::Restore(None))
        .await
        .expect("rollback");
    assert!(reply.changed, "{}", reply.report);
    assert_eq!(fx.read("notes.txt").as_deref(), Some("before\n"));
}

/// A caller that goes away mid-rollback (a closed tab, a dropped connection)
/// drops the call. The restore still runs to its end: a folder left between
/// two states because nobody was listening would be the worst outcome.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_rollback_finishes_after_its_caller_is_gone() {
    let fx = Fixture::new().await;
    fx.write("notes.txt", "before\n");
    fx.undo_point().await;
    fx.write("notes.txt", "after\n");
    fx.write("new.txt", "made by the agent\n");

    // Far too short for a restore, which runs git several times over.
    let hung_up = tokio::time::timeout(
        std::time::Duration::from_millis(1),
        fx.backend.rollback(RollbackRequest::Restore(None)),
    )
    .await;
    assert!(
        hung_up.is_err(),
        "the rollback answered within a millisecond"
    );

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while fx.read("notes.txt").as_deref() != Some("before\n") || fx.read("new.txt").is_some() {
        assert!(
            std::time::Instant::now() < deadline,
            "the restore stopped with its caller: notes.txt = {:?}, new.txt = {:?}",
            fx.read("notes.txt"),
            fx.read("new.txt")
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    // The agent is free again once the work is done.
    let list = fx
        .backend
        .rollback(RollbackRequest::List)
        .await
        .expect("list");
    assert!(list.report.contains("before a rollback"), "{}", list.report);
}

/// The web and desktop apps name no folder. With a default folder set, the
/// session works there without being fenced in, its files can be put back,
/// and a later resume — setting or no setting — carries on in the same place.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_session_with_no_folder_of_its_own_works_in_the_default_one() {
    let fx = Fixture::in_the_default_folder().await;
    let state = fx.backend.snapshot_state().expect("state");

    fx.write("notes.txt", "before\n");
    fx.undo_point().await;
    fx.write("notes.txt", "after\n");

    let done = fx
        .backend
        .rollback(RollbackRequest::Restore(None))
        .await
        .expect("rollback");
    assert!(done.changed, "{}", done.report);
    assert_eq!(fx.read("notes.txt").as_deref(), Some("before\n"));

    let resumed = default_execution_context(&Config::default(), Some(&state.agent_session_id))
        .await
        .expect("a resumed session has the folder it worked in");
    assert_eq!(resumed.workspace_cwd(), fx.ws);
    assert!(
        !resumed.enforce_workspace_root(),
        "a default folder is a place to work, not a fence"
    );

    let unknown = default_execution_context(&Config::default(), Some("no-such-session")).await;
    assert!(unknown.is_none(), "a session nobody stored has no folder");
}

/// A turn that reaches the daemon with no client behind it — a channel
/// message, a scheduled task, the heartbeat — names no folder either, and
/// works in the default one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_channel_session_works_in_the_default_folder() {
    use gateway::session_executor::{InboundTurn, SessionExecutor, TurnKind};

    let _guard = ENV_LOCK.lock().await;
    let tmp = tempfile::tempdir().expect("tempdir");
    let base = tmp.path().canonicalize().expect("canonicalize");
    let home = base.join("home");
    let folder = base.join("work");
    std::fs::create_dir_all(&home).unwrap();
    // SAFETY: serialized by `ENV_LOCK`.
    unsafe { std::env::set_var("BORG_HOME", &home) };
    // A daemon has its database before any turn arrives; without one the
    // pause check fails safe and no session is made.
    // It also has a row for this conversation, stamped with a folder from
    // an earlier life. Nobody chose that folder for it, so it does not count.
    let stale = base.join("stale");
    std::fs::create_dir_all(&stale).unwrap();
    {
        let db = Database::open().await.expect("database");
        db.ensure_session_row("telegram:42", 0, "model")
            .await
            .expect("row");
        db.set_session_workspace("telegram:42", &stale.to_string_lossy(), None)
            .await
            .expect("stamp");
    }
    let mut config = Config::default();
    config.conversation.default_folder = Some(folder.to_string_lossy().into_owned());
    let registry = borgd::session::SessionRegistry::new();
    let executor = borgd::session::DaemonSessionExecutor::new(
        registry.clone(),
        config,
        BorgMetrics::default(),
        None,
        None,
        None,
    );

    // No LLM provider is configured, so the turn itself fails. The session
    // it was for exists all the same, in its folder.
    let (mut events, _cancel) = executor
        .start_turn(InboundTurn {
            session_key: "telegram:42".into(),
            text: "hello".into(),
            images: vec![],
            kind: TurnKind::User,
        })
        .await
        .expect("start_turn");
    while tokio::time::timeout(std::time::Duration::from_secs(20), events.recv())
        .await
        .expect("the turn ends")
        .is_some()
    {}

    let live = registry.live_ids();
    let [id] = live.as_slice() else {
        panic!("one live session, got {live:?}");
    };
    let host = registry.peek(id).expect("a live session");
    assert_eq!(host.working_dir(), Some(folder));
}
