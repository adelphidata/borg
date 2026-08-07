use anyhow::{bail, Context, Result};

use std::str::FromStr;

use cortex::config::Config;
use cortex::constants::{IDENTITY_FILE, MEMORY_INDEX_FILE, SETUP_FILE};
use cortex::llm::provider::Provider;
use cortex::security::secrets_resolve::{provider_api_key_secret_ref, SecretRef};
use protocols::admin::{PluginInstallRequest, PluginInstalledChannel};

use crate::async_bridge::block_on;
use crate::daemon_client;

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
        "bedrock",
        "AWS Bedrock",
        "Claude models via the AWS credential chain",
    ),
    ("gemini", "Gemini", "Google Gemini models directly"),
    ("deepseek", "DeepSeek", "Cost-effective, strong at coding"),
    ("groq", "Groq", "Ultra-fast inference"),
    ("xai", "xAI", "Grok models directly from xAI"),
    (
        "azure-openai",
        "Azure OpenAI",
        "OpenAI models on Microsoft tenancy (set resource/deployment in /settings)",
    ),
    ("ollama", "Ollama", "Run models locally"),
    (
        "claude-cli",
        "Claude Code CLI",
        "Use your Claude subscription via the local `claude` binary",
    ),
];

/// Model choices per provider.
///
/// OpenRouter list leads with the open-weight flagships (GLM, MiniMax, Kimi) as
/// the recommended defaults, then walks the remaining families — Anthropic,
/// OpenAI, Gemini, Grok, Qwen, DeepSeek, Mistral — newest slug first within
/// each. The live catalog fetch ([`crate::openrouter_catalog`]) preserves this
/// order and drops any IDs OpenRouter no longer serves, so keep the newest
/// curated slug at the top of its family. New open-weight flagships go up top
/// with GLM/MiniMax/Kimi; everything else keeps its family grouping. See the
/// `update-model-catalog` skill.
pub(crate) const OPENROUTER_MODELS: &[(&str, &str)] = &[
    ("z-ai/glm-5.1", "GLM 5.1 (recommended)"),
    ("z-ai/glm-4.7", "GLM 4.7"),
    ("minimax/minimax-m2.7", "MiniMax M2.7"),
    ("moonshotai/kimi-k3", "Kimi K3 (reasoning)"),
    ("anthropic/claude-opus-4.7", "Claude Opus 4.7"),
    ("anthropic/claude-sonnet-4.6", "Claude Sonnet 4.6"),
    (
        "anthropic/claude-haiku-4.5",
        "Claude Haiku 4.5 (fast, cheap)",
    ),
    ("openai/gpt-5.5", "GPT-5.5"),
    ("openai/gpt-5.5-pro", "GPT-5.5 Pro (reasoning)"),
    ("openai/gpt-5.4-mini", "GPT-5.4 Mini (fast)"),
    ("openai/gpt-5.4-nano", "GPT-5.4 Nano (cheap)"),
    ("openai/gpt-5.1-codex", "GPT-5.1 Codex (coding)"),
    ("openai/gpt-5", "GPT-5"),
    ("openai/gpt-5-mini", "GPT-5 Mini"),
    ("google/gemini-3.1-pro-preview", "Gemini 3.1 Pro"),
    ("google/gemini-3-flash-preview", "Gemini 3 Flash"),
    (
        "google/gemini-3.1-flash-lite-preview",
        "Gemini 3.1 Flash Lite",
    ),
    ("x-ai/grok-4.20", "Grok 4.20"),
    ("x-ai/grok-4.20-multi-agent", "Grok 4.20 Multi-Agent"),
    ("x-ai/grok-4.1-fast", "Grok 4.1 Fast"),
    ("x-ai/grok-4-fast", "Grok 4 Fast"),
    ("x-ai/grok-4", "Grok 4"),
    ("qwen/qwen3.6-max-preview", "Qwen3.6 Max"),
    ("qwen/qwen3.6-flash", "Qwen3.6 Flash"),
    ("qwen/qwen3-coder-next", "Qwen3 Coder Next (coding)"),
    ("deepseek/deepseek-v3.2", "DeepSeek V3.2"),
    ("deepseek/deepseek-v3.2-exp", "DeepSeek V3.2 Exp"),
    ("deepseek/deepseek-r1", "DeepSeek R1 (reasoning)"),
    ("mistralai/mistral-large-2512", "Mistral Large 3"),
    ("mistralai/devstral-2512", "Devstral 2 (coding)"),
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

pub(crate) const BEDROCK_MODELS: &[(&str, &str)] = &[
    (
        "anthropic.claude-3-7-sonnet-20250219-v1:0",
        "Claude 3.7 Sonnet (recommended)",
    ),
    (
        "anthropic.claude-3-5-sonnet-20241022-v2:0",
        "Claude 3.5 Sonnet",
    ),
    (
        "anthropic.claude-3-5-haiku-20241022-v1:0",
        "Claude 3.5 Haiku (fast, cheap)",
    ),
];

// `deepseek-chat` / `deepseek-reasoner` are deliberately absent: the provider
// retires those ids on 2026-07-24 (they alias V4 Flash's non-thinking /
// thinking modes). Existing configs still resolve — the registry, pricing, and
// deprecation tables keep the aliases — but the picker must not hand a new
// operator an id that is about to stop answering.
pub(crate) const DEEPSEEK_MODELS: &[(&str, &str)] = &[
    ("deepseek-v4-flash", "DeepSeek V4 Flash (recommended)"),
    ("deepseek-v4-pro", "DeepSeek V4 Pro (reasoning)"),
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

pub(crate) const XAI_MODELS: &[(&str, &str)] = &[
    ("grok-4", "Grok 4 (recommended)"),
    (
        "grok-4-fast-reasoning",
        "Grok 4 Fast — reasoning (long context)",
    ),
    (
        "grok-4-fast-non-reasoning",
        "Grok 4 Fast — non-reasoning (fastest)",
    ),
];

// Azure OpenAI deployment names are operator-chosen — these are suggested
// labels for the most common base-model deployments. The actual deployment
// name lives in `llm.azure_deployment` (set via /settings after onboarding);
// the `model` field in the body is informational on Azure (routing is by
// deployment, not model id).
pub(crate) const AZURE_OPENAI_MODELS: &[(&str, &str)] = &[
    ("gpt-4o", "GPT-4o (suggest as deployment name)"),
    ("gpt-4o-mini", "GPT-4o Mini"),
    ("gpt-5", "GPT-5"),
    ("gpt-5-mini", "GPT-5 Mini"),
    ("o3", "o3 (reasoning)"),
];

pub(crate) const CLAUDE_CLI_MODELS: &[(&str, &str)] = &[
    ("claude-opus-4-7", "Claude Opus 4.7 (recommended)"),
    ("claude-sonnet-4-6", "Claude Sonnet 4.6 (balanced)"),
    ("claude-haiku-4-5", "Claude Haiku 4.5 (fast, cheap)"),
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
        "bedrock" => BEDROCK_MODELS,
        "gemini" => GEMINI_MODELS,
        "deepseek" => DEEPSEEK_MODELS,
        "groq" => GROQ_MODELS,
        "xai" => XAI_MODELS,
        "azure-openai" => AZURE_OPENAI_MODELS,
        "ollama" => {
            // Prefer the live installed-model list from `GET /api/tags` —
            // what's actually pulled on this machine rarely matches the
            // hardcoded suggestions.
            if let Some(live) = crate::ollama_catalog::cached_models() {
                return live;
            }
            OLLAMA_MODELS
        }
        "claude-cli" => CLAUDE_CLI_MODELS,
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
    // Fire-and-forget: populate the installed-Ollama-model cache while the
    // user is on the earlier tabs, so the model picker shows what `ollama
    // pull` actually installed.
    crate::ollama_catalog::prefetch_from_sync();
    crate::onboarding_tui::run()
}

/// Edit `crates/interface/templates/IDENTITY.md` to change the content.
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

/// Edit `crates/interface/templates/SETUP.md` to change the content.
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
    plugins::keychain::available()
}

/// Store an API key in the OS keychain.
fn store_in_keychain(provider_id: &str, api_key: &str) -> Result<()> {
    let service_name = format!("borg-{provider_id}");
    plugins::keychain::store(&service_name, "borg", api_key)
}

fn keychain_api_key_toml(provider_id: &str) -> Result<String> {
    let secret_ref = provider_api_key_secret_ref(provider_id);
    let SecretRef::Keychain { service, account } = secret_ref else {
        unreachable!("provider_api_key_secret_ref must return Keychain")
    };
    let service = escape_toml_string(&service);
    let account = escape_toml_string(&account);
    Ok(format!(
        r#"api_key = {{ source = "keychain", service = "{service}", account = "{account}" }}"#
    ))
}

/// Validate that a model ID is safe for TOML interpolation (alphanumeric, slashes, hyphens, dots).
fn validate_model_id(model_id: &str) -> Result<()> {
    if model_id.is_empty() {
        bail!("model ID cannot be empty");
    }
    if model_id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '-' | '.' | '_' | ':'))
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

    let api_key_line = if provider.uses_aws_credential_chain() {
        "# Uses the AWS credential chain (env/profile/role)\n# bedrock_region = \"us-east-1\"  # uncomment to override AWS_REGION / AWS_DEFAULT_REGION".to_string()
    } else if !provider.requires_api_key() {
        "# No API key required for local provider\n# base_url = \"http://localhost:11434/v1/chat/completions\"  # uncomment to override".to_string()
    } else if use_keychain {
        keychain_api_key_toml(provider_id)?
    } else {
        let env_var = provider.default_env_var();
        format!(r#"api_key_env = "{env_var}""#)
    };

    // Auxiliary chain: same provider as primary, `model = "auto"` sentinel
    // resolved at daemon-boot time via `aux_model_picker`. Background work
    // (memory consolidation, heartbeat, scheduled prompts) routes through
    // this chain so a primary outage doesn't kill nightly cron. We emit
    // the block for every provider — even keyless ones like Ollama, where
    // the resolver returns `None` and `for_auxiliary` silently falls
    // through to primary. No special-casing in onboarding; the resolver
    // is the single place that decides "no aux available."
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

[llm.aux]
provider = "{provider_id}"
model = "auto"
"#,
    ))
}

