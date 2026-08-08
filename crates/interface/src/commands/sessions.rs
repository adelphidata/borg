use anyhow::{Context, Result};
use clap::Subcommand;

const DEFAULT_LIMIT: usize = 25;

#[derive(Subcommand)]
pub(crate) enum SessionsAction {
    /// List recent sessions
    List {
        /// Maximum entries to show
        #[arg(long, default_value_t = DEFAULT_LIMIT)]
        limit: usize,
        /// Show auto-archived sessions instead of active ones
        #[arg(long)]
        archived: bool,
    },
    /// Print a session's transcript (role + text)
    Show {
        /// Session ID (full UUID or unique prefix)
        id: String,
    },
    /// Protect a session from retention purging and stale auto-archiving
    Pin {
        /// Session ID (full UUID or unique prefix)
        id: String,
    },
    /// Remove a session's pin
    Unpin {
        /// Session ID (full UUID or unique prefix)
        id: String,
    },
    /// Bring an auto-archived session back into listings
    Unarchive {
        /// Session ID (full UUID or unique prefix)
        id: String,
    },
}

pub(crate) async fn dispatch_sessions(action: Option<SessionsAction>) -> Result<()> {
    match action {
        None => run_list(DEFAULT_LIMIT).await,
        Some(SessionsAction::List { limit, archived }) if archived => {
            run_list_archived(limit).await
        }
        Some(SessionsAction::List { limit, .. }) => run_list(limit).await,
        Some(SessionsAction::Show { id }) => run_show(&id).await,
        Some(SessionsAction::Pin { id }) => run_set_pinned(&id, true).await,
        Some(SessionsAction::Unpin { id }) => run_set_pinned(&id, false).await,
        Some(SessionsAction::Unarchive { id }) => run_unarchive(&id).await,
    }
}

/// `borg sessions pin|unpin <id>`.
///
/// Goes straight to the DB rather than through the daemon: this is operator
/// state about which conversations survive retention, and it must work whether
/// or not borgd is up.
async fn run_set_pinned(id: &str, pinned: bool) -> Result<()> {
    let db = cortex::db::Database::open().await?;
    let Some(full_id) = resolve_session_id_prefix(&db, id).await? else {
        println!("Session not found: {id}");
        return Ok(());
    };
    if !db.set_session_pinned(&full_id, pinned).await? {
        println!("Session not found: {id}");
        return Ok(());
    }
    let short = super::short_id(&full_id);
    if pinned {
        println!("Session {short} pinned — exempt from retention purge and auto-archive");
    } else {
        println!("Session {short} unpinned");
    }
    Ok(())
}

/// `borg sessions unarchive <id>`.
async fn run_unarchive(id: &str) -> Result<()> {
    let db = cortex::db::Database::open().await?;
    let Some(full_id) = resolve_session_id_prefix(&db, id).await? else {
        println!("Session not found: {id}");
        return Ok(());
    };
    // Resolution found the row, so a miss here means it was deleted in
    // between — report that rather than claiming a write that didn't land.
    if !db.unarchive_session(&full_id).await? {
        println!("Session not found: {id}");
        return Ok(());
    }
    println!("Session {} unarchived", super::short_id(&full_id));
    Ok(())
}

/// `borg sessions list --archived`.
///
/// The counterpart to the archived filter on the normal listing: without a way
/// to see what was archived, `sessions unarchive` would need an id the user has
/// no way to discover. Direct-to-DB for the same reason as pin/unpin.
async fn run_list_archived(limit: usize) -> Result<()> {
    let db = cortex::db::Database::open().await?;
    let archived: Vec<_> = db
        .list_sessions(1000)
        .await?
        .into_iter()
        .filter(|s| s.archived_at.is_some())
        .take(limit)
        .collect();
    if archived.is_empty() {
        println!("No archived sessions.");
        return Ok(());
    }
    println!("{:10} {:16}  {:16}  TITLE", "ID", "ARCHIVED", "UPDATED");
    for s in &archived {
        let archived_at = s
            .archived_at
            .map(|ts| super::format_ts(ts, super::TS_FMT))
            .unwrap_or_else(|| "?".to_string());
        println!(
            "{:10} {:16}  {:16}  {}",
            super::short_id(&s.id),
            archived_at,
            super::format_ts(s.updated_at, super::TS_FMT),
            s.title
        );
    }
    println!("\nRestore one with `borg sessions unarchive <id>`.");
    Ok(())
}

/// Resolve a session id prefix to the full id, or `None` when nothing matches.
async fn resolve_session_id_prefix(
    db: &cortex::db::Database,
    prefix: &str,
) -> Result<Option<String>> {
    super::resolve_unique_prefix(
        "sessions",
        prefix,
        db.session_ids_with_prefix(prefix).await?,
    )
}

async fn run_list(limit: usize) -> Result<()> {
    let mut client = crate::daemon_client::connect_default().await?;
    let list = client
        .list_sessions()
        .await
        .context("Admin.ListSessions via borgd")?;
    if list.sessions.is_empty() {
        println!("No sessions.");
        return Ok(());
    }
    println!("{:10} {:>6}  {:16}  TITLE", "ID", "MSGS", "UPDATED");
    for s in list.sessions.iter().take(limit) {
        let updated = chrono::DateTime::from_timestamp(s.updated_unix_ts as i64, 0)
            .map(|dt| dt.format(super::TS_FMT).to_string())
            .unwrap_or_else(|| "?".to_string());
        let short = super::short_id(&s.id);
        let pin = if s.pinned { " *" } else { "" };
        println!(
            "{:10} {:>6}  {:16}  {}{pin}",
            short, s.message_count, updated, s.title
        );
    }
    Ok(())
}

async fn run_show(id: &str) -> Result<()> {
    let mut client = crate::daemon_client::connect_default().await?;
    let detail = client
        .get_session(id)
        .await
        .context("Admin.GetSession via borgd")?;
    println!("Session: {} ({})", detail.title, detail.id);
    println!("Messages: {}", detail.messages.len());
    println!("──────────────");
    for m in &detail.messages {
        let when = chrono::DateTime::from_timestamp(m.unix_ts as i64, 0)
            .map(|dt| dt.format("%H:%M:%S").to_string())
            .unwrap_or_else(|| "?".to_string());
        println!("[{when}] {role}:", role = m.role);
        for line in m.text.lines() {
            println!("  {line}");
        }
    }
    Ok(())
}
