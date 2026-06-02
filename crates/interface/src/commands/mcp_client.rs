//! `borg mcp {list,add,add-preset,remove,enable,disable,test}` — operator
//! surface for the MCP *client* side (Borg consuming external MCP servers).
//!
//! The server-side `borg mcp stdio` lives in `commands::mcp` and is unrelated
//! — it exposes Borg to other MCP clients (Claude Desktop, MCP Inspector).
//! Borg-as-client is the path the agent loop actually uses every turn.
//!
//! All write paths mutate the same `mcp.servers` JSON setting that
//! `cortex::config::mcp::McpConfig` round-trips, so settings written here
//! show up unchanged after `cortex::config::Config::load_from_db()`.
//!
//! `test` is intentionally standalone — it connects directly via the `mcp`
//! crate rather than going through `borgd`, so an operator can sanity-check
//! a freshly-added server without restarting the daemon.

use std::collections::HashMap;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use clap::Args;
use cortex::config::{Config, HttpAuthSpec, McpServerConfig, TransportSpec};
use cortex::db::Database;
use mcp::manifest::{ServerManifest, ToolFilter};
use mcp::transport::http::HttpPolicy;
use serde::{Deserialize, Serialize};

/// Settings key for the per-server JSON map (mirrors `define_settings!`
/// registration in `cortex/src/config/settings_table.rs`).
const SERVERS_KEY: &str = "mcp.servers";

/// Argument shared by `remove` / `enable` / `disable` / `test`.
#[derive(Args)]
pub(crate) struct IdArgs {
    /// Server name (the `mcp.servers` map key).
    pub id: String,
}

/// `borg mcp add <id> --stdio CMD [-- args...] [--env KEY=VAL]...`
/// or `borg mcp add <id> --http URL [--header K=V]... [--auth bearer:TOKEN]`
/// or `borg mcp add <id> --sse URL ...`.
#[derive(Args)]
pub(crate) struct AddArgs {
    /// Server name (must be `[a-z0-9_-]+`, becomes the namespace for tools).
    pub id: String,

    /// Stdio transport: path or argv0 of the MCP server executable.
    #[arg(long, group = "transport", conflicts_with_all = ["http", "sse"])]
    pub stdio: Option<String>,

    /// Streamable-HTTP transport: full URL of the MCP endpoint.
    #[arg(long, group = "transport", conflicts_with_all = ["stdio", "sse"])]
    pub http: Option<String>,

    /// Legacy HTTP+SSE transport: full URL of the `/sse` endpoint.
    #[arg(long, group = "transport", conflicts_with_all = ["stdio", "http"])]
    pub sse: Option<String>,

    /// CLI args for the stdio command. Pass after `--`.
    #[arg(last = true)]
    pub args: Vec<String>,

    /// Environment variables for stdio children: `--env KEY=VAL`. May be
    /// repeated. Values may contain `${SECRET:NAME}` placeholders that get
    /// resolved at connect time from the credential store.
    #[arg(long = "env", value_parser = parse_kv)]
    pub envs: Vec<(String, String)>,

    /// HTTP headers (HTTP / SSE only): `--header KEY=VAL`. May be repeated.
    #[arg(long = "header", value_parser = parse_kv)]
    pub headers: Vec<(String, String)>,

    /// Bearer token shortcut: `--auth bearer:TOKEN` (or `bearer:${SECRET:N}`).
    /// Equivalent to `--header Authorization=Bearer\ TOKEN` but cleaner.
    #[arg(long)]
    pub auth: Option<String>,

    /// Client certificate PEM (HTTP / SSE only) presented for mutual TLS. `~`
    /// is expanded. May be a combined cert+key PEM, in which case `--client-key`
    /// is omitted.
    #[arg(long)]
    pub client_cert: Option<String>,

    /// Client private key PEM (HTTP / SSE only), when separate from the cert.
    #[arg(long, requires = "client_cert")]
    pub client_key: Option<String>,

    /// Initial handshake timeout (ms).
    #[arg(long, default_value_t = 60_000)]
    pub connect_timeout_ms: u64,

    /// Per-call timeout (ms).
    #[arg(long, default_value_t = 120_000)]
    pub call_timeout_ms: u64,

