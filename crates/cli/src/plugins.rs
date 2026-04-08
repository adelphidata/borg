use anyhow::{Context, Result};

use borg_core::config::Config;

pub struct PluginDef {
    pub name: &'static str,
    pub description: &'static str,
    pub category: &'static str,
    pub credentials: &'static [CredentialSpec],
    pub is_channel: bool,
}

pub struct CredentialSpec {
    pub key: &'static str,
    pub label: &'static str,
    pub help: &'static str,
}

pub const PLUGINS: &[PluginDef] = &[
    PluginDef {
        name: "telegram",
        description: "Telegram",
        category: "messaging",
        credentials: &[CredentialSpec {
            key: "TELEGRAM_BOT_TOKEN",
            label: "Bot Token",
            help: "Get from @BotFather on Telegram",
        }],
        is_channel: true,
    },
    PluginDef {
        name: "slack",
        description: "Slack",
        category: "messaging",
        credentials: &[
            CredentialSpec {
                key: "SLACK_BOT_TOKEN",
                label: "Bot Token",
                help: "Get from Slack App settings > OAuth & Permissions",
            },
            CredentialSpec {
                key: "SLACK_SIGNING_SECRET",
                label: "Signing Secret",
                help: "Get from Slack App settings > Basic Information",
            },
        ],
        is_channel: true,
    },
    PluginDef {
        name: "twilio",
        description: "WhatsApp + SMS via Twilio",
        category: "messaging",
        credentials: &[
            CredentialSpec {
                key: "TWILIO_ACCOUNT_SID",
                label: "Account SID",
                help: "Get from Twilio Console",
            },
            CredentialSpec {
                key: "TWILIO_AUTH_TOKEN",
                label: "Auth Token",
                help: "Get from Twilio Console",
            },
            CredentialSpec {
                key: "TWILIO_PHONE_NUMBER",
                label: "Phone Number",
                help: "Your Twilio phone number (e.g. +1234567890)",
            },
        ],
        is_channel: true,
    },
    PluginDef {
        name: "discord",
        description: "Discord",
        category: "messaging",
        credentials: &[
            CredentialSpec {
                key: "DISCORD_BOT_TOKEN",
                label: "Bot Token",
                help: "Get from Discord Developer Portal > Bot",
            },
            CredentialSpec {
                key: "DISCORD_PUBLIC_KEY",
                label: "Public Key",
                help: "Get from Discord Developer Portal > General Information",
            },
        ],
        is_channel: true,
    },
    PluginDef {
        name: "signal",
        description: "Signal Messenger via signal-cli",
        category: "messaging",
        credentials: &[
            CredentialSpec {
                key: "SIGNAL_CLI_URL",
                label: "signal-cli URL",
                help: "URL of the signal-cli REST API daemon (e.g. http://localhost:8080)",
            },
            CredentialSpec {
                key: "SIGNAL_ACCOUNT",
                label: "Phone Number",
                help: "Your Signal phone number (e.g. +15551234567)",
            },
        ],
        is_channel: true,
    },
    PluginDef {
        name: "teams",
        description: "Microsoft Teams",
        category: "messaging",
        credentials: &[
            CredentialSpec {
                key: "TEAMS_APP_ID",
                label: "App ID",
                help: "Get from Azure Portal > Bot registration",
            },
            CredentialSpec {
                key: "TEAMS_APP_SECRET",
                label: "App Secret",
                help: "Get from Azure Portal > Bot registration > Certificates & secrets",
            },
        ],
        is_channel: true,
    },
    PluginDef {
        name: "google-chat",
        description: "Google Chat",
        category: "messaging",
        credentials: &[CredentialSpec {
            key: "GOOGLE_CHAT_WEBHOOK_TOKEN",
            label: "Verification Token",
            help: "Get from Google Cloud Console > Chat API configuration",
        }],
        is_channel: true,
    },
    PluginDef {
        name: "gmail",
        description: "Gmail API",
        category: "email",
        credentials: &[CredentialSpec {
            key: "GMAIL_API_KEY",
            label: "API Key",
            help: "Get from Google Cloud Console",
        }],
        is_channel: false,
    },
    PluginDef {
        name: "google-calendar",
        description: "Google Calendar API",
        category: "productivity",
        credentials: &[CredentialSpec {
            key: "GOOGLE_CALENDAR_TOKEN",
            label: "Access Token",
            help: "Get from Google Cloud Console",
        }],
        is_channel: false,
    },
    PluginDef {
        name: "notion",
        description: "Notion API",
        category: "productivity",
        credentials: &[CredentialSpec {
            key: "NOTION_API_KEY",
            label: "API Key",
            help: "Get from notion.so/my-integrations",
        }],
        is_channel: false,
    },
    PluginDef {
        name: "linear",
        description: "Linear GraphQL API",
        category: "productivity",
        credentials: &[CredentialSpec {
            key: "LINEAR_API_KEY",
            label: "API Key",
            help: "Get from Linear Settings > API",
        }],
        is_channel: false,
    },
];

