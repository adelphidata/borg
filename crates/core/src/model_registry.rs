//! Static metadata for known LLM models — context windows, hard caps, and
//! capability flags. Used to drive token-budget decisions (memory, skills,
//! conversation history) without hardcoding limits that don't fit every model.
//!
//! Tracks `context_window` / `max_context_window` per model; compiled in to
//! match Borg's "all integrations compiled unconditionally" philosophy.
//!
//! Lookup uses prefix matching — `claude-sonnet-4-20250514` resolves via the
//! `claude-sonnet-4` entry. Unknown models fall back to `DEFAULT_CONTEXT_WINDOW`
//! (a conservative 200K) so the system stays usable for new releases.

/// Conservative default context window when a model isn't in the registry.
/// 200K is a safe upper bound for most modern frontier models; downstream
/// budget calculators will clamp against configured caps as well.
pub const DEFAULT_CONTEXT_WINDOW: u32 = 200_000;

/// Per-model metadata. `slug` is matched against the active model name as a
/// prefix, longest match wins.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelMetadata {
    /// Prefix string matched against the model name.
    pub slug: &'static str,
    /// Practical/advertised context window — what the model normally accepts.
    pub context_window: u32,
    /// Hard cap. Often equal to `context_window`; differs for tiered models
    /// like GPT-5.4 where 1M is the absolute max above the standard 272K tier.
    pub max_context_window: u32,
    /// Whether the model supports extended thinking / reasoning tokens.
    pub supports_thinking: bool,
}

