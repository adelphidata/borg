//! Skill and MCP-server management RPC bodies for the admin service.
//!
//! Skills: the catalog is the loader's full view (disabled and unavailable
//! rows included), reads come from disk or the embedded bundle, and writes go
//! through `cortex::skills::store` so the name and placement rules are the
//! CLI's. MCP servers: rows live in the `mcp.servers` setting via
//! `cortex::config::mcp_store`; a write notifies the config watcher, which
//! reconciles the live registry in the background, because a reload waits
//! on each new server's handshake and a save must not hang for a server
//! that never answers.

use std::collections::HashMap;

use super::*;
use cortex::config::mcp_store;
use cortex::config::{HttpAuthSpec, McpServerConfig, TransportSpec};
use cortex::skills::{Skill, SkillSource, DEFAULT_ENABLED_SKILLS, MANDATORY_SKILLS};
use protocols::admin::{
    McpServerEntry, McpServerList, McpServerPut, McpServerRef, McpServerSpec, McpServerTestResult,
    SkillCatalog, SkillCatalogEntry, SkillDetail, SkillEnabledMutation, SkillPut, SkillRef,
};

// ── Skills ──────────────────────────────────────────────────────────────────

/// Load every skill the loader knows about, off the runtime's worker threads:
/// it walks the skills directory and probes `$PATH` for each declared binary.
async fn load_catalog() -> Result<Vec<Skill>, TStatus> {
    let config = cortex::config::Config::load_from_db()
        .await
        .map_err(map_internal("skills config load"))?;
    tokio::task::spawn_blocking(move || {
        let resolved = config.resolve_credentials();
        cortex::skills::load_all_skills(&resolved, &config.skills, None)
    })
    .await
    .map_err(map_internal("skills load panicked"))?
    .map_err(map_internal("skills load"))
}

fn catalog_entry(skill: &Skill) -> SkillCatalogEntry {
    let name = skill.manifest.name.as_str();
    SkillCatalogEntry {
        name: name.to_string(),
        description: skill.manifest.description.clone(),
        source: match skill.source {
            SkillSource::BuiltIn => "builtin",
            SkillSource::User => "user",
            SkillSource::Project => "project",
        }
        .to_string(),
        enabled: !skill.disabled,
        available: skill.available,
        mandatory: MANDATORY_SKILLS.contains(&name),
        category: skill.category().to_string(),
        default_enabled: DEFAULT_ENABLED_SKILLS.contains(&name),
    }
}

fn require_name(name: &str) -> Result<(), TStatus> {
    if name.trim().is_empty() {
        return Err(TStatus::invalid_argument("skill name is required"));
    }
    Ok(())
}

async fn skill_detail(name: &str) -> Result<SkillDetail, TStatus> {
    let skills = load_catalog().await?;
    let skill = skills
        .iter()
        .find(|s| s.manifest.name == name)
        .ok_or_else(|| TStatus::not_found(format!("no skill named `{name}`")))?;
    let entry = catalog_entry(skill);
    let owned = name.to_string();
    let (content, path) = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        let content = cortex::skills::skill_source(&owned)?.unwrap_or_default();
        let path = match cortex::skills::skills_dir() {
            Ok(root) => cortex::skills::user_skill_dir(&root, &owned)
                .map(|d| d.join("SKILL.md").display().to_string())
                .unwrap_or_default(),
            Err(_) => String::new(),
        };
        Ok((content, path))
    })
    .await
    .map_err(map_internal("skill read panicked"))?
    .map_err(map_internal("skill read"))?;
    Ok(SkillDetail {
        entry: Some(entry),
        content,
        // A built-in has no file of its own; the path is only meaningful for
        // a skill the user can open in an editor.
        path: if matches!(skill.source, SkillSource::BuiltIn) {
            String::new()
        } else {
            path
        },
    })
}

pub(super) async fn list_skill_catalog(
    _req: Request<Empty>,
) -> Result<Response<SkillCatalog>, TStatus> {
    let mut skills: Vec<SkillCatalogEntry> = load_catalog()
        .await?
        .iter()
        .filter(|s| !s.is_hidden())
        .map(catalog_entry)
        .collect();
    skills.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(Response::new(SkillCatalog { skills }))
}

pub(super) async fn get_skill(req: Request<SkillRef>) -> Result<Response<SkillDetail>, TStatus> {
    let name = req.into_inner().name;
    require_name(&name)?;
    Ok(Response::new(skill_detail(&name).await?))
}

