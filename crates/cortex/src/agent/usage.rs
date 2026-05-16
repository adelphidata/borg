//! Per-call token usage accounting and budget reconciliation.
//!
//! Pulled out of `agent/mod.rs` so the wide DB lock + rate-card scope no
//! longer captures the outer `run_agent_loop` future. The single entry point
//! is [`Agent::record_usage`], which `stream_handler` invokes from the
//! `StreamEvent::Usage` arm.

use tracing::{debug, warn};

use crate::llm::UsageData;

use super::Agent;

impl Agent {
    /// Account for one `StreamEvent::Usage`: increment counters and persist to the
    /// token-usage table. Extracted from `run_agent_loop` to keep the outer future's
    /// frame small (the inner DB lock + pricing call captures a wide scope).
    pub(super) fn record_usage(&self, usage: &UsageData) {
        let total = usage.prompt_tokens + usage.completion_tokens;
        if total == 0 {
            return;
        }
        self.metrics.llm_tokens.add(total, &[]);
        let guard = self.db_guard();
        let Some(ref db) = *guard else { return };
        let cost = crate::pricing::estimate_cost(
            &usage.model,
            usage.prompt_tokens,
            usage.completion_tokens,
        );
        if let Err(e) = db.log_token_usage_with_cache(
            usage.prompt_tokens,
            usage.completion_tokens,
            total,
            usage.cached_input_tokens,
            usage.cache_creation_tokens,
            &usage.provider,
            &usage.model,
            cost,
        ) {
            warn!("Failed to log token usage: {e}");
        }

        // Budget post-flight reconcile + provider_call event. Price using
        // the rate card row in effect at call time so a future replay of
        // the audit chain produces the same USD total. If no rate row
        // exists for this (provider, model), warn and skip — recording an
        // f64-derived USD value here would seed the audit chain with a
        // value that drifts on replay. The fix is to seed the rate card
        // (`crates/core/src/pricing.rs::bundled_rate_card_entries` for a
        // built-in, or `borg providers refresh-rates` once verification
        // ships).
        let now = chrono::Utc::now().timestamp();
        let provider_id = usage.provider.to_ascii_lowercase();
        let (actual_usd, rate_card_effective_at) =
            match crate::budgets::RateCard::lookup(db.conn(), &provider_id, &usage.model, now) {
                Ok(Some(row)) => {
                    let priced = crate::budgets::RateCard::price(
                        &row,
                        usage.prompt_tokens,
                        usage.completion_tokens,
                    );
                    (priced, Some(row.effective_at))
                }
                Ok(None) => {
                    // Per-turn warning was a major contributor to log spam for
                    // any session on a model without a rate card. Promote to
                    // WARN once per (provider, model); otherwise stay at DEBUG.
                    // Cap the dedupe set so a misbehaving provider returning a
                    // fresh `usage.model` per turn can't grow it without bound.
                    const MAX_TRACKED: usize = 1024;
                    static WARNED: std::sync::OnceLock<
                        std::sync::Mutex<std::collections::HashSet<(String, String)>>,
                    > = std::sync::OnceLock::new();
                    let key = (provider_id.clone(), usage.model.clone());
                    let first_time = WARNED
                        .get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()))
                        .lock()
                        .map(|mut set| {
                            if set.len() >= MAX_TRACKED {
                                false
                            } else {
                                set.insert(key.clone())
                            }
                        })
                        .unwrap_or(false);
                    if first_time {
                        debug!(
                            provider = %provider_id,
                            model = %usage.model,
                            legacy_cost_estimate = ?cost,
                            "No rate card for provider/model; skipping budget reconcile \
                             (token usage still logged). Subsequent occurrences are silent."
                        );
                    } else {
                        tracing::trace!(
                            provider = %provider_id,
                            model = %usage.model,
                            "No rate card; skipping budget reconcile"
                        );
                    }
                    return;
                }
                Err(e) => {
                    warn!(
                        "Rate-card lookup failed for {provider_id}/{}: {e}",
                        usage.model
                    );
                    return;
                }
            };
        if let Err(e) = crate::budgets::BudgetEngine::post_flight(
            db.conn(),
            None, // principal_id wiring is v3
            &provider_id,
            &usage.model,
            rate_card_effective_at,
            usage.prompt_tokens,
            usage.completion_tokens,
            usage.cached_input_tokens,
            actual_usd,
            Some(self.session.meta.id.as_str()),
        ) {
            warn!("Budget post-flight failed: {e}");
        }
    }
}
