//! The undo point taken before a turn changes files.
//!
//! Borg runs tools without asking, so being able to put the files back is the
//! safety control that replaces an approval prompt. Once per user turn, just
//! before the first tool call that can change the workspace, the agent
//! records the workspace in the checkpoint store
//! ([`crate::git::checkpoints`]) and notes the commit against the turn.
//!
//! Taken lazily, not at turn start: most turns of a personal assistant change
//! no files, and they pay nothing.

use std::path::PathBuf;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use super::tool_classification::is_mutating_tool_call;
use super::Agent;
use crate::db::NewSessionSnapshot;
use crate::git::checkpoints::{Checkpoint, CheckpointStore, SnapshotOutcome};
use crate::tool::names;
use crate::types::{Role, ToolCall};

/// How long a turn waits for its undo point. A first snapshot hashes the
/// whole workspace; past this the turn goes on without one rather than stall.
const SNAPSHOT_BUDGET: Duration = Duration::from_secs(30);

/// Mutating tools that never write a file in the workspace: they act on
/// Borg's own state (memory, schedules, goals, the agent tree) or on the
/// outside world. A tool missing from this list is assumed to touch files,
/// so a new tool gets an undo point by default.
const NEVER_TOUCHES_WORKSPACE_FILES: &[&str] = &[
    names::APPLY_SKILL_PATCH,
    names::BROWSER,
    names::CLOSE_AGENT,
    names::CREATE_CHANNEL,
    names::CREATE_GOAL,
    names::GMAIL,
    names::GOOGLE_CALENDAR,
    names::LINEAR,
    names::MANAGE_CRON,
    names::MANAGE_ROLES,
    names::MANAGE_TASKS,
    names::MEETINGS,
    names::NOTION,
    names::PHONE_CALL,
    names::SCHEDULE,
    names::SUBSCRIBE,
    names::TEXT_TO_SPEECH,
    names::TRANSCRIBE_AUDIO,
    names::UPDATE_GOAL,
    names::WAIT_FOR_AGENT,
    names::WRITE_MEMORY,
];

/// True when running `name` with `raw_args` could change a file in the
/// workspace — directly (`apply_patch`, `run_shell`) or through a child
/// agent or an external server (`spawn_agent`, `mcp_*`).
pub(crate) fn may_change_workspace_files(name: &str, raw_args: &str) -> bool {
    is_mutating_tool_call(name, raw_args) && !NEVER_TOUCHES_WORKSPACE_FILES.contains(&name)
}

impl Agent {
    /// Take this turn's undo point, if `calls` are about to change files and
    /// the turn does not have one yet.
    ///
    /// Never fails and never blocks the tools: a workspace that cannot be
    /// snapshotted is logged, and the turn runs without an undo point.
    pub(super) async fn checkpoint_before_mutation(
        &mut self,
        calls: &[ToolCall],
        cancel: &CancellationToken,
    ) {
        if !self.wants_checkpoint(calls) {
            return;
        }
        match crate::paths::resolve_borg_home_readonly(None) {
            Ok(home) => {
                self.take_checkpoint_unless_stopped(
                    &CheckpointStore::new(&home),
                    SNAPSHOT_BUDGET,
                    cancel,
                )
                .await;
            }
            Err(e) => {
                self.turn_checkpointed = true;
                tracing::warn!("no undo point for this turn: data dir unresolved: {e:#}");
            }
        }
    }

    /// True when `calls` are about to change files and this turn has no undo
    /// point yet.
    pub(super) fn wants_checkpoint(&self, calls: &[ToolCall]) -> bool {
        !self.turn_checkpointed
            && self.config.checkpoints.enabled
            // A sub-agent shares its parent's workspace, and the parent took
            // the undo point when it spawned the child.
            && self.spawn_depth == 0
            // Plan mode refuses every mutating call anyway.
            && !self.config.conversation.collaboration_mode.blocks_mutations()
            && calls.iter().any(|call| {
                may_change_workspace_files(&call.function.name, &call.function.arguments)
            })
    }

    /// [`Self::take_checkpoint`], given up the moment the turn is stopped.
    ///
    /// A first snapshot of a large folder can take its whole budget. The
    /// tools of a stopped turn are skipped, so there is nothing to undo and
    /// no reason to keep the person waiting.
    pub(super) async fn take_checkpoint_unless_stopped(
        &mut self,
        store: &CheckpointStore,
        budget: Duration,
        cancel: &CancellationToken,
    ) {
        tokio::select! {
            biased;
            () = cancel.cancelled() => {}
            () = self.take_checkpoint(store, budget) => {}
        }
    }

