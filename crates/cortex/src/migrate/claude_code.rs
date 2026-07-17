//! Parser for Claude Code data directories (`~/.claude`).
//!
//! Memory-only: this tool's config and credentials have no Borg equivalent
//! worth importing, so `config`/`credentials`/`persona`/`skills` are no-ops.
//!
//! Two shapes are imported:
//! - `CLAUDE.md` — global instructions, imported as global memory.
//! - `projects/<key>/memory/**.md` — per-project memories (an index plus one
//!   file per fact). These stay project-scoped rather than being flattened
//!   into global memory: they are only true for the project they came from,
//!   and loading them everywhere would be actively misleading.
//!
//! Project-local `CLAUDE.md` files inside a repo are out of scope — those are
//! already read per-turn by [`crate::project_doc`].

use std::path::Path;

use anyhow::Result;

use super::{collect_markdown, scope_slug, MemoryImport, MigrationCategories, SourceData};

/// Provenance label stamped into imported files.
const ORIGIN: &str = "claude-code";
/// Global instructions file at the root of the data dir.
const GLOBAL_DOC: &str = "CLAUDE.md";
/// Per-project directory holding one subdirectory per project key.
const PROJECTS_DIR: &str = "projects";
/// Memory subdirectory within a project.
const MEMORY_DIR: &str = "memory";

/// True if `root` holds anything this parser would import.
pub fn has_importable(root: &Path) -> bool {
    if !root.is_dir() {
        return false;
    }
    if root.join(GLOBAL_DOC).is_file() {
        return true;
    }
    let mut found = Vec::new();
    collect_project_memories(root, &mut found);
    !found.is_empty()
}

/// Parse Claude Code data from a specific root directory.
pub fn parse_from(root: &Path, categories: &MigrationCategories) -> Result<SourceData> {
    let mut data = SourceData::default();
    if !categories.memory {
        return Ok(data);
    }

    let global = root.join(GLOBAL_DOC);
    if global.is_file() {
        data.memory_files.push(MemoryImport::global(
            global,
            format!("claude-{GLOBAL_DOC}"),
            ORIGIN,
        ));
    }

    collect_project_memories(root, &mut data.memory_files);
    Ok(data)
}

/// Push one [`MemoryImport`] per per-project memory file found under `root`.
fn collect_project_memories(root: &Path, out: &mut Vec<MemoryImport>) {
    let projects = root.join(PROJECTS_DIR);
    let Ok(entries) = std::fs::read_dir(&projects) else {
        return;
    };
    let mut entries: Vec<_> = entries.flatten().collect();
    entries.sort_by_key(std::fs::DirEntry::file_name);

    for entry in entries {
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_dir() || meta.file_type().is_symlink() {
            continue;
        }
        let memory_dir = entry.path().join(MEMORY_DIR);
        if !memory_dir.is_dir() {
            continue;
        }
        let Some(key) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        let scope = scope_slug(&key);
        if scope.is_empty() {
            continue;
        }

        let mut files = Vec::new();
        collect_markdown(&memory_dir, &mut files, "");
        for (path, rel) in files {
            // `rel` keeps any nested subdirectory, so two facts with the same
            // basename in different subdirectories can't collide.
            out.push(MemoryImport::scoped(
                path,
                scope.clone(),
                &format!("claude-{}", rel.replace('/', "-")),
                ORIGIN,
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Global doc + one project with an index and a frontmatter fact.
    fn fixture() -> TempDir {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join(GLOBAL_DOC), "# Global rules").unwrap();
        let mem = dir
            .path()
            .join(PROJECTS_DIR)
            .join("-Users-me-src-Proj")
            .join(MEMORY_DIR);
        std::fs::create_dir_all(&mem).unwrap();
        std::fs::write(mem.join("MEMORY.md"), "# Index").unwrap();
        std::fs::write(mem.join("fact-a.md"), "---\nname: fact-a\n---\n\nBody").unwrap();
        dir
    }

    fn parse(root: &Path) -> Vec<MemoryImport> {
        parse_from(root, &MigrationCategories::default())
            .unwrap()
            .memory_files
    }

    /// Per-project memories are only true for their project, so they must land
    /// scoped — flattening them into global memory would apply one project's
    /// facts everywhere.
    #[test]
    fn project_memories_are_scoped_and_global_doc_is_not() {
        let dir = fixture();
        let imports = parse(dir.path());

        let global: Vec<_> = imports.iter().filter(|i| i.scope.is_none()).collect();
        assert_eq!(global.len(), 1, "expected one global import: {imports:?}");
        assert_eq!(global[0].dest_rel, "claude-CLAUDE.md");

        let scoped: Vec<_> = imports.iter().filter(|i| i.scope.is_some()).collect();
        assert_eq!(
            scoped.len(),
            2,
            "expected the index + the fact: {imports:?}"
        );
        for import in &scoped {
            assert_eq!(import.scope.as_deref(), Some("users-me-src-proj"));
            assert!(
                import.dest_rel.starts_with("scopes/users-me-src-proj/"),
                "scoped import must land in its scope dir: {}",
                import.dest_rel
            );
            assert_eq!(import.origin, ORIGIN);
        }
    }

    /// Nested subdirectories must survive in the destination name, or two facts
    /// sharing a basename would silently overwrite each other.
    #[test]
    fn nested_project_memory_paths_do_not_collide() {
        let dir = fixture();
        let nested = dir
            .path()
            .join(PROJECTS_DIR)
            .join("-Users-me-src-Proj")
            .join(MEMORY_DIR)
            .join("sub");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("fact-a.md"), "nested body").unwrap();

        let dests: Vec<_> = parse(dir.path()).into_iter().map(|i| i.dest_rel).collect();
        let unique: std::collections::HashSet<_> = dests.iter().collect();
        assert_eq!(
            unique.len(),
            dests.len(),
            "destinations collided: {dests:?}"
        );
        assert!(
            dests.iter().any(|d| d.ends_with("claude-sub-fact-a.md")),
            "nested path must be preserved: {dests:?}"
        );
    }

    #[test]
    fn empty_and_missing_layouts_import_nothing() {
        let empty = TempDir::new().unwrap();
        assert!(parse(empty.path()).is_empty());
        assert!(!has_importable(empty.path()));
        assert!(!has_importable(&empty.path().join("nope")));

        // A project directory with no memory/ subdir contributes nothing.
        std::fs::create_dir_all(empty.path().join(PROJECTS_DIR).join("-Users-me-x")).unwrap();
        assert!(parse(empty.path()).is_empty());
    }

    #[test]
    fn has_importable_detects_each_shape() {
        let dir = fixture();
        assert!(has_importable(dir.path()));

        // Project memories alone (no global doc) are still worth importing.
        std::fs::remove_file(dir.path().join(GLOBAL_DOC)).unwrap();
        assert!(has_importable(dir.path()));
    }

    #[test]
    fn memory_category_off_imports_nothing() {
        let dir = fixture();
        let categories = MigrationCategories {
            memory: false,
            ..Default::default()
        };
        assert!(parse_from(dir.path(), &categories)
            .unwrap()
            .memory_files
            .is_empty());
    }
}
