//! The owner's name: what the person who runs this Borg is called.
//!
//! The name lives in the `user.name` setting. The sidebar shows it, the
//! system prompt tells the agent, and a change rewrites it in the seeded
//! memory files so the agent's own memory agrees. Owner names are often
//! ordinary words ("Will", "May"), so the rewrite touches only the spots
//! onboarding wrote, never free text.

use std::path::Path;

use anyhow::Result;

use crate::constants::{IDENTITY_FILE, MEMORY_INDEX_FILE};

/// The setting that holds the owner's name.
pub const USER_NAME_KEY: &str = "user.name";

/// Why `name` cannot be the owner's name, if it cannot. Same limits as the
/// agent's name: one line, short enough for the sidebar. Empty is allowed
/// (it means the name is not known).
pub fn validate(name: &str) -> std::result::Result<(), String> {
    crate::agent_name::validate(name)
}

/// Validate `name`, write it to the `user.name` setting, and carry it into
/// the seeded memory files. Returns the trimmed name that was saved.
pub async fn save(name: &str) -> Result<String> {
    let resolver = crate::settings::SettingsResolver::load().await?;
    save_with(&resolver, crate::config::Config::data_dir()?, name).await
}

/// [`save`] against a given settings store and data dir.
async fn save_with(
    resolver: &crate::settings::SettingsResolver,
    data_dir: std::path::PathBuf,
    name: &str,
) -> Result<String> {
    let name = name.trim();
    validate(name).map_err(anyhow::Error::msg)?;
    let old = resolver.resolve().await?.user.name.unwrap_or_default();
    resolver.set(USER_NAME_KEY, name).await?;
    let new = name.to_string();
    tokio::task::spawn_blocking(move || rename_in_memory_files(&data_dir, &old, &new))
        .await
        .unwrap_or_else(|e| Err(anyhow::anyhow!("rename task failed: {e}")))?;
    Ok(name.to_string())
}

/// Rewrites one memory file's text from the old name to the new; `None` when unchanged.
type RenameEdit = fn(&str, &str, &str) -> Option<String>;

/// Rewrite the owner's name in the seeded memory files under `data_dir`,
/// from `old_name` (the setting's previous value) to `new_name`.
///
/// A missing file is left alone. An empty new name leaves the files as they
/// are: they keep the last name anyone gave, which beats a blank.
pub fn rename_in_memory_files(data_dir: &Path, old_name: &str, new_name: &str) -> Result<()> {
    let (old, new) = (old_name.trim(), new_name.trim());
    if new.is_empty() || old == new {
        return Ok(());
    }
    let _guard = crate::agent_name::MEMORY_FILE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let edits: [(&str, RenameEdit); 2] = [
        (MEMORY_INDEX_FILE, rename_memory_index),
        (IDENTITY_FILE, rename_identity),
    ];
    for (file, edit) in edits {
        let path = data_dir.join(file);
        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e.into()),
        };
        if let Some(updated) = edit(&content, old, new) {
            coding::fs_atomic::atomic_write(&path, updated.as_bytes())?;
        }
    }
    Ok(())
}

/// `MEMORY.md`: the first `- Name:` line in the `## Owner` section onboarding
/// seeds. A `- Name:` line under any other heading is the agent's own note.
fn rename_memory_index(content: &str, _old: &str, new: &str) -> Option<String> {
    let mut offset = 0;
    let mut in_owner = false;
    for line in content.split_inclusive('\n') {
        let bare = line.trim_end_matches(['\r', '\n']);
        if bare.starts_with("## ") {
            in_owner = bare.trim_end() == "## Owner";
        } else if let Some(rest) = bare.strip_prefix("- Name: ").filter(|_| in_owner) {
            if rest.trim_end() == new {
                return None;
            }
            let eol = &line[bare.len()..];
            return Some(format!(
                "{}- Name: {new}{eol}{}",
                &content[..offset],
                &content[offset + line.len()..]
            ));
        }
        offset += line.len();
    }
    None
}

