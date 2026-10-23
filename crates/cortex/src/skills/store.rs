//! Write path for user skills under `~/.borg/skills/<name>/SKILL.md`.
//!
//! The loader reads whatever is on disk; this module is how a surface puts
//! it there. It owns the name rules (a name is also a directory), refuses to
//! touch a bundled skill (the embedded copy wins anyway, so an edit would be
//! silently ignored), and never deletes outside the skills directory.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};

use super::loader::skills_dir;
use super::types::parse_skill_md;
use super::{bundled_skill_content, is_builtin_skill_name};

/// Longest accepted skill name. Matches the MCP server id cap so both
/// operator-chosen identifiers share one rule.
pub const MAX_SKILL_NAME_LEN: usize = 64;

/// A user skill name is also its directory under the skills root, so it must
/// be a single safe path component: ASCII letters, digits, `_`, `-`, with no
/// leading dot or separator.
pub fn validate_user_skill_name(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > MAX_SKILL_NAME_LEN {
        bail!("skill name must be 1..={MAX_SKILL_NAME_LEN} chars");
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        bail!("skill name must match [a-zA-Z0-9_-]+");
    }
    if is_builtin_skill_name(name) {
        bail!(
            "`{name}` is a built-in skill; the built-in copy always wins, so copy it to a new \
             name to customize it"
        );
    }
    Ok(())
}

/// The on-disk directory of the user skill whose frontmatter name is `name`.
///
/// The directory usually matches the name, but the loader keys on the
/// frontmatter, so a skill authored under a different folder name is found
/// by parsing. `None` when no user skill declares that name.
pub fn user_skill_dir(root: &Path, name: &str) -> Option<PathBuf> {
    // The loader does not validate frontmatter names, so a skill that
    // declares `name: ../../x` reaches here. Refuse anything that is not a
    // single path component before it touches the filesystem.
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.contains('/')
        || name.contains('\\')
        || name.contains('\0')
    {
        return None;
    }
    let direct = root.join(name);
    if direct.join("SKILL.md").is_file() {
        return Some(direct);
    }
    let entries = std::fs::read_dir(root).ok()?;
    for entry in entries.flatten() {
        let dir = entry.path();
        let Ok(content) = std::fs::read_to_string(dir.join("SKILL.md")) else {
            continue;
        };
        if let Ok((manifest, _)) = parse_skill_md(&content) {
            if manifest.name == name {
                return Some(dir);
            }
        }
    }
    None
}

/// Raw `SKILL.md` text for `name`: the embedded copy for a bundled skill, the
/// on-disk file for a user skill.
pub fn skill_source(name: &str) -> Result<Option<String>> {
    if let Some(bundled) = bundled_skill_content(name) {
        return Ok(Some(bundled.to_string()));
    }
    let root = skills_dir()?;
    let Some(dir) = user_skill_dir(&root, name) else {
        return Ok(None);
    };
    let content = std::fs::read_to_string(dir.join("SKILL.md"))
        .with_context(|| format!("read {}", dir.join("SKILL.md").display()))?;
    Ok(Some(content))
}

/// Create or replace the user skill `name` with `content`.
///
/// The content must parse as a `SKILL.md` whose frontmatter name is `name`:
/// the catalog, invocation, and shadowing all key on the frontmatter, so a
/// file saved under one name that declares another would show up as the
/// second and leave the author unable to connect the two. Returns the
/// directory the skill now lives in.
pub fn write_user_skill(name: &str, content: &str) -> Result<PathBuf> {
    write_user_skill_in(&skills_dir()?, name, content)
}

/// [`write_user_skill`] against an explicit skills root.
pub fn write_user_skill_in(root: &Path, name: &str, content: &str) -> Result<PathBuf> {
    validate_user_skill_name(name)?;
    let (manifest, _) = parse_skill_md(content)?;
    if manifest.name != name {
        bail!(
            "the frontmatter names this skill `{}`, but it is being saved as `{name}`; make \
             the two match",
            manifest.name
        );
    }
    // An existing skill keeps its folder, even one named differently from
    // the frontmatter: a second folder declaring the same name would make
    // the loader refuse both, and the skill would vanish from the catalog.
    let dir = user_skill_dir(root, name).unwrap_or_else(|| root.join(name));
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let path = dir.join("SKILL.md");
    // Write-then-rename so a crash mid-write cannot leave a half skill that
    // the next turn's loader rejects with a parse error.
    let tmp = dir.join(".SKILL.md.tmp");
    std::fs::write(&tmp, content).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, &path).with_context(|| format!("rename into {}", path.display()))?;
    Ok(dir)
}