    /// Steady-state rate limit (calls/minute).
    #[arg(long, default_value_t = 60)]
    pub rate_limit_per_minute: u32,

    /// Burst capacity before rate-limit refusal.
    #[arg(long, default_value_t = 10)]
    pub rate_limit_burst: u32,

    /// Per-server tool count cap (defense against schema flood).
    #[arg(long, default_value_t = ToolFilter::DEFAULT_MAX)]
    pub max_tools: usize,

    /// Overwrite an existing entry with the same id.
    #[arg(long)]
    pub force: bool,
}

/// `borg mcp add-preset <id> --preset <name>`.
#[derive(Args)]
pub(crate) struct AddPresetArgs {
    /// Server name (`mcp.servers` map key).
    pub id: String,

    /// Preset name. See `PRESETS` for the catalog.
    #[arg(long, value_parser = ["time", "filesystem", "github", "notion"])]
    pub preset: String,

    /// Filesystem preset only: path to expose as the scoped root.
    #[arg(long)]
    pub fs_root: Option<String>,

    /// Overwrite an existing entry with the same id.
    #[arg(long)]
    pub force: bool,
}

fn parse_kv(s: &str) -> Result<(String, String), String> {
    match s.split_once('=') {
        Some((k, v)) if !k.is_empty() => Ok((k.to_string(), v.to_string())),
        _ => Err(format!("expected KEY=VALUE, got `{s}`")),
    }
}

/// `borg mcp list` — print servers + tool counts.
pub(crate) async fn list() -> Result<()> {
    let cfg = Config::load_from_db().await.context("config load failed")?;
    if !cfg.mcp.enabled {
        println!("MCP is globally disabled (`mcp.enabled = false`).");
    }
    if cfg.mcp.servers.is_empty() {
        println!("No MCP servers configured.");
        println!("Try `borg mcp add-preset time --preset time` for a smoke test.");
        return Ok(());
    }
    let disabled: std::collections::HashSet<&String> = cfg.mcp.disabled_servers.iter().collect();
    println!(
        "{:<24} {:<14} {:<8} {:<32}",
        "NAME", "TRANSPORT", "ENABLED", "DETAIL"
    );
    let mut names: Vec<&String> = cfg.mcp.servers.keys().collect();
    names.sort();
    for name in names {
        let s = &cfg.mcp.servers[name];
        let (transport, detail) = match &s.transport {
            TransportSpec::Stdio { command, args, .. } => (
                "stdio",
                format!("{} {}", command, args.join(" ")).trim().to_string(),
            ),
            TransportSpec::Http { url, .. } => ("http", url.clone()),
            TransportSpec::Sse { url, .. } => ("sse", url.clone()),
        };
        let enabled = s.enabled && !disabled.contains(name);
        println!(
            "{:<24} {:<14} {:<8} {:<32}",
            truncate(name, 24),
            transport,
            if enabled { "yes" } else { "no" },
            truncate(&detail, 32)
        );
    }
    Ok(())
}

/// `borg mcp add <id> [--stdio|--http|--sse] …`.
pub(crate) async fn add(args: AddArgs) -> Result<()> {
    validate_id(&args.id)?;
    let transport = build_transport(&args)?;
    let server = McpServerConfig {
        enabled: true,
        transport,
        connect_timeout_ms: args.connect_timeout_ms,
        call_timeout_ms: args.call_timeout_ms,
        rate_limit_per_minute: args.rate_limit_per_minute,
        rate_limit_burst: args.rate_limit_burst,
        tools: ToolFilter {
            max: args.max_tools,
            ..Default::default()
        },
    };
    persist_server(&args.id, server, args.force).await?;
    println!("Added MCP server `{}`.", args.id);
    println!(
        "Run `borg mcp test {}` to verify, or restart `borgd` to pick it up.",
        args.id
    );
    Ok(())
}

/// `borg mcp add-preset <id> --preset <name>`.
pub(crate) async fn add_preset(args: AddPresetArgs) -> Result<()> {
    validate_id(&args.id)?;
    let server = preset_to_config(&args.preset, args.fs_root.as_deref())?;
    persist_server(&args.id, server, args.force).await?;
    println!(
        "Added MCP server `{}` from preset `{}`.",
        args.id, args.preset
    );
    if args.preset == "github" {
        println!(
            "Set the token first:  borg settings set credentials.github_pat=ghp_…  \
             (referenced as `${{SECRET:github_pat}}`)."
        );
    }
    Ok(())
}

