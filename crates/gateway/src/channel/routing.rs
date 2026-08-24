use cortex::config::{ActivationMode, Config, GatewayBinding};
use tracing::debug;

use crate::constants::DEFAULT_BINDING_ID;

/// Result of route resolution for a gateway message.
pub struct ResolvedRoute {
    /// Base config with binding overrides applied.
    pub config: Config,
    /// Unique ID for session isolation.
    pub binding_id: String,
    /// Scoped memory directory name (if set by binding).
    pub memory_scope: Option<String>,
    /// Custom identity file path (relative to ~/.borg/).
    pub identity_path: Option<String>,
    /// Human-readable match descriptor.
    pub matched_by: String,
    /// Activation mode from binding override.
    pub activation: Option<ActivationMode>,
}

/// Resolve which gateway binding (if any) matches the given channel context.
///
/// Uses tiered matching (3 tiers, first match wins):
///   Tier 1: channel + sender + peer_kind (most specific)
///   Tier 2: channel + sender (no peer_kind)
///   Tier 3: channel only (wildcard sender)
///   Tier 4: default (no binding matched)
///
/// First match wins within each tier; higher tiers win over lower.
pub fn resolve_route(
    base_config: &Config,
    channel_name: &str,
    sender_id: &str,
    peer_kind: Option<&str>,
) -> ResolvedRoute {
    let bindings = &base_config.gateway.bindings;

    if bindings.is_empty() {
        return default_route(base_config);
    }

    // Tier 1: channel + sender + peer_kind
    if let Some(pk) = peer_kind {
        for binding in bindings {
            if matches_pattern(&binding.channel, channel_name)
                && binding
                    .sender
                    .as_deref()
                    .map(|s| matches_pattern(s, sender_id))
                    .unwrap_or(false)
                && binding
                    .peer_kind
                    .as_deref()
                    .map(|p| p == pk)
                    .unwrap_or(false)
            {
                return apply_binding(base_config, binding, "binding.peer");
            }
        }
    }

    // Tier 2: channel + sender (binding has sender but no peer_kind)
    for binding in bindings {
        if matches_pattern(&binding.channel, channel_name)
            && binding
                .sender
                .as_deref()
                .map(|s| matches_pattern(s, sender_id))
                .unwrap_or(false)
            && binding.peer_kind.is_none()
        {
            return apply_binding(base_config, binding, "binding.channel+sender");
        }
    }

    // Tier 3: channel only (binding has no sender filter)
    for binding in bindings {
        if matches_pattern(&binding.channel, channel_name)
            && binding.sender.is_none()
            && binding.peer_kind.is_none()
        {
            return apply_binding(base_config, binding, "binding.channel");
        }
    }

    // Tier 4: default
    default_route(base_config)
}

/// Check if a pattern matches a value.
/// Supports: exact match, prefix glob ("foo*"), suffix glob ("*bar"), wildcard ("*").
pub fn matches_pattern(pattern: &str, value: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    if let Some(prefix) = pattern.strip_suffix('*') {
        return value.starts_with(prefix);
    }
    if let Some(suffix) = pattern.strip_prefix('*') {
        return value.ends_with(suffix);
    }
    pattern == value
}

fn default_route(config: &Config) -> ResolvedRoute {
    ResolvedRoute {
        config: config.clone(),
        binding_id: DEFAULT_BINDING_ID.to_string(),
        memory_scope: None,
        identity_path: None,
        matched_by: DEFAULT_BINDING_ID.to_string(),
        activation: None,
    }
}

fn apply_binding(
    base_config: &Config,
    binding: &GatewayBinding,
    matched_by: &str,
) -> ResolvedRoute {
    let mut config = base_config.clone();

    let binding_id = format!(
        "bind:{}:{}:{}",
        binding.channel,
        binding.sender.as_deref().unwrap_or("*"),
        binding.peer_kind.as_deref().unwrap_or("*")
    );

    apply_binding_overrides(&mut config, binding);

    debug!("Route matched: {} (binding_id: {})", matched_by, binding_id);

    ResolvedRoute {
        config,
        binding_id,
        memory_scope: binding.memory_scope.clone(),
        identity_path: binding.identity.clone(),
        matched_by: matched_by.to_string(),
        activation: binding.activation.clone(),
    }
}

