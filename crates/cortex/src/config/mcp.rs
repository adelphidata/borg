//! MCP (Model Context Protocol) client config.
//!
//! Cortex-side operator surface. The wire-level types in `mcp::manifest`
//! (`ServerManifest`, `Transport`, `HttpAuth`, `ToolFilter`) consume already-
//! resolved plaintext values; this module owns the path from a SQLite
//! settings row (with `${SECRET:NAME}` placeholders) to an
//! `mcp::manifest::ServerManifest`.

use std::collections::HashMap;
use std::time::Duration;

use anyhow::{bail, Result};
use regex::Regex;
use serde::{Deserialize, Serialize};

use super::Config;

/// Top-level MCP config. Defaults are opt-in: `enabled = false` keeps borg
/// exactly in its pre-MCP behaviour.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpConfig {
    /// Global kill switch. When `false`, no sessions are spawned and
    /// `mcp_clients::tool_definitions()` returns an empty vec.
    #[serde(default)]
    pub enabled: bool,
    /// Global cap on the total number of MCP tools across all servers.
    /// Mirrors the registry-side `DEFAULT_GLOBAL_TOOL_CAP`.
    #[serde(default = "default_max_tools_total")]
    pub max_tools_total: usize,
    /// Allow `http://` URLs (default: `false`, https-only).
    #[serde(default)]
    pub allow_plaintext_http: bool,
    /// Allow URLs that resolve to loopback / RFC1918 / link-local.
    /// Default: `false`.
    #[serde(default)]
    pub allow_local_network: bool,
    /// Server names to hide even when fully configured under `servers`.
    /// Operator escape hatch; equivalent to flipping `enabled = false`
    /// on each entry.
    #[serde(default)]
    pub disabled_servers: Vec<String>,
    /// Per-server config, keyed by operator-chosen server name.
    #[serde(default)]
    pub servers: HashMap<String, McpServerConfig>,
    /// Per-`tools/call` response size cap (bytes). Output beyond this is
    /// truncated before the redaction + trust-wrap pipeline and an
    /// `mcp_response_truncated` audit row is written. A hostile or buggy
    /// server cannot dump an unbounded blob into the LLM context this way.
    /// Default: 1 MiB — matches the WASM plugin sandbox's `max_output_bytes`
    /// ceiling so the two untrusted-code paths share one budget.
    #[serde(default = "default_max_response_bytes")]
    pub max_response_bytes: usize,
    /// Max retries for a *transient* streamable-HTTP startup failure (a `5xx`/
    /// `429` during `initialize`, or the first read-only `tools/list`). Each
    /// retry recreates the transport. `0` disables startup retry. Only the
    /// idempotent startup path is retried — a mutating `tools/call` is never
    /// auto-replayed. Default: 2.
    #[serde(default = "default_startup_max_retries")]
    pub startup_max_retries: u32,
    /// Base backoff in milliseconds between streamable-HTTP startup retries;
    /// doubles each attempt. Default: 250.
    #[serde(default = "default_startup_retry_base_ms")]
    pub startup_retry_base_ms: u64,
    /// Defer MCP tool schemas out of the per-turn LLM tool list and expose
    /// them through the `search_tools` meta-tool instead. The model searches
    /// registered MCP tools by keyword and receives matching specs on demand,
    /// then calls them directly the next turn (dispatch is registry-keyed, not
    /// advertisement-keyed, so a discovered tool is immediately callable).
    ///
    /// Off by default — existing behaviour (every MCP schema sent every turn)
    /// is unchanged. Turn on to reclaim context when many MCP tools are
    /// registered. Native/compiled-in tools are never deferred.
    #[serde(default)]
    pub tool_search: bool,
}

impl Default for McpConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_tools_total: default_max_tools_total(),
            allow_plaintext_http: false,
            allow_local_network: false,
            disabled_servers: Vec::new(),
            servers: HashMap::new(),
            max_response_bytes: default_max_response_bytes(),
            startup_max_retries: default_startup_max_retries(),
            startup_retry_base_ms: default_startup_retry_base_ms(),
            tool_search: false,
        }
    }
}

