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
        Some(SessionsAction::List { limit }) => run_list(limit).await,
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
    db.set_session_pinned(&full_id, pinned).await?;
    let short: String = full_id.chars().take(8).collect();
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
    db.unarchive_session(&full_id).await?;
    let short: String = full_id.chars().take(8).collect();
    println!("Session {short} unarchived");
    Ok(())
}

/// Resolve a session id prefix to the full id, or `None` when nothing matches.
///
/// An ambiguous prefix is an error rather than a pick: pinning or unpinning
/// the wrong session is invisible until the right one has already been purged.
async fn resolve_session_id_prefix(
    db: &cortex::db::Database,
    prefix: &str,
) -> Result<Option<String>> {
    let matches = db.session_ids_with_prefix(prefix).await?;
    match matches.len() {
        0 => Ok(None),
        1 => Ok(matches.into_iter().next()),
        n => anyhow::bail!("'{prefix}' matches {n} sessions — use a longer id prefix"),
    }
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
        let short: String = s.id.chars().take(8).collect();
        println!(
            "{:10} {:>6}  {:16}  {}",
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
