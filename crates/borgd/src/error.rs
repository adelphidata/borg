//! Typed error surface for the `borgd` daemon crate's public API.
//!
//! Internal modules continue to use `anyhow` for ergonomic context chaining.
//! This enum lets cross-crate callers — and the daemon's binary `main` — match
//! on subsystem-level failures (TLS handshake, auth/verifier, subsystem spawn,
//! scheduler dispatch, pairing) instead of scraping anyhow strings.

use thiserror::Error;

/// Top-level error type for the `borgd` crate.
#[derive(Debug, Error)]
pub enum Error {
    /// TLS / rustls / certificate material failure.
    #[error("tls: {0}")]
    Tls(String),

    /// Peer authentication / verifier / interceptor failure.
    #[error("auth: {0}")]
    Auth(#[from] crate::auth::VerifierError),

    /// Failure spawning a supervised subsystem (gateway, scheduler, etc.).
    #[error("spawn: {0}")]
    Spawn(String),

    /// Scheduler tick / dispatch / task-runner failure.
    #[error("scheduler: {0}")]
    Scheduler(String),

    /// Device pairing flow failure.
    #[error("pairing: {0}")]
    Pairing(String),

    /// Escape hatch for unclassified internal `anyhow` errors. Conversions
    /// implemented below let internal `?` propagation work unchanged.
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// Convenient `Result` alias for crate-public functions.
pub type Result<T> = std::result::Result<T, Error>;

impl From<std::io::Error> for Error {
    fn from(err: std::io::Error) -> Self {
        Error::Other(anyhow::Error::from(err))
    }
}

impl From<cortex::Error> for Error {
    fn from(err: cortex::Error) -> Self {
        Error::Other(anyhow::anyhow!(err.to_string()))
    }
}

impl From<gateway::Error> for Error {
    fn from(err: gateway::Error) -> Self {
        Error::Spawn(err.to_string())
    }
}
