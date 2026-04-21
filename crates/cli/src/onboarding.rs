use anyhow::{bail, Context, Result};

use std::str::FromStr;

use borg_core::config::Config;
use borg_core::constants::{IDENTITY_FILE, MEMORY_INDEX_FILE};
use borg_core::provider::Provider;

/// Standard subdirectories created under the Borg data directory.
pub(crate) const BORG_SUBDIRS: &[&str] = &["memory", "tools", "skills", "logs", "cache"];

/// Provider choices with display labels.
pub(crate) const PROVIDERS: &[(&str, &str, &str)] = &[
    (
        "openrouter",
        "OpenRouter",
        "Access all models via one API key",
    ),
    ("openai", "OpenAI", "GPT models directly"),
    ("anthropic", "Anthropic", "Claude models via API key"),
    ("gemini", "Gemini", "Google Gemini models directly"),
    ("deepseek", "DeepSeek", "Cost-effective, strong at coding"),
    ("groq", "Groq", "Ultra-fast inference"),
    ("ollama", "Ollama", "Run models locally"),
];

/// Model choices per provider.
pub(crate) const OPENROUTER_MODELS: &[(&str, &str)] = &[
    ("moonshotai/kimi-k2.5", "Kimi K2.5 (recommended)"),
    (
        "moonshotai/kimi-k2-thinking",
        "Kimi K2 Thinking (reasoning)",
    ),
    ("minimax/minimax-m2.7", "MiniMax M2.7"),
    ("x-ai/grok-4.20", "Grok 4.20"),
    ("x-ai/grok-4.20-multi-agent", "Grok 4.20 Multi-Agent"),
    ("x-ai/grok-4.1-fast", "Grok 4.1 Fast"),
    ("x-ai/grok-4-fast", "Grok 4 Fast"),
    ("x-ai/grok-4", "Grok 4"),
    ("google/gemini-3.1-pro-preview", "Gemini 3.1 Pro"),
    ("google/gemini-3-flash-preview", "Gemini 3 Flash"),
    (
        "google/gemini-3.1-flash-lite-preview",
        "Gemini 3.1 Flash Lite",
    ),
    ("deepseek/deepseek-v3.2", "DeepSeek V3.2"),
    ("deepseek/deepseek-v3.2-exp", "DeepSeek V3.2 Exp"),
    ("deepseek/deepseek-r1", "DeepSeek R1 (reasoning)"),
    ("anthropic/claude-opus-4.7", "Claude Opus 4.7"),
    ("anthropic/claude-sonnet-4.6", "Claude Sonnet 4.6"),
    (
        "anthropic/claude-haiku-4.5",
        "Claude Haiku 4.5 (fast, cheap)",
    ),
];

pub(crate) const OPENAI_MODELS: &[(&str, &str)] = &[
    ("gpt-5", "GPT-5 (recommended)"),
    ("gpt-5-mini", "GPT-5 Mini (fast, cheap)"),
    ("gpt-5-nano", "GPT-5 Nano (fastest)"),
    ("o3", "o3 (reasoning)"),
    ("o4-mini", "o4 Mini (reasoning, fast)"),
    ("gpt-4.1", "GPT-4.1"),
    ("gpt-4.1-mini", "GPT-4.1 Mini"),
    ("gpt-4o", "GPT-4o"),
];

pub(crate) const ANTHROPIC_MODELS: &[(&str, &str)] = &[
    ("claude-opus-4-7", "Claude Opus 4.7 (recommended)"),
    ("claude-sonnet-4-6", "Claude Sonnet 4.6 (balanced)"),
    ("claude-haiku-4-5", "Claude Haiku 4.5 (fast, cheap)"),
    ("claude-sonnet-4", "Claude Sonnet 4"),
    ("claude-3-5-haiku", "Claude 3.5 Haiku (legacy)"),
];

pub(crate) const GEMINI_MODELS: &[(&str, &str)] = &[
    ("gemini-3-pro", "Gemini 3 Pro (recommended)"),
    ("gemini-3-flash", "Gemini 3 Flash (fast)"),
    ("gemini-3-flash-lite", "Gemini 3 Flash Lite (fastest)"),
    ("gemini-2.5-pro", "Gemini 2.5 Pro (legacy)"),
    ("gemini-2.5-flash", "Gemini 2.5 Flash (legacy)"),
];

