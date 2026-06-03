use anyhow::Result;

use crate::config::security::KekBackend;
use crate::config::tunnel::TunnelProvider;
use crate::config::Config;
use crate::constants::{IDENTITY_FILE, SECS_PER_DAY};
use crate::db::Database;
use crate::llm::provider::Provider;
use crate::process_registry::ProcessRegistry;
use crate::skills::load_all_skills;

pub mod repair;
mod types;
pub use types::{CheckStatus, DiagnosticCheck, DiagnosticReport, RemediationKind};

/// Stepped diagnostic runner that yields one check category at a time.
/// Used by the TUI to show progressive output.
#[derive(Default)]
pub struct DiagnosticRunner {
    /// Current cursor into [`STEPS`].
    step: usize,
}

/// Max acceptable gap between memory entries and indexed embeddings before
/// the doctor escalates to a warning. Below this threshold the gap is the
/// normal write/embed race window — the next embed pass closes it.
const EMBEDDING_LAG_WARN_THRESHOLD: usize = 5;

/// The ordered diagnostic groups. Adding a check is one entry here plus the
/// matching arm in [`StepKind::run`] — no parallel index switch to keep in
/// sync. Order is operator-visible in `borg doctor` output.
const STEPS: &[StepKind] = &[
    StepKind::Provider,
    StepKind::Credentials,
    StepKind::Settings,
    StepKind::Secrets,
    StepKind::Keychain,
    StepKind::Sandbox,
    StepKind::Skills,
    StepKind::Memory,
    StepKind::Embeddings,
    StepKind::Data,
    StepKind::Locks,
    StepKind::Gateway,
    StepKind::Tunnel,
    StepKind::Heartbeat,
    StepKind::ScheduledTasks,
    StepKind::Service,
    StepKind::Budget,
    StepKind::PromptCache,
    StepKind::Plugins,
    StepKind::Mcp,
    StepKind::Docker,
    StepKind::Browser,
    StepKind::Agents,
    StepKind::Security,
    StepKind::SupplyChain,
    StepKind::HostAudit,
];

/// Re-exported for the orphan-lock doctor check and `--repair`. The threshold
/// and its single definition live with the lock mechanism in
/// [`crate::constants`]; `acquire_daemon_lock` / `is_daemon_lock_held` use the
/// same constant, so detection and takeover grace can't drift.
pub use crate::constants::DAEMON_LOCK_STALE_SECS;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StepKind {
    Provider,
    Credentials,
    Settings,
    Secrets,
    Keychain,
    Sandbox,
    Skills,
    Memory,
    Embeddings,
    Data,
    Locks,
    Gateway,
    Tunnel,
    Heartbeat,
    ScheduledTasks,
    Service,
    Budget,
    PromptCache,
    Plugins,
    Mcp,
    Docker,
    Browser,
    Agents,
    Security,
    SupplyChain,
    HostAudit,
}

impl StepKind {
    /// Operator-visible label rendered in the TUI / CLI report.
    const fn label(self) -> &'static str {
        match self {
            Self::Provider => "Provider",
            Self::Credentials => "Credentials",
            Self::Settings => "Settings",
            Self::Secrets => "Secrets",
            Self::Keychain => "Keychain",
            Self::Sandbox => "Sandbox",
            Self::Skills => "Skills",
            Self::Memory => "Memory",
            Self::Embeddings => "Embeddings",
            Self::Data => "Data",
            Self::Locks => "Locks",
            Self::Gateway => "Gateway",
            Self::Tunnel => "Tunnel",
            Self::Heartbeat => "Heartbeat",
            Self::ScheduledTasks => "Scheduled Tasks",
            Self::Service => "Service",
            Self::Budget => "Budget",
            Self::PromptCache => "Prompt Cache",
            Self::Plugins => "Plugins",
            Self::Mcp => "MCP",
            Self::Docker => "Docker",
            Self::Browser => "Browser",
            Self::Agents => "Agents",
            Self::Security => "Security",
            Self::SupplyChain => "Supply Chain",
            Self::HostAudit => "Host Audit",
        }
    }

    /// Whether this step runs for the given config. Returning `false` skips
    /// the step entirely — `next_step` never surfaces it.
    fn enabled(self, config: &Config) -> bool {
        match self {
            Self::HostAudit => config.security.host_audit,
            _ => true,
        }
    }

    /// When `true`, a step that produced zero `DiagnosticCheck` rows is
    /// dropped silently (no header in the output). Host Audit uses this
    /// because "platform doesn't apply" yields an empty result on macOS
    /// and printing a bare header would mislead operators.
    const fn suppress_when_empty(self) -> bool {
        // These checks are applicability-gated: when the subsystem isn't
        // configured/in-use (no MCP servers, no tunnel provider, docker not
        // enabled, no orphan lock, no key-based provider, service not
        // installed) the check fn emits zero rows and we drop the header
        // entirely — never a false "problem" on a vanilla install.
        matches!(
            self,
            Self::HostAudit
                | Self::Credentials
                | Self::Locks
                | Self::Tunnel
                | Self::Service
                | Self::Mcp
                | Self::Docker
        )
    }

    async fn run(self, config: &Config, checks: &mut Vec<DiagnosticCheck>) {
        match self {
            Self::Provider => check_provider(config, checks),
            Self::Credentials => check_llm_credentials(config, checks).await,
            Self::Settings => check_settings(config, checks),
            Self::Secrets => check_secrets(config, checks),
            Self::Keychain => check_keychain(config, checks),
            Self::Sandbox => check_sandbox(checks),
            Self::Skills => check_skills(config, checks).await,
            Self::Memory => check_memory(checks).await,
            Self::Embeddings => check_embeddings(config, checks).await,
            Self::Data => check_data_dir(checks).await,
            Self::Locks => check_daemon_lock(checks).await,
            Self::Gateway => check_gateway(config, checks),
            Self::Tunnel => check_tunnel(config, checks),
            Self::Heartbeat => check_heartbeat(config, checks).await,
            Self::ScheduledTasks => check_scheduled_tasks(checks).await,
            Self::Service => check_service(checks),
            Self::Budget => check_budget(config, checks).await,
            Self::PromptCache => check_prompt_cache(config, checks).await,
            Self::Plugins => check_plugins(checks).await,
            Self::Mcp => check_mcp(config, checks),
            Self::Docker => check_docker(config, checks),
            Self::Browser => check_browser(config, checks),
            Self::Agents => check_agents(config, checks).await,
            Self::Security => check_config_security(config, checks),
            Self::SupplyChain => check_supply_chain(checks),
            Self::HostAudit => crate::host_audit::run_host_security_checks(checks),
        }
    }
}

impl DiagnosticRunner {
    /// Create a new diagnostic runner starting at step 0.
    pub fn new() -> Self {
        Self::default()
    }

    /// Run the next enabled step. Returns `None` when all steps are done.
    pub async fn next_step(
        &mut self,
        config: &Config,
    ) -> Option<(&'static str, Vec<DiagnosticCheck>)> {
        while self.step < STEPS.len() {
            let kind = STEPS[self.step];
            self.step += 1;
            if !kind.enabled(config) {
                continue;
            }
            let mut checks = Vec::new();
            kind.run(config, &mut checks).await;
            if kind.suppress_when_empty() && checks.is_empty() {
                continue;
            }
            return Some((kind.label(), checks));
        }
        None
    }
}

/// Run all diagnostic checks and return a report. Equivalent to
/// [`run_diagnostics_with_registry`] with `None` — preserved for call
/// sites (`borg doctor` CLI, tests) that don't have a process registry
/// handle.
pub async fn run_diagnostics(config: &Config) -> DiagnosticReport {
    run_diagnostics_with_registry(config, None).await
}

/// Run all diagnostic checks plus the `Subprocesses` check when the
/// caller has a live registry. The daemon's maintenance sweep and
/// `Admin.Heal` RPC use this variant.
pub async fn run_diagnostics_with_registry(
    config: &Config,
    registry: Option<&ProcessRegistry>,
) -> DiagnosticReport {
    let mut runner = DiagnosticRunner::new();
    let mut checks = Vec::new();
    while let Some((_label, step_checks)) = runner.next_step(config).await {
        checks.extend(step_checks);
    }
    if let Some(reg) = registry {
        check_subprocesses(config, reg, &mut checks);
    }
    DiagnosticReport { checks }
}

/// Subprocess registry health: surfaces `Warn`/`Fail` when one or more
/// non-pinned children have been running past the configured age
/// thresholds. Squelchable via
/// `/doctor squelch Subprocesses:Long-running children`.
fn check_subprocesses(
    config: &Config,
    registry: &ProcessRegistry,
    checks: &mut Vec<DiagnosticCheck>,
) {
    let now = chrono::Utc::now().timestamp();
    let class = registry.classify(
        now,
        config.process_registry.orphan_warn_secs as i64,
        config.process_registry.orphan_fail_secs as i64,
    );
    let hint =
        "Inspect: `borg status`. Pin the row in `/status` (press `p`) or kill: `kill <pid>`.";
    if !class.failing.is_empty() {
        let msg = format_subprocess_offenders(&class.failing, now);
        let mut check = DiagnosticCheck::fail("Subprocesses", "Long-running children", msg);
        check.remediation_kind = RemediationKind::Manual;
        check.remediation_key = Some("subprocesses.long_running");
        check.user_hint = Some(hint.to_string());
        checks.push(check);
    } else if !class.warning.is_empty() {
        let msg = format_subprocess_offenders(&class.warning, now);
        let mut check = DiagnosticCheck::warn("Subprocesses", "Long-running children", msg);
        check.remediation_kind = RemediationKind::Manual;
        check.remediation_key = Some("subprocesses.long_running");
        check.user_hint = Some(hint.to_string());
        checks.push(check);
    } else {
        checks.push(DiagnosticCheck::pass(
            "Subprocesses",
            "Long-running children",
        ));
    }
}

/// Compare each shipped dependency against the bundled advisory list. A
/// single check row per advisory hit, so operators can squelch noisy entries
/// one at a time. No I/O — both the lockfile and the advisory list are
/// `include_str!`-embedded at compile time.
fn check_supply_chain(checks: &mut Vec<DiagnosticCheck>) {
    let hits = crate::security::advisory::scan_embedded();
    if hits.is_empty() {
        checks.push(DiagnosticCheck::pass(
            "Supply Chain",
            "Dependency advisories",
        ));
        return;
    }
    for hit in hits {
        let name = format!(
            "{} {} ({})",
            hit.advisory.crate_name, hit.installed_version, hit.advisory.id
        );
        let msg = hit.advisory.summary.clone();
        let mut check = if hit.advisory.severity.is_failing() {
            DiagnosticCheck::fail("Supply Chain", name, msg)
        } else {
            DiagnosticCheck::warn("Supply Chain", name, msg)
        };
        check.user_hint = Some(hit.advisory.fix.clone());
        check.remediation_kind = RemediationKind::Manual;
        check.remediation_key = Some("supply_chain.advisory");
        checks.push(check);
    }
}

