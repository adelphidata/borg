//! Budget pre/post-flight engine.
//!
//! Reads the `budgets` table, decides whether an estimated LLM call is
//! allowed (`pre_flight`), reconciles the actual reported spend after the
//! call (`post_flight`), and advances expired periods on a daemon tick
//! (`reset_if_due`). Every state transition writes a row to
//! `provider_call_events` for replayable audit.
//!
//! Effective cap = `min(matching_principal, default)`. If a `principal:<id>`
//! row exists with a tighter cap than the daemon-wide `default`, that wins.
//!
//! Mid-stream cancellation is forbidden: `post_flight` always accepts the
//! actual spend. If actual exceeds the cap (rare overshoot), the *next*
//! `pre_flight` refuses.

use anyhow::{anyhow, Context, Result};
use serde_json::json;
use turso::params;

use crate::db::Database;

use super::usd::Usd;

/// Outcome of [`BudgetEngine::pre_flight`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreFlightDecision {
    /// Call is fully under cap — proceed.
    Allow,
    /// Call is allowed, but the soft cap warning should fire (debounced
    /// once per period — engine has already updated `last_soft_toast_period`
    /// when this variant is returned).
    SoftWarn { soft_cap: Usd, spent: Usd },
    /// Call would push spend past the hard cap. Refuse. A `budget_hit`
    /// event has already been written.
    HardRefuse {
        scope: String,
        hard_cap: Usd,
        spent: Usd,
    },
}

/// Reset cadence for a budget. Stored as a string in
/// `budgets.reset_period`; parsed via `Self::from_db`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResetPeriod {
    /// 24-hour rolling window from `current_period_start`.
    Daily,
    /// 7-day rolling window.
    Weekly,
    /// Approximate calendar month (30 days). v3 may switch to true
    /// calendar-month boundaries.
    Monthly,
}

impl ResetPeriod {
    fn from_db(s: &str) -> Result<Self> {
        match s {
            "daily" => Ok(Self::Daily),
            "weekly" => Ok(Self::Weekly),
            "monthly" => Ok(Self::Monthly),
            other => Err(anyhow!("invalid reset_period '{other}'")),
        }
    }

    fn duration_secs(self) -> i64 {
        match self {
            Self::Daily => 86_400,
            Self::Weekly => 7 * 86_400,
            Self::Monthly => 30 * 86_400,
        }
    }
}

/// One row from `budgets`.
#[derive(Debug, Clone)]
struct BudgetRow {
    scope: String,
    reset_period: ResetPeriod,
    hard_cap: Usd,
    soft_cap: Option<Usd>,
    current_spent: Usd,
    current_period_start: i64,
    last_soft_toast_period: Option<i64>,
}

impl BudgetRow {
    fn period_secs(&self) -> i64 {
        self.reset_period.duration_secs()
    }

    fn period_end(&self) -> i64 {
        self.current_period_start + self.period_secs()
    }
}

/// Budget enforcement engine. Stateless — every call queries fresh state
/// from the DB, so concurrent sessions inside a daemon all see the same
/// `current_spent_usd` after each post-flight reconcile.
pub struct BudgetEngine;

