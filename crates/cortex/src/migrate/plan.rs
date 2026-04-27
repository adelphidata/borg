use std::path::Path;

use crate::config::Config;

use super::{MigrationPlan, MigrationSource, PlanChange, SourceData};

/// Build a migration plan by comparing source data against the current Borg config.
/// Existing values in Borg are marked as skipped (no overwrite).
pub fn build_plan(
    source: MigrationSource,
    data: &SourceData,
    current: &Config,
    borg_data_dir: &Path,
) -> MigrationPlan {
    let config_changes = build_config_changes(data, current);
    let credentials_to_add = build_credential_changes(data, borg_data_dir);
    let memory_files = build_memory_changes(data, borg_data_dir);
    let persona_file = build_persona_change(data, borg_data_dir);
    let skill_dirs = build_skill_changes(data, borg_data_dir);

    MigrationPlan {
        source,
        config_changes,
        credentials_to_add,
        memory_files,
        persona_file,
        skill_dirs,
    }
}

fn build_config_changes(data: &SourceData, current: &Config) -> Vec<PlanChange> {
    data.config_changes
        .iter()
        .map(|change| {
            let key = format!("{}.{}", change.section, change.field);
            let current_value = get_current_value(current, &change.section, &change.field);
            let skipped = is_already_set(&current_value, &change.new_value);

            PlanChange {
                key,
                source_key: change.source_key.clone(),
                current_value,
                new_value: change.new_value.clone(),
                skipped,
            }
        })
        .collect()
}

fn get_current_value(config: &Config, section: &str, field: &str) -> Option<String> {
    match (section, field) {
        ("llm", "model") => {
            let v = &config.llm.model;
            if v.is_empty() {
                None
            } else {
                Some(v.clone())
            }
        }
        ("llm", "provider") => config.llm.provider.clone(),
        ("tools", "default_timeout_ms") => Some(config.tools.default_timeout_ms.to_string()),
        ("browser", "headless") => Some(config.browser.headless.to_string()),
        ("user", "timezone") => config.user.timezone.clone(),
        ("sandbox", "enabled") => Some(config.sandbox.enabled.to_string()),
        ("sandbox", "mode") => Some(config.sandbox.mode.clone()),
        ("compaction", "model") => config.compaction.model.clone(),
        ("tts", "enabled") => Some(config.tts.enabled.to_string()),
        ("tts", "default_voice") => Some(config.tts.default_voice.clone()),
        _ => None,
    }
}

fn is_already_set(current: &Option<String>, _new_value: &str) -> bool {
    // Skip whenever Borg already has a non-empty value set for this field.
    // We never overwrite user customizations.
    current.as_ref().is_some_and(|v| !v.is_empty())
}

fn build_credential_changes(data: &SourceData, borg_data_dir: &Path) -> Vec<String> {
    let env_path = borg_data_dir.join(".env");
    let existing_env = std::fs::read_to_string(&env_path).unwrap_or_default();

    data.credentials
        .iter()
        .filter(|(key, _)| {
            // Skip if already in Borg .env
            !existing_env
                .lines()
                .any(|line| line.starts_with(&format!("{key}=")))
        })
        .map(|(key, _)| key.clone())
        .collect()
}

fn build_memory_changes(
    data: &SourceData,
    borg_data_dir: &Path,
) -> Vec<(std::path::PathBuf, String)> {
    let memory_dir = borg_data_dir.join("memory");
    data.memory_files
        .iter()
        .filter(|(_, dest)| !memory_dir.join(dest).exists())
        .cloned()
        .collect()
}

fn build_persona_change(data: &SourceData, borg_data_dir: &Path) -> Option<std::path::PathBuf> {
    data.persona_file.as_ref().and_then(|src| {
        let identity_path = borg_data_dir.join("IDENTITY.md");
        if identity_path.exists() {
            None // Don't overwrite existing IDENTITY.md
        } else {
            Some(src.clone())
        }
    })
}

