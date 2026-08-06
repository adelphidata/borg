//! MCP (Model Context Protocol) client config.
//!
//! Cortex-side operator surface. The wire-level types in `mcp::manifest`
//! (`ServerManifest`, `Transport`, `HttpAuth`, `ToolFilter`) consume already-
//! resolved plaintext values; this module owns the path from a SQLite
//! settings row (with `${SECRET:NAME}` placeholders) to an
//! `mcp::manifest::ServerManifest`.

use std::collections::{HashMap, HashSet};
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
    /// How long `initialize` waits for cache-miss servers before letting the
    /// daemon finish booting. Servers start concurrently; any still starting
    /// when the grace expires continue in the background and register their
    /// tools on completion (the same behaviour a cached catalog already has).
    /// `0` means don't wait at all. Default: 2_000.
    #[serde(default = "default_startup_grace_ms")]
    pub startup_grace_ms: u64,
    /// Legacy switch for MCP schema deferral: `true` forces
    /// [`McpToolSearchMode::On`] (full deferral behind `search_tools`)
    /// regardless of `tool_search_mode`. `false` (the default) delegates to
    /// `tool_search_mode`, whose `auto` default keeps small catalogs eager
    /// but defers a catalog whose schemas outgrow `listing_max_tokens`.
    /// Prefer `tool_search_mode`. Native/compiled-in tools are never
    /// deferred; dispatch is registry-keyed, so a deferred tool is callable
    /// as soon as the model discovers it.
    #[serde(default)]
    pub tool_search: bool,
    /// Tiered disclosure policy for MCP tool schemas — see
    /// [`McpToolSearchMode`]. Default `auto`: small catalogs stay eager
    /// (schemas advertised directly, unchanged behaviour); a catalog whose
    /// schemas outgrow `listing_max_tokens` is deferred behind
    /// `search_tools` with a budget-scaled catalog listing.
    #[serde(default)]
    pub tool_search_mode: McpToolSearchMode,
    /// Token budget for the deferred-mode catalog listing embedded in the
    /// `search_tools` description (name + one-line description per tool).
    /// Over budget the listing degrades to names-only; over budget even
    /// names-only, to the bare meta-tool. In `auto` mode this is also the
    /// eager-vs-defer activation threshold: full schemas are advertised
    /// eagerly only while they fit this budget. Default: 2000.
    #[serde(default = "default_listing_max_tokens")]
    pub listing_max_tokens: usize,
}

/// Disclosure policy for MCP tool schemas in the per-turn tool list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum McpToolSearchMode {
    /// Always advertise every MCP schema directly (legacy eager behaviour).
    Off,
    /// Always defer MCP schemas behind `search_tools`.
    On,
    /// Defer only when the full schemas exceed `listing_max_tokens` —
    /// small catalogs pay nothing, pathological ones stop flooding the
    /// context window.
    #[default]
    Auto,
}

impl std::str::FromStr for McpToolSearchMode {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "off" => Ok(Self::Off),
            "on" => Ok(Self::On),
            "auto" => Ok(Self::Auto),
            other => Err(anyhow::anyhow!(
                "invalid tool_search_mode '{other}' (expected off|on|auto)"
            )),
        }
    }
}

impl std::fmt::Display for McpToolSearchMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Off => "off",
            Self::On => "on",
            Self::Auto => "auto",
        })
    }
}

/// Which disclosure shape the per-turn tool list should use for MCP tools.
/// Produced by [`McpConfig::disclosure_tier`] from the three size estimates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpDisclosureTier {
    /// Advertise every MCP schema directly.
    Eager,
    /// Defer schemas; `search_tools` carries a name + one-line-description
    /// catalog listing.
    Listed,
    /// Defer schemas; the listing degrades to tool names only.
    NamesOnly,
    /// Defer schemas; even names don't fit — bare `search_tools`, discovery
    /// through queries only.
    Bare,
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
            startup_grace_ms: default_startup_grace_ms(),
            tool_search: false,
            tool_search_mode: McpToolSearchMode::default(),
            listing_max_tokens: default_listing_max_tokens(),
        }
    }
}

