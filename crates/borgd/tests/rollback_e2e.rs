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

use borgd::session::backend::{AgentBackend, RollbackRequest, SessionBackend};
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
    async fn new() -> Self {
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

        let ctx =
            ExecutionContext::for_workspace(ws.clone(), ws.clone()).expect("workspace context");
        let backend = AgentBackend::new(
            Config::default(),
            BorgMetrics::default(),
            None,
            None,
            None,
            Some(ctx),
            None,
        )
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