/// `IDENTITY.md`: only the template's "belonging to Name". Once the agent
/// writes its own persona the phrase is gone and the file is left alone.
fn rename_identity(content: &str, old: &str, new: &str) -> Option<String> {
    if old.is_empty() {
        return None;
    }
    let from = format!("belonging to {old}");
    let updated = content.replace(&from, &format!("belonging to {new}"));
    (updated != content).then_some(updated)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_uses_the_one_line_short_name_rules() {
        assert!(validate("Mike").is_ok());
        assert!(validate("").is_ok());
        assert!(validate("Mi\nke").is_err());
        assert!(validate("**Mike**").is_err());
        assert!(validate(&"m".repeat(41)).is_err());
    }

    #[test]
    fn memory_index_rewrites_the_owner_name_line() {
        let before = "# Memory Index\n\n## Owner\n- Name: Sam\n- Agent: Borgzilla\n";
        let after = rename_memory_index(before, "Sam", "Mike").unwrap();
        assert_eq!(
            after,
            "# Memory Index\n\n## Owner\n- Name: Mike\n- Agent: Borgzilla\n"
        );
        assert_eq!(rename_memory_index(&after, "Sam", "Mike"), None);
    }

    #[test]
    fn memory_index_leaves_name_lines_outside_the_owner_section() {
        let before = "## Pets\n- Name: Rex\n\n## Owner\n- Name: Sam\n\n## Team\n- Name: Ann\n";
        let after = rename_memory_index(before, "Sam", "Mike").unwrap();
        assert_eq!(
            after,
            "## Pets\n- Name: Rex\n\n## Owner\n- Name: Mike\n\n## Team\n- Name: Ann\n"
        );
        assert_eq!(rename_memory_index("## Pets\n- Name: Rex\n", "", "X"), None);
    }

    #[test]
    fn memory_index_keeps_crlf() {
        let after = rename_memory_index("## Owner\r\n- Name: Sam\r\n", "", "Mike").unwrap();
        assert_eq!(after, "## Owner\r\n- Name: Mike\r\n");
    }

    /// An owner called "Will" must not rewrite "will" or "Willow" elsewhere.
    #[test]
    fn identity_rewrites_only_the_template_phrase() {
        let before = "# Borg\n\nYou are Borg, an AI personal assistant belonging to Will.\nWill likes Willow trees; you will help.\n";
        let after = rename_identity(before, "Will", "Ada").unwrap();
        assert_eq!(
            after,
            "# Borg\n\nYou are Borg, an AI personal assistant belonging to Ada.\nWill likes Willow trees; you will help.\n"
        );
        assert_eq!(rename_identity("free-form persona\n", "Will", "Ada"), None);
        assert_eq!(rename_identity(before, "", "Ada"), None);
    }

    #[test]
    fn rename_in_memory_files_rewrites_present_files_and_skips_missing() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(MEMORY_INDEX_FILE),
            "## Owner\n- Name: Sam\n- Agent: Borg\n",
        )
        .unwrap();

        rename_in_memory_files(dir.path(), "Sam", "  Mike ").unwrap();

        let memory = std::fs::read_to_string(dir.path().join(MEMORY_INDEX_FILE)).unwrap();
        assert_eq!(memory, "## Owner\n- Name: Mike\n- Agent: Borg\n");
        assert!(
            !dir.path().join(IDENTITY_FILE).exists(),
            "missing files stay missing"
        );
    }

    #[tokio::test]
    async fn save_writes_the_setting_and_renames_the_owner_in_memory() {
        let resolver = crate::settings::SettingsResolver::new(crate::db::Database::test_db().await);
        resolver.set(USER_NAME_KEY, "Sam").await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        let memory = dir.path().join(MEMORY_INDEX_FILE);
        std::fs::write(&memory, "## Owner\n- Name: Sam\n").unwrap();

        let saved = save_with(&resolver, dir.path().to_path_buf(), "  Mike ")
            .await
            .unwrap();

        assert_eq!(saved, "Mike");
        let config = resolver.resolve().await.unwrap();
        assert_eq!(config.user.name.as_deref(), Some("Mike"));
        assert_eq!(
            std::fs::read_to_string(&memory).unwrap(),
            "## Owner\n- Name: Mike\n"
        );
    }

    #[tokio::test]
    async fn save_refuses_a_bad_name_before_writing_anything() {
        let resolver = crate::settings::SettingsResolver::new(crate::db::Database::test_db().await);
        resolver.set(USER_NAME_KEY, "Sam").await.unwrap();
        let dir = tempfile::tempdir().unwrap();

        let err = save_with(&resolver, dir.path().to_path_buf(), "Mi\nke").await;

        assert!(err.is_err());
        let config = resolver.resolve().await.unwrap();
        assert_eq!(config.user.name.as_deref(), Some("Sam"));
    }

    #[test]
    fn clearing_the_name_leaves_the_files_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(MEMORY_INDEX_FILE);
        std::fs::write(&path, "## Owner\n- Name: Sam\n").unwrap();
        rename_in_memory_files(dir.path(), "Sam", "  ").unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "## Owner\n- Name: Sam\n"
        );
    }
}