/// Render up to 3 offenders by descending age; collapse the rest into
/// "… and N more" so a runaway registry doesn't produce a wall-of-text
/// warning in `borg doctor` or persistent warnings.
fn format_subprocess_offenders(
    entries: &[crate::process_registry::ProcessEntry],
    now: i64,
) -> String {
    const MAX_NAMED: usize = 3;
    let mut parts: Vec<String> = entries
        .iter()
        .take(MAX_NAMED)
        .map(|e| {
            let age = now.saturating_sub(e.started_at).max(0);
            format!(
                "{} pid={} age={}s ({})",
                e.kind.tag(),
                e.pid,
                age,
                e.command
            )
        })
        .collect();
    if entries.len() > MAX_NAMED {
        parts.push(format!("… and {} more", entries.len() - MAX_NAMED));
    }
    format!(
        "{} long-running child(ren): {}",
        entries.len(),
        parts.join("; ")
    )
}

fn check_provider(config: &Config, checks: &mut Vec<DiagnosticCheck>) {
    match config.resolve_provider() {
        Ok((provider, _key)) => {
            if provider == Provider::ClaudeCli {
                if let Some(path) = crate::claude_cli::detect_cli_path() {
                    checks.push(DiagnosticCheck::pass(
                        "Provider",
                        format!("Claude CLI found at {}", path.display()),
                    ));
                } else {
                    checks.push(DiagnosticCheck::fail(
                        "Provider",
                        "Claude CLI binary",
                        "not found — install Claude Code or set CLAUDE_CLI_PATH",
                    ));
                }
                if crate::claude_cli::has_valid_auth() {
                    checks.push(DiagnosticCheck::pass(
                        "Provider",
                        "Claude CLI authenticated",
                    ));
                } else {
                    checks.push(DiagnosticCheck::warn(
                        "Provider",
                        "Claude CLI auth",
                        "not authenticated or expired — run `claude login`",
                    ));
                }
            } else if provider.requires_api_key() {
                checks.push(DiagnosticCheck::pass("Provider", "API key set"));
            } else if Provider::ollama_available() {
                checks.push(DiagnosticCheck::pass("Provider", "Ollama server reachable"));
            } else {
                checks.push(DiagnosticCheck::warn(
                    "Provider",
                    "Ollama server",
                    format!(
                        "not reachable at localhost:{} — run `ollama serve`",
                        crate::constants::OLLAMA_PORT_DEFAULT
                    ),
                ));
            }
            checks.push(DiagnosticCheck::pass(
                "Provider",
                format!("Provider: {provider}"),
            ));
            check_model_deprecation(provider, &config.llm.model, checks);
        }
        Err(e) => {
            checks.push(DiagnosticCheck::fail(
                "Provider",
                "API key set",
                format!("{e}"),
            ));
        }
    }

    // Also report Claude CLI availability even if not the active provider
    if config
        .llm
        .provider
        .as_deref()
        .is_none_or(|p| p != "claude-cli")
        && crate::claude_cli::detect_cli_path().is_some()
        && crate::claude_cli::has_valid_auth()
    {
        checks.push(DiagnosticCheck::pass(
            "Provider",
            "Claude CLI available (alternative provider)",
        ));
    }
}

/// If the active `(provider, model)` is in the deprecation table, emit a
/// warning that names the retirement date and recommended replacement so
/// operators can migrate before the upstream provider starts rejecting
/// requests.
fn check_model_deprecation(provider: Provider, model: &str, checks: &mut Vec<DiagnosticCheck>) {
    let Some(entry) = crate::llm::deprecations::lookup(provider, model) else {
        return;
    };
    let msg = format!(
        "{} retires on {} — switch to `{}` before that date",
        entry.model, entry.retires_on, entry.replacement,
    );
    let mut check = DiagnosticCheck::warn("Provider", "Model retirement", msg);
    check.user_hint = Some(format!(
        "Set `llm.model` to `{}`. Provider advisory: {}",
        entry.replacement, entry.advisory_url
    ));
    check.remediation_kind = RemediationKind::Manual;
    check.remediation_key = Some("provider.model_retirement");
    checks.push(check);
}

fn check_secrets(config: &Config, checks: &mut Vec<DiagnosticCheck>) {
    if config.llm.api_key.is_some() {
        checks.push(DiagnosticCheck::pass("Secrets", "API key via SecretRef"));
    }

    if let Ok(data_dir) = Config::data_dir() {
        let env_path = data_dir.join(".env");
        if env_path.exists() {
            match std::fs::read_to_string(&env_path) {
                Ok(contents) => {
                    let has_plaintext_keys = contents.lines().any(|line| {
                        let trimmed = line.trim();
                        !trimmed.is_empty()
                            && !trimmed.starts_with('#')
                            && trimmed.contains('=')
                            && (trimmed.contains("API_KEY") || trimmed.contains("api_key"))
                    });

                    if has_plaintext_keys {
                        let hint = if config.llm.api_key.is_some() {
                            "plaintext .env still present — consider removing it"
                        } else {
                            "plaintext API keys in .env — move them into the OS keychain and set llm.api_key via a SecretRef keychain source in settings"
                        };
                        checks.push(DiagnosticCheck::warn(
                            "Secrets",
                            "plaintext keys in .env",
                            hint,
                        ));
                    } else {
                        checks.push(DiagnosticCheck::pass(
                            "Secrets",
                            "no plaintext API keys in .env",
                        ));
                    }
                }
                Err(_) => {
                    checks.push(DiagnosticCheck::warn(
                        "Secrets",
                        ".env readable",
                        "could not read .env file",
                    ));
                }
            }

            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                if let Ok(meta) = std::fs::metadata(&env_path) {
                    let mode = meta.mode() & 0o777;
                    if mode & 0o077 != 0 {
                        checks.push(DiagnosticCheck::warn(
                            "Secrets",
                            ".env file permissions",
                            format!("permissions are {mode:04o} — should be 0600 or stricter"),
                        ));
                    } else {
                        checks.push(DiagnosticCheck::pass("Secrets", ".env file permissions"));
                    }
                }
            }
        }
    }

    if !config.llm.api_keys.is_empty() {
        let resolved_count = config
            .llm
            .api_keys
            .iter()
            .filter(|sr| sr.resolve().is_ok())
            .count();
        let name = format!(
            "multi-key fallback: {resolved_count}/{} keys resolvable",
            config.llm.api_keys.len()
        );
        if resolved_count > 0 {
            checks.push(DiagnosticCheck::pass("Secrets", name));
        } else {
            checks.push(DiagnosticCheck::warn(
                "Secrets",
                name,
                "no fallback keys could be resolved",
            ));
        }
    }
}

fn check_sandbox(checks: &mut Vec<DiagnosticCheck>) {
    if cfg!(target_os = "macos") {
        if which::which("sandbox-exec").is_ok() {
            checks.push(DiagnosticCheck::pass("Sandbox", "sandbox-exec available"));
        } else {
            checks.push(DiagnosticCheck::warn(
                "Sandbox",
                "sandbox-exec available",
                "not found",
            ));
        }
    } else if cfg!(target_os = "linux") {
        if which::which("bwrap").is_ok() {
            checks.push(DiagnosticCheck::pass("Sandbox", "bwrap available"));
        } else {
            checks.push(DiagnosticCheck::warn(
                "Sandbox",
                "bwrap available",
                "not found — sandboxing disabled",
            ));
        }
    } else {
        checks.push(DiagnosticCheck::warn(
            "Sandbox",
            "sandbox support",
            "not available on this platform",
        ));
    }
}

async fn check_skills(config: &Config, checks: &mut Vec<DiagnosticCheck>) {
    let resolved_creds = config.resolve_credentials();
    match load_all_skills(&resolved_creds, &config.skills) {
        Ok(skills) => {
            let available = skills.iter().filter(|s| s.available).count();
            let total = skills.len();
            checks.push(DiagnosticCheck::pass(
                "Skills",
                format!("{available}/{total} skills available"),
            ));
        }
        Err(e) => {
            checks.push(DiagnosticCheck::fail(
                "Skills",
                "skills loading",
                format!("{e}"),
            ));
        }
    }

    // Tamper audit. Hashes every SKILL.md under ~/.borg/skills/ and
    // compares against the stored hash in the skill_audit table.
    // First-seen skills are recorded silently (user put them there);
    // subsequent content changes are flagged as potential tampering.
    match (crate::skills::skills_dir(), Database::open().await) {
        (Ok(dir), Ok(db)) => {
            let findings = crate::security::skill::audit_user_skills(&db, &dir).await;
            let modified: Vec<String> = findings
                .iter()
                .filter(|f| f.is_modified())
                .map(|f| f.name.clone())
                .collect();
            if !modified.is_empty() {
                checks.push(DiagnosticCheck::warn(
                    "Skills",
                    format!(
                        "{n} user skill(s) modified since last load: {names}",
                        n = modified.len(),
                        names = modified.join(", "),
                    ),
                    "review content for supply-chain tampering",
                ));
            }
        }
        (Err(e), _) => tracing::warn!("doctor: cannot resolve skills dir for audit: {e}"),
        (_, Err(e)) => tracing::warn!("doctor: cannot open DB for skill audit: {e}"),
    }
}

async fn check_memory(checks: &mut Vec<DiagnosticCheck>) {
    match Database::open().await {
        Ok(db) => match db.list_memory_entries("global").await {
            Ok(entries) => {
                let has_index = entries.iter().any(|e| e.name == "INDEX");
                let total = entries.len();
                if has_index {
                    checks.push(DiagnosticCheck::pass(
                        "Memory",
                        format!("{total} memory entries (INDEX present)"),
                    ));
                } else if total == 0 {
                    checks.push(DiagnosticCheck::warn(
                        "Memory",
                        "memory entries",
                        "no entries in DB yet",
                    ));
                } else {
                    checks.push(
                        DiagnosticCheck::warn("Memory", "memory index", "INDEX entry missing")
                            .with_remediation(
                                RemediationKind::AutoSafe,
                                "memory.ensure_global_index",
                                "run /heal to seed global/INDEX from existing entries",
                            ),
                    );
                }
            }
            Err(e) => {
                checks.push(DiagnosticCheck::fail(
                    "Memory",
                    "list memory entries",
                    format!("{e}"),
                ));
            }
        },
        Err(e) => {
            checks.push(DiagnosticCheck::fail(
                "Memory",
                "open database",
                format!("{e}"),
            ));
        }
    }

    match Config::identity_path() {
        Ok(path) => {
            if path.exists() {
                checks.push(DiagnosticCheck::pass("Memory", "IDENTITY.md exists"));
            } else {
                checks.push(DiagnosticCheck::warn(
                    "Memory",
                    "IDENTITY.md exists",
                    "not found",
                ));
            }
        }
        Err(e) => {
            checks.push(DiagnosticCheck::fail(
                "Memory",
                IDENTITY_FILE,
                format!("{e}"),
            ));
        }
    }
}

