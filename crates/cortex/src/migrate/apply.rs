use std::path::Path;

use anyhow::{Context, Result};

use crate::db::Database;

use super::{MigrationPlan, MigrationResult, SourceData};

/// Apply a migration plan to the Borg data directory.
pub fn apply_plan(
    plan: &MigrationPlan,
    data: &SourceData,
    borg_data_dir: &Path,
) -> Result<MigrationResult> {
    let mut result = MigrationResult {
        config_changes_applied: 0,
        credentials_added: 0,
        memory_files_copied: 0,
        persona_copied: false,
        skills_copied: 0,
        warnings: Vec::new(),
    };

    // Apply config changes to DB
    apply_config_changes(plan, &mut result)?;

    // Apply credentials to DB
    apply_credentials(plan, data, &mut result)?;

    // Copy memory files
    apply_memory_files(plan, borg_data_dir, &mut result)?;

    // Copy persona
    apply_persona(plan, borg_data_dir, &mut result)?;

    // Copy skills
    apply_skills(plan, borg_data_dir, &mut result)?;

    Ok(result)
}

fn apply_config_changes(plan: &MigrationPlan, result: &mut MigrationResult) -> Result<()> {
    let active_changes: Vec<_> = plan.config_changes.iter().filter(|c| !c.skipped).collect();

    if active_changes.is_empty() {
        return Ok(());
    }

    let db = Database::open().context("Failed to open database for migration")?;

    for change in &active_changes {
        // Use apply_setting for validation, then write to DB
        let mut scratch = crate::config::Config::default();
        match scratch.apply_setting(&change.key, &change.new_value) {
            Ok(_) => match db.set_setting(&change.key, &change.new_value) {
                Ok(()) => result.config_changes_applied += 1,
                Err(e) => result.warnings.push(format!("Config {}: {e}", change.key)),
            },
            Err(e) => result.warnings.push(format!("Config {}: {e}", change.key)),
        }
    }

    Ok(())
}

fn apply_credentials(
    plan: &MigrationPlan,
    data: &SourceData,
    result: &mut MigrationResult,
) -> Result<()> {
    if plan.credentials_to_add.is_empty() {
        return Ok(());
    }

    let db = Database::open().context("Failed to open database for credential migration")?;

    // Load existing credentials from DB
    let mut creds: std::collections::HashMap<String, crate::config::CredentialValue> = db
        .get_setting("credentials")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();

    for key_name in &plan.credentials_to_add {
        if data.credentials.iter().any(|(k, _)| k == key_name) {
            // Store as env var reference in DB credentials
            creds.insert(
                key_name.clone(),
                crate::config::CredentialValue::EnvVar(key_name.clone()),
            );
            result.credentials_added += 1;
        }
    }

    if !creds.is_empty() {
        if let Ok(json) = serde_json::to_string(&creds) {
            db.set_setting("credentials", &json)?;
        }
    }

    Ok(())
}

fn apply_memory_files(
    plan: &MigrationPlan,
    borg_data_dir: &Path,
    result: &mut MigrationResult,
) -> Result<()> {
    if plan.memory_files.is_empty() {
        return Ok(());
    }

    let memory_dir = borg_data_dir.join("memory");
    std::fs::create_dir_all(&memory_dir)?;

    for (src, dest_name) in &plan.memory_files {
        let dest = memory_dir.join(dest_name);
        match std::fs::copy(src, &dest) {
            Ok(_) => result.memory_files_copied += 1,
            Err(e) => result
                .warnings
                .push(format!("Failed to copy {}: {e}", src.display())),
        }
    }

    Ok(())
}

fn apply_persona(
    plan: &MigrationPlan,
    borg_data_dir: &Path,
    result: &mut MigrationResult,
) -> Result<()> {
    if let Some(src) = &plan.persona_file {
        let dest = borg_data_dir.join("IDENTITY.md");
        match std::fs::copy(src, &dest) {
            Ok(_) => result.persona_copied = true,
            Err(e) => result.warnings.push(format!("Failed to copy persona: {e}")),
        }
    }
    Ok(())
}

fn apply_skills(
    plan: &MigrationPlan,
    borg_data_dir: &Path,
    result: &mut MigrationResult,
) -> Result<()> {
    if plan.skill_dirs.is_empty() {
        return Ok(());
    }

    let skills_dir = borg_data_dir.join("skills");
    std::fs::create_dir_all(&skills_dir)?;

    for (src_dir, name) in &plan.skill_dirs {
        let dest_dir = skills_dir.join(name);
        match copy_dir_recursive(src_dir, &dest_dir) {
            Ok(()) => result.skills_copied += 1,
            Err(e) => result
                .warnings
                .push(format!("Failed to copy skill {name}: {e}")),
        }
    }

    Ok(())
}

