use std::path::Path;

use anyhow::Result;

use super::{ConfigChange, MigrationCategories, MigrationSource, SourceData};

/// Known API key env var names to look for in .env files.
const API_KEY_VARS: &[&str] = &[
    "OPENROUTER_API_KEY",
    "OPENAI_API_KEY",
    "ANTHROPIC_API_KEY",
    "GEMINI_API_KEY",
    "DEEPSEEK_API_KEY",
    "GROQ_API_KEY",
];

const CHANNEL_TOKEN_VARS: &[&str] = &[
    "TELEGRAM_BOT_TOKEN",
    "SLACK_BOT_TOKEN",
    "SLACK_SIGNING_SECRET",
    "DISCORD_BOT_TOKEN",
];

/// Parse Hermes Agent data from its default location.
pub fn parse(categories: &MigrationCategories) -> Result<SourceData> {
    let root = MigrationSource::Hermes.data_dir();
    parse_from(&root, categories)
}

/// Parse Hermes Agent data from a specific root directory.
pub fn parse_from(root: &Path, categories: &MigrationCategories) -> Result<SourceData> {
    let mut data = SourceData::default();

    if categories.config {
        parse_config(root, &mut data)?;
    }

    if categories.credentials {
        parse_credentials(root, &mut data)?;
    }

    if categories.memory {
        parse_memory(root, &mut data);
    }

    if categories.persona {
        parse_persona(root, &mut data);
    }

    if categories.skills {
        parse_skills(root, &mut data);
    }

    Ok(data)
}

fn parse_config(root: &Path, data: &mut SourceData) -> Result<()> {
    let config_path = root.join("config.yaml");
    if !config_path.exists() {
        return Ok(());
    }

    let content = std::fs::read_to_string(&config_path)?;
    let yaml: serde_yaml::Value = serde_yaml::from_str(&content)?;

    // model.default -> llm.model
    if let Some(model) = yaml
        .get("model")
        .and_then(|m| m.get("default"))
        .and_then(|v| v.as_str())
    {
        data.config_changes.push(ConfigChange {
            section: "llm".into(),
            field: "model".into(),
            source_key: "model.default".into(),
            new_value: model.to_string(),
        });
    }

    // model.provider -> llm.provider
    if let Some(provider) = yaml
        .get("model")
        .and_then(|m| m.get("provider"))
        .and_then(|v| v.as_str())
    {
        if provider != "auto" {
            data.config_changes.push(ConfigChange {
                section: "llm".into(),
                field: "provider".into(),
                source_key: "model.provider".into(),
                new_value: map_hermes_provider(provider),
            });
        }
    }

    // terminal.timeout -> tools.default_timeout_ms
    if let Some(timeout) = yaml
        .get("terminal")
        .and_then(|t| t.get("timeout"))
        .and_then(serde_yaml::Value::as_u64)
    {
        data.config_changes.push(ConfigChange {
            section: "tools".into(),
            field: "default_timeout_ms".into(),
            source_key: "terminal.timeout".into(),
            new_value: (timeout * 1000).to_string(),
        });
    }

    // browser.headless -> browser.headless
    if let Some(headless) = yaml
        .get("browser")
        .and_then(|b| b.get("headless"))
        .and_then(serde_yaml::Value::as_bool)
    {
        data.config_changes.push(ConfigChange {
            section: "browser".into(),
            field: "headless".into(),
            source_key: "browser.headless".into(),
            new_value: headless.to_string(),
        });
    }

    // timezone -> user.timezone
    if let Some(tz) = yaml.get("timezone").and_then(|v| v.as_str()) {
        data.config_changes.push(ConfigChange {
            section: "user".into(),
            field: "timezone".into(),
            source_key: "timezone".into(),
            new_value: tz.to_string(),
        });
    }

    // compression.enabled -> compaction (if model specified)
    if let Some(model) = yaml
        .get("compression")
        .and_then(|c| c.get("summary_model"))
        .and_then(|v| v.as_str())
    {
        data.config_changes.push(ConfigChange {
            section: "compaction".into(),
            field: "model".into(),
            source_key: "compression.summary_model".into(),
            new_value: model.to_string(),
        });
    }

    // tts.provider -> tts.enabled + tts config
    if let Some(provider) = yaml
        .get("tts")
        .and_then(|t| t.get("provider"))
        .and_then(|v| v.as_str())
    {
        data.config_changes.push(ConfigChange {
            section: "tts".into(),
            field: "enabled".into(),
            source_key: "tts.provider".into(),
            new_value: "true".to_string(),
        });
        // Store provider info as a note
        data.config_changes.push(ConfigChange {
            section: "tts".into(),
            field: "default_voice".into(),
            source_key: format!("tts.provider={provider}"),
            new_value: get_tts_voice(&yaml, provider),
        });
    }

    Ok(())
}

