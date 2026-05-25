use anyhow::{Context, Result};
use chrono::Datelike;
use libsql::params;
use tracing::instrument;

use super::models::{ModelUsageRow, TokenSummary};
use super::Database;

impl Database {
    // ── Token usage ──

    fn month_start_ts() -> Result<i64> {
        let now = chrono::Utc::now();
        let first_of_month = now.date_naive().with_day(1).unwrap_or(now.date_naive());
        let midnight = first_of_month
            .and_hms_opt(0, 0, 0)
            .context("failed to construct midnight timestamp")?;
        Ok(midnight.and_utc().timestamp())
    }

    pub async fn log_token_usage(
        &self,
        prompt: u64,
        completion: u64,
        total: u64,
        provider: &str,
        model: &str,
        cost_usd: Option<f64>,
    ) -> Result<()> {
        self.log_token_usage_with_cache(prompt, completion, total, 0, 0, provider, model, cost_usd)
            .await
    }

    /// Log token usage including prompt-cache breakdown. `cached_input_tokens`
    /// counts cache-hit tokens (read) and `cache_creation_tokens` counts
    /// tokens written to the cache this turn (Anthropic only).
    #[allow(clippy::too_many_arguments)]
    pub async fn log_token_usage_with_cache(
        &self,
        prompt: u64,
        completion: u64,
        total: u64,
        cached_input: u64,
        cache_creation: u64,
        provider: &str,
        model: &str,
        cost_usd: Option<f64>,
    ) -> Result<()> {
        let now = chrono::Utc::now().timestamp();
        self.conn.execute(
            "INSERT INTO token_usage (timestamp, prompt_tokens, completion_tokens, total_tokens, provider, model, cost_usd, cached_input_tokens, cache_creation_tokens)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params!(
                now,
                prompt as i64,
                completion as i64,
                total as i64,
                provider,
                model,
                cost_usd,
                cached_input as i64,
                cache_creation as i64,
            ),
        ).await?;
        Ok(())
    }

    /// Cumulative prompt cache tokens since `since_ts` (Unix seconds).
    /// Returns `(prompt_tokens, cached_input_tokens, cache_creation_tokens)`.
    #[instrument(skip_all)]
    pub async fn cache_token_summary_since(&self, since_ts: i64) -> Result<(u64, u64, u64)> {
        let mut rows = self
            .conn
            .query(
                "SELECT COALESCE(SUM(prompt_tokens), 0),
                    COALESCE(SUM(cached_input_tokens), 0),
                    COALESCE(SUM(cache_creation_tokens), 0)
             FROM token_usage WHERE timestamp >= ?1",
                params!(since_ts),
            )
            .await?;
        let (p, c, cr): (i64, i64, i64) = match rows.next().await? {
            Some(r) => (r.get::<i64>(0)?, r.get::<i64>(1)?, r.get::<i64>(2)?),
            None => (0, 0, 0),
        };
        Ok((p.max(0) as u64, c.max(0) as u64, cr.max(0) as u64))
    }

    #[instrument(skip_all)]
    pub async fn monthly_token_total(&self) -> Result<u64> {
        let start_ts = Self::month_start_ts()?;
        let mut rows = self
            .conn
            .query(
                "SELECT COALESCE(SUM(total_tokens), 0) FROM token_usage WHERE timestamp >= ?1",
                params!(start_ts),
            )
            .await?;
        let total: i64 = match rows.next().await? {
            Some(row) => row.get::<i64>(0)?,
            None => 0,
        };
        Ok(total as u64)
    }

    #[instrument(skip_all)]
    pub async fn monthly_total_cost(&self) -> Result<Option<f64>> {
        let start_ts = Self::month_start_ts()?;
        let mut rows = self
            .conn
            .query(
                "SELECT SUM(cost_usd) FROM token_usage WHERE timestamp >= ?1",
                params!(start_ts),
            )
            .await?;
        let cost: Option<f64> = match rows.next().await? {
            Some(row) => row.get::<Option<f64>>(0)?,
            None => None,
        };
        Ok(cost)
    }

    /// Bucket the current month's token usage against `limit` into the
    /// nearest crossed warning threshold: `Some(75)`, `Some(90)`, or
    /// `Some(100)`. Returns `None` when `limit` is `0` (unlimited) or
    /// usage is below 75%. Numbers are pre-bucketed so the heartbeat
    /// `budget_threshold` augmenter can't fabricate intermediate values.
    pub async fn monthly_budget_pct(&self, limit: u64) -> Result<Option<u8>> {
        if limit == 0 {
            return Ok(None);
        }
        let total = self.monthly_token_total().await?;
        let pct = (total as f64) / (limit as f64) * 100.0;
        Ok(if pct >= 100.0 {
            Some(100)
        } else if pct >= 90.0 {
            Some(90)
        } else if pct >= 75.0 {
            Some(75)
        } else {
            None
        })
    }

    #[instrument(skip_all)]
    pub async fn monthly_usage_by_model(&self) -> Result<Vec<ModelUsageRow>> {
        self.usage_by_model_since(Self::month_start_ts()?).await
    }

    /// Aggregate token usage by `(provider, model)` for `token_usage` rows
    /// at or after `since_ts`. Ordered by `total_tokens` descending so the
    /// `/usage` formatter can take the top N.
    #[instrument(skip_all)]
    pub async fn usage_by_model_since(&self, since_ts: i64) -> Result<Vec<ModelUsageRow>> {
        let mut rows = self
            .conn
            .query(
                "SELECT COALESCE(provider, '') as provider, COALESCE(model, '') as model,
                    COALESCE(SUM(prompt_tokens), 0), COALESCE(SUM(completion_tokens), 0),
                    COALESCE(SUM(total_tokens), 0), SUM(cost_usd)
             FROM token_usage WHERE timestamp >= ?1
             GROUP BY provider, model
             ORDER BY SUM(total_tokens) DESC",
                params!(since_ts),
            )
            .await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push(ModelUsageRow {
                provider: row.get::<String>(0)?,
                model: row.get::<String>(1)?,
                prompt_tokens: row.get::<i64>(2)? as u64,
                completion_tokens: row.get::<i64>(3)? as u64,
                total_tokens: row.get::<i64>(4)? as u64,
                total_cost_usd: row.get::<Option<f64>>(5)?,
            });
        }
        Ok(out)
    }

    /// Aggregate token totals over the window starting at `since_ts`.
    ///
    /// `cost_present` distinguishes "no rate-card data for any row" (false →
    /// the cost section is hidden) from "real $0.00" (true). Without this
    /// flag, a regression that returned `Some(0.0)` on no-cost rows would
    /// silently render "$0.00 estimated" — a misleading "pricing unreliable"
    /// number we want to avoid showing the user.
    #[instrument(skip_all)]
    pub async fn token_summary_since(&self, since_ts: i64) -> Result<TokenSummary> {
        let mut rows = self
            .conn
            .query(
                "SELECT COALESCE(SUM(prompt_tokens), 0),
                    COALESCE(SUM(completion_tokens), 0),
                    COALESCE(SUM(total_tokens), 0),
                    COALESCE(SUM(cached_input_tokens), 0),
                    COALESCE(SUM(cache_creation_tokens), 0),
                    SUM(cost_usd),
                    COALESCE(SUM(CASE WHEN cost_usd IS NOT NULL THEN 1 ELSE 0 END), 0)
             FROM token_usage WHERE timestamp >= ?1",
                params!(since_ts),
            )
            .await?;
        match rows.next().await? {
            Some(row) => {
                let prompt: i64 = row.get::<i64>(0)?;
                let completion: i64 = row.get::<i64>(1)?;
                let total: i64 = row.get::<i64>(2)?;
                let cached_input: i64 = row.get::<i64>(3)?;
                let cache_creation: i64 = row.get::<i64>(4)?;
                let cost: Option<f64> = row.get::<Option<f64>>(5)?;
                let cost_row_count: i64 = row.get::<i64>(6)?;
                Ok(TokenSummary {
                    prompt: prompt.max(0) as u64,
                    completion: completion.max(0) as u64,
                    total: total.max(0) as u64,
                    cached_input: cached_input.max(0) as u64,
                    cache_creation: cache_creation.max(0) as u64,
                    cost,
                    cost_present: cost_row_count > 0,
                })
            }
            None => Ok(TokenSummary {
                prompt: 0,
                completion: 0,
                total: 0,
                cached_input: 0,
                cache_creation: 0,
                cost: None,
                cost_present: false,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn fresh_db() -> Database {
        Database::test_db().await
    }

    #[tokio::test]
    async fn monthly_total_is_zero_for_empty_table() {
        let db = fresh_db().await;
        assert_eq!(db.monthly_token_total().await.unwrap(), 0);
    }

    #[tokio::test]
    async fn log_token_usage_sums_into_monthly_total() {
        let db = fresh_db().await;
        db.log_token_usage(100, 200, 300, "openai", "gpt-4", Some(0.50))
            .await
            .unwrap();
        db.log_token_usage(50, 60, 110, "openai", "gpt-4", Some(0.10))
            .await
            .unwrap();
        // Two rows, total=300+110=410. A regression that double-counted
        // (e.g. SUM(prompt+completion+total) by mistake) would land at 820.
        assert_eq!(db.monthly_token_total().await.unwrap(), 410);
    }

    #[tokio::test]
    async fn monthly_total_cost_sums_present_and_skips_null() {
        let db = fresh_db().await;
        // Mix of priced and unpriced rows. SUM() over a column with NULLs
        // treats NULL as zero — the priced rows should sum to 0.50 + 0.10
        // = 0.60. The null row must not poison the sum.
        db.log_token_usage(10, 10, 20, "p", "m", Some(0.50))
            .await
            .unwrap();
        db.log_token_usage(10, 10, 20, "p", "m", None)
            .await
            .unwrap();
        db.log_token_usage(10, 10, 20, "p", "m", Some(0.10))
            .await
            .unwrap();
        let cost = db.monthly_total_cost().await.unwrap().expect("Some(_)");
        assert!((cost - 0.60).abs() < 1e-9, "expected 0.60, got {cost}");
    }

    #[tokio::test]
    async fn monthly_total_cost_is_none_when_no_rows_have_cost() {
        // Spec: cost_present=false must surface as None at this layer so the
        // UI can hide the cost column entirely instead of showing $0.00.
        let db = fresh_db().await;
        db.log_token_usage(10, 10, 20, "p", "m", None)
            .await
            .unwrap();
        assert_eq!(db.monthly_total_cost().await.unwrap(), None);
    }

    #[tokio::test]
    async fn monthly_budget_pct_thresholds_are_bucketed_not_continuous() {
        // Spec: the augmenter is allowed to fabricate ONLY {75, 90, 100}.
        // Any other value is a regression. Table-drive the bucketing logic.
        let db = fresh_db().await;
        // limit = 1000 → 750 = 75%, 900 = 90%, 1000 = 100%.
        db.log_token_usage(0, 0, 700, "p", "m", None).await.unwrap();
        assert_eq!(
            db.monthly_budget_pct(1000).await.unwrap(),
            None,
            "70% must NOT trigger"
        );
        db.log_token_usage(0, 0, 60, "p", "m", None).await.unwrap();
        // total = 760 → 76%, bucket 75.
        assert_eq!(db.monthly_budget_pct(1000).await.unwrap(), Some(75));
        db.log_token_usage(0, 0, 200, "p", "m", None).await.unwrap();
        // total = 960 → 96%, bucket 90.
        assert_eq!(db.monthly_budget_pct(1000).await.unwrap(), Some(90));
        db.log_token_usage(0, 0, 40, "p", "m", None).await.unwrap();
        // total = 1000 → 100%, bucket 100.
        assert_eq!(db.monthly_budget_pct(1000).await.unwrap(), Some(100));
        db.log_token_usage(0, 0, 9999, "p", "m", None)
            .await
            .unwrap();
        // Wildly over budget — still pinned at 100, never a higher value.
        assert_eq!(db.monthly_budget_pct(1000).await.unwrap(), Some(100));
    }

    #[tokio::test]
    async fn monthly_budget_pct_zero_limit_is_unlimited() {
        // Spec: 0 limit means "unlimited", not "instantly 100%" — a
        // regression to division-by-zero or always-Some(100) would spam
        // budget warnings for operators who explicitly opted out.
        let db = fresh_db().await;
        db.log_token_usage(0, 0, 999_999, "p", "m", None)
            .await
            .unwrap();
        assert_eq!(db.monthly_budget_pct(0).await.unwrap(), None);
    }

    #[tokio::test]
    async fn token_summary_distinguishes_no_cost_data_from_zero_cost() {
        // The cost_present flag is the spec — it lets the UI hide the
        // cost column when no row carried a price, instead of showing
        // a misleading $0.00.
        let db = fresh_db().await;
        let empty = db.token_summary_since(0).await.unwrap();
        assert_eq!(empty.total, 0);
        assert!(
            !empty.cost_present,
            "empty table must be cost_present=false"
        );
        assert_eq!(empty.cost, None);

        db.log_token_usage(10, 5, 15, "p", "m", None).await.unwrap();
        let no_cost = db.token_summary_since(0).await.unwrap();
        assert_eq!(no_cost.total, 15);
        assert!(
            !no_cost.cost_present,
            "row without cost stays cost_present=false"
        );

        db.log_token_usage(20, 10, 30, "p", "m", Some(0.0))
            .await
            .unwrap();
        let real_zero = db.token_summary_since(0).await.unwrap();
        assert!(
            real_zero.cost_present,
            "row with explicit Some(0.0) must flip cost_present to true"
        );
        // Two rows now: 15 + 30 = 45 total tokens.
        assert_eq!(real_zero.total, 45);
    }

    #[tokio::test]
    async fn usage_by_model_groups_and_orders_descending() {
        // Spec: aggregation must group by (provider, model) and order by
        // total_tokens DESC so the UI can take the top N. A regression
        // that swapped ASC/DESC or skipped GROUP BY would silently
        // produce a misleading "/usage" view.
        let db = fresh_db().await;
        db.log_token_usage(10, 10, 20, "openai", "gpt-4", None)
            .await
            .unwrap();
        db.log_token_usage(50, 50, 100, "anthropic", "claude", None)
            .await
            .unwrap();
        db.log_token_usage(10, 10, 20, "openai", "gpt-4", None)
            .await
            .unwrap();
        let rows = db.monthly_usage_by_model().await.unwrap();
        assert_eq!(rows.len(), 2, "must group by (provider, model)");
        // anthropic/claude total = 100 > openai/gpt-4 total = 40 → first row.
        assert_eq!(rows[0].provider, "anthropic");
        assert_eq!(rows[0].model, "claude");
        assert_eq!(rows[0].total_tokens, 100);
        assert_eq!(rows[1].provider, "openai");
        assert_eq!(rows[1].total_tokens, 40);
    }

    #[tokio::test]
    async fn cache_token_summary_since_aggregates_breakdown() {
        // Each call's cached_input + cache_creation must accumulate
        // separately from prompt — they're billed differently and a
        // confusion here would inflate the cost estimator.
        let db = fresh_db().await;
        db.log_token_usage_with_cache(100, 0, 100, 80, 20, "anthropic", "claude", None)
            .await
            .unwrap();
        db.log_token_usage_with_cache(200, 0, 200, 60, 10, "anthropic", "claude", None)
            .await
            .unwrap();
        let (p, cached, created) = db.cache_token_summary_since(0).await.unwrap();
        assert_eq!(p, 300, "prompt sum");
        assert_eq!(cached, 140, "cached_input sum");
        assert_eq!(created, 30, "cache_creation sum");
    }
}