    /// Record the workspace in `store` as this turn's undo point, giving up
    /// after `budget`.
    pub(super) async fn take_checkpoint(&mut self, store: &CheckpointStore, budget: Duration) {
        // One attempt per turn, whatever comes of it: a workspace that timed
        // out once would time out before every later call too.
        self.turn_checkpointed = true;

        let root = self.checkpoint_root();
        if self.checkpoint_timed_out_in.as_deref() == Some(root.as_path()) {
            return;
        }
        match tokio::time::timeout(budget, store.snapshot(&root)).await {
            Ok(Ok(SnapshotOutcome::Taken(checkpoint))) => self.record_checkpoint(&checkpoint).await,
            Ok(Ok(SnapshotOutcome::Skipped(reason))) => {
                tracing::debug!(root = %root.display(), ?reason, "workspace not snapshotted");
            }
            Ok(Err(e)) => self.note_missing_undo_point(&root, &format!("{e:#}")).await,
            Err(_) => {
                self.note_missing_undo_point(
                    &root,
                    &format!(
                        "snapshot took longer than {budget:?}; \
                         this conversation makes no more undo points here"
                    ),
                )
                .await;
                self.checkpoint_timed_out_in = Some(root);
            }
        }
    }

    /// The directory an undo point covers: the repository the session works
    /// in when there is one (a tool can write anywhere in it), else the
    /// session's workspace.
    pub(super) fn checkpoint_root(&self) -> PathBuf {
        self.git_repo_root
            .clone()
            .unwrap_or_else(|| self.execution_context.workspace_root().to_path_buf())
    }

    async fn record_checkpoint(&self, checkpoint: &Checkpoint) {
        let user_message_seq = self
            .history
            .iter()
            .rev()
            .find(|message| {
                message.role == Role::User && !crate::conversation::is_agent_authored(message)
            })
            .and_then(|message| message.seq);
        let guard = self.db.lock().await;
        let Some(db) = guard.as_ref() else {
            tracing::warn!("undo point taken but not recorded: no database");
            return;
        };
        let recorded = db
            .record_session_snapshot(&NewSessionSnapshot {
                session_id: self.root_session_id(),
                user_message_seq,
                snapshot_root: &checkpoint.root,
                commit_id: &checkpoint.commit_id,
                uncovered: &checkpoint.uncovered,
                before_rollback: false,
            })
            .await;
        if let Err(e) = recorded {
            tracing::warn!(
                commit_id = %checkpoint.commit_id,
                "undo point taken but not recorded: {e:#}"
            );
        }
    }

