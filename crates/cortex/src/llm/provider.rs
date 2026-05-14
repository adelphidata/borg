use std::fmt;
use std::net::SocketAddr;
use std::str::FromStr;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use reqwest::header::{HeaderMap, HeaderValue};

/// Supported LLM providers.
///
/// The four OAuth/Custom variants below stay unit-shaped so `Provider` remains
/// `Copy`. Per-credential overrides (the `base_url` for `Custom`, the OAuth
/// account label) live on the `PooledCredential` row, not on the enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    OpenRouter,
    OpenAi,
    Anthropic,
    Bedrock,
    Gemini,
    DeepSeek,
    Groq,
    /// xAI (Grok) — OpenAI-compatible chat-completions API.
    XAi,
    Ollama,
    /// Claude Code CLI — uses the user's Claude subscription via subprocess.
    ClaudeCli,
    /// Anthropic API with a Claude Code OAuth access token (Bearer auth).
    /// Same wire format as `Anthropic`, swapped auth header.
    ClaudeCodeOAuth,
    /// OpenAI ChatGPT-Codex OAuth — OpenAI wire format with Bearer auth from
    /// the Codex CLI's stored token.
    CodexOAuth,
    /// GitHub Copilot — OpenAI-compatible wire format with the Copilot
    /// integration headers attached. Token sourced from `gh auth token`.
    CopilotOAuth,
    /// User-defined OpenAI-compatible endpoint. `base_url()` returns "" — the
    /// concrete URL is read from `PooledCredential.base_url` at request time.
    Custom,
    /// Azure OpenAI Service — deployment-name-scoped OpenAI on Microsoft
    /// tenancy. Wire body is OpenAI ChatCompletions; the URL is composed
    /// from `llm.azure_resource` / `llm.azure_deployment` /
    /// `llm.azure_api_version` at request time, and the auth header is
    /// `api-key: <key>` for API-key auth or `Authorization: Bearer <token>`
    /// for AAD (Entra ID) tokens. The auth mode is signaled by the
    /// `PooledCredential.auth_type` field (`ApiKey` vs `OAuth`).
    AzureOpenAi,
    /// Google Vertex AI — service-account / workload identity access to
    /// Gemini and partner Anthropic models.
    VertexAi,
}

impl Provider {
    /// How this provider obtains request credentials.
    pub fn auth_kind(&self) -> ProviderAuthKind {
        match self {
            Provider::Ollama | Provider::ClaudeCli => ProviderAuthKind::None,
            Provider::Bedrock => ProviderAuthKind::AwsCredentialChain,
            Provider::VertexAi => ProviderAuthKind::CloudOAuth,
            Provider::OpenRouter
            | Provider::OpenAi
            | Provider::Anthropic
            | Provider::Gemini
            | Provider::DeepSeek
            | Provider::Groq
            | Provider::XAi
            | Provider::ClaudeCodeOAuth
            | Provider::CodexOAuth
            | Provider::CopilotOAuth
            | Provider::Custom
            | Provider::AzureOpenAi => ProviderAuthKind::ApiKeyLike,
        }
    }

