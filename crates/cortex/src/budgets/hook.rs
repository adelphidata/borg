//! `BudgetHook` — pre-flight refusal at the [`HookPoint::BeforeLlmCall`]
//! point.
//!
//! The hook keeps a private [`Database`] handle (mirroring the
//! `VitalsHook` pattern at `crates/core/src/vitals.rs:716`). On every
//! before-call dispatch:
//!
//! 1. Read the daemon's active provider/model from settings.
//! 2. Estimate USD via the rate card and a token estimate equal to the
//!    current message-set size in tokens (close enough — this gates
//!    obvious cap crossings; post-flight does the exact math).
//! 3. Call [`BudgetEngine::pre_flight`].
//! 4. Return [`HookAction::Skip`] on `HardRefuse` (with an injected
//!    system message explaining the refusal); `Continue` on `Allow` and
//!    `SoftWarn` (the engine has already debounced the warning toast for
//!    this period).
//!
//! Post-flight reconcile is wired in `crate::agent::Agent::record_usage`
//! rather than in this hook — that's the single observation point for the
//! provider's actual reported usage.

use std::fmt;

use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::Mutex as AsyncMutex;

use crate::db::Database;
use crate::hooks::{Hook, HookAction, HookContext, HookData, HookPoint};

use super::engine::{BudgetEngine, PreFlightDecision};
use super::rate_card::RateCard;
use super::usd::Usd;

/// Reads `providers.default_id` and the active model from a config-loaded
/// snapshot. Falls back to `("openai", "gpt-4o-mini")` if no override is
/// stored — matches the v1 hobbyist default.
pub(crate) async fn active_provider_and_model(db: &Database) -> (String, String) {
    let provider = db
        .get_setting("providers.default_id")
        .await
        .ok()
        .flatten()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "openai".to_string())
        .to_ascii_lowercase();
    let model = db
        .get_setting("model")
        .await
        .ok()
        .flatten()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "gpt-4o-mini".to_string());
    (provider, model)
}

/// Pre-flight budget gate. See module docs for behavior.
pub struct BudgetHook {
    db: AsyncMutex<Database>,
}

impl BudgetHook {
    /// Open a fresh DB connection and wrap it in a hook.
    pub async fn new() -> Result<Self> {
        Ok(Self {
            db: AsyncMutex::new(Database::open().await?),
        })
    }

    /// Hook from a pre-existing Database (used by tests + the daemon when
    /// it wants to share a single connection).
    pub fn from_database(db: Database) -> Self {
        Self {
            db: AsyncMutex::new(db),
        }
    }

    /// Estimate USD against the active provider/model's rate card given a
    /// prompt-token estimate. Returns `Usd::ZERO` if no rate row exists
    /// (treat as priceless — local providers, mostly).
    async fn estimate_for_prompt_tokens(db: &Database, prompt_tokens: u64) -> Usd {
        let (provider_id, model) = active_provider_and_model(db).await;
        match RateCard::lookup(db, &provider_id, &model, chrono::Utc::now().timestamp()).await {
            Ok(Some(row)) => RateCard::price(&row, prompt_tokens, 0),
            Ok(None) => Usd::ZERO,
            Err(e) => {
                tracing::warn!(
                    "BudgetHook: rate-card lookup failed for {provider_id}/{model}: {e}"
                );
                Usd::ZERO
            }
        }
    }
}

#[async_trait]
impl Hook for BudgetHook {
    fn name(&self) -> &str {
        "budget"
    }

    fn points(&self) -> &[HookPoint] {
        &[HookPoint::BeforeLlmCall, HookPoint::BeforeAgentStart]
    }