/// Registry of known models. Order matters for prefix matching — longer / more
/// specific prefixes must come before their shorter parents (e.g.
/// `claude-haiku-4` before `claude-haiku`). The lookup performs a longest-prefix
/// scan, so this ordering is for human readability rather than correctness.
const REGISTRY: &[ModelMetadata] = &[
    // ── Anthropic Claude family ──
    // Longer/more specific slugs come first so prefix matching resolves them
    // before bare `claude-opus-4`, `claude-sonnet-4`, `claude-haiku-4`.
    ModelMetadata {
        slug: "claude-opus-4-7",
        context_window: 200_000,
        max_context_window: 1_000_000,
        supports_thinking: true,
    },
    ModelMetadata {
        slug: "claude-sonnet-4-6",
        context_window: 200_000,
        max_context_window: 1_000_000,
        supports_thinking: true,
    },
    ModelMetadata {
        slug: "claude-haiku-4-5",
        context_window: 200_000,
        max_context_window: 200_000,
        supports_thinking: true,
    },
    ModelMetadata {
        slug: "claude-opus-4",
        context_window: 200_000,
        max_context_window: 1_000_000,
        supports_thinking: true,
    },
    ModelMetadata {
        slug: "claude-sonnet-4",
        context_window: 200_000,
        max_context_window: 1_000_000,
        supports_thinking: true,
    },
    ModelMetadata {
        slug: "claude-haiku-4",
        context_window: 200_000,
        max_context_window: 200_000,
        supports_thinking: true,
    },
    ModelMetadata {
        slug: "claude-3-7-sonnet",
        context_window: 200_000,
        max_context_window: 200_000,
        supports_thinking: true,
    },
    ModelMetadata {
        slug: "claude-3-5-sonnet",
        context_window: 200_000,
        max_context_window: 200_000,
        supports_thinking: false,
    },
    ModelMetadata {
        slug: "claude-3-5-haiku",
        context_window: 200_000,
        max_context_window: 200_000,
        supports_thinking: false,
    },
    ModelMetadata {
        slug: "claude-3-opus",
        context_window: 200_000,
        max_context_window: 200_000,
        supports_thinking: false,
    },
    ModelMetadata {
        slug: "claude-3-sonnet",
        context_window: 200_000,
        max_context_window: 200_000,
        supports_thinking: false,
    },
    ModelMetadata {
        slug: "claude-3-haiku",
        context_window: 200_000,
        max_context_window: 200_000,
        supports_thinking: false,
    },
    // ── OpenAI GPT family ──
    // More specific gpt-5-* slugs precede bare `gpt-5` for longest-prefix match.
    ModelMetadata {
        slug: "gpt-5-mini",
        context_window: 272_000,
        max_context_window: 400_000,
        supports_thinking: true,
    },
    ModelMetadata {
        slug: "gpt-5-nano",
        context_window: 272_000,
        max_context_window: 400_000,
        supports_thinking: true,
    },
    ModelMetadata {
        slug: "gpt-5",
        context_window: 272_000,
        max_context_window: 1_000_000,
        supports_thinking: true,
    },
    ModelMetadata {
        slug: "o4-mini",
        context_window: 200_000,
        max_context_window: 200_000,
        supports_thinking: true,
    },
    ModelMetadata {
        slug: "gpt-4.1",
        context_window: 1_000_000,
        max_context_window: 1_000_000,
        supports_thinking: false,
    },
    ModelMetadata {
        slug: "gpt-4o",
        context_window: 128_000,
        max_context_window: 128_000,
        supports_thinking: false,
    },
    ModelMetadata {
        slug: "gpt-4-turbo",
        context_window: 128_000,
        max_context_window: 128_000,
        supports_thinking: false,
    },
    ModelMetadata {
        slug: "gpt-4",
        context_window: 8_192,
        max_context_window: 8_192,
        supports_thinking: false,
    },
    ModelMetadata {
        slug: "gpt-3.5-turbo",
        context_window: 16_385,
        max_context_window: 16_385,
        supports_thinking: false,
    },
    ModelMetadata {
        slug: "o3",
        context_window: 200_000,
        max_context_window: 200_000,
        supports_thinking: true,
    },
    ModelMetadata {
        slug: "o1-mini",
        context_window: 128_000,
        max_context_window: 128_000,
        supports_thinking: true,
    },
    ModelMetadata {
        slug: "o1-preview",
        context_window: 128_000,
        max_context_window: 128_000,
        supports_thinking: true,
    },
    ModelMetadata {
        slug: "o1",
        context_window: 200_000,
        max_context_window: 200_000,
        supports_thinking: true,
    },
    // ── Google Gemini family ──
    // Gemini 3.x — longer slugs first (gemini-3-flash-lite before gemini-3-flash).
    ModelMetadata {
        slug: "gemini-3-flash-lite",
        context_window: 1_000_000,
        max_context_window: 1_000_000,
        supports_thinking: true,
    },
    ModelMetadata {
        slug: "gemini-3-flash",
        context_window: 1_000_000,
        max_context_window: 1_000_000,
        supports_thinking: true,
    },
    ModelMetadata {
        slug: "gemini-3-pro",
        context_window: 2_000_000,
        max_context_window: 2_000_000,
        supports_thinking: true,
    },
    ModelMetadata {
        slug: "gemini-2.5-pro",
        context_window: 2_000_000,
        max_context_window: 2_000_000,
        supports_thinking: true,
    },
    ModelMetadata {
        slug: "gemini-2.5-flash",
        context_window: 1_000_000,
        max_context_window: 1_000_000,
        supports_thinking: true,
    },
    ModelMetadata {
        slug: "gemini-2.0-flash",
        context_window: 1_000_000,
        max_context_window: 1_000_000,
        supports_thinking: false,
    },
    ModelMetadata {
        slug: "gemini-1.5-pro",
        context_window: 2_000_000,
        max_context_window: 2_000_000,
        supports_thinking: false,
    },
    ModelMetadata {
        slug: "gemini-1.5-flash",
        context_window: 1_000_000,
        max_context_window: 1_000_000,
        supports_thinking: false,
    },
    // ── DeepSeek ──
    // `deepseek-chat` is the stable alias — the underlying weights are V3.2 today.
    ModelMetadata {
        slug: "deepseek-reasoner",
        context_window: 128_000,
        max_context_window: 128_000,
        supports_thinking: true,
    },
    ModelMetadata {
        slug: "deepseek-r1",
        context_window: 64_000,
        max_context_window: 64_000,
        supports_thinking: true,
    },
    ModelMetadata {
        slug: "deepseek-v3.2",
        context_window: 128_000,
        max_context_window: 128_000,
        supports_thinking: false,
    },
    ModelMetadata {
        slug: "deepseek-chat",
        context_window: 128_000,
        max_context_window: 128_000,
        supports_thinking: false,
    },
    // ── Groq (Llama / Mixtral / OSS weights hosted) ──
    ModelMetadata {
        slug: "gpt-oss-120b",
        context_window: 131_072,
        max_context_window: 131_072,
        supports_thinking: true,
    },
    ModelMetadata {
        slug: "gpt-oss-20b",
        context_window: 131_072,
        max_context_window: 131_072,
        supports_thinking: true,
    },
    ModelMetadata {
        slug: "kimi-k2-instruct",
        context_window: 200_000,
        max_context_window: 200_000,
        supports_thinking: false,
    },
    ModelMetadata {
        slug: "llama-4-maverick",
        context_window: 1_000_000,
        max_context_window: 1_000_000,
        supports_thinking: false,
    },
    ModelMetadata {
        slug: "llama-4-scout",
        context_window: 10_000_000,
        max_context_window: 10_000_000,
        supports_thinking: false,
    },
    ModelMetadata {
        slug: "qwen3-32b",
        context_window: 131_072,
        max_context_window: 131_072,
        supports_thinking: true,
    },
    ModelMetadata {
        slug: "deepseek-r1-distill-llama-70b",
        context_window: 128_000,
        max_context_window: 128_000,
        supports_thinking: true,
    },
    ModelMetadata {
        slug: "llama-3.3-70b",
        context_window: 128_000,
        max_context_window: 128_000,
        supports_thinking: false,
    },
    ModelMetadata {
        slug: "llama-3.1-70b",
        context_window: 128_000,
        max_context_window: 128_000,
        supports_thinking: false,
    },
    ModelMetadata {
        slug: "llama-3.1-8b",
        context_window: 128_000,
        max_context_window: 128_000,
        supports_thinking: false,
    },
    ModelMetadata {
        slug: "llama3.3",
        context_window: 128_000,
        max_context_window: 128_000,
        supports_thinking: false,
    },
    ModelMetadata {
        slug: "llama3.1",
        context_window: 128_000,
        max_context_window: 128_000,
        supports_thinking: false,
    },
    ModelMetadata {
        slug: "mixtral-8x7b",
        context_window: 32_768,
        max_context_window: 32_768,
        supports_thinking: false,
    },
    // ── Ollama / smaller local defaults ──
    ModelMetadata {
        slug: "qwen3",
        context_window: 131_072,
        max_context_window: 131_072,
        supports_thinking: true,
    },
    ModelMetadata {
        slug: "qwen2.5",
        context_window: 32_768,
        max_context_window: 32_768,
        supports_thinking: false,
    },
    ModelMetadata {
        slug: "gpt-oss",
        context_window: 131_072,
        max_context_window: 131_072,
        supports_thinking: true,
    },
    ModelMetadata {
        slug: "mistral",
        context_window: 32_768,
        max_context_window: 32_768,
        supports_thinking: false,
    },
    ModelMetadata {
        slug: "phi-3",
        context_window: 4_096,
        max_context_window: 4_096,
        supports_thinking: false,
    },
];

