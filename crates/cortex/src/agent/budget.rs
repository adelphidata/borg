//! Monthly token-budget gate for the agent loop.
//!
//! Pulled out of `agent/mod.rs`. The [`BudgetCheck`] outcome and
//! [`Agent::check_budget`] form the per-iteration gate that `run_agent_loop`
//! consults before issuing the next LLM call.

use tracing::warn;

use crate::db::Database;

use super::Agent;

/// Result of monthly token budget check.
pub(super) enum BudgetCheck {
    Ok,
    Warning(String),
    Exceeded(String),
}

impl Agent {
    /// Check monthly token budget. Returns an error message if exceeded, a warning if near limit, or None.
    pub(super) fn check_budget(&self) -> BudgetCheck {
        let budget_limit = self.config.budget.monthly_token_limit;
        if budget_limit == 0 {
            return BudgetCheck::Ok;
        }
        let Ok(db) = Database::open() else {
            warn!("Budget enforcement skipped: database unavailable");
            return BudgetCheck::Ok;
        };
        let Ok(used) = db.monthly_token_total() else {
            return BudgetCheck::Ok;
        };
        if used >= budget_limit {
            return BudgetCheck::Exceeded(format!(
                "Monthly token budget exceeded ({used}/{budget_limit}). \
                 Increase budget.monthly_token_limit in /settings to continue."
            ));
        }
        let threshold = self.config.budget.warning_threshold;
        let ratio = used as f64 / budget_limit as f64;
        if ratio >= 0.95 || ratio >= threshold {
            let pct = (ratio * 100.0) as u64;
            BudgetCheck::Warning(format!(
                "Warning: {pct}% of monthly token budget used ({used}/{budget_limit})"
            ))
        } else {
            BudgetCheck::Ok
        }
    }
}
