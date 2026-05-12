//! Provider category — the air-gap and audit-tier discriminator.

use crate::provider::Provider;

/// Provider categories. Air-gap mode refuses anything that isn't `Local`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderCategory {
    /// Hosted on the public internet (OpenAI, Anthropic, Gemini, Groq,
    /// DeepSeek, OpenRouter). Refused under `outbound_allowed = false`.
    PublicCloud,
    /// Runs on this machine or a trusted local endpoint (Ollama, Claude
    /// Code CLI). Always allowed regardless of air-gap setting.
    Local,
    /// User-pasted endpoint. OpenAI-compatible by assumption. Honors
    /// air-gap if the resolved address is non-loopback / non-RFC1918.
    BYOEndpoint,
}

impl ProviderCategory {
    /// Whether this category is permitted under air-gap mode.
    pub fn is_air_gap_safe(self) -> bool {
        matches!(self, ProviderCategory::Local)
    }

    /// String form for storage in `providers.category`.
    pub fn as_db_str(self) -> &'static str {
        match self {
            ProviderCategory::PublicCloud => "public_cloud",
            ProviderCategory::Local => "local",
            ProviderCategory::BYOEndpoint => "byo_endpoint",
        }
    }

    /// Inverse of [`Self::as_db_str`]. Returns `None` for unknown values.
    pub fn from_db_str(s: &str) -> Option<Self> {
        match s {
            "public_cloud" => Some(ProviderCategory::PublicCloud),
            "local" => Some(ProviderCategory::Local),
            "byo_endpoint" => Some(ProviderCategory::BYOEndpoint),
            _ => None,
        }
    }
}

/// Default category for each built-in [`Provider`] variant. BYO endpoints
/// don't show up here because they have no compile-time identity — they're
/// stored as rows.
impl From<Provider> for ProviderCategory {
    fn from(p: Provider) -> Self {
        match p {
            Provider::OpenRouter
            | Provider::OpenAi
            | Provider::Anthropic
            | Provider::Bedrock
            | Provider::Gemini
            | Provider::DeepSeek
            | Provider::Groq
            | Provider::XAi
            // OAuth flavors hit the same public clouds; same air-gap rules apply.
            | Provider::ClaudeCodeOAuth
            | Provider::CodexOAuth
            | Provider::CopilotOAuth
            | Provider::AzureOpenAi => ProviderCategory::PublicCloud,
            Provider::Ollama | Provider::ClaudeCli => ProviderCategory::Local,
            // Custom is the BYO escape hatch — air-gap policy is enforced on
            // the resolved address (loopback/RFC1918 OK), not compile-time.
            Provider::Custom => ProviderCategory::BYOEndpoint,
        }
    }
}
