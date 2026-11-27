//! Putting the workspace back to an undo point (`/rollback`).
//!
//! The counterpart of [`super::checkpoint`]: that module records the files
//! before a turn changes them, this one puts them back. A rollback is itself
//! undoable — the files as they were just before it are filed as one more
//! undo point, so `/rollback` twice is a round trip.
//!
//! Only files move. The conversation is left as it is (`/undo` is the
//! command for that), and the model is told its edits are gone so it does not
//! build on files that no longer say what it wrote.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use super::Agent;
use crate::db::{NewSessionSnapshot, SessionSnapshotRow};
use crate::git::checkpoints::{
    Checkpoint, CheckpointStore, LeftAlone, LeftReason, RestoreOutcome, SnapshotOutcome,
};

/// How many of a conversation's undo points are offered, newest first.
const MAX_POINTS: u32 = 20;

/// Longest prompt preview in an undo point's label.
const PROMPT_PREVIEW_CHARS: usize = 60;

/// Most file names spelled out in one list of a report; the rest are counted.
const NAMED_PATHS: usize = 8;

/// A state the files of this conversation's workspace can be put back to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UndoPoint {
    /// Position in the list; 1 is the most recent.
    pub number: u32,
    /// When it was taken (unix seconds).
    pub created_at: i64,
    /// First line of what the person wrote to start the turn it precedes.
    pub prompt: Option<String>,
    /// True for the files as a rollback found them: restoring it undoes
    /// that rollback.
    pub before_rollback: bool,
    /// The directory it covers.
    pub root: PathBuf,
    /// How many files differ between this point and the files as they are.
    pub differing_files: usize,
    /// True when the conversation holds nothing after the message that
    /// started the turn it precedes: the turn's work was undone or rewound,
    /// and its file changes were left behind.
    pub turn_undone: bool,
}

/// What a rollback did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rollback {
    pub point: UndoPoint,
    /// Files written back, relative to the point's root.
    pub restored: Vec<PathBuf>,
    /// Files removed because the point does not have them.
    pub removed: Vec<PathBuf>,
    /// Files that differ from the point and were left as found.
    pub left: Vec<LeftAlone>,
}

/// Result of [`Agent::rollback_files`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RollbackOutcome {
    Restored(Rollback),
    /// The conversation has no undo point at all.
    NoUndoPoints,
    /// There are undo points, and none differs from the files as they are.
    NothingToUndo,
    /// The only undo points that differ are for a folder the conversation
    /// no longer works in. They are reached by number, never by default.
    OnlyInAnotherFolder,
    /// The number asked for is not in the list of `available` points.
    NoSuchPoint {
        available: u32,
    },
    /// Sub-agents are still at work and may be changing the same files.
    SubAgentsRunning,
}

/// An undo point with what a restore needs.
struct Candidate {
    point: UndoPoint,
    row: SessionSnapshotRow,
}

impl Agent {
    /// The folder a new undo point would cover, as the store names it.
    pub fn undo_folder(&self) -> PathBuf {
        let here = self.checkpoint_root();
        here.canonicalize().unwrap_or(here)
    }

    /// This conversation's undo points, newest first. Points the store no
    /// longer holds, and points for a directory that is gone, are left out.
    pub async fn undo_points(&self) -> Result<Vec<UndoPoint>> {
        let store = default_store()?;
        let candidates = self.rollback_candidates(&store).await?;
        Ok(candidates.into_iter().map(|c| c.point).collect())
    }

    /// Put the workspace's files back to an undo point: number `number` of
    /// [`Self::undo_points`], or — with `None` — the most recent one that
    /// differs from the files as they are.
    pub async fn rollback_files(&mut self, number: Option<u32>) -> Result<RollbackOutcome> {
        let store = default_store()?;
        self.rollback_files_in(&store, number).await
    }

