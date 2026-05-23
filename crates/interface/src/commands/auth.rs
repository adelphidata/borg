//! `borg auth …` — manage the per-provider credential pool.
//!
//! Backs the `LLM resilience` page (`docs/llm-resilience.md`). Every operation
//! is a single gRPC call against the running daemon; persisted state lives in
//! the `credential_pool` + `credential_suppressions` SQLite tables.

use anyhow::{Context, Result};
use clap::Subcommand;
use protocols::admin::CredentialEntry;

use super::format_ts;

#[derive(Subcommand)]
pub(crate) enum AuthAction {
    /// List configured credentials, optionally filtered by provider.
    List {
        /// Provider name (e.g. `openrouter`). Empty = all configured pools.
        provider: Option<String>,
    },
    /// Add a credential to a provider's pool. The key is sent over the
    /// daemon's mTLS channel; only its 16-hex fingerprint is echoed back.
    Add {
        /// Provider name (e.g. `openrouter`).
        provider: String,
        /// Plaintext API key. Pass via `--key -` to read one line from stdin.
        #[arg(long)]
        key: String,
        /// Optional operator-friendly label (defaults to `manual-<N>`).
        #[arg(long)]
        label: Option<String>,
    },
    /// Remove a credential by 1-based index, label, or id (4+ hex prefix).
    /// Writes a suppression row so re-seeding from env/config won't undo it.
    Remove {
        /// Provider name (e.g. `openrouter`).
        provider: String,
        /// Target — numeric index, exact label, or 4+ hex id prefix.
        target: String,
    },
    /// Clear all cooldowns. Does **not** touch permanently-removed entries.
    Reset {
        /// Provider name (e.g. `openrouter`).
        provider: String,
    },
    /// Change the selection strategy for a provider's pool.
    SetStrategy {
        /// Provider name (e.g. `openrouter`).
        provider: String,
        /// One of `fill_first`, `round_robin`, `least_used`, `random`.
        strategy: String,
    },
    /// Pair an OAuth-capable account into the credential pool. Discovers
    /// credentials from the matching on-disk store (Claude Code, Qwen-CLI,
    /// or the OpenAI OAuth CLI) and seeds them under the corresponding
    /// provider's pool.
    Pair {
        /// One of `claude-code | codex | qwen`.
        kind: String,
    },
}

/// Dispatch for `borg auth …`.
pub(crate) fn dispatch_auth(action: Option<AuthAction>) -> Result<()> {
    match action {
        None | Some(AuthAction::List { provider: None }) => run_list(None),
        Some(AuthAction::List { provider }) => run_list(provider.as_deref()),
        Some(AuthAction::Add {
            provider,
            key,
            label,
        }) => run_add(&provider, &key, label.as_deref()),
        Some(AuthAction::Remove { provider, target }) => run_remove(&provider, &target),
        Some(AuthAction::Reset { provider }) => run_reset(&provider),
        Some(AuthAction::SetStrategy { provider, strategy }) => {
            run_set_strategy(&provider, &strategy)
        }
        Some(AuthAction::Pair { kind }) => run_pair(&kind),
    }
}

fn run_pair(kind: &str) -> Result<()> {
    let entries = tokio::runtime::Handle::current().block_on(async {
        let mut client = crate::daemon_client::connect_or_spawn().await?;
        client
            .pair_credential(kind)
            .await
            .with_context(|| format!("pairing OAuth credential `{kind}`"))
    })?;
    if entries.is_empty() {
        println!(
            "No credentials discovered for `{kind}`. Make sure the upstream CLI \
             (e.g. `claude /login`, `codex`, `gh auth login`) has been run."
        );
        return Ok(());
    }
    for e in &entries {
        println!(
            "Paired `{}` (fingerprint {}, provider {}).",
            e.label, e.fingerprint, e.provider
        );
    }
    Ok(())
}

fn run_list(provider: Option<&str>) -> Result<()> {
    let list = tokio::runtime::Handle::current().block_on(async {
        let mut client = crate::daemon_client::connect_or_spawn().await?;
        client
            .list_credentials(provider)
            .await
            .context("listing credentials")
    })?;
    if list.providers.is_empty() {
        if let Some(p) = provider {
            println!("No configured credential pool for provider `{p}`.");
        } else {
            println!("No configured credential pools.");
        }
        return Ok(());
    }
    for (i, pp) in list.providers.iter().enumerate() {
        if i > 0 {
            println!();
        }
        println!("provider: {}", pp.provider);
        println!(
            "  strategy: {}    cooldown: {}s    max_concurrent_per_key: {}",
            pp.strategy, pp.cooldown_secs, pp.max_concurrent_per_credential
        );
        if pp.entries.is_empty() {
            println!("  (no credentials)");
            continue;
        }
        println!(
            "  {:>3}  {:8}  {:24}  {:9}  {:24}  DETAIL",
            "#", "ID", "LABEL", "STATUS", "SOURCE"
        );
        for (idx, e) in pp.entries.iter().enumerate() {
            println!(
                "  {:>3}  {:8}  {:24}  {:9}  {:24}  {}",
                idx + 1,
                truncate(&e.id, 8),
                truncate(&e.label, 24),
                e.status,
                truncate(&format_source(e), 24),
                detail_line(e),
            );
        }
    }
    Ok(())
}

