//! Model tiering for workflow auto-enable.
//!
//! All Claude models (Anthropic native and via OpenRouter) are considered strong
//! enough to handle complex multi-step tasks without workflow scaffolding.
//! All other models get workflows enabled by default.
//! Users can override via `workflow.enabled = "on"` or `"off"` in settings.

/// Model prefixes that do NOT need workflow scaffolding.
/// All Claude models are exempt — they reliably manage long-running
/// multi-step tasks without explicit structure.
const NO_WORKFLOW_PREFIXES: &[&str] = &["claude-", "anthropic/claude-"];

/// Returns `true` if the given model needs workflow scaffolding.
///
/// All Claude models return `false`. Everything else — GPT, open-source
/// models, unknown models — returns `true`.
pub fn model_needs_workflows(model: &str) -> bool {
    let lower = model.to_lowercase();

    for prefix in NO_WORKFLOW_PREFIXES {
        if lower.starts_with(prefix) {
            return false;
        }
    }

    // All other models benefit from workflow scaffolding
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_claude_models_no_workflows() {
        let models = [
            "claude-opus-4",
            "claude-opus-4-20250514",
            "claude-sonnet-4",
            "claude-sonnet-4-20250514",
            "claude-haiku-4",
            "claude-haiku-4-5",
            "claude-3.5-sonnet",
            "claude-3.5-haiku",
            "claude-sonnet-4-6",
            "claude-opus-4-6",
            "anthropic/claude-opus-4",
            "anthropic/claude-opus-4-20250514",
            "anthropic/claude-sonnet-4",
            "anthropic/claude-haiku-4",
        ];
        for model in models {
            assert!(
                !model_needs_workflows(model),
                "Expected {model} to NOT need workflows (Claude model)"
            );
        }
    }

    #[test]
    fn test_openai_models_need_workflows() {
        let models = [
            "gpt-4o",
            "gpt-4.1",
            "gpt-4o-mini",
            "o3",
            "o4-mini",
            "openai/gpt-4o",
        ];
        for model in models {
            assert!(
                model_needs_workflows(model),
                "Expected {model} to need workflows"
            );
        }
    }

    #[test]
    fn test_open_source_models_need_workflows() {
        let models = [
            "llama-3.3-70b",
            "mistral-large",
            "qwen-2.5-72b",
            "deepseek-v3",
            "kimi-k2.5",
            "phi-4",
            "gemma-3-27b",
        ];
        for model in models {
            assert!(
                model_needs_workflows(model),
                "Expected {model} to need workflows"
            );
        }
    }

    #[test]
    fn test_unknown_model_defaults_to_workflows() {
        assert!(model_needs_workflows("some-random-model-v2"));
        assert!(model_needs_workflows("custom-finetune-7b"));
        assert!(model_needs_workflows(""));
    }

    #[test]
    fn test_case_insensitive() {
        assert!(!model_needs_workflows("Claude-Opus-4"));
        assert!(!model_needs_workflows("Claude-Sonnet-4"));
        assert!(!model_needs_workflows("Claude-Haiku-4"));
        assert!(model_needs_workflows("GPT-4O"));
    }
}
