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
    /// cached tokens, so `estimate_cost_usd` splits the cached slice out and
    /// bills it here.
    ///
    /// `None` bills cached reads at the full input rate rather than guessing a
    /// discount: this is a billing path, and a fabricated discount is worse
    /// than a known over-estimate. Populate it from the provider's published
    /// pricing (see the `update-model-catalog` skill);
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

// Sources captured on 2026-10-23 (standard tier, USD per million tokens):
// - Anthropic: https://platform.claude.com/docs/en/about-claude/pricing
// - OpenAI: https://developers.openai.com/api/docs/pricing
// - Google: https://ai.google.dev/gemini-api/docs/pricing
// - xAI: https://docs.x.ai/docs/models
// A model a vendor no longer lists keeps the rate it was last billed at, so
// replaying an old session prices the same; each such row says so.

static OPENAI_RATES: LazyLock<HashMap<&'static str, ModelRate>> = LazyLock::new(|| {
    HashMap::from([
        // GPT-5.5 has a long-context tier above 272K; see LONG_CONTEXT_RATES.
        (
            "gpt-5.5",
            ModelRate::with_cached_input("5.00", "30.00", "0.50"),
        ),
        ("gpt-5.5-pro", ModelRate::new("30.00", "180.00")),
        (
            "gpt-5.4-mini",
            ModelRate::with_cached_input("0.75", "4.50", "0.075"),
        ),
        (
            "gpt-5.4-nano",
            ModelRate::with_cached_input("0.20", "1.25", "0.02"),
        ),
        (
            "gpt-5.3-codex",
            ModelRate::with_cached_input("1.75", "14.00", "0.175"),
        ),
        (
            "gpt-5.1",
            ModelRate::with_cached_input("1.25", "10.00", "0.125"),
        ),
        // Not on OpenAI's price page. OpenRouter serves it at GPT-5.1's input
        // and output rate; no cache-read rate is published, so none is guessed.
        ("gpt-5.1-codex", ModelRate::new("1.25", "10.00")),
        (
            "gpt-5",
            ModelRate::with_cached_input("1.25", "10.00", "0.125"),
        ),
        (
            "gpt-5-mini",
            ModelRate::with_cached_input("0.25", "2.00", "0.025"),
        ),
        (
            "gpt-5-nano",
            ModelRate::with_cached_input("0.05", "0.40", "0.005"),
        ),
        ("o3", ModelRate::with_cached_input("2.00", "8.00", "0.50")),
        (
            "o4-mini",
            ModelRate::with_cached_input("1.10", "4.40", "0.275"),
        ),
        (
            "gpt-4.1",
            ModelRate::with_cached_input("2.00", "8.00", "0.50"),
        ),
        (
            "gpt-4.1-mini",
            ModelRate::with_cached_input("0.40", "1.60", "0.10"),
        ),
        (
            "gpt-4.1-nano",
            ModelRate::with_cached_input("0.10", "0.40", "0.025"),
        ),
        (
            "gpt-4o",
            ModelRate::with_cached_input("2.50", "10.00", "1.25"),
        ),
        (
            "gpt-4o-2024-08-06",
            ModelRate::with_cached_input("2.50", "10.00", "1.25"),
        ),
        (
            "gpt-4o-mini",
            ModelRate::with_cached_input("0.15", "0.60", "0.075"),
        ),
        // Only dated GPT-4 snapshots remain on the page; this is their rate.
        ("gpt-4", ModelRate::new("30.00", "60.00")),
    ])
});