/// Strip a known vendor prefix (e.g. `anthropic/`, `openai/`) before matching.
/// OpenRouter-style slashed names embed the provider; the tail is what models.json-style entries describe.
fn strip_vendor_prefix(model: &str) -> &str {
    for prefix in [
        "anthropic/",
        "openai/",
        "google/",
        "deepseek/",
        "groq/",
        "claude-cli/",
        "ollama/",
        "x-ai/",
        "meta-llama/",
        "mistralai/",
        "qwen/",
    ] {
        if let Some(rest) = model.strip_prefix(prefix) {
            return rest;
        }
    }
    model
}

/// Look up metadata for `model` by longest matching prefix, after stripping
/// any vendor prefix (e.g. `anthropic/claude-sonnet-4` matches `claude-sonnet-4`).
pub fn lookup(model: &str) -> Option<&'static ModelMetadata> {
    let canonical = strip_vendor_prefix(model);
    let lowered = canonical.to_lowercase();
    let mut best: Option<&'static ModelMetadata> = None;
    for entry in REGISTRY {
        if lowered.starts_with(entry.slug) {
            match best {
                Some(b) if b.slug.len() >= entry.slug.len() => {}
                _ => best = Some(entry),
            }
        }
    }
    best
}

/// Resolve the practical context window for `model`, falling back to
/// `DEFAULT_CONTEXT_WINDOW` when unknown. Never returns 0.
pub fn context_window_for(model: &str) -> u32 {
    lookup(model)
        .map(|m| m.context_window)
        .unwrap_or(DEFAULT_CONTEXT_WINDOW)
}