impl McpConfig {
    /// True when MCP tool-search deferral is in effect. Requires MCP itself to
    /// be enabled — `tool_search` alone with `enabled = false` is a no-op (no
    /// sessions, no tools to defer).
    pub fn tool_search_active(&self) -> bool {
        self.enabled && self.tool_search
    }
}

fn default_max_tools_total() -> usize {
    mcp::registry::DEFAULT_GLOBAL_TOOL_CAP
}

fn default_max_response_bytes() -> usize {
    1024 * 1024
}

fn default_startup_max_retries() -> u32 {
    2
}

fn default_startup_retry_base_ms() -> u64 {
    250
}

/// Per-server config.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerConfig {
    /// If false, this server is skipped at initialize time and contributes
    /// no tools. Default: true.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Transport-specific connection details.
    pub transport: TransportSpec,
    /// Initial handshake timeout. Default: 60_000 ms.
    #[serde(default = "default_connect_ms")]
    pub connect_timeout_ms: u64,
    /// Per-call timeout for `tools/call`. Default: 300_000 ms.
    #[serde(default = "default_call_ms")]
    pub call_timeout_ms: u64,
    /// Steady-state per-minute call rate. Default: 60.
    #[serde(default = "default_rate_per_minute")]
    pub rate_limit_per_minute: u32,
    /// Burst capacity before refusal. Default: 10.
    #[serde(default = "default_rate_burst")]
    pub rate_limit_burst: u32,
    /// Per-server tool include/exclude/read-only/max.
    #[serde(default)]
    pub tools: mcp::manifest::ToolFilter,
}

fn default_true() -> bool {
    true
}
fn default_connect_ms() -> u64 {
    60_000
}
fn default_call_ms() -> u64 {
    300_000
}
fn default_rate_per_minute() -> u32 {
    60
}
fn default_rate_burst() -> u32 {
    10
}

/// Transport-specific knobs. Mirrors `mcp::manifest::Transport` but keeps
/// the cortex side free to evolve placeholder strings without coupling to
/// the wire types.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TransportSpec {
    /// Local subprocess speaking newline-delimited JSON-RPC over stdio.
    Stdio {
        /// Executable to spawn.
        command: String,
        /// CLI args.
        #[serde(default)]
        args: Vec<String>,
        /// Extra env vars (on top of the env allowlist in
        /// `mcp::transport::ENV_ALLOWLIST`). Values may contain
        /// `${SECRET:NAME}` placeholders.
        #[serde(default)]
        env: HashMap<String, String>,
    },
    /// Streamable HTTP MCP endpoint.
    Http {
        /// Endpoint URL.
        url: String,
        /// Headers to send on every request. Values may contain
        /// `${SECRET:NAME}` placeholders.
        #[serde(default)]
        headers: HashMap<String, String>,
        /// Optional structured auth.
        #[serde(default)]
        auth: HttpAuthSpec,
        /// Path to a client certificate PEM for mutual TLS. `~` is expanded.
        /// May be a combined cert+key PEM, in which case `client_key` is unset.
        #[serde(default)]
        client_cert: Option<String>,
        /// Path to the client private key PEM, when separate from `client_cert`.
        #[serde(default)]
        client_key: Option<String>,
    },
    /// Server-Sent Events flavour of MCP HTTP.
    Sse {
        /// SSE endpoint URL.
        url: String,
        /// Headers.
        #[serde(default)]
        headers: HashMap<String, String>,
        /// Optional structured auth.
        #[serde(default)]
        auth: HttpAuthSpec,
        /// Path to a client certificate PEM for mutual TLS (see [`TransportSpec::Http`]).
        #[serde(default)]
        client_cert: Option<String>,
        /// Path to the client private key PEM, when separate from `client_cert`.
        #[serde(default)]
        client_key: Option<String>,
    },
}

