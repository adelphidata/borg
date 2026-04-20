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
/// same pricing entry.
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
    model
}

static PRICING: LazyLock<HashMap<&'static str, ModelPricing>> = LazyLock::new(|| {
    let entries: Vec<(&str, f64, f64)> = vec![
        // Claude 4.x (frontier)
        ("claude-opus-4-7", 15.0, 75.0),
        ("claude-opus-4-20250514", 15.0, 75.0),
        ("claude-opus-4", 15.0, 75.0),
        ("claude-sonnet-4-6", 3.0, 15.0),
        ("claude-sonnet-4-20250514", 3.0, 15.0),
        ("claude-sonnet-4-0", 3.0, 15.0),
        ("claude-sonnet-4", 3.0, 15.0),
        ("claude-haiku-4-5-20251001", 0.80, 4.0),
        ("claude-haiku-4-5", 0.80, 4.0),
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
        ("gemini-2.5-flash", 0.15, 0.60),
        ("gemini-2.0-flash", 0.10, 0.40),
        // DeepSeek (deepseek-chat is V3.2 alias)
        ("deepseek-chat", 0.27, 1.10),
        ("deepseek-v3.2", 0.27, 1.10),
        ("deepseek-chat-v3", 0.27, 1.10),
        ("deepseek-reasoner", 0.55, 2.19),
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
        assert!(estimate_cost("deepseek/deepseek-r1", 1000, 1000).is_some());
    }
}