// xAI bills a whole request at the higher rate once its prompt reaches 200K
// tokens; see LONG_CONTEXT_RATES.
static XAI_RATES: LazyLock<HashMap<&'static str, ModelRate>> = LazyLock::new(|| {
    HashMap::from([
        (
            "grok-4.7",
            ModelRate::with_cached_input("2.00", "6.00", "0.50"),
        ),
        (
            "grok-4.6",
            ModelRate::with_cached_input("2.00", "6.00", "0.50"),
        ),
        (
            "grok-4.5",
            ModelRate::with_cached_input("2.00", "6.00", "0.30"),
        ),
        (
            "grok-4.3",
            ModelRate::with_cached_input("1.25", "2.50", "0.20"),
        ),
        (
            "grok-4.20-0309-reasoning",
            ModelRate::with_cached_input("1.25", "2.50", "0.20"),
        ),
        (
            "grok-4.20-0309-non-reasoning",
            ModelRate::with_cached_input("1.25", "2.50", "0.20"),
        ),
        (
            "grok-4.20-multi-agent-0309",
            ModelRate::with_cached_input("1.25", "2.50", "0.20"),
        ),
        // OpenRouter's undated names for the 4.20 family.
        (
            "grok-4.20",
            ModelRate::with_cached_input("1.25", "2.50", "0.20"),
        ),
        (
            "grok-4.20-multi-agent",
            ModelRate::with_cached_input("1.25", "2.50", "0.20"),
        ),
        (
            "grok-build-0.1",
            ModelRate::with_cached_input("1.00", "2.00", "0.20"),
        ),
        // No longer on xAI's price list; the rates they were billed at. Grok 4
        // was $3/$15, not the $5/$15 this table used to carry.
        ("grok-4", ModelRate::new("3.00", "15.00")),
        ("grok-4-fast", ModelRate::new("0.20", "0.50")),
        ("grok-4-fast-reasoning", ModelRate::new("0.20", "0.50")),
        ("grok-4-fast-non-reasoning", ModelRate::new("0.20", "0.50")),
        ("grok-code-fast-1", ModelRate::new("0.20", "1.50")),
        ("grok-code-fast", ModelRate::new("0.20", "1.50")),
        ("grok-3", ModelRate::new("3.00", "15.00")),
        ("grok-3-mini", ModelRate::new("0.30", "0.50")),
    ])
});

// Paid tier, text and image input. The Pro models have a long-context tier
// above 200K; see LONG_CONTEXT_RATES.
static GEMINI_RATES: LazyLock<HashMap<&'static str, ModelRate>> = LazyLock::new(|| {
    HashMap::from([
        (
            "gemini-3.1-pro-preview",
            ModelRate::with_cached_input("2.00", "12.00", "0.20"),
        ),
        // Retired in favour of 3.1 Pro; the launch rate it was billed at.
        ("gemini-3-pro", ModelRate::new("2.00", "12.00")),
        ("gemini-3-pro-preview", ModelRate::new("2.00", "12.00")),
        // Introductory through 2026-12-31; Google lists $1.50 / $7.50 / $0.15
        // cached from 2027-01-01.
        (
            "gemini-3.8-flash",
            ModelRate::with_cached_input("0.75", "3.75", "0.075"),
        ),
        (
            "gemini-3.6-flash",
            ModelRate::with_cached_input("0.75", "3.75", "0.075"),
        ),
        (
            "gemini-3-flash",
            ModelRate::with_cached_input("0.50", "3.00", "0.05"),
        ),
        (
            "gemini-3-flash-preview",
            ModelRate::with_cached_input("0.50", "3.00", "0.05"),
        ),
        (
            "gemini-3.5-flash-lite",
            ModelRate::with_cached_input("0.30", "2.50", "0.03"),
        ),
        (
            "gemini-3.1-flash-lite",
            ModelRate::with_cached_input("0.25", "1.50", "0.025"),
        ),
        (
            "gemini-3.1-flash-lite-preview",
            ModelRate::with_cached_input("0.25", "1.50", "0.025"),
        ),
        // Google lists no Gemini 3 Flash-Lite; the curated id predates 3.1
        // Flash-Lite. Kept at its old row until the catalog drops the id.
        ("gemini-3-flash-lite", ModelRate::new("0.05", "0.30")),
        (
            "gemini-2.5-pro",
            ModelRate::with_cached_input("1.25", "10.00", "0.125"),
        ),
        (
            "gemini-2.5-flash",
            ModelRate::with_cached_input("0.30", "2.50", "0.03"),
        ),
        (
            "gemini-2.5-flash-lite",
            ModelRate::with_cached_input("0.10", "0.40", "0.01"),
        ),
        // No longer on Google's price page; the rate it was billed at.
        ("gemini-2.0-flash", ModelRate::new("0.10", "0.40")),
    ])
});