pub(crate) const DEEPSEEK_MODELS: &[(&str, &str)] = &[
    ("deepseek-chat", "DeepSeek Chat — V3.2 (recommended)"),
    ("deepseek-reasoner", "DeepSeek Reasoner — R1 (reasoning)"),
];

pub(crate) const GROQ_MODELS: &[(&str, &str)] = &[
    ("openai/gpt-oss-120b", "GPT-OSS 120B (recommended)"),
    (
        "moonshotai/kimi-k2-instruct",
        "Kimi K2 Instruct (large, versatile)",
    ),
    (
        "meta-llama/llama-4-maverick-17b-128e-instruct",
        "Llama 4 Maverick 17B",
    ),
    (
        "meta-llama/llama-4-scout-17b-16e-instruct",
        "Llama 4 Scout 17B",
    ),
    ("qwen/qwen3-32b", "Qwen3 32B"),
    (
        "deepseek-r1-distill-llama-70b",
        "DeepSeek R1 Distill 70B (reasoning)",
    ),
    (
        "llama-3.3-70b-versatile",
        "Llama 3.3 70B Versatile (stable)",
    ),
    ("llama-3.1-8b-instant", "Llama 3.1 8B Instant (fastest)"),
];

pub(crate) const OLLAMA_MODELS: &[(&str, &str)] = &[
    ("llama3.3", "Llama 3.3 70B (recommended)"),
    ("qwen3", "Qwen3 (versatile)"),
    ("deepseek-r1", "DeepSeek R1 (reasoning)"),
    ("gpt-oss", "GPT-OSS 20B (OpenAI open-weight)"),
    ("mistral", "Mistral 7B (fast, lightweight)"),
    ("codellama", "Code Llama (coding)"),
];

/// Assembled choices from the onboarding wizard.
pub struct OnboardingResult {
    pub user_name: String,
    pub agent_name: String,
    pub model_id: String,
    pub api_key: Option<String>,
    pub provider: String,
    /// Channel plugins selected during onboarding: `(plugin_id, credentials)`.
    pub channels: Vec<(String, Vec<(String, String)>)>,
}

/// Get the model list for a given provider.
///
/// For `openrouter`, consults the session cache populated by
/// [`crate::openrouter_catalog::spawn_prefetch`]; falls back to the hardcoded
/// [`OPENROUTER_MODELS`] list when the cache is empty (cold start or fetch
/// failure). All other providers return their hardcoded list.
pub(crate) fn models_for_provider(provider_id: &str) -> Vec<(String, String)> {
    let hardcoded: &'static [(&'static str, &'static str)] = match provider_id {
        "openai" => OPENAI_MODELS,
        "anthropic" => ANTHROPIC_MODELS,
        "gemini" => GEMINI_MODELS,
        "deepseek" => DEEPSEEK_MODELS,
        "groq" => GROQ_MODELS,
        "ollama" => OLLAMA_MODELS,
        "openrouter" => {
            if let Some(live) = crate::openrouter_catalog::cached_models() {
                return live;
            }
            OPENROUTER_MODELS
        }
        _ => OPENROUTER_MODELS,
    };
    hardcoded
        .iter()
        .map(|(id, label)| ((*id).to_string(), (*label).to_string()))
        .collect()
}

/// Run the interactive onboarding wizard. Returns `None` if the user cancels.
pub fn run_onboarding() -> Result<Option<OnboardingResult>> {
    crate::onboarding_tui::run()
}

/// Template for the initial IDENTITY.md scaffold.
/// Edit `crates/cli/templates/IDENTITY.md` to change the content.
const IDENTITY_TEMPLATE: &str = include_str!("../templates/IDENTITY.md");

/// Generate IDENTITY.md content from onboarding choices.
///
/// Produces a minimal scaffold — personality sections are left as placeholders
/// for the agent to fill in during its first conversation via `write_memory`.
pub fn generate_identity(name: &str, owner_name: &str) -> String {
    IDENTITY_TEMPLATE
        .replace("{agent_name}", name)
        .replace("{owner_name}", owner_name)
}

