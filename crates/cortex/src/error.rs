//! Typed error surface for the cortex crate's public API.
//!
//! Internal modules continue to use `anyhow` for ergonomic context chaining.
//! This enum is the conversion target at crate boundaries so cross-crate
//! callers can match on discriminated subsystem failures (DB, config, memory,
//! tool dispatch, LLM, hook, session) without scraping error strings.
//!
//! The `Other(anyhow::Error)` variant is the escape hatch for unmigrated
//! `anyhow::Result` returning code paths.

use thiserror::Error;

/// Top-level error type for the cortex crate.
#[derive(Debug, Error)]
pub enum Error {
    /// Database / turso / migration failure.
    #[error("db: {0}")]
    Db(String),

    /// Configuration load / parse / validation failure.
    #[error("config: {0}")]
    Config(String),

    /// Memory subsystem failure (load, search, write, embedding).
    #[error("memory: {0}")]
    Memory(String),

    /// Tool dispatch / execution / argument parsing failure.
    #[error("tool: {0}")]
    Tool(String),

    /// LLM provider transport / response failure.
    #[error("llm: {0}")]
    Llm(#[from] crate::llm::llm_error::LlmError),

    /// Lifecycle hook execution failure.
    #[error("hook: {0}")]
    Hook(String),

    /// Session lifecycle / resolution failure.
    #[error("session: {0}")]
    Session(String),

    /// Escape hatch for unclassified internal `anyhow` errors. Conversions
    /// implemented below let internal `?` propagation work unchanged.
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// Convenient `Result` alias for crate-public functions.
pub type Result<T> = std::result::Result<T, Error>;

impl From<turso::Error> for Error {
    fn from(err: turso::Error) -> Self {
        Error::Db(err.to_string())
    }
}

impl From<std::io::Error> for Error {
    fn from(err: std::io::Error) -> Self {
        Error::Other(anyhow::Error::from(err))
    }
}

impl From<crate::providers::registry::ResolveError> for Error {
    fn from(err: crate::providers::registry::ResolveError) -> Self {
        Error::Config(err.to_string())
    }
}