impl McpConfig {
    /// Resolve the effective disclosure mode, honoring the legacy
    /// `tool_search` boolean: `true` forces `On` so existing installs that
    /// opted into full deferral keep it.
    pub fn effective_tool_search_mode(&self) -> McpToolSearchMode {
        if self.tool_search {
            McpToolSearchMode::On
        } else {
            self.tool_search_mode
        }
    }

    /// Decide the MCP disclosure tier from three size estimates (in tokens):
    /// the full schema payload, the name+description listing, and the
    /// names-only listing. All three are estimates of what the corresponding
    /// shape would add to every request.
    pub fn disclosure_tier(
        &self,
        schema_tokens: usize,
        listing_tokens: usize,
        names_tokens: usize,
    ) -> McpDisclosureTier {
        if !self.enabled {
            return McpDisclosureTier::Eager; // no MCP tools — moot
        }
        let deferred = match self.effective_tool_search_mode() {
            McpToolSearchMode::Off => false,
            McpToolSearchMode::On => true,
            McpToolSearchMode::Auto => schema_tokens > self.listing_max_tokens,
        };
        if !deferred {
            return McpDisclosureTier::Eager;
        }
        if listing_tokens <= self.listing_max_tokens {
            McpDisclosureTier::Listed
        } else if names_tokens <= self.listing_max_tokens {
            McpDisclosureTier::NamesOnly
        } else {
            McpDisclosureTier::Bare
        }
    }
}

fn default_listing_max_tokens() -> usize {
    2000
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

fn default_startup_grace_ms() -> u64 {
    2_000
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
    /// Cache this stdio server's advertised tool catalog so a restart can
    /// present its tools while the process starts in the background. Default:
    /// true. Set false for a server whose catalog is generated per-run and
    /// would be misleading if shown from the previous run. Ignored for
    /// HTTP/SSE, which have no spawn cost to amortize.
    #[serde(default = "default_true")]
    pub cache_tools: bool,
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
                env: resolve_secret_map(env, cfg, &format!("mcp.servers.{name}.env"))?,
            },
            TransportSpec::Http {
                url,
                headers,
                auth,
                client_cert,
                client_key,
            } => mcp::manifest::Transport::Http {
                url: {
                    // `url` never goes through `resolve_secret_placeholders`,
                    // so it needs its own check — a trailing space here fails
                    // as an unhelpful connect error.
                    warn_if_padded(&format!("mcp.servers.{name}.url"), url);
                    url.clone()
                },
                headers: resolve_secret_map(headers, cfg, &format!("mcp.servers.{name}.headers"))?,
                auth: resolve_auth(auth, cfg, name)?,
                client_identity: resolve_client_identity(client_cert, client_key)?,
            },
            TransportSpec::Sse {
                url,
                headers,
                auth,
                client_cert,
                client_key,
            } => mcp::manifest::Transport::Sse {
                url: {
                    // `url` never goes through `resolve_secret_placeholders`,
                    // so it needs its own check — a trailing space here fails
                    // as an unhelpful connect error.
                    warn_if_padded(&format!("mcp.servers.{name}.url"), url);
                    url.clone()
                },
                headers: resolve_secret_map(headers, cfg, &format!("mcp.servers.{name}.headers"))?,
                auth: resolve_auth(auth, cfg, name)?,
                client_identity: resolve_client_identity(client_cert, client_key)?,
            },
        };
        warn_on_hidden_whitespace(name, &transport);
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

/// Warn when a resolved transport value carries leading or trailing
/// whitespace.
///
/// A token pasted with a trailing newline, or a URL with a leading space, is
/// invisible in the config and surfaces much later as an opaque auth or
/// connect failure that looks like the server's fault. Checked *after* secret
/// interpolation, so whitespace living inside a referenced credential is caught
/// too.
///
/// Advisory only, and deliberately so: the value is never mutated (trimming an
/// operator's credential for them is its own class of bug) and never logged —
/// these are secrets, so only the server and the field path are named. Deduped
/// to once per process per (server, field) to keep a reconnect loop from
/// filling the log.
fn warn_on_hidden_whitespace(server: &str, transport: &mcp::manifest::Transport) {
    for field in hidden_whitespace_fields(transport) {
        if !claim_whitespace_warning(server, &field) {
            continue;
        }
        tracing::warn!(
            "mcp server '{server}': value at `{field}` has leading or trailing whitespace \
             (often a pasted credential that carried a newline). It is sent as-is, which \
             usually surfaces later as an opaque auth or connect error — re-enter the value \
             without the surrounding whitespace."
        );
    }
}

