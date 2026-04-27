use std::path::Path;

use anyhow::Result;

use super::{ConfigChange, MigrationCategories, MigrationSource, SourceData};

// API key vars are handled by hermes::parse_env_content (shared .env parser).

/// Parse OpenClaw data from its default location.
pub fn parse(categories: &MigrationCategories) -> Result<SourceData> {
    let root = MigrationSource::OpenClaw.data_dir();
    parse_from(&root, categories)
}

/// Parse OpenClaw data from a specific root directory.
pub fn parse_from(root: &Path, categories: &MigrationCategories) -> Result<SourceData> {
    let mut data = SourceData::default();

    if categories.config {
        parse_config(
            root,
            &mut data,
            categories.credentials, /* extract_channel_tokens */
        )?;
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

fn parse_config(root: &Path, data: &mut SourceData, extract_channel_tokens: bool) -> Result<()> {
    let config_path = root.join("openclaw.json");
    if !config_path.exists() {
        return Ok(());
    }

    let content = std::fs::read_to_string(&config_path)?;
    // Strip comments for JSON5 compatibility (single-line // comments)
    let cleaned = strip_json_comments(&content);
    let json: serde_json::Value = serde_json::from_str(&cleaned)?;

    // agents.defaults.model -> llm.model
    if let Some(model) = json
        .pointer("/agents/defaults/model")
        .and_then(|v| v.as_str())
    {
        data.config_changes.push(ConfigChange {
            section: "llm".into(),
            field: "model".into(),
            source_key: "agents.defaults.model".into(),
            new_value: model.to_string(),
        });
    }

    // Detect provider from models.providers (use first configured)
    if let Some(providers) = json
        .pointer("/models/providers")
        .and_then(|v| v.as_object())
    {
        if let Some((name, _)) = providers.iter().next() {
            let mapped = map_openclaw_provider(name);
            data.config_changes.push(ConfigChange {
                section: "llm".into(),
                field: "provider".into(),
                source_key: format!("models.providers.{name}"),
                new_value: mapped.to_string(),
            });
        }
    }

    // agents.defaults.timeoutSeconds -> tools.default_timeout_ms
    if let Some(timeout) = json
        .pointer("/agents/defaults/timeoutSeconds")
        .and_then(serde_json::Value::as_u64)
    {
        data.config_changes.push(ConfigChange {
            section: "tools".into(),
            field: "default_timeout_ms".into(),
            source_key: "agents.defaults.timeoutSeconds".into(),
            new_value: (timeout * 1000).to_string(),
        });
    }

    // agents.defaults.sandbox -> sandbox config
    if let Some(sandbox) = json.pointer("/agents/defaults/sandbox") {
        if let Some(enabled) = sandbox.get("enabled").and_then(serde_json::Value::as_bool) {
            data.config_changes.push(ConfigChange {
                section: "sandbox".into(),
                field: "enabled".into(),
                source_key: "agents.defaults.sandbox.enabled".into(),
                new_value: enabled.to_string(),
            });
        }
    }

    // browser.headless -> browser.headless
    if let Some(headless) = json
        .pointer("/browser/headless")
        .and_then(serde_json::Value::as_bool)
    {
        data.config_changes.push(ConfigChange {
            section: "browser".into(),
            field: "headless".into(),
            source_key: "browser.headless".into(),
            new_value: headless.to_string(),
        });
    }

    // agents.defaults.userTimezone -> user.timezone
    if let Some(tz) = json
        .pointer("/agents/defaults/userTimezone")
        .and_then(|v| v.as_str())
    {
        data.config_changes.push(ConfigChange {
            section: "user".into(),
            field: "timezone".into(),
            source_key: "agents.defaults.userTimezone".into(),
            new_value: tz.to_string(),
        });
    }

    // Channel tokens from config (only if credentials category is enabled)
    if extract_channel_tokens {
        extract_channel_token(
            &json,
            "/channels/telegram/botToken",
            "TELEGRAM_BOT_TOKEN",
            data,
        );
        extract_channel_token(&json, "/channels/slack/botToken", "SLACK_BOT_TOKEN", data);
        extract_channel_token(&json, "/channels/discord/token", "DISCORD_BOT_TOKEN", data);
    }

    Ok(())
}

fn extract_channel_token(
    json: &serde_json::Value,
    pointer: &str,
    env_name: &str,
    data: &mut SourceData,
) {
    if let Some(value) = json.pointer(pointer) {
        let token = if let Some(s) = value.as_str() {
            // Plain string or env template
            if s.starts_with("${") && s.ends_with('}') {
                return; // env template, don't extract
            }
            s.to_string()
        } else if let Some(obj) = value.as_object() {
            // SecretRef object
            if obj.get("source").and_then(|v| v.as_str()) == Some("env") {
                return; // env reference, resolved from .env
            }
            return;
        } else {
            return;
        };

        if !token.is_empty() {
            // Only add if not already present from .env parsing
            if !data.credentials.iter().any(|(k, _)| k == env_name) {
                data.credentials.push((env_name.to_string(), token));
            }
        }
    }
}

fn map_openclaw_provider(provider: &str) -> &'static str {
    super::map_provider_name(provider)
}

fn strip_json_comments(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut in_string = false;
    let mut escape_next = false;
    let bytes = input.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        let ch = bytes[i];

        if escape_next {
            out.push(ch as char);
            escape_next = false;
            i += 1;
            continue;
        }

        if in_string {
            if ch == b'\\' {
                escape_next = true;
            } else if ch == b'"' {
                in_string = false;
            }
            out.push(ch as char);
            i += 1;
            continue;
        }

        if ch == b'"' {
            in_string = true;
            out.push(ch as char);
            i += 1;
            continue;
        }

        // Single-line comment
        if ch == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }

        // Block comment
        if ch == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            i += 2; // skip */
            continue;
        }

        out.push(ch as char);
        i += 1;
    }

    // Strip trailing commas before } or ] (handles spaces, tabs, newlines)
    strip_trailing_commas(&out)
}

