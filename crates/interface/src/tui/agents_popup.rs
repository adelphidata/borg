//! `/agents` popup — live spawn-tree of sub-agents for this session.
//!
//! Mirrors `borg agents tree` on the command line: depth-indented rows,
//! status glyph, nickname/role/model/metrics summary. Read paths
//! (`/agents`) hit `Admin.ListAgents`; mutating keys (`p` pause/resume,
//! `x` kill, `Shift+X` kill subtree) return an [`AgentsAction`] that the
//! App layer turns into the corresponding RPC.
//!
//! Keys consumed:
//!   - `j` / `Down`, `k` / `Up` — move cursor
//!   - `Enter` — toggle expansion of the highlighted parent (collapsed
//!     parents hide their children in the rendered list)
//!   - `p` — pause running agent / resume paused agent
//!   - `x` — kill highlighted agent
//!   - `Shift+X` — kill highlighted agent's entire subtree
//!   - `r` — reload snapshot from daemon
//!   - `s` — cycle sort mode (DepthFirst → DurationDesc → Status)
//!   - `f` — cycle filter mode (All → Running → Terminal)
//!   - `Esc` — close

use std::collections::HashSet;
use std::sync::Arc;

use anyhow::Result;
use cortex::config::Config;
use protocols::admin::{AgentEntry, AgentList};
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use tokio::sync::Mutex;

use super::app::{AppAction, PopupHandler};
use super::custom_terminal::Frame;
use super::popup_utils;
use super::theme;
use crate::daemon_client::DaemonClient;