/// HTTP auth flavour. Today: `None` (use `headers` directly) or `Bearer`.
/// OAuth 2.1 is deferred until a concrete server requires it.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HttpAuthSpec {
    /// Use `headers` as-is.
    #[default]
    None,
    /// Set `Authorization: Bearer <token>`. `token` may contain a
    /// `${SECRET:NAME}` placeholder.
    Bearer {
        /// Bearer token (with optional `${SECRET:NAME}` placeholders).
        token: String,
    },
}

impl McpServerConfig {
    /// Build the wire-level `mcp::manifest::ServerManifest` for this server.
    ///
    /// Resolves every `${SECRET:NAME}` placeholder in `env`, `headers`, and
    /// `auth.bearer.token` via `Config::resolve_credential_or_env`.
    /// Returns an error (not an empty string) on the first unresolved
    /// placeholder so a missing credential surfaces as a clear failure
    /// rather than a silent empty token.
    pub fn resolve_to_manifest(
        &self,
        name: &str,
        cfg: &Config,
    ) -> Result<mcp::manifest::ServerManifest> {
        let transport = match &self.transport {
            TransportSpec::Stdio { command, args, env } => mcp::manifest::Transport::Stdio {
                command: command.clone(),
                args: args.clone(),
                env: resolve_secret_map(env, cfg, "env")?,
            },
            TransportSpec::Http {
                url,
                headers,
                auth,
                client_cert,
                client_key,
            } => mcp::manifest::Transport::Http {
                url: url.clone(),
                headers: resolve_secret_map(headers, cfg, "headers")?,
                auth: resolve_auth(auth, cfg)?,
                client_identity: resolve_client_identity(client_cert, client_key)?,
            },
            TransportSpec::Sse {
                url,
                headers,
                auth,
                client_cert,
                client_key,
            } => mcp::manifest::Transport::Sse {
                url: url.clone(),
                headers: resolve_secret_map(headers, cfg, "headers")?,
                auth: resolve_auth(auth, cfg)?,
                client_identity: resolve_client_identity(client_cert, client_key)?,
            },
        };
        Ok(mcp::manifest::ServerManifest {
            name: name.to_string(),
            transport,
            connect_timeout: Duration::from_millis(self.connect_timeout_ms),
            call_timeout: Duration::from_millis(self.call_timeout_ms),
            rate_limit_per_minute: self.rate_limit_per_minute,
            rate_limit_burst: self.rate_limit_burst,
            tools: self.tools.clone(),
        })
    }
}

/// Build the wire-level [`mcp::manifest::ClientIdentity`] from the operator's
/// `client_cert` / `client_key` paths. Tilde-expands both paths so `~/certs/...`
/// works; the PEM bytes are not read here — the transport loads them at connect
/// time so a missing/invalid file fails fast against the real connection. A
/// `client_key` without a `client_cert` is a config error (the cert is what's
/// presented; the key alone is meaningless).
fn resolve_client_identity(
    client_cert: &Option<String>,
    client_key: &Option<String>,
) -> Result<Option<mcp::manifest::ClientIdentity>> {
    match (client_cert, client_key) {
        (None, None) => Ok(None),
        (None, Some(_)) => bail!(
            "mcp server config sets `client_key` without `client_cert` — \
             provide the client certificate path (or a combined cert+key PEM)"
        ),
        (Some(cert), key) => Ok(Some(mcp::manifest::ClientIdentity {
            cert_path: crate::util::unix_ext::expand_tilde(cert),
            key_path: key.as_ref().map(|k| crate::util::unix_ext::expand_tilde(k)),
        })),
    }
}

fn resolve_auth(spec: &HttpAuthSpec, cfg: &Config) -> Result<mcp::manifest::HttpAuth> {
    match spec {
        HttpAuthSpec::None => Ok(mcp::manifest::HttpAuth::None),
        HttpAuthSpec::Bearer { token } => Ok(mcp::manifest::HttpAuth::Bearer {
            token: resolve_secret_placeholders(token, cfg, "auth.bearer.token")?,
        }),
    }
}

