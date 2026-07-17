//! Per-provider model retirement table.
//!
//! Each row records that a specific model id is scheduled for retirement on a
//! known date and names the recommended replacement. The `borg doctor`
//! "Provider" step looks up the active model against this table and surfaces
//! a warning if it matches — operators get a heads-up before the upstream
//! provider starts rejecting requests.
//!
//! Add entries when an upstream provider publishes a retirement notice.
//! Replacements should be a model that's already GA and stable.

use crate::llm::provider::Provider;

/// One row in the deprecation table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeprecatedModel {
    /// Provider this rule applies to.
    pub provider: Provider,
    /// Exact model id (post-`normalize_model`) being retired.
    pub model: &'static str,
    /// ISO-8601 date the provider has announced the retirement, e.g. "2026-05-15".
    pub retires_on: &'static str,
    /// Replacement the operator should switch to.
    pub replacement: &'static str,
    /// Provider-published advisory URL, for the doctor `user_hint`.
    pub advisory_url: &'static str,
}

/// Static deprecation table. Extend per provider; keep entries sorted by
/// `(provider, model)` for diff-friendliness.
const DEPRECATIONS: &[DeprecatedModel] = &[
    // DeepSeek — the legacy aliases retire 2026-07-24; both already route to
    // V4 Flash (non-thinking / thinking), so the replacement is a rename.
    DeprecatedModel {
        provider: Provider::DeepSeek,
        model: "deepseek-chat",
        retires_on: "2026-07-24",
        replacement: "deepseek-v4-flash",
        advisory_url: "https://api-docs.deepseek.com/quick_start/pricing",
    },
    DeprecatedModel {
        provider: Provider::DeepSeek,
        model: "deepseek-reasoner",
        retires_on: "2026-07-24",
        replacement: "deepseek-v4-flash",
        advisory_url: "https://api-docs.deepseek.com/quick_start/pricing",
    },
    // xAI Grok — May 2026 retirement wave.
    DeprecatedModel {
        provider: Provider::XAi,
        model: "grok-2",
        retires_on: "2026-05-15",
        replacement: "grok-4",
        advisory_url: "https://docs.x.ai/docs/models",
    },
    DeprecatedModel {
        provider: Provider::XAi,
        model: "grok-2-vision",
        retires_on: "2026-05-15",
        replacement: "grok-4",
        advisory_url: "https://docs.x.ai/docs/models",
    },
    DeprecatedModel {
        provider: Provider::XAi,
        model: "grok-beta",
        retires_on: "2026-05-15",
        replacement: "grok-4",
        advisory_url: "https://docs.x.ai/docs/models",
    },
];

/// Look up `(provider, model)` against the deprecation table.
///
/// `model` is matched against the post-`normalize_model` form to avoid
/// false-negatives from vendor-prefixed names (`xai/grok-2` vs `grok-2`).
pub fn lookup(provider: Provider, model: &str) -> Option<&'static DeprecatedModel> {
    let normalized = provider.normalize_model(model);
    DEPRECATIONS
        .iter()
        .find(|d| d.provider == provider && d.model == normalized)
}

/// Iterate all known deprecations. Public so the `borg doctor` step can list
/// them without re-implementing the table walk.
pub fn all() -> &'static [DeprecatedModel] {
    DEPRECATIONS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xai_grok_2_is_deprecated() {
        let hit = lookup(Provider::XAi, "grok-2").expect("grok-2 should be flagged");
        assert_eq!(hit.replacement, "grok-4");
        assert_eq!(hit.retires_on, "2026-05-15");
        assert!(hit.advisory_url.starts_with("https://"));
    }

    #[test]
    fn xai_grok_2_with_vendor_prefix_still_matches() {
        // Operators routinely paste OpenRouter-style `xai/grok-2`; the
        // `normalize_model` call inside `lookup` must strip the prefix
        // before comparing against the table.
        assert!(lookup(Provider::XAi, "xai/grok-2").is_some());
        assert!(lookup(Provider::XAi, "x-ai/grok-2").is_some());
    }

    #[test]
    fn current_grok_model_is_not_deprecated() {
        assert!(lookup(Provider::XAi, "grok-4").is_none());
    }

    #[test]
    fn other_providers_unaffected_by_xai_table() {
        // A model named `grok-2` configured against e.g. Anthropic should
        // never trip the xAI deprecation rules — the table is keyed on
        // both provider and model.
        assert!(lookup(Provider::Anthropic, "grok-2").is_none());
    }

    #[test]
    fn deprecation_entries_carry_real_replacement_and_url() {
        // Schema regression: a future contributor shouldn't be able to land
        // an empty replacement or a bare-domain URL.
        for entry in all() {
            assert!(
                !entry.replacement.is_empty(),
                "{:?} missing replacement",
                entry.model
            );
            assert!(
                entry.advisory_url.starts_with("https://"),
                "{:?} advisory_url must be https",
                entry.model
            );
            // ISO-8601 yyyy-mm-dd shape — 10 chars including dashes.
            assert_eq!(
                entry.retires_on.len(),
                10,
                "{:?} retires_on must be ISO-8601 yyyy-mm-dd",
                entry.model
            );
        }
    }
}
