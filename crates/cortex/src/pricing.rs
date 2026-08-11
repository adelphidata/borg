// Pricing data last updated: 2026-04-19
// Source: provider pricing pages. Update entries when prices change.

use std::collections::HashMap;
use std::sync::LazyLock;

struct ModelPricing {
    input_per_million: f64,
    output_per_million: f64,
}

/// Normalize a model identifier by stripping the `provider/` prefix so that
/// both `"anthropic/claude-sonnet-4"` and `"claude-sonnet-4"` resolve to the
/// same pricing entry. Also reduces AWS Bedrock model ids to their underlying
/// Anthropic key so `anthropic.claude-sonnet-4-20250514-v1:0` prices the same
/// as `claude-sonnet-4-20250514`.
fn normalize_model(model: &str) -> &str {
    // Known provider prefixes used by OpenRouter
    const PREFIXES: &[&str] = &[
        "anthropic/",
        "openai/",
        "google/",
        "deepseek/",
        "meta-llama/",
        "mistralai/",
    ];
    for prefix in PREFIXES {
        if let Some(rest) = model.strip_prefix(prefix) {
            return rest;
        }
    }
    strip_bedrock_model_shape(model)
}

/// Reduce an AWS Bedrock model id to the underlying vendor model key by
/// stripping the cross-region inference geo prefix (`us.`, `eu.`, `apac.`,
/// `us-gov.`), the `anthropic.` vendor segment, and the trailing `-vN:M`
/// Bedrock version tag. Ids that are not Bedrock-shaped are returned unchanged,
/// so this is a safe no-op on native Anthropic / OpenAI / Gemini ids.
pub(crate) fn strip_bedrock_model_shape(model: &str) -> &str {
    let geo_stripped = model
        .strip_prefix("us-gov.")
        .or_else(|| model.strip_prefix("us."))
        .or_else(|| model.strip_prefix("eu."))
        .or_else(|| model.strip_prefix("apac."));
    // Only commit to the geo strip when the anthropic vendor segment follows,
    // so a stray `us.`-prefixed non-Bedrock id is never mangled.
    match geo_stripped.unwrap_or(model).strip_prefix("anthropic.") {
        Some(rest) => strip_bedrock_version_suffix(rest),
        None => model,
    }
}

/// Drop a trailing `-v<major>:<minor>` Bedrock version tag (e.g. `-v1:0`),
/// leaving other `-v…` fragments intact.
fn strip_bedrock_version_suffix(model: &str) -> &str {
    let Some(idx) = model.rfind("-v") else {
        return model;
    };
    let Some((major, minor)) = model[idx + 2..].split_once(':') else {
        return model;
    };
    if !major.is_empty()
        && !minor.is_empty()
        && major.bytes().all(|b| b.is_ascii_digit())
        && minor.bytes().all(|b| b.is_ascii_digit())
    {
        &model[..idx]
    } else {
        model
    }
}

