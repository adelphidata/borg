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

        // ── Provider routing / air-gap ──
        "providers.outbound_allowed" => providers.outbound_allowed, parsed(bool);

        // ── Sandbox ──
        "sandbox.enabled" => sandbox.enabled, parsed(bool);

        // ── Memory ──
        "memory.max_context_tokens" => memory.max_context_tokens, nonzero(usize);
        "memory.flush_before_compaction" => memory.flush_before_compaction, parsed(bool);
        "memory.flush_min_messages" => memory.flush_min_messages, parsed(usize);
        "memory.flush_soft_threshold_tokens" => memory.flush_soft_threshold_tokens, parsed(usize);
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

        // ── Security ──
        "security.secret_detection" => security.secret_detection, parsed(bool);
        "security.host_audit" => security.host_audit, parsed(bool);
        "security.blocked_paths" => security.blocked_paths, json;
        "security.allowed_paths" => security.allowed_paths, json;
        "security.action_limits" => security.action_limits, json_set;
        "security.gateway_action_limits" => security.gateway_action_limits, json_set;
        // security.verifier handled in custom_apply below (FromStr enum).

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
        "heartbeat.min_interval_secs" => heartbeat.min_interval_secs, parsed(u64);
        "heartbeat.pre_run_command" => heartbeat.pre_run_command, string;
        "heartbeat.pre_run_cwd" => heartbeat.pre_run_cwd, string;
        "heartbeat.pre_run_timeout_ms" => heartbeat.pre_run_timeout_ms, parsed(u64);
        "heartbeat.turn_timeout_secs" => heartbeat.turn_timeout_secs, parsed(u64);

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

        // ── Debug ──
        "debug.llm_logging" => debug.llm_logging, parsed(bool);

        // ── Telemetry ──
        "telemetry.cost_events" => telemetry.cost_events, parsed(bool);

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
        "memory.active_injection.enabled" => "Active memory inject", Bool, "Memory & Skills";
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

        // — Voice —
        "tts.enabled" => "Enabled", Bool, "Voice";
        "tts.auto_mode" => "Auto reply", Bool, "Voice";

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
    ("security.verifier", &["tofu", "tailscale"]),
    ("gateway.group_activation", &["mention", "always"]),
];

/// `(min, max, step)` for `Float`-kind TUI settings. Drives `step_float` in
/// the settings popup. Entries mirror the `range(..)` bounds in the
/// `registry_and_apply` block above; the extra `step` is TUI-only.
pub const TUI_FLOAT_RANGES: &[(&str, f64, f64, f64)] = &[
    ("temperature", 0.0, 2.0, 0.1),
    ("budget.warning_threshold", 0.0, 1.0, 0.01),
    ("skills.budget_pct", 0.0, 1.0, 0.01),
];