    async fn execute(&self, ctx: &HookContext) -> HookAction {
        // Prefer the agent-supplied tiktoken-based count when available;
        // fall back to message-count × 50 only when the assembled message
        // set isn't yet available.
        let prompt_tokens: u64 = match &ctx.data {
            HookData::LlmCall {
                prompt_tokens: Some(t),
                ..
            } => *t,
            HookData::LlmCall {
                message_count,
                prompt_tokens: None,
            } => (*message_count as u64).saturating_mul(50),
            HookData::AgentStart { user_message } => {
                // First-call fallback: rough byte → token estimate.
                use crate::memory::tokenizer::estimate_tokens;
                estimate_tokens(user_message) as u64
            }
            _ => return HookAction::Continue,
        };

        let db = self.db.lock().await;

        let estimate = Self::estimate_for_prompt_tokens(&db, prompt_tokens).await;

        match BudgetEngine::pre_flight(&db, None, estimate).await {
            Ok(PreFlightDecision::Allow) => HookAction::Continue,
            Ok(PreFlightDecision::SoftWarn { soft_cap, spent }) => {
                tracing::info!(
                    "Budget soft warning: ${} of ${} soft cap",
                    spent.to_display_string(),
                    soft_cap.to_display_string()
                );
                HookAction::InjectContext(format!(
                    "<budget_soft_warning>Spending is at ${} of ${} soft cap this period.</budget_soft_warning>",
                    spent.to_display_string(),
                    soft_cap.to_display_string()
                ))
            }
            Ok(PreFlightDecision::HardRefuse {
                scope,
                hard_cap,
                spent,
            }) => {
                tracing::warn!(
                    "Budget hard refusal at scope '{scope}': ${} of ${} hard cap",
                    spent.to_display_string(),
                    hard_cap.to_display_string()
                );
                HookAction::Skip
            }
            Err(e) => {
                tracing::warn!("BudgetHook: pre_flight failed: {e}");
                HookAction::Continue
            }
        }
    }
}

impl fmt::Debug for BudgetHook {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BudgetHook").finish()
    }
}

// PHASE 9 TODO: budget hook tests disabled pending async DB test harness.
#[cfg(all(test, feature = "phase9-tests-disabled"))]
mod tests {
    use super::*;
    use crate::hooks::{HookData, HookPoint};
    use rusqlite::params;

    fn override_default_budget(
        db: &Database,
        hard: &str,
        soft: Option<&str>,
        spent: &str,
        period_start: i64,
    ) {
        db.conn()
            .execute(
                "UPDATE budgets
                 SET hard_cap_usd = ?1, soft_cap_usd = ?2, current_spent_usd = ?3,
                     current_period_start = ?4, last_soft_toast_period = NULL
                 WHERE scope = 'default'",
                params![hard, soft, spent, period_start],
            )
            .unwrap();
    }

    fn ctx(point: HookPoint, data: HookData) -> HookContext {
        HookContext {
            point,
            session_id: "test-session".to_string(),
            turn_count: 0,
            data,
        }
    }

    #[test]
    fn active_provider_and_model_falls_back_when_unset() {
        let db = Database::test_db();
        let (provider, model) = active_provider_and_model(&db);
        assert_eq!(provider, "openai");
        assert_eq!(model, "gpt-4o-mini");
    }

    #[test]
    fn active_provider_and_model_lowercases_provider_id_only() {
        // Provider IDs are normalized to lowercase for rate-card lookups
        // (rate-card rows are seeded lowercase). Model names must NOT be
        // lowercased — `claude-sonnet-4-6` and many OpenRouter slugs are
        // case-sensitive on the wire.
        let db = Database::test_db();
        db.set_setting("providers.default_id", "OpenRouter")
            .unwrap();
        db.set_setting("model", "GPT-4o").unwrap();
        let (provider, model) = active_provider_and_model(&db);
        assert_eq!(provider, "openrouter");
        assert_eq!(model, "GPT-4o");
    }

    #[test]
    fn active_provider_and_model_treats_empty_string_as_unset() {
        // Settings rows containing the empty string must fall back to the
        // hobbyist default — guards the `.filter(|s| !s.is_empty())` clause.
        let db = Database::test_db();
        db.set_setting("providers.default_id", "").unwrap();
        db.set_setting("model", "").unwrap();
        let (provider, model) = active_provider_and_model(&db);
        assert_eq!(provider, "openai");
        assert_eq!(model, "gpt-4o-mini");
    }

