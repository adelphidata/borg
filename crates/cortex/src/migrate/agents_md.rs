//! Parser for CLIs that keep their user-level instructions in an `AGENTS.md`
//! at the root of their data directory. The probed directory is just where
//! that convention puts the file; see `MigrationSource::data_dir`.
//!
//! Memory-only and single-file: the document is the user's standing
//! instructions, which map onto Borg's global memory. Repo-level `AGENTS.md`
//! files are out of scope — [`crate::project_doc`] already reads those per-turn.

use std::path::Path;

use anyhow::Result;

use super::{MemoryImport, MigrationCategories, SourceData};

/// Provenance label stamped into imported files.
const ORIGIN: &str = "agents-md";
/// The instructions file this source is named for.
const AGENTS_DOC: &str = "AGENTS.md";

/// True if `root` holds anything this parser would import.
///
/// Probes the file, not the directory: the data dir exists whenever the tool
/// has ever run, but with no `AGENTS.md` there is nothing to migrate.
pub fn has_importable(root: &Path) -> bool {
    root.join(AGENTS_DOC).is_file()
}

/// Parse `AGENTS.md`-based CLI data from a specific root directory.
pub fn parse_from(root: &Path, categories: &MigrationCategories) -> Result<SourceData> {
    let mut data = SourceData::default();
    if !categories.memory {
        return Ok(data);
    }
    let doc = root.join(AGENTS_DOC);
    if doc.is_file() {
        data.memory_files.push(MemoryImport::global(
            doc,
            format!("{ORIGIN}-{AGENTS_DOC}"),
            ORIGIN,
        ));
    }
    Ok(data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn agents_doc_imports_as_global_memory() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join(AGENTS_DOC), "# Instructions").unwrap();

        let imports = parse_from(dir.path(), &MigrationCategories::default())
            .unwrap()
            .memory_files;
        assert_eq!(imports.len(), 1);
        assert_eq!(imports[0].dest_rel, "agents-md-AGENTS.md");
        assert_eq!(imports[0].scope, None);
        assert_eq!(imports[0].origin, ORIGIN);
        assert!(has_importable(dir.path()));
    }

    /// The data dir exists whenever the tool has ever run, so the directory
    /// alone must not advertise an import.
    #[test]
    fn bare_directory_has_nothing_to_import() {
        let dir = TempDir::new().unwrap();
        assert!(!has_importable(dir.path()));
        assert!(parse_from(dir.path(), &MigrationCategories::default())
            .unwrap()
            .memory_files
            .is_empty());
    }
}