/// Crash-safe write for the non-secret onboarding files (config, persona,
/// memory, setup notes).
///
/// Delegates to the shared writer rather than repeating temp+rename here: this
/// module's copy had no fsync (a crash right after the rename could surface a
/// zero-length file) and used a fixed `<name>.tmp` path that two writers would
/// race on. Secrets keep their own [`write_secret_file`], which must create at
/// mode 0600 rather than the umask default.
fn atomic_write(path: &std::path::Path, content: &str) -> Result<()> {
    coding::fs_atomic::atomic_write(path, content.as_bytes())
        .with_context(|| format!("Failed to write {}", path.display()))
}

/// Append (or replace) a `KEY=VALUE` line in `<data_dir>/.env`. The temp file
/// is created with mode 0600 from the start (Unix) so the secret never hits
/// disk world-readable, even briefly between rename and chmod. Used as the
/// headless-VPS fallback when the OS keychain is unavailable for plugin
/// credentials. Existing lines for the same key are rewritten in place.
pub(crate) fn append_env_file(data_dir: &std::path::Path, key: &str, value: &str) -> Result<()> {
    if !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        bail!(
            "env var name {key:?} must be alphanumeric/underscore for systemd EnvironmentFile compatibility"
        );
    }
    let clean_value = value.trim().replace(['\n', '\r', '\0'], "");
    // systemd EnvironmentFile parses backslash escapes and quoted strings;
    // reject characters that would break parsing or smuggle adjacent vars.
    if clean_value.contains(['"', '\\', '`']) {
        bail!(
            "credential value for {key} contains characters unsafe for systemd EnvironmentFile (\", \\, `)"
        );
    }
    let path = data_dir.join(".env");
    let prefix = format!("{key}=");
    let mut new_contents = String::new();
    let mut replaced = false;
    if let Ok(existing) = std::fs::read_to_string(&path) {
        for line in existing.lines() {
            let trimmed = line.trim_start();
            if trimmed.starts_with(&prefix) {
                new_contents.push_str(&format!("{key}={clean_value}\n"));
                replaced = true;
            } else {
                new_contents.push_str(line);
                new_contents.push('\n');
            }
        }
    }
    if !replaced {
        new_contents.push_str(&format!("{key}={clean_value}\n"));
    }
    write_secret_file(&path, &new_contents).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

