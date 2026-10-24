use anyhow::{bail, Context, Result};

use std::str::FromStr;
use std::sync::LazyLock;

use cortex::config::Config;
use cortex::constants::{IDENTITY_FILE, MEMORY_INDEX_FILE, SETUP_FILE};
use cortex::llm::provider::Provider;
use cortex::security::secrets_resolve::provider_api_key_secret_ref;
use protocols::admin::{PluginInstallRequest, PluginInstalledChannel};

use crate::async_bridge::block_on;
use crate::daemon_client;

/// Standard subdirectories created under the Borg data directory.
pub(crate) const BORG_SUBDIRS: &[&str] = &["memory", "tools", "skills", "logs", "cache"];

/// Catalog provider consulted when the requested one has no curated block.
///
/// OpenRouter is the widest list and needs only one credential, so an id we do
/// not recognise still lands somewhere a new operator can pick from.
pub(crate) const FALLBACK_PROVIDER: &str = cortex::llm::provider_catalog::RECOMMENDED;

/// Provider choices as `(id, label, blurb)`, in picker order.
///
/// A view over [`cortex::llm::provider_catalog::ONBOARDABLE`] — the same list
/// the daemon serves to the desktop and browser — kept in tuple shape because
/// the wizard and the `/settings` and `/model` popups index and destructure it.
pub(crate) static PROVIDERS: LazyLock<Vec<(&'static str, &'static str, &'static str)>> =
    LazyLock::new(|| {
        cortex::llm::provider_catalog::ONBOARDABLE
            .iter()
            .map(|c| (c.id, c.label, c.blurb))
            .collect()
    });

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
/// The curated catalog ([`cortex::llm::model_catalog`]) is the single source of
/// truth for ids, order, and labels. It ships compiled into the binary, so an
/// airgapped install still gets a full picker, and refreshes from disk cache or
/// the published manifest when there is a network.
///
/// Two providers overlay a live list on top: Ollama reports what is actually
/// pulled on this machine, and OpenRouter prunes the curated list down to what
/// it currently serves. Both fall through to the catalog when their cache is
/// cold.
pub(crate) fn models_for_provider(provider_id: &str) -> Vec<(String, String)> {
    match provider_id {
        "ollama" => {
            if let Some(live) = crate::ollama_catalog::cached_models() {
                return live;
            }
        }
        "openrouter" => {
            if let Some(live) = crate::openrouter_catalog::cached_models() {
                return live;
            }
        }
        _ => {}
    }
    let curated = cortex::llm::model_catalog::curated(provider_id);
    if curated.is_empty() {
        // An unknown provider id still has to render something choosable.
        return cortex::llm::model_catalog::curated(FALLBACK_PROVIDER);
    }
    curated
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

    persist_onboarding_settings(result, use_keychain)?;

    write_seed_file_if_absent(
        &data_dir,
        IDENTITY_FILE,
        &generate_identity(&result.agent_name, &result.user_name),
    )?;
    write_seed_file_if_absent(
        &data_dir,
        MEMORY_INDEX_FILE,
        &format!(
            "# Memory Index\n\n## Owner\n- Name: {}\n- Agent: {}\n",
            result.user_name, result.agent_name
        ),
    )?;
    write_seed_file_if_absent(
        &data_dir,
        SETUP_FILE,
        &generate_setup(&result.agent_name, &result.user_name),
    )?;

    store_llm_api_key(result, use_keychain, &data_dir)?;

    install_channel_plugins(result, &data_dir)
}

/// Write the wizard's choices via the daemon (single writer, cache-coherent).
fn persist_onboarding_settings(result: &OnboardingResult, use_keychain: bool) -> Result<()> {
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
    .context("Failed to persist onboarding settings via daemon")
}

/// Atomically write a first-boot seed file, but never overwrite one that
/// already exists — re-running onboarding must not clobber user edits.
/// On failure, sweep any .tmp leftovers so a retry starts clean.
fn write_seed_file_if_absent(
    data_dir: &std::path::Path,
    file_name: &str,
    content: &str,
) -> Result<()> {
    let path = data_dir.join(file_name);
    if path.exists() {
        return Ok(());
    }
    if let Err(e) = atomic_write(&path, content) {
        cleanup_tmp_files(data_dir);
        return Err(e.context(format!("Failed to write {file_name} during onboarding")));
    }
    Ok(())
}

/// Store the LLM API key: keychain when available, else the 0600
/// `~/.borg/.env` file. A keychain failure downgrades the daemon-side
/// `llm.api_key` SecretRef to the env-var form before falling back.
fn store_llm_api_key(
    result: &OnboardingResult,
    use_keychain: bool,
    data_dir: &std::path::Path,
) -> Result<()> {
    let Some(ref api_key) = result.api_key else {
        return Ok(());
    };
    if use_keychain {
        match store_in_keychain(&result.provider, api_key) {
            Ok(()) => return Ok(()),
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
            }
        }
    }
    let provider = Provider::from_str(&result.provider)?;
    let env_var = provider.default_env_var();
    append_env_file(data_dir, env_var, api_key)?;
    println!("  Updated {}", data_dir.join(".env").display());
    Ok(())
}