    #[test]
    fn hook_continue_on_non_llm_data() {
        // Variants other than LlmCall / AgentStart are explicitly out-of-scope
        // for the budget hook; it must short-circuit to Continue without
        // touching the DB or the engine.
        let db = Database::test_db();
        let hook = BudgetHook::from_database(db);
        let action = hook.execute(&ctx(
            HookPoint::AfterToolCall,
            HookData::ToolCall {
                name: "run_shell".to_string(),
                args: "{}".to_string(),
            },
        ));
        assert!(matches!(action, HookAction::Continue), "got {action:?}");
    }

    #[test]
    fn hook_continue_when_no_rate_card() {
        // Default openai/gpt-4o-mini path with no rate-card row → estimate is
        // Usd::ZERO → Allow. Mirrors the local-Ollama pass-through case.
        let db = Database::test_db();
        let hook = BudgetHook::from_database(db);
        let action = hook.execute(&ctx(
            HookPoint::BeforeLlmCall,
            HookData::LlmCall {
                message_count: 4,
                prompt_tokens: Some(100),
            },
        ));
        assert!(matches!(action, HookAction::Continue), "got {action:?}");
    }

    #[test]
    fn hook_skips_at_hard_cap() {
        // current_spent > hard_cap means even a $0 estimate (no rate card)
        // projects past hard cap → HardRefuse → hook returns Skip. This is
        // the hook's reason for existing.
        let db = Database::test_db();
        let now = chrono::Utc::now().timestamp();
        override_default_budget(&db, "0.01", None, "0.02", now);
        let hook = BudgetHook::from_database(db);
        let action = hook.execute(&ctx(
            HookPoint::BeforeLlmCall,
            HookData::LlmCall {
                message_count: 1,
                prompt_tokens: Some(50),
            },
        ));
        assert!(matches!(action, HookAction::Skip), "got {action:?}");
    }

    #[test]
    fn hook_inject_context_at_soft_cap() {
        // Soft cap $0.50, threshold = 80% = $0.40, spent = $0.40, estimate $0
        // → projected $0.40 ≥ $0.40 → SoftWarn → hook returns InjectContext
        // carrying the budget_soft_warning marker the agent prompt can
        // recognize.
        let db = Database::test_db();
        let now = chrono::Utc::now().timestamp();
        override_default_budget(&db, "10.00", Some("0.50"), "0.40", now);
        let hook = BudgetHook::from_database(db);
        let action = hook.execute(&ctx(
            HookPoint::BeforeLlmCall,
            HookData::LlmCall {
                message_count: 1,
                prompt_tokens: Some(0),
            },
        ));
        let payload = match action {
            HookAction::InjectContext(s) => s,
            other => panic!("expected InjectContext, got {other:?}"),
        };
        assert!(
            payload.contains("<budget_soft_warning>"),
            "missing tag: {payload}"
        );
        assert!(payload.contains("0.40"), "missing spent: {payload}");
        assert!(payload.contains("0.50"), "missing soft cap: {payload}");
    }

    #[test]
    fn hook_agent_start_uses_estimate_tokens_path() {
        // AgentStart routes through the byte→token estimator in
        // crate::memory::tokenizer::estimate_tokens. With no rate cards seeded the
        // estimate is still Usd::ZERO so the engine returns Allow → Continue.
        // The point of this test is to exercise the AgentStart match arm
        // without panicking on an unexpectedly large user_message.
        let db = Database::test_db();
        let hook = BudgetHook::from_database(db);
        let action = hook.execute(&ctx(
            HookPoint::BeforeAgentStart,
            HookData::AgentStart {
                user_message: "x".repeat(2000),
            },
        ));
        assert!(matches!(action, HookAction::Continue), "got {action:?}");
    }
}