/// Template for the first-conversation SETUP.md.
/// Edit `crates/cli/templates/SETUP.md` to change the content.
const SETUP_TEMPLATE: &str = include_str!("../templates/SETUP.md");

/// Generate SETUP.md content for the agent's first conversation.
///
/// This is a one-shot injection — it gets loaded into the system prompt on the
/// very first conversation, then deleted. The content instructs the agent to
/// form its identity and plant core memories during the "first boot" ritual.
pub fn generate_setup(agent_name: &str, owner_name: &str) -> String {
    SETUP_TEMPLATE
        .replace("{agent_name}", agent_name)
        .replace("{owner_name}", owner_name)
}

/// Format a number with comma separators (e.g. 1000000 → "1,000,000").
#[cfg(test)]
pub(crate) fn format_number(n: u64) -> String {
    let s = n.to_string();
    let mut result = String::new();
    for (i, c) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            result.push(',');
        }
        result.push(c);
    }
    result.chars().rev().collect()
}

/// Check if a system keychain is available for secret storage.
pub(crate) fn keychain_available() -> bool {
    borg_plugins::keychain::available()
}

/// Store an API key in the OS keychain.
fn store_in_keychain(provider_id: &str, api_key: &str) -> Result<()> {
    let service_name = format!("borg-{provider_id}");
    borg_plugins::keychain::store(&service_name, "borg", api_key)
}

/// Validate that a model ID is safe for TOML interpolation (alphanumeric, slashes, hyphens, dots).
fn validate_model_id(model_id: &str) -> Result<()> {
    if model_id.is_empty() {
        bail!("model ID cannot be empty");
    }
    if model_id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '-' | '.' | '_'))
    {
        Ok(())
    } else {
        bail!("model ID contains invalid characters: {model_id}")
    }
}

/// Escape a string value for safe embedding in TOML double-quoted strings.
fn escape_toml_string(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', " ")
        .replace('\r', "")
}

/// Generate config.toml content from onboarding choices.
pub fn generate_config(
    model_id: &str,
    provider_id: &str,
    user_name: &str,
    agent_name: &str,
    use_keychain: bool,
) -> Result<String> {
    validate_model_id(model_id)?;
    let user_name = escape_toml_string(user_name);
    let agent_name = escape_toml_string(agent_name);

    let provider = Provider::from_str(provider_id)?;

    let api_key_line = if !provider.requires_api_key() {
        // Keyless providers (e.g., Ollama) — no API key config needed
        "# No API key required for local provider\n# base_url = \"http://localhost:11434/v1/chat/completions\"  # uncomment to override".to_string()
    } else if use_keychain {
        let service_name = format!("borg-{provider_id}");
        if cfg!(target_os = "macos") {
            format!(
                r#"api_key = {{ source = "exec", command = "security", args = ["find-generic-password", "-s", "{service_name}", "-a", "borg", "-w"] }}"#,
            )
        } else {
            format!(
                r#"api_key = {{ source = "exec", command = "secret-tool", args = ["lookup", "service", "borg", "provider", "{provider_id}"] }}"#,
            )
        }
    } else {
        let env_var = provider.default_env_var();
        format!(r#"api_key_env = "{env_var}""#)
    };

    Ok(format!(
        r#"[user]
name = "{user_name}"
agent_name = "{agent_name}"

[llm]
provider = "{provider_id}"
{api_key_line}
model = "{model_id}"
temperature = 0.7
max_tokens = 4096
"#,
    ))
}

/// Write content to a temporary file in the same directory, then atomically rename.
/// This prevents partial writes from leaving corrupted files.
fn atomic_write(path: &std::path::Path, content: &str) -> Result<()> {
    let tmp_path = tmp_path_for(path);
    std::fs::write(&tmp_path, content)
        .with_context(|| format!("Failed to write temp file {}", tmp_path.display()))?;
    std::fs::rename(&tmp_path, path).with_context(|| {
        format!(
            "Failed to rename {} to {}",
            tmp_path.display(),
            path.display()
        )
    })?;
    Ok(())
}

/// Build the .tmp path for a given file by appending ".tmp" to the full name.
/// e.g. "config.toml" → "config.toml.tmp"
fn tmp_path_for(path: &std::path::Path) -> std::path::PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".tmp");
    std::path::PathBuf::from(name)
}