async fn check_data_dir(checks: &mut Vec<DiagnosticCheck>) {
    match Config::data_dir() {
        Ok(data_dir) => {
            if data_dir.exists() {
                let test_file = data_dir.join(".doctor_write_test");
                match std::fs::write(&test_file, "test") {
                    Ok(()) => {
                        let _ = std::fs::remove_file(&test_file);
                        checks.push(DiagnosticCheck::pass("Data", "data directory writable"));
                    }
                    Err(e) => {
                        checks.push(DiagnosticCheck::fail(
                            "Data",
                            "data directory writable",
                            format!("{e}"),
                        ));
                    }
                }
            } else {
                checks.push(
                    DiagnosticCheck::warn(
                        "Data",
                        "data directory exists",
                        "~/.borg not found — run `/heal` or `borg init`",
                    )
                    .with_remediation(
                        RemediationKind::AutoSafe,
                        "data.ensure_subdirs",
                        "run /heal to recreate the runtime subdirectories",
                    ),
                );
            }
        }
        Err(e) => {
            checks.push(DiagnosticCheck::fail(
                "Data",
                "data directory",
                format!("{e}"),
            ));
        }
    }

    checks.push(DiagnosticCheck::from_result(
        "Data",
        "database accessible",
        Database::open().await,
    ));

    // `PRAGMA integrity_check` — surfaces SQLite-level corruption so a
    // bad disk or aborted write stops silently rotting the DB under us.
    // A clean DB returns the single row `"ok"`. Anything else is a
    // concrete problem; we downgrade to Warn (not Fail) so the
    // persistent-warning pipeline in maintenance can escalate it only
    // after it appears on two consecutive runs.
    // `Err` branch already surfaced by the "database accessible" check
    // above — don't double-report. Only run integrity if we could open.
    if let Ok(db) = Database::open().await {
        let integrity_result: Result<String> = async {
            let mut rows = db.conn().query("PRAGMA integrity_check", ()).await?;
            match rows.next().await? {
                Some(row) => Ok(row.get::<String>(0)?),
                None => Ok(String::new()),
            }
        }
        .await;
        match integrity_result {
            Ok(result) if result == "ok" => {
                checks.push(DiagnosticCheck::pass("Data", "database integrity"));
            }
            Ok(result) => {
                checks.push(DiagnosticCheck::warn("Data", "database integrity", result));
            }
            Err(e) => {
                checks.push(DiagnosticCheck::fail(
                    "Data",
                    "database integrity",
                    format!("{e}"),
                ));
            }
        }
    }
}

fn check_gateway(config: &Config, checks: &mut Vec<DiagnosticCheck>) {
    checks.push(DiagnosticCheck::pass(
        "Gateway",
        format!(
            "gateway config: {}:{}",
            config.gateway.host, config.gateway.port
        ),
    ));

    match Config::channels_dir() {
        Ok(channels_dir) => {
            if channels_dir.exists() {
                let mut count = 0;
                let mut errors = Vec::new();
                if let Ok(entries) = std::fs::read_dir(&channels_dir) {
                    for entry in entries.flatten() {
                        let path = entry.path();
                        if path.is_dir() {
                            let manifest_path = path.join("channel.toml");
                            if manifest_path.exists() {
                                match std::fs::read_to_string(&manifest_path) {
                                    Ok(content) => match toml::from_str::<toml::Value>(&content) {
                                        Ok(_) => count += 1,
                                        Err(e) => {
                                            errors.push(format!("{}: {e}", manifest_path.display()))
                                        }
                                    },
                                    Err(e) => {
                                        errors.push(format!("{}: {e}", manifest_path.display()))
                                    }
                                }
                            }
                        }
                    }
                }

                checks.push(DiagnosticCheck::pass(
                    "Gateway",
                    format!("{count} channel(s) found"),
                ));

                for error in errors {
                    checks.push(DiagnosticCheck::fail("Gateway", "channel manifest", error));
                }
            } else {
                // Fresh install: ~/.borg/channels is created lazily when a
                // script channel is installed. No directory means no script
                // channels, which is the normal state — not a warning.
                checks.push(DiagnosticCheck::pass("Gateway", "0 channel(s) found"));
            }
        }
        Err(e) => {
            checks.push(DiagnosticCheck::fail(
                "Gateway",
                "channels directory",
                format!("{e}"),
            ));
        }
    }
}

/// Format a unix timestamp as a human-readable relative time string.
pub fn format_relative_time(timestamp: i64) -> String {
    let now = chrono::Utc::now().timestamp();
    let delta = now - timestamp;
    if delta < 0 {
        return "just now".to_string();
    }
    let secs = delta as u64;
    if secs < 60 {
        if secs == 1 {
            "1 second ago".to_string()
        } else {
            format!("{secs} seconds ago")
        }
    } else if secs < 3600 {
        let mins = secs / 60;
        if mins == 1 {
            "1 minute ago".to_string()
        } else {
            format!("{mins} minutes ago")
        }
    } else if secs < SECS_PER_DAY as u64 {
        let hours = secs / 3600;
        if hours == 1 {
            "1 hour ago".to_string()
        } else {
            format!("{hours} hours ago")
        }
    } else {
        let days = secs / SECS_PER_DAY as u64;
        if days == 1 {
            "1 day ago".to_string()
        } else {
            format!("{days} days ago")
        }
    }
}

async fn check_heartbeat(config: &Config, checks: &mut Vec<DiagnosticCheck>) {
    checks.push(DiagnosticCheck::pass(
        "Heartbeat",
        format!("interval: {}", config.heartbeat.interval),
    ));

    // Check last heartbeat from activity log
    match crate::db::Database::open().await {
        Ok(db) => {
            match db.query_activity(1, Some("info"), Some("heartbeat")).await {
                Ok(entries) if !entries.is_empty() => {
                    let last = &entries[0];
                    let age_secs = chrono::Utc::now().timestamp() - last.created_at;

                    // Stale threshold: 2x configured interval (default 1h for 30m interval)
                    let interval_secs = crate::tasks::parse_interval(&config.heartbeat.interval)
                        .unwrap_or(crate::constants::HEARTBEAT_FALLBACK_INTERVAL)
                        .as_secs() as i64;
                    let stale_threshold = interval_secs * 2;

                    let relative = format_relative_time(last.created_at);
                    if age_secs <= stale_threshold {
                        checks.push(DiagnosticCheck::pass(
                            "Heartbeat",
                            format!("last heartbeat: {relative}"),
                        ));
                    } else {
                        checks.push(DiagnosticCheck::warn(
                            "Heartbeat",
                            "last heartbeat",
                            format!("{relative} — may not be running"),
                        ));
                    }
                }
                Ok(_) => {
                    checks.push(DiagnosticCheck::warn(
                        "Heartbeat",
                        "last heartbeat",
                        "no heartbeat activity recorded",
                    ));
                }
                Err(e) => {
                    checks.push(DiagnosticCheck::fail(
                        "Heartbeat",
                        "activity query",
                        format!("{e}"),
                    ));
                }
            }

            // Check if daemon is running (heartbeat runs in daemon or TUI)
            if db.is_daemon_lock_held().await {
                checks.push(DiagnosticCheck::pass(
                    "Heartbeat",
                    "daemon is running (heartbeat active)".to_string(),
                ));
            } else {
                checks.push(DiagnosticCheck::warn(
                    "Heartbeat",
                    "daemon status",
                    "daemon not running — heartbeat only active during TUI sessions",
                ));
            }
        }
        Err(e) => {
            checks.push(DiagnosticCheck::fail(
                "Heartbeat",
                "database",
                format!("cannot check heartbeat status: {e}"),
            ));
        }
    }
}

async fn check_budget(config: &Config, checks: &mut Vec<DiagnosticCheck>) {
    let limit = config.budget.monthly_token_limit;
    if limit == 0 {
        checks.push(DiagnosticCheck::warn(
            "Budget",
            "monthly limit",
            "unlimited (no budget set)",
        ));
        return;
    }

    match Database::open().await {
        Ok(db) => match db.monthly_token_total().await {
            Ok(used) => {
                let pct = (used as f64 / limit as f64 * 100.0) as u64;
                let name = format!("monthly usage: {used}/{limit} ({pct}%)");
                if used >= limit {
                    checks.push(DiagnosticCheck::fail("Budget", name, "budget exceeded"));
                } else if (used as f64 / limit as f64) >= config.budget.warning_threshold {
                    checks.push(DiagnosticCheck::warn("Budget", name, "approaching limit"));
                } else {
                    checks.push(DiagnosticCheck::pass("Budget", name));
                }
            }
            Err(e) => {
                checks.push(DiagnosticCheck::warn(
                    "Budget",
                    "monthly usage",
                    format!("could not read: {e}"),
                ));
            }
        },
        Err(e) => {
            checks.push(DiagnosticCheck::warn(
                "Budget",
                "monthly usage",
                format!("database unavailable: {e}"),
            ));
        }
    }
}

async fn check_prompt_cache(config: &Config, checks: &mut Vec<DiagnosticCheck>) {
    if !config.llm.cache.enabled {
        checks.push(DiagnosticCheck::warn(
            "Prompt Cache",
            "status",
            "disabled (llm.cache.enabled = false)",
        ));
        return;
    }
    checks.push(DiagnosticCheck::pass(
        "Prompt Cache",
        format!(
            "enabled (ttl={}, tools={}, system={}, rolling={})",
            config.llm.cache.ttl,
            config.llm.cache.cache_tools,
            config.llm.cache.cache_system,
            config.llm.cache.rolling_messages_clamped(),
        ),
    ));

    // Hit ratio over the last 24 hours — informational. Providers that do not
    // support caching (Ollama, Groq, Gemini non-cached) will report 0, which
    // we surface as a warning only when the primary provider is known to
    // support caching.
    let since = chrono::Utc::now().timestamp().saturating_sub(24 * 3600);
    let summary_result: Result<(u64, u64, u64)> = match Database::open().await {
        Ok(db) => db.cache_token_summary_since(since).await,
        Err(e) => Err(e),
    };
    match summary_result {
        Ok((prompt, cached, created)) => {
            if prompt == 0 {
                checks.push(DiagnosticCheck::pass(
                    "Prompt Cache",
                    "24h hit ratio: no traffic yet",
                ));
                return;
            }
            let pct = (cached as f64 / prompt as f64 * 100.0) as u64;
            let name =
                format!("24h hit ratio: {pct}% ({cached}/{prompt} cached, {created} created)");
            let supports_cache = provider_supports_cache(&config.llm);
            if supports_cache && pct < 20 && prompt > 10_000 {
                checks.push(DiagnosticCheck::warn(
                    "Prompt Cache",
                    name,
                    "low hit ratio — check system prompt stability",
                ));
            } else {
                checks.push(DiagnosticCheck::pass("Prompt Cache", name));
            }
        }
        Err(e) => {
            checks.push(DiagnosticCheck::warn(
                "Prompt Cache",
                "24h hit ratio",
                format!("could not read: {e}"),
            ));
        }
    }
}

