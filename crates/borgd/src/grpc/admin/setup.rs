//! `Admin.GetSetupStatus` — the first-run screen's one round trip.
//!
//! Serves the shared provider catalog with curated models, what is
//! configured now, and every credential the daemon can already see. The
//! desktop and browser render this rather than carrying their own lists, so
//! a catalog change lands on every surface at once.

use super::*;
use cortex::llm::credential_detect::{self, LocalCredential};
use cortex::llm::provider::Provider;
use cortex::llm::provider_catalog::{self, ONBOARDABLE, SETTINGS_ONLY};
use protocols::admin::{DetectedCredential, ModelChoice, ProviderChoice, SetupStatus};
use std::str::FromStr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub(super) async fn get_setup_status(
    _req: Request<Empty>,
) -> Result<Response<SetupStatus>, TStatus> {
    let status = async {
        let db = Database::open().await?;
        let config = cortex::config::Config::load_from_db().await?;
        let data_dir = cortex::config::Config::data_dir()?;
        let base_url = config.llm.base_url.clone();
        // TCP probe, `which`, and on macOS a keychain subprocess: off the
        // executor, never inline in the handler.
        let local = tokio::task::spawn_blocking(move || {
            let mut found = credential_detect::detect_keys(&data_dir);
            found.extend(local_runtimes_cached(base_url.as_deref()));
            found
        })
        .await?;

        let mut detected: Vec<DetectedCredential> = local.iter().map(local_to_proto).collect();
        detected.extend(stored_detections(&db).await?);

        let provider = config.llm.provider.clone().unwrap_or_default();
        let typed = Provider::from_str(&provider).ok();
        let mut configured = is_configured(typed, &detected);
        if !configured {
            // Last resort. Building the pool resolves every secret ref, and a
            // keychain or exec ref spawns a subprocess on this executor, so
            // the cheap sources above answer for the common case and this
            // runs only when they found nothing.
            detected.extend(pool_detections(&config, &db).await);
            configured = is_configured(typed, &detected);
        }
        anyhow::Ok(SetupStatus {
            configured,
            provider,
            model: config.llm.model.clone(),
            providers: provider_choices(),
            detected,
            agent_name: agent_name(&config),
            agent_names: cortex::agent_name::BORG_NAMES
                .iter()
                .map(ToString::to_string)
                .collect(),
        })
    }
    .await
    .map_err(map_internal("get_setup_status"))?;
    Ok(Response::new(status))
}

/// `user.agent_name`, else the name a hand-written `IDENTITY.md` already
/// gives itself, so the first-run screen shows (and keeps) that name rather
/// than suggesting a random one over it. "" when neither has one.
fn agent_name(config: &cortex::config::Config) -> String {
    if let Some(name) = config
        .user
        .agent_name
        .as_deref()
        .filter(|n| !n.trim().is_empty())
    {
        return name.to_string();
    }
    cortex::identity::identity_path()
        .ok()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|c| cortex::agent_name::name_in_identity(&c).map(str::to_string))
        .unwrap_or_default()
}

/// How long a runtime probe result is reused. Every page that names a
/// provider asks for setup status, and the Claude CLI check reads the macOS
/// keychain through a subprocess — once a minute is plenty for "is Ollama
/// up" and keeps the daemon from spawning `security` on every navigation.
const RUNTIME_PROBE_TTL: Duration = Duration::from_secs(60);

/// The last runtime probe: when it ran, which Ollama origin it asked, what it found.
struct RuntimeProbe {
    at: Instant,
    base_url: Option<String>,
    found: Vec<LocalCredential>,
}

static RUNTIME_PROBE: Mutex<Option<RuntimeProbe>> = Mutex::new(None);

/// `detect_local_runtimes`, memoised per Ollama base URL for [`RUNTIME_PROBE_TTL`].
fn local_runtimes_cached(base_url: Option<&str>) -> Vec<LocalCredential> {
    let mut slot = RUNTIME_PROBE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(probe) = slot.as_ref() {
        if probe.at.elapsed() < RUNTIME_PROBE_TTL && probe.base_url.as_deref() == base_url {
            return probe.found.clone();
        }
    }
    let found = credential_detect::detect_local_runtimes(base_url);
    *slot = Some(RuntimeProbe {
        at: Instant::now(),
        base_url: base_url.map(str::to_string),
        found: found.clone(),
    });
    found
}

