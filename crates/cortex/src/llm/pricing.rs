//! Static provider pricing used for daemon cost-event emission.

use std::collections::HashMap;
use std::sync::LazyLock;

use crate::budgets::Usd;
use crate::config::CacheTtl;
use crate::llm::provider::Provider;

struct ModelRate {
    input_per_mtok: &'static str,
    output_per_mtok: &'static str,
    /// Discounted rate for prompt-cache *reads*, when known.
    ///
    /// Unlike Anthropic — whose `input_tokens` excludes cached tokens —
    /// OpenAI-compatible providers report `prompt_tokens` **inclusive** of
    /// cached tokens. So a cached read is currently billed at the full input
    /// rate here, over-stating cost by the provider's cache discount.
    ///
    /// `None` preserves that behavior rather than guessing a rate: this is a
    /// billing path, and a fabricated discount is worse than a known
    /// over-estimate. Populate per model from the provider's published pricing
    /// (see the `update-model-catalog` skill) and the discount applies
    /// automatically — `estimate_cost_usd` already honors it, and
    /// `cached_read_rate_is_honored_when_present` pins the arithmetic.
    cached_input_per_mtok: Option<&'static str>,
}

impl ModelRate {
    /// Rate row with no known cache-read discount: cached prompt tokens are
    /// billed at the full input rate.
    const fn new(input_per_mtok: &'static str, output_per_mtok: &'static str) -> Self {
        Self {
            input_per_mtok,
            output_per_mtok,
            cached_input_per_mtok: None,
        }
    }

    /// Rate row with a published cache-read rate.
    #[cfg_attr(not(test), allow(dead_code))]
    const fn with_cached_input(
        input_per_mtok: &'static str,
        output_per_mtok: &'static str,
        cached_input_per_mtok: &'static str,
    ) -> Self {
        Self {
            input_per_mtok,
            output_per_mtok,
            cached_input_per_mtok: Some(cached_input_per_mtok),
        }
    }
}

struct AnthropicModelRate {
    input_per_mtok: &'static str,
    cache_read_per_mtok: &'static str,
    cache_write_5m_per_mtok: &'static str,
    cache_write_1h_per_mtok: &'static str,
    output_per_mtok: &'static str,
}

impl AnthropicModelRate {
    /// Build a rate row from its five per-Mtok decimal strings, in the order
    /// input, cache-read, cache-write-5m, cache-write-1h, output.
    const fn new(
        input_per_mtok: &'static str,
        cache_read_per_mtok: &'static str,
        cache_write_5m_per_mtok: &'static str,
        cache_write_1h_per_mtok: &'static str,
        output_per_mtok: &'static str,
    ) -> Self {
        Self {
            input_per_mtok,
            cache_read_per_mtok,
            cache_write_5m_per_mtok,
            cache_write_1h_per_mtok,
            output_per_mtok,
        }
    }
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
        ("gpt-4", ModelRate::new("30.00", "60.00")),
        ("gpt-4.1", ModelRate::new("2.00", "8.00")),
        ("gpt-4.1-mini", ModelRate::new("0.40", "1.60")),
        ("gpt-4.1-nano", ModelRate::new("0.10", "0.40")),
        ("gpt-4o", ModelRate::new("2.50", "10.00")),
        ("gpt-4o-2024-08-06", ModelRate::new("2.50", "10.00")),
        ("gpt-4o-mini", ModelRate::new("0.15", "0.60")),
        ("gpt-5", ModelRate::new("1.25", "10.00")),
        ("gpt-5-mini", ModelRate::new("0.25", "2.00")),
        ("gpt-5-nano", ModelRate::new("0.05", "0.40")),
        ("o3", ModelRate::new("2.00", "8.00")),
        ("o4-mini", ModelRate::new("1.10", "4.40")),
    ])
});