/// Clean up any leftover .tmp files from a failed onboarding attempt.
fn cleanup_tmp_files(data_dir: &std::path::Path) {
    for name in [
        "config.toml.tmp",
        "IDENTITY.md.tmp",
        "MEMORY.md.tmp",
        "SETUP.md.tmp",
        ".env.tmp",
    ] {
        let tmp = data_dir.join(name);
        if tmp.exists() {
            let _ = std::fs::remove_file(&tmp);
        }
    }
}

/// Apply onboarding results: create directories, write config and identity files.
/// Uses atomic writes (write to .tmp then rename) to prevent partial setups.
pub fn apply_onboarding(result: &OnboardingResult) -> Result<()> {
    let data_dir = Config::data_dir()?;

    // Clean up any leftover temp files from a prior failed attempt
    cleanup_tmp_files(&data_dir);

    // Create directory structure
    for sub in BORG_SUBDIRS {
        std::fs::create_dir_all(data_dir.join(sub))?;
    }

    // Install bundled skills to filesystem
    match borg_core::skills::install_default_skills(&data_dir) {
        Ok(_) => {}
        Err(e) => eprintln!("  Warning: failed to install default skills: {e}"),
    }

    // Determine key storage automatically
    let use_keychain = result.api_key.is_some() && keychain_available();

    // Write settings to DB
    {
        let db = borg_core::db::Database::open()
            .context("Failed to open database for onboarding settings")?;
        db.set_setting("provider", &result.provider)?;
        db.set_setting("model", &result.model_id)?;
        if !result.user_name.is_empty() {
            db.set_setting("user.name", &result.user_name)?;
        }
        if !result.agent_name.is_empty() {
            db.set_setting("user.agent_name", &result.agent_name)?;
        }

        if use_keychain {
            let secret_json = if cfg!(target_os = "macos") {
                format!(
                    r#"{{"source":"exec","command":"security","args":["find-generic-password","-s","borg-{}","-a","borg","-w"]}}"#,
                    result.provider
                )
            } else {
                format!(
                    r#"{{"source":"exec","command":"secret-tool","args":["lookup","service","borg","provider","{}"]}}"#,
                    result.provider
                )
            };
            db.set_setting("llm.api_key", &secret_json)?;
        } else {
            let provider = Provider::from_str(&result.provider)?;
            db.set_setting("llm.api_key_env", provider.default_env_var())?;
        }
    }

    // Write IDENTITY.md (skip if already exists)
    let identity_path = data_dir.join(IDENTITY_FILE);
    if !identity_path.exists() {
        let identity_content = generate_identity(&result.agent_name, &result.user_name);
        if let Err(e) = atomic_write(&identity_path, &identity_content) {
            cleanup_tmp_files(&data_dir);
            return Err(e.context("Failed to write IDENTITY.md during onboarding"));
        }
    }

    // Write MEMORY.md with owner name seeded (skip if already exists)
    let memory_path = data_dir.join(MEMORY_INDEX_FILE);
    if !memory_path.exists() {
        let memory_content = format!(
            "# Memory Index\n\n## Owner\n- Name: {}\n- Agent: {}\n",
            result.user_name, result.agent_name
        );
        if let Err(e) = atomic_write(&memory_path, &memory_content) {
            cleanup_tmp_files(&data_dir);
            return Err(e.context("Failed to write MEMORY.md during onboarding"));
        }
    }

    // Write SETUP.md for first conversation instructions
    let setup_path = data_dir.join("SETUP.md");
    if !setup_path.exists() {
        let setup_content = generate_setup(&result.agent_name, &result.user_name);
        if let Err(e) = atomic_write(&setup_path, &setup_content) {
            cleanup_tmp_files(&data_dir);
            return Err(e.context("Failed to write SETUP.md during onboarding"));
        }
    }

    // Store API key based on chosen storage method
    if let Some(ref api_key) = result.api_key {
        if use_keychain {
            match store_in_keychain(&result.provider, api_key) {
                Ok(()) => {}
                Err(e) => {
                    eprintln!("  Warning: Failed to store in keychain: {e}");
                    eprintln!("  Falling back to .env file");
                    // Update DB to use env var instead of keychain
                    if let Ok(db) = borg_core::db::Database::open() {
                        let provider = Provider::from_str(&result.provider)?;
                        let _ = db.set_setting("llm.api_key_env", provider.default_env_var());
                        let _ = db.delete_setting("llm.api_key");
                    }
                    let provider = Provider::from_str(&result.provider)?;
                    let env_var = provider.default_env_var();
                    let env_path = data_dir.join(".env");
                    let clean_key = api_key.trim().replace(['\n', '\r'], "");
                    atomic_write(&env_path, &format!("{env_var}={clean_key}\n"))?;
                    println!("  Created {}", env_path.display());
                }
            }
        } else {
            let provider = Provider::from_str(&result.provider)?;
            let env_var = provider.default_env_var();
            let env_path = data_dir.join(".env");
            let clean_key = api_key.trim().replace(['\n', '\r'], "");
            atomic_write(&env_path, &format!("{env_var}={clean_key}\n"))?;
            // Restrict permissions on .env file
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if let Err(e) =
                    std::fs::set_permissions(&env_path, std::fs::Permissions::from_mode(0o600))
                {
                    eprintln!(
                        "  Warning: could not restrict permissions on {}: {e}",
                        env_path.display()
                    );
                }
            }
            println!("  Created {}", env_path.display());
        }
    }

    // Install selected channel plugins
    if result.channels.is_empty() {
        println!(
            "  No channels connected. Add one later with `borg add <name>` (e.g. `borg add telegram`)."
        );
    } else {
        let mut cfg = match Config::load_from_db() {
            Ok(c) => c,
            Err(e) => {
                eprintln!("  Warning: failed to load config for channel setup: {e}");
                tracing::warn!("Config::load_from_db failed during channel install: {e}");
                // Skip all channel installs — can't wire credentials without config
                return Ok(());
            }
        };

        let db = match borg_core::db::Database::open() {
            Ok(d) => d,
            Err(e) => {
                eprintln!("  Warning: failed to open database for channel setup: {e}");
                tracing::warn!("Database::open failed during channel install: {e}");
                return Ok(());
            }
        };

        for (plugin_id, credentials) in &result.channels {
            let Some(def) = borg_plugins::catalog::find_by_id(plugin_id) else {
                eprintln!("  Warning: unknown plugin '{plugin_id}', skipping");
                continue;
            };

            let service = def.service_name();
            let mut stored_ok = true;

            // Store credentials in keychain
            for (key, value) in credentials {
                let account = format!("borg-{key}");
                if let Err(e) = borg_plugins::keychain::store(&service, &account, value) {
                    eprintln!("  Warning: failed to store {key} in keychain: {e}");
                    tracing::warn!("Keychain store failed for {key}: {e}");
                    stored_ok = false;
                }
            }

            if !stored_ok {
                continue;
            }

            // Wire credential refs into shared config
            for (key, _) in credentials {
                let account = format!("borg-{key}");
                cfg.credentials.insert(
                    key.clone(),
                    borg_core::config::CredentialValue::Ref(
                        borg_core::secrets_resolve::SecretRef::Keychain {
                            service: service.clone(),
                            account,
                        },
                    ),
                );
            }

            // Record plugin + channel in DB
            if let Err(e) = db.insert_plugin(
                def.id,
                def.name,
                &def.kind.to_string(),
                &def.category.to_string(),
            ) {
                tracing::warn!("Failed to record plugin: {e}");
            }
            let channel_name = def.id.rsplit('/').next().unwrap_or(def.id);
            let runtime = if def.is_native { "native" } else { "python" };
            let webhook = format!("/webhook/{channel_name}");
            if let Err(e) = db.insert_installed_channel(
                channel_name,
                def.description,
                runtime,
                def.id,
                &webhook,
            ) {
                tracing::warn!("Failed to register channel: {e}");
            }

            println!(
                "  Connected {0}. Send a message to the bot on {0} to get your pairing code, then run: /pairing approve <CODE>",
                def.name
            );
        }

        // Persist accumulated credentials to DB once
        match serde_json::to_string(&cfg.credentials) {
            Ok(json) => {
                if let Err(e) = db.set_setting("credentials", &json) {
                    eprintln!("  Warning: failed to save credentials to database: {e}");
                    tracing::warn!("Failed to save credentials after channel install: {e}");
                }
            }
            Err(e) => {
                eprintln!("  Warning: failed to serialize credentials: {e}");
                tracing::warn!("serde_json::to_string failed for credentials: {e}");
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_setup_replaces_all_placeholders() {
        // Regression guard: if a new `{placeholder}` is added to the template
        // and the generator isn't updated, the raw token leaks to the user.
        let setup = generate_setup("Borg42", "Dave");
        assert!(!setup.contains("{agent_name}"));
        assert!(!setup.contains("{owner_name}"));
    }

    #[test]
    fn generate_identity_replaces_all_placeholders() {
        let identity = generate_identity("Borg42", "Dave");
        assert!(!identity.contains("{agent_name}"));
        assert!(!identity.contains("{owner_name}"));
    }

    #[test]
    fn generate_config_default_model() {
        let config = generate_config(
            "anthropic/claude-sonnet-4",
            "openrouter",
            "Mike",
            "Buddy",
            false,
        )
        .expect("valid model");
        assert!(config.contains("model = \"anthropic/claude-sonnet-4\""));
        assert!(config.contains("provider = \"openrouter\""));
        assert!(config.contains("api_key_env = \"OPENROUTER_API_KEY\""));
        assert!(config.contains("[llm]"));
    }

    #[test]
    fn generate_config_includes_user_section() {
        let config = generate_config(
            "anthropic/claude-sonnet-4",
            "openrouter",
            "Mike",
            "Buddy",
            false,
        )
        .expect("valid model");
        assert!(config.contains("[user]"));
        assert!(config.contains("name = \"Mike\""));
        assert!(config.contains("agent_name = \"Buddy\""));
    }

    #[test]
    fn generate_config_provider_matrix() {
        // One test covering the env-var → provider wiring for every env-key
        // provider. Ollama is tested separately because it has its own branch
        // (no api_key_env / no api_key block).
        let cases: &[(&str, &str, &str)] = &[
            ("claude-sonnet-4", "anthropic", "ANTHROPIC_API_KEY"),
            ("gpt-4.1", "openai", "OPENAI_API_KEY"),
            ("gemini-2.5-pro", "gemini", "GEMINI_API_KEY"),
            ("deepseek-chat", "deepseek", "DEEPSEEK_API_KEY"),
            ("llama-3.3-70b-versatile", "groq", "GROQ_API_KEY"),
        ];
        for (model, provider, env) in cases {
            let config = generate_config(model, provider, "User", "Agent", false)
                .unwrap_or_else(|e| panic!("{provider}: {e}"));
            assert!(
                config.contains(&format!("provider = \"{provider}\"")),
                "{provider}: provider line missing"
            );
            assert!(
                config.contains(&format!("api_key_env = \"{env}\"")),
                "{provider}: api_key_env line missing"
            );
            assert!(
                config.contains(&format!("model = \"{model}\"")),
                "{provider}: model line missing"
            );
        }
    }

    #[test]
    fn generate_config_rejects_empty_model() {
        assert!(generate_config("", "openrouter", "User", "Agent", false).is_err());
    }

    #[test]
    fn generate_config_rejects_injection() {
        assert!(generate_config(
            "model\"\nmalicious = true",
            "openrouter",
            "User",
            "Agent",
            false,
        )
        .is_err());
    }

    #[test]
    fn validate_model_id_accepts_valid() {
        assert!(validate_model_id("anthropic/claude-sonnet-4").is_ok());
        assert!(validate_model_id("openai/gpt-4.1-mini").is_ok());
        assert!(validate_model_id("meta-llama/llama-4-scout").is_ok());
        assert!(validate_model_id("llama3.3").is_ok());
    }

    #[test]
    fn validate_model_id_rejects_invalid() {
        assert!(validate_model_id("").is_err());
        assert!(validate_model_id("model\"").is_err());
        assert!(validate_model_id("model\nkey=val").is_err());
        assert!(validate_model_id("model with spaces").is_err());
    }

    #[test]
    fn format_number_basic() {
        assert_eq!(format_number(0), "0");
        assert_eq!(format_number(999), "999");
        assert_eq!(format_number(1000), "1,000");
        assert_eq!(format_number(1_000_000), "1,000,000");
        assert_eq!(format_number(500_000), "500,000");
        assert_eq!(format_number(5_000_000), "5,000,000");
    }

    #[test]
    fn generate_config_keychain_storage() {
        let config = generate_config(
            "anthropic/claude-sonnet-4",
            "openrouter",
            "User",
            "Agent",
            true,
        )
        .expect("valid");
        assert!(config.contains("api_key = {"));
        assert!(config.contains("source = \"exec\""));
        // Should NOT contain api_key_env when using keychain
        assert!(!config.contains("api_key_env"));
    }

    #[test]
    fn generate_config_ollama_provider() {
        let config =
            generate_config("llama3.3", "ollama", "Mike", "Buddy", false).expect("valid config");
        assert!(config.contains("provider = \"ollama\""));
        assert!(config.contains("model = \"llama3.3\""));
        // Ollama config should NOT contain api_key_env or api_key = { lines
        assert!(!config.contains("api_key_env"));
        assert!(!config.contains("api_key = {"));
    }

    #[test]
    fn generate_config_ollama_keychain_still_no_api_key() {
        // Even with keychain flag, Ollama should skip API key config
        let config =
            generate_config("llama3.3", "ollama", "Mike", "Buddy", true).expect("valid config");
        assert!(config.contains("provider = \"ollama\""));
        assert!(!config.contains("api_key_env"));
        assert!(!config.contains("api_key = {"));
    }

    #[test]
    fn ollama_models_non_empty() {
        assert!(!OLLAMA_MODELS.is_empty());
        for (id, _) in OLLAMA_MODELS {
            assert!(validate_model_id(id).is_ok(), "invalid model id: {id}");
        }
    }

    #[test]
    fn bundled_model_ids_all_parse() {
        // Regression guard: every hardcoded model ID must pass validate_model_id.
        // If a typo (or an injected char) sneaks into a constant, onboarding
        // would generate an invalid config without this guard.
        for (id, _) in OLLAMA_MODELS {
            assert!(
                validate_model_id(id).is_ok(),
                "ollama model id invalid: {id}"
            );
        }
        for (id, _) in DEEPSEEK_MODELS {
            assert!(
                validate_model_id(id).is_ok(),
                "deepseek model id invalid: {id}"
            );
        }
        for (id, _) in GROQ_MODELS {
            assert!(validate_model_id(id).is_ok(), "groq model id invalid: {id}");
        }
    }

    #[test]
    fn atomic_write_creates_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.toml");
        atomic_write(&path, "key = \"value\"").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "key = \"value\"");
        // No .tmp file should remain (test.toml.tmp, not test.tmp)
        assert!(!dir.path().join("test.toml.tmp").exists());
    }

    #[test]
    fn tmp_path_appends_suffix() {
        let path = std::path::Path::new("/data/config.toml");
        assert_eq!(
            tmp_path_for(path),
            std::path::PathBuf::from("/data/config.toml.tmp")
        );
        let path2 = std::path::Path::new("/data/IDENTITY.md");
        assert_eq!(
            tmp_path_for(path2),
            std::path::PathBuf::from("/data/IDENTITY.md.tmp")
        );
    }

    #[test]
    fn atomic_write_overwrites_existing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.toml");
        std::fs::write(&path, "old content").unwrap();
        atomic_write(&path, "new content").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new content");
    }

    #[test]
    fn cleanup_tmp_files_removes_leftovers() {
        let dir = tempfile::tempdir().unwrap();
        let tmp = dir.path().join("config.toml.tmp");
        std::fs::write(&tmp, "partial").unwrap();
        cleanup_tmp_files(dir.path());
        assert!(!tmp.exists());
    }
}