/// Returns true if the configured provider is known to honor prompt caching
/// (either via explicit `cache_control` markers or implicit auto-caching).
fn provider_supports_cache(llm: &crate::config::LlmConfig) -> bool {
    let provider = llm.provider.as_deref().unwrap_or("");
    matches!(
        provider,
        "anthropic" | "openai" | "openrouter" | "deepseek" | "claude-cli"
    )
}

async fn check_plugins(checks: &mut Vec<DiagnosticCheck>) {
    match Database::open().await {
        Ok(db) => match db.list_plugins().await {
            Ok(plugins) => {
                if plugins.is_empty() {
                    checks.push(DiagnosticCheck::warn(
                        "Plugins",
                        "installed integrations",
                        "none installed (use /plugins)",
                    ));
                } else {
                    let verified = plugins.iter().filter(|c| c.verified_at.is_some()).count();
                    checks.push(DiagnosticCheck::pass(
                        "Plugins",
                        format!(
                            "{} integration(s) installed, {verified} verified",
                            plugins.len()
                        ),
                    ));

                    for c in &plugins {
                        let name = format!("{} ({})", c.name, c.kind);
                        if c.verified_at.is_some() {
                            checks.push(DiagnosticCheck::pass("Plugins", name));
                        } else {
                            checks.push(DiagnosticCheck::warn("Plugins", name, "not verified"));
                        }

                        if let Ok(data_dir) = Config::data_dir() {
                            if let Ok(result) =
                                crate::integrity::verify_integrity(&db, &c.id, &data_dir).await
                            {
                                let integrity_name = format!("{} file integrity", c.name);
                                if result.ok {
                                    checks.push(DiagnosticCheck::pass("Plugins", integrity_name));
                                } else {
                                    let mut issues = Vec::new();
                                    if !result.tampered.is_empty() {
                                        issues.push(format!(
                                            "tampered: {}",
                                            result.tampered.join(", ")
                                        ));
                                    }
                                    if !result.missing.is_empty() {
                                        issues.push(format!(
                                            "missing: {}",
                                            result.missing.join(", ")
                                        ));
                                    }
                                    checks.push(DiagnosticCheck::fail(
                                        "Plugins",
                                        integrity_name,
                                        issues.join("; "),
                                    ));
                                }
                            }
                        }
                    }
                }
            }
            Err(e) => {
                checks.push(DiagnosticCheck::warn(
                    "Plugins",
                    "plugins table",
                    format!("could not query: {e}"),
                ));
            }
        },
        Err(e) => {
            checks.push(DiagnosticCheck::warn(
                "Plugins",
                "database",
                format!("unavailable: {e}"),
            ));
        }
    }
}

fn check_browser(config: &Config, checks: &mut Vec<DiagnosticCheck>) {
    if !config.browser.enabled {
        checks.push(DiagnosticCheck::warn(
            "Browser",
            "browser automation",
            "disabled in config",
        ));
        return;
    }

    let detection = crate::browser::find_chrome(config.browser.executable.as_deref());
    if let Some(ref exe) = detection.executable {
        checks.push(DiagnosticCheck::pass(
            "Browser",
            format!("Chrome detected: {}", exe.display()),
        ));
    } else {
        checks.push(DiagnosticCheck::warn(
            "Browser",
            "Chrome/Chromium detection",
            "no Chrome-like browser found",
        ));
    }

    if detection.executable.is_some() {
        checks.push(DiagnosticCheck::pass(
            "Browser",
            "native CDP browser tool available",
        ));
    }

    if crate::browser::detect_agent_browser() {
        checks.push(DiagnosticCheck::pass(
            "Browser",
            "agent-browser CLI also available (legacy)",
        ));
    }

    checks.push(DiagnosticCheck::pass(
        "Browser",
        format!(
            "config: headless={}, cdp_port={}, timeout={}ms",
            config.browser.headless, config.browser.cdp_port, config.browser.timeout_ms
        ),
    ));
}

async fn check_agents(config: &Config, checks: &mut Vec<DiagnosticCheck>) {
    if !config.agents.enabled {
        checks.push(DiagnosticCheck::warn(
            "Agents",
            "multi-agent system",
            "disabled",
        ));
        return;
    }
    checks.push(DiagnosticCheck::pass(
        "Agents",
        format!(
            "multi-agent enabled (depth={}, children={}, concurrent={})",
            config.agents.max_spawn_depth,
            config.agents.max_children_per_agent,
            config.agents.max_concurrent,
        ),
    ));
    if config.agents.max_spawn_depth > 3 {
        checks.push(DiagnosticCheck::warn(
            "Agents",
            "spawn depth",
            format!(
                "max_spawn_depth={} is high (recommended ≤3)",
                config.agents.max_spawn_depth
            ),
        ));
    }
    let roles = crate::multi_agent::roles::list_all_roles().await;
    checks.push(DiagnosticCheck::pass(
        "Agents",
        format!("{} role(s) available", roles.len()),
    ));
}

/// Decide pass/warn for the embedding-lag check. A small unindexed gap is
/// the normal race window between `write_memory` and the embedder backfill;
/// only warn when the gap is large enough to suggest the embedder is stuck.
///
/// Pulled out as a pure function so the threshold can be exercised in unit
/// tests without spinning up a real `Database`.
fn classify_embedding_lag(global_count: usize, entry_count: usize) -> DiagnosticCheck {
    let detail = format!("{global_count}/{entry_count} memory entries indexed");
    if entry_count == 0 || global_count >= entry_count {
        return DiagnosticCheck::pass("Embeddings", format!("whole-entry coverage: {detail}"));
    }
    let gap = entry_count - global_count;
    if gap <= EMBEDDING_LAG_WARN_THRESHOLD {
        DiagnosticCheck::pass(
            "Embeddings",
            format!("whole-entry coverage: {detail} (pending backfill)"),
        )
    } else {
        DiagnosticCheck::warn(
            "Embeddings",
            "whole-entry coverage",
            format!("{detail} — {gap} entries lag behind"),
        )
        .with_remediation(
            RemediationKind::AutoSafe,
            "embeddings.repair",
            "run /heal to backfill stale or missing embeddings",
        )
    }
}

#[derive(Default)]
struct ChunkCoverage {
    total_entries: usize,
    fully_indexed_entries: usize,
    entries_missing_chunks: usize,
    entries_missing_chunk_embeddings: usize,
}

fn classify_chunk_coverage(coverage: ChunkCoverage) -> DiagnosticCheck {
    let detail = format!(
        "{}/{} entries fully chunk-indexed",
        coverage.fully_indexed_entries, coverage.total_entries
    );
    if coverage.total_entries == 0 || coverage.fully_indexed_entries == coverage.total_entries {
        return DiagnosticCheck::pass("Embeddings", format!("chunk coverage: {detail}"));
    }
    let mut issues = Vec::new();
    if coverage.entries_missing_chunks > 0 {
        issues.push(format!(
            "{} missing chunk rows",
            coverage.entries_missing_chunks
        ));
    }
    if coverage.entries_missing_chunk_embeddings > 0 {
        issues.push(format!(
            "{} missing chunk embeddings",
            coverage.entries_missing_chunk_embeddings
        ));
    }
    DiagnosticCheck::warn(
        "Embeddings",
        "chunk coverage",
        format!("{detail} — {}", issues.join(", ")),
    )
    .with_remediation(
        RemediationKind::AutoSafe,
        "embeddings.repair",
        "run /heal to rebuild stale chunk indexes",
    )
}

async fn collect_chunk_coverage(db: &Database, scopes: &[String]) -> ChunkCoverage {
    let mut coverage = ChunkCoverage::default();
    for scope in scopes {
        let Ok(entries) = db.list_memory_entries(scope).await else {
            continue;
        };
        for entry in entries {
            coverage.total_entries += 1;
            let Ok(chunks) = db.get_chunks_for_file(scope, &entry.name).await else {
                coverage.entries_missing_chunks += 1;
                continue;
            };
            if chunks.is_empty() {
                coverage.entries_missing_chunks += 1;
                continue;
            }
            if chunks.iter().any(|c| c.embedding.is_none()) {
                coverage.entries_missing_chunk_embeddings += 1;
                continue;
            }
            coverage.fully_indexed_entries += 1;
        }
    }
    coverage
}

async fn check_embeddings(config: &Config, checks: &mut Vec<DiagnosticCheck>) {
    if !config.memory.embeddings.enabled {
        checks.push(DiagnosticCheck::warn(
            "Embeddings",
            "semantic memory search",
            "disabled in config",
        ));
        return;
    }

    match crate::memory::embeddings::EmbeddingProvider::from_config(&config.memory.embeddings) {
        Some(provider) => {
            checks.push(DiagnosticCheck::pass(
                "Embeddings",
                format!(
                    "provider: {} (model: {})",
                    provider.endpoint, provider.model
                ),
            ));
        }
        None => {
            checks.push(DiagnosticCheck::warn(
                "Embeddings",
                "embedding provider",
                "no embedding-capable API key found, using recency-based loading",
            ));
            return;
        }
    }

    match Database::open().await {
        Ok(db) => {
            let scopes = {
                let mut scopes = crate::memory::resolved_memory_scopes(config);
                if let Ok(mut db_scopes) = db.list_memory_scopes().await {
                    scopes.append(&mut db_scopes);
                }
                scopes.sort();
                scopes.dedup();
                scopes
            };
            let mut global_count = 0usize;
            let mut entry_count = 0usize;
            for scope in &scopes {
                global_count += db.count_embeddings(scope).await.unwrap_or(0);
                entry_count += db
                    .list_memory_entries(scope)
                    .await
                    .map(|v| v.len())
                    .unwrap_or(0);
            }

            checks.push(classify_embedding_lag(global_count, entry_count));
            checks.push(classify_chunk_coverage(
                collect_chunk_coverage(&db, &scopes).await,
            ));
        }
        Err(e) => {
            checks.push(DiagnosticCheck::warn(
                "Embeddings",
                "embedding count",
                format!("database unavailable: {e}"),
            ));
        }
    }
}

