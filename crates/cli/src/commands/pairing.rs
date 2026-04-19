use anyhow::Result;
use clap::Subcommand;

use super::format_ts;

#[derive(Subcommand)]
pub(crate) enum PairingAction {
    /// List pending pairing requests
    List {
        /// Filter by channel name
        channel: Option<String>,
    },
    /// Approve a pairing request by code
    Approve {
        /// Pairing code with channel prefix (e.g. TG_H4BRWMRW)
        code: String,
    },
    /// Revoke an approved sender
    Revoke {
        /// Channel name
        channel: String,
        /// Sender ID to revoke
        sender_id: String,
    },
    /// List all approved senders
    Approved {
        /// Filter by channel name
        channel: Option<String>,
    },
}

/// Dispatch for `borg pairing ...`.
pub(crate) fn dispatch_pairing(action: Option<PairingAction>) -> Result<()> {
    match action {
        Some(PairingAction::List { channel }) => run_pairing_list(channel.as_deref()),
        Some(PairingAction::Approve { code }) => run_pairing_approve(&code),
        Some(PairingAction::Revoke { channel, sender_id }) => {
            run_pairing_revoke(&channel, &sender_id)
        }
        Some(PairingAction::Approved { channel }) => run_pairing_approved(channel.as_deref()),
        None => run_pairing_list(None),
    }
}

pub(crate) fn run_pairing_list(channel: Option<&str>) -> Result<()> {
    let db = borg_core::db::Database::open()?;
    let requests = db.list_pairings(channel)?;
    if requests.is_empty() {
        println!("No pending pairing requests.");
        return Ok(());
    }
    println!(
        "{:<12} {:<20} {:<10} {:<20}",
        "Channel", "Sender ID", "Code", "Expires"
    );
    println!("{}", "─".repeat(64));
    for r in &requests {
        let expires = format_ts(r.expires_at, "%Y-%m-%d %H:%M UTC");
        println!(
            "{:<12} {:<20} {:<10} {:<20}",
            r.channel_name, r.sender_id, r.code, expires
        );
    }
    println!();
    println!("Approve with: borg pairing approve <code>");
    Ok(())
}

pub(crate) fn run_pairing_approve(code: &str) -> Result<()> {
    let db = borg_core::db::Database::open()?;

    let (channel_name, request) =
        if let Some((channel, _)) = borg_core::pairing::parse_prefixed_code(code) {
            let req = db.approve_pairing(channel, code)?;
            (channel.to_string(), req)
        } else {
            match db.find_pending_by_code(code)? {
                Some(row) => {
                    let ch = row.channel_name;
                    let req = db.approve_pairing(&ch, code)?;
                    (ch, req)
                }
                None => anyhow::bail!("No pending pairing request found for code '{code}'"),
            }
        };

    println!(
        "Approved {} sender {}.",
        request.channel_name, request.sender_id
    );

    let config = match borg_core::config::Config::load_from_db() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("Failed to load config for approval greeting: {e}");
            borg_core::config::Config::default()
        }
    };
    let sid = request.sender_id;
    let ch = channel_name;
    tokio::runtime::Handle::current().block_on(async {
        crate::service::send_approval_greeting(&config, &ch, &sid).await;
    });

    Ok(())
}

pub(crate) fn run_pairing_revoke(channel: &str, sender_id: &str) -> Result<()> {
    let db = borg_core::db::Database::open()?;
    if db.revoke_sender(channel, sender_id)? {
        println!("Revoked {channel} sender {sender_id}.");
    } else {
        println!("No approved sender found for {channel} with ID {sender_id}.");
    }
    Ok(())
}

pub(crate) fn run_pairing_approved(channel: Option<&str>) -> Result<()> {
    let db = borg_core::db::Database::open()?;
    let senders = db.list_approved_senders(channel)?;
    if senders.is_empty() {
        println!("No approved senders.");
        return Ok(());
    }
    println!(
        "{:<12} {:<20} {:<16} {:<20}",
        "Channel", "Sender ID", "Display Name", "Approved At"
    );
    println!("{}", "─".repeat(70));
    for s in &senders {
        let approved = format_ts(s.approved_at, "%Y-%m-%d %H:%M UTC");
        let name = s.display_name.as_deref().unwrap_or("—");
        println!(
            "{:<12} {:<20} {:<16} {:<20}",
            s.channel_name, s.sender_id, name, approved
        );
    }
    Ok(())
}
