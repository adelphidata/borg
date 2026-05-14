//! Resolve `provider_id` strings to a `(category, ProviderRow option)` and
//! enforce air-gap policy.
//!
//! Resolution order:
//!
//! 1. If `id` parses to a built-in [`Provider`] variant, use that variant's
//!    compile-time category.
//! 2. Otherwise, look it up in the `providers` table (BYO endpoint).
//! 3. Otherwise, [`ResolveError::Unknown`].
//!
//! After resolving, if `outbound_allowed = false` and the resolved
//! category is not [`ProviderCategory::Local`], the resolution is rejected
//! with [`ResolveError::AirGapPolicyViolation`].

use anyhow::Result;
use rusqlite::Connection;
use thiserror::Error;

use super::category::ProviderCategory;
use super::row::ProviderRow;
use crate::llm::provider::Provider;

/// Reason a `provider_id` couldn't be resolved.
#[derive(Debug, Error)]
pub enum ResolveError {
    /// `provider_id` matches neither a built-in nor a row in `providers`.
    #[error("unknown provider id '{0}'")]
    Unknown(String),
    /// Resolved successfully but air-gap mode rejects this category.
    #[error("air-gap policy violation: provider '{id}' is {category:?}, only Local allowed")]
    AirGapPolicyViolation {
        /// The provider id we tried to resolve.
        id: String,
        /// The category that was rejected.
        category: ProviderCategory,
    },
    /// SQLite error.
    #[error(transparent)]
    Db(#[from] anyhow::Error),
}

/// One resolved provider — either a built-in or a stored BYO row.
#[derive(Debug, Clone)]
pub struct ResolvedProvider {
    /// Canonical id (lowercased built-in name, or the row's `id`).
    pub id: String,
    /// Category — drives air-gap policy and audit categorization.
    pub category: ProviderCategory,
    /// `Some` for BYO endpoints, `None` for built-ins.
    pub row: Option<ProviderRow>,
    /// `Some` for built-ins, `None` for BYO endpoints (BYO is always
    /// OpenAI-compatible by assumption).
    pub builtin: Option<Provider>,
}

/// Provider resolver.
pub struct ProviderRegistry;

impl ProviderRegistry {
    /// Resolve a `provider_id`, applying air-gap policy.
    pub fn resolve(
        conn: &Connection,
        id: &str,
        outbound_allowed: bool,
    ) -> Result<ResolvedProvider, ResolveError> {
        let resolved = if let Some(builtin) = builtin_from_id(id) {
            ResolvedProvider {
                id: id.to_string(),
                category: builtin.into(),
                row: None,
                builtin: Some(builtin),
            }
        } else {
            let row = ProviderRow::load(conn, id)
                .map_err(ResolveError::Db)?
                .ok_or_else(|| ResolveError::Unknown(id.to_string()))?;
            ResolvedProvider {
                id: row.id.clone(),
                category: row.category,
                row: Some(row),
                builtin: None,
            }
        };
        if !outbound_allowed && !resolved.category.is_air_gap_safe() {
            return Err(ResolveError::AirGapPolicyViolation {
                id: resolved.id,
                category: resolved.category,
            });
        }
        Ok(resolved)
    }

    /// Air-gap check for an already-known built-in [`Provider`] variant.
    ///
    /// Used by call sites that already hold a `Provider` (e.g.
    /// `LlmClient::new`) and don't need DB lookup. Mirrors the policy in
    /// [`Self::resolve`] so both paths stay in sync.
    pub fn check_builtin_air_gap(
        provider: Provider,
        outbound_allowed: bool,
    ) -> Result<(), ResolveError> {
        let category: ProviderCategory = provider.into();
        if !outbound_allowed && !category.is_air_gap_safe() {
            return Err(ResolveError::AirGapPolicyViolation {
                id: provider.to_string(),
                category,
            });
        }
        Ok(())
    }
}

/// Map a string id to a built-in [`Provider`] variant. Case-insensitive.
fn builtin_from_id(id: &str) -> Option<Provider> {
    match id.to_ascii_lowercase().as_str() {
        "openai" => Some(Provider::OpenAi),
        "anthropic" => Some(Provider::Anthropic),
        "bedrock" => Some(Provider::Bedrock),
        "gemini" => Some(Provider::Gemini),
        "deepseek" => Some(Provider::DeepSeek),
        "groq" => Some(Provider::Groq),
        "xai" | "x-ai" => Some(Provider::XAi),
        "azure-openai" | "azure_openai" | "azureopenai" => Some(Provider::AzureOpenAi),
        "vertex-ai" | "vertex_ai" | "vertexai" | "vertex" => Some(Provider::VertexAi),
        "openrouter" => Some(Provider::OpenRouter),
        "ollama" => Some(Provider::Ollama),
        "claude_cli" | "claude-cli" => Some(Provider::ClaudeCli),
        _ => None,
    }
}
