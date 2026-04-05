use anyhow::{Context, Result};
use chrono::Datelike;
use rusqlite::params;
use tracing::instrument;

use super::models::ModelUsageRow;
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

    pub fn log_token_usage(
        &self,
        prompt: u64,
        completion: u64,
        total: u64,
        provider: &str,
        model: &str,
        cost_usd: Option<f64>,
    ) -> Result<()> {
        self.log_token_usage_with_cache(prompt, completion, total, 0, 0, provider, model, cost_usd)
    }

    /// Log token usage including prompt-cache breakdown. `cached_input_tokens`
    /// counts cache-hit tokens (read) and `cache_creation_tokens` counts
    /// tokens written to the cache this turn (Anthropic only).
    #[allow(clippy::too_many_arguments)]
    pub fn log_token_usage_with_cache(
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
            params![
                now,
                prompt as i64,
                completion as i64,
                total as i64,
                provider,
                model,
                cost_usd,
                cached_input as i64,
                cache_creation as i64,
            ],
        )?;
        Ok(())
    }

    /// Cumulative prompt cache tokens since `since_ts` (Unix seconds).
    /// Returns `(prompt_tokens, cached_input_tokens, cache_creation_tokens)`.
    #[instrument(skip_all)]
    pub fn cache_token_summary_since(&self, since_ts: i64) -> Result<(u64, u64, u64)> {
        let mut stmt = self.conn.prepare(
            "SELECT COALESCE(SUM(prompt_tokens), 0),
                    COALESCE(SUM(cached_input_tokens), 0),
                    COALESCE(SUM(cache_creation_tokens), 0)
             FROM token_usage WHERE timestamp >= ?1",
        )?;
        let row: (i64, i64, i64) =
            stmt.query_row(params![since_ts], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        Ok((
            row.0.max(0) as u64,
            row.1.max(0) as u64,
            row.2.max(0) as u64,
        ))
    }

    #[instrument(skip_all)]
    pub fn monthly_token_total(&self) -> Result<u64> {
        let start_ts = Self::month_start_ts()?;
        let mut stmt = self.conn.prepare(
            "SELECT COALESCE(SUM(total_tokens), 0) FROM token_usage WHERE timestamp >= ?1",
        )?;
        let total: i64 = stmt.query_row(params![start_ts], |row| row.get(0))?;
        Ok(total as u64)
    }

    #[instrument(skip_all)]
    pub fn monthly_total_cost(&self) -> Result<Option<f64>> {
        let start_ts = Self::month_start_ts()?;
        let mut stmt = self
            .conn
            .prepare("SELECT SUM(cost_usd) FROM token_usage WHERE timestamp >= ?1")?;
        let cost: Option<f64> = stmt.query_row(params![start_ts], |row| row.get(0))?;
        Ok(cost)
    }

    #[instrument(skip_all)]
    pub fn monthly_usage_by_model(&self) -> Result<Vec<ModelUsageRow>> {
        let start_ts = Self::month_start_ts()?;
        let mut stmt = self.conn.prepare(
            "SELECT COALESCE(provider, '') as provider, COALESCE(model, '') as model,
                    COALESCE(SUM(prompt_tokens), 0), COALESCE(SUM(completion_tokens), 0),
                    COALESCE(SUM(total_tokens), 0), SUM(cost_usd)
             FROM token_usage WHERE timestamp >= ?1
             GROUP BY provider, model
             ORDER BY SUM(total_tokens) DESC",
        )?;
        let rows = stmt
            .query_map(params![start_ts], |row| {
                Ok(ModelUsageRow {
                    provider: row.get(0)?,
                    model: row.get(1)?,
                    prompt_tokens: row.get::<_, i64>(2)? as u64,
                    completion_tokens: row.get::<_, i64>(3)? as u64,
                    total_tokens: row.get::<_, i64>(4)? as u64,
                    total_cost_usd: row.get(5)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }
}
