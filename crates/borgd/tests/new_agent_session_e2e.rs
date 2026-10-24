//! `/new` swaps the agent onto a fresh session inside the same open host. The
//! conversation that follows must be a first-class session: listed, exportable
//! and forkable. Those all resolve the session through its `sessions` row, and
//! a turn only updates an existing row, so the row has to exist from the
//! moment the session starts — exactly as it does for a session opened fresh.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use borgd::session::backend::{AgentBackend, SessionBackend};
use cortex::config::Config;
use cortex::execution_context::ExecutionContext;
use cortex::telemetry::BorgMetrics;

/// Serializes the file: `BORG_HOME` is process-global.
static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[tokio::test]
async fn a_new_session_is_listed_stamped_and_forkable() {
    let _guard = ENV_LOCK.lock().await;
    let home = tempfile::tempdir().expect("tempdir");
    let workspace = tempfile::tempdir().expect("tempdir");
    // SAFETY: serialized by `ENV_LOCK`; read during backend construction.
    unsafe { std::env::set_var("BORG_HOME", home.path()) };
    let ctx = ExecutionContext::for_workspace(
        workspace.path().to_path_buf(),
        workspace.path().to_path_buf(),
    )
    .expect("workspace context");
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

    let id = backend.new_agent_session().await.expect("new session");
    let db = cortex::db::Database::open().await.expect("db");
    let row = db
        .session_by_id(&id)
        .await
        .expect("lookup")
        .expect("the new session must have a sessions row");
    let (expected_cwd, _) = cortex::session::workspace_identity(workspace.path());
    assert_eq!(
        row.cwd.as_deref(),
        Some(expected_cwd.to_string_lossy().as_ref()),
        "the new session must carry the host's workspace so --continue finds it"
    );

    // A turn in the new session lands in its log; forking must then see it.
    db.insert_message(&id, "user", Some("hello"), None, None, None, None)
        .await
        .expect("append");
    let forked = backend
        .fork()
        .await
        .expect("a conversation started with /new must be forkable");
    assert_eq!(forked.message_count, 1);
}
