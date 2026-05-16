//! Config hot-reload + LLM client construction + sink/watch setters.
//!
//! Pulled out of `agent/mod.rs`. Hosts `reload_config`,
//! `make_llm_client_from_config`, `maybe_reload_config`, and the small public
//! `set_*` setters that wire optional collaborators (steer channel, config
//! watcher, cost-event sink, goal-event sink, execution context, IDE bridge).

use std::sync::Arc;

use anyhow::Result;
use tokio::sync::{mpsc, watch};
use tracing::{debug, info_span};

use crate::config::Config;
use crate::llm::LlmClient;

use super::{Agent, LlmRole};

impl Agent {
    /// Set a channel for receiving user steer messages mid-turn.
    pub fn set_steer_channel(&mut self, rx: mpsc::UnboundedReceiver<String>) {
        self.steer_rx = Some(rx);
    }

    /// Set a config watch receiver for hot reload.
    pub fn set_config_watcher(&mut self, rx: watch::Receiver<Config>) {
        self.config_rx = Some(rx);
    }

    /// Install a best-effort cost-event sink for all session-backed LLM calls.
    pub fn set_cost_event_sink(&mut self, sink: Arc<dyn crate::llm::CostEventSink>) {
        self.cost_event_sink = Some(sink);
    }

    /// Install a best-effort goal-event sink. The daemon implements this by
    /// forwarding into the `SubscribeGoalEvents` broadcast channel so the
    /// TUI footer and any future remote dashboard can react in real time.
    pub fn set_goal_event_sink(&mut self, sink: Arc<dyn crate::goals::GoalEventSink>) {
        self.goal_event_sink = Some(sink);
    }

    /// Override the session execution context. Use this when the workspace
    /// changes (different cwd / root). Invalidates the project-docs cache and
    /// re-derives `git_repo_root`. To swap *only* the IDE-backed text-file
    /// bridge inside an ACP-backed turn, prefer `set_text_file_bridge` —
    /// otherwise every turn pays for an `AGENTS.md` / `CLAUDE.md` re-read.
    pub fn set_execution_context(
        &mut self,
        execution_context: crate::execution_context::ExecutionContext,
    ) {
        self.execution_context = execution_context;
        self.git_repo_root = crate::git::find_repo_root(self.execution_context.workspace_cwd());
        self.caches.clear_project_docs();
    }

    /// Attach or remove the IDE-backed text-file bridge without invalidating
    /// the workspace-derived caches. Hot path on ACP-backed turns.
    pub fn set_text_file_bridge(
        &mut self,
        bridge: Option<std::sync::Arc<dyn crate::execution_context::TextFileBridge>>,
    ) {
        self.execution_context = self.execution_context.clone().with_text_file_bridge(bridge);
    }

    /// Snapshot the current execution context.
    pub fn execution_context(&self) -> crate::execution_context::ExecutionContext {
        self.execution_context.clone()
    }

    /// Replace config, re-derive dependent state (policy, rate limits).
    pub(super) fn reload_config(&mut self, new_config: Config) {
        self.policy = new_config.policy.clone();
        self.rate_guard
            .update_limits(new_config.security.action_limits.clone());
        let resolved_creds = new_config.resolve_credentials();
        self.caches.on_config_reloaded(resolved_creds);
        self.config_arc = Arc::new(new_config.clone());
        self.config = new_config;
        // skills caches were just cleared by `on_config_reloaded`;
        // recompute the env allowlist now that credentials are fresh.
        self.skill_env_allowlist = crate::skills::collect_required_env_vars(
            self.credentials_cached(),
            &self.config.skills,
        );
    }

    pub(super) fn make_llm_client_from_config(&self, config: &Config) -> Result<LlmClient> {
        let mut client = match self.llm_role {
            LlmRole::Primary => LlmClient::new(config)?,
            LlmRole::Auxiliary => LlmClient::for_auxiliary(config)?,
        };
        if let Some(ref sink) = self.cost_event_sink {
            if config.telemetry.cost_events {
                // Tag the sink so dashboards can split spend by role —
                // an aux outage budget is its own line item, not lumped
                // into chat spend.
                let kind = match self.llm_role {
                    LlmRole::Primary => "chat",
                    LlmRole::Auxiliary => "auxiliary",
                };
                client = client.with_cost_event_sink(
                    sink.clone(),
                    crate::llm::CostEventContext {
                        session_id: self.session.meta.id.clone(),
                        kind: kind.to_string(),
                    },
                );
            }
        }
        Ok(client)
    }

    /// Hot-reload config if the watcher has detected changes.
    pub(super) fn maybe_reload_config(&mut self) {
        if let Some(ref mut rx) = self.config_rx {
            if rx.has_changed().unwrap_or(false) {
                let new_config = rx.borrow_and_update().clone();
                info_span!("config_reload").in_scope(|| {
                    // Routine event, not an error condition — DEBUG is the
                    // right level. WARN here used to fire on the first turn of
                    // every session and looked like a real warning to operators.
                    debug!("Config reloaded from disk");
                });
                self.reload_config(new_config);
            }
        }
    }
}
