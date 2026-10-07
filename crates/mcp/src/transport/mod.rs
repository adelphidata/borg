//! Transport abstraction.
//!
//! A transport is a duplex stream of JSON-RPC messages. Three concrete
//! implementations ship today:
//!
//! - [`stdio`] — line-delimited JSON over a child process's stdin/stdout.
//! - [`http`] — streamable HTTP (one HTTP request per call).
//! - [`sse`] — server-sent events flavour of MCP HTTP.
//!
//! The [`Transport`] trait is intentionally minimal: send a `serde_json::Value`,
//! receive the next inbound `Message`. The session actor on top handles
//! request/response correlation, timeouts, and reconnection.

use crate::errors::Result;
use crate::manifest::{ServerManifest, Transport as TransportSpec};
use crate::protocol::Message;
use async_trait::async_trait;
use serde_json::Value;

pub mod http;
pub mod sse;
mod sse_framing;
pub mod stdio;
pub mod url_guard;

/// Open the transport described by `manifest` under `policy`.
///
/// This is the one place that knows how to translate a `ServerManifest` into
/// a live `Box<dyn Transport>`. Borgd's session loop calls it on initial
/// connect and on reconnect, and operator tooling (`borg mcp test`) calls it
/// for a one-shot probe. Keeping the construction here means a future
/// transport addition lights up both code paths automatically.
///
/// Stderr from stdio children is forwarded to the `tracing` `debug` stream
/// rather than being routed to the LLM (the latter would be a credential
/// exfiltration vector — server stack traces routinely echo tokens).
pub async fn open(
    manifest: &ServerManifest,
    policy: http::HttpPolicy,
) -> Result<Box<dyn Transport>> {
    // `*_bytes: 0` in every arm means "use the transport's own default". The
    // cortex dispatch path threads the operator-tuned ceiling instead; this
    // generic builder deliberately keeps the safe default.
    match &manifest.transport {
        TransportSpec::Stdio { command, args, env } => {
            let cfg = stdio::StdioConfig {
                server_name: manifest.name.clone(),
                command: command.clone(),
                args: args.clone(),
                env: env.clone(),
                max_line_bytes: 0,
            };
            let t = stdio::spawn(cfg, stdio::debug_log_sink()).await?;
            Ok(Box::new(t))
        }
        TransportSpec::Http {
            url,
            headers,
            auth,
            client_identity,
            ca_cert_path,
        } => {
            let mut headers = headers.clone();
            auth.apply_to(&mut headers);
            let cfg = http::HttpConfig {
                server_name: manifest.name.clone(),
                url: url.clone(),
                headers,
                timeout: manifest.call_timeout,
                policy,
                client_identity: client_identity.clone(),
                ca_cert_path: ca_cert_path.clone(),
                max_response_bytes: 0,
            };
            let t = http::open(cfg).await?;
            Ok(Box::new(t))
        }
        TransportSpec::Sse {
            url,
            headers,
            auth,
            client_identity,
            ca_cert_path,
        } => {
            let mut headers = headers.clone();
            auth.apply_to(&mut headers);
            let cfg = sse::SseConfig {
                server_name: manifest.name.clone(),
                url: url.clone(),
                headers,
                timeout: manifest.call_timeout,
                policy,
                client_identity: client_identity.clone(),
                ca_cert_path: ca_cert_path.clone(),
                max_response_bytes: 0,
            };
            let t = sse::open(cfg, manifest.connect_timeout).await?;
            Ok(Box::new(t))
        }
    }
}

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

    /// Record the protocol version agreed during `initialize`.
    ///
    /// Transports that carry the version out-of-band (streamable HTTP puts it
    /// in a header on every later request) override this. Stdio and anything
    /// else that only ever speaks the handshake body ignores it, hence the
    /// default no-op.
    fn set_protocol_version(&mut self, _version: &str) {}
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
    // Custom CA trust stores. A CA bundle names *which* certificates to trust;
    // it is not a credential, and withholding it does not protect anything —
    // it just makes every outbound TLS call from a server behind a corporate
    // MITM proxy fail with an unverifiable-certificate error the operator
    // cannot act on. Values are paths, absolutized by
    // [`absolutize_ca_path`] before they reach the child.
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "NODE_EXTRA_CA_CERTS",
    "REQUESTS_CA_BUNDLE",
];

