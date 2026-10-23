// Pricing data last updated: 2026-10-23
// Source: provider pricing pages (the list is in `llm/pricing.rs`). Update
// entries when prices change.

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
        "x-ai/",
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

/// Fold a dotted Claude version (`claude-sonnet-5.5`, as docs and chat
/// pickers spell it) onto the dashed id Anthropic serves and every table here
/// keys on (`claude-sonnet-5-5`). Other vendors use dots in their real ids
/// (`gpt-5.5`, `gemini-2.5-pro`), so only the `claude-` family is folded.
pub(crate) fn fold_claude_dotted_version(model: &str) -> std::borrow::Cow<'_, str> {
    if model.starts_with("claude-") && model.contains('.') {
        std::borrow::Cow::Owned(model.replace('.', "-"))
    } else {
        std::borrow::Cow::Borrowed(model)
    }
}

static PRICING: LazyLock<HashMap<&'static str, ModelPricing>> = LazyLock::new(|| {
    let entries: Vec<(&str, f64, f64)> = vec![
        // Claude 5. The 5.5 Opus is cheaper than Opus 5 ($4/$20 against
        // $5/$25); Sonnet 5 and 5.5 both list at $2/$10.
        ("claude-fable-5-1", 10.0, 50.0),
        ("claude-fable-5", 10.0, 50.0),
        ("claude-opus-5-5", 4.0, 20.0),
        ("claude-sonnet-5-5", 2.0, 10.0),
        // Prompts up to 100K tokens; LONG_CONTEXT_PRICING has the rest.
        ("claude-haiku-5-5", 0.10, 0.50),
        ("claude-opus-5", 5.0, 25.0),
        // Sonnet 5's $2/$10 launch rate is now its standard price; the rise to
        // $3/$15 scheduled for 2026-09-01 did not happen.
        ("claude-sonnet-5", 2.0, 10.0),
        // Claude 4.x. Opus dropped from $15/$75 to $5/$25 at 4.5.
        ("claude-opus-4-8", 5.0, 25.0),
        ("claude-opus-4-7", 5.0, 25.0),
        ("claude-opus-4-6", 5.0, 25.0),
        ("claude-opus-4-5", 5.0, 25.0),
        ("claude-opus-4-1", 15.0, 75.0),
        ("claude-opus-4-20250514", 15.0, 75.0),
        ("claude-opus-4", 15.0, 75.0),
        ("claude-sonnet-4-6", 3.0, 15.0),
        ("claude-sonnet-4-5", 3.0, 15.0),
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
        // xAI. The 4.20 family is listed under its dated ids and the undated
        // names OpenRouter serves.
        ("grok-4.7", 2.0, 6.0),
        ("grok-4.6", 2.0, 6.0),
        ("grok-4.5", 2.0, 6.0),
        ("grok-4.3", 1.25, 2.50),
        ("grok-4.20-0309-reasoning", 1.25, 2.50),
        ("grok-4.20-0309-non-reasoning", 1.25, 2.50),
        ("grok-4.20-multi-agent-0309", 1.25, 2.50),
        ("grok-4.20", 1.25, 2.50),
        ("grok-4.20-multi-agent", 1.25, 2.50),
        ("grok-build-0.1", 1.0, 2.0),
        // No longer on xAI's price list; the rates they were billed at.
        ("grok-4", 3.0, 15.0),
        ("grok-4-fast-reasoning", 0.20, 0.50),
        ("grok-4-fast-non-reasoning", 0.20, 0.50),
        ("grok-4-fast", 0.20, 0.50),
        ("grok-code-fast-1", 0.20, 1.50),
        ("grok-code-fast", 0.20, 1.50),
        ("grok-3", 3.0, 15.0),
        ("grok-3-mini", 0.30, 0.50),
        // GPT-5 family
        ("gpt-5.5", 5.0, 30.0),
        ("gpt-5.5-pro", 30.0, 180.0),
        ("gpt-5.4-mini", 0.75, 4.50),
        ("gpt-5.4-nano", 0.20, 1.25),
        ("gpt-5.3-codex", 1.75, 14.0),
        ("gpt-5.1", 1.25, 10.0),
        // Not on OpenAI's price page; OpenRouter serves it at GPT-5.1's rate.
        ("gpt-5.1-codex", 1.25, 10.0),
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
        ("gpt-4", 30.0, 60.0),
        // o3/o4-mini. o3 fell from $10/$40 to $2/$8.
        ("o3", 2.0, 8.0),
        ("o4-mini", 1.10, 4.40),
        // Gemini. Google serves most 3.x models under a `-preview` id; the
        // bare ids are the names the curated catalog uses.
        ("gemini-3.1-pro-preview", 2.0, 12.0),
        ("gemini-3-pro", 2.0, 12.0),
        ("gemini-3-pro-preview", 2.0, 12.0),
        // Introductory through 2026-12-31; $1.50 / $7.50 from 2027-01-01.
        ("gemini-3.8-flash", 0.75, 3.75),
        ("gemini-3.6-flash", 0.75, 3.75),
        ("gemini-3-flash", 0.50, 3.0),
        ("gemini-3-flash-preview", 0.50, 3.0),
        ("gemini-3.5-flash-lite", 0.30, 2.50),
        ("gemini-3.1-flash-lite", 0.25, 1.50),
        ("gemini-3.1-flash-lite-preview", 0.25, 1.50),
        // Google lists no Gemini 3 Flash-Lite; kept at its old row until the
        // curated catalog drops the id.
        ("gemini-3-flash-lite", 0.05, 0.30),
        ("gemini-2.5-pro", 1.25, 10.0),
        ("gemini-2.5-flash", 0.30, 2.50),
        ("gemini-2.5-flash-lite", 0.10, 0.40),
        ("gemini-2.0-flash", 0.10, 0.40),
        // DeepSeek, direct API (https://api-docs.deepseek.com/quick_start/pricing),
        // cache-miss input. DeepSeek bills peak hours (01:00-04:00 and
        // 06:00-10:00 UTC on weekdays) at twice the off-peak rate; these are
        // the peak rates, so an estimate never under-reports spend.
        // `deepseek-v4-flash`, `deepseek-chat` and `deepseek-reasoner` are
        // legacy names billed as Flash.
        ("deepseek-flash", 0.30, 1.20),
        ("deepseek-v4-flash", 0.30, 1.20),
        ("deepseek-v4-pro", 1.32, 3.96),
        ("deepseek-chat", 0.30, 1.20),
        ("deepseek-reasoner", 0.30, 1.20),
        ("deepseek-v3.2", 0.28, 0.42),
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

/// Rates that replace a model's base row once one request's prompt reaches
/// the first number. The vendor bills the whole request at the higher rate,
/// not only the tokens past the line.
static LONG_CONTEXT_PRICING: LazyLock<HashMap<&'static str, (u64, ModelPricing)>> =
    LazyLock::new(|| {
        let entries: Vec<(&str, u64, f64, f64)> = vec![
            // Anthropic: over 100K.
            ("claude-haiku-5-5", 100_001, 0.50, 2.50),
            // OpenAI: over 272K.
            ("gpt-5.5", 272_001, 10.0, 45.0),
            ("gpt-5.5-pro", 272_001, 60.0, 270.0),
            // Google: over 200K.
            ("gemini-3.1-pro-preview", 200_001, 4.0, 18.0),
            ("gemini-3-pro", 200_001, 4.0, 18.0),
            ("gemini-3-pro-preview", 200_001, 4.0, 18.0),
            ("gemini-2.5-pro", 200_001, 2.50, 15.0),
            // xAI: at 200K and above.
            ("grok-4.7", 200_000, 4.0, 12.0),
            ("grok-4.6", 200_000, 4.0, 12.0),
            ("grok-4.5", 200_000, 4.0, 12.0),
            ("grok-4.3", 200_000, 2.50, 5.0),
            ("grok-4.20-0309-reasoning", 200_000, 2.50, 5.0),
            ("grok-4.20-0309-non-reasoning", 200_000, 2.50, 5.0),
            ("grok-4.20-multi-agent-0309", 200_000, 2.50, 5.0),
            ("grok-4.20", 200_000, 2.50, 5.0),
            ("grok-4.20-multi-agent", 200_000, 2.50, 5.0),
            ("grok-build-0.1", 200_000, 2.0, 4.0),
        ];
        entries
            .into_iter()
            .map(|(name, from, input, output)| {
                let pricing = ModelPricing {
                    input_per_million: input,
                    output_per_million: output,
                };
                (name, (from, pricing))
            })
            .collect()
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
        // Anthropic. Haiku 5.5 carries its rate for prompts up to 100K tokens.
        BundledRateCardEntry {
            provider_id: "anthropic",
            model: "claude-fable-5-1",
            prompt_per_mtok_usd: "10.00",
            completion_per_mtok_usd: "50.00",
        },
        BundledRateCardEntry {
            provider_id: "anthropic",
            model: "claude-fable-5",
            prompt_per_mtok_usd: "10.00",
            completion_per_mtok_usd: "50.00",
        },
        BundledRateCardEntry {
            provider_id: "anthropic",
            model: "claude-opus-5-5",
            prompt_per_mtok_usd: "4.00",
            completion_per_mtok_usd: "20.00",
        },
        BundledRateCardEntry {
            provider_id: "anthropic",
            model: "claude-sonnet-5-5",
            prompt_per_mtok_usd: "2.00",
            completion_per_mtok_usd: "10.00",
        },
        BundledRateCardEntry {
            provider_id: "anthropic",
            model: "claude-haiku-5-5",
            prompt_per_mtok_usd: "0.10",
            completion_per_mtok_usd: "0.50",
        },
        BundledRateCardEntry {
            provider_id: "anthropic",
            model: "claude-opus-5",
            prompt_per_mtok_usd: "5.00",
            completion_per_mtok_usd: "25.00",
        },
        BundledRateCardEntry {
            provider_id: "anthropic",
            model: "claude-sonnet-5",
            prompt_per_mtok_usd: "2.00",
            completion_per_mtok_usd: "10.00",
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
        // OpenAI. GPT-5.5 and 5.5 Pro carry their rate for prompts up to 272K.
        BundledRateCardEntry {
            provider_id: "openai",
            model: "gpt-5.5",
            prompt_per_mtok_usd: "5.00",
            completion_per_mtok_usd: "30.00",
        },
        BundledRateCardEntry {
            provider_id: "openai",
            model: "gpt-5.5-pro",
            prompt_per_mtok_usd: "30.00",
            completion_per_mtok_usd: "180.00",
        },
        BundledRateCardEntry {
            provider_id: "openai",
            model: "gpt-5.4-mini",
            prompt_per_mtok_usd: "0.75",
            completion_per_mtok_usd: "4.50",
        },
        BundledRateCardEntry {
            provider_id: "openai",
            model: "gpt-5.4-nano",
            prompt_per_mtok_usd: "0.20",
            completion_per_mtok_usd: "1.25",
        },
        BundledRateCardEntry {
            provider_id: "openai",
            model: "gpt-5.1-codex",
            prompt_per_mtok_usd: "1.25",
            completion_per_mtok_usd: "10.00",
        },
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
            prompt_per_mtok_usd: "2.00",
            completion_per_mtok_usd: "8.00",
        },
        BundledRateCardEntry {
            provider_id: "openai",
            model: "o4-mini",
            prompt_per_mtok_usd: "1.10",
            completion_per_mtok_usd: "4.40",
        },
        // Gemini. The Pro rows are the rate for prompts up to 200K.
        BundledRateCardEntry {
            provider_id: "gemini",
            model: "gemini-3-pro",
            prompt_per_mtok_usd: "2.00",
            completion_per_mtok_usd: "12.00",
        },
        BundledRateCardEntry {
            provider_id: "gemini",
            model: "gemini-3-flash",
            prompt_per_mtok_usd: "0.50",
            completion_per_mtok_usd: "3.00",
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
        // DeepSeek, at its peak-hour rates. The legacy names bill as Flash.
        BundledRateCardEntry {
            provider_id: "deepseek",
            model: "deepseek-v4-flash",
            prompt_per_mtok_usd: "0.30",
            completion_per_mtok_usd: "1.20",
        },
        BundledRateCardEntry {
            provider_id: "deepseek",
            model: "deepseek-v4-pro",
            prompt_per_mtok_usd: "1.32",
            completion_per_mtok_usd: "3.96",
        },
        BundledRateCardEntry {
            provider_id: "deepseek",
            model: "deepseek-chat",
            prompt_per_mtok_usd: "0.30",
            completion_per_mtok_usd: "1.20",
        },
        BundledRateCardEntry {
            provider_id: "deepseek",
            model: "deepseek-reasoner",
            prompt_per_mtok_usd: "0.30",
            completion_per_mtok_usd: "1.20",
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
///
/// `prompt_tokens` also picks a long-context rate. On a wire that counts cache
/// reads outside the prompt (Anthropic's), a heavily cached request reaches
/// that rate later than the vendor bills it.
pub fn estimate_cost(model: &str, prompt_tokens: u64, completion_tokens: u64) -> Option<f64> {
    let key = fold_claude_dotted_version(normalize_model(model));
    let base = PRICING.get(key.as_ref())?;
    let pricing = match LONG_CONTEXT_PRICING.get(key.as_ref()) {
        Some((from, tier)) if prompt_tokens >= *from => tier,
        _ => base,
    };
    let cost = (prompt_tokens as f64 * pricing.input_per_million
        + completion_tokens as f64 * pricing.output_per_million)
        / 1_000_000.0;
    Some(cost)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Peak-hour rates from the 2026-10 direct-API page. Stale rates here
    /// don't fail loudly — they silently mis-bill every turn and skew the
    /// budget guard, so pin the arithmetic.
    #[test]
    fn deepseek_v4_costs_match_the_published_rates() {
        // 1M input × $0.30/M + 500K output × $1.20/M = 0.30 + 0.60 = 0.90
        let flash = estimate_cost("deepseek-v4-flash", 1_000_000, 500_000).unwrap();
        assert!((flash - 0.90).abs() < 1e-9, "got {flash}");
        let renamed = estimate_cost("deepseek-flash", 1_000_000, 500_000).unwrap();
        assert!((renamed - flash).abs() < 1e-9, "got {renamed}");

        // 1M input × $1.32/M + 500K output × $3.96/M = 1.32 + 1.98 = 3.30
        let pro = estimate_cost("deepseek-v4-pro", 1_000_000, 500_000).unwrap();
        assert!((pro - 3.30).abs() < 1e-9, "got {pro}");

        assert!(
            pro > flash,
            "the Pro tier must never price below Flash: {pro} vs {flash}"
        );
    }

    /// Invariant: `deepseek-chat` / `deepseek-reasoner` are legacy names for
    /// V4 Flash's non-thinking / thinking modes, so they must bill at exactly
    /// its rate — not the retired V3-era rates.
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
    fn dotted_claude_version_prices_as_the_dashed_id() {
        // `claude-sonnet-5.5` is how people write the model; the table is
        // keyed on the served id. A miss here silently zeroes the cost.
        for (dotted, dashed) in [
            ("claude-sonnet-5.5", "claude-sonnet-5-5"),
            ("claude-opus-5.5", "claude-opus-5-5"),
            ("anthropic/claude-sonnet-5.5", "claude-sonnet-5-5"),
        ] {
            let want = estimate_cost(dashed, 1_000_000, 1_000_000).unwrap();
            let got = estimate_cost(dotted, 1_000_000, 1_000_000)
                .unwrap_or_else(|| panic!("{dotted} must price like {dashed}"));
            assert!((got - want).abs() < 1e-9, "{dotted}: {got} vs {want}");
        }
        // Opus 5.5 is the cheaper Opus; a swapped row would invert this.
        let opus_5_5 = estimate_cost("claude-opus-5.5", 1_000_000, 1_000_000).unwrap();
        let opus_5 = estimate_cost("claude-opus-5", 1_000_000, 1_000_000).unwrap();
        assert!(opus_5_5 < opus_5, "{opus_5_5} vs {opus_5}");
        // Dots in other vendors' ids are real.
        assert_eq!(fold_claude_dotted_version("gpt-5.5").as_ref(), "gpt-5.5");
        assert_eq!(
            fold_claude_dotted_version("gemini-2.5-pro").as_ref(),
            "gemini-2.5-pro"
        );
    }

    /// The Usage page's spend comes from this table, so a Claude id OpenRouter
    /// serves must price. Haiku 5.5 was missing and showed no spend.
    #[test]
    fn current_claude_models_price_through_openrouter() {
        for (id, want) in [
            // 100K in + 100K out, under every long-context line.
            ("anthropic/claude-haiku-5.5", 0.06),
            ("anthropic/claude-fable-5.1", 6.0),
            ("anthropic/claude-sonnet-5", 1.2),
        ] {
            let got =
                estimate_cost(id, 100_000, 100_000).unwrap_or_else(|| panic!("{id} must price"));
            assert!((got - want).abs() < 1e-9, "{id}: {got} vs {want}");
        }
    }

    /// A request past a model's long-context line bills in full at the higher
    /// rate; one at the line or under it keeps the base rate.
    #[test]
    fn long_prompts_bill_at_the_long_context_rate() {
        // Haiku 5.5: over 100K. 100,000 × $0.10 = 0.01; 100,001 × $0.50.
        let at = estimate_cost("claude-haiku-5-5", 100_000, 0).unwrap();
        assert!((at - 0.01).abs() < 1e-12, "got {at}");
        let over = estimate_cost("claude-haiku-5-5", 100_001, 1_000).unwrap();
        assert!((over - (0.0500005 + 0.0025)).abs() < 1e-12, "got {over}");

        // GPT-5.5: over 272K. 300K × $10 + 1K × $45 = 3.045.
        let gpt = estimate_cost("openai/gpt-5.5", 300_000, 1_000).unwrap();
        assert!((gpt - 3.045).abs() < 1e-9, "got {gpt}");

        // xAI starts at 200K inclusive: 200K × $2.50 = 0.50.
        let grok = estimate_cost("x-ai/grok-4.20", 200_000, 0).unwrap();
        assert!((grok - 0.50).abs() < 1e-9, "got {grok}");
    }

    /// The OpenAI rates that moved: o3 fell to $2/$8, and the GPT-5.5 line
    /// is priced instead of skipped.
    #[test]
    fn openai_rates_match_the_published_card() {
        for (id, want) in [
            // 100K in + 100K out, under GPT-5.5's 272K line.
            ("o3", 1.0),
            ("gpt-5.5", 3.5),
            ("gpt-5.5-pro", 21.0),
            ("gpt-5.4-mini", 0.525),
            ("gpt-5.4-nano", 0.145),
        ] {
            let got =
                estimate_cost(id, 100_000, 100_000).unwrap_or_else(|| panic!("{id} must price"));
            assert!((got - want).abs() < 1e-9, "{id}: {got} vs {want}");
        }
    }

    /// The fresh-install rate card and the estimate table must agree, or the
    /// budget and the Usage page report two prices for one call.
    #[test]
    fn bundled_rate_card_matches_the_estimate_table() {
        for entry in bundled_rate_card_entries() {
            if entry.model == "*" {
                continue;
            }
            let row = PRICING
                .get(entry.model)
                .unwrap_or_else(|| panic!("{} has a card but no estimate row", entry.model));
            let prompt: f64 = entry.prompt_per_mtok_usd.parse().unwrap();
            let completion: f64 = entry.completion_per_mtok_usd.parse().unwrap();
            assert!(
                (prompt - row.input_per_million).abs() < 1e-9
                    && (completion - row.output_per_million).abs() < 1e-9,
                "{}: card {prompt}/{completion} vs estimate {}/{}",
                entry.model,
                row.input_per_million,
                row.output_per_million
            );
        }
    }

    #[test]
    fn normalize_strips_provider_prefix() {
        assert_eq!(normalize_model("openai/gpt-4o"), "gpt-4o");
        assert_eq!(normalize_model("google/gemini-2.5-pro"), "gemini-2.5-pro");
        assert_eq!(normalize_model("x-ai/grok-4.20"), "grok-4.20");
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
