use std::fmt;
use std::net::SocketAddr;
use std::str::FromStr;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use reqwest::header::{HeaderMap, HeaderValue};

/// Supported LLM providers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    OpenRouter,
    OpenAi,
    Anthropic,
    Gemini,
    DeepSeek,
    Groq,
    Ollama,
    /// Claude Code CLI — uses the user's Claude subscription via subprocess.
    ClaudeCli,
}

/// Priority order for cloud API key auto-detection.
/// Ollama is excluded — it's detected separately via TCP probe.
const DETECT_ORDER: &[Provider] = &[
    Provider::OpenRouter,
    Provider::OpenAi,
    Provider::Anthropic,
    Provider::Gemini,
    Provider::DeepSeek,
    Provider::Groq,
];

impl Provider {
    /// API base URL for this provider.
    pub fn base_url(&self) -> &'static str {
        match self {
            Provider::OpenRouter => "https://openrouter.ai/api/v1/chat/completions",
            Provider::OpenAi => "https://api.openai.com/v1/chat/completions",
            Provider::Anthropic => "https://api.anthropic.com/v1/messages",
            Provider::Gemini => {
                "https://generativelanguage.googleapis.com/v1beta/openai/chat/completions"
            }
            Provider::DeepSeek => "https://api.deepseek.com/v1/chat/completions",
            Provider::Groq => "https://api.groq.com/openai/v1/chat/completions",
            Provider::Ollama => "http://localhost:11434/v1/chat/completions",
            Provider::ClaudeCli => "", // Not used — communicates via subprocess
        }
    }

    /// Default environment variable name for this provider's API key (or host).
    pub fn default_env_var(&self) -> &'static str {
        match self {
            Provider::OpenRouter => "OPENROUTER_API_KEY",
            Provider::OpenAi => "OPENAI_API_KEY",
            Provider::Anthropic => "ANTHROPIC_API_KEY",
            Provider::Gemini => "GEMINI_API_KEY",
            Provider::DeepSeek => "DEEPSEEK_API_KEY",
            Provider::Groq => "GROQ_API_KEY",
            Provider::Ollama => "OLLAMA_HOST",
            Provider::ClaudeCli => "CLAUDE_CLI_PATH", // Optional override for claude binary path
        }
    }

    /// Whether this provider requires an API key.
    pub fn requires_api_key(&self) -> bool {
        !matches!(self, Provider::Ollama | Provider::ClaudeCli)
    }

    /// Whether this provider uses the OpenAI-compatible chat completions format.
    pub fn is_openai_compatible(&self) -> bool {
        !matches!(self, Provider::Anthropic | Provider::ClaudeCli)
    }

    /// Build provider-specific HTTP headers.
    pub fn build_headers(&self, api_key: &str) -> Result<HeaderMap> {
        let mut headers = HeaderMap::new();
        headers.insert("Content-Type", HeaderValue::from_static("application/json"));

        match self {
            Provider::Ollama | Provider::ClaudeCli => {
                // No authentication needed for local providers
            }
            Provider::OpenRouter => {
                let bearer = format!("Bearer {api_key}");
                let val = HeaderValue::from_str(&bearer)
                    .context("API key contains invalid characters for HTTP headers")?;
                headers.insert("Authorization", val);
                headers.insert(
                    "HTTP-Referer",
                    HeaderValue::from_static("https://github.com/borg-ai"),
                );
                headers.insert("X-Title", HeaderValue::from_static("Borg AI Assistant"));
            }
            Provider::Anthropic => {
                let val = HeaderValue::from_str(api_key)
                    .context("API key contains invalid characters for HTTP headers")?;
                headers.insert("x-api-key", val);
                headers.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
            }
            Provider::OpenAi | Provider::Gemini | Provider::DeepSeek | Provider::Groq => {
                let bearer = format!("Bearer {api_key}");
                let val = HeaderValue::from_str(&bearer)
                    .context("API key contains invalid characters for HTTP headers")?;
                headers.insert("Authorization", val);
            }
        }

        Ok(headers)
    }

    /// Auto-detect provider from environment variables. Checks cloud providers first,
    /// then falls back to Ollama if reachable locally.
    pub fn detect_from_env() -> Result<(Provider, String)> {
        for provider in DETECT_ORDER {
            if let Ok(key) = std::env::var(provider.default_env_var()) {
                if !key.is_empty() {
                    return Ok((*provider, key));
                }
            }
        }

        // Check for Claude CLI with valid OAuth auth
        if crate::claude_cli::has_valid_auth() {
            return Ok((Provider::ClaudeCli, String::new()));
        }

        // Fall back to Ollama if running locally
        if Provider::ollama_available() {
            return Ok((Provider::Ollama, String::new()));
        }

        bail!(
            "No API key found. Set one of: {}, or run `ollama serve` for local inference, or install Claude Code for subscription access",
            DETECT_ORDER
                .iter()
                .map(Provider::default_env_var)
                .collect::<Vec<_>>()
                .join(", ")
        )
    }

    /// Strip vendor prefixes for direct providers (e.g., "anthropic/claude-sonnet-4" → "claude-sonnet-4").
    pub fn normalize_model(&self, model: &str) -> String {
        match self {
            Provider::OpenRouter => model.to_string(),
            Provider::OpenAi => model.strip_prefix("openai/").unwrap_or(model).to_string(),
            Provider::Anthropic => model
                .strip_prefix("anthropic/")
                .unwrap_or(model)
                .to_string(),
            Provider::Gemini => model.strip_prefix("google/").unwrap_or(model).to_string(),
            Provider::DeepSeek => model.strip_prefix("deepseek/").unwrap_or(model).to_string(),
            Provider::Groq => model.strip_prefix("groq/").unwrap_or(model).to_string(),
            Provider::Ollama => model.to_string(),
            Provider::ClaudeCli => model
                .strip_prefix("claude-cli/")
                .unwrap_or(model)
                .to_string(),
        }
    }

    /// Whether a model supports tool/function calling.
    ///
    /// Defaults to `true` for most models. Blocklists known reasoning-only models
    /// that lack tool use support (e.g., DeepSeek R1, QwQ).
    pub fn supports_tools(&self, model: &str) -> bool {
        let m = model.to_lowercase();
        // Reasoning-only models that don't support tool calling
        if m.contains("deepseek-r1")
            || m.contains("deepseek/deepseek-r1")
            || m.contains("deepseek-reasoner")
            || m.contains("qwq")
        {
            return false;
        }
        // o1-mini and o1-preview don't support tools (o1 full does)
        if (m.contains("o1-mini") || m.contains("o1-preview")) && !m.contains("o1-mini-2") {
            return false;
        }
        true
    }

    /// Whether a model supports vision (image inputs).
    ///
    /// Defaults to `true` for providers where most modern models support vision,
    /// only blocklisting known text-only models. This avoids false negatives
    /// that would silently downgrade multimodal messages.
    pub fn supports_vision(&self, model: &str) -> bool {
        let m = model.to_lowercase();
        match self {
            // All Claude 3+ models support vision; only older/embedding models don't
            Provider::Anthropic => !m.contains("claude-2") && !m.contains("claude-instant"),
            // Most modern OpenAI models support vision
            Provider::OpenAi => {
                !m.contains("gpt-3.5")
                    && !m.contains("davinci")
                    && !m.contains("babbage")
                    && !m.contains("ada")
                    && !m.contains("whisper")
                    && !m.contains("tts")
                    && !m.contains("dall-e")
                    && !m.contains("embedding")
            }
            Provider::Gemini => true,
            // For OpenRouter, default to true; let the underlying model reject if unsupported
            Provider::OpenRouter => true,
            // DeepSeek: only VL models support vision
            Provider::DeepSeek => m.contains("vl"),
            // Groq: only known vision models
            Provider::Groq => m.contains("vision") || m.contains("llava"),
            // Ollama: only known vision models
            Provider::Ollama => {
                m.contains("llava") || m.contains("vision") || m.contains("moondream")
            }
            // Claude CLI: same as Anthropic — all Claude 3+ models support vision
            Provider::ClaudeCli => !m.contains("claude-2") && !m.contains("claude-instant"),
        }
    }

    /// String identifier for config serialization.
    pub fn as_str(&self) -> &'static str {
        match self {
            Provider::OpenRouter => "openrouter",
            Provider::OpenAi => "openai",
            Provider::Anthropic => "anthropic",
            Provider::Gemini => "gemini",
            Provider::DeepSeek => "deepseek",
            Provider::Groq => "groq",
            Provider::Ollama => "ollama",
            Provider::ClaudeCli => "claude-cli",
        }
    }

    /// Infer provider from an environment variable name.
    /// Returns `None` for unrecognized variable names.
    pub fn from_env_var_name(name: &str) -> Option<Provider> {
        match name {
            "OPENROUTER_API_KEY" => Some(Provider::OpenRouter),
            "OPENAI_API_KEY" => Some(Provider::OpenAi),
            "ANTHROPIC_API_KEY" => Some(Provider::Anthropic),
            "GEMINI_API_KEY" => Some(Provider::Gemini),
            "DEEPSEEK_API_KEY" => Some(Provider::DeepSeek),
            "GROQ_API_KEY" => Some(Provider::Groq),
            "OLLAMA_HOST" => Some(Provider::Ollama),
            "CLAUDE_CLI_PATH" => Some(Provider::ClaudeCli),
            _ => None,
        }
    }

    /// Check if Ollama is reachable (sync, short timeout).
    pub fn ollama_available() -> bool {
        let default_addr: SocketAddr = SocketAddr::from(([127, 0, 0, 1], 11434));
        let addr: SocketAddr = std::env::var("OLLAMA_HOST")
            .ok()
            .and_then(|h| {
                // OLLAMA_HOST can be a URL like "http://host:port" or just "host:port"
                let stripped = h
                    .strip_prefix("http://")
                    .or_else(|| h.strip_prefix("https://"))
                    .unwrap_or(&h);
                // Remove any path component
                let host_port = stripped.split('/').next().unwrap_or(stripped);
                host_port.parse().ok()
            })
            .unwrap_or(default_addr);
        std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_ok()
    }
}