impl BudgetEngine {
    /// Decide whether a call estimated at `estimate` USD is allowed for
    /// `principal_scope` (e.g. `"principal:p1"`). The daemon-wide `default`
    /// budget is always considered; the lower remaining headroom wins.
    pub async fn pre_flight(
        db: &Database,
        principal_scope: Option<&str>,
        estimate: Usd,
    ) -> Result<PreFlightDecision> {
        let default = Self::load_row(db, "default")
            .await?
            .ok_or_else(|| anyhow!("default budget row missing — schema not applied"))?;
        let principal = match principal_scope {
            Some(s) if s != "default" => Self::load_row(db, s).await?,
            _ => None,
        };

        // Choose the binding row: lowest remaining headroom.
        let candidates = [Some(default), principal]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();
        let binding = candidates
            .iter()
            .min_by_key(|r| r.hard_cap.saturating_sub(r.current_spent).raw_micros())
            .ok_or_else(|| anyhow!("no candidate budget rows"))?;

        let projected = binding.current_spent + estimate;

        if projected > binding.hard_cap {
            // Write the budget_hit event before returning so a caller that
            // bubbles HardRefuse straight to the user has the audit row in
            // place by the time the user sees it.
            let metadata = json!({
                "hard_cap_usd": binding.hard_cap.to_storage_string(),
                "spent_usd": binding.current_spent.to_storage_string(),
                "estimate_usd": estimate.to_storage_string(),
                "projected_usd": projected.to_storage_string(),
                "period_start": binding.current_period_start,
            })
            .to_string();
            db.conn()
                .execute(
                    "INSERT INTO provider_call_events
                     (event_type, budget_scope, usd_cost, metadata_json, created_at)
                     VALUES ('budget_hit', ?1, ?2, ?3, ?4)",
                    params!(
                        binding.scope.clone(),
                        estimate.to_storage_string(),
                        metadata,
                        chrono::Utc::now().timestamp(),
                    ),
                )
                .await
                .context("failed to write budget_hit event")?;
            return Ok(PreFlightDecision::HardRefuse {
                scope: binding.scope.clone(),
                hard_cap: binding.hard_cap,
                spent: binding.current_spent,
            });
        }

        if let Some(soft_cap) = binding.soft_cap {
            // Soft warn at ≥80% of soft cap. Debounce via
            // `last_soft_toast_period`: only fire once per period.
            let threshold = soft_cap.scale_percent(80);
            if projected >= threshold
                && binding.last_soft_toast_period != Some(binding.current_period_start)
            {
                // Mark the toast as fired for this period so subsequent
                // pre-flights inside the same period are quiet.
                db.conn()
                    .execute(
                        "UPDATE budgets SET last_soft_toast_period = ?1, updated_at = ?2
                         WHERE scope = ?3",
                        params!(
                            binding.current_period_start,
                            chrono::Utc::now().timestamp(),
                            binding.scope.clone(),
                        ),
                    )
                    .await
                    .context("failed to update last_soft_toast_period")?;
                return Ok(PreFlightDecision::SoftWarn {
                    soft_cap,
                    spent: binding.current_spent,
                });
            }
        }

        Ok(PreFlightDecision::Allow)
    }

