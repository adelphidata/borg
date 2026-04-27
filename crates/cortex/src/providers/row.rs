//! `providers` table row model + DB helpers.
//!
//! Rows here represent user-added BYOEndpoint configurations. Built-in
//! providers (`OpenAI`, `Anthropic`, …) live as compile-time `Provider`
//! enum variants and don't have a row.

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};

use super::category::ProviderCategory;

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
    /// hardcoded in [`crate::provider::Provider::base_url`]).
    pub endpoint_url: String,
    /// Environment variable holding the API key (e.g. `MY_BYO_KEY`).
    /// `None` for keyless local endpoints.
    pub api_key_env: Option<String>,
    /// Soft-delete toggle.
    pub enabled: bool,
}

impl ProviderRow {
    /// Insert a new row. Returns an error if `id` already exists.
    pub fn insert(&self, conn: &Connection) -> Result<()> {
        let now = chrono::Utc::now().timestamp();
        conn.execute(
            "INSERT INTO providers
             (id, category, display_name, endpoint_url, api_key_env, enabled,
              created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)",
            params![
                self.id,
                self.category.as_db_str(),
                self.display_name,
                self.endpoint_url,
                self.api_key_env,
                if self.enabled { 1 } else { 0 },
                now,
            ],
        )
        .with_context(|| format!("failed to insert provider row '{}'", self.id))?;
        Ok(())
    }

    /// Look up a row by id. `None` if not present (caller must fall back to
    /// compile-time built-ins).
    pub fn load(conn: &Connection, id: &str) -> Result<Option<Self>> {
        conn.query_row(
            "SELECT id, category, display_name, endpoint_url, api_key_env, enabled
             FROM providers WHERE id = ?1",
            params![id],
            Self::from_row,
        )
        .optional()
        .with_context(|| format!("failed to load provider row '{id}'"))
    }

    /// All enabled rows, ordered by id.
    pub fn list(conn: &Connection) -> Result<Vec<Self>> {
        let mut stmt = conn
            .prepare(
                "SELECT id, category, display_name, endpoint_url, api_key_env, enabled
                 FROM providers WHERE enabled = 1 ORDER BY id",
            )
            .context("failed to prepare provider list")?;
        let rows = stmt
            .query_map([], Self::from_row)?
            .collect::<Result<Vec<_>, _>>()
            .context("failed to read provider rows")?;
        Ok(rows)
    }

    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        let id: String = row.get(0)?;
        let category_str: String = row.get(1)?;
        let category = ProviderCategory::from_db_str(&category_str).ok_or_else(|| {
            rusqlite::Error::FromSqlConversionFailure(
                1,
                rusqlite::types::Type::Text,
                anyhow::anyhow!("unknown category '{category_str}'").into(),
            )
        })?;
        Ok(ProviderRow {
            id,
            category,
            display_name: row.get(2)?,
            endpoint_url: row.get(3)?,
            api_key_env: row.get(4)?,
            enabled: row.get::<_, i64>(5)? != 0,
        })
    }
}
