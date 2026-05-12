//! `borg client-token` subcommand — manage bearer tokens for the
//! OpenAI-compat HTTP surface and MCP HTTP transport.

use anyhow::{Context, Result};
use clap::Subcommand;
use cortex::auth::tokens;
use cortex::db::Database;

use super::format_ts;

#[derive(Subcommand)]
pub(crate) enum ClientTokenAction {
    /// Issue a new bearer token. Prints the plaintext exactly once —
    /// copy it now, it cannot be recovered later.
    Issue {
        /// Human-readable label, unique per install (e.g. `chatbox-laptop`).
        label: String,
        /// Optional comma-separated scopes. Reserved for future use;
        /// v1 has no scope enforcement.
        #[arg(long, default_value = "")]
        scopes: String,
    },
    /// List existing tokens (live + revoked).
    List,
    /// Revoke a token by label. Idempotent.
    Revoke {
        /// Label of the token to revoke.
        label: String,
    },
}

/// Dispatch for `borg client-token …`.
pub(crate) fn dispatch_client_token(action: Option<ClientTokenAction>) -> Result<()> {
    match action {
        Some(ClientTokenAction::Issue { label, scopes }) => run_issue(&label, &scopes),
        Some(ClientTokenAction::List) | None => run_list(),
        Some(ClientTokenAction::Revoke { label }) => run_revoke(&label),
    }
}

fn run_issue(label: &str, scopes: &str) -> Result<()> {
    let issued = tokio::runtime::Handle::current().block_on(async {
        let mut client = crate::daemon_client::connect_or_spawn().await?;
        client
            .issue_client_token(label, scopes)
            .await
            .with_context(|| format!("issuing client token '{label}'"))
    })?;
    println!("Issued client token `{}` (id {}).", issued.label, issued.id);
    println!();
    println!("    {}", issued.plaintext);
    println!();
    println!(
        "Copy it now — the plaintext is not stored and cannot be recovered. \
         Use it as `Authorization: Bearer <token>` against the OpenAI-compat \
         and MCP HTTP endpoints."
    );
    Ok(())
}

fn run_list() -> Result<()> {
    let db = Database::open_readonly()?;
    let rows = tokens::list(&db)?;
    if rows.is_empty() {
        println!("No client tokens issued.");
        return Ok(());
    }
    println!(
        "{:>4}  {:24}  {:8}  {:19}  {:19}  {:9}",
        "ID", "LABEL", "STATUS", "CREATED", "LAST USED", "SCOPES"
    );
    for r in rows {
        let status = if r.revoked_at.is_some() {
            "revoked"
        } else {
            "live"
        };
        let last_used = r
            .last_used_at
            .map(|ts| format_ts(ts, "%Y-%m-%d %H:%M:%S"))
            .unwrap_or_else(|| "—".to_string());
        println!(
            "{:>4}  {:24}  {:8}  {:19}  {:19}  {}",
            r.id,
            truncate(&r.label, 24),
            status,
            format_ts(r.created_at, "%Y-%m-%d %H:%M:%S"),
            last_used,
            r.scopes,
        );
    }
    Ok(())
}

fn run_revoke(label: &str) -> Result<()> {
    let row = tokio::runtime::Handle::current().block_on(async {
        let mut client = crate::daemon_client::connect_or_spawn().await?;
        client
            .revoke_client_token(label)
            .await
            .with_context(|| format!("revoking client token '{label}'"))
    })?;
    let ts = if row.revoked_at == 0 {
        "?".to_string()
    } else {
        format_ts(row.revoked_at as i64, "%Y-%m-%d %H:%M:%S")
    };
    println!("Revoked `{}` at {}.", row.label, ts);
    Ok(())
}

fn truncate(s: &str, max: usize) -> String {
    cortex::util::truncate::pad_or_truncate(s, max)
}