/// Resolve the absolute max context window for `model`, falling back to
/// `DEFAULT_CONTEXT_WINDOW`. For tiered models this exceeds `context_window_for`.
pub fn max_context_window_for(model: &str) -> u32 {
    lookup(model)
        .map(|m| m.max_context_window)
        .unwrap_or(DEFAULT_CONTEXT_WINDOW)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookup_known_models_across_providers() {
        assert_eq!(lookup("claude-sonnet-4").unwrap().slug, "claude-sonnet-4");
        assert_eq!(lookup("gpt-5").unwrap().slug, "gpt-5");
        assert_eq!(lookup("gemini-2.5-pro").unwrap().slug, "gemini-2.5-pro");
        assert_eq!(lookup("deepseek-chat").unwrap().slug, "deepseek-chat");
        assert_eq!(lookup("llama-3.3-70b").unwrap().slug, "llama-3.3-70b");
        assert_eq!(lookup("mixtral-8x7b").unwrap().slug, "mixtral-8x7b");
        assert_eq!(lookup("o1").unwrap().slug, "o1");
    }

    #[test]
    fn prefix_matches_versioned_models() {
        // Versioned Anthropic naming: claude-sonnet-4-20250514
        assert_eq!(
            lookup("claude-sonnet-4-20250514").unwrap().slug,
            "claude-sonnet-4"
        );
        // Versioned GPT-5: gpt-5-2025-08-07
        assert_eq!(lookup("gpt-5-2025-08-07").unwrap().slug, "gpt-5");
        // Versioned Gemini: gemini-2.5-pro-001
        assert_eq!(lookup("gemini-2.5-pro-001").unwrap().slug, "gemini-2.5-pro");
    }

    #[test]
    fn longest_prefix_wins() {
        // gpt-5 and gpt-4.1 both start with "gpt-" — pick the more specific one
        assert_eq!(lookup("gpt-4.1-mini").unwrap().slug, "gpt-4.1");
        assert_eq!(lookup("gpt-4o-mini").unwrap().slug, "gpt-4o");
        // gpt-5-mini is more specific than gpt-5
        assert_eq!(lookup("gpt-5-mini").unwrap().slug, "gpt-5-mini");
        assert_eq!(lookup("gpt-5-nano").unwrap().slug, "gpt-5-nano");
        // o1-mini is more specific than o1
        assert_eq!(lookup("o1-mini").unwrap().slug, "o1-mini");
        assert_eq!(lookup("o1-mini-2024-09-12").unwrap().slug, "o1-mini");
        // bare o1 falls through to the generic o1 entry
        assert_eq!(lookup("o1-2024-12-17").unwrap().slug, "o1");
        // Frontier Anthropic: claude-opus-4-7 beats claude-opus-4
        assert_eq!(lookup("claude-opus-4-7").unwrap().slug, "claude-opus-4-7");
        assert_eq!(
            lookup("claude-sonnet-4-6").unwrap().slug,
            "claude-sonnet-4-6"
        );
        assert_eq!(lookup("claude-haiku-4-5").unwrap().slug, "claude-haiku-4-5");
        // Dated legacy slug still matches the generic family entry
        assert_eq!(
            lookup("claude-sonnet-4-20250514").unwrap().slug,
            "claude-sonnet-4"
        );
        // Gemini 3 flash-lite beats gemini-3-flash
        assert_eq!(
            lookup("gemini-3-flash-lite").unwrap().slug,
            "gemini-3-flash-lite"
        );
        assert_eq!(lookup("gemini-3-flash").unwrap().slug, "gemini-3-flash");
        assert_eq!(lookup("gemini-3-pro").unwrap().slug, "gemini-3-pro");
    }

    #[test]
    fn vendor_prefix_stripped_before_matching() {
        assert_eq!(
            lookup("anthropic/claude-sonnet-4").unwrap().slug,
            "claude-sonnet-4"
        );
        assert_eq!(lookup("openai/gpt-5").unwrap().slug, "gpt-5");
        assert_eq!(
            lookup("google/gemini-2.5-pro").unwrap().slug,
            "gemini-2.5-pro"
        );
        assert_eq!(
            lookup("claude-cli/claude-opus-4").unwrap().slug,
            "claude-opus-4"
        );
    }

    #[test]
    fn case_insensitive_lookup() {
        assert_eq!(lookup("CLAUDE-SONNET-4").unwrap().slug, "claude-sonnet-4");
        assert_eq!(lookup("GPT-5").unwrap().slug, "gpt-5");
    }

    #[test]
    fn unknown_model_returns_none_from_lookup() {
        assert!(lookup("totally-unknown-model").is_none());
        assert!(lookup("").is_none());
    }

    #[test]
    fn context_window_for_unknown_returns_default() {
        assert_eq!(
            context_window_for("totally-unknown"),
            DEFAULT_CONTEXT_WINDOW
        );
        assert_eq!(context_window_for(""), DEFAULT_CONTEXT_WINDOW);
        // Default must be non-zero so downstream budget calcs don't divide by 0.
        assert!(DEFAULT_CONTEXT_WINDOW > 0);
    }

    #[test]
    fn context_window_for_known_returns_real_value() {
        assert_eq!(context_window_for("claude-sonnet-4-20250514"), 200_000);
        assert_eq!(context_window_for("gpt-5"), 272_000);
        assert_eq!(context_window_for("gemini-2.5-pro"), 2_000_000);
        assert_eq!(context_window_for("deepseek-chat"), 128_000);
    }

    #[test]
    fn max_context_window_meets_or_exceeds_practical() {
        // GPT-5 has a tiered max above its practical window
        assert_eq!(max_context_window_for("gpt-5"), 1_000_000);
        // Claude Sonnet 4 has the 1M tier
        assert_eq!(max_context_window_for("claude-sonnet-4"), 1_000_000);
        // Smaller models have equal practical and max
        let chat = lookup("deepseek-chat").unwrap();
        assert_eq!(chat.context_window, chat.max_context_window);
    }

    #[test]
    fn all_registry_entries_have_sane_bounds() {
        for entry in REGISTRY {
            assert!(
                !entry.slug.is_empty(),
                "registry entry has empty slug: {entry:?}"
            );
            assert!(
                entry.context_window > 0,
                "{} has zero context_window",
                entry.slug
            );
            assert!(
                entry.max_context_window > 0,
                "{} has zero max_context_window",
                entry.slug
            );
            assert!(
                entry.context_window <= entry.max_context_window,
                "{} has context_window > max_context_window",
                entry.slug
            );
        }
    }

    #[test]
    fn registry_has_no_duplicate_slugs() {
        let mut seen = std::collections::HashSet::new();
        for entry in REGISTRY {
            assert!(
                seen.insert(entry.slug),
                "duplicate slug in registry: {}",
                entry.slug
            );
        }
    }
}