fn strip_trailing_commas(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i] == b',' {
            // Look ahead past whitespace for } or ]
            let mut j = i + 1;
            while j < bytes.len() && matches!(bytes[j], b' ' | b'\t' | b'\n' | b'\r') {
                j += 1;
            }
            if j < bytes.len() && matches!(bytes[j], b'}' | b']') {
                // Skip the comma, keep the whitespace
                i += 1;
                continue;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }

    out
}

fn parse_credentials(root: &Path, data: &mut SourceData) -> Result<()> {
    // Parse .env file
    let env_path = root.join(".env");
    if env_path.exists() {
        let content = std::fs::read_to_string(&env_path)?;
        super::hermes::parse_env_content(&content, data);
    }

    // Parse auth-profiles.json
    let auth_path = root.join("auth-profiles.json");
    if auth_path.exists() {
        if let Ok(content) = std::fs::read_to_string(&auth_path) {
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(&content) {
                extract_auth_profile_keys(&json, data);
            }
        }
    }

    // Also check agents/main/agent/auth-profiles.json
    let agent_auth_path = root.join("agents/main/agent/auth-profiles.json");
    if agent_auth_path.exists() {
        if let Ok(content) = std::fs::read_to_string(&agent_auth_path) {
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(&content) {
                extract_auth_profile_keys(&json, data);
            }
        }
    }

    Ok(())
}

fn extract_auth_profile_keys(json: &serde_json::Value, data: &mut SourceData) {
    // auth-profiles.json is typically an object with profile IDs as keys
    // Each profile may contain apiKey or token fields
    if let Some(obj) = json.as_object() {
        for (_profile_id, profile) in obj {
            if let Some(api_key) = profile.get("apiKey").and_then(|v| v.as_str()) {
                // Try to map the key to a known env var based on the provider
                if let Some(provider) = profile.get("provider").and_then(|v| v.as_str()) {
                    if let Some(env_var) = provider_to_env_var(provider) {
                        if !data.credentials.iter().any(|(k, _)| k == env_var) {
                            data.credentials
                                .push((env_var.to_string(), api_key.to_string()));
                        }
                    }
                }
            }
        }
    }
}

fn provider_to_env_var(provider: &str) -> Option<&'static str> {
    match provider.to_lowercase().as_str() {
        "openrouter" => Some("OPENROUTER_API_KEY"),
        "anthropic" => Some("ANTHROPIC_API_KEY"),
        "openai" => Some("OPENAI_API_KEY"),
        "google" | "gemini" => Some("GEMINI_API_KEY"),
        "deepseek" => Some("DEEPSEEK_API_KEY"),
        "groq" => Some("GROQ_API_KEY"),
        _ => None,
    }
}

fn parse_memory(root: &Path, data: &mut SourceData) {
    let workspace = root.join("workspace");

    let memory_md = workspace.join("MEMORY.md");
    if memory_md.exists() {
        data.memory_files
            .push((memory_md, "openclaw-MEMORY.md".to_string()));
    }

    let user_md = workspace.join("USER.md");
    if user_md.exists() {
        data.memory_files
            .push((user_md, "openclaw-USER.md".to_string()));
    }

    // Also check workspace/memory/*.md
    let memory_dir = workspace.join("memory");
    if memory_dir.is_dir() {
        if let Ok(entries) = std::fs::read_dir(&memory_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().is_some_and(|e| e == "md") {
                    if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                        let dest = format!("openclaw-{name}");
                        data.memory_files.push((path, dest));
                    }
                }
            }
        }
    }
}

