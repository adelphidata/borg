//! `borg providers` subcommands — list, add (BYO endpoint),
//! refresh-rates (signed manifest stub).

use anyhow::{anyhow, bail, Context, Result};
use clap::Subcommand;

use cortex::providers::{ProviderCategory, ProviderRow};

#[derive(Subcommand)]
pub(crate) enum ProvidersAction {
    /// List all configured BYO providers (built-ins are always
    /// available without a row).
    List,
    /// Add a BYO endpoint (OpenAI-compatible).
    Add {
        /// Stable identifier (lowercase, no whitespace; matches
        /// `provider_call_events.provider_id`).
        #[arg(long)]
        id: String,
        /// Human-readable label.
        #[arg(long)]
        display_name: Option<String>,
        /// Full chat-completions URL (e.g.
        /// `https://my-vllm.example.com/v1/chat/completions`).
        #[arg(long)]
        base_url: String,
        /// Environment variable name holding the API key. Omit for
        /// keyless local endpoints.
        #[arg(long)]
        api_key_env: Option<String>,
        /// Provider category. Defaults to `byo`. Local endpoints
        /// (RFC1918 / loopback / Tailscale) should set `--category local`
        /// so air-gap mode allows them.
        #[arg(long, default_value = "byo")]
        category: String,
    },
    /// Remove a BYO provider by id. Idempotent — succeeds even if the
    /// row is already absent. Built-ins (openai, anthropic, …) cannot be
    /// removed; they live in code, not in the providers table.
    Remove {
        /// Provider id to delete from the `providers` table.
        #[arg(long)]
        id: String,
    },
    /// Pull the latest published rate-card manifest from the Borg
    /// release host. Opt-in only — never invoked from a timer.
    ///
    /// The signed-manifest verification key is not yet shipped, so this
    /// command refuses to write any rows and exits non-zero. (Per
    /// CLAUDE.md "no silent error swallowing".)
    RefreshRates {
        /// Override the manifest URL (default: Borg release host).
        #[arg(long)]
        manifest_url: Option<String>,
    },
}

/// Dispatch `borg providers ...`.
pub(crate) fn dispatch(action: ProvidersAction) -> Result<()> {
    match action {
        ProvidersAction::List => list(),
        ProvidersAction::Add {
            id,
            display_name,
            base_url,
            api_key_env,
            category,
        } => add(id, display_name, base_url, api_key_env, &category),
        ProvidersAction::Remove { id } => remove(id),
        ProvidersAction::RefreshRates { manifest_url } => refresh_rates(manifest_url.as_deref()),
    }
}

fn remove(id: String) -> Result<()> {
    if id.is_empty() {
        bail!("--id must be non-empty");
    }
    tokio::runtime::Handle::current().block_on(async {
        let mut client = crate::daemon_client::connect_or_spawn().await?;
        client
            .remove_provider(&id)
            .await
            .with_context(|| format!("removing provider '{id}'"))
    })?;
    println!("Removed provider '{id}' (no-op if it wasn't present).");
    Ok(())
}

fn list() -> Result<()> {
    let db = cortex::db::Database::open_readonly()?;
    let rows = ProviderRow::list(db.conn())?;
    if rows.is_empty() {
        println!("No BYO providers configured.");
        println!("(Built-ins openai, anthropic, bedrock, gemini, deepseek, groq, openrouter, ollama, claude_cli are always available.)");
        return Ok(());
    }
    println!(
        "{:<20} {:<14} {:<40} {:<20}",
        "ID", "Category", "Endpoint", "API Key Env"
    );
    println!("{}", "─".repeat(96));
    for r in &rows {
        let cat = match r.category {
            ProviderCategory::PublicCloud => "public_cloud",
            ProviderCategory::Local => "local",
            ProviderCategory::BYOEndpoint => "byo",
        };
        let key_env = r.api_key_env.as_deref().unwrap_or("(none)");
        println!(
            "{:<20} {:<14} {:<40} {:<20}",
            r.id, cat, r.endpoint_url, key_env
        );
    }
    Ok(())
}

fn add(
    id: String,
    display_name: Option<String>,
    base_url: String,
    api_key_env: Option<String>,
    category_str: &str,
) -> Result<()> {
    if id.is_empty() || id.contains(char::is_whitespace) {
        bail!("--id must be non-empty and contain no whitespace");
    }
    // Validate locally before round-tripping so the user gets a fast error
    // for typos without needing to spawn the daemon.
    let _ = parse_category(category_str)?;
    let display_name = display_name.unwrap_or_else(|| id.clone());

    tokio::runtime::Handle::current().block_on(async {
        let mut client = crate::daemon_client::connect_or_spawn().await?;
        client
            .add_provider(
                &id,
                &display_name,
                &base_url,
                api_key_env.as_deref(),
                category_str,
            )
            .await
            .with_context(|| format!("inserting provider '{id}'"))
    })?;
    println!("Added provider '{id}'.");
    Ok(())
}

fn parse_category(s: &str) -> Result<ProviderCategory> {
    match s.to_ascii_lowercase().as_str() {
        "byo" | "byo_endpoint" => Ok(ProviderCategory::BYOEndpoint),
        "local" => Ok(ProviderCategory::Local),
        "public" | "public_cloud" => Ok(ProviderCategory::PublicCloud),
        other => Err(anyhow!(
            "unknown category '{other}'; expected one of: byo, local, public_cloud"
        )),
    }
}

fn refresh_rates(manifest_url: Option<&str>) -> Result<()> {
    let url = manifest_url.unwrap_or("https://borg.example/rates/manifest.json");
    tracing::warn!(
        manifest_url = %url,
        "manifest signature verification not yet implemented; \
         refusing to apply unverified rates"
    );
    eprintln!(
        "borg providers refresh-rates: signed-manifest verification is not \
         shipped yet — refusing to overwrite rate cards from an unverified source."
    );
    bail!("refresh-rates: verification stub, no rates applied");
}