fn check_config_security(config: &Config, checks: &mut Vec<DiagnosticCheck>) {
    use crate::pairing::DmPolicy;

    // DM policy (three-way match — not table-driven)
    match config.gateway.dm_policy {
        DmPolicy::Open => checks.push(DiagnosticCheck::warn(
            "Security",
            "DM policy",
            "set to \"open\" — all senders can interact without approval. Set to \"pairing\" for access control",
        )),
        DmPolicy::Pairing => checks.push(DiagnosticCheck::pass("Security", "DM policy (pairing)")),
        DmPolicy::Disabled => checks.push(DiagnosticCheck::pass("Security", "DM policy (disabled)")),
    }

    // Per-channel open policies (dynamic list — not table-driven)
    let open_channels: Vec<&String> = config
        .gateway
        .channel_policies
        .iter()
        .filter(|(_, policy)| matches!(policy, DmPolicy::Open))
        .map(|(name, _)| name)
        .collect();
    if open_channels.is_empty() {
        checks.push(DiagnosticCheck::pass(
            "Security",
            "channel policies (no open overrides)",
        ));
    } else {
        checks.push(DiagnosticCheck::warn(
            "Security",
            "channel policies",
            format!(
                "open DM policy on: {}. Consider \"pairing\" for untrusted channels",
                open_channels
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        ));
    }

    // Table-driven boolean/simple checks
    struct BoolCheck {
        ok: bool,
        pass_label: &'static str,
        warn_label: &'static str,
        warn_detail: &'static str,
    }

    let bool_checks = [
        BoolCheck {
            ok: config.sandbox.enabled && config.sandbox.mode != "permissive",
            pass_label: if config.sandbox.enabled { "sandbox enabled (strict)" } else { "sandbox" },
            warn_label: if !config.sandbox.enabled { "sandbox" } else { "sandbox mode" },
            warn_detail: if !config.sandbox.enabled {
                "disabled — user tools run without isolation. Set sandbox.enabled = true"
            } else {
                "set to \"permissive\" — weaker isolation. Consider \"strict\""
            },
        },
        BoolCheck {
            ok: config.security.secret_detection,
            pass_label: "secret detection enabled",
            warn_label: "secret detection",
            warn_detail: "disabled — API keys and tokens may leak in tool output. Set security.secret_detection = true",
        },
        BoolCheck {
            ok: !config.security.blocked_paths.is_empty(),
            pass_label: "blocked paths",
            warn_label: "blocked paths",
            warn_detail: "empty — sensitive directories (.ssh, .aws, .gnupg) are not protected. Add entries to security.blocked_paths",
        },
        BoolCheck {
            ok: config.security.action_limits.tool_calls_block <= 1000
                && config.security.action_limits.shell_commands_block <= 500
                && config.security.action_limits.file_writes_block <= 300,
            pass_label: "rate limits within bounds",
            warn_label: "rate limits",
            warn_detail: "block thresholds are very high — consider lowering for tighter safety bounds",
        },
        BoolCheck {
            ok: config.budget.monthly_token_limit != 0,
            pass_label: "budget",
            warn_label: "budget",
            warn_detail: "unlimited (monthly_token_limit = 0) — no spend cap. Set a limit to prevent runaway usage",
        },
    ];

    for check in &bool_checks {
        if check.ok {
            // Dynamic pass labels for blocked_paths count and budget cap
            let label = match check.pass_label {
                "blocked paths" => format!(
                    "blocked paths ({} entries)",
                    config.security.blocked_paths.len()
                ),
                "budget" => format!(
                    "budget capped ({} tokens/month)",
                    config.budget.monthly_token_limit
                ),
                other => other.to_string(),
            };
            checks.push(DiagnosticCheck::pass("Security", label));
        } else {
            checks.push(DiagnosticCheck::warn(
                "Security",
                check.warn_label,
                check.warn_detail,
            ));
        }
    }

    // Browser no-sandbox (conditional on browser being enabled)
    if config.browser.enabled && config.browser.no_sandbox {
        checks.push(DiagnosticCheck::warn(
            "Security",
            "browser sandbox",
            "Chrome --no-sandbox is enabled — browser runs without isolation. Set browser.no_sandbox = false",
        ));
    } else if config.browser.enabled {
        checks.push(DiagnosticCheck::pass("Security", "browser sandbox enabled"));
    }
}

// ───────────────────────── Expanded subsystem checks ─────────────────────────
//
// Each public-facing check below is applicability-gated: when its subsystem
// isn't configured/in-use it emits zero rows and `suppress_when_empty` drops
// the section. The pass/warn/fail decision is factored into a pure `classify_*`
// helper (or a DB-injectable inner fn) wherever external state is involved, so
// the thresholds are unit-testable without a network, a live daemon, or the
// real `~/.borg` on disk.

/// Settings validity: runs the same `Config::validate()` the loader runs so a
/// fatal misconfiguration (out-of-range temperature, zero token caps) surfaces
/// as a failing check rather than a silent clamp at boot. Always applicable.
fn check_settings(config: &Config, checks: &mut Vec<DiagnosticCheck>) {
    match config.validate() {
        Ok(()) => checks.push(DiagnosticCheck::pass("Settings", "config values valid")),
        Err(e) => {
            let mut c = DiagnosticCheck::fail("Settings", "config values valid", format!("{e}"));
            c.remediation_kind = RemediationKind::Manual;
            c.user_hint =
                Some("fix it via `borg settings set <key> <value>` or `/settings`".to_string());
            checks.push(c);
        }
    }
}

/// Pure pass/warn/fail decision for credential-pool health. Split out so the
/// thresholds are testable without constructing a real pool. `has_credentials`
/// false → Fail (nothing usable); available → Pass; configured-but-all-cooling
/// → Warn with the earliest recovery estimate.
fn classify_credential_pool(
    provider_name: &str,
    has_credentials: bool,
    has_available: bool,
    next_recovery_at_ms: Option<i64>,
    now_ms: i64,
) -> DiagnosticCheck {
    if !has_credentials {
        return DiagnosticCheck::fail(
            "Credentials",
            format!("{provider_name} credentials"),
            "no usable API credentials resolved for the active provider",
        );
    }
    if has_available {
        return DiagnosticCheck::pass(
            "Credentials",
            format!("{provider_name} credentials available"),
        );
    }
    let detail = match next_recovery_at_ms {
        Some(reset_ms) if reset_ms > now_ms => {
            format!(
                "all keys rate-limited/cooling — earliest recovery in ~{}s",
                (reset_ms - now_ms) / 1000
            )
        }
        _ => "all keys exhausted with no scheduled recovery".to_string(),
    };
    DiagnosticCheck::warn(
        "Credentials",
        format!("{provider_name} credentials"),
        detail,
    )
}

/// Whether the credential-pool check applies to `provider`: only providers
/// that authenticate with a plaintext API key have an inspectable pool. Local
/// (Ollama), CLI, and AWS-credential-chain providers have no pool and are
/// covered by the Provider check. Pure, so the applicability gate is testable
/// alongside the severity mapping in `classify_credential_pool`.
fn credential_check_applies(provider: Provider) -> bool {
    provider.requires_api_key() && !provider.uses_aws_credential_chain()
}

/// LLM credential-pool health (offline). Applicable only to key-authenticated
/// providers (see [`credential_check_applies`]); other providers are covered by
/// the Provider check.
async fn check_llm_credentials(config: &Config, checks: &mut Vec<DiagnosticCheck>) {
    let (provider, keys) = match config.resolve_api_keys() {
        Ok((p, keys)) if credential_check_applies(p) => (p, keys),
        _ => return,
    };
    if keys.iter().all(String::is_empty) {
        return; // nothing resolvable — the Provider check already reports this
    }
    let pool = crate::llm::credential_pool::try_build_from_keys(
        provider,
        keys,
        config.llm.credential_strategy,
        config.llm.credential_cooldown_secs,
        config.llm.max_concurrent_per_credential,
        config.llm.proactive_rate_limit_threshold_pct,
        config.llm.proactive_rate_limit_min_window_secs,
    )
    .await;
    match pool {
        Some(pool) => checks.push(classify_credential_pool(
            provider.as_str(),
            pool.has_credentials(),
            pool.has_available(),
            pool.next_recovery_at_ms(),
            chrono::Utc::now().timestamp_millis(),
        )),
        None => checks.push(DiagnosticCheck::warn(
            "Credentials",
            format!("{} credential pool", provider.as_str()),
            "could not load credential pool (see logs)",
        )),
    }
}

/// Pure decision for the secrets-backend posture check. `kek_file_present` only
/// matters for the `File` backend, whose encrypted blob must exist.
fn classify_keychain(backend: KekBackend, kek_file_present: bool) -> DiagnosticCheck {
    match backend {
        KekBackend::OsKeychain => DiagnosticCheck::pass("Keychain", "KEK backend: OS keychain"),
        // `Auto` resolves to a concrete backend on first daemon start and
        // persists it; seeing it here just means the daemon hasn't booted yet.
        KekBackend::Auto => {
            DiagnosticCheck::pass("Keychain", "KEK backend: auto (resolves on daemon start)")
        }
        KekBackend::File if kek_file_present => DiagnosticCheck::warn(
            "Keychain",
            "KEK backend: encrypted file",
            "file backend in use — posture is Reduced (×0.85). `borg service install --secure` upgrades to an OS keychain",
        ),
        KekBackend::File => DiagnosticCheck::fail(
            "Keychain",
            "KEK backend: encrypted file",
            "file backend selected but ~/.borg/secrets/kek.enc is missing — the daemon cannot unseal its keys",
        ),
    }
}

/// Secrets/keychain posture. Config-based (cortex doesn't link `identity`, so
/// no live keychain round-trip): reports the resolved KEK backend and, for the
/// file backend, whether its encrypted blob is present. Always applicable.
fn check_keychain(config: &Config, checks: &mut Vec<DiagnosticCheck>) {
    let kek_present = Config::data_dir()
        .map(|home| crate::paths::kek_file_path(&home).exists())
        .unwrap_or(false);
    checks.push(classify_keychain(config.security.kek_backend, kek_present));
}

/// Build the orphan-lock finding from its idle age. Pulled out so the message
/// and remediation wiring are testable without a DB. The detection itself
/// (dead PID + stale heartbeat) lives in `Database::daemon_lock_orphan_age`.
fn orphan_lock_check(idle_secs: i64) -> DiagnosticCheck {
    DiagnosticCheck::warn(
        "Locks",
        "orphan daemon lock",
        format!("daemon lock held by a dead PID, idle {idle_secs}s — a daemon likely crashed mid-session"),
    )
    .with_remediation(
        RemediationKind::AutoSafe,
        "locks.clear_orphan",
        "run `borg doctor --repair` (or `/heal`) to clear the stale lock",
    )
}

/// Orphan daemon-lock detection. Emits a finding only when the lock is held by
/// a dead PID past the staleness grace — a running daemon (live PID or fresh
/// heartbeat) is never flagged. Suppressed when healthy/absent.
async fn check_daemon_lock(checks: &mut Vec<DiagnosticCheck>) {
    let Ok(db) = Database::open().await else {
        return;
    };
    let now = chrono::Utc::now().timestamp();
    if let Ok(Some(age)) = db.daemon_lock_orphan_age(now, DAEMON_LOCK_STALE_SECS).await {
        checks.push(orphan_lock_check(age));
    }
}

/// Scheduled-task health from a DB handle (injectable for tests). Surfaces a
/// count summary plus any task whose last run errored — the genuinely
/// actionable signal. Overdue-but-not-errored tasks are left to the nightly
/// heal (flagging them here would be noise whenever the daemon is simply off).
async fn scheduled_task_checks(db: &Database) -> Vec<DiagnosticCheck> {
    // Single fetch: `list_tasks` already returns every row including
    // `last_error`, so the count summary and the errored subset come from one
    // scan rather than a second `tasks_with_last_error` query.
    let tasks = match db.list_tasks().await {
        Ok(t) => t,
        Err(e) => {
            return vec![DiagnosticCheck::warn(
                "Scheduled Tasks",
                "task store",
                format!("could not list tasks: {e}"),
            )]
        }
    };
    let mut out = vec![DiagnosticCheck::pass(
        "Scheduled Tasks",
        format!("{} scheduled task(s)", tasks.len()),
    )];
    let mut errored: Vec<&str> = tasks
        .iter()
        .filter(|t| t.last_error.as_deref().is_some_and(|e| !e.is_empty()))
        .map(|t| t.name.as_str())
        .collect();
    errored.sort_unstable(); // stable, name-ordered output
    if !errored.is_empty() {
        let sample = errored
            .iter()
            .take(3)
            .copied()
            .collect::<Vec<_>>()
            .join(", ");
        out.push(DiagnosticCheck::warn(
            "Scheduled Tasks",
            "tasks with errors",
            format!(
                "{} task(s) failed their last run (e.g. {sample}) — inspect via `borg schedule list`",
                errored.len()
            ),
        ));
    }
    out
}

async fn check_scheduled_tasks(checks: &mut Vec<DiagnosticCheck>) {
    match Database::open().await {
        Ok(db) => checks.extend(scheduled_task_checks(&db).await),
        Err(e) => checks.push(DiagnosticCheck::warn(
            "Scheduled Tasks",
            "task store",
            format!("database unavailable: {e}"),
        )),
    }
}

/// Candidate service-unit paths for this platform (launchd plist on macOS,
/// systemd user/system unit on Linux). Mirrors `borgd_launcher`'s install
/// paths; kept here so the cortex-side check needs no `interface` dependency.
fn service_unit_paths() -> Vec<std::path::PathBuf> {
    use crate::constants::{LAUNCHD_SERVICE_LABEL, SYSTEMD_UNIT_NAME};
    let mut paths = Vec::new();
    if let Some(home) = dirs::home_dir() {
        if cfg!(target_os = "macos") {
            paths.push(
                home.join("Library/LaunchAgents")
                    .join(format!("{LAUNCHD_SERVICE_LABEL}.plist")),
            );
        } else if cfg!(target_os = "linux") {
            paths.push(home.join(".config/systemd/user").join(SYSTEMD_UNIT_NAME));
        }
    }
    if cfg!(target_os = "linux") {
        paths.push(std::path::PathBuf::from("/etc/systemd/system").join(SYSTEMD_UNIT_NAME));
    }
    paths
}

/// Pure decision for the service-install check. Detection-only: an installed
/// unit → informational Pass; not installed → `None` (a foreground setup is a
/// normal, non-error state, so the section is suppressed).
fn classify_service(installed_at: Option<&std::path::Path>) -> Option<DiagnosticCheck> {
    installed_at.map(|p| {
        DiagnosticCheck::pass(
            "Service",
            format!("daemon installed as a service ({})", p.display()),
        )
    })
}

fn check_service(checks: &mut Vec<DiagnosticCheck>) {
    let found = service_unit_paths().into_iter().find(|p| p.exists());
    if let Some(check) = classify_service(found.as_deref()) {
        checks.push(check);
    }
}

/// External binary a tunnel provider needs, if any. `None` (no tunnel) and
/// `Custom` (operator-supplied command) have no fixed binary requirement.
fn tunnel_binary(provider: TunnelProvider) -> Option<&'static str> {
    match provider {
        TunnelProvider::Cloudflare => Some("cloudflared"),
        TunnelProvider::Ngrok => Some("ngrok"),
        TunnelProvider::Tailscale => Some("tailscale"),
        TunnelProvider::None | TunnelProvider::Custom => None,
    }
}

