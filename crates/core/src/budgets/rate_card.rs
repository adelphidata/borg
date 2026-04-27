//! Rate-card lookup against `provider_rate_cards`.
//!
//! A row is selected by `(provider_id, model, call_ts)` using
//! `effective_at = MAX(effective_at) WHERE effective_at <= call_ts`, so the
//! same call replayed from the audit chain at any future time picks the
//! same rate.

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};

use super::usd::Usd;

/// One row from `provider_rate_cards`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RateRow {
    /// Unix timestamp the row became effective.
    pub effective_at: i64,
    /// USD per million prompt tokens.
    pub prompt_per_mtok: Usd,
    /// USD per million completion tokens.
    pub completion_per_mtok: Usd,
    /// USD per million cached prompt tokens, if priced separately.
    pub cached_per_mtok: Option<Usd>,
    /// `bundled` for the seeded snapshot, `manifest_<ts>` for
    /// `borg providers refresh-rates` writes.
    pub source: String,
}

/// Rate-card resolver.
pub struct RateCard;

impl RateCard {
    /// Look up the rate row in effect for `(provider_id, model)` at
    /// `call_ts`. Returns `None` if no row exists for the pair, or if every
    /// row's `effective_at` is in the future relative to `call_ts`.
    ///
    /// If `model` has no exact row, falls back to `*` (wildcard) which the
    /// V43 seed uses for zero-cost local providers — this prevents silent
    /// `None` returns that would skip post-flight accounting.
    pub fn lookup(
        conn: &Connection,
        provider_id: &str,
        model: &str,
        call_ts: i64,
    ) -> Result<Option<RateRow>> {
        if let Some(row) = Self::lookup_exact(conn, provider_id, model, call_ts)? {
            return Ok(Some(row));
        }
        Self::lookup_exact(conn, provider_id, "*", call_ts)
    }

    fn lookup_exact(
        conn: &Connection,
        provider_id: &str,
        model: &str,
        call_ts: i64,
    ) -> Result<Option<RateRow>> {
        conn.query_row(
            "SELECT effective_at, prompt_per_mtok_usd, completion_per_mtok_usd,
                    cached_per_mtok_usd, source
             FROM provider_rate_cards
             WHERE provider_id = ?1 AND model = ?2 AND effective_at <= ?3
             ORDER BY effective_at DESC
             LIMIT 1",
            params![provider_id, model, call_ts],
            |row| {
                let effective_at: i64 = row.get(0)?;
                let prompt: String = row.get(1)?;
                let completion: String = row.get(2)?;
                let cached: Option<String> = row.get(3)?;
                let source: String = row.get(4)?;
                Ok((effective_at, prompt, completion, cached, source))
            },
        )
        .optional()
        .context("rate-card lookup failed")?
        .map(|(effective_at, prompt, completion, cached, source)| {
            Ok(RateRow {
                effective_at,
                prompt_per_mtok: Usd::parse(&prompt)?,
                completion_per_mtok: Usd::parse(&completion)?,
                cached_per_mtok: cached.as_deref().map(Usd::parse).transpose()?,
                source,
            })
        })
        .transpose()
    }

    /// Compute USD cost for a call given a rate row + token counts.
    pub fn price(row: &RateRow, prompt_tokens: u64, completion_tokens: u64) -> Usd {
        Usd::from_tokens_at_rate(prompt_tokens, row.prompt_per_mtok)
            + Usd::from_tokens_at_rate(completion_tokens, row.completion_per_mtok)
    }

    /// Append (never mutate) a new rate-card row.
    ///
    /// The `effective_at` PRIMARY KEY component means a refresh that re-uses
    /// an existing timestamp would collide; callers should pick `now` (or a
    /// stamp from the manifest) so historical replays still resolve to the
    /// older row.
    #[allow(clippy::too_many_arguments)]
    pub fn append(
        conn: &Connection,
        provider_id: &str,
        model: &str,
        effective_at: i64,
        prompt_per_mtok: Usd,
        completion_per_mtok: Usd,
        cached_per_mtok: Option<Usd>,
        source: &str,
    ) -> Result<()> {
        conn.execute(
            "INSERT INTO provider_rate_cards
             (provider_id, model, effective_at, prompt_per_mtok_usd,
              completion_per_mtok_usd, cached_per_mtok_usd, source)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                provider_id,
                model,
                effective_at,
                prompt_per_mtok.to_storage_string(),
                completion_per_mtok.to_storage_string(),
                cached_per_mtok.map(Usd::to_storage_string),
                source,
            ],
        )
        .context("failed to insert rate-card row")?;
        Ok(())
    }
}
