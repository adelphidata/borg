//! `borg agents …` — operator surface for the spawn-tree dashboard.
//!
//! Read paths (`list`, `tree`, `history`, `snapshot`) hit the daemon's
//! `Admin.ListAgents` / `ListAgentSnapshots` / `GetAgentSnapshot` RPCs,
//! which read from `sub_agent_runs` / `agent_tree_snapshots`. They work
//! against any session, live or terminal.
//!
//! Mutating paths (`kill`, `pause`, `resume`) hit the daemon RPCs that
//! need a live `AgentControl` handle. In this slice the daemon returns
//! `Unimplemented` until the session-executor wiring lands; the CLI
//! surfaces the error verbatim so operators see the cause rather than
//! a generic "RPC failed".

use anyhow::Result;
use clap::Subcommand;
use protocols::admin::{AgentEntry, AgentMetrics};

use super::format_ts;
use crate::daemon_client::connect_default;

#[derive(Subcommand)]
pub(crate) enum AgentsAction {
    /// Flat list of every sub-agent the session has spawned (live + terminal).
    List {
        /// Session id. Empty = the most-recently active session.
        #[arg(long)]
        session: Option<String>,
    },
    /// Same data, rendered as a depth-indented tree.
    Tree {
        /// Session id. Empty = the most-recently active session.
        #[arg(long)]
        session: Option<String>,
    },
    /// List persisted spawn-tree snapshots (per-turn, capped to last 10).
    History {
        /// Session id. Empty = the most-recently active session.
        #[arg(long)]
        session: Option<String>,
    },
    /// Print one persisted snapshot by turn id.
    Snapshot {
        /// Snapshot turn id.
        turn_id: String,
        /// Owning session id. Empty = the most-recently active session.
        #[arg(long)]
        session: Option<String>,
    },
    /// Kill one agent. Returns the daemon's error message verbatim if the
    /// executor handle isn't wired yet.
    Kill {
        /// Agent id (UUID).
        id: String,
        /// Also kill every descendant in the agent's subtree.
        #[arg(long)]
        subtree: bool,
    },
    /// Pause an agent at its next iteration boundary.
    Pause {
        /// Agent id (UUID).
        id: String,
    },
    /// Resume a paused agent.
    Resume {
        /// Agent id (UUID).
        id: String,
    },
}

/// Top-level dispatch for `borg agents …`.
pub(crate) async fn dispatch_agents(action: AgentsAction) -> Result<()> {
    match action {
        AgentsAction::List { session } => list(session.unwrap_or_default()).await,
        AgentsAction::Tree { session } => tree(session.unwrap_or_default()).await,
        AgentsAction::History { session } => history(session.unwrap_or_default()).await,
        AgentsAction::Snapshot { turn_id, session } => {
            snapshot(session.unwrap_or_default(), turn_id).await
        }
        AgentsAction::Kill { id, subtree } => kill(id, subtree).await,
        AgentsAction::Pause { id } => pause(id).await,
        AgentsAction::Resume { id } => resume(id).await,
    }
}

async fn list(session: String) -> Result<()> {
    let mut client = connect_default().await?;
    let snap = client.list_agents(&session).await?;
    if snap.agents.is_empty() {
        println!("(no sub-agents for this session)");
        return Ok(());
    }
    print_flat(&snap.agents);
    Ok(())
}

async fn tree(session: String) -> Result<()> {
    let mut client = connect_default().await?;
    let snap = client.list_agents(&session).await?;
    if snap.agents.is_empty() {
        println!("(no sub-agents for this session)");
        return Ok(());
    }
    print_tree(&snap.agents);
    Ok(())
}

async fn history(session: String) -> Result<()> {
    let mut client = connect_default().await?;
    let snaps = client.list_agent_snapshots(&session).await?;
    if snaps.snapshots.is_empty() {
        println!("(no snapshots for this session)");
        return Ok(());
    }
    println!("{:<14}  {:<20}  LABEL", "TURN_ID", "STARTED");
    for s in &snaps.snapshots {
        println!(
            "{:<14}  {:<20}  {}",
            short_or(&s.turn_id, 12),
            format_ts(s.started_at, super::TS_FMT),
            s.label,
        );
    }
    Ok(())
}

async fn snapshot(session: String, turn_id: String) -> Result<()> {
    let mut client = connect_default().await?;
    let snap = client.get_agent_snapshot(&session, &turn_id).await?;
    if snap.agents.is_empty() {
        println!("(snapshot is empty)");
        return Ok(());
    }
    print_tree(&snap.agents);
    Ok(())
}

async fn kill(id: String, subtree: bool) -> Result<()> {
    let mut client = connect_default().await?;
    if subtree {
        client.kill_agent_subtree(&id).await?;
        println!("killed subtree rooted at {id}");
    } else {
        client.kill_agent(&id).await?;
        println!("killed {id}");
    }
    Ok(())
}

async fn pause(id: String) -> Result<()> {
    let mut client = connect_default().await?;
    client.pause_agent(&id).await?;
    println!("paused {id}");
    Ok(())
}

async fn resume(id: String) -> Result<()> {
    let mut client = connect_default().await?;
    client.resume_agent(&id).await?;
    println!("resumed {id}");
    Ok(())
}

// -- rendering --

fn print_flat(agents: &[AgentEntry]) {
    println!(
        "{:<10}  {:<14}  {:<10}  {:<5}  {:<10}  {:<16}  METRICS",
        "ID", "NICKNAME", "ROLE", "DEPTH", "STATUS", "MODEL"
    );
    for a in agents {
        println!(
            "{:<10}  {:<14}  {:<10}  {:<5}  {:<10}  {:<16}  {}",
            short_or(&a.id, 8),
            super::truncate_str(&a.nickname, 14),
            super::truncate_str(&a.role, 10),
            a.depth,
            a.status,
            super::truncate_str(&a.model, 16),
            fmt_metrics(a.metrics.as_ref()),
        );
    }
}

fn print_tree(agents: &[AgentEntry]) {
    // Order by depth, then created_at if available — within a depth level,
    // children of the same parent appear together as a side-effect of the
    // depth-first ordering the daemon returns.
    for a in agents {
        let indent = "  ".repeat(a.depth as usize);
        let glyph = status_glyph(&a.status);
        let metrics = fmt_metrics(a.metrics.as_ref());
        println!(
            "{indent}{glyph} {nick} ({role}) [{status}]  {model}  {metrics}",
            nick = a.nickname,
            role = a.role,
            status = a.status,
            model = if a.model.is_empty() { "?" } else { &a.model },
        );
        if !a.id.is_empty() {
            println!("{indent}  id={}", a.id);
        }
        if !a.error.is_empty() {
            println!("{indent}  error: {}", a.error);
        }
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

fn fmt_metrics(m: Option<&AgentMetrics>) -> String {
    let Some(m) = m else {
        return String::new();
    };
    if m.input_tokens == 0 && m.output_tokens == 0 && m.tool_calls == 0 && m.cost_usd == 0.0 {
        return String::new();
    }
    format!(
        "in={} out={} tools={} ${:.4}",
        m.input_tokens, m.output_tokens, m.tool_calls, m.cost_usd
    )
}

fn short_or(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_string()
    } else {
        s[..n].to_string()
    }
}
