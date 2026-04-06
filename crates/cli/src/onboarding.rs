use anyhow::{bail, Context, Result};

use std::str::FromStr;

use borg_core::config::Config;
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
    (
        "claude-cli",
        "Claude Code",
        "Use your Claude subscription (no API key)",
    ),
    ("gemini", "Gemini", "Google Gemini models directly"),
    ("deepseek", "DeepSeek", "Cost-effective, strong at coding"),
    ("groq", "Groq", "Ultra-fast inference"),
    ("ollama", "Ollama", "Run models locally (no API key)"),
];

/// Model choices per provider.
pub(crate) const OPENROUTER_MODELS: &[(&str, &str)] = &[
    ("moonshotai/kimi-k2.5", "Kimi K2.5 (recommended)"),
    ("anthropic/claude-sonnet-4", "Claude Sonnet 4"),
    ("anthropic/claude-opus-4", "Claude Opus 4"),
    ("anthropic/claude-haiku-4", "Claude Haiku 4 (fast, cheap)"),
    ("openai/gpt-4.1", "GPT-4.1"),
    ("openai/gpt-4.1-mini", "GPT-4.1 Mini (fast, cheap)"),
    ("openai/gpt-4.1-nano", "GPT-4.1 Nano (fastest)"),
    ("google/gemini-2.5-pro", "Gemini 2.5 Pro"),
    ("google/gemini-2.5-flash", "Gemini 2.5 Flash"),
    ("deepseek/deepseek-r1", "DeepSeek R1 (reasoning)"),
    ("deepseek/deepseek-chat-v3", "DeepSeek V3 (chat)"),
    ("meta-llama/llama-4-maverick", "Llama 4 Maverick"),
];

pub(crate) const OPENAI_MODELS: &[(&str, &str)] = &[
    ("gpt-4.1", "GPT-4.1 (recommended)"),
    ("gpt-4.1-mini", "GPT-4.1 Mini (fast, cheap)"),
    ("gpt-4.1-nano", "GPT-4.1 Nano (fastest)"),
    ("o3", "o3 (reasoning)"),
    ("o3-mini", "o3 Mini (reasoning, fast)"),
    ("o4-mini", "o4 Mini (reasoning, fast)"),
    ("gpt-4o", "GPT-4o"),
    ("gpt-4o-mini", "GPT-4o Mini"),
];

pub(crate) const ANTHROPIC_MODELS: &[(&str, &str)] = &[
    ("claude-sonnet-4", "Claude Sonnet 4 (recommended)"),
    ("claude-opus-4", "Claude Opus 4 (most capable)"),
    ("claude-haiku-4", "Claude Haiku 4 (fast, cheap)"),
    ("claude-3.5-sonnet", "Claude 3.5 Sonnet"),
    ("claude-3.5-haiku", "Claude 3.5 Haiku"),
];

pub(crate) const CLAUDE_CLI_MODELS: &[(&str, &str)] = &[
    ("claude-sonnet-4-6", "Claude Sonnet 4.6 (recommended)"),
    ("claude-haiku-4-5", "Claude Haiku 4.5 (fast)"),
    ("claude-opus-4-6", "Claude Opus 4.6 (most capable)"),
];

pub(crate) const GEMINI_MODELS: &[(&str, &str)] = &[
    ("gemini-2.5-pro", "Gemini 2.5 Pro (recommended)"),
    ("gemini-2.5-flash", "Gemini 2.5 Flash (fast)"),
    ("gemini-2.0-flash", "Gemini 2.0 Flash"),
];

pub(crate) const DEEPSEEK_MODELS: &[(&str, &str)] = &[
    ("deepseek-chat", "DeepSeek Chat (recommended)"),
    ("deepseek-reasoner", "DeepSeek Reasoner (reasoning)"),
];

pub(crate) const GROQ_MODELS: &[(&str, &str)] = &[
    (
        "llama-3.3-70b-versatile",
        "Llama 3.3 70B Versatile (recommended)",
    ),
    ("llama-3.1-8b-instant", "Llama 3.1 8B Instant (fastest)"),
    ("gemma2-9b-it", "Gemma 2 9B IT"),
    ("mixtral-8x7b-32768", "Mixtral 8x7B (large context)"),
];

pub(crate) const OLLAMA_MODELS: &[(&str, &str)] = &[
    ("llama3.3", "Llama 3.3 70B (recommended)"),
    ("qwen2.5", "Qwen 2.5 (versatile)"),
    ("deepseek-r1", "DeepSeek R1 (reasoning)"),
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
}