fn get_tts_voice(yaml: &serde_yaml::Value, provider: &str) -> String {
    // Try tts.<provider>.voice or tts.<provider>.voice_id
    if let Some(voice) = yaml
        .get("tts")
        .and_then(|t| t.get(provider))
        .and_then(|p| p.get("voice_id").or_else(|| p.get("voice")))
        .and_then(|v| v.as_str())
    {
        return voice.to_string();
    }
    "alloy".to_string()
}

fn map_hermes_provider(provider: &str) -> String {
    super::map_provider_name(provider).to_string()
}

fn parse_credentials(root: &Path, data: &mut SourceData) -> Result<()> {
    let env_path = root.join(".env");
    if !env_path.exists() {
        return Ok(());
    }
    let content = std::fs::read_to_string(&env_path)?;
    parse_env_content(&content, data);
    Ok(())
}

pub(crate) fn parse_env_content(content: &str, data: &mut SourceData) {
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            let key = key.trim();
            let value = value.trim().trim_matches('"').trim_matches('\'');
            if value.is_empty() {
                continue;
            }
            let is_known = API_KEY_VARS.contains(&key) || CHANNEL_TOKEN_VARS.contains(&key);
            if is_known {
                data.credentials.push((key.to_string(), value.to_string()));
            }
        }
    }
}

fn parse_memory(root: &Path, data: &mut SourceData) {
    let memories_dir = root.join("memories");

    // MEMORY.md
    let memory_md = memories_dir.join("MEMORY.md");
    if memory_md.exists() {
        data.memory_files
            .push((memory_md, "hermes-MEMORY.md".to_string()));
    }

    // USER.md
    let user_md = memories_dir.join("USER.md");
    if user_md.exists() {
        data.memory_files
            .push((user_md, "hermes-USER.md".to_string()));
    }

    // Also check root-level MEMORY.md (some installations)
    let root_memory = root.join("MEMORY.md");
    if root_memory.exists() && !memories_dir.join("MEMORY.md").exists() {
        data.memory_files
            .push((root_memory, "hermes-MEMORY.md".to_string()));
    }
}

fn parse_persona(root: &Path, data: &mut SourceData) {
    let soul_path = root.join("SOUL.md");
    if soul_path.exists() {
        data.persona_file = Some(soul_path);
    }
}

