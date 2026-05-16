use anyhow::{Context, Result};
use clap::Subcommand;
use cortex::constants::SECS_PER_DAY;

#[derive(Subcommand)]
pub(crate) enum DoctorAction {
    /// List active squelched findings with expiry
    Squelches,
    /// Silence a doctor finding for `--ttl-days` days (default 30)
    Squelch {
        /// Finding category (e.g. "Host Security")
        category: String,
        /// Finding name (e.g. "Application Firewall")
        name: String,
        /// Squelch duration in days
        #[arg(long, default_value_t = 30)]
        ttl_days: i64,
        /// Optional reason recorded with the squelch
        #[arg(long)]
        reason: Option<String>,
    },
    /// Remove a squelch immediately
    Unsquelch {
        /// Finding category
        category: String,
        /// Finding name
        name: String,
    },
}

pub(crate) async fn dispatch_doctor(action: Option<DoctorAction>) -> Result<()> {
    match action {
        None => super::misc::run_doctor().await,
        Some(DoctorAction::Squelches) => run_list().await,
        Some(DoctorAction::Squelch {
            category,
            name,
            ttl_days,
            reason,
        }) => run_squelch(&category, &name, ttl_days, reason.as_deref()).await,
        Some(DoctorAction::Unsquelch { category, name }) => run_unsquelch(&category, &name).await,
    }
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

async fn run_list() -> Result<()> {
    let db = cortex::db::Database::open_readonly().await?;
    let rows = db.doctor_squelch_list_active(now_unix()).await?;
    if rows.is_empty() {
        println!("No active squelches.");
        return Ok(());
    }
    println!("{:24} {:32} {:16}  REASON", "CATEGORY", "NAME", "EXPIRES");
    for r in rows {
        let expires = chrono::DateTime::from_timestamp(r.expires_at, 0)
            .map(|dt| dt.format("%Y-%m-%d %H:%M").to_string())
            .unwrap_or_else(|| "?".to_string());
        println!(
            "{:24} {:32} {:16}  {}",
            r.category,
            r.name,
            expires,
            r.reason.as_deref().unwrap_or("")
        );
    }
    Ok(())
}

async fn run_squelch(
    category: &str,
    name: &str,
    ttl_days: i64,
    reason: Option<&str>,
) -> Result<()> {
    let now = now_unix();
    let expires_at = now + ttl_days * SECS_PER_DAY;
    let mut client = crate::daemon_client::connect_or_spawn().await?;
    client
        .doctor_squelch_set(category, name, expires_at, reason)
        .await
        .with_context(|| format!("squelching {category} / {name}"))?;
    let when = chrono::DateTime::from_timestamp(expires_at, 0)
        .map(|dt| dt.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|| "?".to_string());
    println!("Squelched {category} / {name} until {when}");
    Ok(())
}

async fn run_unsquelch(category: &str, name: &str) -> Result<()> {
    let mut client = crate::daemon_client::connect_or_spawn().await?;
    let removed = client
        .doctor_squelch_clear(category, name)
        .await
        .with_context(|| format!("clearing squelch for {category} / {name}"))?;
    if removed {
        println!("Cleared squelch for {category} / {name}");
    } else {
        println!("No active squelch for {category} / {name}");
    }
    Ok(())
}