    async fn rollback_files_in(
        &mut self,
        store: &CheckpointStore,
        number: Option<u32>,
    ) -> Result<RollbackOutcome> {
        let children_at_work = self
            .agent_control
            .as_mut()
            .is_some_and(crate::multi_agent::AgentControl::has_running_agents);
        if children_at_work {
            return Ok(RollbackOutcome::SubAgentsRunning);
        }

        let mut candidates = self.rollback_candidates(store).await?;
        if candidates.is_empty() {
            return Ok(RollbackOutcome::NoUndoPoints);
        }
        let available = u32::try_from(candidates.len()).unwrap_or(u32::MAX);
        // Without a number, only this folder: a conversation resumed in
        // another checkout still lists the first one's undo points, and
        // "undo the last change" must not reach into a folder the person is
        // no longer looking at. Those points are still there by number.
        let here = self.undo_folder();
        let picked = match number {
            Some(n) if (1..=available).contains(&n) => n as usize - 1,
            Some(_) => return Ok(RollbackOutcome::NoSuchPoint { available }),
            None => {
                let mut differing = candidates
                    .iter()
                    .enumerate()
                    .filter(|(_, c)| c.point.differing_files > 0);
                match differing.clone().find(|(_, c)| c.point.root == here) {
                    Some((index, _)) => index,
                    None if differing.next().is_some() => {
                        return Ok(RollbackOutcome::OnlyInAnotherFolder)
                    }
                    None => return Ok(RollbackOutcome::NothingToUndo),
                }
            }
        };
        let newest_commit = candidates.first().map(|c| c.row.commit_id.clone());
        let Candidate { point, row } = candidates.swap_remove(picked);

        // The files as they are become an undo point of their own, filed
        // before anything moves, so even a restore that fails halfway can be
        // undone. Not when nothing will move, or when the newest point
        // already is this state.
        if point.differing_files > 0 {
            let now = match store.snapshot(&row.snapshot_root).await? {
                SnapshotOutcome::Taken(checkpoint) => checkpoint,
                SnapshotOutcome::Skipped(why) => anyhow::bail!(
                    "{} cannot be rolled back ({why:?})",
                    row.snapshot_root.display()
                ),
            };
            if newest_commit.as_deref() != Some(now.commit_id.as_str()) {
                self.file_undo_point(&now)
                    .await
                    .context("record the files as they are, so the rollback can be undone")?;
            }
        }

        match store
            .restore(&row.snapshot_root, &row.commit_id, &row.uncovered)
            .await?
        {
            RestoreOutcome::Restored(report) => {
                let rollback = Rollback {
                    point,
                    restored: report.restored,
                    removed: report.removed,
                    left: report.left,
                };
                if !rollback.restored.is_empty() || !rollback.removed.is_empty() {
                    self.append_assistant_message(&rollback.notice_for_model())
                        .await;
                }
                Ok(RollbackOutcome::Restored(rollback))
            }
            RestoreOutcome::Gone => {
                anyhow::bail!("that undo point is no longer stored")
            }
            RestoreOutcome::Refused(why) => {
                anyhow::bail!(
                    "{} cannot be rolled back ({why:?})",
                    row.snapshot_root.display()
                )
            }
        }
    }

    /// The session's undo points that can still be restored, newest first,
    /// each compared with the files as they are now.
    async fn rollback_candidates(&self, store: &CheckpointStore) -> Result<Vec<Candidate>> {
        let rows = {
            let guard = self.db.lock().await;
            match guard.as_ref() {
                Some(db) => {
                    db.list_session_snapshots(self.root_session_id(), MAX_POINTS)
                        .await?
                }
                None => Vec::new(),
            }
        };

        // One comparison per directory: it reads the current state once
        // and measures every point of that directory against it.
        let mut by_root: HashMap<&Path, Vec<usize>> = HashMap::new();
        for (index, row) in rows.iter().enumerate() {
            by_root.entry(&row.snapshot_root).or_default().push(index);
        }
        let mut compared: HashMap<usize, usize> = HashMap::new();
        for (root, indexes) in by_root {
            let ids: Vec<&str> = indexes
                .iter()
                .map(|index| rows[*index].commit_id.as_str())
                .collect();
            match store.compare_with_now(root, &ids).await {
                Ok(Some(differing)) => {
                    for (index, files) in indexes.iter().zip(differing) {
                        if let Some(files) = files {
                            compared.insert(*index, files);
                        }
                    }
                }
                Ok(None) => {}
                // Most often the directory was moved or deleted. Its points
                // cannot be restored, which is not this command's failure.
                Err(e) => {
                    tracing::warn!(root = %root.display(), "undo points not offered: {e:#}");
                }
            }
        }

        // `/undo` drops what followed the person's message and keeps the
        // message; a second one drops that too. Either way nothing newer is
        // left. A compaction folds old turns away as well, but leaves newer
        // messages behind, so it is not mistaken for an undo.
        let newest_in_conversation = self.history.iter().filter_map(|message| message.seq).max();
        let mut candidates: Vec<Candidate> = Vec::new();
        for (index, row) in rows.into_iter().enumerate() {
            let Some(differing_files) = compared.remove(&index) else {
                continue;
            };
            // Two turns in a row with no file change between them share one
            // state; the later turn's label is the one to show.
            let repeats_previous = candidates.last().is_some_and(|previous| {
                previous.row.commit_id == row.commit_id
                    && previous.row.snapshot_root == row.snapshot_root
            });
            if repeats_previous {
                continue;
            }
            let number = u32::try_from(candidates.len() + 1).unwrap_or(u32::MAX);
            candidates.push(Candidate {
                point: UndoPoint {
                    number,
                    created_at: row.created_at,
                    prompt: row.prompt.as_deref().and_then(prompt_preview),
                    before_rollback: row.before_rollback,
                    root: row.snapshot_root.clone(),
                    differing_files,
                    turn_undone: row.user_message_seq.is_some_and(|opening| {
                        newest_in_conversation.is_none_or(|newest| newest <= opening)
                    }),
                },
                row,
            });
        }
        Ok(candidates)
    }