/// A store failure as a status: the user's input is `InvalidArgument`, a
/// disk that refused is `Internal`, so the UI does not tell someone to fix
/// their text when the volume is full.
fn store_status(e: anyhow::Error) -> TStatus {
    let text = format!("{e:#}");
    if e.chain().any(<dyn std::error::Error>::is::<std::io::Error>) {
        TStatus::internal(text)
    } else {
        TStatus::invalid_argument(text)
    }
}

pub(super) async fn put_skill(
    svc: &AdminSvc,
    req: Request<SkillPut>,
) -> Result<Response<SkillDetail>, TStatus> {
    let SkillPut { name, content } = req.into_inner();
    require_name(&name)?;
    if content.trim().is_empty() {
        return Err(TStatus::invalid_argument("SKILL.md content is required"));
    }
    let root = cortex::skills::skills_dir().map_err(map_internal("skills dir"))?;
    let is_new = cortex::skills::user_skill_dir(&root, &name).is_none();
    // The loader keeps a user skill off until it is switched on. Someone who
    // just wrote one here wants it read, so a new skill starts on; an edit
    // leaves whatever switch position it already had. The switch is written
    // first: a file without it would read as "saved but off" on a retry.
    let key = format!("skills.entries.{name}.enabled");
    if is_new {
        cortex::skills::validate_user_skill_name(&name).map_err(store_status)?;
        let resolver = open_settings_resolver().await?;
        resolver
            .set(&key, "true")
            .await
            .map_err(|e| TStatus::internal(format!("set `{key}` failed: {e}")))?;
    }
    let write_name = name.clone();
    let write_root = root.clone();
    let written = tokio::task::spawn_blocking(move || {
        cortex::skills::write_user_skill_in(&write_root, &write_name, &content)
    })
    .await
    .map_err(map_internal("skill write panicked"))?;
    if let Err(e) = written {
        if is_new {
            // Nothing landed on disk, so the switch must not stay behind.
            if let Ok(resolver) = open_settings_resolver().await {
                let _ = resolver.unset(&key).await;
            }
        }
        return Err(store_status(e));
    }
    // Live sessions cache their skills prompt; the watcher tick clears it.
    notify_runtime_config_reload(&svc.config_notify_tx).await;
    Ok(Response::new(skill_detail(&name).await?))
}

pub(super) async fn delete_skill(
    svc: &AdminSvc,
    req: Request<SkillRef>,
) -> Result<Response<Empty>, TStatus> {
    let name = req.into_inner().name;
    require_name(&name)?;
    let del_name = name.clone();
    tokio::task::spawn_blocking(move || cortex::skills::delete_user_skill(&del_name))
        .await
        .map_err(map_internal("skill delete panicked"))?
        .map_err(|e| {
            if format!("{e:#}").contains("no user skill named") {
                TStatus::not_found(format!("{e:#}"))
            } else {
                store_status(e)
            }
        })?;
    notify_runtime_config_reload(&svc.config_notify_tx).await;
    Ok(Response::new(Empty {}))
}

pub(super) async fn set_skill_enabled(
    svc: &AdminSvc,
    req: Request<SkillEnabledMutation>,
) -> Result<Response<SkillDetail>, TStatus> {
    let SkillEnabledMutation { name, enabled } = req.into_inner();
    require_name(&name)?;
    // Validate the skill exists so a typo'd name never lands a
    // `skills.entries.<typo>.enabled` row that rides forward forever.
    let skills = load_catalog().await?;
    if !skills.iter().any(|s| s.manifest.name == name) {
        return Err(TStatus::not_found(format!("no skill named `{name}`")));
    }
    if !enabled && MANDATORY_SKILLS.contains(&name.as_str()) {
        return Err(TStatus::failed_precondition(format!(
            "`{name}` is always on: it teaches a tool that is always present"
        )));
    }
    let resolver = open_settings_resolver().await?;
    let key = format!("skills.entries.{name}.enabled");
    resolver
        .set(&key, &enabled.to_string())
        .await
        .map_err(|e| TStatus::invalid_argument(format!("set `{key}` failed: {e}")))?;
    notify_runtime_config_reload(&svc.config_notify_tx).await;
    Ok(Response::new(skill_detail(&name).await?))
}

// ── MCP servers ─────────────────────────────────────────────────────────────

