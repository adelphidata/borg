use anyhow::{anyhow, Context, Result};
use clap::Subcommand;

#[derive(Subcommand)]
pub(crate) enum MemoryAction {
    /// List memory entries (default scope: global; --scope all lists every scope)
    List {
        /// Memory scope (e.g. global, project:abc). Use "all" for every scope.
        #[arg(long, short, default_value = "global")]
        scope: String,
    },
    /// Read a memory entry's full content
    Read {
        /// Memory scope (e.g. global, project:abc)
        scope: String,
        /// Entry name (without .md)
        name: String,
    },
    /// Write a memory entry (overwrite by default)
    Write {
        /// Memory scope
        scope: String,
        /// Entry name (without .md)
        name: String,
        /// Content (mutually exclusive with --file)
        #[arg(long, conflicts_with = "file")]
        content: Option<String>,
        /// Read content from a file (mutually exclusive with --content)
        #[arg(long)]
        file: Option<std::path::PathBuf>,
        /// Append instead of overwrite
        #[arg(long)]
        append: bool,
    },
    /// Full-text search a scope's memory chunks
    Search {
        /// Query string
        query: String,
        /// Memory scope
        #[arg(long, short, default_value = "global")]
        scope: String,
        /// Max results
        #[arg(long, default_value_t = 10)]
        top_k: usize,
    },
    /// Delete a memory entry
    Delete {
        /// Memory scope
        scope: String,
        /// Entry name (without .md)
        name: String,
    },
}

pub(crate) async fn dispatch_memory(action: MemoryAction) -> Result<()> {
    match action {
        MemoryAction::List { scope } => run_list(&scope).await,
        MemoryAction::Read { scope, name } => run_read(&scope, &name).await,
        MemoryAction::Write {
            scope,
            name,
            content,
            file,
            append,
        } => run_write(&scope, &name, content, file, append).await,
        MemoryAction::Search {
            query,
            scope,
            top_k,
        } => run_search(&query, &scope, top_k).await,
        MemoryAction::Delete { scope, name } => run_delete(&scope, &name).await,
    }
}

async fn run_list(scope: &str) -> Result<()> {
    if scope == "all" {
        let db = cortex::db::Database::open_readonly().await?;
        let scopes = db.list_memory_scopes().await?;
        if scopes.is_empty() {
            println!("No memory entries.");
            return Ok(());
        }
        for s in scopes {
            print_scope_listing(&s).await?;
        }
    } else {
        print_scope_listing(scope).await?;
    }
    Ok(())
}

async fn print_scope_listing(scope: &str) -> Result<()> {
    let entries = cortex::memory::list_memory_entries(scope).await?;
    println!("=== {scope} ({} entries) ===", entries.len());
    if entries.is_empty() {
        return Ok(());
    }
    println!("{:32} {:>8}  UPDATED", "NAME", "BYTES");
    for e in &entries {
        let updated = chrono::DateTime::from_timestamp(e.updated_at, 0)
            .map(|dt| dt.format(super::TS_FMT).to_string())
            .unwrap_or_else(|| "?".to_string());
        println!("{:32} {:>8}  {}", e.name, e.content.len(), updated);
    }
    Ok(())
}

async fn run_read(scope: &str, name: &str) -> Result<()> {
    let name = name.strip_suffix(".md").unwrap_or(name);
    let content = cortex::memory::read_memory_db(name, scope)
        .await?
        .ok_or_else(|| anyhow!("memory entry '{scope}/{name}' not found"))?;
    print!("{content}");
    Ok(())
}

async fn run_write(
    scope: &str,
    name: &str,
    content: Option<String>,
    file: Option<std::path::PathBuf>,
    append: bool,
) -> Result<()> {
    let name = name.strip_suffix(".md").unwrap_or(name);
    let body = match (content, file) {
        (Some(c), None) => c,
        (None, Some(p)) => {
            // Reuse the same blocked-paths filter that gates fs_read/fs_write
            // so the agent can't shell out to `borg memory write --file ...`
            // and exfiltrate `.ssh/`, `.env`, etc. into long-term memory.
            let config = cortex::config::Config::load_from_db().await?;
            if cortex::tool_handlers::filesystem::is_blocked_path(
                &p,
                &config.security.blocked_paths,
                &config.security.allowed_paths,
            ) {
                return Err(anyhow!(
                    "{} is blocked by [security] blocked_paths",
                    p.display()
                ));
            }
            std::fs::read_to_string(&p).with_context(|| format!("read {}", p.display()))?
        }
        (Some(_), Some(_)) => return Err(anyhow!("--content and --file are mutually exclusive")),
        (None, None) => return Err(anyhow!("provide --content <text> or --file <path>")),
    };
    let mode = if append {
        cortex::memory::WriteMode::Append
    } else {
        cortex::memory::WriteMode::Overwrite
    };
    let msg = cortex::memory::write_memory_db(name, &body, mode, scope).await?;
    // Index it now, as the agent's memory tool does: `borg memory search`
    // reads the chunk index, so an unindexed entry is unfindable until the
    // nightly repair sweep.
    let config = cortex::config::Config::load_from_db().await?;
    if config.memory.embeddings.enabled {
        cortex::memory::index_memory_entry(&config, name, scope).await;
    }
    println!("{msg}");
    Ok(())
}

async fn run_search(query: &str, scope: &str, top_k: usize) -> Result<()> {
    let db = cortex::db::Database::open_readonly().await?;
    let hits = db.fts_search(scope, query, top_k).await?;
    if hits.is_empty() {
        println!("No matches in scope '{scope}'.");
        return Ok(());
    }
    for (chunk, score) in &hits {
        let lines = match (chunk.start_line, chunk.end_line) {
            (Some(s), Some(e)) => format!(":{s}-{e}"),
            _ => String::new(),
        };
        println!(
            "[{score:.3}] {scope}/{name}{lines}",
            scope = chunk.scope,
            name = chunk.filename
        );
        let snippet = chunk.content.chars().take(200).collect::<String>();
        let snippet = snippet.replace('\n', " ");
        println!("    {snippet}");
    }
    Ok(())
}

async fn run_delete(scope: &str, name: &str) -> Result<()> {
    let name = name.strip_suffix(".md").unwrap_or(name);
    let removed = cortex::memory::delete_memory_db(name, scope).await?;
    if removed {
        println!("Deleted {scope}/{name}");
    } else {
        println!("No entry {scope}/{name} (nothing deleted).");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn write_without_content_or_file_errors() {
        // Both `--content` and `--file` omitted should fail loud rather than
        // silently writing an empty memory entry. clap allows both to be None
        // (they're individually optional); the runtime check is the only
        // guard, so we pin it.
        let err = run_write("global", "test-entry", None, None, false)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("--content"));
    }
}
