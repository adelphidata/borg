use anyhow::Result;
use clap::Subcommand;

use super::format_ts;
use crate::async_bridge::block_on;
use crate::daemon_client::connect_or_spawn;

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
    let resp = block_on(async {
        let mut client = connect_or_spawn().await?;
        client.list_pairing_requests(channel).await
    })?;
    let requests = resp.requests;
    if requests.is_empty() {
        println!("No pending pairing requests.");
        return Ok(());
    }
    // `Code` column shows the SHA-256 hash truncated to a short prefix.
    // The DB no longer holds plaintext codes, so this column is only useful
    // as a stable identifier — operators identify pending requests by
    // sender_id / channel, and approve by typing the plaintext code the
    // sender presented.
    println!(
        "{:<12} {:<20} {:<10} {:<20}",
        "Channel", "Sender ID", "Hash", "Expires"
    );
    println!("{}", "─".repeat(64));
    for r in &requests {
        let expires = format_ts(r.expires_at as i64, "%Y-%m-%d %H:%M UTC");
        let hash_prefix = r.code.chars().take(8).collect::<String>();
        println!(
            "{:<12} {:<20} {:<10} {:<20}",
            r.channel_name, r.sender_id, hash_prefix, expires
        );
    }
    println!();
    println!("Approve with: borg pairing approve <code>");
    Ok(())
}

pub(crate) fn run_pairing_approve(code: &str) -> Result<()> {
    block_on(async {
        let mut client = connect_or_spawn().await?;

        // Resolve the channel: either the code carries a known prefix, or we
        // search the pending pairing list for a matching code.
        let channel_name = if let Some((channel, _)) = cortex::pairing::parse_prefixed_code(code) {
            channel.to_string()
        } else {
            // `r.code` is the SHA-256 hash of the lowercased pairing code;
            // hash the input the same way for the lookup.
            let target_hash = cortex::db::hash_pairing_code(&code.to_lowercase());
            let resp = client.list_pairing_requests(None).await?;
            let row = resp
                .requests
                .into_iter()
                .find(|r| r.code == target_hash && r.status == "pending")
                .ok_or_else(|| {
                    anyhow::anyhow!("No pending pairing request found for code '{code}'")
                })?;
            row.channel_name
        };

        client.approve_pairing(&channel_name, code).await?;

        // Look up the sender id from the now-approved row so we can deliver
        // a personalized greeting. Senders list contains it.
        let approved = client.list_approved_senders(Some(&channel_name)).await?;
        // Heuristic: most-recently-approved row for this channel.
        let sender_id = approved
            .senders
            .iter()
            .max_by_key(|s| s.approved_at)
            .map(|s| s.sender_id.clone());

        println!(
            "Approved {} sender {}.",
            channel_name,
            sender_id.as_deref().unwrap_or("(unknown)")
        );

        if let Some(sid) = sender_id {
            if let Err(e) = client.send_approval_greeting(&channel_name, &sid).await {
                tracing::warn!("Approval greeting RPC failed: {e}");
            }
        }
        Ok::<_, anyhow::Error>(())
    })?;

    Ok(())
}

pub(crate) fn run_pairing_revoke(channel: &str, sender_id: &str) -> Result<()> {
    let result = block_on(async {
        let mut client = connect_or_spawn().await?;
        client.revoke_approved_sender(channel, sender_id).await
    });
    match result {
        Ok(()) => {
            println!("Revoked {channel} sender {sender_id}.");
            Ok(())
        }
        Err(e) => {
            // not_found surfaces from the daemon as a tonic Status; surface
            // the same UX as the previous direct-DB path: a friendly message
            // rather than a stack-trace error.
            let msg = e.to_string();
            if msg.contains("not found") {
                println!("No approved sender found for {channel} with ID {sender_id}.");
                Ok(())
            } else {
                Err(e)
            }
        }
    }
}

pub(crate) fn run_pairing_approved(channel: Option<&str>) -> Result<()> {
    let resp = block_on(async {
        let mut client = connect_or_spawn().await?;
        client.list_approved_senders(channel).await
    })?;
    let senders = resp.senders;
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
        let approved = format_ts(s.approved_at as i64, "%Y-%m-%d %H:%M UTC");
        let name = if s.display_name.is_empty() {
            "—"
        } else {
            s.display_name.as_str()
        };
        println!(
            "{:<12} {:<20} {:<16} {:<20}",
            s.channel_name, s.sender_id, name, approved
        );
    }
    Ok(())
}