/// Atomically write `content` to `path` with mode 0600 from creation (Unix).
/// The temp file is opened with `O_CREAT | O_EXCL` and explicit mode so the
/// secret never lives on disk with permissive perms before chmod.
fn write_secret_file(path: &std::path::Path, content: &str) -> Result<()> {
    let tmp_path = tmp_path_for(path);
    // If a stale tmp exists from a previous crash, remove it; O_EXCL would
    // otherwise fail.
    let _ = std::fs::remove_file(&tmp_path);
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp_path)
            .with_context(|| format!("create {}", tmp_path.display()))?;
        f.write_all(content.as_bytes())
            .with_context(|| format!("write {}", tmp_path.display()))?;
        if let Err(e) = f.sync_all() {
            tracing::warn!("fsync {} failed: {e}", tmp_path.display());
        }
    }
    #[cfg(not(unix))]
    {
        std::fs::write(&tmp_path, content)
            .with_context(|| format!("write {}", tmp_path.display()))?;
    }
    if let Err(e) = std::fs::rename(&tmp_path, path) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(e)
            .with_context(|| format!("rename {} to {}", tmp_path.display(), path.display()));
    }
    Ok(())
}

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
            // Best-effort: a stale tmp we cannot delete is overwritten by the
            // next atomic write anyway, so failure must not abort onboarding.
            let _ = std::fs::remove_file(&tmp);
        }
    }
}