/// Look up a plugin by name.
pub fn find_plugin(name: &str) -> Option<&'static PluginDef> {
    PLUGINS.iter().find(|i| i.name == name)
}

/// Check if a plugin's credentials are already configured.
fn is_configured(def: &PluginDef, config: &Config) -> bool {
    if def.credentials.is_empty() {
        return true;
    }
    def.credentials.iter().all(|cred| {
        config.resolve_credential_or_env(cred.key).is_some()
            || borg_plugins::keychain::check(&format!("borg-{}", def.name), cred.key)
    })
}

/// Set up a plugin: prompt for credentials, store in keychain, update config.
pub fn add_plugin(name: &str) -> Result<()> {
    let def = find_plugin(name).ok_or_else(|| anyhow::anyhow!("Unknown plugin: {name}"))?;

    let config = Config::load_from_db().unwrap_or_default();

    if is_configured(def, &config) {
        println!("{} is already configured.", def.description);
        println!("Run `borg remove {name}` first to reconfigure.");
        return Ok(());
    }

    if def.credentials.is_empty() {
        println!("{} requires no credentials.", def.description);
        return Ok(());
    }

    let use_keychain = borg_plugins::keychain::available();
    let data_dir = Config::data_dir()?;
    let config_path = data_dir.join("config.toml");

    // Read existing config TOML (or start fresh)
    let mut config_str = if config_path.exists() {
        std::fs::read_to_string(&config_path)?
    } else {
        String::new()
    };

    let service_name = format!("borg-{name}");

    for cred in def.credentials {
        eprint!("{} ({}): ", cred.label, cred.help);
        let value = read_line_masked()?;

        if value.trim().is_empty() {
            anyhow::bail!("Credential cannot be empty");
        }

        if use_keychain {
            borg_plugins::keychain::store(&service_name, cred.key, value.trim())
                .with_context(|| format!("Failed to store {} in keychain", cred.key))?;

            // Add SecretRef to config
            let entry = if cfg!(target_os = "macos") {
                format!(
                    "\n[credentials]\n{} = {{ source = \"exec\", command = \"security\", args = [\"find-generic-password\", \"-s\", \"{}\", \"-a\", \"{}\", \"-w\"] }}\n",
                    cred.key, service_name, cred.key,
                )
            } else {
                format!(
                    "\n[credentials]\n{} = {{ source = \"exec\", command = \"secret-tool\", args = [\"lookup\", \"service\", \"{}\", \"account\", \"{}\"] }}\n",
                    cred.key, service_name, cred.key,
                )
            };

            if !config_str.contains(&format!("{} =", cred.key)) {
                append_credential_to_config(&mut config_str, cred.key, &entry);
            }
        } else {
            // Fall back to .env file
            let env_path = data_dir.join(".env");
            let mut env_content = if env_path.exists() {
                std::fs::read_to_string(&env_path)?
            } else {
                String::new()
            };
            // Remove existing entry for this key to avoid duplicates
            let prefix = format!("{}=", cred.key);
            let filtered: String = env_content
                .lines()
                .filter(|line| !line.starts_with(&prefix))
                .collect::<Vec<_>>()
                .join("\n");
            env_content = if filtered.is_empty() {
                String::new()
            } else {
                filtered + "\n"
            };
            let clean_value = value.trim().replace(['\n', '\r'], "");
            env_content.push_str(&format!("{}={}\n", cred.key, clean_value));
            std::fs::write(&env_path, &env_content)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&env_path, std::fs::Permissions::from_mode(0o600));
            }
        }
    }

    std::fs::write(&config_path, &config_str)?;

    println!();
    println!("{} configured successfully.", def.description);
    if def.is_channel {
        match crate::service::restart_service() {
            Ok(()) => println!("Daemon restarted to activate {name} channel."),
            Err(_) => println!("Gateway will start automatically when you run `borg`."),
        }

        // Print pairing hint for native channels
        if borg_core::pairing::channel_to_prefix(name).is_some() {
            let agent_name = config.user.agent_name.as_deref().unwrap_or("Borg");
            let display = borg_core::pairing::channel_display_name(name);
            println!();
            println!(
                "  {display} connected! When anyone messages {agent_name},\n  \
                 they'll receive a pairing code automatically. Enter it here or run:\n    \
                 borg pairing approve <code>"
            );
        }
    }

    Ok(())
}

