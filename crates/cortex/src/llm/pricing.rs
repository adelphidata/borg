//! Static provider pricing used for daemon cost-event emission.

use std::collections::HashMap;
use std::sync::LazyLock;

use crate::budgets::Usd;
use crate::config::CacheTtl;
use crate::llm::provider::Provider;

struct ModelRate {
    input_per_mtok: &'static str,
    output_per_mtok: &'static str,
}

struct AnthropicModelRate {
    input_per_mtok: &'static str,
    cache_read_per_mtok: &'static str,
    cache_write_5m_per_mtok: &'static str,
    cache_write_1h_per_mtok: &'static str,
    output_per_mtok: &'static str,
}

// Sources captured on 2026-05-02:
// OpenAI official pricing/model docs:
// - https://openai.com/api/pricing/
// - https://developers.openai.com/api/docs/models/gpt-4
// - https://developers.openai.com/api/docs/models/gpt-4.1
// - https://developers.openai.com/api/docs/models/gpt-4.1-mini
// - https://developers.openai.com/api/docs/models/gpt-4.1-nano
// - https://developers.openai.com/api/docs/models/gpt-4o
// - https://developers.openai.com/api/docs/models/gpt-4o-mini
// - https://developers.openai.com/api/docs/models/gpt-5
// - https://developers.openai.com/api/docs/models/gpt-5-mini
// - https://developers.openai.com/api/docs/models/o3
// - https://developers.openai.com/api/docs/models/o4-mini
// Anthropic official pricing/model docs:
// - https://docs.anthropic.com/en/docs/about-claude/pricing
// - https://docs.anthropic.com/en/docs/build-with-claude/prompt-caching
// - https://docs.anthropic.com/en/docs/models-overview
// - https://www.anthropic.com/claude/haiku?pubDate=20250827

static OPENAI_RATES: LazyLock<HashMap<&'static str, ModelRate>> = LazyLock::new(|| {
    HashMap::from([
        (
            "gpt-4",
            ModelRate {
                input_per_mtok: "30.00",
                output_per_mtok: "60.00",
            },
        ),
        (
            "gpt-4.1",
            ModelRate {
                input_per_mtok: "2.00",
                output_per_mtok: "8.00",
            },
        ),
        (
            "gpt-4.1-mini",
            ModelRate {
                input_per_mtok: "0.40",
                output_per_mtok: "1.60",
            },
        ),
        (
            "gpt-4.1-nano",
            ModelRate {
                input_per_mtok: "0.10",
                output_per_mtok: "0.40",
            },
        ),
        (
            "gpt-4o",
            ModelRate {
                input_per_mtok: "2.50",
                output_per_mtok: "10.00",
            },
        ),
        (
            "gpt-4o-2024-08-06",
            ModelRate {
                input_per_mtok: "2.50",
                output_per_mtok: "10.00",
            },
        ),
        (
            "gpt-4o-mini",
            ModelRate {
                input_per_mtok: "0.15",
                output_per_mtok: "0.60",
            },
        ),
        (
            "gpt-5",
            ModelRate {
                input_per_mtok: "1.25",
                output_per_mtok: "10.00",
            },
        ),
        (
            "gpt-5-mini",
            ModelRate {
                input_per_mtok: "0.25",
                output_per_mtok: "2.00",
            },
        ),
        (
            "gpt-5-nano",
            ModelRate {
                input_per_mtok: "0.05",
                output_per_mtok: "0.40",
            },
        ),
        (
            "o3",
            ModelRate {
                input_per_mtok: "2.00",
                output_per_mtok: "8.00",
            },
        ),
        (
            "o4-mini",
            ModelRate {
                input_per_mtok: "1.10",
                output_per_mtok: "4.40",
            },
        ),
    ])
});