    /// File `checkpoint` as the state a rollback is about to replace.
    async fn file_undo_point(&self, checkpoint: &Checkpoint) -> Result<()> {
        let guard = self.db.lock().await;
        let db = guard.as_ref().context("no database")?;
        db.record_session_snapshot(&NewSessionSnapshot {
            session_id: self.root_session_id(),
            user_message_seq: None,
            snapshot_root: &checkpoint.root,
            commit_id: &checkpoint.commit_id,
            uncovered: &checkpoint.uncovered,
            before_rollback: true,
        })
        .await?;
        Ok(())
    }
}

fn default_store() -> Result<CheckpointStore> {
    let home = crate::paths::resolve_borg_home_readonly(None).context("resolve the data dir")?;
    Ok(CheckpointStore::new(&home))
}

/// First line of a prompt, cut to a label's length. `None` for a prompt with
/// no text in it.
fn prompt_preview(prompt: &str) -> Option<String> {
    let line = prompt
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())?;
    let mut preview: String = line.chars().take(PROMPT_PREVIEW_CHARS).collect();
    if line.chars().count() > PROMPT_PREVIEW_CHARS {
        preview.push('…');
    }
    Some(preview)
}

impl UndoPoint {
    /// What this point is, in the words a person picks it by.
    fn label(&self) -> String {
        match (&self.prompt, self.before_rollback) {
            (_, true) => "before a rollback".to_string(),
            (Some(prompt), false) => format!("before \"{prompt}\""),
            (None, false) => "before a turn".to_string(),
        }
    }
}

impl Rollback {
    /// What happened, for the person.
    pub fn report(&self) -> String {
        let mut text = if self.restored.is_empty() && self.removed.is_empty() {
            if self.left.is_empty() {
                "The files already match that undo point.".to_string()
            } else {
                "No file was changed.".to_string()
            }
        } else {
            let mut done = Vec::new();
            if !self.restored.is_empty() {
                done.push(format!("{} restored", files(self.restored.len())));
            }
            if !self.removed.is_empty() {
                done.push(format!("{} removed", files(self.removed.len())));
            }
            format!(
                "Files are back to how they were {}: {}. Use /rollback again to undo this.",
                self.point.label(),
                done.join(", ")
            )
        };
        if !self.left.is_empty() {
            let named: Vec<String> = self
                .left
                .iter()
                .take(NAMED_PATHS)
                .map(|left| format!("{} ({})", left.path.display(), reason(&left.why)))
                .collect();
            text.push_str(&format!(
                "\nLeft as they are: {}{}.",
                named.join(", "),
                and_more(self.left.len())
            ));
        }
        text
    }

    /// The row left in the conversation, so the model's next turn does not
    /// assume its earlier edits are still on disk.
    fn notice_for_model(&self) -> String {
        let mut text = format!(
            "The files in {} were rolled back to how they were {}.",
            self.point.root.display(),
            self.point.label()
        );
        for (verb, paths) in [("Restored", &self.restored), ("Removed", &self.removed)] {
            if paths.is_empty() {
                continue;
            }
            let named: Vec<String> = paths
                .iter()
                .take(NAMED_PATHS)
                .map(|path| path.display().to_string())
                .collect();
            text.push_str(&format!(
                " {verb}: {}{}.",
                named.join(", "),
                and_more(paths.len())
            ));
        }
        text.push_str(" Changes made to these files after that point are no longer on disk.");
        text
    }
}

