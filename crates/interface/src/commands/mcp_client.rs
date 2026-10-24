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

use anyhow::{anyhow, bail, Context, Result};
use clap::Args;
use cortex::config::mcp_store::{
    persist_server, preset_server_config, remove_server, set_server_enabled, validate_auth_headers,
    validate_http_url, validate_server_id,
};
use cortex::config::{Config, HttpAuthSpec, McpServerConfig, TransportSpec};
use mcp::manifest::ToolFilter;

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

    /// Extra certificate authorities to trust for this server, as a PEM bundle
    /// (HTTP / SSE only). `~` is expanded. Use this when the server sits behind
    /// a TLS-inspecting proxy whose CA is not in the system trust store. The
    /// bundle is added to the built-in roots, never replacing them.
    #[arg(long)]
    pub ca_cert: Option<String>,

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

    /// Preset name.
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
        "{:<24} {:<14} {:<8} {:<24} {:<32}",
        "NAME", "TRANSPORT", "ENABLED", "TOOLS", "DETAIL"
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
            "{:<24} {:<14} {:<8} {:<24} {:<32}",
            truncate(name, 24),
            transport,
            if enabled { "yes" } else { "no" },
            // `ToolFilter::describe` distinguishes "all tools" from an
            // explicit "no tools" lock-down — the two used to print alike.
            truncate(&s.tools.describe(), 24),
            truncate(&detail, 32)
        );
    }
    Ok(())
}

/// `borg mcp add <id> [--stdio|--http|--sse] …`.
pub(crate) async fn add(args: AddArgs) -> Result<()> {
    validate_server_id(&args.id)?;
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
        cache_tools: true,
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
    validate_server_id(&args.id)?;
    let server = preset_server_config(&args.preset, args.fs_root.as_deref())?;
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
    remove_server(&args.id).await?;
    println!("Removed MCP server `{}`.", args.id);
    Ok(())
}

/// `borg mcp enable|disable <id>`.
pub(crate) async fn enable(args: IdArgs, want_enabled: bool) -> Result<()> {
    set_server_enabled(&args.id, want_enabled).await?;
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
    let report = cortex::config::mcp_store::probe_server(&args.id, server_cfg, &cfg).await?;
    // Print after tools/list so the operator never sees a stale "connected:"
    // banner followed by a downstream error — the printed status reflects
    // what the agent will actually be able to do.
    println!("connected: {}", args.id);
    if let Some(info) = report.server_info.as_deref() {
        println!("server:    {info}");
    }
    println!("tools:     {}", report.tools.len());
    for t in &report.tools {
        println!("  - {t}");
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
    validate_http_url(&url)?;
    // Refuse a conflicting `--auth bearer:…` + `--header Authorization=…`
    // combination outright. Silently dropping one would mask a real
    // operator mistake (e.g. they pasted a Basic credentials string into
    // a header thinking that's how they authenticate). The operator must
    // pick which is authoritative.
    validate_auth_headers(&auth, &headers)?;
    let client_cert = args.client_cert.clone();
    let client_key = args.client_key.clone();
    let ca_cert = args.ca_cert.clone();
    Ok(if is_sse {
        TransportSpec::Sse {
            url,
            headers,
            auth,
            client_cert,
            client_key,
            ca_cert,
        }
    } else {
        TransportSpec::Http {
            url,
            headers,
            auth,
            client_cert,
            client_key,
            ca_cert,
        }
    })
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

#[cfg(test)]
mod tests {
    use super::*;

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
            ca_cert: None,
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
            ca_cert: None,
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
            ca_cert: None,
            force: false,
        };
        let err = build_transport(&args).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("bearer") && msg.contains("Authorization"),
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
            ca_cert: None,
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
            ca_cert: None,
            force: false,
        };
        let err = build_transport(&args).unwrap_err();
        assert!(err.to_string().contains("bearer:"));
    }
}
