//! Resolver for the `[llm.aux] model = "auto"` sentinel.
//!
//! Maps each remote provider to a known-good "cheap reliable sibling" model
//! id. Onboarding writes `model = "auto"` into the aux block; at aux-client
//! construction time `LlmClient::for_auxiliary` calls [`pick_aux_model`] to
//! materialize the sentinel into a concrete model id before the request
//! leaves the daemon.
//!
//! A hardcoded provider → model map, refreshed via PR when a provider
//! deprecates a model. Same release cadence as the slugs in
//! [`crate::llm::model_registry`].
//!
//! Local providers (`ollama`, `claude-cli`) and unknown provider strings return
//! `None` — the caller falls through to the primary chain rather than surface
//! an error. Auxiliary failover must never block the daemon at boot.

/// Hardcoded provider → "cheap reliable sibling" map. Keep entries terse and
/// match real provider id strings exactly (case-sensitive).
const AUX_DEFAULTS: &[(&str, &str)] = &[
    ("anthropic", "claude-haiku-4-5"),
    ("openai", "gpt-5-mini"),
    ("openrouter", "anthropic/claude-haiku-4-5"),
    ("gemini", "gemini-3-flash-lite"),
    ("deepseek", "deepseek-v4-flash"),
    ("groq", "llama-3.3-70b-versatile"),
    ("xai", "grok-4-fast-non-reasoning"),
];

/// Resolve the auxiliary model id for a given provider, or `None` when the
/// provider has no preconfigured aux default (e.g. `ollama`, `claude-cli`,
/// custom user-defined providers). `None` causes the caller to fall through
/// to the primary chain.
pub fn pick_aux_model(provider: &str) -> Option<&'static str> {
    AUX_DEFAULTS
        .iter()
        .find(|(p, _)| *p == provider)
        .map(|(_, m)| *m)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_known_good_for_each_remote_provider() {
        // Table-driven coverage invariant: every remote provider Borg ships
        // a transport for must have an aux default. If a new provider lands
        // and AUX_DEFAULTS isn't updated, this test fails before the resolver
        // silently dead-ends background tasks.
        for provider in [
            "anthropic",
            "openai",
            "openrouter",
            "gemini",
            "deepseek",
            "groq",
            "xai",
        ] {
            let resolved = pick_aux_model(provider);
            assert!(
                resolved.is_some(),
                "{provider} has no aux default — add it to AUX_DEFAULTS"
            );
            assert!(
                !resolved.unwrap().is_empty(),
                "{provider} aux model is empty string"
            );
        }
    }

    #[test]
    fn returns_none_for_local_providers() {
        // Local providers (Ollama, Claude CLI) deliberately have no aux
        // default — they don't speak `/v1/models` and the user is
        // self-hosting, so falling through to the primary chain is the
        // right behavior. Asserts the soft-fail invariant.
        assert_eq!(pick_aux_model("ollama"), None);
        assert_eq!(pick_aux_model("claude-cli"), None);
    }

    #[test]
    fn returns_none_for_unknown_provider() {
        // A typo'd or user-defined provider id falls through silently rather
        // than raising — auxiliary failover must never block the daemon.
        assert_eq!(pick_aux_model("madeup-provider"), None);
        assert_eq!(pick_aux_model(""), None);
    }

    #[test]
    fn case_sensitive_provider_match() {
        // Provider strings are stored case-sensitively in config; the
        // resolver mirrors that. "Anthropic" must NOT match "anthropic"
        // — better to fall through than silently route an unintended
        // canonicalization.
        assert_eq!(pick_aux_model("Anthropic"), None);
        assert_eq!(pick_aux_model("OPENAI"), None);
    }
}