/// Outcome of a key press. App-level code converts these into the
/// corresponding RPC. Split out so `handle_key` stays synchronous and
/// unit-testable without a live `DaemonClient`.
#[derive(Debug, PartialEq, Eq)]
pub enum AgentsAction {
    /// No mutating action — key was navigation or no-op.
    None,
    /// Close the popup.
    Dismiss,
    /// Refresh the snapshot from the daemon.
    Reload,
    /// Pause the highlighted agent (only valid for non-terminal agents).
    Pause { id: String },
    /// Resume the highlighted agent (only valid when status == "paused").
    Resume { id: String },
    /// Kill the highlighted agent.
    Kill { id: String },
    /// Kill the highlighted agent and every descendant.
    KillSubtree { id: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentSort {
    /// Default ordering: by depth then by created_at.
    DepthFirst,
    /// Longest-running first. Terminal agents sort by elapsed (ended-created),
    /// non-terminal by (now - created).
    DurationDesc,
    /// Group by status: Running → Paused → PendingInit → Completed → Errored → Shutdown.
    Status,
}

impl AgentSort {
    fn next(self) -> Self {
        match self {
            Self::DepthFirst => Self::DurationDesc,
            Self::DurationDesc => Self::Status,
            Self::Status => Self::DepthFirst,
        }
    }
    fn label(self) -> &'static str {
        match self {
            Self::DepthFirst => "depth",
            Self::DurationDesc => "duration",
            Self::Status => "status",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentFilter {
    All,
    Running,
    Terminal,
}

impl AgentFilter {
    fn next(self) -> Self {
        match self {
            Self::All => Self::Running,
            Self::Running => Self::Terminal,
            Self::Terminal => Self::All,
        }
    }
    fn label(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Running => "running",
            Self::Terminal => "terminal",
        }
    }
    fn keep(self, entry: &AgentEntry) -> bool {
        match self {
            Self::All => true,
            Self::Running => matches!(entry.status.as_str(), "running" | "paused" | "pending_init"),
            Self::Terminal => is_terminal(&entry.status),
        }
    }
}

fn is_terminal(status: &str) -> bool {
    matches!(status, "completed" | "errored" | "shutdown")
}

fn is_pausable(status: &str) -> bool {
    matches!(status, "running" | "paused" | "pending_init")
}

pub struct AgentsPopup {
    visible: bool,
    snapshot: AgentList,
    /// Indices into `snapshot.agents` filtered + sorted for display.
    /// Recomputed on every state change (cheap; lists are small).
    visible_indices: Vec<usize>,
    /// Cursor into `visible_indices`.
    cursor: usize,
    expanded: HashSet<String>,
    sort: AgentSort,
    filter: AgentFilter,
    /// Last error from a fetch attempt, surfaced in the footer.
    error: Option<String>,
    /// Last status line from an executed action (e.g. "killed <id>").
    status: Option<(String, bool)>,
    client: Option<Arc<Mutex<DaemonClient>>>,
}

impl AgentsPopup {
    pub fn new() -> Self {
        Self {
            visible: false,
            snapshot: AgentList::default(),
            visible_indices: Vec::new(),
            cursor: 0,
            expanded: HashSet::new(),
            sort: AgentSort::DepthFirst,
            filter: AgentFilter::All,
            error: None,
            status: None,
            client: None,
        }
    }

    pub fn is_visible(&self) -> bool {
        self.visible
    }

    /// Open the popup and refresh the snapshot from the daemon.
    pub fn show(&mut self, client: &Arc<Mutex<DaemonClient>>) {
        self.visible = true;
        self.cursor = 0;
        self.expanded.clear();
        self.error = None;
        self.status = None;
        self.client = Some(client.clone());
        self.reload();
    }

    pub fn dismiss(&mut self) {
        self.visible = false;
    }

    /// Re-fetch the snapshot. Synchronously bridged via block_on so the
    /// popup state is consistent with what the operator sees.
    pub fn reload(&mut self) {
        if let Some(client) = self.client.clone() {
            let snap = tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(async {
                    let mut c = client.lock().await;
                    c.list_agents("").await
                })
            });
            match snap {
                Ok(s) => {
                    self.snapshot = s;
                    self.error = None;
                }
                Err(e) => {
                    self.error = Some(format!("list_agents failed: {e}"));
                    tracing::warn!("AgentsPopup reload failed: {e}");
                }
            }
            self.recompute_visible();
        }
    }

    /// Record the outcome of a mutating action — App calls this after the
    /// RPC completes so the operator sees the feedback line in the footer.
    pub fn record_action_result(&mut self, label: impl Into<String>, ok: bool) {
        self.status = Some((label.into(), ok));
    }

    /// Test helper: explicitly set snapshot. Avoids spinning a daemon to
    /// exercise the popup logic in unit tests.
    #[cfg(test)]
    pub fn set_snapshot_for_test(&mut self, snap: AgentList) {
        self.snapshot = snap;
        self.recompute_visible();
        self.visible = true;
    }

    /// Test helper: current cursor target (index into snapshot.agents),
    /// or `None` when no rows are visible.
    #[cfg(test)]
    pub fn cursor_agent_id_for_test(&self) -> Option<&str> {
        self.visible_indices
            .get(self.cursor)
            .map(|&i| self.snapshot.agents[i].id.as_str())
    }

    fn highlighted(&self) -> Option<&AgentEntry> {
        self.visible_indices
            .get(self.cursor)
            .map(|&i| &self.snapshot.agents[i])
    }

    /// Recompute the filter+sort projection. Also collapses descendants
    /// of any parent the operator has folded.
    fn recompute_visible(&mut self) {
        let mut indices: Vec<usize> = (0..self.snapshot.agents.len()).collect();

        // Hide descendants of collapsed parents. We resolve "is descendant"
        // via the chain of parent_session_id → session_id within the live
        // snapshot, so collapsing always hides every level below the
        // chosen node rather than just immediate children.
        //
        // Cycle defense: parent_session_id graphs are produced by the
        // daemon and *should* be acyclic, but a corrupt snapshot (eg.
        // hand-edited DB row) could induce a cycle. We bound the walk
        // both by a visited-set and by `agents.len()` to guarantee
        // forward progress on adversarial data — fail open (show the
        // row) rather than hang the UI thread.
        if !self.expanded.is_empty() {
            let hidden_sessions = self.collapsed_session_set();
            let max_hops = self.snapshot.agents.len() + 1;
            indices.retain(|&i| {
                let a = &self.snapshot.agents[i];
                let mut cur = a.parent_session_id.as_str();
                let mut seen: HashSet<String> = HashSet::new();
                for _ in 0..max_hops {
                    if cur.is_empty() {
                        return true;
                    }
                    if hidden_sessions.contains(cur) {
                        return false;
                    }
                    if !seen.insert(cur.to_string()) {
                        tracing::warn!(
                            session = cur,
                            "parent_session_id cycle detected; failing open"
                        );
                        return true;
                    }
                    match self.snapshot.agents.iter().find(|x| x.session_id == cur) {
                        Some(parent) => cur = parent.parent_session_id.as_str(),
                        None => return true,
                    }
                }
                tracing::warn!(
                    agent_id = %a.id,
                    "parent-chain walk exceeded agents.len(); failing open"
                );
                true
            });
        }

        indices.retain(|&i| self.filter.keep(&self.snapshot.agents[i]));

        // Sort.
        match self.sort {
            AgentSort::DepthFirst => {
                indices.sort_by_key(|&i| {
                    let a = &self.snapshot.agents[i];
                    (a.depth, a.created_at)
                });
            }
            AgentSort::DurationDesc => {
                let now = chrono::Utc::now().timestamp();
                indices.sort_by(|&a, &b| {
                    let ea = elapsed(&self.snapshot.agents[a], now);
                    let eb = elapsed(&self.snapshot.agents[b], now);
                    eb.cmp(&ea)
                });
            }
            AgentSort::Status => {
                indices.sort_by_key(|&i| status_rank(&self.snapshot.agents[i].status));
            }
        }

        self.visible_indices = indices;
        if self.cursor >= self.visible_indices.len() {
            self.cursor = self.visible_indices.len().saturating_sub(1);
        }
    }

    /// The `expanded` field stores the *collapsed* set in v1 — entries
    /// the operator has folded by hitting Enter on the parent. A future
    /// "expand-on-demand" mode (default-collapsed deep trees) can flip
    /// this without changing the recompute_visible contract.
    fn collapsed_session_set(&self) -> HashSet<String> {
        self.expanded.clone()
    }

    pub fn handle_key(&mut self, key: crossterm::event::KeyEvent) -> AgentsAction {
        use crossterm::event::{KeyCode, KeyModifiers};
        if !self.visible {
            return AgentsAction::None;
        }
        match key.code {
            KeyCode::Esc => {
                self.dismiss();
                AgentsAction::Dismiss
            }
            KeyCode::Char('r') => AgentsAction::Reload,
            KeyCode::Up | KeyCode::Char('k') => {
                if !self.visible_indices.is_empty() {
                    self.cursor = if self.cursor == 0 {
                        self.visible_indices.len() - 1
                    } else {
                        self.cursor - 1
                    };
                }
                AgentsAction::None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if !self.visible_indices.is_empty() {
                    self.cursor = (self.cursor + 1) % self.visible_indices.len();
                }
                AgentsAction::None
            }
            KeyCode::Char('s') => {
                self.sort = self.sort.next();
                self.recompute_visible();
                AgentsAction::None
            }
            KeyCode::Char('f') => {
                self.filter = self.filter.next();
                self.cursor = 0;
                self.recompute_visible();
                AgentsAction::None
            }
            KeyCode::Enter => {
                // Toggle collapse state for the highlighted node, but only
                // if it has at least one child — collapsing a leaf is a
                // no-op the operator never wants.
                if let Some(target) = self.highlighted().cloned() {
                    let has_child = self
                        .snapshot
                        .agents
                        .iter()
                        .any(|a| a.parent_session_id == target.session_id);
                    if has_child {
                        if !self.expanded.remove(&target.session_id) {
                            self.expanded.insert(target.session_id.clone());
                        }
                        self.recompute_visible();
                    }
                }
                AgentsAction::None
            }
            KeyCode::Char('p') => {
                if let Some(a) = self.highlighted() {
                    if a.status == "paused" {
                        return AgentsAction::Resume { id: a.id.clone() };
                    }
                    if is_pausable(&a.status) {
                        return AgentsAction::Pause { id: a.id.clone() };
                    }
                }
                AgentsAction::None
            }
            KeyCode::Char('x') if !key.modifiers.contains(KeyModifiers::SHIFT) => {
                // Lowercase 'x' — kill a single agent. Only meaningful on a
                // non-terminal target; emitting Kill for a Completed agent
                // would fire a guaranteed-no-op RPC and confuse the operator.
                if let Some(a) = self.highlighted() {
                    if !is_terminal(&a.status) {
                        return AgentsAction::Kill { id: a.id.clone() };
                    }
                }
                AgentsAction::None
            }
            KeyCode::Char('X') | KeyCode::Char('x')
                if key.modifiers.contains(KeyModifiers::SHIFT) =>
            {
                if let Some(a) = self.highlighted() {
                    if !is_terminal(&a.status) {
                        return AgentsAction::KillSubtree { id: a.id.clone() };
                    }
                }
                AgentsAction::None
            }
            _ => AgentsAction::None,
        }
    }

    pub fn render(&self, frame: &mut Frame) {
        let title = format!(
            "Agents (sort:{} filter:{})",
            self.sort.label(),
            self.filter.label()
        );
        let Some((inner, content_height)) =
            popup_utils::begin_popup_render(frame, self.visible, &title, 3, 2)
        else {
            return;
        };

        let mut lines: Vec<Line<'static>> = Vec::new();

        if let Some(err) = &self.error {
            lines.push(Line::from(Span::styled(format!(" ! {err}"), theme::dim())));
            lines.push(Line::default());
        }

        if self.snapshot.agents.is_empty() {
            lines.push(Line::from(Span::styled(
                " No sub-agents in this session yet.".to_string(),
                theme::dim(),
            )));
        } else if self.visible_indices.is_empty() {
            lines.push(Line::from(Span::styled(
                format!(
                    " No agents match filter '{}'. Press `f` to cycle.",
                    self.filter.label()
                ),
                theme::dim(),
            )));
        } else {
            lines.push(Line::from(Span::styled(
                format!(
                    " {} of {} agent(s)",
                    self.visible_indices.len(),
                    self.snapshot.agents.len()
                ),
                theme::dim(),
            )));
            lines.push(Line::default());
            for (row_idx, &i) in self.visible_indices.iter().enumerate() {
                let a = &self.snapshot.agents[i];
                let collapsed = self.expanded.contains(&a.session_id);
                let tree_glyph = if self
                    .snapshot
                    .agents
                    .iter()
                    .any(|c| c.parent_session_id == a.session_id)
                {
                    if collapsed {
                        "▸ "
                    } else {
                        "▾ "
                    }
                } else {
                    "  "
                };
                let label = format!(
                    "  {indent}{tree}{glyph} {nick} · {role} · {model} · [{status}]",
                    indent = "  ".repeat(a.depth as usize),
                    tree = tree_glyph,
                    glyph = status_glyph(&a.status),
                    nick = a.nickname,
                    role = a.role,
                    model = if a.model.is_empty() { "?" } else { &a.model },
                    status = a.status,
                );
                let style = if row_idx == self.cursor {
                    theme::popup_selected()
                } else {
                    ratatui::style::Style::default()
                };
                lines.push(Line::from(Span::styled(label, style)));
            }
        }

        let scroll_offset = self.cursor.saturating_sub(content_height.saturating_sub(3));
        let visible_lines: Vec<Line<'static>> = lines
            .into_iter()
            .skip(scroll_offset)
            .take(content_height)
            .collect();

        let content_area = Rect::new(inner.x, inner.y, inner.width, content_height as u16);
        frame.render_widget(Paragraph::new(visible_lines), content_area);

        let footer = if let Some((msg, ok)) = &self.status {
            if *ok {
                format!(" {msg}   j/k move · Enter fold · p pause · x kill · X subtree · s sort · f filter · r reload · Esc ")
            } else {
                format!(" ! {msg}   r retry · Esc ")
            }
        } else {
            " j/k move · Enter fold · p pause · x kill · X subtree · s sort · f filter · r reload · Esc "
                .to_string()
        };
        popup_utils::render_footer(frame, inner, &footer);
    }
}

impl Default for AgentsPopup {
    fn default() -> Self {
        Self::new()
    }
}

fn status_glyph(status: &str) -> &'static str {
    match status {
        "pending_init" => "○",
        "running" => "●",
        "paused" => "‖",
        "completed" => "✓",
        "errored" => "✗",
        "shutdown" => "■",
        _ => "?",
    }
}

