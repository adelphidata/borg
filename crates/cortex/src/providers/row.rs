//! `providers` table row model + DB helpers.
//!
//! Rows here represent user-added BYOEndpoint configurations. Built-in
//! providers (`OpenAI`, `Anthropic`, …) live as compile-time `Provider`
//! enum variants and don't have a row.

use anyhow::{Context, Result};
use libsql::params;

use super::category::ProviderCategory;
use crate::db::Database;

/// One row from `providers`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderRow {
    /// Stable identifier (matches `Provider::id()` and rate-card
    /// `provider_id`). Lowercase, no whitespace.
    pub id: String,
    /// Air-gap discriminator.
    pub category: ProviderCategory,
    /// Human-readable label for `borg providers list`.
    pub display_name: String,
    /// Base URL. For OpenAI-compatible BYO this is the full chat-completions
    /// endpoint; for non-BYO it's informational only (the canonical URL is
    /// hardcoded in [`crate::llm::provider::Provider::base_url`]).
    pub endpoint_url: String,
    /// Environment variable holding the API key (e.g. `MY_BYO_KEY`).
    /// `None` for keyless local endpoints.
    pub api_key_env: Option<String>,
    /// Soft-delete toggle.
    pub enabled: bool,
}

impl ProviderRow {
    /// Insert a new row. Returns an error if `id` already exists.
    pub async fn insert(&self, db: &Database) -> Result<()> {
        let now = chrono::Utc::now().timestamp();
        db.conn()
            .execute(
                "INSERT INTO providers
             (id, category, display_name, endpoint_url, api_key_env, enabled,
              created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)",
                params!(
                    self.id.clone(),
                    self.category.as_db_str().to_string(),
                    self.display_name.clone(),
                    self.endpoint_url.clone(),
                    self.api_key_env.clone(),
                    if self.enabled { 1_i64 } else { 0_i64 },
                    now,
                ),
            )
            .await
            .with_context(|| format!("failed to insert provider row '{}'", self.id))?;
        Ok(())
    }

    /// Look up a row by id. `None` if not present (caller must fall back to
    /// compile-time built-ins).
    pub async fn load(db: &Database, id: &str) -> Result<Option<Self>> {
        let mut rows = db
            .conn()
            .query(
                "SELECT id, category, display_name, endpoint_url, api_key_env, enabled
             FROM providers WHERE id = ?1",
                params!(id),
            )
            .await
            .with_context(|| format!("failed to load provider row '{id}'"))?;
        match rows.next().await? {
            Some(row) => Ok(Some(Self::from_row(&row)?)),
            None => Ok(None),
        }
    }

    /// Hard-delete the row. Returns `true` when a row was deleted, `false`
    /// when the id was not present (idempotent — `borg providers remove`
    /// against an already-removed id is a no-op rather than an error).
    pub async fn delete(db: &Database, id: &str) -> Result<bool> {
        let n = db
            .conn()
            .execute("DELETE FROM providers WHERE id = ?1", params!(id))
            .await
            .with_context(|| format!("failed to delete provider row '{id}'"))?;
        Ok(n > 0)
    }

    /// All enabled rows, ordered by id.
    pub async fn list(db: &Database) -> Result<Vec<Self>> {
        let mut rows = db
            .conn()
            .query(
                "SELECT id, category, display_name, endpoint_url, api_key_env, enabled
                 FROM providers WHERE enabled = 1 ORDER BY id",
                (),
            )
            .await
            .context("failed to prepare provider list")?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push(Self::from_row(&row).context("failed to read provider rows")?);
        }
        Ok(out)
    }

    fn from_row(row: &libsql::Row) -> Result<Self> {
        let id: String = row.get::<String>(0)?;
        let category_str: String = row.get::<String>(1)?;
        let category = ProviderCategory::from_db_str(&category_str)
            .ok_or_else(|| anyhow::anyhow!("unknown category '{category_str}'"))?;
        Ok(ProviderRow {
            id,
            category,
            display_name: row.get::<String>(2)?,
            endpoint_url: row.get::<String>(3)?,
            api_key_env: row.get::<Option<String>>(4)?,
            enabled: row.get::<i64>(5)? != 0,
        })
    }
}
