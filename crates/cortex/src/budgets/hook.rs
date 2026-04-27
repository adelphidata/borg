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
//! Post-flight reconcile is wired in [`crate::agent::Agent::record_usage`]
//! rather than in this hook — that's the single observation point for the
//! provider's actual reported usage.

use std::fmt;
use std::sync::Mutex;

use anyhow::Result;

use crate::db::Database;
use crate::hooks::{Hook, HookAction, HookContext, HookData, HookPoint};

use super::engine::{BudgetEngine, PreFlightDecision};
use super::rate_card::RateCard;
use super::usd::Usd;

/// Reads `providers.default_id` and the active model from a config-loaded
/// snapshot. Falls back to `("openai", "gpt-4o-mini")` if no override is
/// stored — matches the v1 hobbyist default.
fn active_provider_and_model(db: &Database) -> (String, String) {
    let provider = db
        .get_setting("providers.default_id")
        .ok()
        .flatten()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "openai".to_string())
        .to_ascii_lowercase();
    let model = db
        .get_setting("model")
        .ok()
        .flatten()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "gpt-4o-mini".to_string());
    (provider, model)
}

/// Pre-flight budget gate. See module docs for behavior.
pub struct BudgetHook {
    db: Mutex<Database>,
}

impl BudgetHook {
    /// Open a fresh DB connection and wrap it in a hook.
    pub fn new() -> Result<Self> {
        Ok(Self {
            db: Mutex::new(Database::open()?),
        })
    }

    /// Hook from a pre-existing Database (used by tests + the daemon when
    /// it wants to share a single connection).
    pub fn from_database(db: Database) -> Self {
        Self { db: Mutex::new(db) }
    }

    /// Estimate USD for a call with `prompt_tokens` against the active
    /// provider/model's rate card. Returns `Usd::ZERO` if no rate row
    /// exists (treat as priceless — local providers, mostly).
    fn estimate_for_message_count(db: &Database, message_count: usize) -> Usd {
        let (provider_id, model) = active_provider_and_model(db);
        // Coarse pre-flight estimate: average message ≈ 50 tokens. The
        // pre-flight check exists to catch *obvious* cap crossings (you're
        // already at 99% and a 5%-of-cap call would push over). Exact
        // accounting is post-flight via `Agent::record_usage`.
        let estimated_tokens = (message_count as u64).saturating_mul(50);
        match RateCard::lookup(
            db.conn(),
            &provider_id,
            &model,
            chrono::Utc::now().timestamp(),
        ) {
            Ok(Some(row)) => RateCard::price(&row, estimated_tokens, 0),
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

impl Hook for BudgetHook {
    fn name(&self) -> &str {
        "budget"
    }

    fn points(&self) -> &[HookPoint] {
        &[HookPoint::BeforeLlmCall, HookPoint::BeforeAgentStart]
    }

    fn execute(&self, ctx: &HookContext) -> HookAction {
        let message_count = match &ctx.data {
            HookData::LlmCall { message_count } => *message_count,
            HookData::AgentStart { user_message } => {
                // Approximate: the very first call carries the user
                // message + system prompt. Use a coarse 1-message
                // placeholder; pre-flight only needs to catch cap crossings.
                1 + user_message.len() / 200
            }
            _ => return HookAction::Continue,
        };

        let db = match self.db.lock() {
            Ok(g) => g,
            Err(_) => {
                tracing::warn!("BudgetHook: mutex poisoned, allowing call");
                return HookAction::Continue;
            }
        };

        let estimate = Self::estimate_for_message_count(&db, message_count);

        match BudgetEngine::pre_flight(db.conn(), None, estimate) {
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
