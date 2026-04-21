//! Single source of truth for all settings: keys, field paths, setter kinds,
//! and extractor functions. The `define_settings!` macro generates both
//! `Config::apply_setting()` and `SETTING_REGISTRY` from this table.

use anyhow::Context;

define_settings! {
    registry_and_apply {
        // ── LLM core ──
        "model" => llm.model, string;
        "temperature" => llm.temperature, range(f32, 0.0_f32, 2.0);
        "max_tokens" => llm.max_tokens, nonzero(u32);
        "llm.api_key_env" => llm.api_key_env, string;
        "llm.max_retries" => llm.max_retries, parsed(u32);
        "llm.initial_retry_delay_ms" => llm.initial_retry_delay_ms, parsed(u64);
        "llm.request_timeout_ms" => llm.request_timeout_ms, parsed(u64);
        "llm.stream_chunk_timeout_secs" => llm.stream_chunk_timeout_secs, parsed(u64);
        "llm.base_url" => llm.base_url, opt_string;
        "llm.thinking" => llm.thinking, json_quoted("Invalid thinking level");
        "llm.fallback" => llm.fallback, json_count("providers");
        "llm.api_keys" => llm.api_keys, json_count("keys");

        // ── LLM cache ──
        "llm.cache.enabled" => llm.cache.enabled, parsed(bool);
        "llm.cache.ttl" => llm.cache.ttl, json_quoted("Invalid cache TTL");
        "llm.cache.cache_tools" => llm.cache.cache_tools, parsed(bool);
        "llm.cache.cache_system" => llm.cache.cache_system, parsed(bool);
        "llm.cache.rolling_messages" => llm.cache.rolling_messages, parsed(u8);
        "llm.cache.strategy" => llm.cache.strategy, json_quoted("Invalid cache strategy");

        // ── Sandbox ──
        "sandbox.enabled" => sandbox.enabled, parsed(bool);

        // ── Memory ──
        "memory.max_context_tokens" => memory.max_context_tokens, nonzero(usize);
        "memory.flush_before_compaction" => memory.flush_before_compaction, parsed(bool);
        "memory.flush_min_messages" => memory.flush_min_messages, parsed(usize);
        "memory.flush_soft_threshold_tokens" => memory.flush_soft_threshold_tokens, parsed(usize);
        "memory.chunk_level_selection" => memory.chunk_level_selection, parsed(bool);

        // ── Memory embeddings ──
        "memory.embeddings.enabled" => memory.embeddings.enabled, parsed(bool);
        "memory.embeddings.mmr_enabled" => memory.embeddings.mmr_enabled, parsed(bool);
        "memory.embeddings.mmr_lambda" => memory.embeddings.mmr_lambda, range(f32, 0.0_f32, 1.0);
        "memory.embeddings.recency_weight" => memory.embeddings.recency_weight, range(f32, 0.0_f32, 1.0);
        "memory.embeddings.bm25_weight" => memory.embeddings.bm25_weight, range(f32, 0.0_f32, 1.0);
        "memory.embeddings.vector_weight" => memory.embeddings.vector_weight, range(f32, 0.0_f32, 1.0);
        "memory.embeddings.vector_threshold_factor" => memory.embeddings.vector_threshold_factor, range(f32, 0.0_f32, 2.0);
        "memory.embeddings.chunk_size_tokens" => memory.embeddings.chunk_size_tokens, parsed(usize);
        "memory.embeddings.chunk_overlap_tokens" => memory.embeddings.chunk_overlap_tokens, parsed(usize);

        // ── Skills ──
        "skills.enabled" => skills.enabled, parsed(bool);
        "skills.max_context_tokens" => skills.max_context_tokens, parsed(usize);
        // skills.budget_pct is custom — see `custom_apply` block below.

        // ── Conversation ──
        "conversation.max_iterations" => conversation.max_iterations, parsed(u32);
        "conversation.show_thinking" => conversation.show_thinking, parsed(bool);
        "conversation.max_history_tokens" => conversation.max_history_tokens, parsed(usize);
        "conversation.tool_output_max_tokens" => conversation.tool_output_max_tokens, parsed(usize);
        "conversation.compaction_marker_tokens" => conversation.compaction_marker_tokens, parsed(usize);
        "conversation.max_transcript_chars" => conversation.max_transcript_chars, parsed(usize);
        "conversation.age_based_degradation" => conversation.age_based_degradation, parsed(bool);
        "conversation.protect_first_n" => conversation.protect_first_n, parsed(usize);
        "conversation.concurrent_tools.enabled" => conversation.concurrent_tools.enabled, parsed(bool);
        "conversation.concurrent_tools.max_workers" => conversation.concurrent_tools.max_workers, parsed(usize);

        // ── Security ──
        "security.secret_detection" => security.secret_detection, parsed(bool);
        "security.host_audit" => security.host_audit, parsed(bool);
        "security.blocked_paths" => security.blocked_paths, json;
        "security.allowed_paths" => security.allowed_paths, json;
        "security.action_limits" => security.action_limits, json_set;
        "security.gateway_action_limits" => security.gateway_action_limits, json_set;

        // ── Budget ──
        "budget.monthly_token_limit" => budget.monthly_token_limit, parsed(u64);
        "budget.warning_threshold" => budget.warning_threshold, range(f64, 0.0_f64, 1.0);

        // ── Browser ──
        "browser.enabled" => browser.enabled, parsed(bool);
        "browser.headless" => browser.headless, parsed(bool);

        // ── TTS ──
        "tts.enabled" => tts.enabled, parsed(bool);
        "tts.auto_mode" => tts.auto_mode, parsed(bool);
        "tts.default_voice" => tts.default_voice, string;
        "tts.max_text_length" => tts.max_text_length, parsed(usize);
        "tts.timeout_ms" => tts.timeout_ms, parsed(u64);
        "tts.models" => tts.models, json_count("models");

        // ── Evolution ──
        "evolution.enabled" => evolution.enabled, parsed(bool);
        "evolution.ambient_header_enabled" => evolution.ambient_header_enabled, parsed(bool);

        // ── Tools ──
        "tools.default_timeout_ms" => tools.default_timeout_ms, parsed(u64);
        "tools.conditional_loading" => tools.conditional_loading, parsed(bool);
        "tools.compact_schemas" => tools.compact_schemas, parsed(bool);
        "tools.policy.profile" => tools.policy.profile, string;
        "tools.policy.allow" => tools.policy.allow, json;
        "tools.policy.deny" => tools.policy.deny, json;
        "tools.policy.subagent_deny" => tools.policy.subagent_deny, json;

        // ── Heartbeat ──
        "heartbeat.interval" => heartbeat.interval, string;
        "heartbeat.quiet_hours_start" => heartbeat.quiet_hours_start, opt_string;
        "heartbeat.quiet_hours_end" => heartbeat.quiet_hours_end, opt_string;
        "heartbeat.cron" => heartbeat.cron, opt_string;
        "heartbeat.channels" => heartbeat.channels, json;
        "heartbeat.recipients" => heartbeat.recipients, json_set;
        "heartbeat.session_start_enabled" => heartbeat.session_start_enabled, parsed(bool);
        "heartbeat.session_start_throttle_minutes" => heartbeat.session_start_throttle_minutes, parsed(u32);
        "heartbeat.last_fired_at" => heartbeat.last_fired_at, parsed(i64);

        // ── User ──
        "user.name" => user.name, opt_string;
        "user.agent_name" => user.agent_name, opt_string;
        "user.timezone" => user.timezone, opt_string;

        // ── Web ──
        "web.enabled" => web.enabled, parsed(bool);
        "web.search_provider" => web.search_provider, string;

        // ── Tasks ──
        "tasks.max_concurrent" => tasks.max_concurrent, parsed(usize);

        // ── Gateway ──
        "gateway.host" => gateway.host, string;
        "gateway.port" => gateway.port, parsed(u16);
        "gateway.max_concurrent" => gateway.max_concurrent, parsed(usize);
        "gateway.request_timeout_ms" => gateway.request_timeout_ms, parsed(u64);
        "gateway.inactivity_timeout_secs" => gateway.inactivity_timeout_secs, parsed(u64);
        "gateway.inactivity_warning_secs" => gateway.inactivity_warning_secs, parsed(u64);
        "gateway.inactivity_notify_secs" => gateway.inactivity_notify_secs, parsed(u64);
        "gateway.rate_limit_per_minute" => gateway.rate_limit_per_minute, parsed(u32);
        "gateway.public_url" => gateway.public_url, opt_string;
        "gateway.pairing_ttl_secs" => gateway.pairing_ttl_secs, parsed(i64);
        "gateway.error_cooldown_ms" => gateway.error_cooldown_ms, parsed(u64);
        "gateway.dm_policy" => gateway.dm_policy, json_quoted("Invalid DM policy");
        "gateway.group_activation" => gateway.group_activation, json_quoted("Invalid activation mode");
        "gateway.bindings" => gateway.bindings, json_count("bindings");
        "gateway.channel_policies" => gateway.channel_policies, json_set;
        "gateway.auto_reply" => gateway.auto_reply, json_set;
        "gateway.link_understanding" => gateway.link_understanding, json_set;
        "gateway.channel_error_policies" => gateway.channel_error_policies, json_set;

        // ── Agents ──
        "agents.enabled" => agents.enabled, parsed(bool);
        "agents.max_spawn_depth" => agents.max_spawn_depth, parsed(u32);
        "agents.max_children_per_agent" => agents.max_children_per_agent, parsed(u32);
        "agents.max_concurrent" => agents.max_concurrent, parsed(u32);
        "agents.delegate_timeout_secs" => agents.delegate_timeout_secs, parsed(u64);

        // ── Debug ──
        "debug.llm_logging" => debug.llm_logging, parsed(bool);

        // ── Audio ──
        "audio.enabled" => audio.enabled, parsed(bool);
        "audio.models" => audio.models, json_count("models");

        // ── Media ──
        "media.max_image_bytes" => media.max_image_bytes, parsed(usize);
        "media.compression_enabled" => media.compression_enabled, parsed(bool);
        "media.max_dimension_px" => media.max_dimension_px, parsed(u32);

        // ── Image Gen ──
        "image_gen.enabled" => image_gen.enabled, parsed(bool);
        "image_gen.default_size" => image_gen.default_size, string;

        // ── Scripts ──
        "scripts.enabled" => scripts.enabled, parsed(bool);
        "scripts.default_timeout_ms" => scripts.default_timeout_ms, parsed(u64);

        // ── Hooks ──
        "hooks.enabled" => hooks.enabled, parsed(bool);

        // ── Compaction ──
        "compaction.provider" => compaction.provider, opt_string;
        "compaction.model" => compaction.model, opt_string;

        // ── Plugins ──
        "plugins.enabled" => plugins.enabled, parsed(bool);
        "plugins.auto_verify" => plugins.auto_verify, parsed(bool);

        // ── Credentials ──
        "credentials" => credentials, json_count("entries");
    }

    registry_only {
        // Gateway fields that are read-only (no apply_setting arm)
        "gateway.max_body_size" => |c| format!("{}", c.gateway.max_body_size);
        "gateway.telegram_poll_timeout_secs" => |c| format!("{}", c.gateway.telegram_poll_timeout_secs);
        "gateway.telegram_circuit_failure_threshold" => |c| format!("{}", c.gateway.telegram_circuit_failure_threshold);
        "gateway.telegram_circuit_suspension_secs" => |c| format!("{}", c.gateway.telegram_circuit_suspension_secs);
        "gateway.telegram_dedup_capacity" => |c| format!("{}", c.gateway.telegram_dedup_capacity);
    }

    custom_apply {
        // llm.claude_cli_path: opt_string with custom "(auto-detect)" display
        "llm.claude_cli_path" => |s, key, value| {
            s.llm.claude_cli_path = if value.is_empty() {
                None
            } else {
                Some(value.to_string())
            };
            Ok(format!("{key} = {value}"))
        };

        // provider: always wraps in Some (never None)
        "provider" => |s, key, value| {
            s.llm.provider = Some(value.to_string());
            Ok(format!("{key} = {value}"))
        };

        // llm.api_key: optional JSON SecretRef
        "llm.api_key" => |s, key, value| {
            if value.is_empty() {
                s.llm.api_key = None;
            } else {
                s.llm.api_key = Some(
                    serde_json::from_str(value)
                        .with_context(|| format!("Invalid JSON for {key}"))?,
                );
            }
            Ok(format!("{key} = (set)"))
        };

        // sandbox.mode: validated enum string
        "sandbox.mode" => |s, key, value| {
            match value {
                "strict" | "permissive" => {}
                other => {
                    anyhow::bail!("Unknown sandbox mode '{other}'. Valid: strict, permissive")
                }
            }
            s.sandbox.mode = value.to_string();
            Ok(format!("{key} = {value}"))
        };

        // memory.extra_paths: comma-separated list
        "memory.extra_paths" => |s, key, value| {
            let paths: Vec<String> = value
                .split(',')
                .map(|p| p.trim().to_string())
                .filter(|p| !p.is_empty())
                .collect();
            s.memory.extra_paths = paths.clone();
            Ok(format!("{key} = {}", paths.join(", ")))
        };

        // conversation.collaboration_mode: FromStr enum
        "conversation.collaboration_mode" => |s, key, value| {
            let mode: crate::config::CollaborationMode = value.parse()?;
            s.conversation.collaboration_mode = mode;
            Ok(format!("{key} = {mode}"))
        };

        // tts.default_format: validated set
        "tts.default_format" => |s, key, value| {
            let allowed = ["mp3", "opus", "aac", "flac", "wav"];
            if !allowed.contains(&value) {
                anyhow::bail!("Invalid format: {value}. Allowed: {}", allowed.join(", "));
            }
            s.tts.default_format = value.to_string();
            Ok(format!("{key} = {value}"))
        };

        // workflow.enabled: tri-state (auto/on/off)
        "workflow.enabled" => |s, key, value| {
            match value {
                "auto" | "on" | "off" => {
                    s.workflow.enabled = value.to_string();
                    Ok(format!("{key} = {value}"))
                }
                _ => anyhow::bail!(
                    "Invalid value for workflow.enabled: {value}. Use 'auto', 'on', or 'off'."
                ),
            }
        };

        // gateway.error_policy: FromStr enum
        "gateway.error_policy" => |s, key, value| {
            s.gateway.error_policy = value.parse()?;
            Ok(format!("{key} = {value}"))
        };

        // skills.budget_pct: Option<f32> in [0.0, 1.0]; empty string clears to None
        "skills.budget_pct" => |s, key, value| {
            if value.trim().is_empty() {
                s.skills.budget_pct = None;
                Ok(format!("{key} = (none)"))
            } else {
                let pct = crate::config::parse_range::<f32>(value, key, 0.0, 1.0)?;
                s.skills.budget_pct = Some(pct);
                Ok(format!("{key} = {pct}"))
            }
        };
    }

    custom_extract {
        "provider" => |c| {
            c.llm.provider.as_deref().unwrap_or("(auto-detect)").to_string()
        };
        "llm.claude_cli_path" => |c| {
            c.llm.claude_cli_path.as_deref().unwrap_or("(auto-detect)").to_string()
        };
        "llm.api_key" => |c| {
            c.llm.api_key.as_ref()
                .map(|sr| serde_json::to_string(sr).unwrap_or_default())
                .unwrap_or_default()
        };
        "sandbox.mode" => |c| c.sandbox.mode.clone();
        "memory.extra_paths" => |c| c.memory.extra_paths.join(", ");
        "conversation.collaboration_mode" => |c| format!("{}", c.conversation.collaboration_mode);
        "tts.default_format" => |c| c.tts.default_format.clone();
        "workflow.enabled" => |c| c.workflow.enabled.clone();
        "gateway.error_policy" => |c| format!("{}", c.gateway.error_policy);
        "skills.budget_pct" => |c| match c.skills.budget_pct {
            Some(pct) => format!("{pct}"),
            None => "(none)".to_string(),
        };
    }

    // TUI-visible settings, in render order. Category breaks produce section
    // headers in the popup. Entries render top-to-bottom: Essentials first,
    // Advanced last.
    tui_settings {
        // — Essentials — what every user sees first
        "provider" => "Provider", Select, "Essentials";
        "model" => "Model", Select, "Essentials";
        "llm.api_key" => "API key", Secret, "Essentials";
        "llm.api_key_env" => "API key env var", Text, "Essentials";
        "temperature" => "Temperature", Float, "Essentials";
        "conversation.collaboration_mode" => "Mode", Select, "Essentials";
        "budget.monthly_token_limit" => "Monthly token limit", Uint, "Essentials";

        // — Conversation — day-to-day tuning
        "max_tokens" => "Response length", Uint, "Conversation";
        "conversation.show_thinking" => "Show reasoning", Bool, "Conversation";
        "conversation.max_iterations" => "Max agent steps", Uint, "Conversation";
        "conversation.concurrent_tools.enabled" => "Parallel tools", Bool, "Conversation";

        // — Memory & Skills —
        "skills.enabled" => "Allow skills", Bool, "Memory & Skills";

        // — Personality —
        "evolution.enabled" => "Evolution", Bool, "Personality";
        "evolution.ambient_header_enabled" => "Ambient header", Bool, "Personality";

        // — Heartbeat —
        "heartbeat.session_start_enabled" => "Greet on TUI open", Bool, "Heartbeat";
        "heartbeat.session_start_throttle_minutes" => "Greeting throttle (min)", Uint, "Heartbeat";

        // — Voice —
        "tts.enabled" => "Enabled", Bool, "Voice";
        "tts.auto_mode" => "Auto reply", Bool, "Voice";

        // — Security —
        "sandbox.enabled" => "Sandbox", Bool, "Security";
        "hooks.enabled" => "Allow user hooks", Bool, "Security";
        "security.secret_detection" => "Secret detection", Bool, "Security";

        // — Advanced — power-user / esoteric knobs
        "llm.cache.strategy" => "Cache layout", Select, "Advanced";
        "conversation.concurrent_tools.max_workers" => "Parallel tool workers", Uint, "Advanced";
        "conversation.protect_first_n" => "Protected head msgs", Uint, "Advanced";
        "skills.budget_pct" => "Skills budget (% ctx)", Float, "Advanced";
        "budget.warning_threshold" => "Budget warning", Float, "Advanced";
        "workflow.enabled" => "Workflows", Select, "Advanced";
    }
}

/// Static choice lists for `Select`-kind TUI settings. Drives
/// `cycle_select` in the settings popup so choice arrays do not have to be
/// restated per-key. `provider` and `model` are dynamic (sourced from the
/// onboarding provider/model catalog) and are handled separately.
pub const TUI_SELECT_CHOICES: &[(&str, &[&str])] = &[
    (
        "conversation.collaboration_mode",
        &["default", "execute", "plan"],
    ),
    (
        "llm.cache.strategy",
        &["tools_system_and_2", "system_and_3"],
    ),
    ("workflow.enabled", &["auto", "on", "off"]),
];

/// `(min, max, step)` for `Float`-kind TUI settings. Drives `step_float` in
/// the settings popup. Entries mirror the `range(..)` bounds in the
/// `registry_and_apply` block above; the extra `step` is TUI-only.
pub const TUI_FLOAT_RANGES: &[(&str, f64, f64, f64)] = &[
    ("temperature", 0.0, 2.0, 0.1),
    ("budget.warning_threshold", 0.0, 1.0, 0.01),
    ("skills.budget_pct", 0.0, 1.0, 0.01),
];