fn spec_of(cfg: &McpServerConfig) -> McpServerSpec {
    let mut spec = McpServerSpec {
        enabled: cfg.enabled,
        connect_timeout_ms: cfg.connect_timeout_ms,
        call_timeout_ms: cfg.call_timeout_ms,
        rate_limit_per_minute: cfg.rate_limit_per_minute,
        rate_limit_burst: cfg.rate_limit_burst,
        max_tools: cfg.tools.max as u32,
        ..Default::default()
    };
    match &cfg.transport {
        TransportSpec::Stdio { command, args, env } => {
            spec.transport = "stdio".into();
            spec.command = command.clone();
            spec.args = args.clone();
            spec.env = env.clone();
        }
        TransportSpec::Http {
            url,
            headers,
            auth,
            client_cert,
            client_key,
            ca_cert,
        }
        | TransportSpec::Sse {
            url,
            headers,
            auth,
            client_cert,
            client_key,
            ca_cert,
        } => {
            spec.transport = if matches!(cfg.transport, TransportSpec::Http { .. }) {
                "http".into()
            } else {
                "sse".into()
            };
            spec.url = url.clone();
            spec.headers = headers.clone();
            spec.bearer_token = match auth {
                HttpAuthSpec::Bearer { token } => token.clone(),
                HttpAuthSpec::None => String::new(),
            };
            spec.client_cert = client_cert.clone().unwrap_or_default();
            spec.client_key = client_key.clone().unwrap_or_default();
            spec.ca_cert = ca_cert.clone().unwrap_or_default();
        }
    }
    spec
}

fn detail_of(cfg: &McpServerConfig) -> String {
    match &cfg.transport {
        TransportSpec::Stdio { command, args, .. } => {
            format!("{} {}", command, args.join(" ")).trim().to_string()
        }
        TransportSpec::Http { url, .. } | TransportSpec::Sse { url, .. } => url.clone(),
    }
}

fn opt_path(s: &str) -> Option<String> {
    let t = s.trim();
    (!t.is_empty()).then(|| t.to_string())
}

/// Turn the wire spec into a config row. `existing` supplies the tool
/// include/exclude lists the spec does not carry, so an edit from a client
/// that never saw them does not wipe them.
fn config_of(
    spec: &McpServerSpec,
    existing: Option<&McpServerConfig>,
) -> anyhow::Result<McpServerConfig> {
    use anyhow::bail;
    let transport = match spec.transport.as_str() {
        "stdio" => {
            let command = spec.command.trim();
            if command.is_empty() {
                bail!("a command is required for a stdio server");
            }
            TransportSpec::Stdio {
                command: command.to_string(),
                args: spec.args.clone(),
                env: spec.env.clone(),
            }
        }
        kind @ ("http" | "sse") => {
            let url = spec.url.trim().to_string();
            mcp_store::validate_http_url(&url)?;
            let auth = match spec.bearer_token.trim() {
                "" => HttpAuthSpec::None,
                token => HttpAuthSpec::Bearer {
                    token: token.to_string(),
                },
            };
            mcp_store::validate_auth_headers(&auth, &spec.headers)?;
            let client_cert = opt_path(&spec.client_cert);
            let client_key = opt_path(&spec.client_key);
            if client_key.is_some() && client_cert.is_none() {
                bail!("a client key needs a client certificate");
            }
            let ca_cert = opt_path(&spec.ca_cert);
            let headers = spec.headers.clone();
            if kind == "http" {
                TransportSpec::Http {
                    url,
                    headers,
                    auth,
                    client_cert,
                    client_key,
                    ca_cert,
                }
            } else {
                TransportSpec::Sse {
                    url,
                    headers,
                    auth,
                    client_cert,
                    client_key,
                    ca_cert,
                }
            }
        }
        other => bail!("transport must be stdio, http, or sse (got `{other}`)"),
    };
    let defaults = existing.cloned().unwrap_or_else(|| McpServerConfig {
        enabled: true,
        transport: transport.clone(),
        connect_timeout_ms: 60_000,
        call_timeout_ms: 120_000,
        rate_limit_per_minute: 60,
        rate_limit_burst: 10,
        tools: mcp::manifest::ToolFilter::default(),
        cache_tools: true,
    });
    let pick_u64 = |v: u64, d: u64| if v == 0 { d } else { v };
    let pick_u32 = |v: u32, d: u32| if v == 0 { d } else { v };
    let mut tools = defaults.tools.clone();
    tools.max = pick_u32(spec.max_tools, tools.max as u32) as usize;
    Ok(McpServerConfig {
        enabled: spec.enabled,
        transport,
        connect_timeout_ms: pick_u64(spec.connect_timeout_ms, defaults.connect_timeout_ms),
        call_timeout_ms: pick_u64(spec.call_timeout_ms, defaults.call_timeout_ms),
        rate_limit_per_minute: pick_u32(spec.rate_limit_per_minute, defaults.rate_limit_per_minute),
        rate_limit_burst: pick_u32(spec.rate_limit_burst, defaults.rate_limit_burst),
        tools,
        cache_tools: defaults.cache_tools,
    })
}

