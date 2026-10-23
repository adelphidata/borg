//! Providers a first-time user is offered, with the copy every surface renders.
//!
//! One ordered list for the TUI wizard, `/settings`, `/model`, the desktop app
//! and the browser, plus [`SETTINGS_ONLY`]: the providers Borg runs that the
//! first run does not offer, so every provider is choosable somewhere. Model ids come from [`super::model_catalog`], auth facts
//! from [`Provider`]; this module only owns the picker order and the words.
//! Keeping the display copy here (rather than in each client) is what stops
//! the surfaces from drifting into offering different answers to "which
//! provider?" — the daemon serves this list over `Admin.GetSetupStatus`.

use super::provider::Provider;
use std::str::FromStr;

/// One row of the provider picker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderChoice {
    /// Provider id as [`Provider::from_str`] accepts it.
    pub id: &'static str,
    /// Display name.
    pub label: &'static str,
    /// One line, outcome-worded, for the row under the label.
    pub blurb: &'static str,
    /// Page that issues the key. `None` for keyless and credential-chain
    /// providers, where there is nothing to paste.
    pub key_url: Option<&'static str>,
    /// Shown before the "More providers" fold on a first run. Three is the
    /// budget: the recommended pick plus the two names people already know.
    pub featured: bool,
}

impl ProviderChoice {
    /// The typed provider. Infallible for catalog rows — a test guards it.
    pub fn provider(&self) -> Provider {
        Provider::from_str(self.id)
            .unwrap_or_else(|_| panic!("catalog id {} is not a provider id", self.id))
    }

    /// Whether the first run has to collect a key for this row.
    pub fn needs_api_key(&self) -> bool {
        self.provider().requires_api_key()
    }

    /// Whether this is the preselected row.
    pub fn recommended(&self) -> bool {
        self.id == RECOMMENDED
    }
}

/// The preselected provider, and the block consulted when an id has no
/// curated model list of its own.
pub const RECOMMENDED: &str = "openrouter";

/// Picker order. The recommended entry leads.
pub const ONBOARDABLE: &[ProviderChoice] = &[
    ProviderChoice {
        id: "openrouter",
        label: "OpenRouter",
        blurb: "One key, every model behind it.",
        key_url: Some("https://openrouter.ai/keys"),
        featured: true,
    },
    ProviderChoice {
        id: "anthropic",
        label: "Anthropic",
        blurb: "Claude, straight from the source.",
        key_url: Some("https://console.anthropic.com/settings/keys"),
        featured: true,
    },
    ProviderChoice {
        id: "openai",
        label: "OpenAI",
        blurb: "GPT models with your own key.",
        key_url: Some("https://platform.openai.com/api-keys"),
        featured: true,
    },
    ProviderChoice {
        id: "gemini",
        label: "Gemini",
        blurb: "Google's models with an AI Studio key.",
        key_url: Some("https://aistudio.google.com/app/apikey"),
        featured: false,
    },
    ProviderChoice {
        id: "deepseek",
        label: "DeepSeek",
        blurb: "Fast, inexpensive reasoning models.",
        key_url: Some("https://platform.deepseek.com/api_keys"),
        featured: false,
    },
    ProviderChoice {
        id: "groq",
        label: "Groq",
        blurb: "Open models at very high speed.",
        key_url: Some("https://console.groq.com/keys"),
        featured: false,
    },
    ProviderChoice {
        id: "xai",
        label: "xAI",
        blurb: "Grok models with your own key.",
        key_url: Some("https://console.x.ai/"),
        featured: false,
    },
    ProviderChoice {
        id: "ollama",
        label: "Ollama",
        blurb: "A model running on this machine. Nothing leaves.",
        key_url: None,
        featured: false,
    },
    ProviderChoice {
        id: "claude-cli",
        label: "Claude Code CLI",
        blurb: "Your Claude subscription, through the claude command already signed in here.",
        key_url: None,
        featured: false,
    },
    ProviderChoice {
        id: "bedrock",
        label: "AWS Bedrock",
        blurb: "Claude through the AWS credentials on this machine.",
        key_url: None,
        featured: false,
    },
    ProviderChoice {
        id: "azure-openai",
        label: "Azure OpenAI",
        blurb:
            "OpenAI models on your Microsoft tenancy. Needs a resource and deployment in Settings.",
        key_url: Some("https://portal.azure.com/"),
        featured: false,
    },
];

