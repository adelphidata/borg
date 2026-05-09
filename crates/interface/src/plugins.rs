use anyhow::{Context, Result};

use cortex::config::{Config, CredentialValue};
use cortex::secrets_resolve::SecretRef;

pub struct PluginDef {
    pub name: &'static str,
    pub description: &'static str,
    pub category: &'static str,
    pub credentials: &'static [CredentialSpec],
    pub is_channel: bool,
}

/// Credential prompt metadata for the legacy `borg add <plugin>` CLI flow.
/// Distinct from [`plugins::CredentialSpec`] (marketplace catalog): `help`
/// is interactive prompt text, not a doc URL. Do not merge — fold the CLI
/// flow into the marketplace catalog instead.
pub struct CredentialSpec {
    pub key: &'static str,
    pub label: &'static str,
    pub help: &'static str,
}

impl PluginDef {
    fn service_name(&self) -> String {
        let plugin_id = match self.name {
            "telegram" => "messaging/telegram",
            "slack" => "messaging/slack",
            "twilio" => "messaging/whatsapp",
            "discord" => "messaging/discord",
            "signal" => "messaging/signal",
            "teams" => "messaging/teams",
            "google-chat" => "messaging/google-chat",
            other => other,
        };
        format!("borg-{}", plugin_id.replace('/', "-"))
    }
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
    let service_name = def.service_name();
    def.credentials.iter().all(|cred| {
        config.resolve_credential_or_env(cred.key).is_some()
            || plugins::keychain::check(&service_name, &format!("borg-{}", cred.key))
    })
}