/// Remove the user skill `name` and everything in its directory.
///
/// Only a directory directly under the skills root goes; a bundled skill has
/// no directory to remove, and a skill directory that is a symlink elsewhere
/// is unlinked rather than followed.
pub fn delete_user_skill(name: &str) -> Result<PathBuf> {
    delete_user_skill_in(&skills_dir()?, name)
}

/// [`delete_user_skill`] against an explicit skills root.
pub fn delete_user_skill_in(root: &Path, name: &str) -> Result<PathBuf> {
    validate_user_skill_name(name)?;
    let dir = user_skill_dir(root, name).ok_or_else(|| anyhow!("no user skill named `{name}`"))?;
    let Some(parent) = dir.parent() else {
        bail!("refusing to delete {}: no parent directory", dir.display());
    };
    if parent != root {
        bail!(
            "refusing to delete {}: not directly under {}",
            dir.display(),
            root.display()
        );
    }
    let meta =
        std::fs::symlink_metadata(&dir).with_context(|| format!("stat {}", dir.display()))?;
    if meta.file_type().is_symlink() {
        std::fs::remove_file(&dir).with_context(|| format!("unlink {}", dir.display()))?;
    } else {
        std::fs::remove_dir_all(&dir).with_context(|| format!("remove {}", dir.display()))?;
    }
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_rules_refuse_paths_and_builtins() {
        assert!(validate_user_skill_name("my-skill_2").is_ok());
        assert!(validate_user_skill_name("").is_err());
        assert!(validate_user_skill_name("../etc").is_err());
        assert!(validate_user_skill_name("a b").is_err());
        assert!(validate_user_skill_name(&"x".repeat(65)).is_err());
        // A bundled name is refused even when the case differs: the default
        // macOS filesystem would land it on the same directory.
        let builtin = super::super::BUNDLED_SKILLS[0].0;
        assert!(validate_user_skill_name(builtin).is_err());
        assert!(validate_user_skill_name(&builtin.to_ascii_uppercase()).is_err());
    }

    #[test]
    fn user_skill_dir_finds_a_skill_by_frontmatter_name() {
        let tmp = tempfile::tempdir().unwrap();
        let folder = tmp.path().join("folder-name");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(
            folder.join("SKILL.md"),
            "---\nname: declared-name\ndescription: d\n---\nbody",
        )
        .unwrap();
        assert_eq!(
            user_skill_dir(tmp.path(), "declared-name"),
            Some(folder.clone())
        );
        assert_eq!(user_skill_dir(tmp.path(), "folder-name"), Some(folder));
        assert_eq!(user_skill_dir(tmp.path(), "other"), None);
        // A name that is not one path component never reaches the disk.
        std::fs::write(tmp.path().join("SKILL.md"), "---\nname: ..\n---\n").unwrap();
        assert_eq!(user_skill_dir(tmp.path(), ".."), None);
        assert_eq!(user_skill_dir(tmp.path(), "../folder-name"), None);
    }

    #[test]
    fn editing_a_skill_keeps_its_folder_and_deleting_removes_it() {
        // Regression: a save used to write `root/<name>/` even when the
        // skill lived in a folder of another name. Two folders declaring one
        // name make the loader drop both, so the edit made the skill vanish.
        let tmp = tempfile::tempdir().unwrap();
        let folder = tmp.path().join("folder-name");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(
            folder.join("SKILL.md"),
            "---\nname: declared-name\ndescription: d\n---\nbody",
        )
        .unwrap();
        let edited = "---\nname: declared-name\ndescription: e\n---\nnew body";
        let dir = write_user_skill_in(tmp.path(), "declared-name", edited).unwrap();
        assert_eq!(dir, folder);
        assert!(!tmp.path().join("declared-name").exists());
        assert_eq!(
            std::fs::read_to_string(folder.join("SKILL.md")).unwrap(),
            edited
        );
        // Frontmatter that disagrees with the name is refused before any write.
        let err = write_user_skill_in(
            tmp.path(),
            "declared-name",
            "---\nname: other\ndescription: e\n---\nx",
        )
        .unwrap_err();
        assert!(err.to_string().contains("other"), "{err}");
        // A new name gets its own folder; delete removes exactly that folder.
        let fresh = write_user_skill_in(
            tmp.path(),
            "fresh",
            "---\nname: fresh\ndescription: f\n---\nx",
        )
        .unwrap();
        assert_eq!(fresh, tmp.path().join("fresh"));
        std::fs::write(fresh.join("extra.sh"), "echo").unwrap();
        assert_eq!(delete_user_skill_in(tmp.path(), "fresh").unwrap(), fresh);
        assert!(!fresh.exists());
        assert!(folder.exists(), "deleting one skill must not touch another");
        assert!(delete_user_skill_in(tmp.path(), "fresh").is_err());
    }
}