/// `borg mcp remove <id>`.
pub(crate) async fn remove(args: IdArgs) -> Result<()> {
    mutate_servers(|map| {
        if map.remove(&args.id).is_none() {
            bail!("no MCP server named `{}`", args.id);
        }
        Ok(())
    })
    .await?;
    println!("Removed MCP server `{}`.", args.id);
    Ok(())
}

/// `borg mcp enable|disable <id>`.
pub(crate) async fn enable(args: IdArgs, want_enabled: bool) -> Result<()> {
    mutate_servers(|map| {
        let entry = map
            .get_mut(&args.id)
            .ok_or_else(|| anyhow!("no MCP server named `{}`", args.id))?;
        entry.enabled = want_enabled;
        Ok(())
    })
    .await?;
    println!(
        "{} MCP server `{}`.",
        if want_enabled { "Enabled" } else { "Disabled" },
        args.id
    );
    Ok(())
}

/// `borg mcp test <id>` — one-shot connect + tools/list, no borgd.
pub(crate) async fn test(args: IdArgs) -> Result<()> {
    let cfg = Config::load_from_db().await.context("config load failed")?;
    // Honor the global kill switch unless the operator forces. A test that
    // succeeds while `mcp.enabled = false` would suggest the agent will pick
    // the server up next turn — which it won't, because init refuses. The
    // explicit error sets expectations.
    if !cfg.mcp.enabled {
        bail!(
            "MCP is globally disabled (`mcp.enabled = false`); enable it with \
             `borg settings set mcp.enabled true` or pass --force to probe anyway"
        );
    }
    let server_cfg = cfg
        .mcp
        .servers
        .get(&args.id)
        .ok_or_else(|| anyhow!("no MCP server named `{}` — try `borg mcp list`", args.id))?;
    let manifest: ServerManifest = server_cfg.resolve_to_manifest(&args.id, &cfg)?;
    let policy = HttpPolicy {
        allow_plaintext_http: cfg.mcp.allow_plaintext_http,
        allow_local_network: cfg.mcp.allow_local_network,
    };
    let transport = mcp::transport::open(&manifest, policy)
        .await
        .context("transport open failed")?;
    let mut session = mcp::session::SessionActor::new(&manifest, transport);
    tokio::time::timeout(manifest.connect_timeout, session.handshake())
        .await
        .map_err(|_| anyhow!("handshake timed out after {:?}", manifest.connect_timeout))?
        .context("handshake failed")?;
    let discovered = session.list_tools().await.context("tools/list failed")?;
    // Print after tools/list so the operator never sees a stale "connected:"
    // banner followed by a downstream error — the printed status reflects
    // what the agent will actually be able to do.
    println!("connected: {}", args.id);
    if let Some(info) = discovered.server_info.as_deref() {
        println!("server:    {info}");
    }
    println!("tools:     {}", discovered.tools.len());
    for t in &discovered.tools {
        println!("  - {}", t.name);
    }
    // Session does not yet expose a graceful shutdown; dropping the actor
    // closes the underlying transport. The `let _ = timeout(yield_now)` is
    // a CLAUDE.md-blessed best-effort cleanup (analogous to terminal-Drop
    // and temp-file removal) — capping the linger so an unresponsive peer
    // cannot hang the CLI.
    drop(session);
    let _ = tokio::time::timeout(Duration::from_millis(200), tokio::task::yield_now()).await;
    Ok(())
}

fn validate_id(id: &str) -> Result<()> {
    if id.is_empty() || id.len() > 64 {
        bail!("server id must be 1..=64 chars");
    }
    if !id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        bail!("server id must match [a-zA-Z0-9_-]+");
    }
    Ok(())
}