    /// API base URL for this provider.
    pub fn base_url(&self) -> &'static str {
        match self {
            Provider::OpenRouter => "https://openrouter.ai/api/v1/chat/completions",
            Provider::OpenAi => "https://api.openai.com/v1/chat/completions",
            Provider::Anthropic => "https://api.anthropic.com/v1/messages",
            Provider::Bedrock => "",
            Provider::Gemini => {
                "https://generativelanguage.googleapis.com/v1beta/openai/chat/completions"
            }
            Provider::DeepSeek => "https://api.deepseek.com/v1/chat/completions",
            Provider::Groq => "https://api.groq.com/openai/v1/chat/completions",
            Provider::XAi => "https://api.x.ai/v1/chat/completions",
            Provider::Ollama => "http://localhost:11434/v1/chat/completions",
            Provider::ClaudeCli => "", // Not used — communicates via subprocess
            // OAuth flavors hit the same wire endpoints as their API-key
            // counterparts; only the auth header changes.
            Provider::ClaudeCodeOAuth => "https://api.anthropic.com/v1/messages",
            Provider::CodexOAuth => "https://api.openai.com/v1/chat/completions",
            Provider::CopilotOAuth => "https://api.githubcopilot.com/chat/completions",
            // Custom resolves base_url from the lease at request time.
            Provider::Custom => "",
            // Azure composes its URL from llm.azure_* settings; see
            // `LlmClient::effective_base_url`. Empty here means a caller that
            // forgets to compose will fail loudly on URL parse instead of
            // silently using a wrong host.
            Provider::AzureOpenAi => "",
            Provider::VertexAi => "",
        }
    }

    /// Default environment variable name for this provider's API key (or host).
    pub fn default_env_var(&self) -> &'static str {
        match self {
            Provider::OpenRouter => "OPENROUTER_API_KEY",
            Provider::OpenAi => "OPENAI_API_KEY",
            Provider::Anthropic => "ANTHROPIC_API_KEY",
            Provider::Bedrock => "",
            Provider::Gemini => "GEMINI_API_KEY",
            Provider::DeepSeek => "DEEPSEEK_API_KEY",
            Provider::Groq => "GROQ_API_KEY",
            Provider::XAi => "XAI_API_KEY",
            Provider::Ollama => "OLLAMA_HOST",
            Provider::ClaudeCli => "CLAUDE_CLI_PATH", // Optional override for claude binary path
            // OAuth + Custom credentials seed via on-disk discoverers, not
            // env vars. Empty here disables the env-seed code path.
            Provider::ClaudeCodeOAuth
            | Provider::CodexOAuth
            | Provider::CopilotOAuth
            | Provider::Custom => "",
            Provider::AzureOpenAi => "AZURE_OPENAI_API_KEY",
            Provider::VertexAi => "",
        }
    }

    /// Whether this provider requires an API key.
    pub fn requires_api_key(&self) -> bool {
        self.auth_kind() == ProviderAuthKind::ApiKeyLike
    }

    /// Whether this provider uses the AWS default credential chain.
    pub fn uses_aws_credential_chain(&self) -> bool {
        self.auth_kind() == ProviderAuthKind::AwsCredentialChain
    }

    /// Whether this provider can obtain short-lived cloud OAuth tokens without
    /// a static API key.
    pub fn supports_cloud_auth(&self) -> bool {
        matches!(self, Provider::AzureOpenAi | Provider::VertexAi)
    }

    /// Whether this provider participates in the API-key credential pool.
    pub fn supports_credential_pool(&self) -> bool {
        self.auth_kind() == ProviderAuthKind::ApiKeyLike
    }

    /// Whether this provider uses the OpenAI-compatible chat completions format.
    pub fn is_openai_compatible(&self) -> bool {
        !matches!(
            self,
            Provider::Anthropic
                | Provider::Bedrock
                | Provider::ClaudeCli
                | Provider::ClaudeCodeOAuth
        )
    }

    /// Build provider-specific HTTP headers.
    ///
    /// `api_key` is the bearer-shaped secret: a raw API key for `ApiKey`
    /// credentials, or the current OAuth access token for OAuth credentials
    /// (the caller pulls it from the lease — `Provider` doesn't need to know
    /// the auth_type, only which header to set).
    pub fn build_headers(&self, api_key: &str) -> Result<HeaderMap> {
        let mut headers = HeaderMap::new();
        headers.insert("Content-Type", HeaderValue::from_static("application/json"));

        match self {
            Provider::Ollama | Provider::ClaudeCli | Provider::Bedrock => {
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
            Provider::OpenAi
            | Provider::Gemini
            | Provider::DeepSeek
            | Provider::Groq
            | Provider::XAi => {
                let bearer = format!("Bearer {api_key}");
                let val = HeaderValue::from_str(&bearer)
                    .context("API key contains invalid characters for HTTP headers")?;
                headers.insert("Authorization", val);
            }
            Provider::ClaudeCodeOAuth => {
                // Anthropic wire format, but auth is OAuth Bearer (not x-api-key).
                let bearer = format!("Bearer {api_key}");
                let val = HeaderValue::from_str(&bearer)
                    .context("Access token contains invalid characters for HTTP headers")?;
                headers.insert("Authorization", val);
                headers.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
                // Claude Code OAuth tokens are scoped via the
                // `anthropic-beta: oauth-2025-04-20` header.
                headers.insert(
                    "anthropic-beta",
                    HeaderValue::from_static("oauth-2025-04-20"),
                );
            }
            Provider::CodexOAuth | Provider::Custom => {
                let bearer = format!("Bearer {api_key}");
                let val = HeaderValue::from_str(&bearer)
                    .context("Access token contains invalid characters for HTTP headers")?;
                headers.insert("Authorization", val);
            }
            Provider::CopilotOAuth => {
                // GitHub Copilot's chat endpoint requires both the OAuth bearer
                // and an integration-id header. Editor-version is technically
                // optional but Copilot warns when it's missing.
                let bearer = format!("Bearer {api_key}");
                let val = HeaderValue::from_str(&bearer)
                    .context("Access token contains invalid characters for HTTP headers")?;
                headers.insert("Authorization", val);
                headers.insert(
                    "Copilot-Integration-Id",
                    HeaderValue::from_static("vscode-chat"),
                );
                headers.insert("Editor-Version", HeaderValue::from_static("Borg/1.0"));
            }
            Provider::AzureOpenAi => {
                // Azure OpenAI uses the `api-key` header for static-key auth;
                // `Authorization: Bearer` is reserved for AAD (Entra ID)
                // tokens, which v2 will wire through the credential-pool
                // auth_type seam landing with Bedrock on main.
                let val = HeaderValue::from_str(api_key)
                    .context("API key contains invalid characters for HTTP headers")?;
                headers.insert("api-key", val);
            }
            Provider::VertexAi => {
                let bearer = format!("Bearer {api_key}");
                let val = HeaderValue::from_str(&bearer)
                    .context("Access token contains invalid characters for HTTP headers")?;
                headers.insert("Authorization", val);
            }
        }

        Ok(headers)
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
            Provider::Bedrock => model.to_string(),
            Provider::Gemini => model.strip_prefix("google/").unwrap_or(model).to_string(),
            Provider::DeepSeek => model.strip_prefix("deepseek/").unwrap_or(model).to_string(),
            Provider::Groq => model.strip_prefix("groq/").unwrap_or(model).to_string(),
            // xAI publishes Grok slugs without a vendor prefix. Tolerate both
            // `xai/grok-4` and the OpenRouter-style `x-ai/grok-4` so users
            // copying slugs from either docs page hit the same endpoint.
            Provider::XAi => model
                .strip_prefix("xai/")
                .or_else(|| model.strip_prefix("x-ai/"))
                .unwrap_or(model)
                .to_string(),
            Provider::Ollama => model.to_string(),
            Provider::ClaudeCli => model
                .strip_prefix("claude-cli/")
                .unwrap_or(model)
                .to_string(),
            // OAuth flavors normalize like their API-key counterparts.
            Provider::ClaudeCodeOAuth => model
                .strip_prefix("anthropic/")
                .unwrap_or(model)
                .to_string(),
            Provider::CodexOAuth => model.strip_prefix("openai/").unwrap_or(model).to_string(),
            Provider::CopilotOAuth => model.strip_prefix("copilot/").unwrap_or(model).to_string(),
            Provider::Custom => model.to_string(),
            // Azure routes by deployment name in the URL, not by model id in
            // the body — but the body's `model` field is required and should
            // be passthrough so the operator can put the deployment name (or
            // a label) there. No vendor prefix to strip.
            Provider::AzureOpenAi => model.to_string(),
            Provider::VertexAi => model
                .strip_prefix("google/")
                .or_else(|| model.strip_prefix("anthropic/"))
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
            Provider::Bedrock => !m.contains("claude-2") && !m.contains("claude-instant"),
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
            // xAI Grok 4 family is multimodal end-to-end; default true so
            // multimodal payloads aren't silently downgraded. If xAI ships a
            // text-only Grok variant later, blocklist it here.
            Provider::XAi => true,
            // Ollama: only known vision models
            Provider::Ollama => {
                m.contains("llava") || m.contains("vision") || m.contains("moondream")
            }
            // Claude CLI: same as Anthropic — all Claude 3+ models support vision
            Provider::ClaudeCli => !m.contains("claude-2") && !m.contains("claude-instant"),
            // OAuth flavors: same vision rules as their API-key counterparts.
            Provider::ClaudeCodeOAuth => !m.contains("claude-2") && !m.contains("claude-instant"),
            Provider::CodexOAuth => {
                !m.contains("gpt-3.5")
                    && !m.contains("davinci")
                    && !m.contains("babbage")
                    && !m.contains("ada")
                    && !m.contains("whisper")
                    && !m.contains("tts")
                    && !m.contains("dall-e")
                    && !m.contains("embedding")
            }
            Provider::CopilotOAuth => true,
            Provider::Custom => true, // operator-driven; let the endpoint reject if unsupported
            // Azure deployments are vendor-named (e.g. `gpt-4o`, `gpt-5`).
            // Mirror the OpenAI blocklist so embedding/whisper deployments
            // don't silently accept image inputs.
            Provider::AzureOpenAi => {
                !m.contains("gpt-3.5")
                    && !m.contains("davinci")
                    && !m.contains("babbage")
                    && !m.contains("ada")
                    && !m.contains("whisper")
                    && !m.contains("tts")
                    && !m.contains("dall-e")
                    && !m.contains("embedding")
            }
            Provider::VertexAi => {
                if m.starts_with("claude-")
                    || m.starts_with("anthropic/")
                    || m.contains("claude-2")
                    || m.contains("claude-instant")
                {
                    !m.contains("claude-2") && !m.contains("claude-instant")
                } else {
                    true
                }
            }
        }
    }

    /// String identifier for config serialization.
    pub fn as_str(&self) -> &'static str {
        match self {
            Provider::OpenRouter => "openrouter",
            Provider::OpenAi => "openai",
            Provider::Anthropic => "anthropic",
            Provider::Bedrock => "bedrock",
            Provider::Gemini => "gemini",
            Provider::DeepSeek => "deepseek",
            Provider::Groq => "groq",
            Provider::XAi => "xai",
            Provider::Ollama => "ollama",
            Provider::ClaudeCli => "claude-cli",
            Provider::ClaudeCodeOAuth => "claude-code-oauth",
            Provider::CodexOAuth => "codex-oauth",
            Provider::CopilotOAuth => "copilot-oauth",
            Provider::Custom => "custom",
            Provider::AzureOpenAi => "azure-openai",
            Provider::VertexAi => "vertex-ai",
        }
    }

    /// Infer the provider from an API key's distinctive prefix.
    ///
    /// Returns `Some(provider)` only when the prefix is unambiguous
    /// (OpenRouter `sk-or-`, Anthropic `sk-ant-`). Plain `sk-` is intentionally
    /// not classified — too many providers use that format.
    pub fn from_api_key_prefix(key: &str) -> Option<Provider> {
        let trimmed = key.trim();
        if trimmed.starts_with("sk-or-") {
            Some(Provider::OpenRouter)
        } else if trimmed.starts_with("sk-ant-") {
            Some(Provider::Anthropic)
        } else if trimmed.starts_with("xai-") {
            Some(Provider::XAi)
        } else {
            None
        }
    }

    /// Check if Ollama is reachable (sync, short timeout).
    pub fn ollama_available() -> bool {
        let default_addr: SocketAddr =
            SocketAddr::from(([127, 0, 0, 1], crate::constants::OLLAMA_PORT_DEFAULT));
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
            "bedrock" => Ok(Provider::Bedrock),
            "gemini" => Ok(Provider::Gemini),
            "deepseek" => Ok(Provider::DeepSeek),
            "groq" => Ok(Provider::Groq),
            "xai" | "x-ai" | "x_ai" => Ok(Provider::XAi),
            "ollama" => Ok(Provider::Ollama),
            "claude-cli" | "claudecli" | "claude_cli" => Ok(Provider::ClaudeCli),
            "claude-code-oauth" | "claude_code_oauth" | "claudecodeoauth" => {
                Ok(Provider::ClaudeCodeOAuth)
            }
            "codex-oauth" | "codex_oauth" | "codexoauth" => Ok(Provider::CodexOAuth),
            "copilot-oauth" | "copilot_oauth" | "copilotoauth" => Ok(Provider::CopilotOAuth),
            "custom" => Ok(Provider::Custom),
            "azure-openai" | "azure_openai" | "azureopenai" => Ok(Provider::AzureOpenAi),
            "vertex-ai" | "vertex_ai" | "vertexai" | "vertex" => Ok(Provider::VertexAi),
            _ => {
                bail!("Unknown provider: {s}. Valid options: openrouter, openai, anthropic, bedrock, gemini, deepseek, groq, xai, ollama, claude-cli, claude-code-oauth, codex-oauth, copilot-oauth, custom, azure-openai, vertex-ai")
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderAuthKind {
    ApiKeyLike,
    AwsCredentialChain,
    CloudOAuth,
    None,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_str_round_trip() {
        let all = [
            Provider::OpenRouter,
            Provider::OpenAi,
            Provider::Anthropic,
            Provider::Bedrock,
            Provider::Gemini,
            Provider::DeepSeek,
            Provider::Groq,
            Provider::XAi,
            Provider::Ollama,
            Provider::ClaudeCli,
            Provider::ClaudeCodeOAuth,
            Provider::CodexOAuth,
            Provider::CopilotOAuth,
            Provider::Custom,
            Provider::AzureOpenAi,
            Provider::VertexAi,
        ];
        for provider in &all {
            let s = provider.as_str();
            let parsed = Provider::from_str(s).unwrap();
            assert_eq!(parsed, *provider);
        }
    }

    #[test]
    fn oauth_variants_use_bearer_not_x_api_key() {
        // Claude Code OAuth wraps the Anthropic wire format but swaps
        // `x-api-key` for `Authorization: Bearer <access_token>`. If anyone
        // copy-pastes the Anthropic branch and forgets to flip the auth
        // header, the upstream rejects with 401 — caught here.
        let headers = Provider::ClaudeCodeOAuth
            .build_headers("oauth_at_xyz")
            .unwrap();
        assert!(
            headers.get("x-api-key").is_none(),
            "OAuth must not send x-api-key"
        );
        assert_eq!(
            headers.get("Authorization").unwrap().to_str().unwrap(),
            "Bearer oauth_at_xyz"
        );
        assert!(headers.get("anthropic-version").is_some());

        let headers = Provider::CodexOAuth.build_headers("oauth_at_abc").unwrap();
        assert_eq!(
            headers.get("Authorization").unwrap().to_str().unwrap(),
            "Bearer oauth_at_abc"
        );

        let headers = Provider::CopilotOAuth.build_headers("gho_xyz").unwrap();
        assert_eq!(
            headers.get("Authorization").unwrap().to_str().unwrap(),
            "Bearer gho_xyz"
        );
        assert!(headers.get("Copilot-Integration-Id").is_some());
    }

    #[test]
    fn custom_provider_uses_bearer_and_empty_static_base_url() {
        // Custom resolves base_url from the lease at request time; the static
        // base_url() is empty so a caller that forgets to consult the lease
        // hits an obvious "" URL parse error instead of a wrong-host request.
        assert_eq!(Provider::Custom.base_url(), "");
        let headers = Provider::Custom.build_headers("sk-local-vllm").unwrap();
        assert_eq!(
            headers.get("Authorization").unwrap().to_str().unwrap(),
            "Bearer sk-local-vllm"
        );
    }

    #[test]
    fn claude_code_oauth_is_not_openai_compatible() {
        // It uses the Anthropic /v1/messages wire format. Misclassifying it
        // would route requests through the OpenAI chat-completions path and
        // produce malformed payloads.
        assert!(!Provider::ClaudeCodeOAuth.is_openai_compatible());
        assert!(Provider::CodexOAuth.is_openai_compatible());
        assert!(Provider::CopilotOAuth.is_openai_compatible());
        assert!(Provider::Custom.is_openai_compatible());
        assert!(!Provider::Bedrock.is_openai_compatible());
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

    #[test]
    fn bedrock_uses_aws_chain_not_api_key_pool() {
        assert!(Provider::Bedrock.uses_aws_credential_chain());
        assert!(!Provider::Bedrock.requires_api_key());
        assert!(!Provider::Bedrock.supports_credential_pool());
    }

    // detect_from_env / from_env_var_name were removed along with provider auto-detection.
    // Provider is now strictly sourced from `Config.llm.provider` (set via onboarding,
    // `/settings`, or `borg settings set llm.provider <name>`). See config::resolve_provider.

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
            Provider::Bedrock.normalize_model("anthropic.claude-3-7-sonnet-20250219-v1:0"),
            "anthropic.claude-3-7-sonnet-20250219-v1:0"
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
            Provider::Bedrock.normalize_model("anthropic.claude-3-7-sonnet-20250219-v1:0"),
            "anthropic.claude-3-7-sonnet-20250219-v1:0"
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
    fn from_api_key_prefix_openrouter() {
        assert_eq!(
            Provider::from_api_key_prefix("sk-or-v1-abc123"),
            Some(Provider::OpenRouter)
        );
    }

    #[test]
    fn from_api_key_prefix_anthropic() {
        assert_eq!(
            Provider::from_api_key_prefix("sk-ant-api03-xyz"),
            Some(Provider::Anthropic)
        );
    }

    #[test]
    fn from_api_key_prefix_trims_whitespace() {
        assert_eq!(
            Provider::from_api_key_prefix("  sk-or-v1-key  "),
            Some(Provider::OpenRouter)
        );
    }

    #[test]
    fn from_api_key_prefix_plain_sk_is_ambiguous() {
        // `sk-` alone is used by OpenAI, DeepSeek, Groq, etc. — not classified.
        assert_eq!(Provider::from_api_key_prefix("sk-abc123"), None);
    }

    #[test]
    fn from_api_key_prefix_unknown() {
        assert_eq!(Provider::from_api_key_prefix(""), None);
        assert_eq!(Provider::from_api_key_prefix("AIzaFakeGemini"), None);
        assert_eq!(Provider::from_api_key_prefix("random-key"), None);
    }

    // ── xAI (Grok) tests ──

    #[test]
    fn xai_round_trip() {
        assert_eq!(Provider::XAi.as_str(), "xai");
        assert_eq!(Provider::from_str("xai").unwrap(), Provider::XAi);
        // Case-insensitive + alternate spellings users might paste from docs.
        assert_eq!(Provider::from_str("XAI").unwrap(), Provider::XAi);
        assert_eq!(Provider::from_str("x-ai").unwrap(), Provider::XAi);
        assert_eq!(Provider::from_str("X-AI").unwrap(), Provider::XAi);
    }

    #[test]
    fn xai_base_url_and_env() {
        // Pin the production endpoint. An accidental edit that points Grok
        // traffic at a different host would silently break every xAI user
        // — this is cheap insurance against that.
        assert_eq!(
            Provider::XAi.base_url(),
            "https://api.x.ai/v1/chat/completions"
        );
        assert_eq!(Provider::XAi.default_env_var(), "XAI_API_KEY");
        assert!(Provider::XAi.requires_api_key());
    }

    #[test]
    fn xai_is_openai_compatible() {
        // Guards against a future enum reshuffle that accidentally lands xAI
        // in the Anthropic match arm — that would route Grok requests through
        // /v1/messages and produce a 4xx on every turn.
        assert!(Provider::XAi.is_openai_compatible());
    }

    #[test]
    fn xai_build_headers_bearer() {
        let headers = Provider::XAi.build_headers("xai-test123").unwrap();
        assert_eq!(
            headers.get("Authorization").unwrap().to_str().unwrap(),
            "Bearer xai-test123"
        );
        // xAI doesn't use any of the provider-specific headers other paths set.
        assert!(headers.get("x-api-key").is_none());
        assert!(headers.get("anthropic-version").is_none());
        assert!(headers.get("Copilot-Integration-Id").is_none());
        assert!(headers.get("HTTP-Referer").is_none());
    }

    #[test]
    fn xai_normalize_model_strips_vendor_prefix() {
        assert_eq!(Provider::XAi.normalize_model("xai/grok-4"), "grok-4");
        // Tolerate the OpenRouter-style x-ai/ prefix too — users routinely
        // copy slugs across the two docs sites.
        assert_eq!(
            Provider::XAi.normalize_model("x-ai/grok-4-fast-reasoning"),
            "grok-4-fast-reasoning"
        );
        // Bare slugs pass through unchanged.
        assert_eq!(Provider::XAi.normalize_model("grok-4"), "grok-4");
    }

    #[test]
    fn xai_supports_vision_true_for_grok_family() {
        // Grok 4 frontier is multimodal — default true mirrors Gemini/OpenAI.
        // If a future Grok variant is text-only it gets blocklisted here, but
        // today every published Grok 4 model accepts image inputs.
        assert!(Provider::XAi.supports_vision("grok-4"));
        assert!(Provider::XAi.supports_vision("grok-4-fast-reasoning"));
        assert!(Provider::XAi.supports_vision("grok-4-fast-non-reasoning"));
    }

    #[test]
    fn xai_supports_tools_for_grok_4_family() {
        // The reasoning-only blocklist (deepseek-r1, qwq, o1-mini/preview)
        // must not match any Grok slug. Lock that explicitly so a future
        // edit to `supports_tools` (e.g. adding `r1` keyword) can't quietly
        // disable tool calling for `grok-4-fast-reasoning`.
        assert!(Provider::XAi.supports_tools("grok-4"));
        assert!(Provider::XAi.supports_tools("grok-4-fast-reasoning"));
        assert!(Provider::XAi.supports_tools("grok-4-fast-non-reasoning"));
    }

    #[test]
    fn from_api_key_prefix_xai() {
        assert_eq!(
            Provider::from_api_key_prefix("xai-abc123"),
            Some(Provider::XAi)
        );
        assert_eq!(
            Provider::from_api_key_prefix("  xai-key  "),
            Some(Provider::XAi)
        );
    }
    // ── AzureOpenAi tests ──

    #[test]
    fn azure_openai_from_str_aliases() {
        // All accepted alias spellings must resolve to the same variant.
        for alias in [
            "azure-openai",
            "azure_openai",
            "azureopenai",
            "Azure-OpenAI",
        ] {
            assert_eq!(
                Provider::from_str(alias).unwrap(),
                Provider::AzureOpenAi,
                "alias {alias} did not parse"
            );
        }
    }

    #[test]
    fn azure_openai_api_key_header_uses_api_key_not_bearer() {
        // Spec invariant: Azure rejects `Authorization: Bearer <api_key>`
        // for static-key auth. The `api-key` header is the documented
        // shape; misclassifying this regresses every Azure request to a
        // 401.
        let headers = Provider::AzureOpenAi.build_headers("test-key").unwrap();
        assert_eq!(
            headers.get("api-key").unwrap().to_str().unwrap(),
            "test-key"
        );
        assert!(
            headers.get("Authorization").is_none(),
            "Azure api-key path must not send Authorization: Bearer"
        );
    }

    #[test]
    fn azure_openai_is_openai_compatible_true() {
        // Azure speaks OpenAI ChatCompletions on the wire. Routing it
        // through the Anthropic transport would produce malformed bodies.
        assert!(Provider::AzureOpenAi.is_openai_compatible());
    }

    #[test]
    fn azure_openai_static_base_url_is_empty() {
        // URL is composed from llm.azure_* settings at request time. A
        // non-empty static URL here would let callers that forget to
        // compose silently hit the wrong host.
        assert_eq!(Provider::AzureOpenAi.base_url(), "");
    }

    #[test]
    fn azure_openai_default_env_var() {
        assert_eq!(
            Provider::AzureOpenAi.default_env_var(),
            "AZURE_OPENAI_API_KEY"
        );
    }

    #[test]
    fn azure_openai_normalize_model_passthrough() {
        // Azure deployment names are operator-chosen — must not be
        // prefix-stripped or mutated.
        assert_eq!(
            Provider::AzureOpenAi.normalize_model("my-gpt-4o"),
            "my-gpt-4o"
        );
        assert_eq!(
            Provider::AzureOpenAi.normalize_model("openai/gpt-4o"),
            "openai/gpt-4o"
        );
    }

    #[test]
    fn azure_openai_supports_vision_blocklist() {
        // Vision-capable deployments must pass; embedding/whisper/etc.
        // must be flagged so multimodal messages don't get silently
        // downgraded to a 400.
        assert!(Provider::AzureOpenAi.supports_vision("gpt-4o"));
        assert!(Provider::AzureOpenAi.supports_vision("gpt-5"));
        assert!(!Provider::AzureOpenAi.supports_vision("text-embedding-3-large"));
        assert!(!Provider::AzureOpenAi.supports_vision("whisper-1"));
        assert!(!Provider::AzureOpenAi.supports_vision("dall-e-3"));
    }

    #[test]
    fn vertex_ai_from_str_aliases_and_cloud_auth() {
        for alias in ["vertex-ai", "vertex_ai", "vertexai", "vertex", "Vertex-AI"] {
            assert_eq!(Provider::from_str(alias).unwrap(), Provider::VertexAi);
        }
        assert!(Provider::VertexAi.supports_cloud_auth());
        assert!(!Provider::VertexAi.requires_api_key());
        assert_eq!(Provider::VertexAi.default_env_var(), "");
    }

    #[test]
    fn vertex_ai_normalizes_google_and_anthropic_prefixes() {
        assert_eq!(
            Provider::VertexAi.normalize_model("google/gemini-2.5-pro"),
            "gemini-2.5-pro"
        );
        assert_eq!(
            Provider::VertexAi.normalize_model("anthropic/claude-sonnet-4"),
            "claude-sonnet-4"
        );
    }
}
