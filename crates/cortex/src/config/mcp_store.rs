//! Operator write path for the `mcp.servers` setting.
//!
//! Every surface that adds, edits, or removes an MCP server — the CLI, the
//! daemon's admin RPCs — goes through here, so the id rules, the preset
//! catalog, and the concurrent-write handling live in one place and the row
//! round-trips unchanged through [`McpConfig`](super::McpConfig).

use std::collections::HashMap;

use anyhow::{anyhow, bail, Context, Result};

use super::{HttpAuthSpec, McpServerConfig, TransportSpec};
use crate::db::Database;
use mcp::manifest::ToolFilter;

/// Settings key for the per-server JSON map (mirrors `define_settings!`
/// registration in `settings_table.rs`).
pub const MCP_SERVERS_KEY: &str = "mcp.servers";

/// Preset names [`preset_server_config`] understands, in display order.
pub const MCP_PRESETS: &[&str] = &["time", "filesystem", "github", "notion"];

/// Refuse a server id that would break the tool-name prefix (`mcp_<id>_…`)
/// or leak into structured log fields.
pub fn validate_server_id(id: &str) -> Result<()> {
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

/// Refuse a URL without an explicit HTTP scheme before it reaches the
/// transport, where it fails as an unhelpful connect error.
pub fn validate_http_url(url: &str) -> Result<()> {
    if !url.starts_with("http://") && !url.starts_with("https://") {
        bail!("URL must start with http:// or https://");
    }
    Ok(())
}

/// Refuse a bearer token beside an explicit `Authorization` header. Silently
/// dropping one would mask a real operator mistake; they must pick one.
pub fn validate_auth_headers(auth: &HttpAuthSpec, headers: &HashMap<String, String>) -> Result<()> {
    if matches!(auth, HttpAuthSpec::Bearer { .. })
        && headers
            .keys()
            .any(|k| k.eq_ignore_ascii_case("authorization"))
    {
        bail!(
            "a bearer token and an Authorization header are both set; the bearer token is \
             authoritative — drop the explicit Authorization header to resolve the ambiguity"
        );
    }
    Ok(())
}

/// Catalog of operator-friendly MCP server presets. These are *templates* —
/// adding one writes a row, it does NOT auto-install anything. The first time
/// the server actually runs it pulls the underlying binary via `npx`/`uvx`,
/// so operators control the install moment.
///
/// Keep the list short. These four cover the whole transport matrix while
/// keeping the mental overhead of picking a preset low:
///
/// - `time` exercises stdio with no secrets (smoke test).
/// - `filesystem` exercises stdio with a scoped path argument.
/// - `github` exercises stdio with a `${SECRET:…}` credential.
/// - `notion` exercises remote HTTP (operator must add OAuth headers).
pub fn preset_server_config(name: &str, fs_root: Option<&str>) -> Result<McpServerConfig> {
    struct Preset {
        transport: TransportSpec,
        rate_limit_per_minute: u32,
        rate_limit_burst: u32,
    }
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
                .filter(|r| !r.trim().is_empty())
                .or_else(|| {
                    dirs::home_dir()
                        .map(|h| h.join(".borg").join("mcp").join("fs-scope"))
                        .and_then(|p| p.to_str().map(str::to_string))
                })
                .ok_or_else(|| {
                    anyhow!("filesystem preset needs a root path (could not derive default)")
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
                ca_cert: None,
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
        cache_tools: true,
    })
}

/// Insert `server` under `id`, refusing to replace an existing row unless
/// `overwrite` is set.
pub async fn persist_server(id: &str, server: McpServerConfig, overwrite: bool) -> Result<()> {
    mutate_servers(|map| {
        if map.contains_key(id) && !overwrite {
            bail!("MCP server `{id}` already exists — pass --force to overwrite");
        }
        map.insert(id.to_string(), server.clone());
        Ok(())
    })
    .await
}

/// Drop the row for `id`; an unknown id is an error, not a no-op, so a typo
/// is reported rather than passing as a removal.
pub async fn remove_server(id: &str) -> Result<()> {
    mutate_servers(|map| {
        if map.remove(id).is_none() {
            bail!("no MCP server named `{id}`");
        }
        Ok(())
    })
    .await
}

/// Flip the per-server `enabled` flag.
pub async fn set_server_enabled(id: &str, enabled: bool) -> Result<()> {
    mutate_servers(|map| {
        let entry = map
            .get_mut(id)
            .ok_or_else(|| anyhow!("no MCP server named `{id}`"))?;
        entry.enabled = enabled;
        Ok(())
    })
    .await
}

/// What a one-shot probe of a server learned: its advertised server info,
/// and the names of the tools it lists.
pub struct ProbeReport {
    /// `serverInfo` from the handshake, when the server sent one.
    pub server_info: Option<String>,
    /// Tool names from `tools/list`, in the order the server gave them.
    pub tools: Vec<String>,
}

/// Connect to `server_cfg` once, list its tools, and disconnect — without
/// touching the live registry. Shared by `borg mcp test` and the daemon's
/// `TestMcpServer`, so the two always agree on what "works" means.
pub async fn probe_server(
    name: &str,
    server_cfg: &McpServerConfig,
    config: &super::Config,
) -> Result<ProbeReport> {
    let manifest = server_cfg.resolve_to_manifest(name, config)?;
    let policy = mcp::transport::http::HttpPolicy {
        allow_plaintext_http: config.mcp.allow_plaintext_http,
        allow_local_network: config.mcp.allow_local_network,
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
    let tools = discovered.tools.iter().map(|t| t.name.clone()).collect();
    // The session has no graceful shutdown yet; dropping the actor closes the
    // transport. One yield lets that close run before the caller moves on.
    drop(session);
    tokio::task::yield_now().await;
    Ok(ProbeReport {
        server_info: discovered.server_info,
        tools,
    })
}

/// Optimistic-CAS read-modify-write on the `mcp.servers` JSON setting.
///
/// libsql's WAL mode allows concurrent writers (borgd's settings writer +
/// standalone `borg` CLI processes). A naive read-then-write can lose entries
/// when two writers interleave — both read the same prior JSON, both insert,
/// the later `set_setting` wins, and the earlier entry is silently dropped.
///
/// We avoid that by snapshotting `PRAGMA data_version` before the read; if it
/// has advanced by write time we re-read and re-apply the closure. The closure
/// runs at most `MAX_RETRIES` times — far more than any realistic contention
/// burst on settings — and refuses with a clear error if the database is
/// genuinely write-saturated (operator can retry by hand).
pub async fn mutate_servers<F>(mutator: F) -> Result<()>
where
    F: Fn(&mut HashMap<String, McpServerConfig>) -> Result<()>,
{
    const MAX_RETRIES: u32 = 8;

    let db = Database::open().await.context("open db")?;
    for attempt in 0..MAX_RETRIES {
        let v0 = db
            .data_version()
            .await
            .context("read PRAGMA data_version (pre)")?;
        let current_raw = db
            .get_setting(MCP_SERVERS_KEY)
            .await
            .context("read mcp.servers")?
            .unwrap_or_else(|| "{}".to_string());
        let mut map: HashMap<String, McpServerConfig> = serde_json::from_str(&current_raw)
            .unwrap_or_else(|_| {
                // Round-trip recovery: a malformed setting shouldn't trap the
                // operator. Surface a warning and start from empty rather than
                // silently dropping their last entry — the upcoming write will
                // overwrite whatever was there.
                tracing::warn!(
                    value = %current_raw,
                    "mcp.servers is not valid JSON; treating as empty"
                );
                HashMap::new()
            });
        mutator(&mut map)?;
        let next = serde_json::to_string(&map).context("serialize mcp.servers")?;
        // Re-check the version before committing. If anyone wrote between our
        // read and now, our snapshot is stale — retry with the latest state.
        // This is a soft CAS: it narrows the race window but doesn't close it
        // perfectly. The libsql crate doesn't expose `BEGIN IMMEDIATE` from
        // this surface; a future helper on `Database` should wrap the whole
        // compound operation in one transaction.
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
        db.set_setting(MCP_SERVERS_KEY, &next)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_validation_rejects_empty_and_bad_chars() {
        // The id becomes a URL-like prefix on every tool name (`mcp_<id>_…`)
        // and is concatenated into log/audit fields. Anything outside the
        // safe alphabet would either break the tool-name parser downstream
        // or open an injection vector into structured logs.
        assert!(validate_server_id("ok-1").is_ok());
        assert!(validate_server_id("ok_2").is_ok());
        assert!(validate_server_id("").is_err());
        assert!(validate_server_id("with space").is_err());
        assert!(validate_server_id("with/slash").is_err());
        assert!(validate_server_id("$danger").is_err());
        assert!(validate_server_id(&"a".repeat(65)).is_err());
    }

    #[test]
    fn preset_time_uses_stdio_uvx() {
        // Smoke-test preset: stdio, no env, no secrets — proves the
        // transport plumbing without auth complexity. If a future refactor
        // changes the command or transport shape, this test pins the
        // contract that the smoke preset stays credential-free.
        let cfg = preset_server_config("time", None).unwrap();
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
        let err = preset_server_config("filesystem", home.to_str()).unwrap_err();
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
        let cfg = preset_server_config("github", None).unwrap();
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
        let cfg = preset_server_config("notion", None).unwrap();
        match &cfg.transport {
            TransportSpec::Http { url, .. } => assert!(url.starts_with("https://")),
            _ => panic!("notion preset must be HTTP"),
        }
    }

    #[test]
    fn unknown_preset_errors() {
        let err = preset_server_config("not-a-real-preset", None).unwrap_err();
        assert!(err.to_string().contains("unknown preset"));
    }

    #[test]
    fn bearer_beside_authorization_header_is_refused() {
        let bearer = HttpAuthSpec::Bearer { token: "t".into() };
        let mut headers = HashMap::new();
        assert!(validate_auth_headers(&bearer, &headers).is_ok());
        headers.insert("authorization".into(), "Basic x".into());
        assert!(validate_auth_headers(&bearer, &headers).is_err());
        assert!(validate_auth_headers(&HttpAuthSpec::None, &headers).is_ok());
        assert!(validate_http_url("ftp://x").is_err());
        assert!(validate_http_url("https://x").is_ok());
    }
}
