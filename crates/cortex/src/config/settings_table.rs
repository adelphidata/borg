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
        "llm.bedrock_region" => llm.bedrock_region, opt_string;
        "llm.bedrock_endpoint_url" => llm.bedrock_endpoint_url, opt_string;
        "llm.bedrock_use_fips" => llm.bedrock_use_fips, parsed(bool);
        "llm.azure_resource" => llm.azure_resource, opt_string;
        "llm.azure_deployment" => llm.azure_deployment, opt_string;
        "llm.azure_api_version" => llm.azure_api_version, opt_string;
        "llm.vertex_project" => llm.vertex_project, opt_string;
        "llm.vertex_location" => llm.vertex_location, opt_string;
        "llm.thinking" => llm.thinking, json_quoted("Invalid thinking level");
        "llm.thinking_overrides" => llm.thinking_overrides, json_count("overrides");
        "llm.fallback" => llm.fallback, json_count("providers");
        "llm.api_keys" => llm.api_keys, json_count("keys");

        // ── LLM cache ──
        "llm.cache.enabled" => llm.cache.enabled, parsed(bool);
        "llm.cache.ttl" => llm.cache.ttl, json_quoted("Invalid cache TTL");
        "llm.cache.cache_tools" => llm.cache.cache_tools, parsed(bool);
        "llm.cache.cache_system" => llm.cache.cache_system, parsed(bool);
        "llm.cache.rolling_messages" => llm.cache.rolling_messages, parsed(u8);
        "llm.cache.strategy" => llm.cache.strategy, json_quoted("Invalid cache strategy");

        // ── Credential pool ──
        "llm.credential_strategy" => llm.credential_strategy, json_quoted("Invalid credential strategy");
        "llm.credential_cooldown_secs" => llm.credential_cooldown_secs, parsed(u64);
        "llm.max_concurrent_per_credential" => llm.max_concurrent_per_credential, nonzero(u32);
        "llm.proactive_rate_limit_threshold_pct" => llm.proactive_rate_limit_threshold_pct, parsed(u8);
        "llm.proactive_rate_limit_min_window_secs" => llm.proactive_rate_limit_min_window_secs, parsed(u64);

        // ── Provider routing / air-gap ──
        "providers.outbound_allowed" => providers.outbound_allowed, parsed(bool);

        // ── Sandbox ──
        "sandbox.enabled" => sandbox.enabled, parsed(bool);

        // ── Memory ──
        // memory.provider handled in custom_apply (FromStr enum).
        "memory.mem0.organization" => memory.mem0.organization, opt_string;
        "memory.mem0.project" => memory.mem0.project, opt_string;
        "memory.mem0.base_url" => memory.mem0.base_url, opt_string;
        "memory.mem0.user_id" => memory.mem0.user_id, opt_string;
        "memory.honcho.app_id" => memory.honcho.app_id, opt_string;
        "memory.honcho.base_url" => memory.honcho.base_url, opt_string;
        "memory.honcho.peer_id" => memory.honcho.peer_id, opt_string;
        "memory.openviking.endpoint" => memory.openviking.endpoint, opt_string;
        "memory.openviking.account" => memory.openviking.account, opt_string;
        "memory.openviking.user" => memory.openviking.user, opt_string;
        "memory.openviking.agent" => memory.openviking.agent, opt_string;
        "memory.letta.agent_id" => memory.letta.agent_id, opt_string;
        "memory.letta.base_url" => memory.letta.base_url, opt_string;
        "memory.letta.project" => memory.letta.project, opt_string;
        "memory.letta.core_scope" => memory.letta.core_scope, opt_string;
        "memory.zep.base_url" => memory.zep.base_url, opt_string;
        "memory.zep.user_id" => memory.zep.user_id, opt_string;
        "memory.max_context_tokens" => memory.max_context_tokens, nonzero(usize);
        "memory.flush_before_compaction" => memory.flush_before_compaction, parsed(bool);
        "memory.flush_min_messages" => memory.flush_min_messages, parsed(usize);
        "memory.flush_soft_threshold_tokens" => memory.flush_soft_threshold_tokens, parsed(usize);
        "memory.exclude_external_tool_output" => memory.exclude_external_tool_output, parsed(bool);
        "memory.chunk_level_selection" => memory.chunk_level_selection, parsed(bool);
        "memory.active_injection.enabled" => memory.active_injection.enabled, parsed(bool);
        "memory.active_injection.top_k" => memory.active_injection.top_k, parsed(usize);
        "memory.active_injection.max_tokens" => memory.active_injection.max_tokens, parsed(usize);

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
        "conversation.context_window_guidance" => conversation.context_window_guidance, string;

        // ── Security ──
        "security.secret_detection" => security.secret_detection, parsed(bool);
        "security.host_audit" => security.host_audit, parsed(bool);
        "security.startup_audit" => security.startup_audit, parsed(bool);
        "security.output_redaction" => security.output_redaction, parsed(bool);
        "security.blocked_paths" => security.blocked_paths, json;
        "security.allowed_paths" => security.allowed_paths, json;
        "security.trusted_publisher_keys" => security.trusted_publisher_keys, json;
        "security.deprecated_publisher_keys" => security.deprecated_publisher_keys, json;
        "security.action_limits" => security.action_limits, json_set;
        "security.gateway_action_limits" => security.gateway_action_limits, json_set;
        // security.verifier handled in custom_apply below (FromStr enum).
        "security.url_policy.enabled" => security.url_policy.enabled, parsed(bool);
        "security.url_policy.allow_private_urls" => security.url_policy.allow_private_urls, parsed(bool);
        "security.url_policy.allowlist" => security.url_policy.allowlist, json;
        "security.url_policy.denylist" => security.url_policy.denylist, json;
        "security.url_policy.throttle_rules" => security.url_policy.throttle_rules, json;
        "security.url_policy.transform_rules" => security.url_policy.transform_rules, json;

        // ── Budget ──
        "budget.monthly_token_limit" => budget.monthly_token_limit, parsed(u64);
        "budget.warning_threshold" => budget.warning_threshold, range(f64, 0.0_f64, 1.0);

        // ── Browser ──
        "browser.enabled" => browser.enabled, parsed(bool);
        "browser.headless" => browser.headless, parsed(bool);
        "browser.executable" => browser.executable, opt_string;
        "browser.snapshot_max_elements" => browser.snapshot_max_elements, parsed(usize);
        "browser.cdp_url" => browser.cdp_url, opt_string;
        // backend / engine are FromStr enums → custom_apply / custom_extract.
        // API keys are JSON SecretRefs → custom_apply / custom_extract.
        "browser.browserbase_project_id" => browser.browserbase_project_id, opt_string;
        "browser.browserbase_proxies" => browser.browserbase_proxies, parsed(bool);
        "browser.browserbase_base_url" => browser.browserbase_base_url, opt_string;
        "browser.browser_use_base_url" => browser.browser_use_base_url, opt_string;
        "browser.firecrawl_base_url" => browser.firecrawl_base_url, opt_string;
        "browser.firecrawl_browser_ttl_secs" => browser.firecrawl_browser_ttl_secs, parsed(u64);

        // ── Computer use ──
        "computer_use.enabled" => computer_use.enabled, parsed(bool);
        "computer_use.driver_path" => computer_use.driver_path, string;
        "computer_use.cua_telemetry" => computer_use.cua_telemetry, parsed(bool);
        "computer_use.driver_timeout_secs" => computer_use.driver_timeout_secs, parsed(u64);

        // ── TTS ──
        "tts.enabled" => tts.enabled, parsed(bool);
        "tts.auto_mode" => tts.auto_mode, parsed(bool);
        "tts.default_voice" => tts.default_voice, string;
        "tts.max_text_length" => tts.max_text_length, parsed(usize);
        "tts.timeout_ms" => tts.timeout_ms, parsed(u64);
        "tts.models" => tts.models, json_count("models");

        // ── Voice ──
        "voice.enabled" => voice.enabled, parsed(bool);
        "voice.auto_tts" => voice.auto_tts, parsed(bool);
        "voice.beep_enabled" => voice.beep_enabled, parsed(bool);
        "voice.auto_submit" => voice.auto_submit, parsed(bool);
        "voice.silence_threshold_rms" => voice.silence_threshold_rms, parsed(u16);
        "voice.silence_duration_secs" => voice.silence_duration_secs, parsed(f32);
        "voice.min_speech_duration_secs" => voice.min_speech_duration_secs, parsed(f32);
        "voice.dip_tolerance_secs" => voice.dip_tolerance_secs, parsed(f32);
        "voice.max_wait_secs" => voice.max_wait_secs, parsed(f32);
        "voice.no_speech_limit" => voice.no_speech_limit, parsed(u8);
        "voice.sample_rate_hz" => voice.sample_rate_hz, parsed(u32);
        "voice.record_key" => voice.record_key, string;

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
        "heartbeat.min_interval_secs" => heartbeat.min_interval_secs, parsed(u64);
        "heartbeat.pre_run_command" => heartbeat.pre_run_command, string;
        "heartbeat.pre_run_cwd" => heartbeat.pre_run_cwd, string;
        "heartbeat.pre_run_timeout_ms" => heartbeat.pre_run_timeout_ms, parsed(u64);
        "heartbeat.turn_timeout_secs" => heartbeat.turn_timeout_secs, parsed(u64);
        "heartbeat.remediation_enabled" => heartbeat.remediation_enabled, parsed(bool);
        "heartbeat.remediation_timeout_secs" => heartbeat.remediation_timeout_secs, parsed(u64);
        "heartbeat.remediation_token_budget" => heartbeat.remediation_token_budget, parsed(u64);
        "heartbeat.remediation_max_ticks" => heartbeat.remediation_max_ticks, parsed(u32);

        // ── User ──
        "user.name" => user.name, opt_string;
        "user.agent_name" => user.agent_name, opt_string;
        "user.timezone" => user.timezone, opt_string;

        // ── Web ──
        "web.enabled" => web.enabled, parsed(bool);
        "web.search_provider" => web.search_provider, string;
        "web.search_cache_ttl_secs" => web.search_cache_ttl_secs, parsed(u64);

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
        "gateway.imessage_group_allowlist" => gateway.imessage_group_allowlist, json;
        "gateway.signal_cli_host" => gateway.signal_cli_host, opt_string;
        "gateway.signal_cli_port" => gateway.signal_cli_port, opt_parsed(u16);
        "gateway.discord_guild_allowlist" => gateway.discord_guild_allowlist, json;
        "gateway.slack_channel_allowlist" => gateway.slack_channel_allowlist, json;

        // ── Agents ──
        "agents.enabled" => agents.enabled, parsed(bool);
        "agents.max_spawn_depth" => agents.max_spawn_depth, parsed(u32);
        "agents.max_children_per_agent" => agents.max_children_per_agent, parsed(u32);
        "agents.max_concurrent" => agents.max_concurrent, parsed(u32);
        "agents.delegate_timeout_secs" => agents.delegate_timeout_secs, parsed(u64);
        "agents.orchestrator_enabled" => agents.orchestrator_enabled, parsed(bool);
        "agents.delegation_model" => agents.delegation_model, opt_string;
        "agents.delegation_provider" => agents.delegation_provider, opt_string;
        "agents.heartbeat_interval_secs" => agents.heartbeat_interval_secs, parsed(u64);
        "agents.heartbeat_stale_cycles" => agents.heartbeat_stale_cycles, parsed(u32);

        // ── Debug ──
        "debug.llm_logging" => debug.llm_logging, parsed(bool);

        // ── Telemetry ──
        "telemetry.cost_events" => telemetry.cost_events, parsed(bool);
        "telemetry.tracing_enabled" => telemetry.tracing_enabled, parsed(bool);
        "telemetry.metrics_enabled" => telemetry.metrics_enabled, parsed(bool);
        "telemetry.logs_enabled" => telemetry.logs_enabled, parsed(bool);
        "telemetry.otlp_endpoint" => telemetry.otlp_endpoint, string;
        "telemetry.service_name" => telemetry.service_name, string;
        "telemetry.sampling_ratio" => telemetry.sampling_ratio, range(f64, 0.0_f64, 1.0);

        // ── Audio ──
        "audio.enabled" => audio.enabled, parsed(bool);
        "audio.models" => audio.models, json_count("models");
        "audio.max_file_size" => audio.max_file_size, parsed(u64);
        "audio.min_file_size" => audio.min_file_size, parsed(u64);
        "audio.language" => audio.language, opt_string;
        "audio.timeout_ms" => audio.timeout_ms, parsed(u64);
        "audio.echo_transcript" => audio.echo_transcript, parsed(bool);

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
        "plugins.wasm.enabled" => plugins.wasm.enabled, parsed(bool);
        "plugins.wasm.disabled_plugin_ids" => plugins.wasm.disabled_plugin_ids, json;
        "plugins.wasm.disabled_publisher_keys" => plugins.wasm.disabled_publisher_keys, json;
        "plugins.wasm.rate_limit_per_minute" => plugins.wasm.rate_limit_per_minute, parsed(u32);
        "plugins.wasm.rate_limit_burst" => plugins.wasm.rate_limit_burst, parsed(u32);

        // ── MCP (Model Context Protocol) ──
        "mcp.enabled" => mcp.enabled, parsed(bool);
        "mcp.max_tools_total" => mcp.max_tools_total, parsed(usize);
        "mcp.allow_plaintext_http" => mcp.allow_plaintext_http, parsed(bool);
        "mcp.allow_local_network" => mcp.allow_local_network, parsed(bool);
        "mcp.disabled_servers" => mcp.disabled_servers, json;
        "mcp.servers" => mcp.servers, json;
        "mcp.max_response_bytes" => mcp.max_response_bytes, parsed(usize);
        "mcp.startup_max_retries" => mcp.startup_max_retries, parsed(u32);
        "mcp.startup_retry_base_ms" => mcp.startup_retry_base_ms, parsed(u64);
        "mcp.tool_search" => mcp.tool_search, parsed(bool);

        // ── Credentials ──
        "credentials" => credentials, json_count("entries");

        // ── Self-update prompt ──
        "update.skipped_version" => update.skipped_version, string;

        // ── Maintenance (self-healing) ──
        "maintenance.enabled" => maintenance.enabled, parsed(bool);
        "maintenance.logs_retention_days" => maintenance.logs_retention_days, parsed(u32);
        "maintenance.activity_retention_days" => maintenance.activity_retention_days, parsed(u32);
        "maintenance.doctor_runs_keep" => maintenance.doctor_runs_keep, parsed(u32);
        "maintenance.embedding_repair_enabled" => maintenance.embedding_repair_enabled, parsed(bool);
        "maintenance.embedding_repair_max_files" => maintenance.embedding_repair_max_files, parsed(u32);
        "maintenance.embedding_repair_max_chunks" => maintenance.embedding_repair_max_chunks, parsed(u32);
        "maintenance.issue_state_keep_days" => maintenance.issue_state_keep_days, parsed(u32);
        "maintenance.security_squelch_ttl_days" => maintenance.security_squelch_ttl_days, parsed(u32);
        "maintenance.vacuum_interval_days" => maintenance.vacuum_interval_days, parsed(u32);
        "maintenance.heal_action_budget" => maintenance.heal_action_budget, parsed(u32);

        // ── Agent-loop safety (stuck-loop backoff + unknown-tool guard) ──
        "loop_guard.identical_call_max" => loop_guard.identical_call_max, nonzero(u32);
        "loop_guard.backoff_initial_ms" => loop_guard.backoff_initial_ms, parsed(u64);
        "loop_guard.backoff_max_ms" => loop_guard.backoff_max_ms, parsed(u64);
        "loop_guard.unknown_tool_max_per_name" => loop_guard.unknown_tool_max_per_name, nonzero(u32);

        // ── Subprocess process registry ──
        "process_registry.orphan_warn_secs" => process_registry.orphan_warn_secs, parsed(u64);
        "process_registry.orphan_fail_secs" => process_registry.orphan_fail_secs, parsed(u64);
        "process_registry.auto_reap_secs" => process_registry.auto_reap_secs, parsed(u64);
        "process_registry.sigterm_grace_secs" => process_registry.sigterm_grace_secs, parsed(u64);
        "process_registry.never_kill_patterns" => process_registry.never_kill_patterns, json;
        "process_registry.finished_ttl_secs" => process_registry.finished_ttl_secs, parsed(u64);
        "process_registry.max_finished_entries" => process_registry.max_finished_entries, parsed(usize);

        // ── Per-session goals (/goal) ──
        "goals.enabled" => goals.enabled, parsed(bool);
        "goals.default_token_budget" => goals.default_token_budget, opt_parsed(i64);

        // ── Discovery (mDNS + Tailscale peer enumeration) ──
        "discovery.mdns_enabled" => discovery.mdns_enabled, parsed(bool);
        "discovery.mdns_instance_name" => discovery.mdns_instance_name, opt_string;
        "discovery.tailscale_enabled" => discovery.tailscale_enabled, parsed(bool);

        // ── Tunneling ──
        // tunnel.provider handled in custom_apply (FromStr enum).
        "tunnel.cloudflare.token" => tunnel.cloudflare.token, string;
        "tunnel.ngrok.auth_token" => tunnel.ngrok.auth_token, string;
        "tunnel.ngrok.domain" => tunnel.ngrok.domain, opt_string;
        "tunnel.tailscale.funnel" => tunnel.tailscale.funnel, parsed(bool);
        "tunnel.tailscale.hostname" => tunnel.tailscale.hostname, opt_string;
        "tunnel.custom.start_command" => tunnel.custom.start_command, string;
        "tunnel.custom.health_url" => tunnel.custom.health_url, opt_string;
        "tunnel.custom.url_pattern" => tunnel.custom.url_pattern, opt_string;

        // ── UI / theme ──
        // The active theme name. Validation is intentionally lax — the
        // resolver at the TUI layer falls back to the builtin `default`
        // with a `tracing::warn!` when the name is unknown (e.g. a user
        // theme file was deleted), so the agent always boots. Builtin
        // names + dynamically-discovered user themes drive the `/theme`
        // picker; this row also surfaces in `/settings` under "Appearance".
        "ui.theme" => ui.theme, string;

        // ── APNs ──
        "apns.enabled" => apns.enabled, parsed(bool);
        "apns.bundle_id" => apns.bundle_id, opt_string;
        "apns.team_id" => apns.team_id, opt_string;
        "apns.key_id" => apns.key_id, opt_string;
        "apns.use_sandbox" => apns.use_sandbox, parsed(bool);
        // apns.private_key_p8_path handled in custom_apply (PathBuf, not String).
    }

    registry_only {
        // First-run onboarding sentinel — written directly via `db.set_setting`
        // by the daemon/cli, not through `apply_setting`. Listed here so the
        // settings reload doesn't emit "Unknown setting" for it.
        "onboarded" => |_c| String::from("(sentinel)");
        // Gateway fields that are read-only (no apply_setting arm)
        "gateway.max_body_size" => |c| format!("{}", c.gateway.max_body_size);
        "gateway.telegram_poll_timeout_secs" => |c| format!("{}", c.gateway.telegram_poll_timeout_secs);
        "gateway.telegram_circuit_failure_threshold" => |c| format!("{}", c.gateway.telegram_circuit_failure_threshold);
        "gateway.telegram_circuit_suspension_secs" => |c| format!("{}", c.gateway.telegram_circuit_suspension_secs);
        "gateway.telegram_dedup_capacity" => |c| format!("{}", c.gateway.telegram_dedup_capacity);
    }

    custom_apply {
        // llm.claude_cli_path: opt_string. Empty or the legacy "(auto-detect)"
        // display sentinel both clear to None — earlier versions of the
        // extractor returned the sentinel as the round-trip value, so this
        // arm has to absorb whatever crept into the DB.
        "llm.claude_cli_path" => |s, key, value| {
            s.llm.claude_cli_path = if value.is_empty() || value == "(auto-detect)" {
                None
            } else {
                Some(value.to_string())
            };
            Ok(format!("{key} = {value}"))
        };

        // provider: opt_string. Same sentinel-clearing rule as claude_cli_path
        // — without this, an "unset" round-trip persisted "(auto-detect)" as
        // the literal provider name, which then failed Provider::from_str at
        // turn time with "Unknown provider: (auto-detect)".
        "provider" => |s, key, value| {
            s.llm.provider = if value.is_empty() || value == "(auto-detect)" {
                None
            } else {
                Some(value.to_string())
            };
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

        // browser.backend: FromStr enum (local | browserbase | browser_use | firecrawl | cdp_attach)
        "browser.backend" => |s, key, value| {
            let backend: crate::config::BrowserBackend = value.parse()?;
            s.browser.backend = backend;
            Ok(format!("{key} = {backend}"))
        };

        // browser.engine: FromStr enum (chrome | brave | edge | chromium | lightpanda | custom)
        "browser.engine" => |s, key, value| {
            let engine: crate::config::BrowserEngine = value.parse()?;
            s.browser.engine = engine;
            Ok(format!("{key} = {engine}"))
        };

        // browser.*_api_key: optional JSON SecretRef (empty clears to None).
        // Same shape as llm.api_key above.
        "browser.browserbase_api_key" => |s, key, value| {
            if value.is_empty() {
                s.browser.browserbase_api_key = None;
            } else {
                s.browser.browserbase_api_key = Some(
                    serde_json::from_str(value)
                        .with_context(|| format!("Invalid JSON for {key}"))?,
                );
            }
            Ok(format!("{key} = (set)"))
        };
        "browser.browser_use_api_key" => |s, key, value| {
            if value.is_empty() {
                s.browser.browser_use_api_key = None;
            } else {
                s.browser.browser_use_api_key = Some(
                    serde_json::from_str(value)
                        .with_context(|| format!("Invalid JSON for {key}"))?,
                );
            }
            Ok(format!("{key} = (set)"))
        };
        "browser.firecrawl_api_key" => |s, key, value| {
            if value.is_empty() {
                s.browser.firecrawl_api_key = None;
            } else {
                s.browser.firecrawl_api_key = Some(
                    serde_json::from_str(value)
                        .with_context(|| format!("Invalid JSON for {key}"))?,
                );
            }
            Ok(format!("{key} = (set)"))
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

        // gateway.error_policy: FromStr enum
        "gateway.error_policy" => |s, key, value| {
            s.gateway.error_policy = value.parse()?;
            Ok(format!("{key} = {value}"))
        };

        // security.verifier: FromStr enum (tofu | tailscale)
        "security.verifier" => |s, key, value| {
            let kind: crate::config::VerifierKind = value.parse()?;
            s.security.verifier = kind;
            Ok(format!("{key} = {kind}"))
        };

        // memory.provider: FromStr enum
        // (native | mem0 | honcho | openviking | letta | zep).
        // Default `native`. Selecting an external provider with missing
        // credentials falls back to native at resolve-time with a warn
        // so the agent always boots; here we just persist the chosen
        // string verbatim.
        "memory.provider" => |s, key, value| {
            let kind: crate::config::media::MemoryProviderKind = value.parse()?;
            s.memory.provider = kind;
            Ok(format!("{key} = {kind}"))
        };

        // memory.mem0.api_key / memory.honcho.api_key /
        // memory.openviking.api_key: optional JSON SecretRef. Empty string
        // clears to None. Same shape as `llm.api_key` above.
        "memory.mem0.api_key" => |s, key, value| {
            if value.is_empty() {
                s.memory.mem0.api_key = None;
            } else {
                s.memory.mem0.api_key = Some(
                    serde_json::from_str(value)
                        .with_context(|| format!("Invalid JSON for {key}"))?,
                );
            }
            Ok(format!("{key} = (set)"))
        };
        "memory.honcho.api_key" => |s, key, value| {
            if value.is_empty() {
                s.memory.honcho.api_key = None;
            } else {
                s.memory.honcho.api_key = Some(
                    serde_json::from_str(value)
                        .with_context(|| format!("Invalid JSON for {key}"))?,
                );
            }
            Ok(format!("{key} = (set)"))
        };
        "memory.openviking.api_key" => |s, key, value| {
            if value.is_empty() {
                s.memory.openviking.api_key = None;
            } else {
                s.memory.openviking.api_key = Some(
                    serde_json::from_str(value)
                        .with_context(|| format!("Invalid JSON for {key}"))?,
                );
            }
            Ok(format!("{key} = (set)"))
        };
        "memory.letta.api_key" => |s, key, value| {
            if value.is_empty() {
                s.memory.letta.api_key = None;
            } else {
                s.memory.letta.api_key = Some(
                    serde_json::from_str(value)
                        .with_context(|| format!("Invalid JSON for {key}"))?,
                );
            }
            Ok(format!("{key} = (set)"))
        };
        "memory.zep.api_key" => |s, key, value| {
            if value.is_empty() {
                s.memory.zep.api_key = None;
            } else {
                s.memory.zep.api_key = Some(
                    serde_json::from_str(value)
                        .with_context(|| format!("Invalid JSON for {key}"))?,
                );
            }
            Ok(format!("{key} = (set)"))
        };

        // security.kek_backend: FromStr enum (auto | os_keychain | file).
        // Install-time choice; the daemon resolves `Auto` to a concrete
        // value on first start and persists the result. Surfacing the
        // setter lets `borg service install --secure` write the chosen
        // backend before borgd boots.
        "security.kek_backend" => |s, key, value| {
            let kind: crate::config::security::KekBackend = value.parse()?;
            s.security.kek_backend = kind;
            Ok(format!("{key} = {kind}"))
        };

        // consolidation.last_nightly / last_weekly: Option<i64> unix-epoch
        // seconds, written by `consolidation::run_dream` via `db.set_setting`
        // on every successful pass. Empty string clears to None; non-empty
        // must parse as i64. Without these arms the resolver logged "Unknown
        // setting" on every reload after the first Dream run.
        "consolidation.last_nightly" => |s, key, value| {
            if value.trim().is_empty() {
                s.consolidation.last_nightly = None;
                Ok(format!("{key} = (none)"))
            } else {
                let ts = crate::config::parse_value::<i64>(value, key)?;
                s.consolidation.last_nightly = Some(ts);
                Ok(format!("{key} = {ts}"))
            }
        };
        "consolidation.last_weekly" => |s, key, value| {
            if value.trim().is_empty() {
                s.consolidation.last_weekly = None;
                Ok(format!("{key} = (none)"))
            } else {
                let ts = crate::config::parse_value::<i64>(value, key)?;
                s.consolidation.last_weekly = Some(ts);
                Ok(format!("{key} = {ts}"))
            }
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

        // llm.aux: Option<AuxLlmConfig> — entire aux blob. Empty string clears
        // to None (back to primary chain). Non-empty must deserialize into
        // AuxLlmConfig. The TUI Resilience category edits this whole blob;
        // sub-setters below cover the common sub-fields.
        "llm.aux" => |s, key, value| {
            if value.trim().is_empty() {
                s.llm.aux = None;
                Ok(format!("{key} = (unset)"))
            } else {
                let aux: crate::config::llm::AuxLlmConfig = serde_json::from_str(value)
                    .with_context(|| format!("Invalid JSON for {key}"))?;
                s.llm.aux = Some(aux);
                Ok(format!("{key} = (set)"))
            }
        };

        // llm.aux.model: convenience setter for the most commonly edited
        // sub-field. Non-empty value auto-creates aux when None so the user
        // can pick a model from the TUI Resilience row without first writing
        // a full JSON blob. Empty value is a no-op — the extractor returns
        // empty when aux is unset, so an empty round-trip must not
        // spuriously materialize aux on every reload. To reset to the
        // resolver's default, set this to the literal "auto"; to clear aux
        // entirely, use the parent `llm.aux` setter with an empty value.
        "llm.aux.model" => |s, key, value| {
            if !value.is_empty() {
                s.llm.aux
                    .get_or_insert_with(crate::config::llm::AuxLlmConfig::default)
                    .model = value.to_string();
            }
            Ok(format!("{key} = {value}"))
        };

        // llm.aux.fallback: Vec<LlmFallback> for the aux chain. A non-empty
        // JSON list auto-creates aux when None (TUI quick-edit path). Empty
        // string is a no-op when aux is None — same back-compat invariant
        // as llm.aux.model. When aux exists, an empty string clears the
        // fallback list while preserving the rest of the aux block.
        "llm.aux.fallback" => |s, key, value| {
            if value.trim().is_empty() {
                if let Some(aux) = s.llm.aux.as_mut() {
                    aux.fallback.clear();
                }
                Ok(format!("{key} = (0 providers)"))
            } else {
                let fallback: Vec<crate::config::llm::LlmFallback> = serde_json::from_str(value)
                    .with_context(|| format!("Invalid JSON for {key}"))?;
                let count = fallback.len();
                s.llm.aux
                    .get_or_insert_with(crate::config::llm::AuxLlmConfig::default)
                    .fallback = fallback;
                Ok(format!("{key} = ({count} providers)"))
            }
        };

        // tunnel.provider: FromStr-validated enum. Empty maps to None so an
        // unset row clears any prior selection rather than silently
        // persisting a stale provider.
        "tunnel.provider" => |s, key, value| {
            use std::str::FromStr as _;
            s.tunnel.provider = crate::config::tunnel::TunnelProvider::from_str(value)?;
            Ok(format!("{key} = {value}"))
        };

        // apns.private_key_p8_path: PathBuf. Empty clears to None.
        "apns.private_key_p8_path" => |s, key, value| {
            s.apns.private_key_p8_path = if value.is_empty() {
                None
            } else {
                Some(std::path::PathBuf::from(value))
            };
            Ok(format!("{key} = {value}"))
        };
    }

    custom_extract {
        // Round-trippable: empty string when None. The "(auto-detect)" string
        // is for human display only and must never be persisted, since it is
        // not a valid Provider name.
        "provider" => |c| {
            c.llm.provider.clone().unwrap_or_default()
        };
        "llm.claude_cli_path" => |c| {
            c.llm.claude_cli_path.clone().unwrap_or_default()
        };
        "llm.api_key" => |c| {
            c.llm.api_key.as_ref()
                .map(|sr| serde_json::to_string(sr).unwrap_or_default())
                .unwrap_or_default()
        };
        "sandbox.mode" => |c| c.sandbox.mode.clone();
        "memory.extra_paths" => |c| c.memory.extra_paths.join(", ");
        "conversation.collaboration_mode" => |c| format!("{}", c.conversation.collaboration_mode);
        // browser backend / engine: FromStr-validated enums, round-trip via Display.
        "browser.backend" => |c| format!("{}", c.browser.backend);
        "browser.engine" => |c| format!("{}", c.browser.engine);
        // browser.*_api_key: round-trip the SecretRef as JSON, empty when unset.
        "browser.browserbase_api_key" => |c| c.browser.browserbase_api_key.as_ref()
            .map(|sr| serde_json::to_string(sr).unwrap_or_default())
            .unwrap_or_default();
        "browser.browser_use_api_key" => |c| c.browser.browser_use_api_key.as_ref()
            .map(|sr| serde_json::to_string(sr).unwrap_or_default())
            .unwrap_or_default();
        "browser.firecrawl_api_key" => |c| c.browser.firecrawl_api_key.as_ref()
            .map(|sr| serde_json::to_string(sr).unwrap_or_default())
            .unwrap_or_default();
        "tts.default_format" => |c| c.tts.default_format.clone();
        "gateway.error_policy" => |c| format!("{}", c.gateway.error_policy);
        "security.verifier" => |c| format!("{}", c.security.verifier);
        "security.kek_backend" => |c| format!("{}", c.security.kek_backend);
        "skills.budget_pct" => |c| match c.skills.budget_pct {
            Some(pct) => format!("{pct}"),
            None => "(none)".to_string(),
        };
        "consolidation.last_nightly" => |c| match c.consolidation.last_nightly {
            Some(ts) => ts.to_string(),
            None => String::new(),
        };
        "consolidation.last_weekly" => |c| match c.consolidation.last_weekly {
            Some(ts) => ts.to_string(),
            None => String::new(),
        };
        // Aux chain: round-trip the whole blob as JSON when set, empty when
        // unset. Mirrors the apply contract (empty string = unset).
        "llm.aux" => |c| match &c.llm.aux {
            Some(aux) => serde_json::to_string(aux).unwrap_or_default(),
            None => String::new(),
        };
        // Round-trip empty when aux is unset — feeding "auto" back through
        // apply_setting would auto-create aux on every reload, breaking the
        // back-compat default-None contract. The TUI extractor adapter can
        // substitute "auto" at display time if it wants a friendlier label.
        "llm.aux.model" => |c| c
            .llm
            .aux
            .as_ref()
            .map(|a| a.model.clone())
            .unwrap_or_default();
        // Round-trip empty when aux is unset OR fallback is empty — same
        // rationale as llm.aux.model: feeding "[]" back through apply_setting
        // would auto-create aux on every reload.
        "llm.aux.fallback" => |c| c
            .llm
            .aux
            .as_ref()
            .filter(|a| !a.fallback.is_empty())
            .map(|a| serde_json::to_string(&a.fallback).unwrap_or_default())
            .unwrap_or_default();

        // tunnel.provider: FromStr-validated enum, round-trip via Display.
        "tunnel.provider" => |c| format!("{}", c.tunnel.provider);

        // apns.private_key_p8_path: PathBuf, round-trip via display.
        "apns.private_key_p8_path" => |c| c
            .apns
            .private_key_p8_path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
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
        "llm.bedrock_region" => "Bedrock region", Text, "Essentials";
        "llm.bedrock_endpoint_url" => "Bedrock endpoint URL", Text, "Essentials";
        "llm.bedrock_use_fips" => "Bedrock FIPS endpoints", Bool, "Essentials";
        "llm.azure_resource" => "Azure resource", Text, "Essentials";
        "llm.azure_deployment" => "Azure deployment", Text, "Essentials";
        "llm.azure_api_version" => "Azure API version", Text, "Essentials";
        "llm.vertex_project" => "Vertex project", Text, "Essentials";
        "llm.vertex_location" => "Vertex location", Text, "Essentials";
        "temperature" => "Temperature", Float, "Essentials";
        "conversation.collaboration_mode" => "Mode", Select, "Essentials";
        "budget.monthly_token_limit" => "Monthly token limit", Uint, "Essentials";

        // — Conversation — day-to-day tuning
        "max_tokens" => "Response length", Uint, "Conversation";
        "llm.thinking" => "Thinking level", Select, "Conversation";
        "conversation.show_thinking" => "Show reasoning", Bool, "Conversation";
        "conversation.max_iterations" => "Max agent steps", Uint, "Conversation";
        "conversation.tool_output_max_tokens" => "Tool output cap (tokens)", Uint, "Conversation";
        "conversation.concurrent_tools.enabled" => "Parallel tools", Bool, "Conversation";

        // — Memory & Skills —
        "skills.enabled" => "Allow skills", Bool, "Memory & Skills";
        "memory.active_injection.enabled" => "Active memory inject", Bool, "Memory & Skills";
        "memory.exclude_external_tool_output" => "Exclude untrusted from memory", Bool, "Memory & Skills";
        "memory.active_injection.top_k" => "Active memory top-k", Uint, "Memory & Skills";

        // — Personality —
        "evolution.enabled" => "Evolution", Bool, "Personality";
        "evolution.ambient_header_enabled" => "Ambient header", Bool, "Personality";

        // — Heartbeat —
        "heartbeat.session_start_enabled" => "Greet on TUI open", Bool, "Heartbeat";
        "heartbeat.session_start_throttle_minutes" => "Greeting throttle (min)", Uint, "Heartbeat";

        // — Resilience — auxiliary chain for background tasks (consolidation,
        // heartbeat, scheduled prompts). When the primary provider has an
        // outage, the aux chain keeps cron alive. Onboarding seeds these to
        // a same-provider default; users can edit the model or paste a
        // cross-provider fallback list here.
        "llm.aux.model" => "Aux model (or 'auto')", Text, "Resilience";
        "llm.aux.fallback" => "Aux fallback (JSON)", Text, "Resilience";
        "llm.credential_strategy" => "Credential strategy", Select, "Resilience";
        "llm.credential_cooldown_secs" => "Credential cooldown (s)", Uint, "Resilience";
        "llm.max_concurrent_per_credential" => "Max concurrent / key", Uint, "Resilience";
        "llm.proactive_rate_limit_threshold_pct" => "Proactive RL threshold (%)", Uint, "Resilience";
        "llm.proactive_rate_limit_min_window_secs" => "Proactive RL min window (s)", Uint, "Resilience";

        // — Voice —
        "tts.enabled" => "TTS provider enabled", Bool, "Voice";
        "tts.auto_mode" => "TTS on gateway replies", Bool, "Voice";
        "audio.enabled" => "STT provider enabled", Bool, "Voice";
        "audio.echo_transcript" => "Echo STT transcript", Bool, "Voice";
        "voice.enabled" => "Allow /voice", Bool, "Voice";
        "voice.auto_tts" => "Speak agent replies", Bool, "Voice";
        "voice.auto_submit" => "Auto-submit transcript", Bool, "Voice";
        "voice.beep_enabled" => "Record/stop beeps", Bool, "Voice";
        "voice.silence_threshold_rms" => "Silence RMS threshold", Uint, "Voice";
        "voice.silence_duration_secs" => "Silence duration (s)", Float, "Voice";
        "voice.min_speech_duration_secs" => "Min speech duration (s)", Float, "Voice";
        "voice.dip_tolerance_secs" => "Dip tolerance (s)", Float, "Voice";
        "voice.max_wait_secs" => "No-speech timeout (s)", Float, "Voice";
        "voice.no_speech_limit" => "Silent-cycle limit", Uint, "Voice";
        "voice.sample_rate_hz" => "Sample rate (Hz)", Uint, "Voice";

        // — Channels — only rows for channels with credentials configured
        // are shown; the popup filters at render time. Registry entries
        // exist regardless so the admin RPC / `borg config` can edit them.
        "gateway.group_activation" => "Group activation", Select, "Channels";
        "gateway.signal_cli_host" => "Signal CLI host", Text, "Channels";
        "gateway.signal_cli_port" => "Signal CLI port", Uint, "Channels";
        "gateway.discord_guild_allowlist" => "Discord guilds (JSON)", Text, "Channels";
        "gateway.slack_channel_allowlist" => "Slack channels (JSON)", Text, "Channels";

        // — Security —
        "sandbox.enabled" => "Sandbox", Bool, "Security";
        "hooks.enabled" => "Allow user hooks", Bool, "Security";
        "security.secret_detection" => "Secret detection", Bool, "Security";
        "security.startup_audit" => "Startup posture audit", Bool, "Security";
        "security.output_redaction" => "Output redaction", Bool, "Security";
        "security.verifier" => "mTLS verifier", Select, "Security";

        // — Maintenance (self-healing) —
        "maintenance.enabled" => "Daily self-heal task", Bool, "Maintenance";
        "maintenance.logs_retention_days" => "Log retention (days)", Uint, "Maintenance";
        "maintenance.activity_retention_days" => "Activity retention (days)", Uint, "Maintenance";
        "maintenance.doctor_runs_keep" => "Doctor runs kept", Uint, "Maintenance";
        "maintenance.embedding_repair_enabled" => "Repair embeddings", Bool, "Maintenance";
        "maintenance.embedding_repair_max_files" => "Embed repair files", Uint, "Maintenance";
        "maintenance.embedding_repair_max_chunks" => "Embed repair chunks", Uint, "Maintenance";
        "maintenance.issue_state_keep_days" => "Issue state retention", Uint, "Maintenance";
        "maintenance.security_squelch_ttl_days" => "Doctor squelch TTL (days)", Uint, "Maintenance";
        "maintenance.vacuum_interval_days" => "VACUUM interval (days)", Uint, "Maintenance";
        "maintenance.heal_action_budget" => "Heal action budget", Uint, "Maintenance";

        // — Reliability — agent-loop safety guards
        "loop_guard.identical_call_max" => "Identical-call cap", Uint, "Reliability";
        "loop_guard.backoff_initial_ms" => "Repeat backoff initial (ms)", Uint, "Reliability";
        "loop_guard.backoff_max_ms" => "Repeat backoff max (ms)", Uint, "Reliability";
        "loop_guard.unknown_tool_max_per_name" => "Unknown-tool max / name", Uint, "Reliability";

        "process_registry.orphan_warn_secs" => "Subprocess warn age (s)", Uint, "Subprocesses";
        "process_registry.orphan_fail_secs" => "Subprocess fail age (s)", Uint, "Subprocesses";
        "process_registry.auto_reap_secs" => "Subprocess auto-reap age (s, 0=off)", Uint, "Subprocesses";
        "process_registry.sigterm_grace_secs" => "SIGTERM grace (s)", Uint, "Subprocesses";
        "process_registry.finished_ttl_secs" => "Finished entry TTL (s)", Uint, "Subprocesses";
        "process_registry.max_finished_entries" => "Max finished entries", Uint, "Subprocesses";

        // — Per-session goals (/goal) —
        "goals.enabled" => "Enable /goal", Bool, "Goals";

        // — Networking & deployment —
        "discovery.mdns_enabled" => "LAN mDNS announce", Bool, "Networking";
        "discovery.tailscale_enabled" => "Tailscale discovery", Bool, "Networking";
        "tunnel.provider" => "Tunnel provider", Select, "Networking";
        "apns.enabled" => "APNs push (iOS wake)", Bool, "Networking";
        "apns.use_sandbox" => "APNs sandbox endpoint", Bool, "Networking";

        // — MCP (Model Context Protocol) — per-server config is JSON-only
        // (edited via the Slice 3 CLI), so only the kill switch surfaces
        // in the TUI popup.
        "mcp.enabled" => "MCP", Bool, "MCP";
        "mcp.tool_search" => "MCP tool search", Bool, "MCP";

        // — Browser — automation backend + engine picker and cloud creds.
        // Cloud backends (browserbase / browser_use / firecrawl) create a
        // remote session and are driven over CDP; `local` launches the chosen
        // engine; `cdp_attach` uses the CDP attach URL.
        "browser.enabled" => "Browser automation", Bool, "Browser";
        "browser.backend" => "Browser backend", Select, "Browser";
        "browser.engine" => "Browser engine", Select, "Browser";
        "browser.headless" => "Headless", Bool, "Browser";
        "browser.executable" => "Custom exe path", Text, "Browser";
        "browser.snapshot_max_elements" => "Snapshot max elems", Uint, "Browser";
        "browser.cdp_url" => "CDP attach URL", Text, "Browser";
        "browser.browserbase_api_key" => "Browserbase API key", Secret, "Browser";
        "browser.browserbase_project_id" => "Browserbase project", Text, "Browser";
        "browser.browser_use_api_key" => "Browser Use API key", Secret, "Browser";
        "browser.firecrawl_api_key" => "Firecrawl API key", Secret, "Browser";

        // — Appearance — TUI theme picker (also reachable as `/theme`)
        "ui.theme" => "Theme", Select, "Appearance";

        // — Telemetry (OpenTelemetry export) — off by default
        "telemetry.tracing_enabled" => "OTel traces", Bool, "Telemetry";
        "telemetry.metrics_enabled" => "OTel metrics", Bool, "Telemetry";
        "telemetry.logs_enabled" => "OTel logs", Bool, "Telemetry";
        "telemetry.sampling_ratio" => "Trace sampling ratio", Float, "Telemetry";

        // — Advanced — power-user / esoteric knobs
        "llm.cache.strategy" => "Cache layout", Select, "Advanced";
        "conversation.concurrent_tools.max_workers" => "Parallel tool workers", Uint, "Advanced";
        "conversation.protect_first_n" => "Protected head msgs", Uint, "Advanced";
        "skills.budget_pct" => "Skills budget (% ctx)", Float, "Advanced";
        "budget.warning_threshold" => "Budget warning", Float, "Advanced";
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
    (
        "llm.credential_strategy",
        &["fill_first", "round_robin", "least_used", "random"],
    ),
    ("security.verifier", &["tofu", "tailscale"]),
    (
        "browser.backend",
        &[
            "local",
            "browserbase",
            "browser_use",
            "firecrawl",
            "cdp_attach",
        ],
    ),
    (
        "browser.engine",
        &[
            "chrome",
            "brave",
            "edge",
            "chromium",
            "lightpanda",
            "custom",
        ],
    ),
    ("gateway.group_activation", &["mention", "always"]),
    (
        "llm.thinking",
        &[
            "off", "minimal", "low", "medium", "high", "xhigh", "adaptive", "max",
        ],
    ),
    (
        "tunnel.provider",
        &["none", "cloudflare", "ngrok", "tailscale", "custom"],
    ),
    // Builtin themes only — `/settings` cycles this static list AND fires
    // the same `AppAction::UpdateSetting` plumbing that `/theme` uses, so
    // a cycle here DOES live-swap the theme (not just persist the value).
    // User themes from `~/.borg/themes/` are reachable via `/theme` (which
    // discovers them at popup time) and via `borg config set ui.theme
    // <name>` — they can't appear in this `static` slice. Keep in sync
    // with the builtin registry in `crates/interface/src/tui/themes/builtins.rs`.
    ("ui.theme", &["default", "mono", "slate", "daylight"]),
];

/// `(min, max, step)` for `Float`-kind TUI settings. Drives `step_float` in
/// the settings popup. Where a key has a `range(..)` bound in the
/// `registry_and_apply` block above the entry mirrors it; the rest use sane UI
/// stepping bounds around the compiled default. The `step` is TUI-only. Every
/// `Float`-kind `tui_settings` key MUST appear here (guarded by
/// `every_float_tui_setting_has_range`).
pub const TUI_FLOAT_RANGES: &[(&str, f64, f64, f64)] = &[
    ("temperature", 0.0, 2.0, 0.1),
    ("budget.warning_threshold", 0.0, 1.0, 0.01),
    ("skills.budget_pct", 0.0, 1.0, 0.01),
    ("telemetry.sampling_ratio", 0.0, 1.0, 0.05),
    ("voice.silence_duration_secs", 0.5, 10.0, 0.5),
    ("voice.min_speech_duration_secs", 0.1, 5.0, 0.1),
    ("voice.dip_tolerance_secs", 0.1, 5.0, 0.1),
    ("voice.max_wait_secs", 1.0, 60.0, 1.0),
];