/// A rate that replaces a model's base row once a single request's prompt
/// reaches `from_prompt_tokens`. Vendors bill the whole request at the higher
/// rate, not just the tokens past the line.
struct LongContext<R> {
    from_prompt_tokens: u64,
    rate: R,
}

static LONG_CONTEXT_RATES: LazyLock<HashMap<&'static str, LongContext<ModelRate>>> =
    LazyLock::new(|| {
        let above = |from_prompt_tokens, rate| LongContext {
            from_prompt_tokens,
            rate,
        };
        HashMap::from([
            // OpenAI: over 272K input tokens.
            (
                "gpt-5.5",
                above(
                    272_001,
                    ModelRate::with_cached_input("10.00", "45.00", "1.00"),
                ),
            ),
            (
                "gpt-5.5-pro",
                above(272_001, ModelRate::new("60.00", "270.00")),
            ),
            // Google: over 200K.
            (
                "gemini-3.1-pro-preview",
                above(
                    200_001,
                    ModelRate::with_cached_input("4.00", "18.00", "0.40"),
                ),
            ),
            (
                "gemini-3-pro",
                above(200_001, ModelRate::new("4.00", "18.00")),
            ),
            (
                "gemini-3-pro-preview",
                above(200_001, ModelRate::new("4.00", "18.00")),
            ),
            (
                "gemini-2.5-pro",
                above(
                    200_001,
                    ModelRate::with_cached_input("2.50", "15.00", "0.25"),
                ),
            ),
            // xAI: at 200K and above.
            (
                "grok-4.7",
                above(
                    200_000,
                    ModelRate::with_cached_input("4.00", "12.00", "1.00"),
                ),
            ),
            (
                "grok-4.6",
                above(
                    200_000,
                    ModelRate::with_cached_input("4.00", "12.00", "1.00"),
                ),
            ),
            (
                "grok-4.5",
                above(
                    200_000,
                    ModelRate::with_cached_input("4.00", "12.00", "0.60"),
                ),
            ),
            (
                "grok-4.3",
                above(
                    200_000,
                    ModelRate::with_cached_input("2.50", "5.00", "0.40"),
                ),
            ),
            (
                "grok-4.20-0309-reasoning",
                above(
                    200_000,
                    ModelRate::with_cached_input("2.50", "5.00", "0.40"),
                ),
            ),
            (
                "grok-4.20-0309-non-reasoning",
                above(
                    200_000,
                    ModelRate::with_cached_input("2.50", "5.00", "0.40"),
                ),
            ),
            (
                "grok-4.20-multi-agent-0309",
                above(
                    200_000,
                    ModelRate::with_cached_input("2.50", "5.00", "0.40"),
                ),
            ),
            (
                "grok-4.20",
                above(
                    200_000,
                    ModelRate::with_cached_input("2.50", "5.00", "0.40"),
                ),
            ),
            (
                "grok-4.20-multi-agent",
                above(
                    200_000,
                    ModelRate::with_cached_input("2.50", "5.00", "0.40"),
                ),
            ),
            (
                "grok-build-0.1",
                above(
                    200_000,
                    ModelRate::with_cached_input("2.00", "4.00", "0.40"),
                ),
            ),
        ])
    });

/// Claude Haiku 5.5 bills a request whose prompt is over 100,000 tokens at
/// its higher rates. Anthropic counts cache reads and writes in that length.
static ANTHROPIC_LONG_CONTEXT_RATES: LazyLock<
    HashMap<&'static str, LongContext<AnthropicModelRate>>,