/// Apply onboarding results: create directories, write config and identity files.
/// Uses atomic writes (write to .tmp then rename) to prevent partial setups.
///
/// All settings/plugin/credential writes are routed through the daemon over
/// gRPC — the cli no longer opens the SQLite DB directly. This keeps the
/// daemon as the single writer and lets its in-memory caches stay coherent
/// after onboarding completes.
pub fn apply_onboarding(result: &OnboardingResult) -> Result<()> {
    let data_dir = Config::data_dir()?;

    cleanup_tmp_files(&data_dir);

    for sub in BORG_SUBDIRS {
        std::fs::create_dir_all(data_dir.join(sub))?;
    }

    match cortex::skills::install_default_skills(&data_dir) {
        Ok(_) => {}
        Err(e) => eprintln!("  Warning: failed to install default skills: {e}"),
    }

    let use_keychain = result.api_key.is_some() && keychain_available();

    // Write settings via the daemon (single writer, cache-coherent).
    block_on(async {
        let mut client = daemon_client::connect_or_spawn()
            .await
            .context("connect to borgd for onboarding settings")?;
        client.set_setting("provider", &result.provider).await?;
        client.set_setting("model", &result.model_id).await?;
        if !result.user_name.is_empty() {
            client.set_setting("user.name", &result.user_name).await?;
        }
        if !result.agent_name.is_empty() {
            client
                .set_setting("user.agent_name", &result.agent_name)
                .await?;
        }
        // Auto-detect the host's IANA timezone so heartbeat quiet hours
        // anchor in local time (e.g. 00:00–06:00 ET on a Mac in EST) instead
        // of UTC. Headless boxes whose OS doesn't expose a usable tz return
        // None here; we leave the setting unset and `user_timezone()`
        // continues to fall back to UTC.
        if let Some(tz) = cortex::config::Config::detect_host_timezone() {
            client.set_setting("user.timezone", &tz).await?;
        }
        // Sentinel that gates re-running the onboarding wizard on subsequent
        // launches. See `is_onboarded` in `crates/interface/src/main.rs`.
        client.set_onboarding_complete().await?;

        if use_keychain {
            let secret_json = serde_json::to_string(&provider_api_key_secret_ref(&result.provider))
                .context("serialize onboarding keychain SecretRef")?;
            client.set_setting("llm.api_key", &secret_json).await?;
        } else {
            let provider = Provider::from_str(&result.provider)?;
            client
                .set_setting("llm.api_key_env", provider.default_env_var())
                .await?;
        }
        anyhow::Ok(())
    })
    .context("Failed to persist onboarding settings via daemon")?;

    let identity_path = data_dir.join(IDENTITY_FILE);
    if !identity_path.exists() {
        let identity_content = generate_identity(&result.agent_name, &result.user_name);
        if let Err(e) = atomic_write(&identity_path, &identity_content) {
            cleanup_tmp_files(&data_dir);
            return Err(e.context("Failed to write IDENTITY.md during onboarding"));
        }
    }

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

    let setup_path = data_dir.join(SETUP_FILE);
    if !setup_path.exists() {
        let setup_content = generate_setup(&result.agent_name, &result.user_name);
        if let Err(e) = atomic_write(&setup_path, &setup_content) {
            cleanup_tmp_files(&data_dir);
            return Err(e.context("Failed to write SETUP.md during onboarding"));
        }
    }

    if let Some(ref api_key) = result.api_key {
        if use_keychain {
            match store_in_keychain(&result.provider, api_key) {
                Ok(()) => {}
                Err(e) => {
                    eprintln!("  Warning: Failed to store in keychain: {e}");
                    eprintln!("  Falling back to .env file");
                    let provider = Provider::from_str(&result.provider)?;
                    let env_var_owned = provider.default_env_var().to_string();
                    if let Err(e) = block_on(async {
                        let mut client = daemon_client::connect_or_spawn().await?;
                        client
                            .set_setting("llm.api_key_env", &env_var_owned)
                            .await?;
                        client.unset_setting("llm.api_key").await?;
                        anyhow::Ok(())
                    }) {
                        tracing::warn!(
                            "onboarding: failed to switch llm.api_key to env-var fallback: {e}"
                        );
                    }
                    let env_var = provider.default_env_var();
                    append_env_file(&data_dir, env_var, api_key)?;
                    println!("  Updated {}", data_dir.join(".env").display());
                }
            }
        } else {
            let provider = Provider::from_str(&result.provider)?;
            let env_var = provider.default_env_var();
            append_env_file(&data_dir, env_var, api_key)?;
            println!("  Updated {}", data_dir.join(".env").display());
        }
    }

    if result.channels.is_empty() {
        println!(
            "  No channels connected. Add one later with `borg add <name>` (e.g. `borg add telegram`)."
        );
    } else {
        let mut cfg = match block_on(Config::load_from_db()) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("  Warning: failed to load config for channel setup: {e}");
                tracing::warn!("Config::load_from_db failed during channel install: {e}");
                // Skip all channel installs — can't wire credentials without config
                return Ok(());
            }
        };

        // Pre-build per-plugin install requests; route credential persistence
        // through the daemon at the end so the cli never opens the DB itself.
        struct PendingInstall {
            req: PluginInstallRequest,
            display_name: &'static str,
        }
        let mut pending: Vec<PendingInstall> = Vec::new();

        for (plugin_id, credentials) in &result.channels {
            let Some(def) = plugins::catalog::find_by_id(plugin_id) else {
                eprintln!("  Warning: unknown plugin '{plugin_id}', skipping");
                continue;
            };

            let service = def.service_name();

            // Try OS keychain first; on headless servers (no secret-service /
            // dbus) it fails — fall back to a 0600 ~/.borg/.env file with an
            // env-var SecretRef so the daemon still resolves the credential
            // through systemd's `EnvironmentFile=`. Without this fallback the
            // channel install was silently dropped on every Linux VPS.
            for (key, value) in credentials {
                let account = format!("borg-{key}");
                match plugins::keychain::store(&service, &account, value) {
                    Ok(()) => {
                        cfg.credentials.insert(
                            key.clone(),
                            cortex::config::CredentialValue::Ref(
                                cortex::security::secrets_resolve::SecretRef::Keychain {
                                    service: service.clone(),
                                    account,
                                },
                            ),
                        );
                    }
                    Err(e) => {
                        tracing::warn!(
                            "Keychain store failed for {key}: {e}; falling back to .env"
                        );
                        eprintln!(
                            "  · Keychain unavailable for {key}; storing in {}/.env",
                            data_dir.display()
                        );
                        if let Err(e) = append_env_file(&data_dir, key, value) {
                            eprintln!("  Warning: failed to write .env for {key}: {e}");
                            tracing::warn!("append_env_file failed for {key}: {e}");
                            continue;
                        }
                        cfg.credentials.insert(
                            key.clone(),
                            cortex::config::CredentialValue::Ref(
                                cortex::security::secrets_resolve::SecretRef::Env {
                                    var: key.clone(),
                                },
                            ),
                        );
                    }
                }
            }

            let channel_name = def.id.rsplit('/').next().unwrap_or(def.id);
            let runtime = if def.is_native { "native" } else { "python" };
            let webhook = format!("/webhook/{channel_name}");
            pending.push(PendingInstall {
                req: PluginInstallRequest {
                    plugin_id: def.id.to_string(),
                    plugin_name: def.name.to_string(),
                    kind: def.kind.to_string(),
                    category: def.category.to_string(),
                    file_hashes: Vec::new(),
                    installed_tool: None,
                    installed_channel: Some(PluginInstalledChannel {
                        name: channel_name.to_string(),
                        description: def.description.to_string(),
                        runtime: runtime.to_string(),
                        plugin_id: def.id.to_string(),
                        webhook,
                    }),
                    // Per-plugin install RPCs leave credentials alone; we
                    // overwrite the full credentials map once at the end.
                    credentials_json: String::new(),
                },
                display_name: def.name,
            });
        }

        // Single round-trip: install each plugin row + persist the merged
        // credentials map. The daemon owns the DB writes.
        let credentials_json = match serde_json::to_string(&cfg.credentials) {
            Ok(j) => j,
            Err(e) => {
                eprintln!("  Warning: failed to serialize credentials: {e}");
                tracing::warn!("serde_json::to_string failed for credentials: {e}");
                String::new()
            }
        };

        let install_outcome: Result<()> = block_on(async {
            let mut client = daemon_client::connect_or_spawn()
                .await
                .context("connect to borgd for channel install")?;
            for p in &pending {
                if let Err(e) = client.plugin_install(p.req.clone()).await {
                    tracing::warn!("plugin_install RPC failed for {}: {e}", p.req.plugin_id);
                    eprintln!(
                        "  Warning: failed to register plugin {}: {e}",
                        p.req.plugin_id
                    );
                } else {
                    println!(
                        "  Connected {0}. Send a message to the bot on {0} to get your pairing code, then run: /pairing approve <CODE>",
                        p.display_name
                    );
                }
            }
            if !credentials_json.is_empty() {
                client
                    .set_setting("credentials", &credentials_json)
                    .await
                    .context("Admin.SetSetting(credentials) failed")?;
            }
            Ok(())
        });
        if let Err(e) = install_outcome {
            eprintln!("  Warning: channel install partly failed: {e}");
            tracing::warn!("channel install daemon round-trip failed: {e}");
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn setup_instructs_batched_questions_for_purpose_channels_personality() {
        // Regression guard: the first-boot template must tell the agent to use
        // `request_user_input` with a batched `questions` array. The default
        // batch covers three enumerable topics — purpose, channels, and
        // personality/communication style. Timezone is auto-detected on the
        // host so it's intentionally NOT in the default example.
        let setup = generate_setup("Borg", "Dave");
        let setup_lc = setup.to_lowercase();
        assert!(
            setup_lc.contains("request_user_input"),
            "SETUP.md must reference the request_user_input tool"
        );
        assert!(
            setup_lc.contains("\"questions\""),
            "SETUP.md must instruct using the batched `questions` array shape"
        );
        for id in ["purpose", "channels", "personality"] {
            assert!(
                setup_lc.contains(&format!("\"id\": \"{id}\"")),
                "SETUP.md must include a `{id}` question id"
            );
        }
        // Timezone is auto-detected — it must NOT be in the default example.
        // The template explicitly tells the model to only add a timezone
        // question when the live time block shows UTC.
        assert!(
            !setup_lc.contains("\"id\": \"timezone\""),
            "SETUP.md must NOT include a default `timezone` question — host timezone is auto-detected"
        );
        // The legacy `choices` parameter is gone — guard against the template
        // being copy-pasted back from history.
        assert!(
            !setup.contains("`choices`"),
            "SETUP.md must not reference the removed `choices` parameter"
        );
    }

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
            ("grok-4-fast-non-reasoning", "xai", "XAI_API_KEY"),
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

    /// Table-driven check that model-id validation accepts the slug shapes
    /// real providers emit and rejects inputs that could inject into config
    /// TOML (quotes, newlines, internal whitespace). Both directions live in
    /// one test so a regression that flips a rule into being a no-op fails
    /// loudly here.
    #[test]
    fn validate_model_id_accepts_safe_and_rejects_unsafe() {
        let valid = [
            "anthropic/claude-sonnet-4",
            "openai/gpt-4.1-mini",
            "meta-llama/llama-4-scout",
            "llama3.3",
        ];
        let invalid = [
            ("", "empty string must not pass"),
            ("model\"", "embedded quote could inject into TOML"),
            ("model\nkey=val", "newline could inject a fake key"),
            ("model with spaces", "whitespace not allowed in model slug"),
        ];
        for id in valid {
            validate_model_id(id).unwrap_or_else(|e| panic!("expected {id:?} to be accepted: {e}"));
        }
        for (id, why) in invalid {
            assert!(
                validate_model_id(id).is_err(),
                "expected {id:?} to be rejected ({why})"
            );
        }
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
        assert!(config.contains("source = \"keychain\""));
        let parsed: cortex::config::Config = toml::from_str(&config).expect("parse config");
        match parsed.llm.api_key {
            Some(SecretRef::Keychain { service, account }) => {
                assert_eq!(service, "borg-openrouter");
                assert_eq!(account, "borg");
            }
            other => panic!("expected Keychain ref, got {other:?}"),
        }
        // Should NOT contain api_key_env when using keychain
        assert!(!config.contains("api_key_env"));
    }

    #[test]
    fn generate_config_emits_aux_sentinel_for_remote_provider() {
        // Onboarding ships every config with `[llm.aux] provider=<same>
        // model="auto"` so background work has its own failover chain by
        // default. Sentinel-based — `for_auxiliary` resolves "auto" via
        // `aux_model_picker` at daemon-boot, so model id drift in this
        // file isn't a maintenance trap.
        let config = generate_config("claude-sonnet-4", "anthropic", "Mike", "Buddy", false)
            .expect("valid config");
        let parsed: cortex::config::Config = toml::from_str(&config).expect("config parses");
        let aux = parsed.llm.aux.expect("aux block must be emitted");
        assert_eq!(aux.provider.as_deref(), Some("anthropic"));
        assert!(
            aux.model_is_auto(),
            "aux model must be the 'auto' sentinel (got {:?})",
            aux.model
        );
    }

    #[test]
    fn generate_config_emits_aux_sentinel_for_openrouter() {
        // Mirrors the remote-provider test for openrouter, where the
        // model id includes a slash (`anthropic/claude-…`). Validates
        // the aux block stays on the same provider regardless of the
        // primary's model-slug shape.
        let config = generate_config(
            "anthropic/claude-sonnet-4",
            "openrouter",
            "Mike",
            "Buddy",
            false,
        )
        .expect("valid config");
        let parsed: cortex::config::Config = toml::from_str(&config).expect("config parses");
        let aux = parsed.llm.aux.expect("aux block must be emitted");
        assert_eq!(aux.provider.as_deref(), Some("openrouter"));
        assert!(aux.model_is_auto());
    }

    #[test]
    fn generate_config_emits_aux_sentinel_even_for_ollama() {
        // Local providers get the sentinel too — no special-casing in
        // onboarding. The runtime resolver returns `None` for ollama so
        // `for_auxiliary` silently falls through to primary. This locks
        // the "no special-casing" invariant: future contributors who add
        // a local provider must NOT need to remember to skip aux here.
        let config =
            generate_config("llama3.3", "ollama", "Mike", "Buddy", false).expect("valid config");
        let parsed: cortex::config::Config = toml::from_str(&config).expect("config parses");
        let aux = parsed
            .llm
            .aux
            .expect("aux block must be emitted for ollama too");
        assert_eq!(aux.provider.as_deref(), Some("ollama"));
        assert!(aux.model_is_auto());
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
    fn generate_config_bedrock_uses_region_comment_not_api_key() {
        let model = "anthropic.claude-3-7-sonnet-20250219-v1:0";
        let config = generate_config(model, "bedrock", "Mike", "Buddy", false).expect("valid");
        assert!(config.contains("provider = \"bedrock\""));
        assert!(config.contains(&format!("model = \"{model}\"")));
        assert!(config.contains("bedrock_region = \"us-east-1\""));
        assert!(!config.contains("api_key_env"));
        assert!(!config.contains("api_key = {"));
    }

    #[test]
    fn bedrock_models_non_empty_and_parseable() {
        assert!(!BEDROCK_MODELS.is_empty());
        for (id, _) in BEDROCK_MODELS {
            assert!(
                validate_model_id(id).is_ok(),
                "invalid bedrock model id: {id}"
            );
        }
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
        for (id, _) in BEDROCK_MODELS {
            assert!(
                validate_model_id(id).is_ok(),
                "bedrock model id invalid: {id}"
            );
        }
        for (id, _) in GROQ_MODELS {
            assert!(validate_model_id(id).is_ok(), "groq model id invalid: {id}");
        }
        for (id, _) in XAI_MODELS {
            assert!(validate_model_id(id).is_ok(), "xai model id invalid: {id}");
        }
        for (id, _) in CLAUDE_CLI_MODELS {
            assert!(
                validate_model_id(id).is_ok(),
                "claude-cli model id invalid: {id}"
            );
        }
    }

    #[test]
    fn claude_cli_provider_wired_through_onboarding() {
        // Real failure mode: the `match provider_id` in `models_for_provider`
        // forgets a case for `claude-cli` and silently falls through to the
        // OpenRouter list — the wizard would then offer Claude users a
        // bunch of OpenRouter model IDs that won't resolve. The non-empty
        // claude-cli list + the keyless requirement together exercise that
        // dispatch arm and the Provider parsing.
        let models = models_for_provider("claude-cli");
        assert!(
            !models.is_empty(),
            "claude-cli must surface a non-empty model list"
        );
        let first_model = models[0].0.as_str();
        assert_ne!(
            first_model,
            models_for_provider("openrouter")[0].0,
            "claude-cli must not fall through to OpenRouter models"
        );
        let provider = Provider::from_str("claude-cli").expect("claude-cli parses");
        assert!(
            !provider.requires_api_key(),
            "claude-cli must not require an API key (uses local binary)"
        );
    }

    #[test]
    fn atomic_write_creates_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.toml");
        atomic_write(&path, "key = \"value\"").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "key = \"value\"");
        // The target must be the only thing left behind — no temp residue
        // under any naming scheme.
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(leftovers, vec!["test.toml".to_string()]);
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