impl fmt::Display for Provider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Provider {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s.to_lowercase().as_str() {
            "openrouter" => Ok(Provider::OpenRouter),
            "openai" => Ok(Provider::OpenAi),
            "anthropic" => Ok(Provider::Anthropic),
            "gemini" => Ok(Provider::Gemini),
            "deepseek" => Ok(Provider::DeepSeek),
            "groq" => Ok(Provider::Groq),
            "ollama" => Ok(Provider::Ollama),
            "claude-cli" | "claudecli" | "claude_cli" => Ok(Provider::ClaudeCli),
            _ => {
                bail!("Unknown provider: {s}. Valid options: openrouter, openai, anthropic, gemini, deepseek, groq, ollama, claude-cli")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_str_round_trip() {
        for provider in DETECT_ORDER {
            let s = provider.as_str();
            let parsed = Provider::from_str(s).unwrap();
            assert_eq!(parsed, *provider);
        }
        // Ollama is not in DETECT_ORDER, test separately
        assert_eq!(
            Provider::from_str(Provider::Ollama.as_str()).unwrap(),
            Provider::Ollama
        );
    }

    #[test]
    fn from_str_case_insensitive() {
        assert_eq!(
            Provider::from_str("OpenRouter").unwrap(),
            Provider::OpenRouter
        );
        assert_eq!(
            Provider::from_str("ANTHROPIC").unwrap(),
            Provider::Anthropic
        );
        assert_eq!(Provider::from_str("Gemini").unwrap(), Provider::Gemini);
        assert_eq!(Provider::from_str("DeepSeek").unwrap(), Provider::DeepSeek);
        assert_eq!(Provider::from_str("DEEPSEEK").unwrap(), Provider::DeepSeek);
        assert_eq!(Provider::from_str("Groq").unwrap(), Provider::Groq);
        assert_eq!(Provider::from_str("GROQ").unwrap(), Provider::Groq);
        assert_eq!(Provider::from_str("Ollama").unwrap(), Provider::Ollama);
        assert_eq!(Provider::from_str("OLLAMA").unwrap(), Provider::Ollama);
    }

    #[test]
    fn from_str_unknown_errors() {
        assert!(Provider::from_str("unknown").is_err());
    }

    // Note: detect_from_env tests are omitted because std::env::set_var/remove_var
    // is not thread-safe and causes flaky failures in parallel test execution.
    // The detection logic is simple enough to verify by inspection, and is covered
    // by integration testing (setting env vars and running the binary).

    #[test]
    fn normalize_model_strips_prefix() {
        assert_eq!(
            Provider::OpenAi.normalize_model("openai/gpt-4.1"),
            "gpt-4.1"
        );
        assert_eq!(
            Provider::Anthropic.normalize_model("anthropic/claude-sonnet-4"),
            "claude-sonnet-4"
        );
        assert_eq!(
            Provider::Gemini.normalize_model("google/gemini-2.5-pro"),
            "gemini-2.5-pro"
        );
        assert_eq!(
            Provider::DeepSeek.normalize_model("deepseek/deepseek-chat"),
            "deepseek-chat"
        );
        assert_eq!(
            Provider::Groq.normalize_model("groq/llama-3.3-70b-versatile"),
            "llama-3.3-70b-versatile"
        );
        // OpenRouter keeps the full path
        assert_eq!(
            Provider::OpenRouter.normalize_model("anthropic/claude-sonnet-4"),
            "anthropic/claude-sonnet-4"
        );
        // Ollama passes through as-is
        assert_eq!(Provider::Ollama.normalize_model("llama3.3"), "llama3.3");
    }

    #[test]
    fn normalize_model_no_prefix_passthrough() {
        assert_eq!(Provider::OpenAi.normalize_model("gpt-4.1"), "gpt-4.1");
        assert_eq!(
            Provider::Anthropic.normalize_model("claude-sonnet-4"),
            "claude-sonnet-4"
        );
        assert_eq!(
            Provider::DeepSeek.normalize_model("deepseek-chat"),
            "deepseek-chat"
        );
        assert_eq!(
            Provider::Groq.normalize_model("llama-3.3-70b-versatile"),
            "llama-3.3-70b-versatile"
        );
    }

    #[test]
    fn is_openai_compatible() {
        assert!(Provider::OpenRouter.is_openai_compatible());
        assert!(Provider::OpenAi.is_openai_compatible());
        assert!(Provider::Gemini.is_openai_compatible());
        assert!(Provider::DeepSeek.is_openai_compatible());
        assert!(Provider::Groq.is_openai_compatible());
        assert!(Provider::Ollama.is_openai_compatible());
        assert!(!Provider::Anthropic.is_openai_compatible());
    }

    #[test]
    fn build_headers_openrouter() {
        let headers = Provider::OpenRouter.build_headers("sk-test").unwrap();
        assert!(headers
            .get("Authorization")
            .unwrap()
            .to_str()
            .unwrap()
            .contains("Bearer"));
        assert!(headers.get("HTTP-Referer").is_some());
        assert!(headers.get("X-Title").is_some());
    }

    #[test]
    fn build_headers_anthropic() {
        let headers = Provider::Anthropic.build_headers("sk-test").unwrap();
        assert_eq!(
            headers.get("x-api-key").unwrap().to_str().unwrap(),
            "sk-test"
        );
        assert!(headers.get("anthropic-version").is_some());
        assert!(headers.get("Authorization").is_none());
    }

    #[test]
    fn build_headers_openai() {
        let headers = Provider::OpenAi.build_headers("sk-test").unwrap();
        assert!(headers
            .get("Authorization")
            .unwrap()
            .to_str()
            .unwrap()
            .contains("Bearer"));
        assert!(headers.get("x-api-key").is_none());
    }

    #[test]
    fn build_headers_ollama_no_auth() {
        let headers = Provider::Ollama.build_headers("").unwrap();
        assert!(headers.get("Authorization").is_none());
        assert!(headers.get("x-api-key").is_none());
        assert_eq!(
            headers.get("Content-Type").unwrap().to_str().unwrap(),
            "application/json"
        );
    }

    #[test]
    fn ollama_base_url() {
        assert_eq!(
            Provider::Ollama.base_url(),
            "http://localhost:11434/v1/chat/completions"
        );
    }

    #[test]
    fn ollama_default_env_var() {
        assert_eq!(Provider::Ollama.default_env_var(), "OLLAMA_HOST");
    }

    #[test]
    fn requires_api_key_cloud_vs_local() {
        assert!(Provider::OpenRouter.requires_api_key());
        assert!(Provider::OpenAi.requires_api_key());
        assert!(Provider::Anthropic.requires_api_key());
        assert!(Provider::Gemini.requires_api_key());
        assert!(Provider::DeepSeek.requires_api_key());
        assert!(Provider::Groq.requires_api_key());
        assert!(!Provider::Ollama.requires_api_key());
    }

    #[test]
    fn build_headers_deepseek_bearer() {
        let headers = Provider::DeepSeek.build_headers("sk-test").unwrap();
        assert!(headers
            .get("Authorization")
            .unwrap()
            .to_str()
            .unwrap()
            .contains("Bearer"));
    }

    #[test]
    fn build_headers_groq_bearer() {
        let headers = Provider::Groq.build_headers("gsk-test").unwrap();
        assert!(headers
            .get("Authorization")
            .unwrap()
            .to_str()
            .unwrap()
            .contains("Bearer"));
    }

    #[test]
    fn supports_tools_reasoning_models() {
        // DeepSeek R1 variants — no tool support
        assert!(!Provider::OpenRouter.supports_tools("deepseek/deepseek-r1"));
        assert!(!Provider::OpenRouter.supports_tools("deepseek/deepseek-r1:free"));
        assert!(!Provider::DeepSeek.supports_tools("deepseek-reasoner"));
        assert!(!Provider::DeepSeek.supports_tools("deepseek-r1"));

        // QwQ — no tool support
        assert!(!Provider::OpenRouter.supports_tools("qwen/qwq-32b"));

        // o1-mini/preview — no tool support
        assert!(!Provider::OpenAi.supports_tools("o1-mini"));
        assert!(!Provider::OpenAi.supports_tools("o1-preview"));

        // Regular models — tool support
        assert!(Provider::OpenRouter.supports_tools("deepseek/deepseek-chat-v3-0324"));
        assert!(Provider::DeepSeek.supports_tools("deepseek-chat"));
        assert!(Provider::OpenAi.supports_tools("gpt-4.1"));
        assert!(Provider::Anthropic.supports_tools("claude-sonnet-4"));
        assert!(Provider::OpenRouter.supports_tools("anthropic/claude-sonnet-4"));
        assert!(Provider::Ollama.supports_tools("llama3.3"));
        // o1 full supports tools
        assert!(Provider::OpenAi.supports_tools("o1"));
    }

    #[test]
    fn deepseek_supports_vision() {
        assert!(Provider::DeepSeek.supports_vision("deepseek-vl-7b"));
        assert!(!Provider::DeepSeek.supports_vision("deepseek-chat"));
        assert!(!Provider::DeepSeek.supports_vision("deepseek-reasoner"));
    }

    #[test]
    fn groq_supports_vision() {
        assert!(Provider::Groq.supports_vision("llava-v1.5-7b-4096-preview"));
        assert!(Provider::Groq.supports_vision("some-vision-model"));
        assert!(!Provider::Groq.supports_vision("llama-3.3-70b-versatile"));
        assert!(!Provider::Groq.supports_vision("mixtral-8x7b-32768"));
    }

    #[test]
    fn from_env_var_name_round_trip() {
        for provider in DETECT_ORDER {
            let env_var = provider.default_env_var();
            assert_eq!(Provider::from_env_var_name(env_var), Some(*provider));
        }
        assert_eq!(
            Provider::from_env_var_name("OLLAMA_HOST"),
            Some(Provider::Ollama)
        );
        assert_eq!(Provider::from_env_var_name("UNKNOWN_KEY"), None);
    }

    #[test]
    fn ollama_supports_vision() {
        assert!(Provider::Ollama.supports_vision("llava"));
        assert!(Provider::Ollama.supports_vision("llama3.2-vision"));
        assert!(Provider::Ollama.supports_vision("some-vision-model"));
        assert!(Provider::Ollama.supports_vision("moondream"));
        assert!(!Provider::Ollama.supports_vision("llama3.3"));
        assert!(!Provider::Ollama.supports_vision("mistral"));
    }

    // ── ClaudeCli tests ──

    #[test]
    fn claude_cli_from_str() {
        assert_eq!(
            Provider::from_str("claude-cli").unwrap(),
            Provider::ClaudeCli
        );
        assert_eq!(
            Provider::from_str("claudecli").unwrap(),
            Provider::ClaudeCli
        );
        assert_eq!(
            Provider::from_str("claude_cli").unwrap(),
            Provider::ClaudeCli
        );
        assert_eq!(
            Provider::from_str("Claude-CLI").unwrap(),
            Provider::ClaudeCli
        );
    }

    #[test]
    fn claude_cli_round_trip() {
        let s = Provider::ClaudeCli.as_str();
        assert_eq!(s, "claude-cli");
        assert_eq!(Provider::from_str(s).unwrap(), Provider::ClaudeCli);
    }

    #[test]
    fn claude_cli_no_api_key_required() {
        assert!(!Provider::ClaudeCli.requires_api_key());
    }

    #[test]
    fn claude_cli_not_openai_compatible() {
        assert!(!Provider::ClaudeCli.is_openai_compatible());
    }

    #[test]
    fn claude_cli_normalize_model() {
        assert_eq!(
            Provider::ClaudeCli.normalize_model("claude-cli/claude-sonnet-4-6"),
            "claude-sonnet-4-6"
        );
        assert_eq!(
            Provider::ClaudeCli.normalize_model("claude-sonnet-4-6"),
            "claude-sonnet-4-6"
        );
    }

    #[test]
    fn claude_cli_supports_vision() {
        assert!(Provider::ClaudeCli.supports_vision("claude-sonnet-4-6"));
        assert!(Provider::ClaudeCli.supports_vision("claude-opus-4-6"));
        assert!(!Provider::ClaudeCli.supports_vision("claude-2"));
    }

    #[test]
    fn claude_cli_base_url_is_empty() {
        assert!(Provider::ClaudeCli.base_url().is_empty());
    }

    #[test]
    fn claude_cli_build_headers_no_auth() {
        let headers = Provider::ClaudeCli.build_headers("").unwrap();
        assert!(headers.get("Authorization").is_none());
        assert!(headers.get("x-api-key").is_none());
    }

    #[test]
    fn claude_cli_from_env_var_name() {
        assert_eq!(
            Provider::from_env_var_name("CLAUDE_CLI_PATH"),
            Some(Provider::ClaudeCli)
        );
    }
}
