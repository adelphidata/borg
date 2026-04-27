use anyhow::Result;

use crate::config::Config;
use crate::skills::{load_all_skills, Skill};

use super::require_str_param;

pub fn handle_list_skills(config: &Config) -> Result<String> {
    let resolved_creds = config.resolve_credentials();
    let skills = load_all_skills(&resolved_creds, &config.skills)?;
    if skills.is_empty() {
        Ok("No skills installed.".to_string())
    } else {
        Ok(skills
            .iter()
            .map(Skill::summary_line)
            .collect::<Vec<_>>()
            .join("\n"))
    }
}

/// Unified list handler: dispatches based on `what` parameter.
pub fn handle_list(
    args: &serde_json::Value,
    config: &Config,
    agent_control: Option<&crate::multi_agent::AgentControl>,
) -> Result<String> {
    let what = require_str_param(args, "what")?;
    match what {
        "skills" => handle_list_skills(config),
        "channels" => handle_list_channels(config),
        "agents" => {
            if let Some(ctrl) = agent_control {
                crate::multi_agent::tools::handle_list_agents(ctrl)
            } else {
                Ok("Multi-agent system is not enabled.".to_string())
            }
        }
        other => Ok(format!(
            "Unknown list target: {other}. Use: tools, skills, channels, agents."
        )),
    }
}

pub fn handle_list_channels(config: &Config) -> Result<String> {
    let mut channels = Vec::new();

    // Script-based channels from ~/.borg/channels/
    if let Ok(channels_dir) = Config::channels_dir() {
        if channels_dir.exists() {
            if let Ok(entries) = std::fs::read_dir(&channels_dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.is_dir() {
                        let manifest_path = path.join("channel.toml");
                        if manifest_path.exists() {
                            if let Ok(content) = std::fs::read_to_string(&manifest_path) {
                                if let Ok(manifest) = toml::from_str::<toml::Value>(&content) {
                                    let name = manifest
                                        .get("name")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("?");
                                    let desc = manifest
                                        .get("description")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("");
                                    channels.push(format!("{name}: {desc}"));
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // Native channels detected via credentials
    for (name, desc) in config.detected_native_channels() {
        let prefix = format!("{name}:");
        if !channels.iter().any(|c| c.starts_with(&prefix)) {
            channels.push(format!("{name}: {desc}"));
        }
    }

    Ok(if channels.is_empty() {
        "No channels installed.".to_string()
    } else {
        channels.join("\n")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn list_unknown_what() {
        let config = Config::default();
        let args = json!({"what": "unknown"});
        let result = handle_list(&args, &config, None).unwrap();
        assert!(result.contains("Unknown list target"));
    }

    #[test]
    fn list_missing_what() {
        let config = Config::default();
        let args = json!({});
        let result = handle_list(&args, &config, None);
        assert!(result.is_err());
    }

    #[test]
    fn list_agents_without_control() {
        let config = Config::default();
        let args = json!({"what": "agents"});
        let result = handle_list(&args, &config, None).unwrap();
        assert!(result.contains("not enabled"));
    }

    #[test]
    fn list_skills_dispatches() {
        let config = Config::default();
        let args = json!({"what": "skills"});
        let result = handle_list(&args, &config, None);
        assert!(result.is_ok());
    }

    #[test]
    fn list_skills_output_contains_skill_names() {
        let config = Config::default();
        let output = handle_list_skills(&config).unwrap();
        assert!(
            output.contains("] slack ("),
            "output should list slack skill, got:\n{output}"
        );
        assert!(
            output.contains("] git ("),
            "output should list git skill, got:\n{output}"
        );
        assert!(
            output.contains("[✓]") || output.contains("[✗]") || output.contains("[—]"),
            "output should contain status markers"
        );
    }

    #[test]
    fn list_channels_dispatches() {
        let config = Config::default();
        let args = json!({"what": "channels"});
        let result = handle_list(&args, &config, None);
        assert!(result.is_ok());
    }

    /// Mutex to prevent env-var–mutating channel tests from racing each other.
    static CHANNEL_ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn list_channels_includes_native_telegram() {
        let _lock = CHANNEL_ENV_MUTEX.lock().unwrap();
        std::env::set_var("TELEGRAM_BOT_TOKEN", "test-token-for-list-test");
        let config = Config::default();
        let result = handle_list_channels(&config).unwrap();
        assert!(
            result.contains("telegram"),
            "Should list native Telegram channel, got: {result}"
        );
        assert!(
            result.contains("native"),
            "Should indicate it's native, got: {result}"
        );
        std::env::remove_var("TELEGRAM_BOT_TOKEN");
    }

    #[test]
    fn list_channels_no_native_when_no_credentials() {
        let _lock = CHANNEL_ENV_MUTEX.lock().unwrap();
        let keys = [
            "TELEGRAM_BOT_TOKEN",
            "SLACK_BOT_TOKEN",
            "DISCORD_BOT_TOKEN",
            "TWILIO_ACCOUNT_SID",
            "TEAMS_APP_ID",
            "GOOGLE_CHAT_SERVICE_TOKEN",
        ];
        let saved: Vec<_> = keys.iter().map(|k| (*k, std::env::var(k).ok())).collect();
        for k in &keys {
            std::env::remove_var(k);
        }
        let config = Config::default();
        // Skip if the OS keychain has real credentials installed (e.g. dev machine)
        if config.detected_native_channels().is_empty() {
            let result = handle_list_channels(&config).unwrap();
            assert!(
                !result.contains("native"),
                "Should not list native channels without credentials, got: {result}"
            );
        }
        // Restore
        for (k, v) in saved {
            if let Some(val) = v {
                std::env::set_var(k, val);
            }
        }
    }
}