/// Pure severity mapping for the tunnel check, given the provider's already-
/// resolved `bin` (applicability — does the provider need a binary — is gated by
/// `check_tunnel`). Binary missing from PATH → Fail; present → Pass.
fn classify_tunnel(provider: TunnelProvider, bin: &str, binary_present: bool) -> DiagnosticCheck {
    if binary_present {
        DiagnosticCheck::pass("Tunnel", format!("{provider} binary `{bin}` found"))
    } else {
        let mut c = DiagnosticCheck::fail(
            "Tunnel",
            format!("{provider} binary"),
            format!("`{bin}` not found on PATH — `borg tunnel start` will fail"),
        );
        c.remediation_kind = RemediationKind::Manual;
        c.user_hint = Some(format!("install `{bin}` and ensure it is on PATH"));
        c
    }
}

fn check_tunnel(config: &Config, checks: &mut Vec<DiagnosticCheck>) {
    let provider = config.tunnel.provider;
    if let Some(bin) = tunnel_binary(provider) {
        checks.push(classify_tunnel(provider, bin, which::which(bin).is_ok()));
    }
}

/// Build the per-server MCP check from its lifecycle label. Pure so the
/// state→severity mapping is testable without a live registry — which the CLI
/// `borg doctor` doesn't have, so every server reads as "not-started" there.
/// That state must NOT be a failure (configured ≠ broken).
fn mcp_check_for_label(name: &str, label: &str) -> DiagnosticCheck {
    match label {
        "connected" => DiagnosticCheck::pass("MCP", format!("{name}: connected")),
        "auth-failed" => DiagnosticCheck::fail(
            "MCP",
            format!("{name}: auth"),
            "authentication failed — re-pair the server's credentials",
        ),
        "reconnecting" => DiagnosticCheck::warn(
            "MCP",
            format!("{name}: reconnecting"),
            "transport dropped — retrying",
        ),
        "degraded" => DiagnosticCheck::warn(
            "MCP",
            format!("{name}: degraded"),
            "server is up but some tools were rejected",
        ),
        // "not-started" / "disabled" / "live-disabled": informational only.
        other => DiagnosticCheck::pass("MCP", format!("{name}: {other}")),
    }
}

/// MCP server reachability. Applicable only when servers are configured;
/// suppressed otherwise. Per server, maps its live lifecycle state to a check.
fn check_mcp(config: &Config, checks: &mut Vec<DiagnosticCheck>) {
    for snap in crate::mcp_clients::server_status_snapshot(config) {
        checks.push(mcp_check_for_label(&snap.name, snap.state_label()));
    }
}

/// Whether docker is actually in use — the docker built-in skill is explicitly
/// enabled. It ships disabled, so a vanilla install reports docker not-in-use
/// and the whole check is skipped (honoring "don't flag what isn't applicable").
fn docker_in_use(config: &Config) -> bool {
    config
        .skills
        .entries
        .get("docker")
        .map(|e| e.enabled)
        .unwrap_or(false)
}

/// Pure severity mapping for the docker check (applicability — whether docker
/// is in use — is gated by `check_docker` before this is reached). Missing
/// binary or a down daemon → Warn (not Fail: a down docker only breaks
/// docker-backed skills, not borg itself).
fn classify_docker(binary_present: bool, daemon_up: bool) -> DiagnosticCheck {
    if !binary_present {
        DiagnosticCheck::warn(
            "Docker",
            "docker CLI",
            "docker skill is enabled but the `docker` binary is not on PATH",
        )
    } else if !daemon_up {
        DiagnosticCheck::warn(
            "Docker",
            "docker daemon",
            "docker skill is enabled but the daemon is not responding (`docker info` failed)",
        )
    } else {
        DiagnosticCheck::pass("Docker", "docker daemon reachable")
    }
}