static PRICING: LazyLock<HashMap<&'static str, ModelPricing>> = LazyLock::new(|| {
    let entries: Vec<(&str, f64, f64)> = vec![
        // Claude 5 (frontier)
        ("claude-opus-5", 5.0, 25.0),
        ("claude-sonnet-5", 3.0, 15.0),
        ("claude-fable-5", 10.0, 50.0),
        // Claude 4.x. Opus dropped from $15/$75 to $5/$25 at 4.6; 4.7 was
        // carrying the retired rate and over-reporting cost by 3x.
        ("claude-opus-4-8", 5.0, 25.0),
        ("claude-opus-4-7", 5.0, 25.0),
        ("claude-opus-4-20250514", 15.0, 75.0),
        ("claude-opus-4", 15.0, 75.0),
        ("claude-sonnet-4-6", 3.0, 15.0),
        ("claude-sonnet-4-20250514", 3.0, 15.0),
        ("claude-sonnet-4-0", 3.0, 15.0),
        ("claude-sonnet-4", 3.0, 15.0),
        ("claude-haiku-4-5-20251001", 1.0, 5.0),
        ("claude-haiku-4-5", 1.0, 5.0),
        // Claude 3.x (available on AWS Bedrock incl. GovCloud, mapped via the
        // `anthropic.*` Bedrock ids to these Anthropic public rates)
        ("claude-3-7-sonnet", 3.0, 15.0),
        ("claude-3-7-sonnet-20250219", 3.0, 15.0),
        ("claude-3-5-sonnet", 3.0, 15.0),
        ("claude-3-5-sonnet-20241022", 3.0, 15.0),
        ("claude-3-5-sonnet-20240620", 3.0, 15.0),
        ("claude-3-5-haiku", 0.80, 4.0),
        ("claude-3-5-haiku-20241022", 0.80, 4.0),
        ("claude-3-opus", 15.0, 75.0),
        ("claude-3-opus-20240229", 15.0, 75.0),
        // xAI. Same rates the direct-provider table in `llm/pricing.rs` already
        // carries — this table missed them, so Grok spend never reached the
        // budget at all.
        ("grok-4", 5.0, 15.0),
        ("grok-4-fast-reasoning", 0.20, 0.50),
        ("grok-4-fast-non-reasoning", 0.20, 0.50),
        ("grok-4-fast", 0.20, 0.50),
        ("grok-3", 3.0, 15.0),
        ("grok-3-mini", 0.30, 0.50),
        // GPT-5 family
        ("gpt-5", 1.25, 10.0),
        ("gpt-5-mini", 0.25, 2.0),
        ("gpt-5-nano", 0.05, 0.40),
        // GPT-4o
        ("gpt-4o", 2.50, 10.0),
        ("gpt-4o-2024-08-06", 2.50, 10.0),
        ("gpt-4o-mini", 0.15, 0.60),
        // GPT-4.1
        ("gpt-4.1", 2.0, 8.0),
        ("gpt-4.1-mini", 0.40, 1.60),
        ("gpt-4.1-nano", 0.10, 0.40),
        // o3/o4-mini
        ("o3", 10.0, 40.0),
        ("o4-mini", 1.10, 4.40),
        // Gemini 3.x (frontier) + 2.5 legacy
        ("gemini-3-pro", 1.25, 10.0),
        ("gemini-3-flash", 0.15, 0.60),
        ("gemini-3-flash-lite", 0.05, 0.30),
        ("gemini-2.5-pro", 1.25, 10.0),
        ("gemini-2.5-flash", 0.30, 2.50),
        ("gemini-2.0-flash", 0.10, 0.40),
        // DeepSeek — direct-API rates, 2026-07 snapshot of
        // https://api-docs.deepseek.com/quick_start/pricing (cache-miss input).
        // `deepseek-chat` / `deepseek-reasoner` retire 2026-07-24 and already
        // alias V4 Flash's non-thinking / thinking modes, so they price as it.
        ("deepseek-v4-flash", 0.14, 0.28),
        ("deepseek-v4-pro", 0.435, 0.87),
        ("deepseek-chat", 0.14, 0.28),
        ("deepseek-reasoner", 0.14, 0.28),
        ("deepseek-v3.2", 0.27, 1.10),
        ("deepseek-chat-v3", 0.27, 1.10),
        ("deepseek-r1", 0.55, 2.19),
    ];

    let mut m = HashMap::with_capacity(entries.len());
    for (name, input, output) in entries {
        m.insert(
            name,
            ModelPricing {
                input_per_million: input,
                output_per_million: output,
            },
        );
    }
    m
});

/// One bundled rate-card row for the schema seed.
///
/// Decimal strings (not floats) so the seeded values round-trip through
/// `provider_rate_cards.{prompt,completion}_per_mtok_usd` without any drift,
/// preserving replay determinism for budget computations.
pub struct BundledRateCardEntry {
    /// Provider identifier (matches `Provider::id()`).
    pub provider_id: &'static str,
    /// Model identifier as the provider expects it.
    pub model: &'static str,
    /// Prompt token cost per million tokens, decimal string (e.g. "3.00").
    pub prompt_per_mtok_usd: &'static str,
    /// Completion token cost per million tokens, decimal string.
    pub completion_per_mtok_usd: &'static str,
}