fn status_rank(status: &str) -> u8 {
    match status {
        "running" => 0,
        "paused" => 1,
        "pending_init" => 2,
        "completed" => 3,
        "errored" => 4,
        "shutdown" => 5,
        _ => 9,
    }
}

fn elapsed(a: &AgentEntry, now: i64) -> i64 {
    let end = if a.ended_at > 0 { a.ended_at } else { now };
    (end - a.created_at).max(0)
}

impl PopupHandler for AgentsPopup {
    fn is_visible(&self) -> bool {
        self.visible
    }

    fn handle_key_event(
        &mut self,
        key: crossterm::event::KeyEvent,
        _config: &mut Config,
    ) -> Result<Option<AppAction>> {
        let action = self.handle_key(key);
        // Pull the (kind, id) tuple out in a single match so we don't
        // partially-move the id field and then try to re-discriminate
        // on the partially-moved enum.
        let (kind, id) = match action {
            AgentsAction::None => return Ok(None),
            AgentsAction::Dismiss => return Ok(Some(AppAction::LeaveAltScreen)),
            AgentsAction::Reload => {
                self.reload();
                return Ok(None);
            }
            AgentsAction::Pause { id } => (ActionKind::Pause, id),
            AgentsAction::Resume { id } => (ActionKind::Resume, id),
            AgentsAction::Kill { id } => (ActionKind::Kill, id),
            AgentsAction::KillSubtree { id } => (ActionKind::KillSubtree, id),
        };

        let Some(client) = self.client.clone() else {
            self.record_action_result("no daemon client wired", false);
            return Ok(None);
        };
        let id_for_rpc = id.clone();
        let rpc_result = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                let mut c = client.lock().await;
                match kind {
                    ActionKind::Pause => c.pause_agent(&id_for_rpc).await,
                    ActionKind::Resume => c.resume_agent(&id_for_rpc).await,
                    ActionKind::Kill => c.kill_agent(&id_for_rpc).await,
                    ActionKind::KillSubtree => c.kill_agent_subtree(&id_for_rpc).await,
                }
            })
        });
        let label = match kind {
            ActionKind::Pause => format!("paused {id}"),
            ActionKind::Resume => format!("resumed {id}"),
            ActionKind::Kill => format!("killed {id}"),
            ActionKind::KillSubtree => format!("killed subtree rooted at {id}"),
        };
        match rpc_result {
            Ok(()) => {
                self.record_action_result(label, true);
                self.reload();
            }
            Err(e) => {
                // Surface the daemon's actual error (e.g. the
                // executor-wiring-pending Unimplemented message) so
                // the operator sees the cause, not a generic "RPC failed".
                self.record_action_result(format!("{label}: {e}"), false);
            }
        }
        Ok(None)
    }
}

/// Discriminant for the four mutating RPCs the popup can fire. Folds the
/// action enum's payload-bearing variants into a single dispatch arm.
#[derive(Debug, Clone, Copy)]
enum ActionKind {
    Pause,
    Resume,
    Kill,
    KillSubtree,
}

#[cfg(test)]
mod tests;