fn local_to_proto(c: &LocalCredential) -> DetectedCredential {
    DetectedCredential {
        provider: c.provider.as_str().to_string(),
        source: c.source.as_str().to_string(),
        detail: c.detail.clone(),
    }
}

/// `credentials.{provider}` routing rows → `stored` / `keychain`.
async fn stored_detections(db: &Database) -> anyhow::Result<Vec<DetectedCredential>> {
    let entries = credentials::api_key_entries(db).await?;
    Ok(entries
        .into_iter()
        .filter_map(|e| {
            let source = match e.backend.as_str() {
                "literal" => "stored",
                "keychain" => "keychain",
                _ => return None,
            };
            Some(DetectedCredential {
                provider: e.provider,
                source: source.to_string(),
                detail: "saved in Borg".to_string(),
            })
        })
        .collect())
}

/// Live entries in the primary pool → `pool`. These are the credentials the
/// resolver actually handed the LLM client, so they are the proof that a
/// request can be sent. A pool that cannot be built (no provider, nothing
/// resolvable) is the ordinary not-configured state, not an error.
async fn pool_detections(
    config: &cortex::config::Config,
    db: &Database,
) -> Vec<DetectedCredential> {
    let pool = match build_primary_pool(config, db).await {
        Ok(pool) => pool,
        Err(e) => {
            tracing::debug!("setup status: no credential pool yet: {e}");
            return Vec::new();
        }
    };
    let provider = pool.provider_str();
    pool.entries()
        .iter()
        .filter(|c| !c.is_removed())
        .map(|c| {
            let entry = pooled_to_proto(provider, c);
            DetectedCredential {
                provider: entry.provider,
                source: "pool".to_string(),
                detail: format!("{}:{}", entry.source_kind, entry.label),
            }
        })
        .collect()
}

/// Catalog → wire: the first-run rows, then the Settings-only ones. A
/// first-run row whose curated block is missing borrows the recommended
/// provider's models, the same rule the TUI wizard applies, so the first run
/// never preselects nothing. A Settings-only row without a list keeps none:
/// its model is typed, and another provider's ids would only be wrong there.
fn provider_choices() -> Vec<ProviderChoice> {
    let rows = ONBOARDABLE
        .iter()
        .map(|c| (c, true))
        .chain(SETTINGS_ONLY.iter().map(|c| (c, false)));
    rows.map(|(c, first_run)| {
        let mut entries =
            cortex::llm::model_catalog::curated_entries(provider_catalog::models_source(c.id));
        if entries.is_empty() && first_run {
            entries = cortex::llm::model_catalog::curated_entries(provider_catalog::RECOMMENDED);
        }
        let default_model = entries
            .iter()
            .find(|e| e.default)
            .or_else(|| entries.first())
            .map(|e| e.id.clone())
            .unwrap_or_default();
        ProviderChoice {
            id: c.id.to_string(),
            label: c.label.to_string(),
            blurb: c.blurb.to_string(),
            key_url: c.key_url.unwrap_or_default().to_string(),
            needs_api_key: c.needs_api_key(),
            recommended: c.recommended(),
            featured: c.featured,
            first_run,
            default_model,
            models: entries
                .into_iter()
                .map(|e| ModelChoice {
                    id: e.id,
                    label: e.label,
                    note: e.note.unwrap_or_default(),
                })
                .collect(),
        }
    })
    .collect()
}

