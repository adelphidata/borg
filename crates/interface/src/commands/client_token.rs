//! `borg client-token` subcommand — manage bearer tokens for the
//! OpenAI-compat HTTP surface and MCP HTTP transport.

use anyhow::Result;
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
    let db = Database::open()?;
    let issued = tokens::issue(&db, label, scopes)?;
    println!(
        "Issued client token `{}` (id {}).",
        issued.row.label, issued.row.id
    );
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
    let db = Database::open()?;
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
    let db = Database::open()?;
    let row = tokens::revoke_by_label(&db, label)?;
    let ts = row
        .revoked_at
        .map(|t| format_ts(t, "%Y-%m-%d %H:%M:%S"))
        .unwrap_or_else(|| "?".to_string());
    println!("Revoked `{}` at {}.", row.label, ts);
    Ok(())
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return format!("{s:<max$}");
    }
    let mut end = max - 1;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}