/// Apply LLM and other config overrides from a gateway binding.
fn apply_binding_overrides(config: &mut Config, binding: &GatewayBinding) {
    if let Some(ref provider) = binding.provider {
        config.llm.provider = Some(provider.clone());
    }
    if let Some(ref model) = binding.model {
        config.llm.model = model.clone();
    }
    if let Some(ref api_key_env) = binding.api_key_env {
        config.llm.api_key_env = api_key_env.clone();
    }
    if let Some(temp) = binding.temperature {
        config.llm.temperature = temp;
    }
    if let Some(max_tokens) = binding.max_tokens {
        config.llm.max_tokens = max_tokens;
    }
    if !binding.fallback.is_empty() {
        config.llm.fallback = binding.fallback.clone();
    }
    if let Some(ref scope) = binding.memory_scope {
        config.memory.memory_scope = Some(scope.clone());
    }
    if let Some(ref thinking) = binding.thinking {
        config.llm.thinking = thinking.clone();
    }
    if let Some(timeout) = binding.request_timeout_ms {
        config.llm.request_timeout_ms = timeout;
    }
    if let Some(timeout) = binding.gateway_timeout_ms {
        config.gateway.request_timeout_ms = timeout;
    }
    if let Some(timeout) = binding.stream_chunk_timeout_secs {
        config.llm.stream_chunk_timeout_secs = timeout;
    }
    if let Some(secs) = binding.inactivity_timeout_secs {
        config.gateway.inactivity_timeout_secs = secs;
    }
    if let Some(secs) = binding.inactivity_warning_secs {
        config.gateway.inactivity_warning_secs = secs;
    }
    if let Some(secs) = binding.inactivity_notify_secs {
        config.gateway.inactivity_notify_secs = secs;
    }
    // Ensure gateway timeout is at least as large as LLM request timeout
    // to prevent the gateway timer from cancelling mid-request.
    if config.gateway.request_timeout_ms < config.llm.request_timeout_ms {
        config.gateway.request_timeout_ms = config.llm.request_timeout_ms;
    }
    if let Some(ref identity) = binding.identity {
        // Validate identity path to prevent traversal outside data dir
        if identity.contains("..") || identity.starts_with('/') || identity.starts_with('\\') {
            tracing::warn!("Ignoring identity path with traversal: {}", identity);
        } else if let Ok(data_dir) = cortex::config::Config::data_dir() {
            config.identity_override = Some(data_dir.join(identity));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Binding with only the matching fields set — overrides all None.
    fn bare_binding(
        channel: &str,
        sender: Option<&str>,
        peer_kind: Option<&str>,
    ) -> GatewayBinding {
        GatewayBinding {
            channel: channel.to_string(),
            sender: sender.map(str::to_string),
            peer_kind: peer_kind.map(str::to_string),
            provider: None,
            model: None,
            api_key_env: None,
            temperature: None,
            max_tokens: None,
            identity: None,
            memory_scope: None,
            fallback: Vec::new(),
            activation: None,
            thinking: None,
            request_timeout_ms: None,
            gateway_timeout_ms: None,
            stream_chunk_timeout_secs: None,
            inactivity_timeout_secs: None,
            inactivity_warning_secs: None,
            inactivity_notify_secs: None,
        }
    }

    #[test]
    fn matches_pattern_exact() {
        assert!(matches_pattern("telegram", "telegram"));
        assert!(!matches_pattern("telegram", "slack"));
    }

    #[test]
    fn matches_pattern_prefix_glob() {
        assert!(matches_pattern("U12345*", "U12345678"));
        assert!(matches_pattern("U12345*", "U12345"));
        assert!(!matches_pattern("U12345*", "U99999"));
    }

    #[test]
    fn matches_pattern_suffix_glob() {
        assert!(matches_pattern("*gram", "telegram"));
        assert!(!matches_pattern("*gram", "slack"));
    }

    #[test]
    fn matches_pattern_wildcard() {
        assert!(matches_pattern("*", "anything"));
        assert!(matches_pattern("*", ""));
    }

    #[test]
    fn resolve_route_no_bindings_returns_default() {
        let config = Config::default();
        let route = resolve_route(&config, "telegram", "user1", None);
        assert_eq!(route.binding_id, DEFAULT_BINDING_ID);
        assert_eq!(route.matched_by, DEFAULT_BINDING_ID);
        assert!(route.memory_scope.is_none());
        assert!(route.identity_path.is_none());
    }

    #[test]
    fn resolve_route_channel_match() {
        let mut config = Config::default();
        config.gateway.bindings.push(GatewayBinding {
            channel: "telegram".to_string(),
            sender: None,
            peer_kind: None,
            provider: Some("anthropic".to_string()),
            model: Some("claude-sonnet-4".to_string()),
            api_key_env: None,
            temperature: Some(0.3),
            max_tokens: None,
            identity: None,
            memory_scope: Some("work".to_string()),
            fallback: Vec::new(),
            activation: None,
            thinking: None,
            request_timeout_ms: None,
            gateway_timeout_ms: None,
            stream_chunk_timeout_secs: None,
            inactivity_timeout_secs: None,
            inactivity_warning_secs: None,
            inactivity_notify_secs: None,
        });

        let route = resolve_route(&config, "telegram", "user1", None);
        assert_eq!(route.matched_by, "binding.channel");
        assert_eq!(route.memory_scope.as_deref(), Some("work"));
        assert_eq!(route.config.llm.provider.as_deref(), Some("anthropic"));
        assert_eq!(route.config.llm.model, "claude-sonnet-4");
        assert!((route.config.llm.temperature - 0.3).abs() < f32::EPSILON);
    }

    #[test]
    fn resolve_route_channel_sender_match() {
        let mut config = Config::default();
        config.gateway.bindings.push(GatewayBinding {
            channel: "slack".to_string(),
            sender: Some("U12345*".to_string()),
            peer_kind: None,
            provider: Some("openai".to_string()),
            model: Some("gpt-4.1".to_string()),
            api_key_env: None,
            temperature: None,
            max_tokens: None,
            identity: None,
            memory_scope: None,
            fallback: Vec::new(),
            activation: None,
            thinking: None,
            request_timeout_ms: None,
            gateway_timeout_ms: None,
            stream_chunk_timeout_secs: None,
            inactivity_timeout_secs: None,
            inactivity_warning_secs: None,
            inactivity_notify_secs: None,
        });

        let route = resolve_route(&config, "slack", "U12345678", None);
        assert_eq!(route.matched_by, "binding.channel+sender");
        assert_eq!(route.config.llm.model, "gpt-4.1");
    }

    #[test]
    fn resolve_route_peer_kind_match() {
        let mut config = Config::default();
        config.gateway.bindings.push(GatewayBinding {
            channel: "discord".to_string(),
            sender: Some("*".to_string()),
            peer_kind: Some("group".to_string()),
            provider: None,
            model: None,
            api_key_env: None,
            temperature: None,
            max_tokens: None,
            identity: None,
            memory_scope: Some("team".to_string()),
            fallback: Vec::new(),
            activation: None,
            thinking: None,
            request_timeout_ms: None,
            gateway_timeout_ms: None,
            stream_chunk_timeout_secs: None,
            inactivity_timeout_secs: None,
            inactivity_warning_secs: None,
            inactivity_notify_secs: None,
        });

        let route = resolve_route(&config, "discord", "user1", Some("group"));
        assert_eq!(route.matched_by, "binding.peer");
        assert_eq!(route.memory_scope.as_deref(), Some("team"));
    }

    #[test]
    fn resolve_route_higher_tier_wins() {
        let mut config = Config::default();
        // Tier 3: channel only
        config.gateway.bindings.push(GatewayBinding {
            channel: "telegram".to_string(),
            sender: None,
            peer_kind: None,
            provider: None,
            model: Some("model-channel".to_string()),
            api_key_env: None,
            temperature: None,
            max_tokens: None,
            identity: None,
            memory_scope: None,
            fallback: Vec::new(),
            activation: None,
            thinking: None,
            request_timeout_ms: None,
            gateway_timeout_ms: None,
            stream_chunk_timeout_secs: None,
            inactivity_timeout_secs: None,
            inactivity_warning_secs: None,
            inactivity_notify_secs: None,
        });
        // Tier 2: channel+sender
        config.gateway.bindings.push(GatewayBinding {
            channel: "telegram".to_string(),
            sender: Some("user1".to_string()),
            peer_kind: None,
            provider: None,
            model: Some("model-sender".to_string()),
            api_key_env: None,
            temperature: None,
            max_tokens: None,
            identity: None,
            memory_scope: None,
            fallback: Vec::new(),
            activation: None,
            thinking: None,
            request_timeout_ms: None,
            gateway_timeout_ms: None,
            stream_chunk_timeout_secs: None,
            inactivity_timeout_secs: None,
            inactivity_warning_secs: None,
            inactivity_notify_secs: None,
        });

        let route = resolve_route(&config, "telegram", "user1", None);
        // channel+sender (Tier 2) should win over channel-only (Tier 3)
        assert_eq!(route.matched_by, "binding.channel+sender");
        assert_eq!(route.config.llm.model, "model-sender");
    }

    #[test]
    fn resolve_route_non_matching_falls_through() {
        let mut config = Config::default();
        config.gateway.bindings.push(GatewayBinding {
            channel: "slack".to_string(),
            sender: None,
            peer_kind: None,
            provider: None,
            model: Some("slack-model".to_string()),
            api_key_env: None,
            temperature: None,
            max_tokens: None,
            identity: None,
            memory_scope: None,
            fallback: Vec::new(),
            activation: None,
            thinking: None,
            request_timeout_ms: None,
            gateway_timeout_ms: None,
            stream_chunk_timeout_secs: None,
            inactivity_timeout_secs: None,
            inactivity_warning_secs: None,
            inactivity_notify_secs: None,
        });

        let route = resolve_route(&config, "telegram", "user1", None);
        assert_eq!(route.binding_id, DEFAULT_BINDING_ID);
    }

    #[test]
    fn resolve_route_first_match_wins() {
        let mut config = Config::default();
        config.gateway.bindings.push(GatewayBinding {
            channel: "telegram".to_string(),
            sender: None,
            peer_kind: None,
            provider: None,
            model: Some("first-model".to_string()),
            api_key_env: None,
            temperature: None,
            max_tokens: None,
            identity: None,
            memory_scope: None,
            fallback: Vec::new(),
            activation: None,
            thinking: None,
            request_timeout_ms: None,
            gateway_timeout_ms: None,
            stream_chunk_timeout_secs: None,
            inactivity_timeout_secs: None,
            inactivity_warning_secs: None,
            inactivity_notify_secs: None,
        });
        config.gateway.bindings.push(GatewayBinding {
            channel: "telegram".to_string(),
            sender: None,
            peer_kind: None,
            provider: None,
            model: Some("second-model".to_string()),
            api_key_env: None,
            temperature: None,
            max_tokens: None,
            identity: None,
            memory_scope: None,
            fallback: Vec::new(),
            activation: None,
            thinking: None,
            request_timeout_ms: None,
            gateway_timeout_ms: None,
            stream_chunk_timeout_secs: None,
            inactivity_timeout_secs: None,
            inactivity_warning_secs: None,
            inactivity_notify_secs: None,
        });

        let route = resolve_route(&config, "telegram", "user1", None);
        assert_eq!(route.config.llm.model, "first-model");
    }

    #[test]
    fn session_key_includes_binding_id() {
        let mut config = Config::default();
        config.gateway.bindings.push(GatewayBinding {
            channel: "telegram".to_string(),
            sender: None,
            peer_kind: None,
            provider: None,
            model: None,
            api_key_env: None,
            temperature: None,
            max_tokens: None,
            identity: None,
            memory_scope: None,
            fallback: Vec::new(),
            activation: None,
            thinking: None,
            request_timeout_ms: None,
            gateway_timeout_ms: None,
            stream_chunk_timeout_secs: None,
            inactivity_timeout_secs: None,
            inactivity_warning_secs: None,
            inactivity_notify_secs: None,
        });

        let route = resolve_route(&config, "telegram", "user1", None);
        // binding_id is the session-isolation key: bind:{channel}:{sender}:{peer_kind}.
        // A channel-only binding has wildcard sender/peer, so both render as "*".
        // Two distinct bindings must never collide on this id.
        assert_eq!(route.binding_id, "bind:telegram:*:*");
    }

    #[test]
    fn activation_propagates_from_binding() {
        let mut config = Config::default();
        config.gateway.bindings.push(GatewayBinding {
            channel: "telegram".to_string(),
            sender: None,
            peer_kind: None,
            provider: None,
            model: None,
            api_key_env: None,
            temperature: None,
            max_tokens: None,
            identity: None,
            memory_scope: None,
            fallback: Vec::new(),
            activation: Some(ActivationMode::Always),
            thinking: None,
            request_timeout_ms: None,
            gateway_timeout_ms: None,
            stream_chunk_timeout_secs: None,
            inactivity_timeout_secs: None,
            inactivity_warning_secs: None,
            inactivity_notify_secs: None,
        });

        let route = resolve_route(&config, "telegram", "user1", None);
        assert_eq!(route.activation, Some(ActivationMode::Always));
    }

    #[test]
    fn thinking_override_propagates_from_binding() {
        use cortex::config::ThinkingLevel;
        let mut config = Config::default();
        config.gateway.bindings.push(GatewayBinding {
            channel: "telegram".to_string(),
            sender: None,
            peer_kind: None,
            provider: None,
            model: None,
            api_key_env: None,
            temperature: None,
            max_tokens: None,
            identity: None,
            memory_scope: None,
            fallback: Vec::new(),
            activation: None,
            thinking: Some(ThinkingLevel::High),
            request_timeout_ms: None,
            gateway_timeout_ms: None,
            stream_chunk_timeout_secs: None,
            inactivity_timeout_secs: None,
            inactivity_warning_secs: None,
            inactivity_notify_secs: None,
        });

        let route = resolve_route(&config, "telegram", "user1", None);
        assert_eq!(route.config.llm.thinking, ThinkingLevel::High);
    }

    #[test]
    fn default_route_has_no_activation() {
        let config = Config::default();
        let route = resolve_route(&config, "unknown", "user1", None);
        assert!(route.activation.is_none());
    }

    #[test]
    fn timeout_overrides_propagate_from_binding() {
        let mut config = Config::default();
        config.gateway.bindings.push(GatewayBinding {
            channel: "telegram".to_string(),
            sender: None,
            peer_kind: None,
            provider: None,
            model: None,
            api_key_env: None,
            temperature: None,
            max_tokens: None,
            identity: None,
            memory_scope: None,
            fallback: Vec::new(),
            activation: None,
            thinking: None,
            request_timeout_ms: Some(300_000),
            gateway_timeout_ms: Some(360_000),
            stream_chunk_timeout_secs: Some(120),
            inactivity_timeout_secs: None,
            inactivity_warning_secs: None,
            inactivity_notify_secs: None,
        });

        let route = resolve_route(&config, "telegram", "user1", None);
        assert_eq!(route.config.llm.request_timeout_ms, 300_000);
        assert_eq!(route.config.gateway.request_timeout_ms, 360_000);
        assert_eq!(route.config.llm.stream_chunk_timeout_secs, 120);
    }

    #[test]
    fn wrong_channel_never_matches_even_when_sender_and_peer_do() {
        // Tier matching is a conjunction: a binding for another channel must
        // not capture a message just because its sender/peer filters happen
        // to match. Otherwise a slack-only credential override would leak
        // onto telegram traffic.
        let mut config = Config::default();
        let mut binding = bare_binding("slack", Some("user1"), Some("group"));
        binding.model = Some("slack-only-model".to_string());
        config.gateway.bindings.push(binding);

        let route = resolve_route(&config, "telegram", "user1", Some("group"));
        assert_eq!(route.binding_id, DEFAULT_BINDING_ID);
        assert_ne!(route.config.llm.model, "slack-only-model");
    }

    fn fallback_entry(model: &str) -> cortex::config::LlmFallback {
        cortex::config::LlmFallback {
            provider: "openrouter".to_string(),
            model: model.to_string(),
            api_key: None,
            api_key_env: None,
            api_keys: Vec::new(),
            temperature: None,
            max_tokens: None,
            base_url: None,
        }
    }

    #[test]
    fn fallback_override_applies_only_when_binding_sets_it() {
        // Non-empty binding fallback replaces the base chain.
        let mut config = Config::default();
        config.llm.fallback = vec![fallback_entry("base-fallback")];
        let mut binding = bare_binding("telegram", None, None);
        binding.fallback = vec![fallback_entry("binding-fallback")];
        config.gateway.bindings.push(binding);
        let route = resolve_route(&config, "telegram", "user1", None);
        assert_eq!(route.config.llm.fallback.len(), 1);
        assert_eq!(route.config.llm.fallback[0].model, "binding-fallback");

        // An empty binding fallback means "no override" — the base chain
        // must survive, not be cleared.
        let mut config = Config::default();
        config.llm.fallback = vec![fallback_entry("base-fallback")];
        config
            .gateway
            .bindings
            .push(bare_binding("telegram", None, None));
        let route = resolve_route(&config, "telegram", "user1", None);
        assert_eq!(route.config.llm.fallback.len(), 1);
        assert_eq!(route.config.llm.fallback[0].model, "base-fallback");
    }

    #[test]
    fn identity_traversal_paths_are_rejected_safe_relative_applies() {
        // Each traversal shape must independently block the override —
        // otherwise a binding identity can escape the data dir.
        for bad in ["../outside.md", "/etc/passwd", "\\windows\\evil"] {
            let mut config = Config::default();
            let mut binding = bare_binding("telegram", None, None);
            binding.identity = Some(bad.to_string());
            config.gateway.bindings.push(binding);
            let route = resolve_route(&config, "telegram", "user1", None);
            assert!(
                route.config.identity_override.is_none(),
                "identity {bad:?} must not set identity_override"
            );
        }

        // A safe relative path resolves inside the data dir.
        let mut config = Config::default();
        let mut binding = bare_binding("telegram", None, None);
        binding.identity = Some("personas/work.md".to_string());
        config.gateway.bindings.push(binding);
        let route = resolve_route(&config, "telegram", "user1", None);
        let over = route.config.identity_override.expect("override applied");
        assert!(over.ends_with("personas/work.md"), "{over:?}");
    }

    #[test]
    fn gateway_timeout_auto_bumped_to_match_llm_timeout() {
        // The gateway default is generous (30 min), so to exercise the
        // auto-bump rule we explicitly set a small gateway base timeout
        // and a larger per-binding LLM timeout. The route should bump
        // gateway back up to match LLM.
        let mut config = Config::default();
        config.gateway.request_timeout_ms = 60_000;
        config.gateway.bindings.push(GatewayBinding {
            channel: "telegram".to_string(),
            sender: None,
            peer_kind: None,
            provider: None,
            model: None,
            api_key_env: None,
            temperature: None,
            max_tokens: None,
            identity: None,
            memory_scope: None,
            fallback: Vec::new(),
            activation: None,
            thinking: None,
            request_timeout_ms: Some(300_000),
            gateway_timeout_ms: None,
            stream_chunk_timeout_secs: None,
            inactivity_timeout_secs: None,
            inactivity_warning_secs: None,
            inactivity_notify_secs: None,
        });

        let route = resolve_route(&config, "telegram", "user1", None);
        assert_eq!(route.config.llm.request_timeout_ms, 300_000);
        assert_eq!(route.config.gateway.request_timeout_ms, 300_000);
    }
}