/// Get the model list for a given provider.
pub(crate) fn models_for_provider(provider_id: &str) -> &'static [(&'static str, &'static str)] {
    match provider_id {
        "openai" => OPENAI_MODELS,
        "anthropic" => ANTHROPIC_MODELS,
        "claude-cli" => CLAUDE_CLI_MODELS,
        "gemini" => GEMINI_MODELS,
        "deepseek" => DEEPSEEK_MODELS,
        "groq" => GROQ_MODELS,
        "ollama" => OLLAMA_MODELS,
        _ => OPENROUTER_MODELS,
    }
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

    let api_key_line = if provider_id == "claude-cli" {
        // Claude CLI uses the user's Claude subscription via OAuth — no API key needed
        "# No API key required — uses your Claude Code subscription\n# claude_cli_path = \"/usr/local/bin/claude\"  # uncomment to override auto-detection".to_string()
    } else if !provider.requires_api_key() {
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
        Ok(0) => {}
        Ok(n) => println!("  Installed {n} default skill(s)"),
        Err(e) => eprintln!("  Warning: failed to install default skills: {e}"),
    }

    // Determine key storage automatically
    let use_keychain = result.api_key.is_some() && keychain_available();

    // Write config.toml (skip if already exists to avoid clobbering manual edits)
    let config_path = data_dir.join("config.toml");
    if config_path.exists() {
        println!("  Skipped {} (already exists)", config_path.display());
    } else {
        let config_content = generate_config(
            &result.model_id,
            &result.provider,
            &result.user_name,
            &result.agent_name,
            use_keychain,
        )?;
        if let Err(e) = atomic_write(&config_path, &config_content) {
            cleanup_tmp_files(&data_dir);
            return Err(e.context("Failed to write config.toml during onboarding"));
        }
        println!("  Created {}", config_path.display());
    }

    // Write IDENTITY.md (skip if already exists)
    let identity_path = data_dir.join("IDENTITY.md");
    if identity_path.exists() {
        println!("  Skipped {} (already exists)", identity_path.display());
    } else {
        let identity_content = generate_identity(&result.agent_name, &result.user_name);
        if let Err(e) = atomic_write(&identity_path, &identity_content) {
            cleanup_tmp_files(&data_dir);
            return Err(e.context("Failed to write IDENTITY.md during onboarding"));
        }
        println!("  Created {}", identity_path.display());
    }

    // Write MEMORY.md with owner name seeded (skip if already exists)
    let memory_path = data_dir.join("MEMORY.md");
    if memory_path.exists() {
        println!("  Skipped {} (already exists)", memory_path.display());
    } else {
        let memory_content = format!(
            "# Memory Index\n\n## Owner\n- Name: {}\n- Agent: {}\n",
            result.user_name, result.agent_name
        );
        if let Err(e) = atomic_write(&memory_path, &memory_content) {
            cleanup_tmp_files(&data_dir);
            return Err(e.context("Failed to write MEMORY.md during onboarding"));
        }
        println!("  Created {}", memory_path.display());
    }

    // Write SETUP.md for first conversation instructions
    let setup_path = data_dir.join("SETUP.md");
    if !setup_path.exists() {
        let setup_content = generate_setup(&result.agent_name, &result.user_name);
        if let Err(e) = atomic_write(&setup_path, &setup_content) {
            cleanup_tmp_files(&data_dir);
            return Err(e.context("Failed to write SETUP.md during onboarding"));
        }
        println!("  Created {}", setup_path.display());
    }

    // Store API key based on chosen storage method
    if let Some(ref api_key) = result.api_key {
        if use_keychain {
            match store_in_keychain(&result.provider, api_key) {
                Ok(()) => {
                    println!(
                        "  Stored API key in OS keychain (service: borg-{})",
                        result.provider
                    );
                }
                Err(e) => {
                    eprintln!("  Warning: Failed to store in keychain: {e}");
                    eprintln!("  Falling back to .env file");
                    let fallback_config = generate_config(
                        &result.model_id,
                        &result.provider,
                        &result.user_name,
                        &result.agent_name,
                        false,
                    )?;
                    atomic_write(&config_path, &fallback_config)?;
                    let provider = Provider::from_str(&result.provider)?;
                    let env_var = provider.default_env_var();
                    let env_path = data_dir.join(".env");
                    let clean_key = api_key.trim().replace(['\n', '\r'], "");
                    atomic_write(&env_path, &format!("{env_var}={clean_key}\n"))?;
                    println!("  Updated config to use .env file");
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
                let _ = std::fs::set_permissions(&env_path, std::fs::Permissions::from_mode(0o600));
            }
            println!("  Created {}", env_path.display());
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_identity_contains_name_and_placeholders() {
        let identity = generate_identity("Buddy", "Mike");
        assert!(identity.contains("# Buddy"));
        assert!(identity.contains("You are Buddy"));
        assert!(identity.contains("Mike"));
        assert!(identity.contains("## Personality"));
        assert!(identity.contains("## Capabilities"));
        assert!(identity.contains("## Communication Style"));
        assert!(identity.contains("## What Matters"));
    }

    #[test]
    fn generate_identity_is_minimal_scaffold() {
        let identity = generate_identity("Nova", "Alice");
        // Should have placeholder comments for the agent to fill in
        assert!(identity.contains("<!--"));
        // Should keep factual capabilities
        assert!(identity.contains("Memory across sessions"));
        assert!(identity.contains("Tool creation"));
    }

    #[test]
    fn generate_setup_contains_agent_and_owner() {
        let setup = generate_setup("Nova", "Alice");
        assert!(setup.contains("Nova"));
        assert!(setup.contains("Alice"));
    }

    #[test]
    fn generate_setup_contains_identity_instruction() {
        let setup = generate_setup("Nova", "Alice");
        assert!(setup.contains("IDENTITY.md"));
        assert!(setup.contains("write_memory"));
    }

    #[test]
    fn generate_setup_references_systems() {
        let setup = generate_setup("Nova", "Alice");
        // References vitals/evolution concepts (wording may be casual)
        assert!(setup.contains("Vitals") || setup.contains("vitals"));
        assert!(
            setup.contains("evolve") || setup.contains("specialization") || setup.contains("Lvl")
        );
    }

    #[test]
    fn generate_setup_mentions_channels() {
        let setup = generate_setup("Nova", "Alice");
        assert!(setup.contains("borg add"));
    }

    #[test]
    fn generate_setup_not_too_large() {
        let setup = generate_setup("TestAgent", "TestOwner");
        assert!(
            setup.len() < 2500,
            "SETUP.md is {} chars, should be under 2500",
            setup.len()
        );
    }

    #[test]
    fn generate_setup_has_first_boot_heading() {
        let setup = generate_setup("Nova", "Alice");
        assert!(setup.contains("# First Boot"));
    }

    #[test]
    fn generate_setup_replaces_all_placeholders() {
        let setup = generate_setup("Borg42", "Dave");
        // No raw placeholders should remain
        assert!(!setup.contains("{agent_name}"));
        assert!(!setup.contains("{owner_name}"));
    }

    #[test]
    fn generate_setup_mentions_level() {
        let setup = generate_setup("Nova", "Alice");
        assert!(setup.contains("Lvl.0") || setup.contains("Base"));
    }

    #[test]
    fn generate_setup_mentions_evolving() {
        let setup = generate_setup("Nova", "Alice");
        assert!(setup.contains("evolve") || setup.contains("specializ"));
    }

    #[test]
    fn generate_setup_mentions_memories() {
        let setup = generate_setup("Nova", "Alice");
        assert!(setup.contains("memories") || setup.contains("write_memory"));
    }

    #[test]
    fn generate_identity_replaces_all_placeholders() {
        let identity = generate_identity("Borg42", "Dave");
        assert!(!identity.contains("{agent_name}"));
        assert!(!identity.contains("{owner_name}"));
    }

    #[test]
    fn generate_identity_starts_with_heading() {
        let identity = generate_identity("Nova", "Alice");
        assert!(identity.starts_with("# Nova"));
    }

    #[test]
    fn generate_identity_has_capability_list() {
        let identity = generate_identity("Nova", "Alice");
        assert!(identity.contains("Memory across sessions"));
        assert!(identity.contains("Tool creation"));
        assert!(identity.contains("heartbeat"));
        assert!(identity.contains("Channel integrations"));
    }

    #[test]
    fn setup_template_is_valid() {
        // The raw template should have placeholders
        assert!(SETUP_TEMPLATE.contains("{agent_name}"));
        assert!(SETUP_TEMPLATE.contains("{owner_name}"));
    }

    #[test]
    fn identity_template_is_valid() {
        // The raw template should have placeholders
        assert!(IDENTITY_TEMPLATE.contains("{agent_name}"));
        assert!(IDENTITY_TEMPLATE.contains("{owner_name}"));
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
    fn generate_config_anthropic_provider() {
        let config =
            generate_config("claude-sonnet-4", "anthropic", "User", "Agent", false).expect("valid");
        assert!(config.contains("provider = \"anthropic\""));
        assert!(config.contains("api_key_env = \"ANTHROPIC_API_KEY\""));
    }

    #[test]
    fn generate_config_openai_provider() {
        let config = generate_config("gpt-4.1", "openai", "User", "Agent", false).expect("valid");
        assert!(config.contains("provider = \"openai\""));
        assert!(config.contains("api_key_env = \"OPENAI_API_KEY\""));
    }

    #[test]
    fn generate_config_gemini_provider() {
        let config =
            generate_config("gemini-2.5-pro", "gemini", "User", "Agent", false).expect("valid");
        assert!(config.contains("provider = \"gemini\""));
        assert!(config.contains("api_key_env = \"GEMINI_API_KEY\""));
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
    fn models_for_all_providers_non_empty() {
        for (id, _, _) in PROVIDERS {
            let models = models_for_provider(id);
            assert!(!models.is_empty(), "models for {id} should not be empty");
        }
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
    fn keychain_available_returns_bool() {
        // Just verify it doesn't panic — actual availability depends on platform
        let _available = keychain_available();
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
    fn models_for_provider_ollama() {
        let models = models_for_provider("ollama");
        assert_eq!(models.len(), OLLAMA_MODELS.len());
    }

    #[test]
    fn generate_config_deepseek_provider() {
        let config =
            generate_config("deepseek-chat", "deepseek", "User", "Agent", false).expect("valid");
        assert!(config.contains("provider = \"deepseek\""));
        assert!(config.contains("api_key_env = \"DEEPSEEK_API_KEY\""));
    }

    #[test]
    fn generate_config_groq_provider() {
        let config = generate_config("llama-3.3-70b-versatile", "groq", "User", "Agent", false)
            .expect("valid");
        assert!(config.contains("provider = \"groq\""));
        assert!(config.contains("api_key_env = \"GROQ_API_KEY\""));
    }

    #[test]
    fn deepseek_models_non_empty() {
        assert!(!DEEPSEEK_MODELS.is_empty());
        for (id, _) in DEEPSEEK_MODELS {
            assert!(validate_model_id(id).is_ok(), "invalid model id: {id}");
        }
    }

    #[test]
    fn groq_models_non_empty() {
        assert!(!GROQ_MODELS.is_empty());
        for (id, _) in GROQ_MODELS {
            assert!(validate_model_id(id).is_ok(), "invalid model id: {id}");
        }
    }

    #[test]
    fn models_for_provider_deepseek() {
        let models = models_for_provider("deepseek");
        assert_eq!(models.len(), DEEPSEEK_MODELS.len());
    }

    #[test]
    fn models_for_provider_groq() {
        let models = models_for_provider("groq");
        assert_eq!(models.len(), GROQ_MODELS.len());
    }

    #[test]
    fn openrouter_models_count() {
        assert!(
            OPENROUTER_MODELS.len() <= 15,
            "OpenRouter models should be at most 15"
        );
        assert!(
            OPENROUTER_MODELS.len() >= 10,
            "OpenRouter models should have at least 10"
        );
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

    // ── Claude CLI provider tests ──

    #[test]
    fn models_for_provider_claude_cli() {
        let models = models_for_provider("claude-cli");
        assert_eq!(models.len(), CLAUDE_CLI_MODELS.len());
        assert!(!models.is_empty());
    }

    #[test]
    fn claude_cli_models_valid() {
        for (id, _) in CLAUDE_CLI_MODELS {
            assert!(validate_model_id(id).is_ok(), "invalid model id: {id}");
        }
    }

    #[test]
    fn claude_cli_models_all_normalize_to_known_aliases() {
        // Every entry must map to one of the short CLI aliases so the subprocess
        // gets a value the `claude` binary understands regardless of version drift.
        for (id, _) in CLAUDE_CLI_MODELS {
            let alias = borg_core::claude_cli::normalize_cli_model(id);
            assert!(
                matches!(alias.as_str(), "opus" | "sonnet" | "haiku"),
                "model {id} normalized to {alias}, expected opus/sonnet/haiku"
            );
        }
    }

    #[test]
    fn providers_includes_claude_cli() {
        assert!(
            PROVIDERS.iter().any(|(id, _, _)| *id == "claude-cli"),
            "PROVIDERS should include claude-cli"
        );
    }

    #[test]
    fn generate_config_claude_cli_provider() {
        let config = generate_config("claude-sonnet-4-6", "claude-cli", "User", "Agent", false)
            .expect("valid config");
        assert!(config.contains("provider = \"claude-cli\""));
        assert!(config.contains("model = \"claude-sonnet-4-6\""));
        // Should NOT contain api_key_env or api_key lines
        assert!(!config.contains("api_key_env"));
        assert!(!config.contains("api_key = {"));
        // Should have Claude-specific comment
        assert!(config.contains("Claude Code subscription"));
    }

    #[test]
    fn generate_config_claude_cli_keychain_still_no_api_key() {
        // Even with keychain flag, claude-cli should skip API key config
        let config = generate_config("claude-sonnet-4-6", "claude-cli", "User", "Agent", true)
            .expect("valid config");
        assert!(config.contains("provider = \"claude-cli\""));
        assert!(!config.contains("api_key_env"));
        assert!(!config.contains("api_key = {"));
    }
}