/// Store one channel's credentials: OS keychain first; on headless servers
/// (no secret-service / dbus) it fails — fall back to a 0600 ~/.borg/.env
/// file with an env-var SecretRef so the daemon still resolves the credential
/// through systemd's `EnvironmentFile=`. Without this fallback the channel
/// install was silently dropped on every Linux VPS.
fn store_channel_credentials(
    cfg: &mut Config,
    service: &str,
    credentials: &[(String, String)],
    data_dir: &std::path::Path,
) {
    for (key, value) in credentials {
        let account = format!("borg-{key}");
        match plugins::keychain::store(service, &account, value) {
            Ok(()) => {
                cfg.credentials.insert(
                    key.clone(),
                    cortex::config::CredentialValue::Ref(
                        cortex::security::secrets_resolve::SecretRef::Keychain {
                            service: service.to_string(),
                            account,
                        },
                    ),
                );
            }
            Err(e) => {
                tracing::warn!("Keychain store failed for {key}: {e}; falling back to .env");
                eprintln!(
                    "  · Keychain unavailable for {key}; storing in {}/.env",
                    data_dir.display()
                );
                if let Err(e) = append_env_file(data_dir, key, value) {
                    eprintln!("  Warning: failed to write .env for {key}: {e}");
                    tracing::warn!("append_env_file failed for {key}: {e}");
                    continue;
                }
                cfg.credentials.insert(
                    key.clone(),
                    cortex::config::CredentialValue::Ref(
                        cortex::security::secrets_resolve::SecretRef::Env { var: key.clone() },
                    ),
                );
            }
        }
    }
}

/// Build the daemon-side install request for one catalog channel definition.
fn build_channel_install_request(def: &plugins::catalog::PluginDef) -> PluginInstallRequest {
    let channel_name = def.id.rsplit('/').next().unwrap_or(def.id);
    let runtime = if def.is_native { "native" } else { "python" };
    let webhook = format!("/webhook/{channel_name}");
    PluginInstallRequest {
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
    }
}

