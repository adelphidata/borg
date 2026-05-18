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
}

pub(crate) async fn dispatch_sessions(action: Option<SessionsAction>) -> Result<()> {
    match action {
        None => run_list(DEFAULT_LIMIT).await,
        Some(SessionsAction::List { limit }) => run_list(limit).await,
        Some(SessionsAction::Show { id }) => run_show(&id).await,
    }
}

async fn connect() -> Result<crate::daemon_client::DaemonClient> {
    let socket = crate::daemon_client::default_socket_path();
    if !socket.exists() {
        anyhow::bail!("Could not reach daemon. Start it with: borg service start");
    }
    crate::daemon_client::connect(Some(&socket))
        .await
        .context("connect to borgd")
}

async fn run_list(limit: usize) -> Result<()> {
    let mut client = connect().await?;
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
    let mut client = connect().await?;
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