/// Extra variables a Windows MCP child inherits on top of [`ENV_ALLOWLIST`].
///
/// A Windows process without `SYSTEMROOT` cannot load core DLLs, and node and
/// python fail at start without the profile and program roots. None of these
/// carry a credential. Only applied on Windows: a Unix child has no use for
/// them, so forwarding them there would only widen the inherited surface.
pub const WINDOWS_ENV_ALLOWLIST: &[&str] = &[
    "SYSTEMROOT",
    "WINDIR",
    "SYSTEMDRIVE",
    "COMSPEC",
    "PATHEXT",
    "USERPROFILE",
    "APPDATA",
    "LOCALAPPDATA",
    "PROGRAMDATA",
    "PROGRAMFILES",
    "TEMP",
    "TMP",
];

/// Allowlisted variables whose value is a filesystem path.
///
/// Borg does not set the child's working directory to its own, so a relative
/// path inherited from the operator's shell would resolve against whatever
/// directory the server happens to start in — silently pointing at the wrong
/// file, or none.
pub const CA_PATH_ENV_VARS: &[&str] = &[
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "NODE_EXTRA_CA_CERTS",
    "REQUESTS_CA_BUNDLE",
];

/// True iff `key` is in the inherit allowlist for the current platform.
pub fn is_env_allowed(key: &str) -> bool {
    env_allowed_for(key, cfg!(windows))
}

/// Allowlist check with the platform rules made explicit, so both rule sets
/// are testable on every host.
///
/// Windows env names are case-insensitive and `PATH` is usually spelled
/// `Path` there, so Windows rules match without regard to ASCII case and add
/// [`WINDOWS_ENV_ALLOWLIST`]. Unix names are case-sensitive: `path` is a
/// different variable from `PATH` and stays stripped.
fn env_allowed_for(key: &str, windows: bool) -> bool {
    name_in(ENV_ALLOWLIST, key, windows) || (windows && name_in(WINDOWS_ENV_ALLOWLIST, key, true))
}

/// True iff `key` is in `list`, matched without ASCII case when
/// `ignore_case` is set.
fn name_in(list: &[&str], key: &str, ignore_case: bool) -> bool {
    if ignore_case {
        list.iter().any(|name| name.eq_ignore_ascii_case(key))
    } else {
        list.contains(&key)
    }
}