impl RollbackOutcome {
    /// What happened, for the person.
    pub fn report(&self) -> String {
        match self {
            Self::Restored(rollback) => rollback.report(),
            Self::NothingToUndo => {
                "There is nothing to roll back: no undo point differs from the files as they are."
                    .to_string()
            }
            Self::OnlyInAnotherFolder => {
                "No undo point for this folder differs from the files as they are. \
                 This conversation has undo points for another folder: \
                 use /rollback list, then /rollback <number>."
                    .to_string()
            }
            Self::NoUndoPoints => NO_POINTS.to_string(),
            Self::NoSuchPoint { available } => format!(
                "There is no undo point with that number. Use a number from 1 to {available}, \
                 or /rollback list to see them."
            ),
            Self::SubAgentsRunning => {
                "Sub-agents are still working in this conversation. Stop them with /stop, \
                 then try again."
                    .to_string()
            }
        }
    }
}

/// Shown when a conversation has no undo point to offer.
const NO_POINTS: &str = "This conversation has no undo points. \
    Borg makes one before it changes files in a project folder.";

/// The undo points as a list a person picks from. `now` is the current time
/// in unix seconds. A point for a folder other than `here`, the one the
/// conversation works in, is shown with its folder: `/rollback <number>` on
/// it moves files there.
pub fn describe_undo_points(points: &[UndoPoint], here: &Path, now: i64) -> String {
    if points.is_empty() {
        return NO_POINTS.to_string();
    }
    let mut text = String::from("Undo points, newest first:");
    for point in points {
        let differs = match point.differing_files {
            0 => "same as now".to_string(),
            1 => "1 file differs".to_string(),
            n => format!("{n} files differ"),
        };
        let folder = if point.root == here {
            String::new()
        } else {
            format!(" · in {}", point.root.display())
        };
        text.push_str(&format!(
            "\n  {}. {} · {differs} · {}{folder}",
            point.number,
            point.label(),
            age(now.saturating_sub(point.created_at))
        ));
    }
    text.push_str("\nUse /rollback <number> to put the files back.");
    text
}

fn files(count: usize) -> String {
    if count == 1 {
        "1 file".to_string()
    } else {
        format!("{count} files")
    }
}

fn and_more(total: usize) -> String {
    match total.saturating_sub(NAMED_PATHS) {
        0 => String::new(),
        more => format!(" and {more} more"),
    }
}

fn reason(why: &LeftReason) -> String {
    match why {
        LeftReason::NotCovered => "not kept in undo points".to_string(),
        LeftReason::NestedRepository => "a repository of its own".to_string(),
        LeftReason::InTheWay => "a file undo points do not track is in its place".to_string(),
        LeftReason::Failed(error) => format!("could not be changed: {error}"),
    }
}

