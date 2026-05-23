//! Transport abstraction.
//!
//! A transport is a duplex stream of JSON-RPC messages. Phase 1 ships three
//! concrete implementations:
//!
//! - [`stdio`] — line-delimited JSON over a child process's stdin/stdout.
//! - [`http`] — streamable HTTP (one HTTP request per call).
//! - [`sse`] — server-sent events flavour of MCP HTTP.
//!
//! The [`Transport`] trait is intentionally minimal: send a `serde_json::Value`,
//! receive the next inbound `Message`. The session actor on top handles
//! request/response correlation, timeouts, and reconnection.

use crate::errors::Result;
use crate::protocol::Message;
use async_trait::async_trait;
use serde_json::Value;

pub mod http;
pub mod sse;
pub mod stdio;

/// One end of a duplex JSON-RPC stream.
///
/// `send` is allowed to block on backpressure; `recv` waits for the next
/// inbound message. Both methods return [`crate::errors::McpError::TransportClosed`]
/// once the peer has gone away — the session actor uses that signal to
/// trigger reconnect with backoff.
#[async_trait]
pub trait Transport: Send {
    /// Send one outbound JSON-RPC message.
    async fn send(&mut self, msg: Value) -> Result<()>;

    /// Wait for the next inbound message.
    ///
    /// Returns `Ok(None)` if the peer has cleanly closed — the supervisor
    /// distinguishes this from `Err(TransportClosed)` (unexpected drop) for
    /// the reconnect decision.
    async fn recv(&mut self) -> Result<Option<Message>>;

    /// Best-effort graceful shutdown.
    async fn close(&mut self) -> Result<()>;
}

/// Allowlist of env vars that may be inherited by stdio MCP subprocesses.
///
/// **Security-critical.** Borg's own credentials (`*_API_KEY`,
/// `BORG_KEK_PASSPHRASE`, OAuth tokens, anything else picked up from the
/// operator's shell) are *not* in this list and are stripped before spawn.
/// Servers that need credentials receive them via the explicit `env` block
/// in their config, which is resolved through `SecretRef` upstream.
///
/// Deliberately conservative — anything not listed here gets stripped.
pub const ENV_ALLOWLIST: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "TMPDIR",
    "TMP",
    "TEMP",
    "LANG",
    "LANGUAGE",
    "LC_ALL",
    "LC_CTYPE",
    "LC_MESSAGES",
    "LC_NUMERIC",
    "LC_TIME",
    "TZ",
    "TERM",
    "COLORTERM",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_CACHE_HOME",
    "XDG_RUNTIME_DIR",
];

/// True iff `key` is in the inherit allowlist.
pub fn is_env_allowed(key: &str) -> bool {
    ENV_ALLOWLIST.contains(&key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowlist_admits_locale_and_path_but_not_credentials() {
        // Spec: borg's outbound credentials never leak to MCP subprocesses.
        // Fails if anyone broadens the allowlist to include API key envs.
        assert!(is_env_allowed("PATH"));
        assert!(is_env_allowed("LANG"));
        assert!(is_env_allowed("LC_ALL"));
        assert!(!is_env_allowed("ANTHROPIC_API_KEY"));
        assert!(!is_env_allowed("OPENAI_API_KEY"));
        assert!(!is_env_allowed("BORG_KEK_PASSPHRASE"));
        assert!(!is_env_allowed("AWS_SECRET_ACCESS_KEY"));
        assert!(!is_env_allowed("GITHUB_TOKEN"));
    }
}