fn entries_of(config: &cortex::config::Config) -> Vec<McpServerEntry> {
    let live: HashMap<String, _> = cortex::mcp_clients::server_status_snapshot(config)
        .into_iter()
        .map(|s| (s.name.clone(), s))
        .collect();
    let mut out: Vec<McpServerEntry> = config
        .mcp
        .servers
        .iter()
        .map(|(name, cfg)| {
            let snap = live.get(name);
            McpServerEntry {
                name: name.clone(),
                spec: Some(spec_of(cfg)),
                state: snap
                    .map(cortex::mcp_clients::McpServerSnapshot::state_label)
                    .unwrap_or("not-started")
                    .to_string(),
                tool_count: snap.map(|s| s.tool_count as u32).unwrap_or(0),
                detail: detail_of(cfg),
            }
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

pub(super) async fn list_mcp_servers(
    _req: Request<Empty>,
) -> Result<Response<McpServerList>, TStatus> {
    let config = cortex::config::Config::load_from_db()
        .await
        .map_err(map_internal("mcp config load"))?;
    Ok(Response::new(McpServerList {
        servers: entries_of(&config),
        mcp_enabled: config.mcp.enabled,
        presets: mcp_store::MCP_PRESETS
            .iter()
            .map(|p| (*p).to_string())
            .collect(),
    }))
}

pub(super) async fn put_mcp_server(
    svc: &AdminSvc,
    req: Request<McpServerPut>,
) -> Result<Response<McpServerEntry>, TStatus> {
    let req = req.into_inner();
    let name = req.name.trim().to_string();
    mcp_store::validate_server_id(&name).map_err(|e| TStatus::invalid_argument(e.to_string()))?;
    // Validate the shape before touching the store, so a bad spec is an
    // `InvalidArgument` and never a retry of the CAS loop.
    enum Source {
        Preset(McpServerConfig),
        Spec(McpServerSpec),
    }
    let source = if !req.preset.trim().is_empty() {
        let fs_root = opt_path(&req.fs_root);
        Source::Preset(
            mcp_store::preset_server_config(req.preset.trim(), fs_root.as_deref())
                .map_err(|e| TStatus::invalid_argument(e.to_string()))?,
        )
    } else {
        let spec = req
            .spec
            .ok_or_else(|| TStatus::invalid_argument("a server spec or a preset is required"))?;
        config_of(&spec, None).map_err(|e| TStatus::invalid_argument(e.to_string()))?;
        Source::Spec(spec)
    };
    // The existence check and the merge with the stored row happen inside
    // the CAS closure, against the map that is actually written: two
    // creates with one name cannot both pass, and a CLI edit to the tool
    // lists between read and write is kept.
    const EXISTS: &str = "already exists";
    let stored: std::sync::Mutex<Option<McpServerConfig>> = std::sync::Mutex::new(None);
    let overwrite = req.overwrite;
    let result = mcp_store::mutate_servers(|map| {
        let existing = map.get(&name);
        if existing.is_some() && !overwrite {
            anyhow::bail!("an MCP server named `{name}` {EXISTS}");
        }
        let row = match &source {
            Source::Preset(row) => row.clone(),
            Source::Spec(spec) => config_of(spec, existing)?,
        };
        map.insert(name.clone(), row.clone());
        *stored
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(row);
        Ok(())
    })
    .await;
    if let Err(e) = result {
        let text = e.to_string();
        return Err(if text.contains(EXISTS) {
            TStatus::already_exists(text)
        } else {
            TStatus::internal(text)
        });
    }
    let row = stored
        .into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .ok_or_else(|| TStatus::internal("mcp.servers write left no row"))?;
    // The config watcher reconciles the live registry on this notify, so a
    // new server starts in the background while this call returns.
    notify_runtime_config_reload(&svc.config_notify_tx).await;
    Ok(Response::new(McpServerEntry {
        name,
        spec: Some(spec_of(&row)),
        state: if row.enabled { "starting" } else { "disabled" }.to_string(),
        tool_count: 0,
        detail: detail_of(&row),
    }))
}

pub(super) async fn remove_mcp_server(
    svc: &AdminSvc,
    req: Request<McpServerRef>,
) -> Result<Response<Empty>, TStatus> {
    let name = req.into_inner().name;
    mcp_store::remove_server(&name).await.map_err(|e| {
        let text = e.to_string();
        if text.contains("no MCP server named") {
            TStatus::not_found(text)
        } else {
            TStatus::internal(text)
        }
    })?;
    notify_runtime_config_reload(&svc.config_notify_tx).await;
    Ok(Response::new(Empty {}))
}

/// One-shot connect + `tools/list` against the stored row, bypassing the
/// live registry, so a freshly saved server can be checked without waiting
/// for the background reload. A failure is an answer, not an RPC error.
pub(super) async fn test_mcp_server(
    req: Request<McpServerRef>,
) -> Result<Response<McpServerTestResult>, TStatus> {
    let name = req.into_inner().name;
    let config = cortex::config::Config::load_from_db()
        .await
        .map_err(map_internal("mcp config load"))?;
    let Some(server_cfg) = config.mcp.servers.get(&name) else {
        return Err(TStatus::not_found(format!("no MCP server named `{name}`")));
    };
    let outcome = mcp_store::probe_server(&name, server_cfg, &config).await;
    Ok(Response::new(match outcome {
        Ok(report) => McpServerTestResult {
            ok: true,
            error: String::new(),
            server_info: report.server_info.unwrap_or_default(),
            tools: report.tools,
        },
        Err(e) => McpServerTestResult {
            ok: false,
            error: format!("{e:#}"),
            server_info: String::new(),
            tools: Vec::new(),
        },
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stdio_row() -> McpServerConfig {
        McpServerConfig {
            enabled: true,
            transport: TransportSpec::Stdio {
                command: "uvx".into(),
                args: vec!["mcp-server-time".into()],
                env: HashMap::from([("A".to_string(), "1".to_string())]),
            },
            connect_timeout_ms: 1_000,
            call_timeout_ms: 2_000,
            rate_limit_per_minute: 7,
            rate_limit_burst: 3,
            tools: mcp::manifest::ToolFilter {
                include: Some(vec!["only_this".into()]),
                exclude: vec![],
                read_only: vec!["only_this".into()],
                max: 12,
            },
            cache_tools: false,
        }
    }

    #[test]
    fn spec_round_trips_through_config_and_keeps_tool_filters() {
        // The wire spec carries no include/exclude lists, so an edit that
        // round-trips through it must leave the stored filters as they were.
        let row = stdio_row();
        let spec = spec_of(&row);
        assert_eq!(spec.transport, "stdio");
        assert_eq!(spec.max_tools, 12);
        let back = config_of(&spec, Some(&row)).unwrap();
        assert_eq!(back.tools.include, row.tools.include);
        assert_eq!(back.tools.read_only, row.tools.read_only);
        assert_eq!(back.connect_timeout_ms, 1_000);
        assert!(
            !back.cache_tools,
            "cache_tools is not on the wire; keep the row's"
        );
        assert_eq!(detail_of(&back), "uvx mcp-server-time");
    }

    #[test]
    fn zero_knobs_fall_back_to_defaults_and_bad_specs_are_refused() {
        let mut spec = McpServerSpec {
            enabled: true,
            transport: "http".into(),
            url: "https://example.com/mcp".into(),
            bearer_token: "${SECRET:tok}".into(),
            ..Default::default()
        };
        let cfg = config_of(&spec, None).unwrap();
        assert_eq!(cfg.connect_timeout_ms, 60_000);
        assert_eq!(cfg.rate_limit_per_minute, 60);
        assert!(matches!(
            cfg.transport,
            TransportSpec::Http {
                auth: HttpAuthSpec::Bearer { .. },
                ..
            }
        ));
        // Bearer beside an explicit Authorization header is ambiguous.
        spec.headers
            .insert("Authorization".into(), "Basic x".into());
        assert!(config_of(&spec, None).is_err());
        spec.headers.clear();
        spec.url = "example.com".into();
        assert!(config_of(&spec, None).is_err());
        spec.transport = "stdio".into();
        assert!(config_of(&spec, None).is_err(), "stdio needs a command");
        spec.transport = "carrier-pigeon".into();
        assert!(config_of(&spec, None).is_err());
    }
}
