//! `borg webhook` subcommand — manage signed webhook definitions.

use anyhow::{Context, Result};
use clap::Subcommand;
use cortex::db::Database;
use cortex::util::hex::hex_encode;
use gateway::generic_webhook::definitions;

use super::format_ts;

#[derive(Subcommand)]
pub(crate) enum WebhookAction {
    /// Define a new signed webhook. Prints the HMAC-key plaintext
    /// once — copy it now, it cannot be recovered later.
    Create {
        /// Unique name. Appears in the URL: `/v1/webhook/{name}`.
        name: String,
        /// JSON dot-path to extract `sender_id` (e.g. `$.user.id`).
        #[arg(long)]
        sender: String,
        /// JSON dot-path to extract `body` text (e.g. `$.text`).
        #[arg(long)]
        body: String,
        /// JSON dot-path to extract `thread_id` (optional).
        #[arg(long)]
        thread: Option<String>,
    },
    /// List all webhook definitions (live + revoked).
    List,
    /// Revoke a webhook definition by name. Idempotent.
    Revoke {
        /// Name of the webhook to revoke.
        name: String,
    },
}

/// Dispatch for `borg webhook …`.
pub(crate) async fn dispatch_webhook(action: Option<WebhookAction>) -> Result<()> {
    match action {
        Some(WebhookAction::Create {
            name,
            sender,
            body,
            thread,
        }) => run_create(&name, &sender, thread.as_deref(), &body).await,
        Some(WebhookAction::List) | None => run_list().await,
        Some(WebhookAction::Revoke { name }) => run_revoke(&name).await,
    }
}

async fn run_create(
    name: &str,
    sender_path: &str,
    thread_path: Option<&str>,
    body_path: &str,
) -> Result<()> {
    let mut client = crate::daemon_client::connect_or_spawn().await?;
    let resp = client
        .create_webhook(name, sender_path, thread_path, body_path)
        .await
        .with_context(|| format!("creating webhook '{name}'"))?;
    println!(
        "Created webhook `{}` at /v1/webhook/{}",
        resp.name, resp.name
    );
    println!();
    println!("HMAC key (hex): {}", hex_encode(&resp.hmac_secret));
    println!();
    println!(
        "Sign requests with: HMAC-SHA256(<key>, \"{{ts}}.{{body}}\")\n\
         Headers: X-Borg-Signature: v1=<hex>, X-Borg-Timestamp: <unix-seconds>\n\
         Replay window: 5 minutes."
    );
    Ok(())
}

async fn run_list() -> Result<()> {
    let db = Database::open_readonly().await?;
    let rows = definitions::list(&db).await?;
    if rows.is_empty() {
        println!("No webhook definitions.");
        return Ok(());
    }
    println!(
        "{:24}  {:8}  {:19}  {:32}  {:32}  THREAD PATH",
        "NAME", "STATUS", "CREATED", "SENDER PATH", "BODY PATH"
    );
    for r in rows {
        let status = if r.revoked_at.is_some() {
            "revoked"
        } else {
            "live"
        };
        println!(
            "{:24}  {:8}  {:19}  {:32}  {:32}  {}",
            truncate(&r.name, 24),
            status,
            format_ts(r.created_at, "%Y-%m-%d %H:%M:%S"),
            truncate(&r.sender_path, 32),
            truncate(&r.body_path, 32),
            r.thread_path.as_deref().unwrap_or("—"),
        );
    }
    Ok(())
}

async fn run_revoke(name: &str) -> Result<()> {
    let mut client = crate::daemon_client::connect_or_spawn().await?;
    client
        .revoke_webhook(name)
        .await
        .with_context(|| format!("revoking webhook '{name}'"))?;
    println!("Revoked webhook `{name}`.");
    Ok(())
}

fn truncate(s: &str, max: usize) -> String {
    cortex::util::truncate::pad_or_truncate(s, max)
}