fn copy_dir_recursive(src: &Path, dest: &Path) -> Result<()> {
    std::fs::create_dir_all(dest)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let src_path = entry.path();
        let dest_path = dest.join(entry.file_name());
        if src_path.is_dir() {
            copy_dir_recursive(&src_path, &dest_path)?;
        } else {
            std::fs::copy(&src_path, &dest_path)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migrate::{MigrationSource, PlanChange};
    use tempfile::TempDir;

    fn make_borg_dir() -> TempDir {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("memory")).unwrap();
        std::fs::create_dir_all(dir.path().join("skills")).unwrap();
        dir
    }

    #[test]
    fn test_apply_config_changes_validates_keys() {
        // Verify apply_setting validates config keys correctly
        let mut config = crate::config::Config::default();
        assert!(config.apply_setting("model", "gpt-4o").is_ok());
        assert_eq!(config.llm.model, "gpt-4o");
        assert!(config.apply_setting("provider", "openai").is_ok());
        assert_eq!(config.llm.provider.as_deref(), Some("openai"));
    }

    #[test]
    fn test_skipped_changes_not_applied() {
        let plan = MigrationPlan {
            source: MigrationSource::Hermes,
            config_changes: vec![PlanChange {
                key: "llm.model".into(),
                source_key: "other".into(),
                current_value: Some("existing".into()),
                new_value: "should-skip".into(),
                skipped: true,
            }],
            credentials_to_add: vec![],
            memory_files: vec![],
            persona_file: None,
            skill_dirs: vec![],
        };
        let active: Vec<_> = plan.config_changes.iter().filter(|c| !c.skipped).collect();
        assert!(active.is_empty());
    }

    #[test]
    fn test_apply_copies_memory_files() {
        let borg = make_borg_dir();
        let src = TempDir::new().unwrap();
        std::fs::write(src.path().join("MEMORY.md"), "# My Memories").unwrap();

        let plan = MigrationPlan {
            source: MigrationSource::Hermes,
            config_changes: vec![],
            credentials_to_add: vec![],
            memory_files: vec![(src.path().join("MEMORY.md"), "hermes-MEMORY.md".into())],
            persona_file: None,
            skill_dirs: vec![],
        };

        let data = SourceData::default();
        let result = apply_plan(&plan, &data, borg.path()).unwrap();

        assert_eq!(result.memory_files_copied, 1);
        let content = std::fs::read_to_string(borg.path().join("memory/hermes-MEMORY.md")).unwrap();
        assert_eq!(content, "# My Memories");
    }

    #[test]
    fn test_apply_copies_persona() {
        let borg = make_borg_dir();
        let src = TempDir::new().unwrap();
        std::fs::write(src.path().join("SOUL.md"), "# My Persona").unwrap();

        let plan = MigrationPlan {
            source: MigrationSource::Hermes,
            config_changes: vec![],
            credentials_to_add: vec![],
            memory_files: vec![],
            persona_file: Some(src.path().join("SOUL.md")),
            skill_dirs: vec![],
        };

        let data = SourceData::default();
        let result = apply_plan(&plan, &data, borg.path()).unwrap();

        assert!(result.persona_copied);
        let content = std::fs::read_to_string(borg.path().join("IDENTITY.md")).unwrap();
        assert_eq!(content, "# My Persona");
    }

    #[test]
    fn test_apply_copies_skills() {
        let borg = make_borg_dir();
        let src = TempDir::new().unwrap();
        std::fs::create_dir_all(src.path().join("my-skill")).unwrap();
        std::fs::write(
            src.path().join("my-skill/SKILL.md"),
            "---\nname: my-skill\n---",
        )
        .unwrap();

        let plan = MigrationPlan {
            source: MigrationSource::Hermes,
            config_changes: vec![],
            credentials_to_add: vec![],
            memory_files: vec![],
            persona_file: None,
            skill_dirs: vec![(src.path().join("my-skill"), "my-skill".into())],
        };

        let data = SourceData::default();
        let result = apply_plan(&plan, &data, borg.path()).unwrap();

        assert_eq!(result.skills_copied, 1);
        assert!(borg.path().join("skills/my-skill/SKILL.md").exists());
    }

    #[test]
    fn test_credential_value_variants() {
        // Verify CredentialValue round-trips through JSON (used by DB storage)
        let cred = crate::config::CredentialValue::EnvVar("MY_KEY".to_string());
        let json = serde_json::to_string(&cred).unwrap();
        assert_eq!(json, "\"MY_KEY\"");
        let parsed: crate::config::CredentialValue = serde_json::from_str(&json).unwrap();
        match parsed {
            crate::config::CredentialValue::EnvVar(v) => assert_eq!(v, "MY_KEY"),
            _ => panic!("expected EnvVar"),
        }
    }
}