    async fn note_missing_undo_point(&self, root: &std::path::Path, why: &str) {
        tracing::warn!(root = %root.display(), "no undo point for this turn: {why}");
        let guard = self.db.lock().await;
        if let Some(db) = guard.as_ref() {
            crate::activity_log::log_activity(
                db,
                "warn",
                "session",
                &format!(
                    "No undo point for file changes in {}: {why}",
                    root.display()
                ),
            )
            .await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::tests::minimal_test_agent;
    use crate::config::media::CollaborationMode;
    use crate::config::Config;
    use crate::db::{Database, SessionSnapshotRow};
    use crate::types::{FunctionCall, Message};

    /// An agent working in a plain directory, with a database and a store of
    /// its own. The person's message that started the turn is row 7 of the
    /// session log.
    struct Fixture {
        _tmp: tempfile::TempDir,
        agent: Agent,
        store: CheckpointStore,
        ws: PathBuf,
    }

    const TURN_SEQ: i64 = 7;

    impl Fixture {
        async fn new() -> Self {
            let tmp = tempfile::tempdir().expect("tempdir");
            let base = tmp.path().canonicalize().expect("canonicalize");
            let ws = base.join("project");
            std::fs::create_dir(&ws).expect("mkdir workspace");
            std::fs::write(ws.join("notes.txt"), "before\n").expect("seed file");

            let mut agent = minimal_test_agent(Config::default());
            agent.db = tokio::sync::Mutex::new(Some(Database::test_db().await));
            agent.execution_context =
                crate::execution_context::ExecutionContext::unrestricted(ws.clone());
            let mut asked = Message::user("tidy the notes");
            asked.seq = Some(TURN_SEQ);
            agent.history.push(asked);

            Self {
                store: CheckpointStore::new(&base.join("borg-home")),
                agent,
                ws,
                _tmp: tmp,
            }
        }

        /// What the loop does before it runs `calls`.
        async fn before(&mut self, calls: &[ToolCall]) {
            self.before_within(calls, SNAPSHOT_BUDGET).await;
        }

        async fn before_within(&mut self, calls: &[ToolCall], budget: Duration) {
            self.before_unless_stopped(calls, budget, &CancellationToken::new())
                .await;
        }

        async fn before_unless_stopped(
            &mut self,
            calls: &[ToolCall],
            budget: Duration,
            cancel: &CancellationToken,
        ) {
            if self.agent.wants_checkpoint(calls) {
                self.agent
                    .take_checkpoint_unless_stopped(&self.store, budget, cancel)
                    .await;
            }
        }

        async fn rows(&self) -> Vec<SessionSnapshotRow> {
            let guard = self.agent.db.lock().await;
            guard
                .as_ref()
                .expect("db")
                .list_session_snapshots(self.agent.root_session_id(), 10)
                .await
                .expect("list")
        }

        async fn warnings(&self) -> Vec<String> {
            let guard = self.agent.db.lock().await;
            guard
                .as_ref()
                .expect("db")
                .query_activity(10, Some("warn"), Some("session"))
                .await
                .expect("activity")
                .into_iter()
                .map(|entry| entry.message)
                .collect()
        }

        /// A file's content as an undo point holds it.
        fn held(&self, commit: &str, rel: &str) -> String {
            let out = std::process::Command::new("git")
                .args(["cat-file", "-p", &format!("{commit}:{rel}")])
                .env("GIT_DIR", self.store.dir())
                .output()
                .expect("spawn git");
            assert!(out.status.success(), "{rel} is not in {commit}");
            String::from_utf8(out.stdout).expect("utf-8")
        }
    }

    fn call(name: &str, args: &str) -> ToolCall {
        ToolCall {
            id: format!("call_{name}"),
            call_type: "function".to_string(),
            function: FunctionCall {
                name: name.to_string(),
                arguments: args.to_string(),
            },
        }
    }

    fn patch_call() -> ToolCall {
        call("apply_patch", r#"{"patch":"x"}"#)
    }

    /// The undo point holds the files as they were before the tool ran, and
    /// its row names the turn (the person's message) and the directory, so a
    /// later rollback knows what to put back and where.
    #[tokio::test]
    async fn a_file_changing_call_records_the_workspace_as_it_was() {
        let mut fx = Fixture::new().await;

        fx.before(&[call("read_file", r#"{"path":"notes.txt"}"#), patch_call()])
            .await;
        std::fs::write(fx.ws.join("notes.txt"), "after\n").expect("the tool's edit");

        let rows = fx.rows().await;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].user_message_seq, Some(TURN_SEQ));
        assert_eq!(rows[0].snapshot_root, fx.ws);
        assert_eq!(fx.held(&rows[0].commit_id, "notes.txt"), "before\n");
    }

    /// A turn has one undo point, taken before its first change: a second
    /// one would record the turn's own half-done edits as the state to go
    /// back to.
    #[tokio::test]
    async fn later_calls_in_the_same_turn_do_not_move_the_undo_point() {
        let mut fx = Fixture::new().await;

        fx.before(&[patch_call()]).await;
        std::fs::write(fx.ws.join("notes.txt"), "half done\n").expect("first edit");
        fx.before(&[call("run_shell", r#"{"command":"make"}"#)])
            .await;

        let rows = fx.rows().await;
        assert_eq!(rows.len(), 1);
        assert_eq!(fx.held(&rows[0].commit_id, "notes.txt"), "before\n");
    }

    /// The turn is the one the person started: a sub-agent's report and
    /// other rows Borg writes in the user role come later in the history and
    /// must not be taken for it.
    #[tokio::test]
    async fn the_undo_point_is_filed_under_the_persons_message() {
        let mut fx = Fixture::new().await;
        let mut notice = Message::user(crate::multi_agent::completion_notice("child finished"));
        notice.seq = Some(TURN_SEQ + 3);
        fx.agent.history.push(notice);

        fx.before(&[patch_call()]).await;

        assert_eq!(fx.rows().await[0].user_message_seq, Some(TURN_SEQ));
    }

    /// Turns that cannot change the workspace pay nothing: no row, and no
    /// store is even created.
    #[tokio::test]
    async fn turns_that_cannot_change_files_take_no_undo_point() {
        type Setup = fn(&mut Agent);
        let cases: &[(&str, Setup, ToolCall)] = &[
            (
                "read-only calls",
                |_| {},
                call("read_file", r#"{"path":"notes.txt"}"#),
            ),
            (
                "a change to Borg's own state",
                |_| {},
                call("write_memory", r#"{"content":"x"}"#),
            ),
            (
                "undo points switched off",
                |agent| agent.config.checkpoints.enabled = false,
                patch_call(),
            ),
            (
                "a sub-agent (its parent took the undo point)",
                |agent| agent.spawn_depth = 1,
                patch_call(),
            ),
            (
                "plan mode (the call is refused)",
                |agent| agent.config.conversation.collaboration_mode = CollaborationMode::Plan,
                patch_call(),
            ),
        ];
        for (name, setup, tool_call) in cases {
            let mut fx = Fixture::new().await;
            setup(&mut fx.agent);

            fx.before(std::slice::from_ref(tool_call)).await;

            assert!(fx.rows().await.is_empty(), "{name}");
            assert!(!fx.store.dir().exists(), "{name}");
        }
    }

    /// A workspace that cannot be snapshotted must not stop the work: the
    /// call returns, the failure is on record for the user, and the turn does
    /// not try (and fail, and wait) again before every later tool.
    #[tokio::test]
    async fn a_failed_or_slow_snapshot_is_reported_once_and_the_turn_goes_on() {
        // The data dir is a file, so the store cannot be created.
        let mut broken = Fixture::new().await;
        let not_a_dir = broken.ws.parent().expect("base").join("borg-home");
        std::fs::write(&not_a_dir, "").expect("occupy the data dir path");
        broken.before(&[patch_call()]).await;
        broken.before(&[patch_call()]).await;

        assert!(broken.rows().await.is_empty());
        let warnings = broken.warnings().await;
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(
            warnings[0].starts_with(&format!(
                "No undo point for file changes in {}",
                broken.ws.display()
            )),
            "{warnings:?}"
        );

        // Stopped: the tools will not run, so nothing is snapshotted and
        // nothing is missing.
        let mut stopped = Fixture::new().await;
        let stop = CancellationToken::new();
        stop.cancel();
        stopped
            .before_unless_stopped(&[patch_call()], SNAPSHOT_BUDGET, &stop)
            .await;

        assert!(stopped.rows().await.is_empty());
        assert!(stopped.warnings().await.is_empty());

        // No time at all: the snapshot is abandoned at its first wait.
        let mut slow = Fixture::new().await;
        slow.before_within(&[patch_call()], Duration::ZERO).await;

        assert!(slow.rows().await.is_empty());
        let warnings = slow.warnings().await;
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("took longer than"), "{warnings:?}");

        // A snapshot that ran out of time saved nothing, so the next one
        // would wait just as long. Later turns in that directory go straight
        // to their tools: no second wait, no second warning — even with all
        // the time in the world, which here would have made an undo point.
        slow.agent.turn_checkpointed = false;
        slow.before(&[patch_call()]).await;

        assert!(slow.rows().await.is_empty());
        assert_eq!(slow.warnings().await.len(), 1);

        // Another directory is another matter.
        let elsewhere = slow.ws.parent().expect("base").join("other-project");
        std::fs::create_dir(&elsewhere).expect("mkdir");
        std::fs::write(elsewhere.join("a.txt"), "1\n").expect("seed");
        slow.agent.execution_context =
            crate::execution_context::ExecutionContext::unrestricted(elsewhere.clone());
        slow.agent.turn_checkpointed = false;
        slow.before(&[patch_call()]).await;

        let rows = slow.rows().await;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].snapshot_root, elsewhere);
    }

    /// The tools that write files get an undo point; the ones that only
    /// change Borg's own state, or only read, do not cost a snapshot. A tool
    /// nobody classified is treated as file-changing.
    #[test]
    fn only_file_changing_calls_ask_for_an_undo_point() {
        let cases: &[(&str, &str, bool)] = &[
            ("apply_patch", r#"{"patch":"x"}"#, true),
            ("run_shell", r#"{"command":"make"}"#, true),
            ("spawn_agent", r#"{"task":"fix it"}"#, true),
            ("send_to_agent", r#"{"id":"a","message":"go"}"#, true),
            ("mcp_filesystem_write_file", "{}", true),
            ("some_tool_added_next_year", "{}", true),
            ("write_memory", r#"{"content":"x"}"#, false),
            ("schedule", r#"{"action":"create"}"#, false),
            ("update_goal", "{}", false),
            ("browser", r#"{"action":"click"}"#, false),
            ("read_file", r#"{"path":"a"}"#, false),
            ("list_dir", "{}", false),
            ("memory_search", r#"{"query":"x"}"#, false),
            ("web_search", r#"{"query":"x"}"#, false),
        ];
        for (name, args, expected) in cases {
            assert_eq!(may_change_workspace_files(name, args), *expected, "{name}");
        }
    }
}