/// Dotted field paths whose resolved value has leading/trailing whitespace.
///
/// Returns paths, never values — the caller logs what this returns, and these
/// fields hold credentials.
fn hidden_whitespace_fields(transport: &mcp::manifest::Transport) -> Vec<String> {
    // The common case is that nothing is padded, so test the value first and
    // only build a field path for the rare hit.
    fn is_padded(value: &str) -> bool {
        !value.is_empty() && value.trim() != value
    }

    let mut flagged: Vec<String> = Vec::new();
    match transport {
        mcp::manifest::Transport::Stdio { command, env, .. } => {
            if is_padded(command) {
                flagged.push("command".to_string());
            }
            for (k, _) in env.iter().filter(|(_, v)| is_padded(v)) {
                flagged.push(format!("env[{k}]"));
            }
        }
        mcp::manifest::Transport::Http {
            url, headers, auth, ..
        }
        | mcp::manifest::Transport::Sse {
            url, headers, auth, ..
        } => {
            if is_padded(url) {
                flagged.push("url".to_string());
            }
            for (k, _) in headers.iter().filter(|(_, v)| is_padded(v)) {
                flagged.push(format!("headers[{k}]"));
            }
            if matches!(auth, mcp::manifest::HttpAuth::Bearer { token } if is_padded(token)) {
                flagged.push("auth.bearer.token".to_string());
            }
        }
    }

    // Map iteration order is arbitrary; sort so the warning stream (and the
    // tests) are stable across runs.
    flagged.sort();
    flagged
}