fn parse_persona(root: &Path, data: &mut SourceData) {
    let soul_path = root.join("workspace/SOUL.md");
    if soul_path.exists() {
        data.persona_file = Some(soul_path);
    }
}

fn parse_skills(root: &Path, data: &mut SourceData) {
    // workspace/skills/
    let workspace_skills = root.join("workspace/skills");
    collect_skill_dirs(&workspace_skills, data);

    // ~/.openclaw/skills/ (managed/shared)
    let managed_skills = root.join("skills");
    collect_skill_dirs(&managed_skills, data);
}

fn collect_skill_dirs(dir: &Path, data: &mut SourceData) {
    if !dir.is_dir() {
        return;
    }
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                    let name = name.to_string();
                    // Avoid duplicates
                    if !data.skill_dirs.iter().any(|(_, n)| n == &name) {
                        data.skill_dirs.push((path, name));
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn setup_openclaw_dir() -> TempDir {
        let dir = TempDir::new().unwrap();
        let root = dir.path();

        // openclaw.json
        std::fs::write(
            root.join("openclaw.json"),
            r#"{
  "agents": {
    "defaults": {
      "model": "anthropic/claude-sonnet-4",
      "timeoutSeconds": 300,
      "userTimezone": "Europe/London",
      "sandbox": {
        "enabled": true
      }
    }
  },
  "models": {
    "providers": {
      "anthropic": {
        "baseUrl": "https://api.anthropic.com"
      }
    }
  },
  "browser": {
    "headless": true
  },
  "channels": {
    "telegram": {
      "botToken": "tg-token-123"
    }
  }
}"#,
        )
        .unwrap();

        // .env
        std::fs::write(
            root.join(".env"),
            "ANTHROPIC_API_KEY=sk-ant-openclaw\nSLACK_BOT_TOKEN=xoxb-slack\n",
        )
        .unwrap();

        // workspace/
        std::fs::create_dir_all(root.join("workspace/skills/my-skill")).unwrap();
        std::fs::create_dir_all(root.join("workspace/memory")).unwrap();
        std::fs::write(root.join("workspace/SOUL.md"), "# OpenClaw Persona").unwrap();
        std::fs::write(root.join("workspace/MEMORY.md"), "# Memories").unwrap();
        std::fs::write(root.join("workspace/USER.md"), "# User Profile").unwrap();
        std::fs::write(root.join("workspace/memory/daily.md"), "# Daily notes").unwrap();
        std::fs::write(
            root.join("workspace/skills/my-skill/SKILL.md"),
            "---\nname: my-skill\n---",
        )
        .unwrap();

        // managed skills/
        std::fs::create_dir_all(root.join("skills/shared-skill")).unwrap();
        std::fs::write(
            root.join("skills/shared-skill/SKILL.md"),
            "---\nname: shared-skill\n---",
        )
        .unwrap();

        dir
    }

    #[test]
    fn test_parse_openclaw_config() {
        let dir = setup_openclaw_dir();
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
        assert_eq!(model.new_value, "anthropic/claude-sonnet-4");

        let provider = data
            .config_changes
            .iter()
            .find(|c| c.field == "provider")
            .unwrap();
        assert_eq!(provider.new_value, "anthropic");

        let timeout = data
            .config_changes
            .iter()
            .find(|c| c.field == "default_timeout_ms")
            .unwrap();
        assert_eq!(timeout.new_value, "300000");

        let sandbox = data
            .config_changes
            .iter()
            .find(|c| c.field == "enabled" && c.section == "sandbox")
            .unwrap();
        assert_eq!(sandbox.new_value, "true");

        let tz = data
            .config_changes
            .iter()
            .find(|c| c.field == "timezone")
            .unwrap();
        assert_eq!(tz.new_value, "Europe/London");
    }

    #[test]
    fn test_parse_openclaw_credentials() {
        let dir = setup_openclaw_dir();
        let categories = MigrationCategories {
            config: false,
            credentials: true,
            memory: false,
            persona: false,
            skills: false,
        };
        let data = parse_from(dir.path(), &categories).unwrap();

        assert!(data
            .credentials
            .iter()
            .any(|(k, v)| k == "ANTHROPIC_API_KEY" && v == "sk-ant-openclaw"));
        assert!(data
            .credentials
            .iter()
            .any(|(k, v)| k == "SLACK_BOT_TOKEN" && v == "xoxb-slack"));
    }

    #[test]
    fn test_parse_openclaw_channel_tokens_from_config() {
        let dir = setup_openclaw_dir();
        // Channel tokens from config are only extracted when credentials category is enabled
        let categories = MigrationCategories {
            config: true,
            credentials: true,
            memory: false,
            persona: false,
            skills: false,
        };
        let data = parse_from(dir.path(), &categories).unwrap();

        // Telegram token from config channels section
        assert!(data
            .credentials
            .iter()
            .any(|(k, v)| k == "TELEGRAM_BOT_TOKEN" && v == "tg-token-123"));
    }

    #[test]
    fn test_channel_tokens_not_extracted_without_credentials_category() {
        let dir = setup_openclaw_dir();
        let categories = MigrationCategories {
            config: true,
            credentials: false,
            memory: false,
            persona: false,
            skills: false,
        };
        let data = parse_from(dir.path(), &categories).unwrap();

        // Channel tokens should NOT be extracted when credentials=false
        assert!(!data
            .credentials
            .iter()
            .any(|(k, _)| k == "TELEGRAM_BOT_TOKEN"));
    }

    #[test]
    fn test_parse_openclaw_memory() {
        let dir = setup_openclaw_dir();
        let categories = MigrationCategories {
            config: false,
            credentials: false,
            memory: true,
            persona: false,
            skills: false,
        };
        let data = parse_from(dir.path(), &categories).unwrap();

        assert!(data
            .memory_files
            .iter()
            .any(|(_, d)| d == "openclaw-MEMORY.md"));
        assert!(data
            .memory_files
            .iter()
            .any(|(_, d)| d == "openclaw-USER.md"));
        assert!(data
            .memory_files
            .iter()
            .any(|(_, d)| d == "openclaw-daily.md"));
    }

    #[test]
    fn test_parse_openclaw_persona() {
        let dir = setup_openclaw_dir();
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
    fn test_parse_openclaw_skills() {
        let dir = setup_openclaw_dir();
        let categories = MigrationCategories {
            config: false,
            credentials: false,
            memory: false,
            persona: false,
            skills: true,
        };
        let data = parse_from(dir.path(), &categories).unwrap();

        assert_eq!(data.skill_dirs.len(), 2);
        let names: Vec<&str> = data.skill_dirs.iter().map(|(_, n)| n.as_str()).collect();
        assert!(names.contains(&"my-skill"));
        assert!(names.contains(&"shared-skill"));
    }

    #[test]
    fn test_parse_openclaw_auth_profiles() {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("auth-profiles.json"),
            r#"{
                "profile1": {
                    "provider": "openai",
                    "apiKey": "sk-openai-from-profile"
                },
                "profile2": {
                    "provider": "anthropic",
                    "apiKey": "sk-ant-from-profile"
                }
            }"#,
        )
        .unwrap();

        let categories = MigrationCategories {
            config: false,
            credentials: true,
            memory: false,
            persona: false,
            skills: false,
        };
        let data = parse_from(dir.path(), &categories).unwrap();

        assert!(data
            .credentials
            .iter()
            .any(|(k, v)| k == "OPENAI_API_KEY" && v == "sk-openai-from-profile"));
        assert!(data
            .credentials
            .iter()
            .any(|(k, v)| k == "ANTHROPIC_API_KEY" && v == "sk-ant-from-profile"));
    }

    #[test]
    fn test_openclaw_provider_mapping() {
        assert_eq!(map_openclaw_provider("anthropic"), "anthropic");
        assert_eq!(map_openclaw_provider("openai"), "openai");
        assert_eq!(map_openclaw_provider("google"), "gemini");
        assert_eq!(map_openclaw_provider("Gemini"), "gemini");
        assert_eq!(map_openclaw_provider("unknown-provider"), "openrouter");
    }

    #[test]
    fn test_strip_json_comments() {
        let input = r#"{
  // This is a comment
  "key": "value", // trailing comment
  /* block comment */
  "key2": "value2",
}"#;
        let cleaned = strip_json_comments(input);
        let parsed: serde_json::Value = serde_json::from_str(&cleaned).unwrap();
        assert_eq!(parsed["key"], "value");
        assert_eq!(parsed["key2"], "value2");
    }

    #[test]
    fn test_strip_json_comments_preserves_urls() {
        let input = r#"{
  "baseUrl": "https://api.example.com/v1",
  "callback": "http://localhost:8080"
}"#;
        let cleaned = strip_json_comments(input);
        let parsed: serde_json::Value = serde_json::from_str(&cleaned).unwrap();
        assert_eq!(parsed["baseUrl"], "https://api.example.com/v1");
        assert_eq!(parsed["callback"], "http://localhost:8080");
    }

    #[test]
    fn test_missing_openclaw_json_is_ok() {
        let dir = TempDir::new().unwrap();
        let categories = MigrationCategories::default();
        let data = parse_from(dir.path(), &categories).unwrap();
        assert!(data.config_changes.is_empty());
    }
}
