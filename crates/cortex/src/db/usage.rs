use anyhow::{Context, Result};
use chrono::Datelike;
use tracing::instrument;
use libsql::params;

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
    /// silently render "$0.00 estimated" — exactly the misleading number
    /// the hermes implementation flagged as "pricing unreliable."
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
