//! Agent constructors and one-time startup work.
//!
//! Pulled out of `agent/mod.rs`. Hosts:
//! - [`build_common`] / [`AgentCommon`]: shared init shared by every constructor.
//! - [`Agent::build_agent`] / [`Agent::new`] / [`Agent::new_sub_agent`] /
//!   [`Agent::new_sub_agent_aux`] / [`Agent::new_sub_agent_with_role`]:
//!   the public constructor surface plus the role-aware private builder.
//! - [`Agent::prune_embedding_cache_on_startup`]: one-time embedding-cache
//!   pruning called from `Agent::new`.

use std::sync::Arc;
use std::sync::Mutex;

use anyhow::Result;
use tracing::warn;

use crate::config::Config;
use crate::constants;
use crate::db::Database;
use crate::hooks::HookRegistry;
use crate::llm::LlmClient;
use crate::policy::ExecutionPolicy;
use crate::rate_guard::SessionRateGuard;
use crate::session::Session;
use crate::telemetry::BorgMetrics;

use super::system_prompt_cache::SystemPromptCache;
use super::tool_validation::ToolCallRepeatTracker;
use super::{Agent, LlmRole};

/// Common state produced by `build_common`, consumed by both `new()` and `new_sub_agent()`.
pub(super) struct AgentCommon {
    pub(super) policy: ExecutionPolicy,
    pub(super) rate_guard: SessionRateGuard,
    pub(super) session: Session,
    pub(super) db: Option<Database>,
    pub(super) skill_env_allowlist: std::collections::HashSet<String>,
    pub(super) tts_synthesizer: Option<crate::media::tts::TtsSynthesizer>,
}

/// Build the shared initialization state used by both `Agent::new` and `Agent::new_sub_agent`.
pub(super) fn build_common(config: &Config) -> Result<AgentCommon> {
    let _ = LlmClient::new(config)?;
    let policy = config.policy.clone();
    let session = Session::new();
    let db = match Database::open() {
        Ok(db) => Some(db),
        Err(e) => {
            warn!("Failed to open database on agent init: {e}");
            None
        }
    };
    // Action limits receive an evolution-stage floor: higher stages unlock
    // additional headroom per `docs/evolution.md#action-limits-by-stage`.
    // Explicit user config (set higher) is preserved via `max`.
    let mut limits = config.security.action_limits.clone();
    if let Some(ref d) = db {
        if let Ok(state) = d.get_evolution_state() {
            let stage = match state.stage {
                crate::evolution::Stage::Base => crate::rate_guard::EvolutionStage::Base,
                crate::evolution::Stage::Evolved => crate::rate_guard::EvolutionStage::Evolved,
                crate::evolution::Stage::Final => crate::rate_guard::EvolutionStage::Final,
            };
            limits.apply_stage(stage);
        }
    }
    let rate_guard = SessionRateGuard::new(limits);
    let resolved_creds = config.resolve_credentials();
    let skill_env_allowlist =
        crate::skills::collect_required_env_vars(&resolved_creds, &config.skills);
    let tts_synthesizer = crate::media::tts::TtsSynthesizer::from_config(config);
    Ok(AgentCommon {
        policy,
        rate_guard,
        session,
        db,
        skill_env_allowlist,
        tts_synthesizer,
    })
}

impl Agent {
    /// Assemble an `Agent` from pre-computed parts shared by all constructors.
    pub(super) fn build_agent(
        config: Config,
        common: AgentCommon,
        agent_control: Option<crate::multi_agent::AgentControl>,
        spawn_depth: u32,
        tools_filter: Option<Vec<String>>,
        metrics: BorgMetrics,
        llm_role: LlmRole,
    ) -> Self {
        let execution_context = crate::execution_context::ExecutionContext::from_process()
            .unwrap_or_else(|_| {
                crate::execution_context::ExecutionContext::unrestricted(std::path::PathBuf::from(
                    ".",
                ))
            });
        let git_repo_root = crate::git::find_repo_root(execution_context.workspace_cwd());
        let config_arc = Arc::new(config.clone());
        let session_id = common.session.meta.id.clone();
        Self {
            config,
            history: Vec::new(),
            session: common.session,
            policy: common.policy,
            hook_registry: HookRegistry::new(),
            turn_count: 0,
            rate_guard: common.rate_guard,
            agent_control,
            spawn_depth,
            tools_filter,
            skill_env_allowlist: common.skill_env_allowlist,
            tts_synthesizer: common.tts_synthesizer,
            metrics,
            browser_session: None,
            config_rx: None,
            db: Mutex::new(common.db),
            caches: SystemPromptCache::default(),
            ghost_commit: None,
            execution_context,
            git_repo_root,
            config_arc,
            steer_rx: None,
            recent_tool_names: std::collections::HashSet::new(),
            cost_event_sink: None,
            short_term_memory: crate::memory::short_term_memory::ShortTermMemory::new(
                session_id, 2000,
            ),
            previous_summary: None,
            setup_just_consumed: false,
            tool_call_repeat: ToolCallRepeatTracker::default(),
            llm_role,
            goal_runtime: crate::goals::runtime::GoalRuntimeState::new(),
            goal_event_sink: None,
        }
    }