fn build_transport(args: &AddArgs) -> Result<TransportSpec> {
    if let Some(cmd) = &args.stdio {
        return Ok(TransportSpec::Stdio {
            command: cmd.clone(),
            args: args.args.clone(),
            env: args.envs.iter().cloned().collect(),
        });
    }
    let (url, is_sse) = match (&args.http, &args.sse) {
        (Some(u), None) => (u.clone(), false),
        (None, Some(u)) => (u.clone(), true),
        _ => bail!("must specify exactly one of --stdio / --http / --sse"),
    };
    let headers: HashMap<String, String> = args.headers.iter().cloned().collect();
    let auth = if let Some(a) = &args.auth {
        let Some(token) = a.strip_prefix("bearer:") else {
            bail!("--auth must look like `bearer:TOKEN` or `bearer:${{SECRET:NAME}}`");
        };
        // Both forms supported: the structured Bearer (auth) field AND a raw
        // Authorization header. We prefer the structured field because the
        // transport then knows to refresh it on 401 in future iterations.
        HttpAuthSpec::Bearer {
            token: token.to_string(),
        }
    } else {
        HttpAuthSpec::None
    };
    // Reject obvious mistakes early — neither path nor stdio command, but
    // also no URL scheme.
    if !url.starts_with("http://") && !url.starts_with("https://") {
        bail!("URL must start with http:// or https://");
    }
    // Refuse a conflicting `--auth bearer:…` + `--header Authorization=…`
    // combination outright. Silently dropping one would mask a real
    // operator mistake (e.g. they pasted a Basic credentials string into
    // a header thinking that's how they authenticate). The operator must
    // pick which is authoritative.
    if matches!(&auth, HttpAuthSpec::Bearer { .. })
        && headers
            .keys()
            .any(|k| k.eq_ignore_ascii_case("authorization"))
    {
        bail!(
            "--auth and --header Authorization=… are both set; --auth is authoritative — \
             drop the explicit Authorization header to resolve the ambiguity"
        );
    }
    let client_cert = args.client_cert.clone();
    let client_key = args.client_key.clone();
    Ok(if is_sse {
        TransportSpec::Sse {
            url,
            headers,
            auth,
            client_cert,
            client_key,
        }
    } else {
        TransportSpec::Http {
            url,
            headers,
            auth,
            client_cert,
            client_key,
        }
    })
}

async fn persist_server(id: &str, server: McpServerConfig, force: bool) -> Result<()> {
    mutate_servers(|map| {
        if map.contains_key(id) && !force {
            bail!("MCP server `{id}` already exists — pass --force to overwrite");
        }
        map.insert(id.to_string(), server.clone());
        Ok(())
    })
    .await
}