/// Bundled rate card seeded by the schema. One row per
/// `(provider_id, model)` pair we have pricing for. Anthropic, OpenAI,
/// Gemini, and DeepSeek native; OpenRouter intentionally excluded (its
/// per-route surcharge is not encoded here) — users add an OpenRouter
/// rate card via `borg providers add` if they want it priced.
pub fn bundled_rate_card_entries() -> &'static [BundledRateCardEntry] {
    &[
        // Anthropic
        BundledRateCardEntry {
            provider_id: "anthropic",
            model: "claude-opus-5",
            prompt_per_mtok_usd: "5.00",
            completion_per_mtok_usd: "25.00",
        },
        BundledRateCardEntry {
            provider_id: "anthropic",
            model: "claude-sonnet-5",
            prompt_per_mtok_usd: "3.00",
            completion_per_mtok_usd: "15.00",
        },
        BundledRateCardEntry {
            provider_id: "anthropic",
            model: "claude-fable-5",
            prompt_per_mtok_usd: "10.00",
            completion_per_mtok_usd: "50.00",
        },
        BundledRateCardEntry {
            provider_id: "anthropic",
            model: "claude-opus-4-8",
            prompt_per_mtok_usd: "5.00",
            completion_per_mtok_usd: "25.00",
        },
        BundledRateCardEntry {
            provider_id: "anthropic",
            model: "claude-opus-4-7",
            prompt_per_mtok_usd: "5.00",
            completion_per_mtok_usd: "25.00",
        },
        BundledRateCardEntry {
            provider_id: "anthropic",
            model: "claude-opus-4",
            prompt_per_mtok_usd: "15.00",
            completion_per_mtok_usd: "75.00",
        },
        BundledRateCardEntry {
            provider_id: "anthropic",
            model: "claude-sonnet-4-6",
            prompt_per_mtok_usd: "3.00",
            completion_per_mtok_usd: "15.00",
        },
        BundledRateCardEntry {
            provider_id: "anthropic",
            model: "claude-sonnet-4",
            prompt_per_mtok_usd: "3.00",
            completion_per_mtok_usd: "15.00",
        },
        BundledRateCardEntry {
            provider_id: "anthropic",
            model: "claude-haiku-4-5",
            prompt_per_mtok_usd: "1.00",
            completion_per_mtok_usd: "5.00",
        },
        // OpenAI
        BundledRateCardEntry {
            provider_id: "openai",
            model: "gpt-5",
            prompt_per_mtok_usd: "1.25",
            completion_per_mtok_usd: "10.00",
        },
        BundledRateCardEntry {
            provider_id: "openai",
            model: "gpt-5-mini",
            prompt_per_mtok_usd: "0.25",
            completion_per_mtok_usd: "2.00",
        },
        BundledRateCardEntry {
            provider_id: "openai",
            model: "gpt-5-nano",
            prompt_per_mtok_usd: "0.05",
            completion_per_mtok_usd: "0.40",
        },
        BundledRateCardEntry {
            provider_id: "openai",
            model: "gpt-4o",
            prompt_per_mtok_usd: "2.50",
            completion_per_mtok_usd: "10.00",
        },
        BundledRateCardEntry {
            provider_id: "openai",
            model: "gpt-4o-mini",
            prompt_per_mtok_usd: "0.15",
            completion_per_mtok_usd: "0.60",
        },
        BundledRateCardEntry {
            provider_id: "openai",
            model: "gpt-4.1",
            prompt_per_mtok_usd: "2.00",
            completion_per_mtok_usd: "8.00",
        },
        BundledRateCardEntry {
            provider_id: "openai",
            model: "gpt-4.1-mini",
            prompt_per_mtok_usd: "0.40",
            completion_per_mtok_usd: "1.60",
        },
        BundledRateCardEntry {
            provider_id: "openai",
            model: "gpt-4.1-nano",
            prompt_per_mtok_usd: "0.10",
            completion_per_mtok_usd: "0.40",
        },
        BundledRateCardEntry {
            provider_id: "openai",
            model: "o3",
            prompt_per_mtok_usd: "10.00",
            completion_per_mtok_usd: "40.00",
        },
        BundledRateCardEntry {
            provider_id: "openai",
            model: "o4-mini",
            prompt_per_mtok_usd: "1.10",
            completion_per_mtok_usd: "4.40",
        },
        // Gemini
        BundledRateCardEntry {
            provider_id: "gemini",
            model: "gemini-3-pro",
            prompt_per_mtok_usd: "1.25",
            completion_per_mtok_usd: "10.00",
        },
        BundledRateCardEntry {
            provider_id: "gemini",
            model: "gemini-3-flash",
            prompt_per_mtok_usd: "0.15",
            completion_per_mtok_usd: "0.60",
        },
        BundledRateCardEntry {
            provider_id: "gemini",
            model: "gemini-3-flash-lite",
            prompt_per_mtok_usd: "0.05",
            completion_per_mtok_usd: "0.30",
        },
        BundledRateCardEntry {
            provider_id: "gemini",
            model: "gemini-2.5-pro",
            prompt_per_mtok_usd: "1.25",
            completion_per_mtok_usd: "10.00",
        },
        BundledRateCardEntry {
            provider_id: "gemini",
            model: "gemini-2.5-flash",
            prompt_per_mtok_usd: "0.30",
            completion_per_mtok_usd: "2.50",
        },
        // DeepSeek — 2026-07 direct-API snapshot. The legacy `deepseek-chat` /
        // `deepseek-reasoner` ids alias V4 Flash until they retire 2026-07-24,
        // so all three carry its rate.
        BundledRateCardEntry {
            provider_id: "deepseek",
            model: "deepseek-v4-flash",
            prompt_per_mtok_usd: "0.14",
            completion_per_mtok_usd: "0.28",
        },
        BundledRateCardEntry {
            provider_id: "deepseek",
            model: "deepseek-v4-pro",
            prompt_per_mtok_usd: "0.435",
            completion_per_mtok_usd: "0.87",
        },
        BundledRateCardEntry {
            provider_id: "deepseek",
            model: "deepseek-chat",
            prompt_per_mtok_usd: "0.14",
            completion_per_mtok_usd: "0.28",
        },
        BundledRateCardEntry {
            provider_id: "deepseek",
            model: "deepseek-reasoner",
            prompt_per_mtok_usd: "0.14",
            completion_per_mtok_usd: "0.28",
        },
        // Local providers — zero rate card so the budget pipeline records $0
        // uniformly without special-casing (per task spec).
        BundledRateCardEntry {
            provider_id: "ollama",
            model: "*",
            prompt_per_mtok_usd: "0",
            completion_per_mtok_usd: "0",
        },
        BundledRateCardEntry {
            provider_id: "claude_cli",
            model: "*",
            prompt_per_mtok_usd: "0",
            completion_per_mtok_usd: "0",
        },
    ]
}