/// Remove a plugin's credentials.
pub fn remove_plugin(name: &str) -> Result<()> {
    let def = find_plugin(name).ok_or_else(|| anyhow::anyhow!("Unknown plugin: {name}"))?;

    let service_name = format!("borg-{name}");

    for cred in def.credentials {
        borg_plugins::keychain::remove(&service_name, cred.key);
    }

    // Also clean up .env file if it exists
    let data_dir = Config::data_dir()?;
    let env_path = data_dir.join(".env");
    if env_path.exists() {
        let content = std::fs::read_to_string(&env_path)?;
        let filtered: Vec<&str> = content
            .lines()
            .filter(|line| {
                !def.credentials
                    .iter()
                    .any(|cred| line.starts_with(&format!("{}=", cred.key)))
            })
            .collect();
        std::fs::write(&env_path, filtered.join("\n") + "\n")?;
    }

    println!("{} credentials removed.", def.description);
    Ok(())
}

/// List all plugins with their configured/unconfigured status.
pub fn list_plugins() -> Result<()> {
    let config = Config::load_from_db().unwrap_or_default();

    let categories = ["messaging", "email", "productivity"];
    let category_labels = ["Messaging", "Email", "Productivity"];

    for (cat, label) in categories.iter().zip(category_labels.iter()) {
        let items: Vec<_> = PLUGINS.iter().filter(|i| i.category == *cat).collect();
        if items.is_empty() {
            continue;
        }

        println!("{label}:");
        for def in &items {
            let configured = is_configured(def, &config);
            let icon = if configured { "\u{2713}" } else { "\u{2717}" };
            let hint = if configured {
                String::new()
            } else {
                format!("  (borg add {})", def.name)
            };
            println!("  {} {:<18} {}{}", icon, def.name, def.description, hint);
        }
        println!();
    }

    // iMessage note (macOS-only, no credentials needed)
    #[cfg(target_os = "macos")]
    println!("Note: iMessage is built-in on macOS (no setup needed).");

    Ok(())
}

/// Read a line from stdin (no echo for secrets isn't trivial without a TUI lib,
/// so we just read normally — the onboarding TUI handles masking properly).
fn read_line_masked() -> Result<String> {
    let mut input = String::new();
    std::io::stdin()
        .read_line(&mut input)
        .context("Failed to read input")?;
    Ok(input.trim().to_string())
}

/// Append a credential entry to the config string, merging into existing [credentials] section if present.
fn append_credential_to_config(config_str: &mut String, key: &str, entry: &str) {
    if config_str.contains("[credentials]") {
        // Find the [credentials] section and append after it
        if let Some(pos) = config_str.find("[credentials]") {
            // Find the end of the line after [credentials]
            let after_header = pos + "[credentials]".len();
            let insert_pos = config_str[after_header..]
                .find('\n')
                .map(|p| after_header + p + 1)
                .unwrap_or(config_str.len());

            // Extract just the key = value part (skip the \n[credentials]\n prefix)
            let key_line = entry.lines().find(|l| l.starts_with(key)).unwrap_or(entry);
            config_str.insert_str(insert_pos, &format!("{key_line}\n"));
        }
    } else {
        config_str.push_str(entry);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_known_plugin() {
        assert!(find_plugin("telegram").is_some());
        assert!(find_plugin("slack").is_some());
        assert!(find_plugin("discord").is_some());
        assert!(find_plugin("signal").is_some());
        assert!(find_plugin("teams").is_some());
        assert!(find_plugin("google-chat").is_some());
        assert!(find_plugin("gmail").is_some());
        assert!(find_plugin("notion").is_some());
        assert!(find_plugin("linear").is_some());
    }

    #[test]
    fn find_unknown_plugin() {
        assert!(find_plugin("nonexistent").is_none());
    }

    #[test]
    fn all_plugins_have_names_and_descriptions() {
        for def in PLUGINS {
            assert!(!def.name.is_empty());
            assert!(!def.description.is_empty());
            assert!(!def.category.is_empty());
        }
    }

    #[test]
    fn channel_plugins_have_credentials() {
        for def in PLUGINS {
            if def.is_channel && def.name != "imessage" {
                assert!(
                    !def.credentials.is_empty(),
                    "channel {} should have credentials",
                    def.name
                );
            }
        }
    }

    #[test]
    fn append_credential_new_section() {
        let mut config = "[llm]\nmodel = \"test\"\n".to_string();
        let entry = "\n[credentials]\nMY_KEY = { source = \"env\", var = \"MY_KEY\" }\n";
        append_credential_to_config(&mut config, "MY_KEY", entry);
        assert!(config.contains("[credentials]"));
        assert!(config.contains("MY_KEY"));
    }

    #[test]
    fn append_credential_existing_section() {
        let mut config = "[llm]\nmodel = \"test\"\n\n[credentials]\nOLD = \"old\"\n".to_string();
        let entry = "\n[credentials]\nNEW_KEY = { source = \"env\", var = \"NEW_KEY\" }\n";
        append_credential_to_config(&mut config, "NEW_KEY", entry);
        // Should only have one [credentials] section
        assert_eq!(config.matches("[credentials]").count(), 1);
        assert!(config.contains("NEW_KEY"));
        assert!(config.contains("OLD"));
    }
}