/// The decision the first-run screen branches on.
///
/// Keyless runtimes need their own liveness signal — an API key lying
/// around for some other provider says nothing about whether `claude` is
/// signed in. The AWS chain is assumed present, matching the wizard.
fn is_configured(provider: Option<Provider>, detected: &[DetectedCredential]) -> bool {
    let Some(provider) = provider else {
        return false;
    };
    if provider.uses_aws_credential_chain() {
        return true;
    }
    let mine = |c: &&DetectedCredential| c.provider.eq_ignore_ascii_case(provider.as_str());
    match provider {
        Provider::Ollama => detected.iter().filter(mine).any(|c| c.source == "ollama"),
        Provider::ClaudeCli => detected
            .iter()
            .filter(mine)
            .any(|c| c.source == "claude_cli"),
        p if !p.requires_api_key() => false,
        _ => detected.iter().filter(mine).any(|c| {
            matches!(
                c.source.as_str(),
                "env" | "dotenv" | "stored" | "keychain" | "pool"
            )
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn det(provider: &str, source: &str) -> DetectedCredential {
        DetectedCredential {
            provider: provider.into(),
            source: source.into(),
            detail: String::new(),
        }
    }

    #[test]
    fn configured_needs_a_credential_of_the_right_kind() {
        let cases: &[(&str, Option<Provider>, Vec<DetectedCredential>, bool)] = &[
            (
                "no provider chosen",
                None,
                vec![det("openai", "stored")],
                false,
            ),
            (
                "stored key for the chosen provider",
                Some(Provider::OpenAi),
                vec![det("openai", "stored")],
                true,
            ),
            (
                "key for another provider",
                Some(Provider::Anthropic),
                vec![det("openai", "stored")],
                false,
            ),
            (
                "provider match ignores case",
                Some(Provider::OpenAi),
                vec![det("OpenAI", "env")],
                true,
            ),
            (
                "claude-cli is not satisfied by an api key",
                Some(Provider::ClaudeCli),
                vec![det("claude-cli", "env")],
                false,
            ),
            (
                "claude-cli signed in",
                Some(Provider::ClaudeCli),
                vec![det("claude-cli", "claude_cli")],
                true,
            ),
            (
                "ollama needs a live server",
                Some(Provider::Ollama),
                vec![det("ollama", "dotenv")],
                false,
            ),
            (
                "ollama reachable",
                Some(Provider::Ollama),
                vec![det("ollama", "ollama")],
                true,
            ),
            (
                "bedrock assumes the aws chain",
                Some(Provider::Bedrock),
                vec![],
                true,
            ),
            (
                "keyed provider with nothing",
                Some(Provider::Groq),
                vec![],
                false,
            ),
        ];
        for (name, provider, detected, want) in cases {
            assert_eq!(is_configured(*provider, detected), *want, "{name}");
        }
    }

    #[test]
    fn every_first_run_choice_preselects_a_real_model() {
        for c in provider_choices().into_iter().filter(|c| c.first_run) {
            assert!(!c.models.is_empty(), "{} has no models", c.id);
            assert!(
                c.models.iter().any(|m| m.id == c.default_model),
                "{}: default {} is not in its own list",
                c.id,
                c.default_model
            );
            assert_eq!(c.key_url.is_empty(), !c.needs_api_key, "{}", c.id);
        }
    }

    #[test]
    fn settings_only_choices_follow_the_first_run_and_never_borrow_a_stranger() {
        let choices = provider_choices();
        let first_settings_only = choices.iter().position(|c| !c.first_run).unwrap();
        assert!(
            choices[first_settings_only..].iter().all(|c| !c.first_run),
            "a first-run row sorts after a Settings-only one"
        );
        let by_id = |id: &str| choices.iter().find(|c| c.id == id).unwrap();
        // A sign-in offers the models of the API it speaks.
        assert_eq!(
            by_id("claude-code-oauth").default_model,
            by_id("anthropic").default_model
        );
        assert_eq!(
            by_id("codex-oauth").default_model,
            by_id("openai").default_model
        );
        // No curated list means a typed model, not OpenRouter's slugs.
        for id in ["custom", "vertex-ai", "copilot-oauth"] {
            let c = by_id(id);
            assert!(c.models.is_empty(), "{id} borrowed a list");
            assert_eq!(c.default_model, "", "{id} preselects a model");
        }
    }
}