    /// Create a new agent with the given config and metrics.
    pub fn new(config: Config, metrics: BorgMetrics) -> Result<Self> {
        let common = build_common(&config)?;
        let agent_control = if config.agents.enabled {
            Some(crate::multi_agent::AgentControl::new(
                &config.agents,
                &common.session.meta.id,
                0,
            ))
        } else {
            None
        };

        // Best-effort embedding cache pruning on startup
        Self::prune_embedding_cache_on_startup(&config);

        Ok(Self::build_agent(
            config,
            common,
            agent_control,
            0,
            None,
            metrics,
            LlmRole::Primary,
        ))
    }

    /// Prune stale embedding cache entries on startup based on config.
    fn prune_embedding_cache_on_startup(config: &Config) {
        let ttl_days = config.memory.cache_ttl_days;
        let max_entries = config.memory.cache_max_entries;

        if ttl_days == 0 && max_entries == 0 {
            return;
        }

        let db = match Database::open() {
            Ok(db) => db,
            Err(e) => {
                tracing::warn!("Cache pruning: failed to open database: {e}");
                return;
            }
        };

        if ttl_days > 0 {
            let max_age_secs = i64::from(ttl_days) * constants::SECS_PER_DAY;
            match db.prune_embedding_cache(max_age_secs) {
                Ok(0) => {}
                Ok(n) => tracing::info!(
                    "Pruned {n} stale embedding cache entries (>{ttl_days} days old)"
                ),
                Err(e) => tracing::warn!("Cache TTL pruning failed: {e}"),
            }
        }

        if max_entries > 0 {
            match db.prune_embedding_cache_by_count(max_entries) {
                Ok(0) => {}
                Ok(n) => {
                    tracing::info!("Pruned {n} embedding cache entries (over {max_entries} limit)")
                }
                Err(e) => tracing::warn!("Cache count pruning failed: {e}"),
            }
        }
    }

    /// Create a sub-agent that shares the parent's config but has its own session.
    pub fn new_sub_agent(
        config: Config,
        spawn_depth: u32,
        agents_config: &crate::config::MultiAgentConfig,
        metrics: BorgMetrics,
        tools_filter: Option<Vec<String>>,
    ) -> Result<Self> {
        Self::new_sub_agent_with_role(
            config,
            spawn_depth,
            agents_config,
            metrics,
            tools_filter,
            LlmRole::Primary,
        )
    }

    /// Create a sub-agent that routes through the auxiliary LLM chain
    /// (`[llm.aux]`). Use for background work — memory consolidation,
    /// scheduled prompts, heartbeat — so a primary-provider outage can't
    /// take down nightly cron. Tool-spawned sub-agents and interactive
    /// turns must keep using [`Agent::new_sub_agent`] (Primary).
    pub fn new_sub_agent_aux(
        config: Config,
        spawn_depth: u32,
        agents_config: &crate::config::MultiAgentConfig,
        metrics: BorgMetrics,
        tools_filter: Option<Vec<String>>,
    ) -> Result<Self> {
        Self::new_sub_agent_with_role(
            config,
            spawn_depth,
            agents_config,
            metrics,
            tools_filter,
            LlmRole::Auxiliary,
        )
    }

    fn new_sub_agent_with_role(
        config: Config,
        spawn_depth: u32,
        agents_config: &crate::config::MultiAgentConfig,
        metrics: BorgMetrics,
        tools_filter: Option<Vec<String>>,
        llm_role: LlmRole,
    ) -> Result<Self> {
        let common = build_common(&config)?;
        let agent_control = if agents_config.enabled && spawn_depth < agents_config.max_spawn_depth
        {
            Some(crate::multi_agent::AgentControl::new(
                agents_config,
                &common.session.meta.id,
                spawn_depth,
            ))
        } else {
            None
        };
        Ok(Self::build_agent(
            config,
            common,
            agent_control,
            spawn_depth,
            tools_filter,
            metrics,
            llm_role,
        ))
    }
}