/// Set up a plugin: prompt for credentials, store in keychain (or `.env`
/// fallback), and persist the merged credentials map via the daemon.
///
/// All settings live in the SQLite settings table — this used to write
/// `~/.borg/config.toml` directly, but the schema imports that file into
/// the DB once and renames it to `.bak`, so post-import writes were
/// silently dropped. We now mirror the onboarding flow: store the secret
/// locally, build a `CredentialValue::Ref`, and round-trip the full
/// `credentials` map through `Admin.SetSetting`.
pub fn add_plugin(name: &str) -> Result<()> {
    let def = find_plugin(name).ok_or_else(|| anyhow::anyhow!("Unknown plugin: {name}"))?;

    let mut config = Config::load_from_db().unwrap_or_default();

    if is_configured(def, &config) {
        println!("{} is already configured.", def.description);
        println!("Run `borg remove {name}` first to reconfigure.");
        return Ok(());
    }

    if def.credentials.is_empty() {
        println!("{} requires no credentials.", def.description);
        return Ok(());
    }

    let data_dir = Config::data_dir()?;
    let service_name = def.service_name();

    for cred in def.credentials {
        eprint!("{} ({}): ", cred.label, cred.help);
        let value = read_line_masked()?;
        let trimmed = value.trim();

        if trimmed.is_empty() {
            anyhow::bail!("Credential cannot be empty");
        }

        let secret_ref = store_credential(&data_dir, &service_name, cred.key, trimmed)?;
        config
            .credentials
            .insert(cred.key.to_string(), CredentialValue::Ref(secret_ref));
    }

    persist_credentials(&config.credentials).context("failed to persist credentials via daemon")?;

    println!();
    println!("{} configured successfully.", def.description);
    if def.is_channel {
        // Ask the daemon to shut down gracefully; the next `borg`
        // invocation re-spawns it via `connect_or_spawn`, picking up the
        // new channel. Switching to a `borgd_launcher` restart is a
        // future refinement.
        let shutdown_ok = match tokio::runtime::Handle::try_current() {
            Ok(h) => tokio::task::block_in_place(|| {
                h.block_on(async {
                    match crate::daemon_client::connect(None).await {
                        Ok(mut client) => client.shutdown().await.is_ok(),
                        Err(_) => false,
                    }
                })
            }),
            Err(_) => false,
        };
        if shutdown_ok {
            println!("Daemon stopping to activate {name} channel; it will restart on next `borg` invocation.");
        } else {
            // Fall back to the legacy launchd/systemd restart so existing
            // installations keep working.
            let restart_result: Result<()> = match tokio::runtime::Handle::try_current() {
                Ok(h) => tokio::task::block_in_place(|| {
                    h.block_on(async { crate::borgd_launcher::restart_service().await })
                }),
                Err(_) => Err(anyhow::anyhow!("no tokio runtime available")),
            };
            match restart_result {
                Ok(()) => println!("Daemon restarted to activate {name} channel."),
                Err(_) => println!("Gateway will start automatically when you run `borg`."),
            }
        }

        // Print pairing hint for native channels
        if cortex::pairing::channel_to_prefix(name).is_some() {
            let agent_name = config.user.agent_name.as_deref().unwrap_or("Borg");
            let display = cortex::pairing::channel_display_name(name);
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

    let service_name = def.service_name();

    for cred in def.credentials {
        plugins::keychain::remove(&service_name, &format!("borg-{}", cred.key));
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

/// Store one credential locally, returning the `SecretRef` that should be
/// recorded in the credentials map. Tries the OS keychain first; falls back
/// to a 0600 `~/.borg/.env` line on headless hosts where keychain isn't
/// available — same ladder as `apply_onboarding`. Reusing `append_env_file`
/// keeps the two entry points from drifting on EnvironmentFile escaping.
fn store_credential(
    data_dir: &std::path::Path,
    service: &str,
    key: &str,
    value: &str,
) -> Result<SecretRef> {
    let account = format!("borg-{key}");
    if plugins::keychain::available() {
        match plugins::keychain::store(service, &account, value) {
            Ok(()) => {
                return Ok(SecretRef::Keychain {
                    service: service.to_string(),
                    account,
                });
            }
            Err(e) => {
                tracing::warn!("Keychain store failed for {key}: {e}; falling back to .env");
                eprintln!(
                    "  · Keychain unavailable for {key}; storing in {}/.env",
                    data_dir.display()
                );
            }
        }
    }
    crate::onboarding::append_env_file(data_dir, key, value)
        .with_context(|| format!("write .env entry for {key}"))?;
    Ok(SecretRef::Env {
        var: key.to_string(),
    })
}

/// Persist the merged credentials map through the daemon. Mirrors the single
/// round-trip onboarding does at the end of channel install — the daemon owns
/// the DB write so the CLI never opens the SQLite file directly.
fn persist_credentials(
    credentials: &std::collections::HashMap<String, CredentialValue>,
) -> Result<()> {
    let json = serde_json::to_string(credentials).context("serialize credentials map")?;
    let fut = async move {
        let mut client = crate::daemon_client::connect_or_spawn()
            .await
            .context("connect to borgd for credential persistence")?;
        client
            .set_setting("credentials", &json)
            .await
            .context("Admin.SetSetting(credentials) failed")?;
        Ok::<(), anyhow::Error>(())
    };
    match tokio::runtime::Handle::try_current() {
        Ok(h) => tokio::task::block_in_place(|| h.block_on(fut)),
        Err(_) => {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .context("spin tokio runtime for credential persistence")?;
            rt.block_on(fut)
        }
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
    fn native_plugin_configuration_uses_canonical_keychain_address() {
        // The keychain SecretRef built for a native channel must use the
        // canonical `borg-{plugin-id}` / `borg-{KEY}` shape — onboarding,
        // `borg add`, and the daemon's `read_credentials_file` fallback all
        // expect this exact pair, so a drift here silently hides credentials.
        let def = find_plugin("telegram").expect("telegram plugin");
        let service = def.service_name();
        let account = format!("borg-{}", def.credentials[0].key);
        let secret_ref = SecretRef::Keychain {
            service: service.clone(),
            account: account.clone(),
        };
        match secret_ref {
            SecretRef::Keychain { service, account } => {
                assert_eq!(service, "borg-messaging-telegram");
                assert_eq!(account, "borg-TELEGRAM_BOT_TOKEN");
            }
            other => panic!("expected Keychain variant, got {other:?}"),
        }
    }

    #[test]
    fn add_plugin_merges_into_existing_credentials_map() {
        // Simulates the in-memory map mutation `add_plugin` performs after
        // `store_credential` succeeds: a pre-existing credential from another
        // plugin survives, and the new entries land as `Ref(Keychain)` keyed
        // by the credential `key`. If this regresses (e.g. a refactor that
        // overwrites the map), users would lose previously-configured
        // channels every time they add a new one.
        use std::collections::HashMap;
        let mut map: HashMap<String, CredentialValue> = HashMap::new();
        map.insert(
            "EXISTING_KEY".into(),
            CredentialValue::Ref(SecretRef::Env {
                var: "EXISTING_KEY".into(),
            }),
        );

        let def = find_plugin("slack").expect("slack plugin");
        let service = def.service_name();
        for cred in def.credentials {
            map.insert(
                cred.key.to_string(),
                CredentialValue::Ref(SecretRef::Keychain {
                    service: service.clone(),
                    account: format!("borg-{}", cred.key),
                }),
            );
        }

        assert!(map.contains_key("EXISTING_KEY"));
        assert!(map.contains_key("SLACK_BOT_TOKEN"));
        assert!(map.contains_key("SLACK_SIGNING_SECRET"));
        match map.get("SLACK_BOT_TOKEN") {
            Some(CredentialValue::Ref(SecretRef::Keychain { service, account })) => {
                assert_eq!(service, "borg-messaging-slack");
                assert_eq!(account, "borg-SLACK_BOT_TOKEN");
            }
            other => panic!("expected Keychain Ref for SLACK_BOT_TOKEN, got {other:?}"),
        }
    }
}