// xAI Grok pricing per xAI's public docs (https://docs.x.ai/docs/models).
// Captured 2026-05-24. xAI prices in USD per million tokens; cache-read is
// charged separately on some models but the existing OpenAI-compat
// cost-estimation path ignores cache tokens, so only input/output land here.
// Update when xAI revises pricing.
static XAI_RATES: LazyLock<HashMap<&'static str, ModelRate>> = LazyLock::new(|| {
    HashMap::from([
        ("grok-4", ModelRate::new("5.00", "15.00")),
        ("grok-4-fast", ModelRate::new("0.20", "0.50")),
        ("grok-4-fast-reasoning", ModelRate::new("0.20", "0.50")),
        ("grok-4-fast-non-reasoning", ModelRate::new("0.20", "0.50")),
        ("grok-code-fast-1", ModelRate::new("0.20", "1.50")),
        ("grok-code-fast", ModelRate::new("0.20", "1.50")),
        ("grok-3", ModelRate::new("3.00", "15.00")),
        ("grok-3-mini", ModelRate::new("0.30", "0.50")),
    ])
});

static GEMINI_RATES: LazyLock<HashMap<&'static str, ModelRate>> = LazyLock::new(|| {
    HashMap::from([
        ("gemini-3-pro", ModelRate::new("1.25", "10.00")),
        ("gemini-3-flash", ModelRate::new("0.15", "0.60")),
        ("gemini-3-flash-lite", ModelRate::new("0.05", "0.30")),
        ("gemini-2.5-pro", ModelRate::new("1.25", "10.00")),
        ("gemini-2.5-flash", ModelRate::new("0.30", "2.50")),
        ("gemini-2.5-flash-lite", ModelRate::new("0.10", "0.40")),
        ("gemini-2.0-flash", ModelRate::new("0.10", "0.40")),
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
        // Claude 3.x families offered on AWS Bedrock (incl. GovCloud). The
        // `anthropic.*` Bedrock ids normalize onto these keys and are billed at
        // the same Anthropic public rates as the direct API.
        (
            "claude-3-7-sonnet",
            AnthropicModelRate::new("3.00", "0.30", "3.75", "6.00", "15.00"),
        ),
        (
            "claude-3-7-sonnet-20250219",
            AnthropicModelRate::new("3.00", "0.30", "3.75", "6.00", "15.00"),
        ),
        (
            "claude-3-5-sonnet",
            AnthropicModelRate::new("3.00", "0.30", "3.75", "6.00", "15.00"),
        ),
        (
            "claude-3-5-sonnet-20241022",
            AnthropicModelRate::new("3.00", "0.30", "3.75", "6.00", "15.00"),
        ),
        (
            "claude-3-5-sonnet-20240620",
            AnthropicModelRate::new("3.00", "0.30", "3.75", "6.00", "15.00"),
        ),
        (
            "claude-3-5-haiku",
            AnthropicModelRate::new("0.80", "0.08", "1.00", "1.60", "4.00"),
        ),
        (
            "claude-3-5-haiku-20241022",
            AnthropicModelRate::new("0.80", "0.08", "1.00", "1.60", "4.00"),
        ),
        (
            "claude-3-opus",
            AnthropicModelRate::new("15.00", "1.50", "18.75", "30.00", "75.00"),
        ),
        (
            "claude-3-opus-20240229",
            AnthropicModelRate::new("15.00", "1.50", "18.75", "30.00", "75.00"),
        ),
    ])
});