fn age(seconds: i64) -> String {
    match seconds {
        i64::MIN..=59 => "just now".to_string(),
        60..=3599 => format!("{} min ago", seconds / 60),
        3600..=86_399 => format!("{} h ago", seconds / 3600),
        _ => format!("{} d ago", seconds / 86_400),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::agent::tests::minimal_test_agent;
    use crate::config::Config;
    use crate::db::Database;
    use crate::types::{Message, Role};

    /// An agent working in a plain directory with a database and a store of
    /// its own, driven turn by turn the way the loop drives it.
    struct Fixture {
        _tmp: tempfile::TempDir,
        agent: Agent,
        store: CheckpointStore,
        ws: PathBuf,
    }

    impl Fixture {
        async fn new() -> Self {
            let tmp = tempfile::tempdir().expect("tempdir");
            let base = tmp.path().canonicalize().expect("canonicalize");
            let ws = base.join("project");
            std::fs::create_dir(&ws).expect("mkdir workspace");

            let mut agent = minimal_test_agent(Config::default());
            agent.db = tokio::sync::Mutex::new(Some(Database::test_db().await));
            agent.execution_context =
                crate::execution_context::ExecutionContext::unrestricted(ws.clone());
            Self {
                store: CheckpointStore::new(&base.join("borg-home")),
                agent,
                ws,
                _tmp: tmp,
            }
        }

        fn write(&self, rel: &str, content: &str) {
            let path = self.ws.join(rel);
            std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
            std::fs::write(path, content).expect("write");
        }

        fn read(&self, rel: &str) -> Option<String> {
            std::fs::read_to_string(self.ws.join(rel)).ok()
        }

        /// The person sends `prompt`, and the turn takes its undo point the
        /// way the loop does before the first file-changing tool call.
        async fn turn(&mut self, prompt: &str) {
            let session = self.agent.root_session_id().to_string();
            let seq = {
                let guard = self.agent.db.lock().await;
                guard
                    .as_ref()
                    .expect("db")
                    .insert_message(&session, "user", Some(prompt), None, None, None, None)
                    .await
                    .expect("log the message")
            };
            let mut asked = Message::user(prompt);
            asked.seq = Some(seq);
            self.agent.history.push(asked);
            self.agent.turn_checkpointed = false;
            self.agent
                .take_checkpoint(&self.store, Duration::from_secs(30))
                .await;
        }

        async fn rollback(&mut self, number: Option<u32>) -> RollbackOutcome {
            self.agent
                .rollback_files_in(&self.store, number)
                .await
                .expect("rollback")
        }

        async fn restored(&mut self, number: Option<u32>) -> Rollback {
            match self.rollback(number).await {
                RollbackOutcome::Restored(rollback) => rollback,
                other => panic!("not restored: {other:?}"),
            }
        }

        async fn points(&self) -> Vec<UndoPoint> {
            self.agent
                .rollback_candidates(&self.store)
                .await
                .expect("list")
                .into_iter()
                .map(|c| c.point)
                .collect()
        }
    }

    fn paths(names: &[&str]) -> Vec<PathBuf> {
        names.iter().map(PathBuf::from).collect()
    }

    /// `/rollback` with no number undoes the file changes of the last turn
    /// that made any, names the turn by the person's own words, and tells
    /// the model. A second `/rollback` brings the changes back.
    #[tokio::test]
    async fn rollback_puts_the_last_turns_files_back_and_is_itself_undoable() {
        let mut fx = Fixture::new().await;
        fx.write("notes.txt", "before\n");
        fx.turn("tidy the notes\nand be quick").await;
        fx.write("notes.txt", "after\n");
        fx.write("extra/new.txt", "made by the agent\n");

        let rollback = fx.restored(None).await;

        assert_eq!(fx.read("notes.txt").as_deref(), Some("before\n"));
        assert_eq!(fx.read("extra/new.txt"), None);
        assert_eq!(rollback.point.prompt.as_deref(), Some("tidy the notes"));
        assert_eq!(rollback.restored, paths(&["notes.txt"]));
        assert_eq!(rollback.removed, paths(&["extra/new.txt"]));
        assert_eq!(
            rollback.report(),
            "Files are back to how they were before \"tidy the notes\": 1 file restored, \
             1 file removed. Use /rollback again to undo this."
        );

        let told = fx.agent.history.last().expect("a notice for the model");
        assert_eq!(told.role, Role::Assistant);
        let told = told.text_content().expect("text");
        assert!(told.contains("Restored: notes.txt."), "{told}");
        assert!(told.contains("Removed: extra/new.txt."), "{told}");
        assert!(told.contains("no longer on disk"), "{told}");

        let again = fx.restored(None).await;

        assert!(again.point.before_rollback);
        assert_eq!(fx.read("notes.txt").as_deref(), Some("after\n"));
        assert_eq!(
            fx.read("extra/new.txt").as_deref(),
            Some("made by the agent\n")
        );
    }

    /// The list is newest first, numbered from 1, and says how far each
    /// point is from the files as they are. Two turns with no file change
    /// between them are one state and are listed once, under the later turn.
    #[tokio::test]
    async fn undo_points_are_listed_newest_first_with_how_much_differs() {
        let mut fx = Fixture::new().await;
        fx.write("a.txt", "1\n");
        fx.turn("first change").await;
        fx.write("a.txt", "2\n");
        fx.turn("looked, changed nothing").await;
        fx.turn("second change").await;
        fx.write("b.txt", "new\n");

        let points = fx.points().await;

        let seen: Vec<(u32, Option<&str>, usize)> = points
            .iter()
            .map(|p| (p.number, p.prompt.as_deref(), p.differing_files))
            .collect();
        assert_eq!(
            seen,
            [(1, Some("second change"), 1), (2, Some("first change"), 2),]
        );
        assert!(points.iter().all(|p| p.root == fx.ws && !p.before_rollback));
    }

    /// `/undo` takes a turn's work out of the conversation and leaves its
    /// files. The turn's undo point says so, which is how a surface knows
    /// to tell the person their files are still changed. A turn that is
    /// still there, or that the conversation has moved on from, never
    /// claims it; neither does a state a rollback filed.
    #[tokio::test]
    async fn an_undo_point_knows_when_its_turn_was_undone() {
        let mut fx = Fixture::new().await;
        fx.write("a.txt", "1\n");
        fx.turn("first change").await;
        fx.write("a.txt", "2\n");
        fx.agent.append_assistant_message("changed it").await;
        fx.turn("second change").await;
        fx.write("a.txt", "3\n");
        fx.agent.append_assistant_message("changed it again").await;
        // (second change, first change), newest first.
        async fn undone(fx: &Fixture) -> Vec<bool> {
            let points = fx.points().await;
            assert_eq!(points.len(), 2);
            points.iter().map(|p| p.turn_undone).collect()
        }
        assert_eq!(undone(&fx).await, [false, false]);

        // The first `/undo` drops the reply and keeps the person's message;
        // the second drops the message. The turn is undone from the first.
        assert_eq!(fx.agent.undo(), 1);
        assert_eq!(undone(&fx).await, [true, false]);
        assert_eq!(fx.agent.undo(), 1);
        assert_eq!(undone(&fx).await, [true, false]);

        // The conversation goes on: the old point is still offered, but its
        // turn is no longer the one that was just undone.
        fx.agent.append_assistant_message("something else").await;
        assert_eq!(undone(&fx).await, [false, false]);

        // A state a rollback files belongs to no turn.
        fx.restored(None).await;
        let points = fx.points().await;
        assert!(points[0].before_rollback);
        assert!(!points[0].turn_undone);
    }

    /// A number picks that point; a number that is not in the list changes
    /// nothing and says how many there are.
    #[tokio::test]
    async fn a_numbered_rollback_goes_to_that_point() {
        let mut fx = Fixture::new().await;
        fx.write("a.txt", "1\n");
        fx.turn("first change").await;
        fx.write("a.txt", "2\n");
        fx.turn("second change").await;
        fx.write("a.txt", "3\n");

        for missing in [0, 3, 99] {
            assert_eq!(
                fx.rollback(Some(missing)).await,
                RollbackOutcome::NoSuchPoint { available: 2 }
            );
            assert_eq!(fx.read("a.txt").as_deref(), Some("3\n"));
        }

        let rollback = fx.restored(Some(2)).await;

        assert_eq!(rollback.point.prompt.as_deref(), Some("first change"));
        assert_eq!(fx.read("a.txt").as_deref(), Some("1\n"));
    }

    /// A conversation that moved to another folder still lists the first
    /// folder's undo points. `/rollback` with no number is "undo the last
    /// change here": it never reaches into the folder that was left. By
    /// number, those points still work.
    #[tokio::test]
    async fn rollback_without_a_number_stays_in_the_folder_the_conversation_works_in() {
        let mut fx = Fixture::new().await;
        fx.write("a.txt", "1\n");
        fx.turn("first folder").await;
        fx.write("a.txt", "2\n");

        let second = fx.ws.parent().expect("base").join("second");
        std::fs::create_dir(&second).expect("mkdir");
        std::fs::write(second.join("b.txt"), "1\n").expect("seed");
        fx.agent.execution_context =
            crate::execution_context::ExecutionContext::unrestricted(second.clone());

        let outcome = fx.rollback(None).await;

        assert_eq!(outcome, RollbackOutcome::OnlyInAnotherFolder);
        assert!(outcome.report().contains("/rollback list"), "{outcome:?}");
        assert_eq!(fx.read("a.txt").as_deref(), Some("2\n"));

        fx.turn("second folder").await;
        std::fs::write(second.join("b.txt"), "2\n").expect("edit");

        let here = fx.restored(None).await;

        assert_eq!(here.point.root, second);
        assert_eq!(here.point.prompt.as_deref(), Some("second folder"));
        assert_eq!(
            std::fs::read_to_string(second.join("b.txt")).expect("read"),
            "1\n"
        );
        assert_eq!(fx.read("a.txt").as_deref(), Some("2\n"));

        let points = fx.points().await;
        let first = points
            .iter()
            .find(|point| point.prompt.as_deref() == Some("first folder"))
            .expect("the first folder's point is still listed");
        assert_eq!(first.root, fx.ws);
        let listed = describe_undo_points(&points, &fx.agent.undo_folder(), first.created_at);
        assert!(
            listed.contains(&format!("in {}", fx.ws.display())),
            "{listed}"
        );
        assert_eq!(listed.matches(" · in ").count(), 1, "{listed}");

        let there = fx.restored(Some(first.number)).await;

        assert_eq!(there.restored, paths(&["a.txt"]));
        assert_eq!(fx.read("a.txt").as_deref(), Some("1\n"));
    }

    /// With nothing to put back the command says so: a conversation with no
    /// undo points, and one whose points all match the files.
    #[tokio::test]
    async fn rollback_with_nothing_to_undo_changes_nothing() {
        let mut fx = Fixture::new().await;
        fx.write("a.txt", "1\n");
        // No undo point at all is its own answer, with or without a number:
        // "nothing differs" would claim there are points to compare.
        assert_eq!(fx.rollback(None).await, RollbackOutcome::NoUndoPoints);
        assert_eq!(fx.rollback(Some(1)).await, RollbackOutcome::NoUndoPoints);

        fx.turn("changed nothing").await;
        let history_len = fx.agent.history.len();

        assert_eq!(fx.rollback(None).await, RollbackOutcome::NothingToUndo);
        // Asked for by number, a point that matches the files is a no-op:
        // no new undo point, nothing said to the model.
        let rollback = fx.restored(Some(1)).await;
        assert_eq!(
            rollback.report(),
            "The files already match that undo point."
        );
        assert_eq!(fx.points().await.len(), 1);
        assert_eq!(fx.agent.history.len(), history_len);
    }

    /// A point the store no longer has (pruned) and a point for a directory
    /// that is gone are not offered: every listed point can be restored.
    #[tokio::test]
    async fn points_that_cannot_be_restored_are_not_offered() {
        let mut fx = Fixture::new().await;
        fx.write("a.txt", "1\n");
        fx.turn("real").await;
        fx.write("a.txt", "2\n");
        {
            let guard = fx.agent.db.lock().await;
            let db = guard.as_ref().expect("db");
            let gone_dir = fx.ws.parent().expect("parent").join("deleted-project");
            for (root, commit) in [
                (fx.ws.as_path(), "0".repeat(40)),
                (gone_dir.as_path(), "1".repeat(40)),
            ] {
                db.record_session_snapshot(&NewSessionSnapshot {
                    session_id: fx.agent.root_session_id(),
                    user_message_seq: None,
                    snapshot_root: root,
                    commit_id: &commit,
                    uncovered: &[],
                    before_rollback: false,
                })
                .await
                .expect("record");
            }
        }

        let points = fx.points().await;

        assert_eq!(points.len(), 1);
        assert_eq!(points[0].prompt.as_deref(), Some("real"));
        assert_eq!(points[0].number, 1);
    }

    /// Sub-agents share the workspace. While one is still at work a rollback
    /// would race its writes, so it is refused until they are done.
    #[tokio::test]
    async fn rollback_is_refused_while_sub_agents_are_at_work() {
        let mut fx = Fixture::new().await;
        fx.write("a.txt", "1\n");
        fx.turn("delegate").await;
        fx.write("a.txt", "2\n");
        let mut control = crate::multi_agent::AgentControl::new(
            &crate::config::MultiAgentConfig::default(),
            "root-session",
            0,
        );
        control.insert_mock_handle("child", "atlas");
        let done = control.test_completion_tx();
        fx.agent.agent_control = Some(control);

        assert_eq!(fx.rollback(None).await, RollbackOutcome::SubAgentsRunning);
        assert_eq!(fx.read("a.txt").as_deref(), Some("2\n"));

        done.send(crate::multi_agent::SubAgentCompletion {
            agent_id: "child".to_string(),
            nickname: "atlas".to_string(),
            status: crate::multi_agent::SubAgentStatus::Completed {
                result: "done".to_string(),
            },
            final_response: Some("done".to_string()),
            files_written: Vec::new(),
            undelivered_input: Vec::new(),
        })
        .await
        .expect("send completion");

        fx.restored(None).await;
        assert_eq!(fx.read("a.txt").as_deref(), Some("1\n"));
        // The child's report was only looked at, not taken: the parent's
        // next turn still gets it.
        let control = fx.agent.agent_control.as_mut().expect("control");
        assert_eq!(control.drain_completions().len(), 1);
    }

    #[test]
    fn a_prompt_preview_is_the_first_line_cut_to_length() {
        assert_eq!(
            prompt_preview("  \n\n  fix it  \nmore"),
            Some("fix it".into())
        );
        assert_eq!(prompt_preview(" \n\t\n"), None);
        let long = "é".repeat(PROMPT_PREVIEW_CHARS + 5);
        let preview = prompt_preview(&long).expect("preview");
        assert_eq!(preview.chars().count(), PROMPT_PREVIEW_CHARS + 1);
        assert!(preview.ends_with('…'));
        let exact = "x".repeat(PROMPT_PREVIEW_CHARS);
        assert_eq!(prompt_preview(&exact), Some(exact.clone()));
    }

    fn point(number: u32, prompt: Option<&str>, differing_files: usize) -> UndoPoint {
        UndoPoint {
            number,
            created_at: 1_000,
            prompt: prompt.map(str::to_string),
            before_rollback: false,
            root: PathBuf::from("/work/project"),
            differing_files,
            turn_undone: false,
        }
    }

    /// What a person reads after `/rollback list`.
    #[test]
    fn the_list_reads_as_numbered_lines_a_person_can_pick_from() {
        let mut undone = point(2, None, 1);
        undone.before_rollback = true;
        undone.created_at = 1_000 - 7_300;
        let mut old = point(3, None, 0);
        old.created_at = 1_000 - 200_000;
        let mut recent = point(1, Some("fix the parser"), 4);
        recent.created_at = 1_000 - 125;

        // The oldest point is from a folder the conversation has left.
        old.root = PathBuf::from("/work/first checkout");
        let here = Path::new("/work/project");

        assert_eq!(
            describe_undo_points(&[recent, undone, old], here, 1_000),
            "Undo points, newest first:\n  \
             1. before \"fix the parser\" · 4 files differ · 2 min ago\n  \
             2. before a rollback · 1 file differs · 2 h ago\n  \
             3. before a turn · same as now · 2 d ago · in /work/first checkout\n\
             Use /rollback <number> to put the files back."
        );
        assert_eq!(describe_undo_points(&[], here, 1_000), NO_POINTS);
        assert_eq!(age(-5), "just now");
        assert_eq!(age(59), "just now");
    }

    /// What a person reads after a rollback that had to leave files alone,
    /// and after one with too many files to name.
    #[test]
    fn a_report_names_what_was_left_and_counts_what_it_cannot_name() {
        let left = |path: &str, why: LeftReason| LeftAlone {
            path: PathBuf::from(path),
            why,
        };
        let mut rollback = Rollback {
            point: point(1, None, 3),
            restored: paths(&["a", "b"]),
            removed: Vec::new(),
            left: vec![
                left("big.bin", LeftReason::NotCovered),
                left("vendor", LeftReason::NestedRepository),
                left("build.log", LeftReason::InTheWay),
                left("locked", LeftReason::Failed("permission denied".into())),
            ],
        };
        assert_eq!(
            rollback.report(),
            "Files are back to how they were before a turn: 2 files restored. \
             Use /rollback again to undo this.\n\
             Left as they are: big.bin (not kept in undo points), \
             vendor (a repository of its own), \
             build.log (a file undo points do not track is in its place), \
             locked (could not be changed: permission denied)."
        );

        // Nothing moved and something was in the way: "the files already
        // match" would be false.
        let stuck = Rollback {
            point: point(1, None, 1),
            restored: Vec::new(),
            removed: Vec::new(),
            left: vec![left(
                "locked",
                LeftReason::Failed("permission denied".into()),
            )],
        };
        assert_eq!(
            stuck.report(),
            "No file was changed.\n\
             Left as they are: locked (could not be changed: permission denied)."
        );

        rollback.left.clear();
        rollback.removed = (0..NAMED_PATHS + 3)
            .map(|n| PathBuf::from(format!("gen/{n}.txt")))
            .collect();
        let told = rollback.notice_for_model();
        assert!(told.contains("Restored: a, b."), "{told}");
        assert!(told.contains("gen/7.txt and 3 more."), "{told}");
        assert!(!told.contains("gen/8.txt"), "{told}");
    }

    #[test]
    fn each_refusal_says_what_to_do_next() {
        assert!(RollbackOutcome::SubAgentsRunning.report().contains("/stop"));
        assert!(RollbackOutcome::NoSuchPoint { available: 4 }
            .report()
            .contains("from 1 to 4"));
        assert_eq!(RollbackOutcome::NoUndoPoints.report(), NO_POINTS);
        assert!(RollbackOutcome::NothingToUndo
            .report()
            .starts_with("There is nothing to roll back"));
    }
}