/// Install the channel plugins picked during onboarding: store credentials,
/// then register each plugin and persist the merged credentials map through
/// the daemon in a single round-trip so the cli never opens the DB itself.
fn install_channel_plugins(result: &OnboardingResult, data_dir: &std::path::Path) -> Result<()> {
    if result.channels.is_empty() {
        println!(
            "  No channels connected. Add one later with `borg add <name>` (e.g. `borg add telegram`)."
        );
        return Ok(());
    }

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

        store_channel_credentials(&mut cfg, &def.service_name(), credentials, data_dir);

        pending.push(PendingInstall {
            req: build_channel_install_request(def),
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
    fn bedrock_models_non_empty_and_parseable() {
        let models = models_for_provider("bedrock");
        assert!(!models.is_empty());
        for (id, _) in &models {
            assert!(
                cortex::llm::model_catalog::validate_model_id(id).is_ok(),
                "invalid bedrock model id: {id}"
            );
        }
    }

    #[test]
    fn ollama_models_non_empty() {
        let models = models_for_provider("ollama");
        assert!(!models.is_empty());
        for (id, _) in &models {
            assert!(
                cortex::llm::model_catalog::validate_model_id(id).is_ok(),
                "invalid model id: {id}"
            );
        }
    }

    #[test]
    fn bundled_model_ids_all_parse() {
        // Regression guard: every curated model ID must pass the catalog's validate_model_id.
        // A typo (or an injected char) in the catalog would otherwise reach the
        // model setting and the provider URL. Driven off the catalog
        // rather than a fixed provider list, so a new provider block is covered
        // the day it lands.
        // Reads the compiled-in floor, not process state: `models_for_provider`
        // would otherwise assert against whatever this machine has cached in
        // `~/.borg/cache/model-catalog.json`.
        let floor = cortex::llm::model_catalog::embedded_manifest();
        let providers = floor.provider_names();
        assert!(
            providers.len() >= 10,
            "catalog lost providers: {providers:?}"
        );
        for provider in &providers {
            let models = floor.curated(provider);
            assert!(!models.is_empty(), "no models for provider: {provider}");
            for (id, _) in &models {
                assert!(
                    cortex::llm::model_catalog::validate_model_id(id).is_ok(),
                    "{provider} model id invalid: {id}"
                );
            }
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

    /// Every provider the wizard offers must have a curated block, or the
    /// picker silently hands that provider OpenRouter's list — ids the
    /// provider will not serve.
    #[test]
    fn wizard_rows_mirror_the_shared_catalog() {
        // The popups index PROVIDERS by position and hand the id to the
        // daemon; a view that reorders or drops a row would select the
        // wrong provider silently.
        let shared = cortex::llm::provider_catalog::ONBOARDABLE;
        assert_eq!(PROVIDERS.len(), shared.len());
        for (i, (id, label, _)) in PROVIDERS.iter().enumerate() {
            assert_eq!((*id, *label), (shared[i].id, shared[i].label));
        }
    }

    /// An id with no curated block still renders something choosable, and it
    /// is the fallback provider's list rather than an empty picker.
    #[test]
    fn unknown_provider_falls_back_rather_than_rendering_nothing() {
        let got = models_for_provider("no-such-provider");
        assert_eq!(got, cortex::llm::model_catalog::curated(FALLBACK_PROVIDER));
        assert!(!got.is_empty());
        // The fallback has to be a real provider block, not an accident of the
        // running machine's cache.
        assert!(!cortex::llm::model_catalog::embedded_manifest()
            .curated(FALLBACK_PROVIDER)
            .is_empty());
    }

    /// Exactly one entry per provider carries the recommended badge, and it is
    /// the first row — the wizard preselects index 0.
    #[test]
    fn the_recommended_pick_leads_every_provider_list() {
        let floor = cortex::llm::model_catalog::embedded_manifest();
        for provider in floor.provider_names() {
            let models = floor.curated(&provider);
            let flagged: Vec<&String> = models
                .iter()
                .filter(|(_, label)| label.contains("(recommended)"))
                .map(|(id, _)| id)
                .collect();
            assert_eq!(
                flagged.len(),
                1,
                "{provider} recommended count: {flagged:?}"
            );
            assert_eq!(
                flagged[0], &models[0].0,
                "{provider} recommends a row that is not first"
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

    #[test]
    fn seed_file_written_when_absent_and_never_clobbered() {
        let dir = tempfile::tempdir().unwrap();
        write_seed_file_if_absent(dir.path(), "IDENTITY.md", "seed").unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("IDENTITY.md")).unwrap(),
            "seed"
        );
        // Re-running onboarding must preserve user edits, not re-seed.
        std::fs::write(dir.path().join("IDENTITY.md"), "user edited").unwrap();
        write_seed_file_if_absent(dir.path(), "IDENTITY.md", "seed").unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("IDENTITY.md")).unwrap(),
            "user edited"
        );
    }

    #[test]
    fn channel_install_request_derives_name_webhook_and_runtime() {
        let def = plugins::catalog::find_by_id("messaging/telegram")
            .expect("telegram must exist in the catalog");
        let req = build_channel_install_request(def);
        assert_eq!(req.plugin_id, "messaging/telegram");
        let channel = req.installed_channel.expect("channel plugin sets channel");
        // The gateway routes by the short channel name, not the catalog id.
        assert_eq!(channel.name, "telegram");
        assert_eq!(channel.webhook, "/webhook/telegram");
        // Telegram is a native integration, so its channel row must select
        // the native runtime, not the python plugin lane.
        assert_eq!(channel.runtime, "native");
        // Credentials are persisted once at the end of onboarding, never per
        // install RPC.
        assert!(req.credentials_json.is_empty());
    }
}