fn run_add(provider: &str, key: &str, label: Option<&str>) -> Result<()> {
    let key_owned = if key == "-" {
        let mut s = String::new();
        std::io::stdin()
            .read_line(&mut s)
            .context("reading key from stdin")?;
        let trimmed = s.trim().to_string();
        if trimmed.is_empty() {
            anyhow::bail!("stdin produced an empty key");
        }
        trimmed
    } else {
        key.to_string()
    };
    let entry = tokio::runtime::Handle::current().block_on(async {
        let mut client = crate::daemon_client::connect_or_spawn().await?;
        client
            .add_credential(provider, &key_owned, label)
            .await
            .with_context(|| format!("adding credential for provider `{provider}`"))
    })?;
    println!(
        "Added credential `{}` (fingerprint {}) to provider `{}`.",
        entry.label, entry.fingerprint, entry.provider
    );
    Ok(())
}

fn run_remove(provider: &str, target: &str) -> Result<()> {
    let entry = tokio::runtime::Handle::current().block_on(async {
        let mut client = crate::daemon_client::connect_or_spawn().await?;
        client
            .remove_credential(provider, target)
            .await
            .with_context(|| format!("removing credential `{target}` from `{provider}`"))
    })?;
    println!(
        "Removed credential `{}` (id {}) from provider `{}`. Suppression row written — \
         re-seeding from env/config will not undo this.",
        entry.label, entry.id, entry.provider
    );
    Ok(())
}

fn run_reset(provider: &str) -> Result<()> {
    let cleared = tokio::runtime::Handle::current().block_on(async {
        let mut client = crate::daemon_client::connect_or_spawn().await?;
        client
            .reset_credentials(provider)
            .await
            .with_context(|| format!("resetting credential cooldowns for `{provider}`"))
    })?;
    if cleared == 0 {
        println!("No cooldowns to clear for provider `{provider}`.");
    } else {
        println!("Cleared {cleared} cooldown(s) for provider `{provider}`.");
    }
    Ok(())
}

fn run_set_strategy(provider: &str, strategy: &str) -> Result<()> {
    tokio::runtime::Handle::current().block_on(async {
        let mut client = crate::daemon_client::connect_or_spawn().await?;
        client
            .set_credential_strategy(provider, strategy)
            .await
            .with_context(|| format!("setting strategy `{strategy}` for `{provider}`"))
    })?;
    println!("Strategy for provider `{provider}` is now `{strategy}`.");
    Ok(())
}

fn format_source(e: &CredentialEntry) -> String {
    match e.source_kind.as_str() {
        "env" => format!("env:{}", e.source_value),
        "config" => format!("config:{}", e.source_value),
        "oauth" => format!("oauth:{}", e.source_value),
        "custom" => format!("custom:{}", e.source_value),
        "manual" => "manual".to_string(),
        other => other.to_string(),
    }
}

fn detail_line(e: &CredentialEntry) -> String {
    let mut parts = Vec::new();
    if e.use_count > 0 {
        parts.push(format!("used {}×", e.use_count));
    }
    if e.last_status_at_ms > 0 {
        parts.push(format!(
            "seen {}",
            format_ts(e.last_status_at_ms / 1000, "%Y-%m-%d")
        ));
    }
    if e.status == "exhausted" && e.last_error_reset_at_ms > 0 {
        parts.push(format!(
            "cools until {}",
            format_ts(e.last_error_reset_at_ms / 1000, "%H:%M:%S")
        ));
    }
    if e.last_error_code > 0 {
        let reason = if e.last_error_reason.is_empty() {
            "?".to_string()
        } else {
            e.last_error_reason.clone()
        };
        parts.push(format!("{} {}", e.last_error_code, reason));
    }
    if e.removed_at_ms > 0 {
        parts.push(format!(
            "removed {}",
            format_ts(e.removed_at_ms / 1000, "%Y-%m-%d")
        ));
    }
    parts.join("; ")
}

fn truncate(s: &str, max: usize) -> String {
    cortex::util::truncate::pad_or_truncate(s, max)
}
