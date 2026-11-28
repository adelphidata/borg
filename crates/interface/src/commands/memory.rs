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
    super::print_lines([content.strip_suffix('\n').unwrap_or(&content)])
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
            let config = cortex::config::Config::load_from_db().await?;
            let home = cortex::tool_handlers::filesystem::control_plane_home();
            read_memory_source(&p, &config.security, home.as_deref())?
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

/// Read the file behind `borg memory write --file`.
///
/// It passes the same read gate as `read_file`, so the agent can't shell out
/// to `borg memory write --file ...` and copy `.ssh/`, `.env`, or Borg's own
/// key and DB files (those under `borg_home`) into long-term memory. The gate
/// judges an absolute path: a relative one would otherwise be compared as
/// written, from whatever directory the command ran in.
fn read_memory_source(
    path: &std::path::Path,
    security: &cortex::config::SecurityConfig,
    borg_home: Option<&std::path::Path>,
) -> Result<String> {
    let absolute = std::path::absolute(path).with_context(|| format!("read {}", path.display()))?;
    if cortex::tool_handlers::filesystem::is_read_denied_under(
        &absolute,
        &security.blocked_paths,
        &security.allowed_paths,
        borg_home,
    ) {
        return Err(anyhow!(
            "{} is a blocked path and cannot be saved to memory",
            path.display()
        ));
    }
    std::fs::read_to_string(&absolute).with_context(|| format!("read {}", path.display()))
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

    /// `--file` passes the read gate on the absolute path: a blocked file is
    /// refused however it is spelled, an ordinary one is read.
    #[test]
    fn memory_source_file_passes_the_read_gate() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir(root.join(".ssh")).unwrap();
        std::fs::write(root.join(".ssh/id_rsa"), "PRIVATE KEY").unwrap();
        std::fs::write(root.join("notes.md"), "plain notes").unwrap();
        let security = cortex::config::SecurityConfig {
            blocked_paths: vec![".ssh".to_string()],
            allowed_paths: Vec::new(),
            ..Default::default()
        };

        std::fs::write(root.join("hooks.json"), "HOOKS").unwrap();
        let read = |rel: &str| read_memory_source(&root.join(rel), &security, Some(&root));

        // The operator's blocklist, and Borg's own files under the data dir.
        for blocked in ["notes.md/../.ssh/id_rsa", "hooks.json"] {
            let refused = read(blocked).expect_err("a blocked file");
            assert!(
                refused.to_string().contains("is a blocked path"),
                "{blocked}: {refused:#}"
            );
        }
        assert_eq!(read("notes.md").unwrap(), "plain notes");
        // The same file outside any data dir is an ordinary file.
        assert_eq!(
            read_memory_source(&root.join("hooks.json"), &security, None).unwrap(),
            "HOOKS"
        );
    }
}
