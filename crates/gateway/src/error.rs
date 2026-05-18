//! Typed error surface for the gateway crate's public API.
//!
//! Internal modules continue to use `anyhow` for ergonomic context chaining.
//! This enum is the conversion target at crate boundaries (`GatewayServer`,
//! channel send/verify entry points) so cross-crate callers can match on
//! discriminated failure modes for retry / circuit-break policy without
//! string scraping.

use thiserror::Error;

/// Top-level error type for the gateway crate.
#[derive(Debug, Error)]
pub enum Error {
    /// Inbound webhook signature / HMAC / token verification failed.
    #[error("verify failed: {0}")]
    Verify(String),

    /// Outbound message delivery to a channel failed (network, API, auth).
    #[error("send failed: {0}")]
    Send(String),

    /// Inbound payload could not be decoded into a recognized message shape.
    #[error("decode failed: {0}")]
    Decode(String),

    /// Channel exists in the registry but is disabled by configuration.
    #[error("channel disabled: {0}")]
    ChannelDisabled(String),

    /// Escape hatch for internal `anyhow` errors that have not yet been
    /// classified into a typed variant. Conversions implemented below allow
    /// internal modules to keep using `anyhow` and `?` at the boundary.
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

impl From<libsql::Error> for Error {
    fn from(err: libsql::Error) -> Self {
        Error::Other(anyhow::anyhow!("db: {err}"))
    }
}