/// Probe the docker daemon with a short `docker info`. Side-effect free; a
/// non-zero exit (daemon down / no socket) returns false.
fn docker_daemon_up() -> bool {
    std::process::Command::new("docker")
        .args(["info", "--format", "{{.ServerVersion}}"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn check_docker(config: &Config, checks: &mut Vec<DiagnosticCheck>) {
    if !docker_in_use(config) {
        return;
    }
    let binary_present = which::which("docker").is_ok();
    let daemon_up = binary_present && docker_daemon_up();
    checks.push(classify_docker(binary_present, daemon_up));
}

#[cfg(all(test, feature = "phase9-tests-disabled"))]
mod tests {
    use super::*;

    #[test]
    fn report_format_output() {
        let report = DiagnosticReport {
            checks: vec![
                DiagnosticCheck::pass("Provider", "API key set"),
                DiagnosticCheck::pass("Provider", "Provider: openrouter"),
                DiagnosticCheck::fail("Provider", "Claude CLI binary", "not found"),
                DiagnosticCheck::warn("Sandbox", "sandbox-exec", "not available"),
            ],
        };
        let output = report.format();
        assert!(output.contains("Borg Doctor"));
        assert!(output.contains("✓ API key set"));
        assert!(output.contains("✗ Claude CLI binary"));
        assert!(output.contains("⚠ sandbox-exec"));
        assert!(output.contains("2 passed, 1 warning(s), 1 failed"));
    }

    #[test]
    fn report_counts() {
        let report = DiagnosticReport {
            checks: vec![
                DiagnosticCheck::pass("Test", "pass"),
                DiagnosticCheck::warn("Test", "warn", "w"),
                DiagnosticCheck::fail("Test", "fail", "f"),
            ],
        };
        assert_eq!(report.counts(), (1, 1, 1));
    }

    #[test]
    fn empty_report() {
        let report = DiagnosticReport { checks: vec![] };
        let output = report.format();
        assert!(output.contains("0 passed"));
        assert_eq!(report.counts(), (0, 0, 0));
    }

    #[test]
    fn run_diagnostics_produces_checks() {
        let config = Config::default();
        let report = run_diagnostics(&config);
        // Should always produce at least the provider checks
        assert!(!report.checks.is_empty());
        // Should have at least provider, sandbox, skills, memory, data categories
        let categories: std::collections::HashSet<&str> =
            report.checks.iter().map(|c| c.category).collect();
        assert!(categories.contains("Provider"));
        assert!(categories.contains("Sandbox"));
    }

    #[test]
    fn diagnostic_check_pass_fields() {
        let check = DiagnosticCheck::pass("Provider", "API key set");
        assert_eq!(check.category, "Provider");
        assert_eq!(check.name, "API key set");
        assert_eq!(check.status, CheckStatus::Pass);
    }

    #[test]
    fn diagnostic_check_fail_includes_detail() {
        let check = DiagnosticCheck::fail("Provider", "API key", "not found in env");
        assert_eq!(check.category, "Provider");
        assert!(matches!(check.status, CheckStatus::Fail(ref msg) if msg == "not found in env"));
    }

    #[test]
    fn diagnostic_check_warn_includes_detail() {
        let check = DiagnosticCheck::warn("Sandbox", "sandbox-exec", "not available on Linux");
        assert_eq!(check.category, "Sandbox");
        assert!(
            matches!(check.status, CheckStatus::Warn(ref msg) if msg == "not available on Linux")
        );
    }

    #[test]
    fn diagnostic_check_with_remediation_formats_fixability() {
        let check = DiagnosticCheck::warn("Embeddings", "chunk coverage", "backlog")
            .with_remediation(RemediationKind::AutoSafe, "embeddings.repair", "run /heal");
        assert_eq!(check.issue_key(), "Embeddings:chunk coverage");
        assert!(check.format_line().contains("[auto-fixable]"));
    }

    #[test]
    fn report_format_groups_consecutive_same_category() {
        let report = DiagnosticReport {
            checks: vec![
                DiagnosticCheck::pass("Sandbox", "check A"),
                DiagnosticCheck::pass("Sandbox", "check C"),
                DiagnosticCheck::pass("Provider", "check B"),
            ],
        };
        let output = report.format();
        // Consecutive Sandbox checks should appear under a single Sandbox heading
        let sandbox_heading_count = output.matches("\nSandbox\n").count();
        assert_eq!(
            sandbox_heading_count, 1,
            "Consecutive same-category checks should share one heading"
        );
        // check A and check C should both appear before Provider
        let check_a_pos = output.find("check A").unwrap();
        let check_c_pos = output.find("check C").unwrap();
        let provider_pos = output.find("Provider").unwrap();
        assert!(check_a_pos < check_c_pos);
        assert!(check_c_pos < provider_pos);
    }

    #[test]
    fn run_diagnostics_includes_browser_category() {
        let config = Config::default();
        let report = run_diagnostics(&config);
        let categories: std::collections::HashSet<&str> =
            report.checks.iter().map(|c| c.category).collect();
        assert!(categories.contains("Browser"));
    }

    #[test]
    fn browser_disabled_produces_warn() {
        let mut checks = Vec::new();
        let mut config = Config::default();
        config.browser.enabled = false;
        check_browser(&config, &mut checks);
        assert_eq!(checks.len(), 1);
        assert!(matches!(checks[0].status, CheckStatus::Warn(_)));
        assert_eq!(checks[0].category, "Browser");
    }

    #[test]
    fn classify_embedding_lag_warns_with_auto_fix_metadata() {
        let check = classify_embedding_lag(1, 10);
        assert_eq!(check.name, "whole-entry coverage");
        assert_eq!(check.remediation_kind, RemediationKind::AutoSafe);
        assert_eq!(check.remediation_key, Some("embeddings.repair"));
    }

    #[test]
    fn classify_chunk_coverage_warns_when_entries_lack_chunks() {
        let check = classify_chunk_coverage(ChunkCoverage {
            total_entries: 3,
            fully_indexed_entries: 1,
            entries_missing_chunks: 1,
            entries_missing_chunk_embeddings: 1,
        });
        assert_eq!(check.name, "chunk coverage");
        assert!(matches!(check.status, CheckStatus::Warn(_)));
        assert_eq!(check.remediation_kind, RemediationKind::AutoSafe);
    }

    #[test]
    fn config_security_secure_defaults_all_pass() {
        let config = Config::default();
        let mut checks = Vec::new();
        check_config_security(&config, &mut checks);
        // All checks should be in the Security category
        assert!(checks.iter().all(|c| c.category == "Security"));
        // Default config is secure — no warnings expected
        let warns: Vec<_> = checks
            .iter()
            .filter(|c| matches!(c.status, CheckStatus::Warn(_)))
            .collect();
        assert!(
            warns.is_empty(),
            "Secure defaults should produce no warnings: {warns:?}"
        );
    }

    #[test]
    fn config_security_open_dm_policy_warns() {
        let mut config = Config::default();
        config.gateway.dm_policy = crate::pairing::DmPolicy::Open;
        let mut checks = Vec::new();
        check_config_security(&config, &mut checks);
        let dm_check = checks.iter().find(|c| c.name == "DM policy").unwrap();
        assert!(matches!(dm_check.status, CheckStatus::Warn(_)));
    }

    #[test]
    fn config_security_sandbox_disabled_warns() {
        let mut config = Config::default();
        config.sandbox.enabled = false;
        let mut checks = Vec::new();
        check_config_security(&config, &mut checks);
        let sandbox_check = checks.iter().find(|c| c.name == "sandbox").unwrap();
        assert!(matches!(sandbox_check.status, CheckStatus::Warn(_)));
    }

    #[test]
    fn config_security_secret_detection_off_warns() {
        let mut config = Config::default();
        config.security.secret_detection = false;
        let mut checks = Vec::new();
        check_config_security(&config, &mut checks);
        let check = checks
            .iter()
            .find(|c| c.name == "secret detection")
            .unwrap();
        assert!(matches!(check.status, CheckStatus::Warn(_)));
    }

    #[test]
    fn config_security_empty_blocked_paths_warns() {
        let mut config = Config::default();
        config.security.blocked_paths.clear();
        let mut checks = Vec::new();
        check_config_security(&config, &mut checks);
        let check = checks.iter().find(|c| c.name == "blocked paths").unwrap();
        assert!(matches!(check.status, CheckStatus::Warn(_)));
    }

    #[test]
    fn config_security_browser_no_sandbox_warns() {
        let mut config = Config::default();
        config.browser.enabled = true;
        config.browser.no_sandbox = true;
        let mut checks = Vec::new();
        check_config_security(&config, &mut checks);
        let check = checks.iter().find(|c| c.name == "browser sandbox").unwrap();
        assert!(matches!(check.status, CheckStatus::Warn(_)));
    }

    #[test]
    fn config_security_open_channel_policy_warns() {
        let mut config = Config::default();
        config
            .gateway
            .channel_policies
            .insert("telegram".into(), crate::pairing::DmPolicy::Open);
        let mut checks = Vec::new();
        check_config_security(&config, &mut checks);
        let check = checks
            .iter()
            .find(|c| c.name == "channel policies")
            .unwrap();
        assert!(matches!(check.status, CheckStatus::Warn(ref msg) if msg.contains("telegram")));
    }

    #[test]
    fn config_security_high_rate_limits_warns() {
        let mut config = Config::default();
        config.security.action_limits.tool_calls_block = 1500;
        let mut checks = Vec::new();
        check_config_security(&config, &mut checks);
        let check = checks.iter().find(|c| c.name == "rate limits").unwrap();
        assert!(matches!(check.status, CheckStatus::Warn(_)));
    }

    #[test]
    fn config_security_budget_set_passes() {
        let mut config = Config::default();
        config.budget.monthly_token_limit = 1_000_000;
        let mut checks = Vec::new();
        check_config_security(&config, &mut checks);
        let check = checks.iter().find(|c| c.name.contains("budget")).unwrap();
        assert!(matches!(check.status, CheckStatus::Pass));
    }

    #[test]
    fn run_diagnostics_includes_security_category() {
        let config = Config::default();
        let report = run_diagnostics(&config);
        let categories: std::collections::HashSet<&str> =
            report.checks.iter().map(|c| c.category).collect();
        assert!(categories.contains("Security"));
    }

    #[test]
    fn format_relative_time_seconds() {
        let now = chrono::Utc::now().timestamp();
        assert_eq!(format_relative_time(now - 30), "30 seconds ago");
        assert_eq!(format_relative_time(now - 1), "1 second ago");
    }

    #[test]
    fn format_relative_time_minutes() {
        let now = chrono::Utc::now().timestamp();
        assert_eq!(format_relative_time(now - 60), "1 minute ago");
        assert_eq!(format_relative_time(now - 120), "2 minutes ago");
        assert_eq!(format_relative_time(now - 1800), "30 minutes ago");
    }

    #[test]
    fn format_relative_time_hours() {
        let now = chrono::Utc::now().timestamp();
        assert_eq!(format_relative_time(now - 3600), "1 hour ago");
        assert_eq!(format_relative_time(now - 7200), "2 hours ago");
    }

    #[test]
    fn format_relative_time_days() {
        let now = chrono::Utc::now().timestamp();
        assert_eq!(format_relative_time(now - 86400), "1 day ago");
        assert_eq!(format_relative_time(now - 172800), "2 days ago");
    }

    #[test]
    fn format_relative_time_future() {
        let now = chrono::Utc::now().timestamp();
        assert_eq!(format_relative_time(now + 100), "just now");
    }

    #[test]
    fn run_diagnostics_includes_heartbeat_category() {
        let config = Config::default();
        let report = run_diagnostics(&config);
        let categories: std::collections::HashSet<&str> =
            report.checks.iter().map(|c| c.category).collect();
        assert!(categories.contains("Heartbeat"));
    }

    #[test]
    fn heartbeat_check_shows_interval() {
        let config = Config::default();
        let mut checks = Vec::new();
        check_heartbeat(&config, &mut checks);
        assert!(checks.iter().any(|c| c.name.contains("interval: 60m")));
    }

    #[test]
    fn classify_embedding_lag_thresholds() {
        // (global, entries, expected_status_label, expect_pending_suffix)
        let cases: &[(usize, usize, &str, bool)] = &[
            (0, 0, "pass", false),   // empty DB
            (5, 5, "pass", false),   // fully indexed
            (10, 5, "pass", false),  // count > entries (cache > db)
            (0, 1, "pass", true),    // single-entry race window
            (3, 8, "pass", true),    // gap = 5, exactly at threshold
            (2, 8, "warn", false),   // gap = 6, just over threshold
            (0, 100, "warn", false), // dead embedder
        ];
        for (global, entries, want_status, want_pending) in cases {
            let got = classify_embedding_lag(*global, *entries);
            let status = match got.status {
                CheckStatus::Pass => "pass",
                CheckStatus::Warn(_) => "warn",
                CheckStatus::Fail(_) => "fail",
            };
            assert_eq!(
                status, *want_status,
                "({global}/{entries}) expected {want_status}, got {status}"
            );
            assert_eq!(
                got.name.contains("(pending backfill)"),
                *want_pending,
                "({global}/{entries}) pending-suffix mismatch"
            );
        }
    }

    // ── Subprocesses check ──

    fn find_subprocess_check(report: &DiagnosticReport) -> Option<&DiagnosticCheck> {
        report
            .checks
            .iter()
            .find(|c| c.category == "Subprocesses" && c.name == "Long-running children")
    }

    #[test]
    fn subprocesses_check_absent_without_registry() {
        // The CLI `borg doctor` path (`run_diagnostics`) has no registry
        // handle and MUST NOT produce a misleading "Pass" for a system
        // it couldn't actually inspect.
        let cfg = Config::default();
        let report = run_diagnostics(&cfg);
        assert!(
            find_subprocess_check(&report).is_none(),
            "registry-less doctor must omit the Subprocesses check"
        );
    }

    #[test]
    fn subprocesses_check_passes_when_registry_empty() {
        let cfg = Config::default();
        let reg = ProcessRegistry::new();
        let report = run_diagnostics_with_registry(&cfg, Some(&reg));
        let check = find_subprocess_check(&report).expect("present");
        assert_eq!(check.status, CheckStatus::Pass);
    }

    #[test]
    fn subprocesses_check_passes_when_all_young() {
        let cfg = Config::default();
        let reg = ProcessRegistry::new();
        let _g = reg.register(
            111,
            crate::process_registry::ProcessKind::RunShell,
            "fresh",
            None,
        );
        let report = run_diagnostics_with_registry(&cfg, Some(&reg));
        let check = find_subprocess_check(&report).expect("present");
        assert_eq!(check.status, CheckStatus::Pass);
    }

    #[test]
    fn subprocesses_check_warns_at_warn_threshold() {
        // Threshold = 0s means every running entry is "older than the
        // threshold" — that's the easiest way to drive Warn from a test
        // without a fake clock.
        let mut cfg = Config::default();
        cfg.process_registry.orphan_warn_secs = 0;
        cfg.process_registry.orphan_fail_secs = 1_000_000;
        let reg = ProcessRegistry::new();
        let _g = reg.register(
            222,
            crate::process_registry::ProcessKind::RunShell,
            "long-build",
            None,
        );
        let report = run_diagnostics_with_registry(&cfg, Some(&reg));
        let check = find_subprocess_check(&report).expect("present");
        assert!(
            matches!(check.status, CheckStatus::Warn(_)),
            "expected Warn, got {:?}",
            check.status
        );
        assert_eq!(check.remediation_kind, RemediationKind::Manual);
        assert_eq!(check.remediation_key, Some("subprocesses.long_running"));
    }

    #[test]
    fn subprocesses_check_fails_at_fail_threshold() {
        let mut cfg = Config::default();
        cfg.process_registry.orphan_warn_secs = 0;
        cfg.process_registry.orphan_fail_secs = 0;
        let reg = ProcessRegistry::new();
        let _g = reg.register(
            333,
            crate::process_registry::ProcessKind::RunShell,
            "really-old",
            None,
        );
        let report = run_diagnostics_with_registry(&cfg, Some(&reg));
        let check = find_subprocess_check(&report).expect("present");
        assert!(
            matches!(check.status, CheckStatus::Fail(_)),
            "expected Fail, got {:?}",
            check.status
        );
    }

    #[test]
    fn subprocesses_check_omits_pinned_from_warn_count() {
        // A pinned entry past the warn threshold MUST NOT push the
        // check into Warn — otherwise pinning would be useless.
        let mut cfg = Config::default();
        cfg.process_registry.orphan_warn_secs = 0;
        cfg.process_registry.orphan_fail_secs = 1_000_000;
        let reg = ProcessRegistry::new();
        let g = reg.register(
            444,
            crate::process_registry::ProcessKind::RunShell,
            "pinned-build",
            None,
        );
        reg.pin(g.id()).expect("pin");
        let report = run_diagnostics_with_registry(&cfg, Some(&reg));
        let check = find_subprocess_check(&report).expect("present");
        assert_eq!(
            check.status,
            CheckStatus::Pass,
            "pinned entries must not trigger Warn"
        );
    }

    #[test]
    fn subprocesses_check_message_caps_offender_list() {
        // 5 offenders → first 3 named, then "… and 2 more". This keeps
        // the persistent-warning surface scannable.
        let mut cfg = Config::default();
        cfg.process_registry.orphan_warn_secs = 0;
        cfg.process_registry.orphan_fail_secs = 1_000_000;
        let reg = ProcessRegistry::new();
        let mut guards = Vec::new();
        for i in 0..5 {
            guards.push(reg.register(
                1000 + i,
                crate::process_registry::ProcessKind::RunShell,
                format!("c{i}"),
                None,
            ));
        }
        let report = run_diagnostics_with_registry(&cfg, Some(&reg));
        let check = find_subprocess_check(&report).expect("present");
        if let CheckStatus::Warn(msg) = &check.status {
            assert!(msg.contains("… and 2 more"), "message: {msg}");
        } else {
            panic!("expected Warn, got {:?}", check.status);
        }
    }
}

#[cfg(test)]
mod expanded_check_tests {
    use super::*;
    use crate::db::NewTask;

    fn fails(c: &DiagnosticCheck) -> bool {
        matches!(c.status, CheckStatus::Fail(_))
    }
    fn warns(c: &DiagnosticCheck) -> bool {
        matches!(c.status, CheckStatus::Warn(_))
    }
    fn passes(c: &DiagnosticCheck) -> bool {
        matches!(c.status, CheckStatus::Pass)
    }

    #[test]
    fn settings_check_passes_default_and_fails_out_of_range() {
        let mut checks = Vec::new();
        check_settings(&Config::default(), &mut checks);
        assert_eq!(checks.len(), 1);
        assert!(passes(&checks[0]), "valid defaults must pass");

        let mut bad = Config::default();
        bad.llm.temperature = 5.0; // validate() bails outside 0.0..=2.0
        let mut checks = Vec::new();
        check_settings(&bad, &mut checks);
        assert!(fails(&checks[0]), "out-of-range temperature must fail");
        assert_eq!(checks[0].remediation_kind, RemediationKind::Manual);
    }

    #[test]
    fn credential_pool_classification_covers_all_states() {
        let no_creds = classify_credential_pool("openrouter", false, false, None, 0);
        assert!(fails(&no_creds), "no credentials → fail");

        let ok = classify_credential_pool("openrouter", true, true, None, 0);
        assert!(passes(&ok), "available → pass");

        let cooling = classify_credential_pool("openrouter", true, false, Some(61_000), 1_000);
        assert!(warns(&cooling), "configured but all cooling → warn");
        let CheckStatus::Warn(msg) = &cooling.status else {
            unreachable!()
        };
        assert!(msg.contains("60s"), "warn must estimate recovery: {msg}");
    }

    #[test]
    fn keychain_classification_reflects_backend_and_file_presence() {
        assert!(passes(&classify_keychain(KekBackend::OsKeychain, false)));
        assert!(passes(&classify_keychain(KekBackend::Auto, false)));
        assert!(
            warns(&classify_keychain(KekBackend::File, true)),
            "file backend with blob present → reduced-posture warning",
        );
        assert!(
            fails(&classify_keychain(KekBackend::File, false)),
            "file backend with missing blob → fail (cannot unseal)",
        );
    }

    #[test]
    fn orphan_lock_check_is_autosafe_with_repair_key() {
        let c = orphan_lock_check(420);
        assert!(warns(&c));
        assert_eq!(c.remediation_kind, RemediationKind::AutoSafe);
        assert_eq!(c.remediation_key, Some("locks.clear_orphan"));
        let CheckStatus::Warn(msg) = &c.status else {
            unreachable!()
        };
        assert!(msg.contains("420"), "message names the idle age: {msg}");
    }

    #[test]
    fn service_check_reports_only_when_installed() {
        assert!(
            classify_service(None).is_none(),
            "no unit file → no finding (foreground is normal)",
        );
        let p = std::path::Path::new("/etc/systemd/system/borg.service");
        let c = classify_service(Some(p)).expect("installed → some");
        assert!(
            passes(&c),
            "installed service is informational, never a failure"
        );
    }

    #[test]
    fn tunnel_classification_gates_on_provider_and_binary() {
        // Applicability lives in tunnel_binary: no tunnel / custom command have
        // no fixed binary, so check_tunnel emits nothing for them.
        assert!(tunnel_binary(TunnelProvider::None).is_none());
        assert!(tunnel_binary(TunnelProvider::Custom).is_none());
        assert_eq!(
            tunnel_binary(TunnelProvider::Cloudflare),
            Some("cloudflared")
        );
        // Provider binary missing → fail; present → pass.
        let missing = classify_tunnel(TunnelProvider::Ngrok, "ngrok", false);
        assert!(fails(&missing));
        assert_eq!(missing.remediation_kind, RemediationKind::Manual);
        assert!(passes(&classify_tunnel(
            TunnelProvider::Ngrok,
            "ngrok",
            true
        )));
    }

    #[test]
    fn mcp_label_maps_states_without_false_positives() {
        assert!(fails(&mcp_check_for_label("srv", "auth-failed")));
        assert!(warns(&mcp_check_for_label("srv", "reconnecting")));
        assert!(warns(&mcp_check_for_label("srv", "degraded")));
        assert!(passes(&mcp_check_for_label("srv", "connected")));
        // The CLI has no live registry — "not-started" must NOT be a problem.
        assert!(passes(&mcp_check_for_label("srv", "not-started")));
    }

    #[test]
    fn mcp_check_emits_nothing_without_configured_servers() {
        let mut checks = Vec::new();
        check_mcp(&Config::default(), &mut checks);
        assert!(
            checks.is_empty(),
            "no configured MCP servers → no rows (suppressed section)",
        );
    }

    #[test]
    fn docker_classification_gates_on_use_and_health() {
        // Applicability: the docker skill ships disabled, so a vanilla install
        // is not "in use" and check_docker emits nothing.
        assert!(
            !docker_in_use(&Config::default()),
            "docker skill ships disabled"
        );
        // Severity mapping (only reached once in-use):
        assert!(warns(&classify_docker(false, false)), "no binary → warn");
        assert!(warns(&classify_docker(true, false)), "daemon down → warn");
        assert!(passes(&classify_docker(true, true)), "healthy → pass");
    }

    #[test]
    fn credential_check_applies_only_to_key_providers() {
        use crate::llm::provider::Provider;
        // Key-authenticated cloud provider → applicable.
        assert!(credential_check_applies(Provider::OpenRouter));
        // Local provider has no key pool → not applicable.
        assert!(!credential_check_applies(Provider::Ollama));
        // AWS-credential-chain provider resolves creds elsewhere → not applicable.
        assert!(!credential_check_applies(Provider::Bedrock));
    }

    #[tokio::test]
    async fn scheduled_tasks_warns_only_on_errored_tasks() {
        let db = Database::test_db().await;

        // Clean store: summary pass, no warning.
        let clean = scheduled_task_checks(&db).await;
        assert!(clean.iter().all(passes), "clean store → all pass");
        assert!(
            clean.iter().any(|c| c.name.contains("scheduled task(s)")),
            "must report a task count summary",
        );

        // Seed a task whose last run errored.
        db.create_task(&NewTask {
            id: "boom",
            name: "flaky job",
            prompt: "do stuff",
            schedule_type: "interval",
            schedule_expr: "1h",
            timezone: "local",
            next_run: Some(100),
            max_retries: Some(3),
            timeout_ms: None,
            delivery_channel: None,
            delivery_target: None,
            allowed_tools: None,
            task_type: "prompt",
            pre_run_command: None,
            pre_run_cwd: None,
            pre_run_timeout_ms: None,
            min_interval_secs: None,
        })
        .await
        .expect("create task");
        db.set_task_retry("boom", 1, "connection refused", 0)
            .await
            .expect("set last_error");

        let after = scheduled_task_checks(&db).await;
        let err_warn = after
            .iter()
            .find(|c| c.name == "tasks with errors")
            .expect("errored-task warning present");
        assert!(warns(err_warn), "a task with last_error must warn");
        let CheckStatus::Warn(msg) = &err_warn.status else {
            unreachable!()
        };
        assert!(
            msg.contains("flaky job"),
            "warning names the offending task: {msg}"
        );
    }
}
