//! Parser for Cursor rules (`~/.cursor`).
//!
//! Memory-only, and global-only: `rules` holds the user's editor-wide
//! instructions, which map onto Borg's global memory. Per-repo `.cursorrules`
//! and `<repo>/.cursor/rules` are deliberately out of scope — they belong to a
//! checkout, not to the user, and Borg already reads per-repo instruction files
//! per-turn via [`crate::project_doc`].
//!
//! `rules` is accepted both as a single file and as a directory of rule files,
//! since both layouts exist in the wild.

use std::path::Path;

use anyhow::Result;

use super::{MemoryImport, MigrationCategories, SourceData};

/// Provenance label stamped into imported files.
const ORIGIN: &str = "cursor";
/// Rules file or directory at the root of the data dir.
const RULES: &str = "rules";
/// Cursor's own rule-file extension, alongside plain `.md`.
const RULE_EXT: &str = "mdc";

/// True if `root` holds anything this parser would import.
pub fn has_importable(root: &Path) -> bool {
    !collect(root).is_empty()
}

/// Parse Cursor data from a specific root directory.
pub fn parse_from(root: &Path, categories: &MigrationCategories) -> Result<SourceData> {
    let mut data = SourceData::default();
    if categories.memory {
        data.memory_files = collect(root);
    }
    Ok(data)
}

fn collect(root: &Path) -> Vec<MemoryImport> {
    let rules = root.join(RULES);
    let mut out = Vec::new();

    if rules.is_file() {
        out.push(MemoryImport::global(
            rules,
            format!("{ORIGIN}-rules.md"),
            ORIGIN,
        ));
        return out;
    }

    let Ok(entries) = std::fs::read_dir(&rules) else {
        return out;
    };
    let mut entries: Vec<_> = entries.flatten().collect();
    entries.sort_by_key(std::fs::DirEntry::file_name);

    for entry in entries {
        let path = entry.path();
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_file() || meta.file_type().is_symlink() {
            continue;
        }
        let is_rule = path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e == "md" || e == RULE_EXT);
        if !is_rule {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        // Land everything as `.md`: Borg's memory loader reads markdown, and a
        // `.mdc` file is markdown with a different name.
        out.push(MemoryImport::global(
            path.clone(),
            format!("{ORIGIN}-{stem}.md"),
            ORIGIN,
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn parse(root: &Path) -> Vec<MemoryImport> {
        parse_from(root, &MigrationCategories::default())
            .unwrap()
            .memory_files
    }

    /// Both layouts exist in the wild; a directory of rules imports each file.
    #[test]
    fn rules_directory_imports_md_and_mdc_as_markdown() {
        let dir = TempDir::new().unwrap();
        let rules = dir.path().join(RULES);
        std::fs::create_dir_all(&rules).unwrap();
        std::fs::write(rules.join("style.mdc"), "be terse").unwrap();
        std::fs::write(rules.join("testing.md"), "write tests").unwrap();
        std::fs::write(rules.join("notes.txt"), "ignored").unwrap();

        let dests: Vec<_> = parse(dir.path()).into_iter().map(|i| i.dest_rel).collect();
        assert_eq!(
            dests,
            vec!["cursor-style.md", "cursor-testing.md"],
            "`.mdc` must land as `.md` and non-rule files must be skipped"
        );
    }

    #[test]
    fn rules_as_single_file_imports_one_entry() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join(RULES), "be nice").unwrap();

        let imports = parse(dir.path());
        assert_eq!(imports.len(), 1);
        assert_eq!(imports[0].dest_rel, "cursor-rules.md");
        assert_eq!(imports[0].scope, None, "editor rules are user-wide");
        assert_eq!(imports[0].origin, ORIGIN);
    }

    /// `~/.cursor` exists on plenty of machines with no rules in it; offering
    /// an import that copies nothing is just noise.
    #[test]
    fn no_rules_means_nothing_to_import() {
        let dir = TempDir::new().unwrap();
        assert!(parse(dir.path()).is_empty());
        assert!(!has_importable(dir.path()));

        std::fs::create_dir_all(dir.path().join(RULES)).unwrap();
        assert!(
            !has_importable(dir.path()),
            "an empty rules dir has nothing"
        );
    }
}
