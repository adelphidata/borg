//! Rate-card lookup against `provider_rate_cards`.
//!
//! A row is selected by `(provider_id, model, call_ts)` using
//! `effective_at = MAX(effective_at) WHERE effective_at <= call_ts`, so the
//! same call replayed from the audit chain at any future time picks the
//! same rate.

use anyhow::{Context, Result};
use libsql::params;

use crate::db::Database;

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

/// The provider id and model a `<vendor>/<model>` id has in the bundled
/// cards, or `None` when the vendor has no cards there.
fn vendor_card(model: &str) -> Option<(&'static str, String)> {
    let (vendor, name) = model.split_once('/')?;
    let provider = match vendor.to_ascii_lowercase().as_str() {
        "anthropic" => "anthropic",
        "openai" => "openai",
        "google" => "gemini",
        "deepseek" => "deepseek",
        "x-ai" | "xai" => "xai",
        _ => return None,
    };
    Some((
        provider,
        crate::pricing::fold_claude_dotted_version(name).into_owned(),
    ))
}

/// Rate-card resolver.
pub struct RateCard;

impl RateCard {
    /// Look up the rate row in effect for `(provider_id, model)` at
    /// `call_ts`. Returns `None` if no row exists for the pair, or if every
    /// row's `effective_at` is in the future relative to `call_ts`.
    ///
    /// If `model` has no exact row, falls back to `*` (wildcard) which the
    /// schema seed uses for zero-cost local providers — this prevents silent
    /// `None` returns that would skip post-flight accounting.
    pub async fn lookup(
        db: &Database,
        provider_id: &str,
        model: &str,
        call_ts: i64,
    ) -> Result<Option<RateRow>> {
        if let Some(row) = Self::lookup_exact(db, provider_id, model, call_ts).await? {
            return Ok(Some(row));
        }
        if let Some(row) = Self::lookup_exact(db, provider_id, "*", call_ts).await? {
            return Ok(Some(row));
        }
        // An aggregator names the model `<vendor>/<model>` and has no card of
        // its own. The vendor's card is the closest fixed price; without it
        // the call skips post-flight and the dollar cap never sees its spend.
        match vendor_card(model) {
            Some((vendor, name)) => Self::lookup_exact(db, vendor, &name, call_ts).await,
            None => Ok(None),
        }
    }

    async fn lookup_exact(
        db: &Database,
        provider_id: &str,
        model: &str,
        call_ts: i64,
    ) -> Result<Option<RateRow>> {
        let mut rows = db
            .conn()
            .query(
                "SELECT effective_at, prompt_per_mtok_usd, completion_per_mtok_usd,
                        cached_per_mtok_usd, source
                 FROM provider_rate_cards
                 WHERE provider_id = ?1 AND model = ?2 AND effective_at <= ?3
                 ORDER BY effective_at DESC
                 LIMIT 1",
                params!(provider_id, model, call_ts),
            )
            .await
            .context("rate-card lookup failed")?;
        let Some(row) = rows.next().await.context("rate-card row fetch")? else {
            return Ok(None);
        };
        let effective_at: i64 = row.get::<i64>(0)?;
        let prompt: String = row.get::<String>(1)?;
        let completion: String = row.get::<String>(2)?;
        let cached: Option<String> = row.get::<Option<String>>(3)?;
        let source: String = row.get::<String>(4)?;
        Ok(Some(RateRow {
            effective_at,
            prompt_per_mtok: Usd::parse(&prompt)?,
            completion_per_mtok: Usd::parse(&completion)?,
            cached_per_mtok: cached.as_deref().map(Usd::parse).transpose()?,
            source,
        }))
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
    pub async fn append(
        db: &Database,
        provider_id: &str,
        model: &str,
        effective_at: i64,
        prompt_per_mtok: Usd,
        completion_per_mtok: Usd,
        cached_per_mtok: Option<Usd>,
        source: &str,
    ) -> Result<()> {
        db.conn()
            .execute(
                "INSERT INTO provider_rate_cards
                 (provider_id, model, effective_at, prompt_per_mtok_usd,
                  completion_per_mtok_usd, cached_per_mtok_usd, source)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params!(
                    provider_id,
                    model,
                    effective_at,
                    prompt_per_mtok.to_storage_string(),
                    completion_per_mtok.to_storage_string(),
                    cached_per_mtok.map(Usd::to_storage_string),
                    source,
                ),
            )
            .await
            .context("failed to insert rate-card row")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The aggregator's own id resolves to the vendor's bundled card, so the
    /// dollar cap counts calls made through it.
    #[tokio::test]
    async fn an_aggregator_model_prices_at_its_vendor_card() {
        let db = Database::test_db().await;
        let now = chrono::Utc::now().timestamp();
        let direct = RateCard::lookup(&db, "deepseek", "deepseek-v4-flash", now)
            .await
            .unwrap()
            .expect("bundled card");
        let routed = RateCard::lookup(&db, "openrouter", "deepseek/deepseek-v4-flash", now)
            .await
            .unwrap()
            .expect("vendor card through the aggregator");
        assert_eq!(routed, direct);

        // Dotted Claude ids fold to the bundled spelling.
        assert!(
            RateCard::lookup(&db, "openrouter", "anthropic/claude-opus-5.5", now)
                .await
                .unwrap()
                .is_some()
        );
        // An unknown vendor still has no price.
        assert!(RateCard::lookup(&db, "openrouter", "acme/model-x", now)
            .await
            .unwrap()
            .is_none());
    }
}
