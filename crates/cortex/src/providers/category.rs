//! Provider category — the air-gap and audit-tier discriminator.

use crate::llm::provider::Provider;

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
            | Provider::AzureOpenAi
            | Provider::VertexAi => ProviderCategory::PublicCloud,
            Provider::Ollama | Provider::ClaudeCli => ProviderCategory::Local,
            // Custom is the BYO escape hatch — air-gap policy is enforced on
            // the resolved address (loopback/RFC1918 OK), not compile-time.
            Provider::Custom => ProviderCategory::BYOEndpoint,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_local_is_air_gap_safe() {
        // Spec: air-gap mode must refuse public-cloud AND BYO endpoints —
        // the latter because we can't statically prove the URL targets a
        // local resource. A regression that flipped BYO to "safe" would
        // silently let an air-gapped operator phone home.
        assert!(ProviderCategory::Local.is_air_gap_safe());
        assert!(!ProviderCategory::PublicCloud.is_air_gap_safe());
        assert!(!ProviderCategory::BYOEndpoint.is_air_gap_safe());
    }

    #[test]
    fn db_str_round_trip_for_every_variant() {
        // The DB column stores these values; a rename without a migration
        // is silently corrupting on first restart. Lock the wire form.
        for c in [
            ProviderCategory::PublicCloud,
            ProviderCategory::Local,
            ProviderCategory::BYOEndpoint,
        ] {
            let s = c.as_db_str();
            assert_eq!(ProviderCategory::from_db_str(s), Some(c));
        }
        // Also lock the exact strings — the migration test for an older
        // DB depends on these literals matching.
        assert_eq!(ProviderCategory::PublicCloud.as_db_str(), "public_cloud");
        assert_eq!(ProviderCategory::Local.as_db_str(), "local");
        assert_eq!(ProviderCategory::BYOEndpoint.as_db_str(), "byo_endpoint");
    }

    #[test]
    fn from_db_str_returns_none_for_unknown() {
        // Negative: unknown DB values surface as None so callers can
        // fall back / quarantine. A panic / wrong-default here would
        // make an upgraded DB unstartable.
        assert_eq!(ProviderCategory::from_db_str(""), None);
        assert_eq!(ProviderCategory::from_db_str("Local"), None); // case-sensitive
        assert_eq!(ProviderCategory::from_db_str("future_category"), None);
    }

    #[test]
    fn provider_to_category_classifies_oauth_as_public_cloud() {
        // OAuth-credential providers still hit the same public-cloud
        // endpoints — they MUST be classified as PublicCloud, not Local,
        // or air-gap enforcement is bypassed. Encode the spec invariant.
        for p in [
            Provider::OpenAi,
            Provider::Anthropic,
            Provider::OpenRouter,
            Provider::Gemini,
            Provider::DeepSeek,
            Provider::Groq,
            Provider::XAi,
            Provider::ClaudeCodeOAuth,
            Provider::CodexOAuth,
            Provider::CopilotOAuth,
            Provider::AzureOpenAi,
            Provider::VertexAi,
            Provider::Bedrock,
        ] {
            assert_eq!(
                ProviderCategory::from(p),
                ProviderCategory::PublicCloud,
                "{p:?} must classify as PublicCloud"
            );
        }
    }

    #[test]
    fn local_providers_classify_as_local() {
        // Ollama and the Claude Code CLI both run on the operator's
        // machine. A misclassification to PublicCloud would silently
        // disable them under air-gap mode.
        assert_eq!(
            ProviderCategory::from(Provider::Ollama),
            ProviderCategory::Local
        );
        assert_eq!(
            ProviderCategory::from(Provider::ClaudeCli),
            ProviderCategory::Local
        );
    }

    #[test]
    fn custom_provider_classifies_as_byo_endpoint() {
        // The BYO category triggers the runtime address check in air-gap
        // mode rather than a compile-time refusal. A miscategorization
        // would either over-block (Local) or under-block (PublicCloud).
        assert_eq!(
            ProviderCategory::from(Provider::Custom),
            ProviderCategory::BYOEndpoint
        );
    }
}