/// Resolve a CA trust-store variable to an absolute path.
///
/// Returns the value unchanged for any other variable, for an already-absolute
/// path, and for an empty value — an empty `SSL_CERT_FILE` is how some tools
/// spell "use the built-in store", and turning it into the current directory
/// would change that meaning.
pub fn absolutize_ca_path(key: &str, value: &str) -> String {
    if !name_in(CA_PATH_ENV_VARS, key, cfg!(windows)) || value.is_empty() {
        return value.to_string();
    }
    let path = std::path::Path::new(value);
    if path.is_absolute() {
        return value.to_string();
    }
    match std::env::current_dir() {
        Ok(cwd) => cwd.join(path).to_string_lossy().into_owned(),
        Err(e) => {
            tracing::warn!(
                key,
                error = %e,
                "cannot resolve a relative CA path; passing it through unchanged"
            );
            value.to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stderr_tail_keeps_only_the_newest_lines_and_cuts_long_ones() {
        // Spec: the tail is a bounded ring on both axes. A child that floods
        // stderr must not grow memory, and the operator sees the *last*
        // lines — the ones that explain why the server died.
        let tail = stdio::StderrTail::with_limits(3, 8);
        for i in 0..25 {
            tail.push(&format!("line {i}"));
        }
        assert_eq!(tail.lines(), vec!["line 22", "line 23", "line 24"]);

        tail.push("0123456789 a very long diagnostic");
        let last = tail.lines().pop().expect("just pushed");
        assert_eq!(last, "01234567…", "lines are cut at the character cap");
        assert_eq!(tail.lines().len(), 3, "a push never grows past the ring");
    }

    #[test]
    fn stderr_tail_cuts_on_a_character_boundary() {
        let tail = stdio::StderrTail::with_limits(1, 2);
        tail.push("éèê");
        assert_eq!(tail.lines(), vec!["éè…"]);
    }

    #[test]
    fn stderr_tail_sink_records_and_a_zero_limit_still_keeps_one_line() {
        let tail = std::sync::Arc::new(stdio::StderrTail::with_limits(0, 0));
        assert!(tail.is_empty());
        let sink = tail.sink();
        sink("srv", "first");
        sink("srv", "second");
        assert_eq!(tail.lines(), vec!["s…"]);
    }

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

    /// Spec: on Windows the child needs `Path` (its usual spelling there) and
    /// `SystemRoot` to start at all, and env names are case-insensitive. The
    /// Windows rules still never admit a credential, in any case.
    #[test]
    fn windows_rules_admit_essentials_in_any_case_but_not_credentials() {
        for key in [
            "Path",
            "PATH",
            "SystemRoot",
            "windir",
            "ComSpec",
            "PATHEXT",
            "UserProfile",
            "APPDATA",
            "LocalAppData",
            "ProgramData",
            "ProgramFiles",
            "Temp",
        ] {
            assert!(
                env_allowed_for(key, true),
                "{key} must reach a Windows child"
            );
        }
        for key in [
            "AWS_SECRET_ACCESS_KEY",
            "aws_secret_access_key",
            "Anthropic_Api_Key",
            "BORG_KEK_PASSPHRASE",
            "GITHUB_TOKEN",
        ] {
            assert!(
                !env_allowed_for(key, true),
                "{key} must be stripped on Windows"
            );
        }
    }

    /// Spec: Unix env names are case-sensitive and the Windows-only
    /// essentials stay out of a Unix child.
    #[test]
    fn unix_rules_are_case_sensitive_and_skip_windows_essentials() {
        assert!(env_allowed_for("PATH", false));
        assert!(!env_allowed_for("Path", false));
        assert!(!env_allowed_for("SYSTEMROOT", false));
        assert!(!env_allowed_for("USERPROFILE", false));
        assert!(!env_allowed_for("AWS_SECRET_ACCESS_KEY", false));
    }

    /// Spec: a CA bundle names which certificates to trust. It carries no
    /// secret, and withholding it only breaks TLS for servers behind a
    /// corporate proxy. Pinned as a deliberate carve-out so the credential
    /// assertions above and these cannot be confused for one another.
    #[test]
    fn allowlist_admits_ca_trust_stores() {
        for key in [
            "SSL_CERT_FILE",
            "SSL_CERT_DIR",
            "NODE_EXTRA_CA_CERTS",
            "REQUESTS_CA_BUNDLE",
        ] {
            assert!(is_env_allowed(key), "{key} must reach MCP children");
        }
    }

    /// Spec: borg does not set the child's cwd to its own, so a relative CA
    /// path inherited from the operator's shell must be resolved before it is
    /// handed over or it points somewhere else entirely.
    /// Spec: the two CA lists must not drift. `ENV_ALLOWLIST` decides what the
    /// child inherits and `CA_PATH_ENV_VARS` decides what gets absolutized; a
    /// name in the second but not the first is dead, and a CA name added only
    /// to the first is inherited as a relative path that resolves against the
    /// server's own directory.
    #[test]
    fn every_ca_path_variable_is_also_inheritable() {
        for key in CA_PATH_ENV_VARS {
            assert!(
                is_env_allowed(key),
                "{key} is absolutized but never inherited"
            );
        }
    }

    #[test]
    fn a_relative_ca_path_is_made_absolute() {
        let resolved = absolutize_ca_path("SSL_CERT_FILE", "certs/ca.pem");
        assert!(
            std::path::Path::new(&resolved).is_absolute(),
            "must be absolute: {resolved}"
        );
        assert!(resolved.ends_with("certs/ca.pem"));
    }

    #[test]
    fn absolutize_leaves_everything_else_alone() {
        // Already absolute.
        assert_eq!(
            absolutize_ca_path("SSL_CERT_FILE", "/etc/ssl/ca.pem"),
            "/etc/ssl/ca.pem"
        );
        // Empty means "use the built-in store"; joining it to cwd would
        // silently redirect that to a directory.
        assert_eq!(absolutize_ca_path("SSL_CERT_FILE", ""), "");
        // Not a path variable: PATH is a list and must never be rewritten.
        assert_eq!(absolutize_ca_path("PATH", "bin:sbin"), "bin:sbin");
    }
}