/// Estimate cost in USD for the given model and token counts.
/// Returns `None` if the model is not in the pricing table.
pub fn estimate_cost(model: &str, prompt_tokens: u64, completion_tokens: u64) -> Option<f64> {
    let key = normalize_model(model);
    let pricing = PRICING.get(key)?;
    let cost = (prompt_tokens as f64 * pricing.input_per_million
        + completion_tokens as f64 * pricing.output_per_million)
        / 1_000_000.0;
    Some(cost)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Rates from the 2026-07 direct-API snapshot. Stale rates here don't fail
    /// loudly — they silently mis-bill every turn and skew the budget guard, so
    /// pin the arithmetic.
    #[test]
    fn deepseek_v4_costs_match_the_published_rates() {
        // 1M input × $0.14/M + 500K output × $0.28/M = 0.14 + 0.14 = 0.28
        let flash = estimate_cost("deepseek-v4-flash", 1_000_000, 500_000).unwrap();
        assert!((flash - 0.28).abs() < 1e-9, "got {flash}");

        // 1M input × $0.435/M + 500K output × $0.87/M = 0.435 + 0.435 = 0.87
        let pro = estimate_cost("deepseek-v4-pro", 1_000_000, 500_000).unwrap();
        assert!((pro - 0.87).abs() < 1e-9, "got {pro}");

        assert!(
            pro > flash,
            "the Pro tier must never price below Flash: {pro} vs {flash}"
        );
    }

    /// Invariant: `deepseek-chat` / `deepseek-reasoner` are aliases of V4
    /// Flash's non-thinking / thinking modes until they retire on 2026-07-24,
    /// so they must bill at exactly its rate — not the retired V3-era rates.
    #[test]
    fn deepseek_legacy_aliases_price_as_v4_flash() {
        let flash = estimate_cost("deepseek-v4-flash", 1_000_000, 500_000).unwrap();
        for alias in ["deepseek-chat", "deepseek-reasoner"] {
            let aliased = estimate_cost(alias, 1_000_000, 500_000)
                .unwrap_or_else(|| panic!("{alias} must stay priced while it still resolves"));
            assert!(
                (aliased - flash).abs() < 1e-9,
                "{alias} must bill as V4 Flash: {aliased} vs {flash}"
            );
        }
    }

    #[test]
    fn known_model_returns_cost() {
        let cost = estimate_cost("claude-sonnet-4", 1000, 500);
        assert!(cost.is_some());
        let c = cost.unwrap();
        // 1000 * 3.0 / 1M + 500 * 15.0 / 1M = 0.003 + 0.0075 = 0.0105
        assert!((c - 0.0105).abs() < 1e-9);
    }

    #[test]
    fn openrouter_prefixed_model_returns_cost() {
        let cost = estimate_cost("anthropic/claude-sonnet-4", 1_000_000, 0);
        assert!(cost.is_some());
        assert!((cost.unwrap() - 3.0).abs() < 1e-9);
    }

    #[test]
    fn unknown_model_returns_none() {
        assert!(estimate_cost("unknown-model-xyz", 100, 50).is_none());
    }

    #[test]
    fn zero_tokens_returns_zero_cost() {
        let cost = estimate_cost("claude-sonnet-4", 0, 0);
        assert_eq!(cost, Some(0.0));
    }

    #[test]
    fn normalize_strips_provider_prefix() {
        assert_eq!(normalize_model("openai/gpt-4o"), "gpt-4o");
        assert_eq!(normalize_model("google/gemini-2.5-pro"), "gemini-2.5-pro");
        assert_eq!(normalize_model("gpt-4o"), "gpt-4o");
    }

    #[test]
    fn deepseek_via_openrouter_resolves() {
        // 1000 * 0.55/1M + 1000 * 2.19/1M = 0.00055 + 0.00219 = 0.00274
        let c = estimate_cost("deepseek/deepseek-r1", 1000, 1000).expect("known model");
        assert!((c - 0.00274).abs() < 1e-9);
    }

    #[test]
    fn bedrock_anthropic_model_prices_like_direct() {
        // `anthropic.claude-sonnet-4-20250514-v1:0` folds to `claude-sonnet-4`
        // (3/15 $/Mtok): 1000*3/1M + 500*15/1M = 0.0105.
        let c = estimate_cost("anthropic.claude-sonnet-4-20250514-v1:0", 1000, 500)
            .expect("bedrock model priced");
        assert!((c - 0.0105).abs() < 1e-9);
    }

    #[test]
    fn bedrock_cross_region_and_version_normalized() {
        // Geo prefix + `-v2:0` version must both be stripped to the 3.5 Sonnet key.
        let c = estimate_cost("us.anthropic.claude-3-5-sonnet-20241022-v2:0", 1_000_000, 0)
            .expect("bedrock model priced");
        assert!((c - 3.0).abs() < 1e-9);
    }

    #[test]
    fn strip_bedrock_shape_leaves_non_bedrock_untouched() {
        assert_eq!(
            strip_bedrock_model_shape("us.anthropic.claude-opus-4-20250514-v1:0"),
            "claude-opus-4-20250514"
        );
        // No anthropic vendor segment → returned verbatim (geo strip not applied).
        assert_eq!(
            strip_bedrock_model_shape("us-west-2-model"),
            "us-west-2-model"
        );
        assert_eq!(
            strip_bedrock_model_shape("claude-sonnet-4"),
            "claude-sonnet-4"
        );
        // Trailing `-v` without a `major:minor` tag is preserved.
        assert_eq!(
            strip_bedrock_model_shape("anthropic.claude-vision"),
            "claude-vision"
        );
    }
}