// xAI Grok pricing per xAI's public docs (https://docs.x.ai/docs/models).
// Captured 2026-05-24. xAI prices in USD per million tokens; cache-read is
// charged separately on some models but the existing OpenAI-compat
// cost-estimation path ignores cache tokens, so only input/output land here.
// Update when xAI revises pricing.
static XAI_RATES: LazyLock<HashMap<&'static str, ModelRate>> = LazyLock::new(|| {
    HashMap::from([
        (
            "grok-4",
            ModelRate {
                input_per_mtok: "5.00",
                output_per_mtok: "15.00",
            },
        ),
        (
            "grok-4-fast",
            ModelRate {
                input_per_mtok: "0.20",
                output_per_mtok: "0.50",
            },
        ),
        (
            "grok-4-fast-reasoning",
            ModelRate {
                input_per_mtok: "0.20",
                output_per_mtok: "0.50",
            },
        ),
        (
            "grok-4-fast-non-reasoning",
            ModelRate {
                input_per_mtok: "0.20",
                output_per_mtok: "0.50",
            },
        ),
        (
            "grok-code-fast-1",
            ModelRate {
                input_per_mtok: "0.20",
                output_per_mtok: "1.50",
            },
        ),
        (
            "grok-code-fast",
            ModelRate {
                input_per_mtok: "0.20",
                output_per_mtok: "1.50",
            },
        ),
        (
            "grok-3",
            ModelRate {
                input_per_mtok: "3.00",
                output_per_mtok: "15.00",
            },
        ),
        (
            "grok-3-mini",
            ModelRate {
                input_per_mtok: "0.30",
                output_per_mtok: "0.50",
            },
        ),
    ])
});

static GEMINI_RATES: LazyLock<HashMap<&'static str, ModelRate>> = LazyLock::new(|| {
    HashMap::from([
        (
            "gemini-2.5-pro",
            ModelRate {
                input_per_mtok: "1.25",
                output_per_mtok: "10.00",
            },
        ),
        (
            "gemini-2.5-flash",
            ModelRate {
                input_per_mtok: "0.30",
                output_per_mtok: "2.50",
            },
        ),
        (
            "gemini-2.5-flash-lite",
            ModelRate {
                input_per_mtok: "0.10",
                output_per_mtok: "0.40",
            },
        ),
        (
            "gemini-2.0-flash",
            ModelRate {
                input_per_mtok: "0.10",
                output_per_mtok: "0.40",
            },
        ),
    ])
});

static ANTHROPIC_RATES: LazyLock<HashMap<&'static str, AnthropicModelRate>> = LazyLock::new(|| {
    HashMap::from([
        (
            "claude-opus-4",
            AnthropicModelRate {
                input_per_mtok: "15.00",
                cache_read_per_mtok: "1.50",
                cache_write_5m_per_mtok: "18.75",
                cache_write_1h_per_mtok: "30.00",
                output_per_mtok: "75.00",
            },
        ),
        (
            "claude-opus-4-20250514",
            AnthropicModelRate {
                input_per_mtok: "15.00",
                cache_read_per_mtok: "1.50",
                cache_write_5m_per_mtok: "18.75",
                cache_write_1h_per_mtok: "30.00",
                output_per_mtok: "75.00",
            },
        ),
        (
            "claude-opus-4-7",
            AnthropicModelRate {
                input_per_mtok: "15.00",
                cache_read_per_mtok: "1.50",
                cache_write_5m_per_mtok: "18.75",
                cache_write_1h_per_mtok: "30.00",
                output_per_mtok: "75.00",
            },
        ),
        (
            "claude-sonnet-4",
            AnthropicModelRate {
                input_per_mtok: "3.00",
                cache_read_per_mtok: "0.30",
                cache_write_5m_per_mtok: "3.75",
                cache_write_1h_per_mtok: "6.00",
                output_per_mtok: "15.00",
            },
        ),
        (
            "claude-sonnet-4-20250514",
            AnthropicModelRate {
                input_per_mtok: "3.00",
                cache_read_per_mtok: "0.30",
                cache_write_5m_per_mtok: "3.75",
                cache_write_1h_per_mtok: "6.00",
                output_per_mtok: "15.00",
            },
        ),
        (
            "claude-sonnet-4-6",
            AnthropicModelRate {
                input_per_mtok: "3.00",
                cache_read_per_mtok: "0.30",
                cache_write_5m_per_mtok: "3.75",
                cache_write_1h_per_mtok: "6.00",
                output_per_mtok: "15.00",
            },
        ),
        (
            "claude-haiku-4-5",
            AnthropicModelRate {
                input_per_mtok: "1.00",
                cache_read_per_mtok: "0.10",
                cache_write_5m_per_mtok: "1.25",
                cache_write_1h_per_mtok: "2.00",
                output_per_mtok: "5.00",
            },
        ),
        (
            "claude-haiku-4-5-20251001",
            AnthropicModelRate {
                input_per_mtok: "1.00",
                cache_read_per_mtok: "0.10",
                cache_write_5m_per_mtok: "1.25",
                cache_write_1h_per_mtok: "2.00",
                output_per_mtok: "5.00",
            },
        ),
    ])
});

