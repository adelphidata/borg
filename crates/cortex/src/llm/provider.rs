use std::fmt;
use std::net::ToSocketAddrs;
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
    /// OpenAI ChatGPT OAuth — OpenAI wire format with Bearer auth from the
    /// CLI's stored token at `~/.codex/auth.json`.
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

/// Static, data-only facts about a provider.
///
/// Every provider's pure-data dimensions live in exactly one place: the
/// `Provider::meta` match. The `auth_kind` / `base_url` / `default_env_var` /
/// `as_str` / `normalize_model` / `from_str` accessors all read from here, so
/// adding a provider means filling in one struct literal instead of editing six
/// parallel match statements (which the compiler can't keep in sync). Genuinely
/// behavioral methods (`build_headers`, `supports_vision`, …) stay as their own
/// matches because they encode logic, not data.
pub(crate) struct ProviderMeta {
    /// Canonical config string (the `as_str` value).
    name: &'static str,
    /// Extra accepted spellings for `from_str`, beyond `name`. All lowercase.
    aliases: &'static [&'static str],
    /// Static API base URL, or `""` when composed at request time.
    base_url: &'static str,
    /// Default env var seeding this provider's key/host, or `""` when none.
    env_var: &'static str,
    /// How requests are authenticated.
    auth_kind: ProviderAuthKind,
    /// Vendor prefixes stripped from model ids, tried in order. Empty = passthrough.
    model_prefixes: &'static [&'static str],
}

impl Provider {
    /// Every provider variant, in declaration order. Single source for
    /// iteration (`from_str`, round-trip tests, onboarding menus).
    pub const ALL: &'static [Provider] = &[
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