> = LazyLock::new(|| {
    HashMap::from([(
        "claude-haiku-5-5",
        LongContext {
            from_prompt_tokens: 100_001,
            rate: AnthropicModelRate::new("0.50", "0.05", "0.625", "1.00", "2.50"),
        },
    )])
});

static ANTHROPIC_RATES: LazyLock<HashMap<&'static str, AnthropicModelRate>> = LazyLock::new(|| {
    // Columns: input, cache read, 5-minute cache write, 1-hour cache write,
    // output. Cache reads are 0.1x input except where noted.
    let opus_4_5_on = || AnthropicModelRate::new("5.00", "0.50", "6.25", "10.00", "25.00");
    let opus_4 = || AnthropicModelRate::new("15.00", "1.50", "18.75", "30.00", "75.00");
    let sonnet_4 = || AnthropicModelRate::new("3.00", "0.30", "3.75", "6.00", "15.00");
    let haiku_4_5 = || AnthropicModelRate::new("1.00", "0.10", "1.25", "2.00", "5.00");
    HashMap::from([
        // Cache reads are 0.025x input on Fable 5.1, and 0.05x on Opus 5.5
        // and Sonnet 5.5.
        (
            "claude-fable-5-1",
            AnthropicModelRate::new("10.00", "0.25", "12.50", "20.00", "50.00"),
        ),
        (
            "claude-fable-5",
            AnthropicModelRate::new("10.00", "1.00", "12.50", "20.00", "50.00"),
        ),
        (
            "claude-opus-5-5",
            AnthropicModelRate::new("4.00", "0.20", "5.00", "8.00", "20.00"),
        ),
        (
            "claude-sonnet-5-5",
            AnthropicModelRate::new("2.00", "0.10", "2.50", "4.00", "10.00"),
        ),
        // Prompts up to 100K tokens; ANTHROPIC_LONG_CONTEXT_RATES has the rest.
        (
            "claude-haiku-5-5",
            AnthropicModelRate::new("0.10", "0.01", "0.125", "0.20", "0.50"),
        ),
        ("claude-opus-5", opus_4_5_on()),
        // Sonnet 5's $2/$10 launch rate is now its standard price; the rise to
        // $3/$15 scheduled for 2026-09-01 did not happen.
        (
            "claude-sonnet-5",
            AnthropicModelRate::new("2.00", "0.20", "2.50", "4.00", "10.00"),
        ),
        // Opus dropped from $15/$75 to $5/$25 at 4.5.
        ("claude-opus-4-8", opus_4_5_on()),
        ("claude-opus-4-7", opus_4_5_on()),
        ("claude-opus-4-6", opus_4_5_on()),
        ("claude-opus-4-5", opus_4_5_on()),
        ("claude-opus-4-1", opus_4()),
        ("claude-opus-4", opus_4()),
        ("claude-opus-4-20250514", opus_4()),
        ("claude-sonnet-4-6", sonnet_4()),
        ("claude-sonnet-4-5", sonnet_4()),
        ("claude-sonnet-4", sonnet_4()),
        ("claude-sonnet-4-20250514", sonnet_4()),
        ("claude-haiku-4-5", haiku_4_5()),
        ("claude-haiku-4-5-20251001", haiku_4_5()),
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

/// The Anthropic rate a request pays, given its full prompt length (uncached
/// input plus cache reads and writes).
fn anthropic_rates_for(model: &str, prompt_len: u64) -> Option<&'static AnthropicModelRate> {
    let key = crate::pricing::fold_claude_dotted_version(normalize_model_id(model));
    let base = ANTHROPIC_RATES.get(key.as_ref())?;
    match ANTHROPIC_LONG_CONTEXT_RATES.get(key.as_ref()) {
        Some(tier) if prompt_len >= tier.from_prompt_tokens => Some(&tier.rate),
        _ => Some(base),
    }
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
    let key = normalize_model_id(model);
    let base = rates_for(provider)?.get(key)?;
    // These providers count cached tokens inside `prompt_tokens`, so it is
    // already the request's full prompt length.
    let rate = match LONG_CONTEXT_RATES.get(key) {
        Some(tier) if prompt_tokens >= tier.from_prompt_tokens => &tier.rate,
        _ => base,
    };
    cost_for_rate(rate, prompt_tokens, completion_tokens, cached_input_tokens)
}

/// The arithmetic behind [`estimate_cost_usd`], split out so it can be pinned
/// against a synthetic rate row independent of the shipped tables.
fn cost_for_rate(
    rate: &ModelRate,
    prompt_tokens: u64,
    completion_tokens: u64,
    cached_input_tokens: u64,
) -> Option<String> {
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
    let prompt_len = prompt_tokens
        .saturating_add(cached_input_tokens)
        .saturating_add(cache_creation_tokens);
    let rate = anthropic_rates_for(model, prompt_len)?;
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
        "grok-4.7",
        "grok-4.6",
        "grok-4.5",
        "grok-4.3",
        "grok-4.20",
        "grok-4.20-multi-agent",
        "grok-build-0.1",
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
    &[
        "gpt-5.5",
        "gpt-5.5-pro",
        "gpt-5.4-mini",
        "gpt-5.4-nano",
        "gpt-5.1-codex",
        "gpt-5",
        "gpt-5-mini",
        "o3",
        "gpt-4",
        "gpt-4.1",
        "gpt-4.1-mini",
    ]
}

/// Shipped Anthropic direct models that must never silently fall out of the
/// daemon cost-emission table.
#[cfg(test)]
pub fn shipped_anthropic_models() -> &'static [&'static str] {
    &[
        "claude-fable-5-1",
        "claude-fable-5",
        "claude-opus-5-5",
        "claude-sonnet-5-5",
        "claude-haiku-5-5",
        "claude-opus-5",
        "claude-sonnet-5",
        "claude-opus-4-8",
        "claude-opus-4-7",
        "claude-sonnet-4-6",
        "claude-sonnet-4",
        "claude-haiku-4-5",
    ]
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
        let discounted = ModelRate::with_cached_input("10.00", "30.00", "1.00");
        assert_eq!(
            cost_for_rate(&discounted, 1_000, 500, 800).unwrap(),
            "0.0178"
        );

        // Same rate row without the cache-read tier bills the full 1000 prompt
        // tokens at the input rate: 1000 × 10/1e6 + 0.015 = 0.025.
        let plain = ModelRate::new("10.00", "30.00");
        assert_eq!(cost_for_rate(&plain, 1_000, 500, 800).unwrap(), "0.025");

        // A cached count exceeding the prompt total must clamp, not underflow into
        // a huge uncached slice.
        assert_eq!(
            cost_for_rate(&discounted, 1_000, 500, 5_000).unwrap(),
            cost_for_rate(&discounted, 1_000, 500, 1_000).unwrap()
        );
    }

    /// A model with no published cache-read rate bills cached tokens at the
    /// input rate: an over-estimate we accept rather than guessing a discount.
    #[test]
    fn cached_tokens_do_not_change_cost_without_a_published_rate() {
        assert!(
            OPENAI_RATES
                .get("gpt-5.5-pro")
                .expect("gpt-5.5-pro rate")
                .cached_input_per_mtok
                .is_none(),
            "update this test alongside the rate when gpt-5.5-pro gains a cache-read rate"
        );
        let without = estimate_cost_usd(Provider::OpenAi, "gpt-5.5-pro", 1_000, 500, 0).unwrap();
        let with_cached =
            estimate_cost_usd(Provider::OpenAi, "gpt-5.5-pro", 1_000, 500, 800).unwrap();
        assert_eq!(without, with_cached);
    }

    /// OpenAI publishes a cache-read rate for GPT-4.1, so the cached slice of
    /// the prompt bills at it.
    ///   uncached: 200 × 2.00/1e6 = 0.0004
    ///   cached:   800 × 0.50/1e6 = 0.0004
    ///   output:   500 × 8.00/1e6 = 0.004
    #[test]
    fn published_cache_read_rate_discounts_cached_prompt_tokens() {
        let cost = estimate_cost_usd(Provider::OpenAi, "gpt-4.1", 1_000, 500, 800).unwrap();
        assert_eq!(cost, "0.0048");
    }

    /// A provider reporting more cached than prompt tokens must not underflow the
    /// uncached remainder into a huge number.
    #[test]
    fn cached_tokens_exceeding_prompt_tokens_are_clamped() {
        let cost = estimate_cost_usd(Provider::OpenAi, "gpt-4.1", 100, 0, u64::MAX)
            .expect("cost must still compute");
        let all_cached = estimate_cost_usd(Provider::OpenAi, "gpt-4.1", 100, 0, 100).unwrap();
        assert_eq!(cost, all_cached);
    }

    /// Haiku 5.5 bills the whole request at its higher rate once the prompt,
    /// counting cache reads and writes, is over 100,000 tokens.
    #[test]
    fn haiku_5_5_bills_long_prompts_at_the_higher_rate() {
        // 50K uncached × $0.10 + 1K output × $0.50 = 0.005 + 0.0005.
        let short =
            estimate_anthropic_cost_usd("claude-haiku-5-5", 50_000, 1_000, 0, 0, CacheTtl::Auto)
                .unwrap();
        assert_eq!(short, "0.0055");

        // 50K uncached + 60K cache reads = 110K, over the line:
        // 50K × $0.50 + 60K × $0.05 + 1K × $2.50 = 0.025 + 0.003 + 0.0025.
        let long = estimate_anthropic_cost_usd(
            "claude-haiku-5-5",
            50_000,
            1_000,
            60_000,
            0,
            CacheTtl::Auto,
        )
        .unwrap();
        assert_eq!(long, "0.0305");

        // Exactly 100,000 is not over the line.
        let at_line =
            estimate_anthropic_cost_usd("claude-haiku-5-5", 100_000, 0, 0, 0, CacheTtl::Auto)
                .unwrap();
        assert_eq!(at_line, "0.01");
    }

    #[test]
    fn claude_rates_match_the_published_card() {
        // Sonnet 5 at its standard $2/$10, not the $3/$15 that never took effect.
        let sonnet_5 = estimate_anthropic_cost_usd(
            "claude-sonnet-5",
            1_000_000,
            1_000_000,
            0,
            0,
            CacheTtl::Auto,
        )
        .unwrap();
        assert_eq!(sonnet_5, "12.00");
        // Cache reads: Sonnet 5.5 at 0.05x input, Fable 5.1 at 0.025x.
        let sonnet_5_5_read =
            estimate_anthropic_cost_usd("claude-sonnet-5-5", 0, 0, 1_000_000, 0, CacheTtl::Auto)
                .unwrap();
        assert_eq!(sonnet_5_5_read, "0.10");
        let fable_5_1_read = estimate_anthropic_cost_usd(
            "anthropic/claude-fable-5.1",
            0,
            0,
            1_000_000,
            0,
            CacheTtl::Auto,
        )
        .expect("the OpenRouter spelling of Fable 5.1 must price");
        assert_eq!(fable_5_1_read, "0.25");
    }

    /// GPT-5.5 bills a request over 272K input tokens at its long-context rate.
    #[test]
    fn gpt_5_5_long_context_rate_starts_past_272k() {
        // 272,000 × $5 + 1K × $30 = 1.36 + 0.03.
        let at_line = estimate_cost_usd(Provider::OpenAi, "gpt-5.5", 272_000, 1_000, 0).unwrap();
        assert_eq!(at_line, "1.39");
        // 300,000 × $10 + 1K × $45 = 3.00 + 0.045.
        let long = estimate_cost_usd(Provider::OpenAi, "gpt-5.5", 300_000, 1_000, 0).unwrap();
        assert_eq!(long, "3.045");
    }

    /// xAI's higher rate starts at 200K, inclusive.
    #[test]
    fn xai_long_context_rate_starts_at_200k() {
        let below = estimate_cost_usd(Provider::XAi, "grok-4.3", 199_992, 0, 0).unwrap();
        let at = estimate_cost_usd(Provider::XAi, "grok-4.3", 200_000, 0, 0).unwrap();
        // 199,992 × $1.25 and 200,000 × $2.50.
        assert_eq!(below, "0.24999");
        assert_eq!(at, "0.50");
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
        // Hand-computed: grok-4 = $3/Mtok input + $15/Mtok output.
        // 1_000 prompt × 3 / 1e6 = 0.003; 500 completion × 15 / 1e6 = 0.0075.
        // Total = 0.0105. If the rate table is corrupted or the lookup
        // regresses to None, this asserts the exact dollar figure rather than
        // .is_some() — a tautological check would pass for the wrong rate.
        let cost = estimate_cost_usd(Provider::XAi, "grok-4", 1_000, 500, 0).unwrap();
        assert_eq!(cost, "0.0105");
    }

    #[test]
    fn anthropic_cost_folds_dotted_claude_version() {
        // 1M input × $2 + 1M output × $10 = $12 for Sonnet 5.5, however the
        // version is spelled.
        let dashed = estimate_anthropic_cost_usd(
            "claude-sonnet-5-5",
            1_000_000,
            1_000_000,
            0,
            0,
            CacheTtl::Auto,
        )
        .unwrap();
        let dotted = estimate_anthropic_cost_usd(
            "claude-sonnet-5.5",
            1_000_000,
            1_000_000,
            0,
            0,
            CacheTtl::Auto,
        )
        .expect("dotted alias must price");
        assert_eq!(dotted, dashed);
        assert_eq!(dotted, "12.00");
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

    /// `crate::pricing` (the Usage page) carries the same rates by hand,
    /// long-context tiers included. Pin the two together so a price edit in
    /// one table cannot leave the other behind. Probed on both sides of every
    /// long-context line.
    #[test]
    fn cost_event_rates_agree_with_the_usage_estimate_table() {
        const PROMPTS: &[u64] = &[
            1_000, 100_000, 100_001, 199_999, 200_000, 200_001, 272_000, 272_001, 1_000_000,
        ];
        let mut drift = Vec::new();
        let mut check = |model: &str, prompt: u64, event: Option<String>| {
            let event: f64 = event
                .unwrap_or_else(|| panic!("{model} must price"))
                .parse()
                .unwrap();
            match crate::pricing::estimate_cost(model, prompt, 1_000) {
                // Each `Usd` component rounds to the micro-dollar.
                Some(usage) if (usage - event).abs() <= 2e-6 => {}
                usage => drift.push(format!(
                    "{model} @ {prompt}: event {event}, usage {usage:?}"
                )),
            }
        };
        for (provider, table) in [
            (Provider::OpenAi, &*OPENAI_RATES),
            (Provider::Gemini, &*GEMINI_RATES),
            (Provider::XAi, &*XAI_RATES),
        ] {
            for model in table.keys() {
                for &prompt in PROMPTS {
                    check(
                        model,
                        prompt,
                        estimate_cost_usd(provider, model, prompt, 1_000, 0),
                    );
                }
            }
        }
        for model in ANTHROPIC_RATES.keys() {
            for &prompt in PROMPTS {
                check(
                    model,
                    prompt,
                    estimate_anthropic_cost_usd(model, prompt, 1_000, 0, 0, CacheTtl::Auto),
                );
            }
        }
        assert!(
            drift.is_empty(),
            "the two price tables disagree:\n  {}",
            drift.join("\n  ")
        );
    }

    #[test]
    fn shipped_bedrock_models_resolve_to_anthropic_rates() {
        for model in shipped_bedrock_models() {
            assert!(
                anthropic_rates_for(model, 0).is_some(),
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