fn normalize_model_id(model: &str) -> &str {
    model
        .strip_prefix("openai/")
        .or_else(|| model.strip_prefix("anthropic/"))
        .or_else(|| model.strip_prefix("google/"))
        .or_else(|| model.strip_prefix("gemini/"))
        .or_else(|| model.strip_prefix("xai/"))
        .or_else(|| model.strip_prefix("x-ai/"))
        .or_else(|| model.strip_prefix("x.ai/"))
        .or_else(|| model.strip_prefix("grok/"))
        .unwrap_or(model)
}

fn rates_for(provider: Provider) -> Option<&'static HashMap<&'static str, ModelRate>> {
    match provider {
        Provider::OpenAi => Some(&OPENAI_RATES),
        Provider::Gemini => Some(&GEMINI_RATES),
        Provider::XAi => Some(&XAI_RATES),
        _ => None,
    }
}

fn anthropic_rates_for(model: &str) -> Option<&'static AnthropicModelRate> {
    ANTHROPIC_RATES.get(normalize_model_id(model))
}

/// Return the exact USD string for a direct OpenAI model.
pub fn estimate_cost_usd(
    provider: Provider,
    model: &str,
    prompt_tokens: u64,
    completion_tokens: u64,
) -> Option<String> {
    let rates = rates_for(provider)?;
    let rate = rates.get(normalize_model_id(model))?;
    let input = Usd::from_tokens_at_rate(prompt_tokens, Usd::parse(rate.input_per_mtok).ok()?);
    let output =
        Usd::from_tokens_at_rate(completion_tokens, Usd::parse(rate.output_per_mtok).ok()?);
    Some((input + output).to_storage_string())
}

/// Return the exact USD string for a direct Anthropic model, including
/// prompt-cache reads/writes when present.
pub fn estimate_anthropic_cost_usd(
    model: &str,
    prompt_tokens: u64,
    completion_tokens: u64,
    cached_input_tokens: u64,
    cache_creation_tokens: u64,
    cache_ttl: CacheTtl,
) -> Option<String> {
    let rate = anthropic_rates_for(model)?;
    let uncached_input =
        Usd::from_tokens_at_rate(prompt_tokens, Usd::parse(rate.input_per_mtok).ok()?);
    let cached_read = Usd::from_tokens_at_rate(
        cached_input_tokens,
        Usd::parse(rate.cache_read_per_mtok).ok()?,
    );
    let cache_write_rate = match cache_ttl {
        CacheTtl::OneHour => rate.cache_write_1h_per_mtok,
        CacheTtl::Auto | CacheTtl::FiveMin => rate.cache_write_5m_per_mtok,
    };
    let cache_write =
        Usd::from_tokens_at_rate(cache_creation_tokens, Usd::parse(cache_write_rate).ok()?);
    let output =
        Usd::from_tokens_at_rate(completion_tokens, Usd::parse(rate.output_per_mtok).ok()?);
    Some((uncached_input + cached_read + cache_write + output).to_storage_string())
}

/// True when the direct-provider table contains the given model id.
#[cfg(test)]
pub fn has_model_rate(provider: Provider, model: &str) -> bool {
    match provider {
        Provider::OpenAi => OPENAI_RATES.contains_key(normalize_model_id(model)),
        Provider::Anthropic => ANTHROPIC_RATES.contains_key(normalize_model_id(model)),
        Provider::Gemini => GEMINI_RATES.contains_key(normalize_model_id(model)),
        Provider::XAi => XAI_RATES.contains_key(normalize_model_id(model)),
        _ => false,
    }
}

/// Shipped xAI direct models that must never silently fall out of the
/// daemon cost-emission table.
#[cfg(test)]
pub fn shipped_xai_models() -> &'static [&'static str] {
    &[
        "grok-4",
        "grok-4-fast",
        "grok-4-fast-reasoning",
        "grok-4-fast-non-reasoning",
        "grok-code-fast",
        "grok-code-fast-1",
        "grok-3",
        "grok-3-mini",
    ]
}

/// Shipped OpenAI direct models that must never silently fall out of the
/// daemon cost-emission table.
#[cfg(test)]
pub fn shipped_openai_models() -> &'static [&'static str] {
    &["gpt-4", "gpt-4.1", "gpt-4.1-mini"]
}