fn parse_skills(root: &Path, data: &mut SourceData) {
    let skills_dir = root.join("skills");
    if !skills_dir.is_dir() {
        return;
    }
    if let Ok(entries) = std::fs::read_dir(&skills_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                    let name = name.to_string();
                    data.skill_dirs.push((path, name));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn setup_hermes_dir() -> TempDir {
        let dir = TempDir::new().unwrap();
        let root = dir.path();

        // config.yaml
        std::fs::write(
            root.join("config.yaml"),
            r#"
model:
  default: "anthropic/claude-sonnet-4"
  provider: "anthropic"

terminal:
  timeout: 60

browser:
  headless: false

timezone: "America/New_York"

compression:
  summary_model: "anthropic/claude-haiku-4-5"

tts:
  provider: "openai"
  openai:
    voice: "nova"
"#,
        )
        .unwrap();

        // .env
        std::fs::write(
            root.join(".env"),
            r#"
ANTHROPIC_API_KEY=sk-ant-test123
TELEGRAM_BOT_TOKEN=123456:ABC-DEF
UNRELATED_KEY=should-be-ignored
"#,
        )
        .unwrap();

        // SOUL.md
        std::fs::write(root.join("SOUL.md"), "# My Persona\nI am helpful.").unwrap();

        // memories/
        std::fs::create_dir_all(root.join("memories")).unwrap();
        std::fs::write(root.join("memories/MEMORY.md"), "# Memory\nSome notes.").unwrap();
        std::fs::write(root.join("memories/USER.md"), "# User\nUser info.").unwrap();

        // skills/
        std::fs::create_dir_all(root.join("skills/research")).unwrap();
        std::fs::write(
            root.join("skills/research/SKILL.md"),
            "---\nname: research\n---\n# Research",
        )
        .unwrap();

        dir
    }

    #[test]
    fn test_parse_hermes_config() {
        let dir = setup_hermes_dir();
        let categories = MigrationCategories {
            config: true,
            credentials: false,
            memory: false,
            persona: false,
            skills: false,
        };
        let data = parse_from(dir.path(), &categories).unwrap();

        assert!(!data.config_changes.is_empty());

        let model_change = data
            .config_changes
            .iter()
            .find(|c| c.field == "model" && c.section == "llm")
            .unwrap();
        assert_eq!(model_change.new_value, "anthropic/claude-sonnet-4");

        let provider_change = data
            .config_changes
            .iter()
            .find(|c| c.field == "provider" && c.section == "llm")
            .unwrap();
        assert_eq!(provider_change.new_value, "anthropic");

        let timeout_change = data
            .config_changes
            .iter()
            .find(|c| c.field == "default_timeout_ms")
            .unwrap();
        assert_eq!(timeout_change.new_value, "60000");

        let browser_change = data
            .config_changes
            .iter()
            .find(|c| c.field == "headless")
            .unwrap();
        assert_eq!(browser_change.new_value, "false");

        let tz_change = data
            .config_changes
            .iter()
            .find(|c| c.field == "timezone")
            .unwrap();
        assert_eq!(tz_change.new_value, "America/New_York");
    }

    #[test]
    fn test_parse_hermes_env() {
        let dir = setup_hermes_dir();
        let categories = MigrationCategories {
            config: false,
            credentials: true,
            memory: false,
            persona: false,
            skills: false,
        };
        let data = parse_from(dir.path(), &categories).unwrap();

        assert_eq!(data.credentials.len(), 2);
        assert!(data
            .credentials
            .iter()
            .any(|(k, v)| k == "ANTHROPIC_API_KEY" && v == "sk-ant-test123"));
        assert!(data
            .credentials
            .iter()
            .any(|(k, v)| k == "TELEGRAM_BOT_TOKEN" && v == "123456:ABC-DEF"));
    }

    #[test]
    fn test_parse_hermes_memory() {
        let dir = setup_hermes_dir();
        let categories = MigrationCategories {
            config: false,
            credentials: false,
            memory: true,
            persona: false,
            skills: false,
        };
        let data = parse_from(dir.path(), &categories).unwrap();

        assert_eq!(data.memory_files.len(), 2);
        assert!(data
            .memory_files
            .iter()
            .any(|(_, d)| d == "hermes-MEMORY.md"));
        assert!(data.memory_files.iter().any(|(_, d)| d == "hermes-USER.md"));
    }

    #[test]
    fn test_parse_hermes_persona() {
        let dir = setup_hermes_dir();
        let categories = MigrationCategories {
            config: false,
            credentials: false,
            memory: false,
            persona: true,
            skills: false,
        };
        let data = parse_from(dir.path(), &categories).unwrap();
        assert!(data.persona_file.is_some());
    }

    #[test]
    fn test_parse_hermes_skills() {
        let dir = setup_hermes_dir();
        let categories = MigrationCategories {
            config: false,
            credentials: false,
            memory: false,
            persona: false,
            skills: true,
        };
        let data = parse_from(dir.path(), &categories).unwrap();
        assert_eq!(data.skill_dirs.len(), 1);
        assert_eq!(data.skill_dirs[0].1, "research");
    }

    #[test]
    fn test_hermes_timeout_conversion() {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("config.yaml"),
            "terminal:\n  timeout: 120\n",
        )
        .unwrap();

        let categories = MigrationCategories {
            config: true,
            credentials: false,
            memory: false,
            persona: false,
            skills: false,
        };
        let data = parse_from(dir.path(), &categories).unwrap();

        let change = data
            .config_changes
            .iter()
            .find(|c| c.field == "default_timeout_ms")
            .unwrap();
        assert_eq!(change.new_value, "120000");
    }

    #[test]
    fn test_hermes_model_mapping() {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("config.yaml"),
            "model:\n  default: \"gpt-4o\"\n  provider: \"openai\"\n",
        )
        .unwrap();

        let categories = MigrationCategories {
            config: true,
            credentials: false,
            memory: false,
            persona: false,
            skills: false,
        };
        let data = parse_from(dir.path(), &categories).unwrap();

        let model = data
            .config_changes
            .iter()
            .find(|c| c.field == "model")
            .unwrap();
        assert_eq!(model.new_value, "gpt-4o");

        let provider = data
            .config_changes
            .iter()
            .find(|c| c.field == "provider")
            .unwrap();
        assert_eq!(provider.new_value, "openai");
    }

    #[test]
    fn test_parse_env_content() {
        let mut data = SourceData::default();
        parse_env_content(
            r#"
# comment
OPENAI_API_KEY=sk-test123
ANTHROPIC_API_KEY="sk-ant-test456"
EMPTY_KEY=
UNKNOWN=value
TELEGRAM_BOT_TOKEN='bot123'
"#,
            &mut data,
        );

        assert_eq!(data.credentials.len(), 3);
        assert!(data
            .credentials
            .iter()
            .any(|(k, v)| k == "OPENAI_API_KEY" && v == "sk-test123"));
        assert!(data
            .credentials
            .iter()
            .any(|(k, v)| k == "ANTHROPIC_API_KEY" && v == "sk-ant-test456"));
        assert!(data
            .credentials
            .iter()
            .any(|(k, v)| k == "TELEGRAM_BOT_TOKEN" && v == "bot123"));
    }

    #[test]
    fn test_missing_config_file_is_ok() {
        let dir = TempDir::new().unwrap();
        let categories = MigrationCategories::default();
        let data = parse_from(dir.path(), &categories).unwrap();
        assert!(data.config_changes.is_empty());
        assert!(data.credentials.is_empty());
    }

    #[test]
    fn test_auto_provider_skipped() {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("config.yaml"),
            "model:\n  provider: \"auto\"\n",
        )
        .unwrap();

        let categories = MigrationCategories {
            config: true,
            ..Default::default()
        };
        let data = parse_from(dir.path(), &categories).unwrap();

        assert!(!data.config_changes.iter().any(|c| c.field == "provider"));
    }
}