/// Record a (server, field) pair as warned. Returns `false` when it was already
/// warned about in this process.
fn claim_whitespace_warning(server: &str, field: &str) -> bool {
    static WARNED: std::sync::OnceLock<std::sync::Mutex<HashSet<String>>> =
        std::sync::OnceLock::new();
    let warned = WARNED.get_or_init(|| std::sync::Mutex::new(HashSet::new()));
    // A poisoned mutex must not silence a config warning; the worst case is a
    // repeated log line.
    let mut set = warned
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    set.insert(format!("{server}\0{field}"))
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

fn resolve_auth(spec: &HttpAuthSpec, cfg: &Config, name: &str) -> Result<mcp::manifest::HttpAuth> {
    match spec {
        HttpAuthSpec::None => Ok(mcp::manifest::HttpAuth::None),
        HttpAuthSpec::Bearer { token } => Ok(mcp::manifest::HttpAuth::Bearer {
            token: resolve_secret_placeholders(
                token,
                cfg,
                &format!("mcp.servers.{name}.auth.bearer.token"),
            )?,
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

/// What kind of invisible padding a config value carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PaddingReport {
    /// Leading whitespace characters.
    pub leading: usize,
    /// Trailing whitespace characters.
    pub trailing: usize,
    /// Which whitespace classes are present, e.g. `"LF"`, `"space, CR"`.
    pub classes: String,
}

/// Describe leading/trailing whitespace on a config value, or `None` when
/// there is none.
///
/// Interior whitespace is explicitly not flagged — `"Bearer a b"` and a
/// multi-word header value are perfectly legal, and flagging them would make
/// the warning noise. Only the invisible edges matter, because that is what a
/// paste from a browser or a `cat` of a token file actually produces.
///
/// The report deliberately carries no fragment of the value, and not even its
/// length: these values are credentials, and a length is entropy.
pub(crate) fn padding_report(value: &str) -> Option<PaddingReport> {
    if value.is_empty() || value.trim() == value {
        return None;
    }
    // Char counts, not byte counts: the field is documented (and rendered) as
    // a number of whitespace characters, and the `edges` reconstruction below
    // consumes them as such — a byte count would mis-slice a multi-byte
    // whitespace character like U+00A0.
    let leading = value.chars().take_while(|c| c.is_whitespace()).count();
    // Counted on what's left *after* the leading run, so an all-whitespace
    // value reports its characters once rather than twice.
    let trailing = value
        .trim_start()
        .chars()
        .rev()
        .take_while(|c| c.is_whitespace())
        .count();

    let edges: String = value
        .chars()
        .take(leading)
        .chain(value.chars().rev().take(trailing))
        .collect();
    let mut classes = Vec::new();
    for (ch, label) in [(' ', "space"), ('\t', "tab"), ('\r', "CR"), ('\n', "LF")] {
        if edges.contains(ch) {
            classes.push(label);
        }
    }
    if edges
        .chars()
        .any(|c| c.is_whitespace() && !" \t\r\n".contains(c))
    {
        classes.push("other");
    }
    Some(PaddingReport {
        leading,
        trailing,
        classes: classes.join(", "),
    })
}

/// Warn once per config key about invisible padding on a resolved value.
///
/// Never mutates: silently trimming would change a legitimately padded header
/// value with no trace. Never logs the value. Deduped by key path because
/// `resolve_to_manifest` runs on every connect and hot-reload, so a flapping
/// server would otherwise fill the log.
fn warn_if_padded(key_path: &str, value: &str) {
    let Some(report) = padding_report(value) else {
        return;
    };
    static SEEN: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<String>>> =
        std::sync::OnceLock::new();
    let seen = SEEN.get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()));
    {
        let mut guard = seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !guard.insert(key_path.to_string()) {
            return;
        }
    }
    tracing::warn!(
        key = %key_path,
        leading = report.leading,
        trailing = report.trailing,
        whitespace = %report.classes,
        "MCP config value has leading/trailing whitespace — a pasted token with \
         a trailing newline typically fails later as an opaque 401. The value is \
         used as-is; re-set it without the padding."
    );
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
    // After interpolation on purpose: a credential sourced from the settings
    // table or the environment can carry the trailing newline just as easily
    // as a literal in the config can.
    warn_if_padded(field, &out);
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

    /// A pasted credential that carried a newline is invisible in the config
    /// and fails much later as an opaque auth error. Every transport value the
    /// operator can paste into gets checked — and the detector yields field
    /// paths only, never values, because these fields hold secrets.
    #[test]
    fn hidden_whitespace_is_flagged_by_field_path_across_transports() {
        let stdio = mcp::manifest::Transport::Stdio {
            command: "server-bin\n".to_string(),
            args: vec![],
            env: HashMap::from([
                ("API_KEY".to_string(), "sk-live-abc123\n".to_string()),
                ("CLEAN".to_string(), "fine".to_string()),
            ]),
        };
        let flagged = hidden_whitespace_fields(&stdio);
        assert_eq!(flagged, vec!["command", "env[API_KEY]"]);
        assert!(
            !flagged.iter().any(|f| f.contains("sk-live-abc123")),
            "the credential value must never reach the log line"
        );

        let http = mcp::manifest::Transport::Http {
            url: " https://example.com/mcp".to_string(),
            headers: HashMap::from([("X-Api-Key".to_string(), "  padded".to_string())]),
            auth: mcp::manifest::HttpAuth::Bearer {
                token: "tok\t".to_string(),
            },
            client_identity: None,
        };
        assert_eq!(
            hidden_whitespace_fields(&http),
            vec!["auth.bearer.token", "headers[X-Api-Key]", "url"]
        );
    }

    /// Clean config must stay silent, and an empty value is a different
    /// problem (unset credential) that this check must not claim as its own.
    #[test]
    fn clean_and_empty_transport_values_are_not_flagged() {
        let clean = mcp::manifest::Transport::Stdio {
            command: "server-bin".to_string(),
            args: vec![],
            env: HashMap::from([
                ("API_KEY".to_string(), "sk-live-abc123".to_string()),
                ("EMPTY".to_string(), String::new()),
            ]),
        };
        assert!(hidden_whitespace_fields(&clean).is_empty());
    }

    /// The whole point is catching whitespace the operator can't see, so a
    /// value that only becomes padded after `${SECRET:…}` interpolation must
    /// be caught too — the check has to run on resolved values, not raw config.
    #[test]
    fn whitespace_inside_a_resolved_secret_is_caught() {
        let _lock = env_lock();
        let _env = EnvGuard::set("PADDED_TOKEN", "sk-live-abc123\n");
        let cfg = cfg_with("PADDED_TOKEN");

        let json = serde_json::json!({
            "transport": {
                "type": "stdio",
                "command": "true",
                "env": { "API_KEY": "${SECRET:PADDED_TOKEN}" }
            }
        });
        let server: McpServerConfig = serde_json::from_value(json).unwrap();
        let manifest = server.resolve_to_manifest("padded", &cfg).unwrap();

        assert_eq!(
            hidden_whitespace_fields(&manifest.transport),
            vec!["env[API_KEY]"],
            "whitespace living inside the credential must be caught after interpolation"
        );
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
            cache_tools: true,
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
            cache_tools: true,
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
            cache_tools: true,
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

    // ── invisible whitespace on config values ─────────────────────────────

    #[test]
    fn padding_report_flags_only_edge_whitespace() {
        // `"Bearer a b"` is the row that fails a naive
        // `contains(char::is_whitespace)` implementation.
        let cases: &[(&str, Option<(usize, usize)>)] = &[
            ("tok", None),
            ("", None),
            ("Bearer a b", None),
            ("multi word header value", None),
            ("tok\n", Some((0, 1))),
            (" tok", Some((1, 0))),
            ("  tok\t\r\n", Some((2, 3))),
            ("   ", Some((3, 0))),
        ];
        for (value, want) in cases {
            let got = padding_report(value).map(|r| (r.leading, r.trailing));
            assert_eq!(got, *want, "padding_report wrong for {value:?}");
        }
    }

    #[test]
    fn padding_report_names_the_whitespace_classes() {
        let r = padding_report("tok\r\n").expect("padded");
        assert!(r.classes.contains("CR"), "got: {}", r.classes);
        assert!(r.classes.contains("LF"), "got: {}", r.classes);
    }

    #[test]
    fn padding_report_never_contains_the_value() {
        // These reports are rendered into ~/.borg/logs/. A future
        // `format!("{value:?}")` would put the credential there.
        let secret = "sk-live-super-secret-token";
        let r = padding_report(&format!("{secret}\n")).expect("padded");
        let rendered = format!("{r:?}");
        assert!(
            !rendered.contains(secret),
            "report leaked the value: {rendered}"
        );
        assert!(
            !rendered.contains("secret"),
            "report leaked a fragment: {rendered}"
        );
    }

    #[test]
    fn secret_sourced_padding_is_detected_after_interpolation() {
        // This is what fails if the check runs *before* `${SECRET:…}`
        // interpolation: the config literal is clean, the resolved value is
        // not, and the resolved value is what reaches the server.
        let _lock = env_lock();
        let key = "BORG_TEST_MCP_PADDED";
        let _g = EnvGuard::set(key, "sk-live-abc\n");
        let cfg = Config::default();
        let resolved =
            resolve_secret_placeholders(&format!("${{SECRET:{key}}}"), &cfg, "headers[X-Api-Key]")
                .expect("resolves");
        assert!(
            padding_report(&resolved).is_some(),
            "trailing newline from the credential store must be detected"
        );
    }

    #[test]
    fn unresolved_placeholder_error_names_the_server_and_field() {
        // The qualified key path is what makes the message actionable when
        // several servers declare the same header name.
        let _lock = env_lock();
        let cfg = Config::default();
        let err = resolve_secret_placeholders(
            "${SECRET:BORG_TEST_MCP_DEFINITELY_MISSING}",
            &cfg,
            "mcp.servers.myserver.headers[X-Api-Key]",
        )
        .expect_err("missing credential must error");
        let msg = err.to_string();
        assert!(msg.contains("mcp.servers.myserver"), "got: {msg}");
        assert!(msg.contains("X-Api-Key"), "got: {msg}");
    }
}