/// Optimistic-CAS read-modify-write on the `mcp.servers` JSON setting.
///
/// libsql's WAL mode allows concurrent writers (borgd's settings writer +
/// standalone `borg` CLI processes). A naive read-then-write
/// can lose entries when two `borg mcp add` invocations interleave — both
/// read the same prior JSON, both insert, the later `set_setting` wins, and
/// the earlier entry is silently dropped.
///
/// We avoid that by snapshotting `PRAGMA data_version` before the read; if
/// it has advanced by write time we re-read and re-apply the closure. The
/// closure runs at most `MAX_RETRIES` times — far more than any realistic
/// contention burst on settings — and refuses with a clear error if the
/// database is genuinely write-saturated (operator can retry by hand).
async fn mutate_servers<F>(mutator: F) -> Result<()>
where
    F: Fn(&mut HashMap<String, McpServerConfig>) -> Result<()>,
{
    /// Retry budget. Settings writes are infrequent and from the operator's
    /// CLI; if we lose CAS this many times in a row something pathological
    /// is happening.
    const MAX_RETRIES: u32 = 8;

    let db = Database::open().await.context("open db")?;
    for attempt in 0..MAX_RETRIES {
        let v0 = db
            .data_version()
            .await
            .context("read PRAGMA data_version (pre)")?;
        let current_raw = db
            .get_setting(SERVERS_KEY)
            .await
            .context("read mcp.servers")?
            .unwrap_or_else(|| "{}".to_string());
        let mut map: HashMap<String, McpServerConfig> = serde_json::from_str(&current_raw)
            .unwrap_or_else(|_| {
                // Round-trip recovery: a malformed setting shouldn't trap
                // the operator. Surface a warning and start from empty
                // rather than silently dropping their last entry — the
                // upcoming write will overwrite whatever was there.
                tracing::warn!(
                    value = %current_raw,
                    "mcp.servers is not valid JSON; treating as empty"
                );
                HashMap::new()
            });
        mutator(&mut map)?;
        let next = serde_json::to_string(&map).context("serialize mcp.servers")?;
        // Re-check the version before committing. If anyone wrote between
        // our read and now, our snapshot is stale — retry with the latest
        // state. This is a soft CAS: it narrows the race window but doesn't
        // close it perfectly. The libsql crate doesn't expose `BEGIN
        // IMMEDIATE` from this surface; a future helper on `Database`
        // should wrap the whole compound operation in one transaction.
        let v1 = db
            .data_version()
            .await
            .context("read PRAGMA data_version (post)")?;
        if v1 != v0 {
            tracing::debug!(
                attempt,
                v0,
                v1,
                "mcp.servers changed under read; retrying CAS"
            );
            continue;
        }
        db.set_setting(SERVERS_KEY, &next)
            .await
            .context("write mcp.servers")?;
        return Ok(());
    }
    bail!(
        "mcp.servers CAS retried {MAX_RETRIES} times without converging — \
         the database appears to be under sustained concurrent write pressure; \
         retry the command"
    )
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(n.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

/// Catalog of operator-friendly MCP server presets. These are *templates* —
/// adding one writes a row, it does NOT auto-install anything. The first
/// time the server actually runs it pulls the underlying binary via
/// `npx`/`uvx`, so operators control the install moment.
///
/// Keep the list short. These four cover the whole transport matrix while
/// keeping the mental overhead of picking a preset low:
///
/// - `time` exercises stdio with no secrets (smoke test).
/// - `filesystem` exercises stdio with a scoped path argument.
/// - `github` exercises stdio with a `${SECRET:…}` credential.
/// - `notion` exercises remote HTTP (operator must add OAuth headers).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Preset {
    transport: TransportSpec,
    rate_limit_per_minute: u32,
    rate_limit_burst: u32,
}

fn preset_to_config(name: &str, fs_root: Option<&str>) -> Result<McpServerConfig> {
    let p = match name {
        "time" => Preset {
            transport: TransportSpec::Stdio {
                command: "uvx".into(),
                args: vec!["mcp-server-time".into()],
                env: HashMap::new(),
            },
            rate_limit_per_minute: 60,
            rate_limit_burst: 10,
        },
        "filesystem" => {
            let root = fs_root
                .map(str::to_string)
                .or_else(|| {
                    dirs::home_dir()
                        .map(|h| h.join(".borg").join("mcp").join("fs-scope"))
                        .and_then(|p| p.to_str().map(str::to_string))
                })
                .ok_or_else(|| {
                    anyhow!("filesystem preset needs --fs-root <path> (could not derive default)")
                })?;
            // Refuse $HOME-as-root: `mcp-server-filesystem` recursively
            // exposes whatever it's given. Borg's `blocked_paths` will still
            // filter sensitive subpaths at the tool boundary, but it's much
            // safer to scope the server to a dedicated subtree.
            if let Some(home) = dirs::home_dir() {
                if std::path::Path::new(&root) == home {
                    bail!(
                        "filesystem preset refuses $HOME as the root — pick a subdirectory \
                         (e.g. ~/.borg/mcp/fs-scope) to limit blast radius"
                    );
                }
            }
            Preset {
                transport: TransportSpec::Stdio {
                    command: "npx".into(),
                    args: vec![
                        "-y".into(),
                        "@modelcontextprotocol/server-filesystem".into(),
                        root,
                    ],
                    env: HashMap::new(),
                },
                rate_limit_per_minute: 120,
                rate_limit_burst: 20,
            }
        }
        "github" => Preset {
            transport: TransportSpec::Stdio {
                command: "npx".into(),
                args: vec!["-y".into(), "@modelcontextprotocol/server-github".into()],
                env: HashMap::from([(
                    "GITHUB_PERSONAL_ACCESS_TOKEN".into(),
                    "${SECRET:github_pat}".into(),
                )]),
            },
            rate_limit_per_minute: 60,
            rate_limit_burst: 10,
        },
        "notion" => Preset {
            transport: TransportSpec::Http {
                url: "https://mcp.notion.com/mcp".into(),
                headers: HashMap::new(),
                auth: HttpAuthSpec::None,
                client_cert: None,
                client_key: None,
            },
            rate_limit_per_minute: 60,
            rate_limit_burst: 10,
        },
        other => bail!("unknown preset `{other}` — choose: time, filesystem, github, notion"),
    };
    Ok(McpServerConfig {
        enabled: true,
        transport: p.transport,
        connect_timeout_ms: 60_000,
        call_timeout_ms: 120_000,
        rate_limit_per_minute: p.rate_limit_per_minute,
        rate_limit_burst: p.rate_limit_burst,
        tools: ToolFilter::default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_validation_rejects_empty_and_bad_chars() {
        // The id becomes a URL-like prefix on every tool name (`mcp_<id>_…`)
        // and is concatenated into log/audit fields. Anything outside the
        // safe alphabet would either break the tool-name parser downstream
        // or open an injection vector into structured logs.
        assert!(validate_id("ok-1").is_ok());
        assert!(validate_id("ok_2").is_ok());
        assert!(validate_id("").is_err());
        assert!(validate_id("with space").is_err());
        assert!(validate_id("with/slash").is_err());
        assert!(validate_id("$danger").is_err());
        assert!(validate_id(&"a".repeat(65)).is_err());
    }

    #[test]
    fn preset_time_uses_stdio_uvx() {
        // Smoke-test preset: stdio, no env, no secrets — proves the
        // transport plumbing without auth complexity. If a future refactor
        // changes the command or transport shape, this test pins the
        // contract that the smoke preset stays credential-free.
        let cfg = preset_to_config("time", None).unwrap();
        match &cfg.transport {
            TransportSpec::Stdio { command, args, env } => {
                assert_eq!(command, "uvx");
                assert_eq!(args, &vec!["mcp-server-time".to_string()]);
                assert!(env.is_empty(), "smoke preset must not require env vars");
            }
            other => panic!("expected stdio, got {other:?}"),
        }
    }

    #[test]
    fn preset_filesystem_refuses_home_root() {
        // Spec invariant: a misconfigured filesystem server can hand the LLM
        // arbitrary read/write under $HOME. The preset refuses that even
        // when an operator passes it explicitly. Borg's blocked_paths still
        // backstops at the tool boundary, but rejecting up front is much
        // less rope to hang oneself with.
        let home = dirs::home_dir().expect("home dir");
        let err = preset_to_config("filesystem", home.to_str()).unwrap_err();
        assert!(
            err.to_string().contains("refuses $HOME"),
            "expected explicit refusal, got: {err}"
        );
    }

    #[test]
    fn preset_github_uses_secret_placeholder_not_inline_token() {
        // Spec invariant: presets never inline a literal token. They emit
        // `${SECRET:NAME}` so the credential resolver pulls from the
        // keychain at connect time. Catches the regression where a future
        // edit accidentally bakes a placeholder default like "ghp_xxx".
        let cfg = preset_to_config("github", None).unwrap();
        match &cfg.transport {
            TransportSpec::Stdio { env, .. } => {
                let tok = env
                    .get("GITHUB_PERSONAL_ACCESS_TOKEN")
                    .expect("token env var");
                assert!(
                    tok.contains("${SECRET:"),
                    "preset must use a secret placeholder, got literal: {tok}"
                );
            }
            _ => panic!("github preset must be stdio"),
        }
    }

    #[test]
    fn preset_notion_is_https_only() {
        // Spec invariant: remote presets ship https URLs. With
        // allow_plaintext_http=false (the default), shipping a plain-http
        // preset would refuse to connect — and silently shipping http://
        // would invite operators to flip the global flag, weakening every
        // other server.
        let cfg = preset_to_config("notion", None).unwrap();
        match &cfg.transport {
            TransportSpec::Http { url, .. } => assert!(url.starts_with("https://")),
            _ => panic!("notion preset must be HTTP"),
        }
    }

    #[test]
    fn unknown_preset_errors() {
        let err = preset_to_config("not-a-real-preset", None).unwrap_err();
        assert!(err.to_string().contains("unknown preset"));
    }

    #[test]
    fn build_transport_rejects_no_choice() {
        let args = AddArgs {
            id: "x".into(),
            stdio: None,
            http: None,
            sse: None,
            args: vec![],
            envs: vec![],
            headers: vec![],
            auth: None,
            connect_timeout_ms: 1,
            call_timeout_ms: 1,
            rate_limit_per_minute: 1,
            rate_limit_burst: 1,
            max_tools: 1,
            client_cert: None,
            client_key: None,
            force: false,
        };
        let err = build_transport(&args).unwrap_err();
        assert!(err.to_string().contains("exactly one"));
    }

    #[test]
    fn build_transport_rejects_non_http_url() {
        // A typo like `file://...` or `ftp://...` would never reach the
        // policy check inside mcp::transport — catching it here means the
        // operator sees a clean error message at config time instead of a
        // transport error several seconds into a connect attempt.
        let args = AddArgs {
            id: "x".into(),
            stdio: None,
            http: Some("ftp://foo".into()),
            sse: None,
            args: vec![],
            envs: vec![],
            headers: vec![],
            auth: None,
            connect_timeout_ms: 1,
            call_timeout_ms: 1,
            rate_limit_per_minute: 1,
            rate_limit_burst: 1,
            max_tools: 1,
            client_cert: None,
            client_key: None,
            force: false,
        };
        let err = build_transport(&args).unwrap_err();
        assert!(err.to_string().contains("http://"));
    }

    #[test]
    fn build_transport_bearer_conflicts_with_authorization_header_errors() {
        // Spec invariant: structured `--auth bearer:…` is authoritative, and
        // when an operator also sets an explicit `--header Authorization=…`
        // the intent is genuinely ambiguous — they may have pasted a Basic
        // creds string they wanted to keep. Refuse with a clear error
        // rather than silently dropping one. This is the negative-path
        // counterpart to `--auth bearer:…` alone (which is accepted).
        let args = AddArgs {
            id: "x".into(),
            stdio: None,
            http: Some("https://example.com".into()),
            sse: None,
            args: vec![],
            envs: vec![],
            headers: vec![("Authorization".into(), "Bearer leftover".into())],
            auth: Some("bearer:${SECRET:tok}".into()),
            connect_timeout_ms: 1,
            call_timeout_ms: 1,
            rate_limit_per_minute: 1,
            rate_limit_burst: 1,
            max_tools: 1,
            client_cert: None,
            client_key: None,
            force: false,
        };
        let err = build_transport(&args).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("--auth") && msg.contains("Authorization"),
            "error must name both knobs to help the operator: {msg}"
        );
    }

    #[test]
    fn build_transport_bearer_alone_succeeds() {
        // Companion positive case: `--auth bearer:…` with no operator
        // Authorization header round-trips into the structured field.
        let args = AddArgs {
            id: "x".into(),
            stdio: None,
            http: Some("https://example.com".into()),
            sse: None,
            args: vec![],
            envs: vec![],
            headers: vec![("X-Other".into(), "kept".into())],
            auth: Some("bearer:${SECRET:tok}".into()),
            connect_timeout_ms: 1,
            call_timeout_ms: 1,
            rate_limit_per_minute: 1,
            rate_limit_burst: 1,
            max_tools: 1,
            client_cert: None,
            client_key: None,
            force: false,
        };
        let t = build_transport(&args).unwrap();
        match t {
            TransportSpec::Http { headers, auth, .. } => {
                assert!(
                    matches!(auth, HttpAuthSpec::Bearer { .. }),
                    "structured Bearer was not populated"
                );
                assert!(
                    headers.contains_key("X-Other"),
                    "unrelated headers must be preserved: {headers:?}"
                );
            }
            _ => panic!("expected Http variant"),
        }
    }

    #[test]
    fn build_transport_bearer_requires_prefix() {
        // Spec: `--auth` is meant to be ergonomic shorthand. We refuse a
        // raw token because silently treating it as Bearer would surprise
        // operators who paste a Basic creds string by mistake.
        let args = AddArgs {
            id: "x".into(),
            stdio: None,
            http: Some("https://example.com".into()),
            sse: None,
            args: vec![],
            envs: vec![],
            headers: vec![],
            auth: Some("just-a-token".into()),
            connect_timeout_ms: 1,
            call_timeout_ms: 1,
            rate_limit_per_minute: 1,
            rate_limit_burst: 1,
            max_tools: 1,
            client_cert: None,
            client_key: None,
            force: false,
        };
        let err = build_transport(&args).unwrap_err();
        assert!(err.to_string().contains("bearer:"));
    }
}