fn normalize_model_id(model: &str) -> &str {
    let slash_stripped = model
        .strip_prefix("openai/")
        .or_else(|| model.strip_prefix("anthropic/"))
        .or_else(|| model.strip_prefix("google/"))
        .or_else(|| model.strip_prefix("gemini/"))
        .or_else(|| model.strip_prefix("xai/"))
        .or_else(|| model.strip_prefix("x-ai/"))
        .or_else(|| model.strip_prefix("x.ai/"))
        .or_else(|| model.strip_prefix("grok/"))
        .unwrap_or(model);
    // Also fold AWS Bedrock `anthropic.*` ids onto their Anthropic keys.
    crate::pricing::strip_bedrock_model_shape(slash_stripped)
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

/// Return the exact USD string for a direct OpenAI-compatible model.
///
/// `cached_input_tokens` is the prompt-cache *read* count. These providers
/// report `prompt_tokens` **inclusive** of cached tokens (unlike Anthropic), so
/// the cached portion is split out and priced at the model's
/// [`ModelRate::cached_input_per_mtok`] when one is known. Where it is not,
/// cached tokens stay at the full input rate — see that field for why no rate is
/// guessed.
pub fn estimate_cost_usd(
    provider: Provider,
    model: &str,
    prompt_tokens: u64,
    completion_tokens: u64,
    cached_input_tokens: u64,
) -> Option<String> {
    let rates = rates_for(provider)?;
    let rate = rates.get(normalize_model_id(model))?;
    let input_rate = Usd::parse(rate.input_per_mtok).ok()?;
    // Clamp: a provider reporting more cached than prompt tokens would
    // otherwise underflow to a huge uncached count.
    let cached = cached_input_tokens.min(prompt_tokens);
    let (uncached_tokens, cached_cost) = match rate.cached_input_per_mtok {
        Some(cached_rate) => (
            prompt_tokens - cached,
            Usd::from_tokens_at_rate(cached, Usd::parse(cached_rate).ok()?),
        ),
        // No published cache-read rate: bill the whole prompt at the input rate,
        // exactly as before.
        None => (prompt_tokens, Usd::from_tokens_at_rate(0, input_rate)),
    };
    let input = Usd::from_tokens_at_rate(uncached_tokens, input_rate);
    let output =
        Usd::from_tokens_at_rate(completion_tokens, Usd::parse(rate.output_per_mtok).ok()?);
    Some((input + cached_cost + output).to_storage_string())
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

/// Shipped AWS Bedrock model ids (with region prefix + `-vN:M` suffix) that
/// must keep resolving to an Anthropic rate.
#[cfg(test)]
pub fn shipped_bedrock_models() -> &'static [&'static str] {
    &[
        "anthropic.claude-sonnet-4-20250514-v1:0",
        "us.anthropic.claude-opus-4-20250514-v1:0",
        "anthropic.claude-3-7-sonnet-20250219-v1:0",
        "us.anthropic.claude-3-5-sonnet-20241022-v2:0",
        "anthropic.claude-3-5-haiku-20241022-v1:0",
        "eu.anthropic.claude-3-opus-20240229-v1:0",
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openai_cost_is_deterministic() {
        let cost = estimate_cost_usd(Provider::OpenAi, "gpt-4.1", 1_000, 500, 0).unwrap();
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
        assert!(estimate_cost_usd(Provider::OpenAi, "missing", 1_000, 500, 0).is_none());
    }

    /// With a published cache-read rate, the cached slice of `prompt_tokens` must
    /// be billed at that rate and only the remainder at the full input rate.
    ///
    /// Hand-computed against a synthetic rate row so the arithmetic is pinned
    /// independently of the shipped tables: input $10/Mtok, cached $1/Mtok,
    /// output $30/Mtok, with 1000 prompt tokens of which 800 are cached, and 500
    /// output tokens.
    ///   uncached: 200 × 10/1e6  = 0.002
    ///   cached:   800 × 1/1e6   = 0.0008
    ///   output:   500 × 30/1e6  = 0.015
    ///   total                   = 0.0178
    #[test]
    fn cached_read_rate_is_honored_when_present() {
        let rate = ModelRate::with_cached_input("10.00", "30.00", "1.00");
        let input_rate = Usd::parse(rate.input_per_mtok).unwrap();
        let cached_rate = Usd::parse(rate.cached_input_per_mtok.unwrap()).unwrap();
        let output_rate = Usd::parse(rate.output_per_mtok).unwrap();

        let total = Usd::from_tokens_at_rate(200, input_rate)
            + Usd::from_tokens_at_rate(800, cached_rate)
            + Usd::from_tokens_at_rate(500, output_rate);
        assert_eq!(total.to_storage_string(), "0.0178");

        // The same inputs with no cache-read rate bill the full 1000 prompt
        // tokens at the input rate: 1000 × 10/1e6 + 0.015 = 0.025.
        let uncached_total = Usd::from_tokens_at_rate(1_000, input_rate)
            + Usd::from_tokens_at_rate(500, output_rate);
        assert_eq!(uncached_total.to_storage_string(), "0.025");
        assert!(
            total < uncached_total,
            "a cache-read discount must lower the total"
        );
    }

    /// Until a model carries a published cache-read rate, cached tokens stay at
    /// the input rate — an over-estimate we accept rather than guessing. Pins the
    /// documented current behavior so filling a rate in is a visible change.
    #[test]
    fn cached_tokens_do_not_change_cost_without_a_published_rate() {
        assert!(
            OPENAI_RATES
                .get("gpt-4.1")
                .expect("gpt-4.1 rate")
                .cached_input_per_mtok
                .is_none(),
            "update this test alongside the rate when gpt-4.1 gains a cache-read rate"
        );
        let without = estimate_cost_usd(Provider::OpenAi, "gpt-4.1", 1_000, 500, 0).unwrap();
        let with_cached = estimate_cost_usd(Provider::OpenAi, "gpt-4.1", 1_000, 500, 800).unwrap();
        assert_eq!(without, with_cached);
    }

    /// A provider reporting more cached than prompt tokens must not underflow the
    /// uncached remainder into a huge number.
    #[test]
    fn cached_tokens_exceeding_prompt_tokens_are_clamped() {
        let cost = estimate_cost_usd(Provider::OpenAi, "gpt-4.1", 100, 0, u64::MAX)
            .expect("cost must still compute");
        let baseline = estimate_cost_usd(Provider::OpenAi, "gpt-4.1", 100, 0, 0).unwrap();
        assert_eq!(cost, baseline);
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
        let cost = estimate_cost_usd(Provider::XAi, "grok-4", 1_000, 500, 0).unwrap();
        assert_eq!(cost, "0.0125");
    }

    #[test]
    fn xai_cost_for_grok_code_fast_uses_published_rates() {
        // grok-code-fast = $0.20 input / $1.50 output per Mtok.
        // 10_000 prompt × 0.20 / 1e6 = 0.002; 2_000 completion × 1.50 / 1e6 = 0.003.
        let cost = estimate_cost_usd(Provider::XAi, "grok-code-fast", 10_000, 2_000, 0).unwrap();
        assert_eq!(cost, "0.005");
        // The dated `-1` alias is the same family — must yield the same number.
        let cost_alias =
            estimate_cost_usd(Provider::XAi, "grok-code-fast-1", 10_000, 2_000, 0).unwrap();
        assert_eq!(cost_alias, cost);
    }

    #[test]
    fn xai_cost_normalizes_every_vendor_prefix() {
        // A user who pulls a slug from xAI docs (`xai/`), OpenRouter (`x-ai/`),
        // the dotted brand (`x.ai/`), or just types the product name (`grok/`)
        // must hit the same rate. If any prefix path silently bypasses the
        // table, cost-event attribution regresses to None for that variant.
        let base = estimate_cost_usd(Provider::XAi, "grok-4-fast", 1_000_000, 0, 0).unwrap();
        for variant in [
            "xai/grok-4-fast",
            "x-ai/grok-4-fast",
            "x.ai/grok-4-fast",
            "grok/grok-4-fast",
        ] {
            let got = estimate_cost_usd(Provider::XAi, variant, 1_000_000, 0, 0).unwrap();
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
        assert!(estimate_cost_usd(Provider::XAi, "grok-vapor-99", 100, 100, 0).is_none());
    }

    #[test]
    fn bedrock_anthropic_id_prices_like_direct_anthropic() {
        // `anthropic.claude-sonnet-4-20250514-v1:0` must bill exactly like the
        // direct `claude-sonnet-4` API model: 1_000 in × 3/Mtok = 0.003,
        // 500 out × 15/Mtok = 0.0075, total 0.0105.
        let bedrock = estimate_anthropic_cost_usd(
            "anthropic.claude-sonnet-4-20250514-v1:0",
            1_000,
            500,
            0,
            0,
            CacheTtl::FiveMin,
        )
        .unwrap();
        let direct =
            estimate_anthropic_cost_usd("claude-sonnet-4", 1_000, 500, 0, 0, CacheTtl::FiveMin)
                .unwrap();
        assert_eq!(bedrock, "0.0105");
        assert_eq!(bedrock, direct);
    }

    #[test]
    fn bedrock_cross_region_prefix_and_version_are_stripped() {
        // Cross-region inference profile ids carry a geo prefix (`us.`, `eu.`)
        // and a `-vN:M` version tag; both must be normalized away.
        for id in [
            "us.anthropic.claude-3-7-sonnet-20250219-v1:0",
            "eu.anthropic.claude-3-7-sonnet-20250219-v1:0",
            "anthropic.claude-3-7-sonnet-20250219-v1:0",
        ] {
            let cost =
                estimate_anthropic_cost_usd(id, 1_000, 500, 0, 0, CacheTtl::FiveMin).unwrap();
            assert_eq!(cost, "0.0105", "{id} did not price at the 3.7 Sonnet rate");
        }
    }

    #[test]
    fn bedrock_haiku_35_uses_haiku_rate_and_cache_tiers() {
        // claude-3-5-haiku = $0.80 in / $4.00 out; cache read 0.08, write5m 1.00.
        // 1_000 in × 0.80/Mtok = 0.0008; 500 out × 4/Mtok = 0.002 → 0.0028.
        let cost = estimate_anthropic_cost_usd(
            "anthropic.claude-3-5-haiku-20241022-v1:0",
            1_000,
            500,
            0,
            0,
            CacheTtl::FiveMin,
        )
        .unwrap();
        assert_eq!(cost, "0.0028");
        // With 1_000 cache-read + 1_000 cache-write-5m tokens:
        // +1_000 × 0.08/Mtok = 0.00008, +1_000 × 1.00/Mtok = 0.001 → 0.00388.
        let cached = estimate_anthropic_cost_usd(
            "anthropic.claude-3-5-haiku-20241022-v1:0",
            1_000,
            500,
            1_000,
            1_000,
            CacheTtl::FiveMin,
        )
        .unwrap();
        assert_eq!(cached, "0.00388");
    }

    #[test]
    fn non_anthropic_bedrock_model_returns_none() {
        // Bedrock also hosts Titan/Llama; those are not Anthropic-priced and
        // must return None so the daemon emits "unknown" rather than wrong dollars.
        assert!(estimate_anthropic_cost_usd(
            "amazon.titan-text-express-v1",
            1_000,
            500,
            0,
            0,
            CacheTtl::FiveMin
        )
        .is_none());
    }

    #[test]
    fn shipped_bedrock_models_resolve_to_anthropic_rates() {
        for model in shipped_bedrock_models() {
            assert!(
                anthropic_rates_for(model).is_some(),
                "Bedrock model {model} lost its Anthropic rate mapping"
            );
        }
    }

    #[test]
    fn normalize_model_id_strips_bedrock_shape_only_when_valid() {
        // A well-formed Bedrock id folds to the bare Anthropic key.
        assert_eq!(
            normalize_model_id("us.anthropic.claude-opus-4-20250514-v1:0"),
            "claude-opus-4-20250514"
        );
        // A `-v…` fragment that is not a `major:minor` version stays intact.
        assert_eq!(
            normalize_model_id("anthropic.claude-vision-preview"),
            "claude-vision-preview"
        );
        // Native Anthropic ids are untouched.
        assert_eq!(normalize_model_id("claude-sonnet-4"), "claude-sonnet-4");
    }
}