    /// The single data table. One arm per provider; everything data-shaped is here.
    fn meta(self) -> &'static ProviderMeta {
        match self {
            Provider::OpenRouter => &ProviderMeta {
                name: "openrouter",
                aliases: &[],
                base_url: "https://openrouter.ai/api/v1/chat/completions",
                env_var: "OPENROUTER_API_KEY",
                auth_kind: ProviderAuthKind::ApiKeyLike,
                model_prefixes: &[], // keeps the full vendor/model path
            },
            Provider::OpenAi => &ProviderMeta {
                name: "openai",
                aliases: &[],
                base_url: "https://api.openai.com/v1/chat/completions",
                env_var: "OPENAI_API_KEY",
                auth_kind: ProviderAuthKind::ApiKeyLike,
                model_prefixes: &["openai/"],
            },
            Provider::Anthropic => &ProviderMeta {
                name: "anthropic",
                aliases: &[],
                base_url: "https://api.anthropic.com/v1/messages",
                env_var: "ANTHROPIC_API_KEY",
                auth_kind: ProviderAuthKind::ApiKeyLike,
                model_prefixes: &["anthropic/"],
            },
            Provider::Bedrock => &ProviderMeta {
                name: "bedrock",
                aliases: &[],
                base_url: "",
                env_var: "",
                auth_kind: ProviderAuthKind::AwsCredentialChain,
                model_prefixes: &[],
            },
            Provider::Gemini => &ProviderMeta {
                name: "gemini",
                aliases: &[],
                base_url:
                    "https://generativelanguage.googleapis.com/v1beta/openai/chat/completions",
                env_var: "GEMINI_API_KEY",
                auth_kind: ProviderAuthKind::ApiKeyLike,
                model_prefixes: &["google/"],
            },
            Provider::DeepSeek => &ProviderMeta {
                name: "deepseek",
                aliases: &[],
                base_url: "https://api.deepseek.com/v1/chat/completions",
                env_var: "DEEPSEEK_API_KEY",
                auth_kind: ProviderAuthKind::ApiKeyLike,
                model_prefixes: &["deepseek/"],
            },
            Provider::Groq => &ProviderMeta {
                name: "groq",
                aliases: &[],
                base_url: "https://api.groq.com/openai/v1/chat/completions",
                env_var: "GROQ_API_KEY",
                auth_kind: ProviderAuthKind::ApiKeyLike,
                model_prefixes: &["groq/"],
            },
            // xAI publishes Grok slugs without a vendor prefix. Tolerate every
            // common docs spelling so a slug/config copy-pasted from any source
            // resolves to the same endpoint.
            Provider::XAi => &ProviderMeta {
                name: "xai",
                aliases: &["x-ai", "x_ai", "x.ai", "grok"],
                base_url: "https://api.x.ai/v1/chat/completions",
                env_var: "XAI_API_KEY",
                auth_kind: ProviderAuthKind::ApiKeyLike,
                model_prefixes: &["xai/", "x-ai/", "x.ai/", "grok/"],
            },
            Provider::Ollama => &ProviderMeta {
                name: "ollama",
                aliases: &[],
                base_url: "http://localhost:11434/v1/chat/completions",
                env_var: "OLLAMA_HOST",
                auth_kind: ProviderAuthKind::None,
                model_prefixes: &[],
            },
            Provider::ClaudeCli => &ProviderMeta {
                name: "claude-cli",
                aliases: &["claudecli", "claude_cli"],
                base_url: "",               // Not used — communicates via subprocess
                env_var: "CLAUDE_CLI_PATH", // Optional override for claude binary path
                auth_kind: ProviderAuthKind::None,
                model_prefixes: &["claude-cli/"],
            },
            // OAuth flavors hit the same wire endpoints as their API-key
            // counterparts; only the auth header changes. Their credentials seed
            // via on-disk discoverers, not env vars (empty `env_var`).
            Provider::ClaudeCodeOAuth => &ProviderMeta {
                name: "claude-code-oauth",
                aliases: &["claude_code_oauth", "claudecodeoauth"],
                base_url: "https://api.anthropic.com/v1/messages",
                env_var: "",
                auth_kind: ProviderAuthKind::ApiKeyLike,
                model_prefixes: &["anthropic/"],
            },
            Provider::CodexOAuth => &ProviderMeta {
                name: "codex-oauth",
                aliases: &["codex_oauth", "codexoauth"],
                base_url: "https://api.openai.com/v1/chat/completions",
                env_var: "",
                auth_kind: ProviderAuthKind::ApiKeyLike,
                model_prefixes: &["openai/"],
            },
            Provider::CopilotOAuth => &ProviderMeta {
                name: "copilot-oauth",
                aliases: &["copilot_oauth", "copilotoauth"],
                base_url: "https://api.githubcopilot.com/chat/completions",
                env_var: "",
                auth_kind: ProviderAuthKind::ApiKeyLike,
                model_prefixes: &["copilot/"],
            },
            // Custom resolves base_url from the lease at request time.
            Provider::Custom => &ProviderMeta {
                name: "custom",
                aliases: &[],
                base_url: "",
                env_var: "",
                auth_kind: ProviderAuthKind::ApiKeyLike,
                model_prefixes: &[],
            },
            // Azure composes its URL from llm.azure_* settings; see
            // `LlmClient::effective_base_url`. Empty base_url means a caller that
            // forgets to compose fails loudly on URL parse instead of silently
            // hitting a wrong host. Deployment names are operator-chosen, so no
            // prefix is stripped.
            Provider::AzureOpenAi => &ProviderMeta {
                name: "azure-openai",
                aliases: &["azure_openai", "azureopenai"],
                base_url: "",
                env_var: "AZURE_OPENAI_API_KEY",
                auth_kind: ProviderAuthKind::ApiKeyLike,
                model_prefixes: &[],
            },
            Provider::VertexAi => &ProviderMeta {
                name: "vertex-ai",
                aliases: &["vertex_ai", "vertexai", "vertex"],
                base_url: "",
                env_var: "",
                auth_kind: ProviderAuthKind::CloudOAuth,
                model_prefixes: &["google/", "anthropic/"],
            },
        }
    }

    /// How this provider obtains request credentials.
    pub fn auth_kind(&self) -> ProviderAuthKind {
        self.meta().auth_kind
    }

    /// API base URL for this provider.
    pub fn base_url(&self) -> &'static str {
        self.meta().base_url
    }

    /// Default environment variable name for this provider's API key (or host).
    pub fn default_env_var(&self) -> &'static str {
        self.meta().env_var
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
    ///
    /// Gemini was historically routed through OpenAI-compat (Gemini AI Studio
    /// exposes an OpenAI-shaped façade at `/v1beta/openai/chat/completions`),
    /// but that path strips `inline_data` for video and refuses
    /// Gemini-specific request fields (`thinkingConfig`, `cachedContents`).
    /// Now routed through the native `gemini_transport()` so video parts and
    /// the full request shape reach the model.
    pub fn is_openai_compatible(&self) -> bool {
        !matches!(
            self,
            Provider::Anthropic
                | Provider::Bedrock
                | Provider::ClaudeCli
                | Provider::ClaudeCodeOAuth
                | Provider::Gemini
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
            Provider::OpenAi | Provider::DeepSeek | Provider::Groq | Provider::XAi => {
                let bearer = format!("Bearer {api_key}");
                let val = HeaderValue::from_str(&bearer)
                    .context("API key contains invalid characters for HTTP headers")?;
                headers.insert("Authorization", val);
            }
            Provider::Gemini => {
                // Native AI Studio (`/v1beta/models/...:generateContent`)
                // authenticates raw API keys with `x-goog-api-key`, NOT
                // `Authorization: Bearer` (which is reserved for OAuth /
                // service-account access tokens and returns 401 for an
                // API key). The native endpoint rejects Bearer for static
                // API keys, so this must stay in its own arm.
                let val = HeaderValue::from_str(api_key)
                    .context("API key contains invalid characters for HTTP headers")?;
                headers.insert("x-goog-api-key", val);
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
            Provider::CodexOAuth => {
                let bearer = format!("Bearer {api_key}");
                let val = HeaderValue::from_str(&bearer)
                    .context("Access token contains invalid characters for HTTP headers")?;
                headers.insert("Authorization", val);
            }
            Provider::Custom => {
                // Custom endpoints are commonly keyless local servers
                // (llama.cpp, LM Studio, vLLM). Skip the header entirely
                // rather than sending a malformed `Bearer ` credential,
                // which some servers reject with a 400/401.
                if !api_key.is_empty() {
                    let bearer = format!("Bearer {api_key}");
                    let val = HeaderValue::from_str(&bearer)
                        .context("Access token contains invalid characters for HTTP headers")?;
                    headers.insert("Authorization", val);
                }
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
                // `Authorization: Bearer` is reserved for AAD (Entra ID) tokens.
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
    ///
    /// Tries each of the provider's `model_prefixes` in order and strips the
    /// first match; otherwise the model id passes through unchanged. For
    /// providers that *require* a vendor prefix this also runs the inverse
    /// repair, so a config that lost its prefix self-heals on the next request
    /// instead of 404ing forever.
    pub fn normalize_model(&self, model: &str) -> String {
        for prefix in self.meta().model_prefixes {
            if let Some(stripped) = model.strip_prefix(prefix) {
                return stripped.to_string();
            }
        }
        if let Some(repaired) = self.repair_missing_vendor_prefix(model) {
            tracing::info!(
                "model id '{model}' is missing its vendor prefix for {self}; using '{repaired}'"
            );
            return repaired;
        }
        model.to_string()
    }

    /// Whether every model id this provider serves is a `vendor/model` path.
    ///
    /// Only OpenRouter: it is a router, so the vendor half is what selects the
    /// upstream. Providers with an empty `model_prefixes` are not equivalent —
    /// Ollama, Bedrock, Vertex and custom endpoints all serve bare ids.
    pub fn requires_vendor_prefix(self) -> bool {
        matches!(self, Provider::OpenRouter)
    }

    /// Restore the vendor prefix on a bare model id, when the id's family
    /// identifies exactly one vendor.
    ///
    /// A bare id on a prefix-routed provider produces a plain HTTP 404, which
    /// reads like an outage or an auth failure — the one thing it doesn't look
    /// like is a typo in the model id. This is a lookup against the same vendor
    /// list [`Self::normalize_model`] strips with, not a guess: a family we
    /// don't recognise is left alone for the error path to explain.
    pub fn repair_missing_vendor_prefix(self, model: &str) -> Option<String> {
        if !self.requires_vendor_prefix() || model.contains('/') || model.is_empty() {
            return None;
        }
        Self::vendor_for_model_family(model).map(|vendor| format!("{vendor}/{model}"))
    }

    /// Map a bare model id to its vendor, by family.
    ///
    /// The inverse of the vendor prefixes stripped elsewhere in this module.
    /// Each arm is a family stem, not a fuzzy match, so an unrecognised id
    /// yields `None` rather than a wrong vendor.
    fn vendor_for_model_family(model: &str) -> Option<&'static str> {
        let lowered = model.to_lowercase();
        MODEL_FAMILY_VENDORS
            .iter()
            .find(|(stem, _)| lowered.starts_with(stem))
            .map(|(_, vendor)| *vendor)
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
        self.meta().name
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
    ///
    /// Honors `OLLAMA_HOST` (host[:port], scheme/path optional). Callers that
    /// know the configured `llm.base_url` should prefer
    /// [`Provider::ollama_available_at`] so a remote endpoint configured only
    /// via `base_url` isn't misreported as down.
    pub fn ollama_available() -> bool {
        Self::ollama_available_at(None)
    }

    /// Check if Ollama is reachable, preferring `base_url_override` (the
    /// configured `llm.base_url`, full endpoint URL accepted) over
    /// `OLLAMA_HOST`, falling back to `localhost:11434`.
    pub fn ollama_available_at(base_url_override: Option<&str>) -> bool {
        let (host, port) = ollama_probe_target(&ollama_origin(base_url_override));
        // Hostname specs need DNS resolution — `SocketAddr::parse` only
        // accepts IP literals, and silently probing localhost instead of the
        // configured remote host produced false health signals. The lookup
        // itself uses the OS resolver with no timeout of our own; localhost
        // resolves from /etc/hosts instantly, and remote names are only
        // consulted once per doctor/onboarding run.
        let Ok(addrs) = (host.as_str(), port).to_socket_addrs() else {
            return false;
        };
        // Probe runs on the onboarding/startup hot path; keep the connect
        // short so an absent local Ollama doesn't stall boot.
        addrs
            .take(2)
            .any(|addr| std::net::TcpStream::connect_timeout(&addr, PROBE_TIMEOUT).is_ok())
    }
}

/// Model-family stem → vendor, used to restore a dropped vendor prefix.
///
/// The inverse of the vendor prefixes stripped in [`crate::llm::model_registry`],
/// so every vendor here must also appear there — otherwise a repaired id sails
/// past the context-window lookup and silently takes the default window.
/// `vendor_prefixes_round_trip_through_the_registry` guards that.
pub(crate) const MODEL_FAMILY_VENDORS: &[(&str, &str)] = &[
    ("claude-", "anthropic"),
    ("gpt-", "openai"),
    ("o1", "openai"),
    ("o3", "openai"),
    ("o4-", "openai"),
    ("codex-", "openai"),
    ("gemini-", "google"),
    ("gemma-", "google"),
    ("grok-", "x-ai"),
    ("qwen", "qwen"),
    ("deepseek-", "deepseek"),
    ("glm-", "z-ai"),
    ("kimi-", "moonshotai"),
    ("minimax", "minimax"),
    ("mistral", "mistralai"),
    ("ministral", "mistralai"),
    ("magistral", "mistralai"),
    ("devstral", "mistralai"),
    ("llama-", "meta-llama"),
    ("phi-", "microsoft"),
];

/// Connect timeout for the Ollama reachability probe (per resolved address).
const PROBE_TIMEOUT: Duration = Duration::from_millis(200);

/// Resolve the Ollama server origin (`scheme://host[:port]`, no path).
///
/// Precedence: explicit override (the configured `llm.base_url`; a full
/// endpoint URL is accepted and reduced to its origin) → `OLLAMA_HOST`
/// (ecosystem convention: `host[:port]`, scheme optional) → the localhost
/// default.
///
/// A schemeless spec follows the Ollama client convention: `http` with the
/// port defaulting to 11434, made explicit in the returned origin
/// (`myhost` → `http://myhost:11434`) so requests and reachability probes
/// can never disagree about the target. A spec with an explicit scheme is
/// kept verbatim — `https://host` means the scheme's default port (443), as
/// for any URL. Bare IPv6 hosts are bracketed to keep the origin a valid URL.
pub fn ollama_origin(base_url_override: Option<&str>) -> String {
    let spec = base_url_override
        .map(str::to_string)
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            std::env::var("OLLAMA_HOST")
                .ok()
                .filter(|s| !s.trim().is_empty())
        });
    let Some(spec) = spec else {
        return format!("http://localhost:{}", crate::constants::OLLAMA_PORT_DEFAULT);
    };
    let trimmed = spec.trim().trim_end_matches('/');
    if let Some(rest) = trimmed
        .strip_prefix("https://")
        .map(|r| ("https", r))
        .or_else(|| trimmed.strip_prefix("http://").map(|r| ("http", r)))
    {
        let (scheme, rest) = rest;
        let host_port = rest.split('/').next().unwrap_or(rest);
        return format!("{scheme}://{host_port}");
    }
    let host_port = trimmed.split('/').next().unwrap_or(trimmed);
    let (host, port) = split_host_port(host_port);
    let port = port.unwrap_or(crate::constants::OLLAMA_PORT_DEFAULT);
    if host.contains(':') {
        format!("http://[{host}]:{port}")
    } else {
        format!("http://{host}:{port}")
    }
}

/// Split a `host[:port]` string, understanding bracketed (`[::1]:11434`) and
/// bare (`::1`) IPv6 hosts. An unparseable port yields the whole input as
/// host (garbage in, unresolvable host out).
fn split_host_port(host_port: &str) -> (String, Option<u16>) {
    if let Some(rest) = host_port.strip_prefix('[') {
        if let Some((host, after)) = rest.split_once(']') {
            return (
                host.to_string(),
                after.strip_prefix(':').and_then(|p| p.parse().ok()),
            );
        }
    }
    match host_port.rsplit_once(':') {
        // A single colon separates host from port; multiple colons mean a
        // bare IPv6 address with no port syntax.
        Some((host, port_str)) if !host.contains(':') => match port_str.parse() {
            Ok(port) => (host.to_string(), Some(port)),
            Err(_) => (host_port.to_string(), None),
        },
        _ => (host_port.to_string(), None),
    }
}

/// Extract the `(host, port)` TCP probe target from an origin or
/// `OLLAMA_HOST`-style spec.
///
/// The default port is scheme-aware so the probe target always matches where
/// requests actually go: an explicit `http://`/`https://` spec without a
/// port defaults to 80/443 (what reqwest will dial for the composed
/// endpoint URL), while a schemeless `host[:port]` spec follows the Ollama
/// client convention of 11434.
fn ollama_probe_target(spec: &str) -> (String, u16) {
    let s = spec.trim();
    let (default_port, rest) = if let Some(rest) = s.strip_prefix("https://") {
        (443, rest)
    } else if let Some(rest) = s.strip_prefix("http://") {
        (80, rest)
    } else {
        (crate::constants::OLLAMA_PORT_DEFAULT, s)
    };
    let host_port = rest.split('/').next().unwrap_or(rest);
    let (host, port) = split_host_port(host_port);
    (host, port.unwrap_or(default_port))
}

impl fmt::Display for Provider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Provider {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        let lower = s.to_lowercase();
        // Canonical name or any documented alias resolves to the provider.
        // Both live in the `meta()` table, so adding a spelling is a one-line
        // edit there rather than a new match arm here.
        for provider in Provider::ALL {
            let meta = provider.meta();
            if lower == meta.name || meta.aliases.contains(&lower.as_str()) {
                return Ok(*provider);
            }
        }
        let valid = Provider::ALL
            .iter()
            .map(Provider::as_str)
            .collect::<Vec<_>>()
            .join(", ");
        bail!("Unknown provider: {s}. Valid options: {valid}")
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
        for provider in Provider::ALL {
            let s = provider.as_str();
            let parsed = Provider::from_str(s).unwrap();
            assert_eq!(parsed, *provider);
        }
    }

    /// A config that lost its vendor prefix must self-heal on the next
    /// request — without this a bare id 404s on every attempt, and the failure
    /// names neither the model nor the missing prefix. The repair is a lookup,
    /// not a guess: an id whose family we don't know passes through untouched
    /// so the error path can explain it, and an already-prefixed id is never
    /// rewritten.
    #[test]
    fn openrouter_repairs_a_bare_model_id_from_its_family() {
        for (input, expected) in [
            ("claude-sonnet-4.6", "anthropic/claude-sonnet-4.6"),
            ("gpt-5.5", "openai/gpt-5.5"),
            ("o3", "openai/o3"),
            ("gemini-3-flash", "google/gemini-3-flash"),
            ("grok-4", "x-ai/grok-4"),
            ("glm-5.1", "z-ai/glm-5.1"),
            ("kimi-k3", "moonshotai/kimi-k3"),
            ("devstral-2512", "mistralai/devstral-2512"),
            // Unknown family and already-prefixed ids are left exactly as-is.
            ("some-unknown-model-v9", "some-unknown-model-v9"),
            ("anthropic/claude-sonnet-4.6", "anthropic/claude-sonnet-4.6"),
        ] {
            assert_eq!(
                Provider::OpenRouter.normalize_model(input),
                expected,
                "{input} must normalize to {expected}"
            );
        }
        // An unrepairable id yields no suggestion, so the error path says
        // "missing prefix" rather than inventing one.
        assert!(Provider::OpenRouter
            .repair_missing_vendor_prefix("some-unknown-model-v9")
            .is_none());
    }

    /// Repairing a prefix the registry can't strip back off would send the id
    /// out fine but silently lose its context window to the 200k default —
    /// which reads as "compaction never fires" and then a wall of 400s. The
    /// two lists have to agree in both directions.
    #[test]
    fn vendor_prefixes_round_trip_through_the_registry() {
        for (stem, vendor) in MODEL_FAMILY_VENDORS {
            let repaired = format!("{vendor}/{stem}probe");
            assert_eq!(
                crate::llm::model_registry::strip_vendor_prefix(&repaired),
                format!("{stem}probe"),
                "`{vendor}/` is repaired onto ids but the registry cannot strip it back off"
            );
        }
    }

    /// Only OpenRouter routes on the vendor half. Providers that serve bare
    /// ids — Ollama, Bedrock, custom endpoints — must never have a prefix
    /// invented for them, which would turn a working config into a 404.
    #[test]
    fn only_prefix_routed_providers_repair_bare_ids() {
        for provider in Provider::ALL {
            let repaired = provider.repair_missing_vendor_prefix("claude-sonnet-4.6");
            assert_eq!(
                repaired.is_some(),
                provider.requires_vendor_prefix(),
                "{provider} repair must follow requires_vendor_prefix"
            );
        }
        assert_eq!(
            Provider::Ollama.normalize_model("llama-3.3-70b"),
            "llama-3.3-70b"
        );
        assert_eq!(
            Provider::Groq.normalize_model("llama-3.3-70b-versatile"),
            "llama-3.3-70b-versatile"
        );
    }

    #[test]
    fn all_table_is_complete_and_unique() {
        // `ALL` drives `from_str` and every iteration site. A variant added to
        // the enum + `meta()` but forgotten in `ALL` would be silently
        // unparseable, so pin the count and the no-duplicate-name invariant.
        assert_eq!(Provider::ALL.len(), 16);
        let mut names: Vec<&str> = Provider::ALL.iter().map(|p| p.as_str()).collect();
        names.sort_unstable();
        let unique = names.len();
        names.dedup();
        assert_eq!(names.len(), unique, "duplicate provider name in ALL/meta()");
    }

    #[test]
    fn aliases_are_lowercase_and_distinct_from_name() {
        // `from_str` lowercases its input before matching, so an uppercase
        // alias would be dead. An alias equal to the canonical name is
        // redundant noise. Guard both so the meta() table stays honest.
        for provider in Provider::ALL {
            let meta = provider.meta();
            for alias in meta.aliases {
                assert_eq!(
                    *alias,
                    alias.to_lowercase(),
                    "alias {alias:?} for {} must be lowercase",
                    meta.name
                );
                assert_ne!(
                    *alias, meta.name,
                    "alias {alias:?} duplicates canonical name"
                );
            }
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

    // Provider is sourced strictly from `Config.llm.provider` (set via onboarding,
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
        assert!(!Provider::Gemini.is_openai_compatible());
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
    fn ollama_origin_reduces_override_to_origin() {
        // A configured `llm.base_url` is a full endpoint URL; the origin is
        // what reachability probes and the /api/tags catalog need. Schemeless
        // specs (the `OLLAMA_HOST` convention) canonicalize to an explicit
        // http origin with the 11434 default port, so every consumer of the
        // origin dials the same place.
        let cases: &[(&str, &str)] = &[
            (
                "http://10.0.0.5:11434/v1/chat/completions",
                "http://10.0.0.5:11434",
            ),
            // Explicit scheme without a port is a normal URL: the scheme's
            // default port applies, so the origin must stay verbatim.
            (
                "https://ollama.tailnet.example/v1/chat/completions",
                "https://ollama.tailnet.example",
            ),
            // OLLAMA_HOST-style host[:port] specs, scheme optional.
            ("10.0.0.5:11434", "http://10.0.0.5:11434"),
            ("myhost", "http://myhost:11434"),
            ("myhost:1234", "http://myhost:1234"),
            // Bare IPv6 must be bracketed to stay a valid URL.
            ("::1", "http://[::1]:11434"),
            ("[::1]:8080", "http://[::1]:8080"),
            ("http://myhost:11434/", "http://myhost:11434"),
        ];
        for (input, want) in cases {
            assert_eq!(super::ollama_origin(Some(input)), *want, "input: {input}");
        }
    }

    #[test]
    fn ollama_probe_target_matches_request_target() {
        // Spec: the TCP probe must dial exactly where HTTP requests to the
        // same spec go — a probe that checks a different port reports false
        // health signals (the pre-fix bug this locks out). Scheme present →
        // scheme default port (80/443); schemeless → Ollama's 11434
        // convention. Hostnames and IPv6 must parse (the old
        // `SocketAddr::parse` silently fell back to localhost for them).
        let cases: &[(&str, &str, u16)] = &[
            ("http://localhost:11434", "localhost", 11434),
            // HTTPS-fronted Ollama (Caddy/Tailscale Funnel): requests go to
            // 443, so the probe must too.
            (
                "https://ollama.tailnet.example",
                "ollama.tailnet.example",
                443,
            ),
            ("http://myhost", "myhost", 80),
            ("http://10.0.0.5:8080", "10.0.0.5", 8080),
            ("myhost:1234", "myhost", 1234),
            ("myhost", "myhost", 11434),
            ("http://[::1]:11434", "::1", 11434),
            ("[::1]", "::1", 11434),
            ("::1", "::1", 11434),
            ("http://h:11434/v1/chat/completions", "h", 11434),
        ];
        for (input, want_host, want_port) in cases {
            let (host, port) = super::ollama_probe_target(input);
            assert_eq!(
                (host.as_str(), port),
                (*want_host, *want_port),
                "input: {input}"
            );
        }
        // End-to-end consistency: probing through `ollama_origin` (what
        // `ollama_available_at` does) preserves the schemeless convention.
        let (host, port) = super::ollama_probe_target(&super::ollama_origin(Some("myhost")));
        assert_eq!((host.as_str(), port), ("myhost", 11434));
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
    fn build_headers_custom_skips_auth_when_keyless() {
        // Keyless local OpenAI-compatible servers (llama.cpp, LM Studio):
        // no Authorization header at all — `Bearer ` (empty credential)
        // makes some servers reject the request.
        let headers = Provider::Custom.build_headers("").unwrap();
        assert!(headers.get("Authorization").is_none());
        // With a key configured, bearer auth is sent as before.
        let headers = Provider::Custom.build_headers("sk-byo").unwrap();
        assert_eq!(
            headers.get("Authorization").unwrap().to_str().unwrap(),
            "Bearer sk-byo"
        );
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
        // Case-insensitive + every documented alias users might paste:
        //   xai / x-ai / x_ai — canonical & OpenRouter / serde flavors
        //   x.ai             — the dotted brand name
        //   grok             — the product name users actually type
        // All must resolve to the same provider; if any drifts apart, a
        // config copy-pasted from any xAI docs page silently fails.
        assert_eq!(Provider::from_str("XAI").unwrap(), Provider::XAi);
        assert_eq!(Provider::from_str("x-ai").unwrap(), Provider::XAi);
        assert_eq!(Provider::from_str("X-AI").unwrap(), Provider::XAi);
        assert_eq!(Provider::from_str("x.ai").unwrap(), Provider::XAi);
        assert_eq!(Provider::from_str("X.AI").unwrap(), Provider::XAi);
        assert_eq!(Provider::from_str("grok").unwrap(), Provider::XAi);
        assert_eq!(Provider::from_str("Grok").unwrap(), Provider::XAi);
        assert_eq!(Provider::from_str("GROK").unwrap(), Provider::XAi);
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
        // Every documented prefix flavor — OpenRouter (`x-ai/`), dotted brand
        // (`x.ai/`), and bare product name (`grok/`) — must strip to the same
        // canonical slug. Drift here means a slug from one docs page silently
        // hits the xAI endpoint with the prefix attached and 404s.
        assert_eq!(
            Provider::XAi.normalize_model("x-ai/grok-4-fast-reasoning"),
            "grok-4-fast-reasoning"
        );
        assert_eq!(Provider::XAi.normalize_model("x.ai/grok-4.3"), "grok-4.3");
        assert_eq!(
            Provider::XAi.normalize_model("grok/grok-code-fast-1"),
            "grok-code-fast-1"
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