    /// Reconcile actual reported spend after the LLM call returned. Adds
    /// `actual` to `current_spent_usd`, writes a `provider_call` event.
    /// Mid-stream cancellation is forbidden — this method is infallible
    /// from the budget standpoint (overshoots are absorbed; the *next*
    /// pre-flight refuses).
    #[allow(clippy::too_many_arguments)]
    pub async fn post_flight(
        db: &Database,
        principal_scope: Option<&str>,
        provider_id: &str,
        model: &str,
        rate_card_effective_at: Option<i64>,
        prompt_tokens: u64,
        completion_tokens: u64,
        cached_tokens: u64,
        actual: Usd,
        session_id: Option<&str>,
    ) -> Result<()> {
        let now = chrono::Utc::now().timestamp();
        // BEGIN IMMEDIATE so a concurrent reset/post_flight sees serialized state.
        db.conn()
            .execute_batch("BEGIN IMMEDIATE;")
            .await
            .context("failed to begin post_flight transaction")?;
        let res: Result<()> = async {
            // Bump default; bump principal if it exists.
            Self::bump_spent(db, "default", actual, now).await?;
            if let Some(scope) = principal_scope {
                if scope != "default" {
                    // Only bump if the row exists (ignore if no per-principal cap).
                    let mut rows = db
                        .conn()
                        .query(
                            "SELECT COUNT(*) FROM budgets WHERE scope = ?1",
                            params!(scope),
                        )
                        .await?;
                    let exists: i64 = match rows.next().await? {
                        Some(r) => r.get::<i64>(0)?,
                        None => 0,
                    };
                    if exists > 0 {
                        Self::bump_spent(db, scope, actual, now).await?;
                    }
                }
            }

            let metadata = json!({
                "prompt_tokens": prompt_tokens,
                "completion_tokens": completion_tokens,
                "cached_tokens": cached_tokens,
            })
            .to_string();

            db.conn()
                .execute(
                    "INSERT INTO provider_call_events
                     (event_type, provider_id, model, rate_card_effective_at,
                      prompt_tokens, completion_tokens, cached_tokens,
                      usd_cost, budget_scope, metadata_json, session_id, created_at)
                     VALUES ('provider_call', ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                    params!(
                        provider_id,
                        model,
                        rate_card_effective_at,
                        prompt_tokens as i64,
                        completion_tokens as i64,
                        cached_tokens as i64,
                        actual.to_storage_string(),
                        principal_scope.unwrap_or("default").to_string(),
                        metadata,
                        session_id.map(|s| s.to_string()),
                        now,
                    ),
                )
                .await
                .context("failed to write provider_call event")?;
            Ok(())
        }
        .await;

        match res {
            Ok(()) => {
                db.conn()
                    .execute_batch("COMMIT;")
                    .await
                    .context("failed to commit post_flight")?;
                Ok(())
            }
            Err(e) => {
                if let Err(rb) = db.conn().execute_batch("ROLLBACK;").await {
                    tracing::warn!("post_flight rollback failed: {rb}");
                }
                Err(e)
            }
        }
    }

    async fn bump_spent(db: &Database, scope: &str, delta: Usd, now: i64) -> Result<()> {
        let mut rows = db
            .conn()
            .query(
                "SELECT current_spent_usd FROM budgets WHERE scope = ?1",
                params!(scope),
            )
            .await
            .with_context(|| format!("failed to read current_spent_usd for scope '{scope}'"))?;
        let current: String = match rows.next().await? {
            Some(r) => r.get::<String>(0)?,
            None => return Err(anyhow!("budget scope '{scope}' missing")),
        };
        drop(rows);
        let new_spent = Usd::parse(&current)? + delta;
        db.conn()
            .execute(
                "UPDATE budgets SET current_spent_usd = ?1, updated_at = ?2 WHERE scope = ?3",
                params!(new_spent.to_storage_string(), now, scope),
            )
            .await
            .with_context(|| format!("failed to update current_spent_usd for scope '{scope}'"))?;
        Ok(())
    }

    /// Daemon tick: zero spend on rows whose period has elapsed and write a
    /// `budget_reset` event recording the prior spend. Returns the number
    /// of rows reset.
    pub async fn reset_if_due(db: &Database, now: i64) -> Result<usize> {
        db.conn()
            .execute_batch("BEGIN IMMEDIATE;")
            .await
            .context("failed to begin reset_if_due transaction")?;

        let res: Result<usize> = async {
            let mut rows = db
                .conn()
                .query(
                    "SELECT id, scope, reset_period, hard_cap_usd, soft_cap_usd,
                            current_spent_usd, current_period_start, last_soft_toast_period
                     FROM budgets",
                    (),
                )
                .await
                .context("failed to scan budgets")?;
            let mut budget_rows: Vec<BudgetRow> = Vec::new();
            while let Some(row) = rows.next().await? {
                budget_rows.push(Self::row_from_db(&row)?);
            }
            drop(rows);

            let mut count = 0usize;
            for row in budget_rows {
                if now < row.period_end() {
                    continue;
                }
                let prior = row.current_spent;
                let new_period_start = row.current_period_start + row.period_secs();
                db.conn()
                    .execute(
                        "UPDATE budgets
                         SET current_spent_usd = '0',
                             current_period_start = ?1,
                             last_soft_toast_period = NULL,
                             updated_at = ?2
                         WHERE scope = ?3",
                        params!(new_period_start, now, row.scope.clone()),
                    )
                    .await
                    .with_context(|| format!("failed to reset scope '{}'", row.scope))?;
                let metadata = json!({
                    "prior_spent_usd": prior.to_storage_string(),
                    "prior_period_start": row.current_period_start,
                    "new_period_start": new_period_start,
                })
                .to_string();
                db.conn()
                    .execute(
                        "INSERT INTO provider_call_events
                         (event_type, budget_scope, usd_cost, metadata_json, created_at)
                         VALUES ('budget_reset', ?1, ?2, ?3, ?4)",
                        params!(row.scope, prior.to_storage_string(), metadata, now),
                    )
                    .await
                    .context("failed to write budget_reset event")?;
                count += 1;
            }
            Ok(count)
        }
        .await;

        match res {
            Ok(count) => {
                db.conn()
                    .execute_batch("COMMIT;")
                    .await
                    .context("failed to commit reset")?;
                Ok(count)
            }
            Err(e) => {
                if let Err(rb) = db.conn().execute_batch("ROLLBACK;").await {
                    tracing::warn!("reset_if_due rollback failed: {rb}");
                }
                Err(e)
            }
        }
    }

    /// Snapshot of `current_spent_usd` and `hard_cap_usd` for the binding
    /// scope (used by `/status`). Returns `None` if no budget exists.
    pub async fn status_snapshot(
        db: &Database,
        principal_scope: Option<&str>,
    ) -> Result<Option<BudgetStatus>> {
        let default = Self::load_row(db, "default").await?;
        let principal = match principal_scope {
            Some(s) if s != "default" => Self::load_row(db, s).await?,
            _ => None,
        };
        let binding = match (default, principal) {
            (None, None) => return Ok(None),
            (Some(d), None) => d,
            (None, Some(p)) => p,
            (Some(d), Some(p)) => {
                let p_headroom = p.hard_cap.saturating_sub(p.current_spent).raw_micros();
                let d_headroom = d.hard_cap.saturating_sub(d.current_spent).raw_micros();
                if p_headroom < d_headroom {
                    p
                } else {
                    d
                }
            }
        };
        Ok(Some(BudgetStatus {
            scope: binding.scope.clone(),
            spent: binding.current_spent,
            hard_cap: binding.hard_cap,
            reset_period: binding.reset_period,
            current_period_start: binding.current_period_start,
        }))
    }

    async fn load_row(db: &Database, scope: &str) -> Result<Option<BudgetRow>> {
        let mut rows = db
            .conn()
            .query(
                "SELECT id, scope, reset_period, hard_cap_usd, soft_cap_usd,
                        current_spent_usd, current_period_start, last_soft_toast_period
                 FROM budgets WHERE scope = ?1",
                params!(scope),
            )
            .await
            .with_context(|| format!("failed to load budget row '{scope}'"))?;
        match rows.next().await? {
            Some(row) => Ok(Some(Self::row_from_db(&row)?)),
            None => Ok(None),
        }
    }

    fn row_from_db(row: &turso::Row) -> Result<BudgetRow> {
        let _id: String = row.get::<String>(0)?;
        let scope: String = row.get::<String>(1)?;
        let reset_period_str: String = row.get::<String>(2)?;
        let hard_cap_str: String = row.get::<String>(3)?;
        let soft_cap_str: Option<String> = row.get::<Option<String>>(4)?;
        let current_spent_str: String = row.get::<String>(5)?;
        let current_period_start: i64 = row.get::<i64>(6)?;
        let last_soft_toast_period: Option<i64> = row.get::<Option<i64>>(7)?;

        let reset_period = ResetPeriod::from_db(&reset_period_str)?;
        let hard_cap = Usd::parse(&hard_cap_str)?;
        let soft_cap = soft_cap_str.as_deref().map(Usd::parse).transpose()?;
        let current_spent = Usd::parse(&current_spent_str)?;

        Ok(BudgetRow {
            scope,
            reset_period,
            hard_cap,
            soft_cap,
            current_spent,
            current_period_start,
            last_soft_toast_period,
        })
    }
}

/// Snapshot for `/status` rendering.
#[derive(Debug, Clone)]
pub struct BudgetStatus {
    /// Which scope is binding (lowest headroom).
    pub scope: String,
    /// Spend so far this period.
    pub spent: Usd,
    /// Hard cap for the period.
    pub hard_cap: Usd,
    /// Period cadence.
    pub reset_period: ResetPeriod,
    /// Unix timestamp of the period start.
    pub current_period_start: i64,
}