/// Shipped Anthropic direct models that must never silently fall out of the
/// daemon cost-emission table.
#[cfg(test)]
pub fn shipped_anthropic_models() -> &'static [&'static str] {
    &["claude-sonnet-4", "claude-haiku-4-5"]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openai_cost_is_deterministic() {
        let cost = estimate_cost_usd(Provider::OpenAi, "gpt-4.1", 1_000, 500).unwrap();
        assert_eq!(cost, "0.006");
    }

    #[test]
    fn anthropic_cost_is_deterministic() {
        let cost =
            estimate_anthropic_cost_usd("claude-sonnet-4", 1_000, 500, 0, 0, CacheTtl::FiveMin)
                .unwrap();
        assert_eq!(cost, "0.0105");
    }

    #[test]
    fn anthropic_cost_accounts_for_prompt_cache_reads_and_writes() {
        let cost =
            estimate_anthropic_cost_usd("claude-sonnet-4", 1_000, 500, 800, 400, CacheTtl::FiveMin)
                .unwrap();
        assert_eq!(cost, "0.01224");
    }

    #[test]
    fn anthropic_one_hour_cache_write_uses_one_hour_rate() {
        let cost =
            estimate_anthropic_cost_usd("claude-sonnet-4", 1_000, 500, 0, 1_000, CacheTtl::OneHour)
                .unwrap();
        assert_eq!(cost, "0.0165");
    }

    #[test]
    fn unknown_model_returns_none() {
        assert!(estimate_cost_usd(Provider::OpenAi, "missing", 1_000, 500).is_none());
    }

    #[test]
    fn shipped_openai_models_have_rates() {
        for model in shipped_openai_models() {
            assert!(
                has_model_rate(Provider::OpenAi, model),
                "missing OpenAI rate for {model}"
            );
        }
    }

    #[test]
    fn shipped_anthropic_models_have_rates() {
        for model in shipped_anthropic_models() {
            assert!(
                has_model_rate(Provider::Anthropic, model),
                "missing Anthropic rate for {model}"
            );
        }
    }

    #[test]
    fn xai_cost_is_deterministic_for_grok_4() {
        // Hand-computed: grok-4 = $5/Mtok input + $15/Mtok output.
        // 1_000 prompt × 5 / 1e6 = 0.005; 500 completion × 15 / 1e6 = 0.0075.
        // Total = 0.0125. If the rate table is corrupted or the lookup
        // regresses to None, this asserts the exact dollar figure rather than
        // .is_some() — a tautological check would pass for the wrong rate.
        let cost = estimate_cost_usd(Provider::XAi, "grok-4", 1_000, 500).unwrap();
        assert_eq!(cost, "0.0125");
    }

    #[test]
    fn xai_cost_for_grok_code_fast_uses_published_rates() {
        // grok-code-fast = $0.20 input / $1.50 output per Mtok.
        // 10_000 prompt × 0.20 / 1e6 = 0.002; 2_000 completion × 1.50 / 1e6 = 0.003.
        let cost = estimate_cost_usd(Provider::XAi, "grok-code-fast", 10_000, 2_000).unwrap();
        assert_eq!(cost, "0.005");
        // The dated `-1` alias is the same family — must yield the same number.
        let cost_alias =
            estimate_cost_usd(Provider::XAi, "grok-code-fast-1", 10_000, 2_000).unwrap();
        assert_eq!(cost_alias, cost);
    }

    #[test]
    fn xai_cost_normalizes_every_vendor_prefix() {
        // A user who pulls a slug from xAI docs (`xai/`), OpenRouter (`x-ai/`),
        // hermes-agent (`x.ai/`), or just types the product name (`grok/`)
        // must hit the same rate. If any prefix path silently bypasses the
        // table, cost-event attribution regresses to None for that variant.
        let base = estimate_cost_usd(Provider::XAi, "grok-4-fast", 1_000_000, 0).unwrap();
        for variant in [
            "xai/grok-4-fast",
            "x-ai/grok-4-fast",
            "x.ai/grok-4-fast",
            "grok/grok-4-fast",
        ] {
            let got = estimate_cost_usd(Provider::XAi, variant, 1_000_000, 0).unwrap();
            assert_eq!(got, base, "prefix {variant} produced a different cost");
        }
    }

    #[test]
    fn shipped_xai_models_have_rates() {
        for model in shipped_xai_models() {
            assert!(
                has_model_rate(Provider::XAi, model),
                "missing xAI rate for {model}"
            );
        }
    }

    #[test]
    fn xai_unknown_model_returns_none() {
        // A model that's not in the table must not silently fall back to a
        // wrong rate. Returning None signals the daemon to skip cost
        // emission rather than emit incorrect dollars.
        assert!(estimate_cost_usd(Provider::XAi, "grok-vapor-99", 100, 100).is_none());
    }
}