fn resolve_secret_map(
    map: &HashMap<String, String>,
    cfg: &Config,
    field: &str,
) -> Result<HashMap<String, String>> {
    let mut out = HashMap::with_capacity(map.len());
    for (k, v) in map {
        let resolved = resolve_secret_placeholders(v, cfg, &format!("{field}[{k}]"))?;
        out.insert(k.clone(), resolved);
    }
    Ok(out)
}

/// Compiled lazily — the regex is shared across all calls.
fn secret_re() -> &'static Regex {
    static SECRET_RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    SECRET_RE.get_or_init(|| {
        // `${SECRET:NAME}` where NAME is `[A-Za-z0-9_]+`. Stops at the first
        // closing brace — no nested expansion.
        #[allow(clippy::expect_used)]
        Regex::new(r"\$\{SECRET:([A-Za-z0-9_]+)\}").expect("valid SECRET regex")
    })
}

/// Substitute every `${SECRET:NAME}` in `value` with
/// `cfg.resolve_credential_or_env(NAME)`. Returns an error on the first
/// unresolved placeholder; literal strings (no match) pass through.
pub fn resolve_secret_placeholders(value: &str, cfg: &Config, field: &str) -> Result<String> {
    let re = secret_re();
    let mut out = String::with_capacity(value.len());
    let mut last_end = 0usize;
    for m in re.captures_iter(value) {
        // Capture group 0 is the whole match and group 1 is the `NAME`
        // sub-pattern. The regex guarantees both exist on every match;
        // unwrap is fine on a successful captures iterator entry.
        #[allow(clippy::unwrap_used)]
        let whole = m.get(0).unwrap();
        #[allow(clippy::unwrap_used)]
        let name = m.get(1).unwrap().as_str();
        out.push_str(&value[last_end..whole.start()]);
        match cfg.resolve_credential_or_env(name) {
            Some(v) => out.push_str(&v),
            None => bail!(
                "unresolved secret reference ${{SECRET:{name}}} in {field} — \
                 set the credential via `borg settings set credentials …` or \
                 export {name} in the daemon's environment"
            ),
        }
        last_end = whole.end();
    }
    out.push_str(&value[last_end..]);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CredentialValue;

    /// Mutex serializing env-var mutation across this module's tests. The
    /// credential-store path uses `EnvVar(String)` under the hood, so we
    /// can't fully exercise the resolver without an env var. Without this
    /// lock, `placeholder_supports_multiple_substitutions_in_one_value` and
    /// `placeholder_resolves_from_credentials` race on shared state.
    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap_or_else(|p| p.into_inner())
    }

    /// Set an env var for the duration of the guard. Restores prior value
    /// (or unsets) on drop, including on panic — keeping the test suite
    /// hermetic.
    struct EnvGuard {
        key: String,
        prev: Option<String>,
    }
    impl EnvGuard {
        fn set(key: &str, value: &str) -> Self {
            let prev = std::env::var(key).ok();
            // SAFETY: env mutation guarded by env_lock.
            unsafe { std::env::set_var(key, value) };
            Self {
                key: key.to_string(),
                prev,
            }
        }
    }
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            // SAFETY: env mutation guarded by env_lock.
            unsafe {
                match &self.prev {
                    Some(v) => std::env::set_var(&self.key, v),
                    None => std::env::remove_var(&self.key),
                }
            }
        }
    }

    fn cfg_with(name: &str) -> Config {
        let mut c = Config::default();
        // `EnvVar(name)` points the credential store at an env var whose
        // value the test will set via `EnvGuard`.
        c.credentials
            .insert(name.to_string(), CredentialValue::EnvVar(name.to_string()));
        c
    }

    #[test]
    fn call_timeout_ms_defaults_to_300s_when_omitted() {
        // Spec: a server config that omits `call_timeout_ms` inherits the
        // 300_000 ms default. Long-running MCP tools should not hit a
        // 2-minute ceiling unless an operator opts into a shorter one. This
        // pins the serde default so a regression in `default_call_ms` fails.
        let json = serde_json::json!({
            "transport": { "type": "stdio", "command": "true" }
        });
        let server: McpServerConfig = serde_json::from_value(json).unwrap();
        assert_eq!(server.call_timeout_ms, 300_000);
        // And the resolved wire manifest carries that same wall-clock bound.
        let manifest = server.resolve_to_manifest("t", &Config::default()).unwrap();
        assert_eq!(manifest.call_timeout, Duration::from_millis(300_000));
    }

    #[test]
    fn placeholder_resolves_from_credentials() {
        // The placeholder substitution is the single seam between operator
        // config and a wire-bound HTTP header. If it ever silently let an
        // unresolved `${SECRET:…}` through, every MCP server downstream
        // would receive a literal `${SECRET:GITHUB_TOKEN}` as its
        // Authorization header — that's the failure this test guards
        // against.
        let _lock = env_lock();
        let _g = EnvGuard::set("BORG_TEST_MCP_GHTOKEN", "ghp_real");
        let cfg = cfg_with("BORG_TEST_MCP_GHTOKEN");
        let resolved = resolve_secret_placeholders(
            "Bearer ${SECRET:BORG_TEST_MCP_GHTOKEN}",
            &cfg,
            "headers[Authorization]",
        )
        .unwrap();
        assert_eq!(resolved, "Bearer ghp_real");
    }

    #[test]
    fn placeholder_supports_multiple_substitutions_in_one_value() {
        // Spec: a header value can mix literals with multiple `${SECRET:…}`
        // refs. Two placeholders both expand; this caught an earlier
        // implementation that called `replace` on the first match only.
        let _lock = env_lock();
        let _a = EnvGuard::set("BORG_TEST_MCP_A", "aa");
        let _b = EnvGuard::set("BORG_TEST_MCP_B", "bb");
        let mut cfg = Config::default();
        cfg.credentials.insert(
            "BORG_TEST_MCP_A".to_string(),
            CredentialValue::EnvVar("BORG_TEST_MCP_A".to_string()),
        );
        cfg.credentials.insert(
            "BORG_TEST_MCP_B".to_string(),
            CredentialValue::EnvVar("BORG_TEST_MCP_B".to_string()),
        );
        let resolved = resolve_secret_placeholders(
            "x=${SECRET:BORG_TEST_MCP_A}, y=${SECRET:BORG_TEST_MCP_B}, raw=${SECRET:BORG_TEST_MCP_A}",
            &cfg,
            "env[X]",
        )
        .unwrap();
        assert_eq!(resolved, "x=aa, y=bb, raw=aa");
    }

    #[test]
    fn unresolved_placeholder_errors_with_field_context() {
        // Spec: an unresolved placeholder MUST fail loud, not silently
        // substitute an empty string. The error must name the credential
        // AND the field (env/header/auth) so the operator knows what to
        // fix.
        let _lock = env_lock();
        // Use a credential name no real env var or keychain would hold.
        let key = "BORG_TEST_MCP_DEFINITELY_MISSING_K9X3Q";
        // Defensive: make sure the env is genuinely clear for this name.
        // SAFETY: env mutation guarded by env_lock.
        unsafe { std::env::remove_var(key) };
        let cfg = Config::default();
        let err =
            resolve_secret_placeholders(&format!("${{SECRET:{key}}}"), &cfg, "headers[X-Api-Key]")
                .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains(key), "error must name the credential: {msg}");
        assert!(
            msg.contains("headers[X-Api-Key]"),
            "error must name the field: {msg}",
        );
    }

    #[test]
    fn literal_value_passes_through_unchanged() {
        // Spec: values without `${SECRET:` are not parsed/escaped/mangled.
        // Asserting on both a plain string and one that *contains* a dollar
        // sign so an over-eager regex change can't silently start matching.
        let cfg = Config::default();
        let r = resolve_secret_placeholders("plain text $5.00", &cfg, "env[Y]").unwrap();
        assert_eq!(r, "plain text $5.00");
    }

    #[test]
    fn resolve_to_manifest_carries_timeouts_and_rate() {
        // Spec: the operator-facing millisecond knobs must reach the wire
        // manifest as the matching `Duration` and the rate limits as-is.
        // This is the single point where the cortex/mcp seam translates
        // units; a typo (us vs ms) would silently break per-call timeouts.
        let server = McpServerConfig {
            enabled: true,
            transport: TransportSpec::Stdio {
                command: "/bin/true".to_string(),
                args: vec![],
                env: HashMap::new(),
            },
            connect_timeout_ms: 5_000,
            call_timeout_ms: 30_000,
            rate_limit_per_minute: 120,
            rate_limit_burst: 20,
            tools: mcp::manifest::ToolFilter::default(),
        };
        let m = server
            .resolve_to_manifest("alpha", &Config::default())
            .unwrap();
        assert_eq!(m.name, "alpha");
        assert_eq!(m.connect_timeout, Duration::from_millis(5_000));
        assert_eq!(m.call_timeout, Duration::from_millis(30_000));
        assert_eq!(m.rate_limit_per_minute, 120);
        assert_eq!(m.rate_limit_burst, 20);
    }

    #[test]
    fn http_transport_deserializes_client_cert_and_key() {
        // The mTLS knobs must round-trip through serde with the rest of the
        // HTTP transport. Exercises the `#[serde(default)]` fields.
        let json = serde_json::json!({
            "type": "http",
            "url": "https://mcp.example.com/mcp",
            "client_cert": "/etc/borg/client.crt",
            "client_key": "/etc/borg/client.key",
        });
        let spec: TransportSpec = serde_json::from_value(json).unwrap();
        match spec {
            TransportSpec::Http {
                client_cert,
                client_key,
                ..
            } => {
                assert_eq!(client_cert.as_deref(), Some("/etc/borg/client.crt"));
                assert_eq!(client_key.as_deref(), Some("/etc/borg/client.key"));
            }
            other => panic!("expected Http transport, got {other:?}"),
        }
    }

    #[test]
    fn resolve_to_manifest_carries_client_identity_with_tilde_expansion() {
        // The cortex→mcp seam must thread the cert/key into the wire manifest
        // (a previous version silently dropped them) and expand `~` so a
        // home-relative cert path actually resolves.
        let server = McpServerConfig {
            enabled: true,
            transport: TransportSpec::Http {
                url: "https://mcp.example.com/mcp".to_string(),
                headers: HashMap::new(),
                auth: HttpAuthSpec::None,
                client_cert: Some("~/certs/client.pem".to_string()),
                client_key: Some("~/certs/client.key".to_string()),
            },
            connect_timeout_ms: 60_000,
            call_timeout_ms: 120_000,
            rate_limit_per_minute: 60,
            rate_limit_burst: 10,
            tools: mcp::manifest::ToolFilter::default(),
        };
        let m = server
            .resolve_to_manifest("mtls", &Config::default())
            .unwrap();
        let home = dirs::home_dir().unwrap();
        match m.transport {
            mcp::manifest::Transport::Http {
                client_identity: Some(id),
                ..
            } => {
                assert_eq!(
                    id.cert_path,
                    home.join("certs/client.pem").to_string_lossy()
                );
                assert_eq!(
                    id.key_path.as_deref(),
                    Some(home.join("certs/client.key").to_string_lossy().as_ref())
                );
            }
            other => panic!("expected Http transport with client identity, got {other:?}"),
        }
    }

    #[test]
    fn client_key_without_cert_is_rejected() {
        let server = McpServerConfig {
            enabled: true,
            transport: TransportSpec::Http {
                url: "https://mcp.example.com/mcp".to_string(),
                headers: HashMap::new(),
                auth: HttpAuthSpec::None,
                client_cert: None,
                client_key: Some("/etc/borg/client.key".to_string()),
            },
            connect_timeout_ms: 60_000,
            call_timeout_ms: 120_000,
            rate_limit_per_minute: 60,
            rate_limit_burst: 10,
            tools: mcp::manifest::ToolFilter::default(),
        };
        let err = server
            .resolve_to_manifest("bad", &Config::default())
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("client_key") && err.contains("client_cert"),
            "error should explain the missing cert: {err}"
        );
    }
}