fn build_skill_changes(
    data: &SourceData,
    borg_data_dir: &Path,
) -> Vec<(std::path::PathBuf, String)> {
    let skills_dir = borg_data_dir.join("skills");
    data.skill_dirs
        .iter()
        .filter(|(_, name)| !skills_dir.join(name).exists())
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migrate::ConfigChange;
    use tempfile::TempDir;

    fn default_config() -> Config {
        Config::default()
    }

    #[test]
    fn test_build_plan_detects_changes() {
        let dir = TempDir::new().unwrap();
        let data = SourceData {
            config_changes: vec![
                ConfigChange {
                    section: "llm".into(),
                    field: "model".into(),
                    source_key: "model.default".into(),
                    new_value: "gpt-4o".into(),
                },
                ConfigChange {
                    section: "llm".into(),
                    field: "provider".into(),
                    source_key: "model.provider".into(),
                    new_value: "openai".into(),
                },
            ],
            credentials: vec![("OPENAI_API_KEY".into(), "sk-test".into())],
            memory_files: vec![],
            persona_file: Some(dir.path().join("SOUL.md")),
            skill_dirs: vec![],
        };

        let mut config = default_config();
        config.llm.model = String::new(); // clear default to allow migration
        let plan = build_plan(MigrationSource::Hermes, &data, &config, dir.path());

        assert_eq!(plan.config_changes.len(), 2);
        assert!(!plan.config_changes[0].skipped); // model is empty
        assert!(!plan.config_changes[1].skipped); // provider is None

        assert_eq!(plan.credentials_to_add.len(), 1);
        assert_eq!(plan.credentials_to_add[0], "OPENAI_API_KEY");
    }

    #[test]
    fn test_build_plan_skips_existing_credentials() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join(".env"), "OPENAI_API_KEY=existing\n").unwrap();

        let data = SourceData {
            credentials: vec![("OPENAI_API_KEY".into(), "new-key".into())],
            ..Default::default()
        };

        let config = default_config();
        let plan = build_plan(MigrationSource::Hermes, &data, &config, dir.path());

        assert!(plan.credentials_to_add.is_empty());
    }

    #[test]
    fn test_build_plan_skips_existing_memory() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("memory")).unwrap();
        std::fs::write(dir.path().join("memory/hermes-MEMORY.md"), "existing").unwrap();

        let data = SourceData {
            memory_files: vec![
                (dir.path().join("src.md"), "hermes-MEMORY.md".into()),
                (dir.path().join("src2.md"), "hermes-USER.md".into()),
            ],
            ..Default::default()
        };

        let config = default_config();
        let plan = build_plan(MigrationSource::Hermes, &data, &config, dir.path());

        assert_eq!(plan.memory_files.len(), 1);
        assert_eq!(plan.memory_files[0].1, "hermes-USER.md");
    }

    #[test]
    fn test_build_plan_skips_existing_identity() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("IDENTITY.md"), "existing persona").unwrap();

        let data = SourceData {
            persona_file: Some(dir.path().join("SOUL.md")),
            ..Default::default()
        };

        let config = default_config();
        let plan = build_plan(MigrationSource::Hermes, &data, &config, dir.path());

        assert!(plan.persona_file.is_none());
    }

    #[test]
    fn test_build_plan_empty_when_no_data() {
        let dir = TempDir::new().unwrap();
        let data = SourceData::default();
        let config = default_config();
        let plan = build_plan(MigrationSource::Hermes, &data, &config, dir.path());

        assert!(plan.is_empty());
    }

    #[test]
    fn test_plan_skips_non_default_config() {
        let dir = TempDir::new().unwrap();
        let data = SourceData {
            config_changes: vec![ConfigChange {
                section: "llm".into(),
                field: "model".into(),
                source_key: "model.default".into(),
                new_value: "gpt-4o".into(),
            }],
            ..Default::default()
        };

        let mut config = default_config();
        config.llm.model = "my-custom-model".to_string();

        let plan = build_plan(MigrationSource::Hermes, &data, &config, dir.path());

        assert_eq!(plan.config_changes.len(), 1);
        assert!(plan.config_changes[0].skipped);
    }
}