/// Providers Borg runs that the first run does not offer. Each needs more
/// than one pasted key — a sign-in imported from another tool, an endpoint of
/// your own, or a cloud project — so they are chosen in Settings, where the
/// Provider page carries their setup. Settings lists them after
/// [`ONBOARDABLE`].
pub const SETTINGS_ONLY: &[ProviderChoice] = &[
    ProviderChoice {
        id: "claude-code-oauth",
        label: "Claude Code",
        blurb: "The sign-in Claude Code saved on this machine, imported.",
        key_url: None,
        featured: false,
    },
    ProviderChoice {
        id: "codex-oauth",
        label: "Codex",
        blurb: "The ChatGPT sign-in Codex saved on this machine, imported.",
        key_url: None,
        featured: false,
    },
    ProviderChoice {
        id: "copilot-oauth",
        label: "GitHub Copilot",
        blurb: "Copilot's models with a Copilot token.",
        key_url: None,
        featured: false,
    },
    ProviderChoice {
        id: "vertex-ai",
        label: "Vertex AI",
        blurb: "Gemini and Claude through your Google Cloud project.",
        key_url: None,
        featured: false,
    },
    ProviderChoice {
        id: "custom",
        label: "Custom endpoint",
        blurb: "Any OpenAI-compatible server, such as LM Studio, vLLM or llama.cpp.",
        key_url: None,
        featured: false,
    },
];

/// The curated block a row's models come from. A sign-in that speaks a keyed
/// provider's API offers that provider's models; every other row reads its
/// own block, and one without a block has no list — its model is typed.
pub fn models_source(id: &str) -> &str {
    match id {
        "claude-code-oauth" => "anthropic",
        "codex-oauth" => "openai",
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::model_catalog::embedded_manifest;

    /// A misspelled id would compile fine and then fail at the first
    /// `Provider::from_str` on the RPC path. Round-trip every row.
    #[test]
    fn every_id_is_a_real_provider_spelled_canonically() {
        for c in ONBOARDABLE.iter().chain(SETTINGS_ONLY) {
            let p =
                Provider::from_str(c.id).unwrap_or_else(|_| panic!("{} is not a provider", c.id));
            assert_eq!(p.as_str(), c.id, "catalog must use the canonical id");
        }
    }

    /// A keyed provider with no link is the one setup step a first-time
    /// user cannot finish unaided; a keyless one with a link is a lie.
    #[test]
    fn key_url_present_exactly_when_a_key_is_needed() {
        for c in ONBOARDABLE {
            assert_eq!(
                c.key_url.is_some(),
                c.needs_api_key(),
                "{}: key_url and needs_api_key disagree",
                c.id
            );
        }
    }

    /// The wizard preselects a model for every row; a provider without a
    /// curated block would preselect nothing.
    #[test]
    fn every_row_has_a_curated_block_with_a_default() {
        let manifest = embedded_manifest();
        for c in ONBOARDABLE {
            assert!(
                manifest.default_model(c.id).is_some(),
                "{} has no catalog block or no default model",
                c.id
            );
        }
    }

    /// A provider with code behind it and no row is one nobody can pick
    /// outside a config file. Each appears exactly once across both lists.
    #[test]
    fn every_provider_borg_runs_has_exactly_one_row() {
        for p in Provider::ALL {
            let rows = ONBOARDABLE
                .iter()
                .chain(SETTINGS_ONLY)
                .filter(|c| c.provider() == *p)
                .count();
            assert_eq!(rows, 1, "{} has {rows} catalog rows", p.as_str());
        }
    }

    /// A borrowed list must exist, or the sign-in would preselect nothing
    /// while claiming its sibling's models.
    #[test]
    fn a_borrowed_model_list_has_a_default() {
        let manifest = embedded_manifest();
        for c in SETTINGS_ONLY {
            let source = models_source(c.id);
            if source != c.id {
                assert!(
                    manifest.default_model(source).is_some(),
                    "{} borrows {source}, which has no default model",
                    c.id
                );
            }
        }
    }

    #[test]
    fn recommended_leads_and_ids_are_unique() {
        assert_eq!(ONBOARDABLE[0].id, RECOMMENDED);
        assert!(ONBOARDABLE[0].recommended());
        let mut ids: Vec<&str> = ONBOARDABLE.iter().map(|c| c.id).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), ONBOARDABLE.len(), "duplicate provider id");
        assert_eq!(
            ONBOARDABLE.iter().filter(|c| c.featured).count(),
            3,
            "the first-run fold shows exactly three rows"
        );
    }
}
