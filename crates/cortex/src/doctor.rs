use anyhow::Result;

use crate::config::security::KekBackend;
use crate::config::tunnel::TunnelProvider;
use crate::config::Config;
use crate::constants::SECS_PER_DAY;
use crate::db::Database;
use crate::process_registry::ProcessRegistry;
use crate::skills::load_all_skills;

mod checks;
pub mod repair;
mod types;
pub use types::{CheckStatus, DiagnosticCheck, DiagnosticReport, RemediationKind};

/// Stepped diagnostic runner that yields one check category at a time.
/// Used by the TUI to show progressive output.
#[derive(Default)]
pub struct DiagnosticRunner {
    /// Current cursor into [`STEPS`].
    step: usize,
    /// Whether checks may issue outbound network probes (e.g. the memory
    /// provider reachability check). `false` by default so unattended
    /// callers (the maintenance sweep) never trigger egress; the
    /// interactive `borg doctor` / TUI paths opt in via
    /// [`DiagnosticRunner::interactive`].
    probe_network: bool,
}

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

    async fn run(self, config: &Config, checks: &mut Vec<DiagnosticCheck>, probe_network: bool) {
        match self {
            Self::Provider => checks::provider::check_provider(config, checks),
            Self::Credentials => checks::provider::check_llm_credentials(config, checks).await,
            Self::Settings => check_settings(config, checks),
            Self::Secrets => check_secrets(config, checks),
            Self::Keychain => check_keychain(config, checks),
            Self::Sandbox => check_sandbox(checks),
            Self::Skills => check_skills(config, checks).await,
            Self::Memory => checks::memory::check_memory(config, checks, probe_network).await,
            Self::Embeddings => checks::memory::check_embeddings(config, checks).await,
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
    /// Create a new diagnostic runner starting at step 0. Network probes
    /// are disabled — safe for unattended callers.
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a runner for an interactive `borg doctor` / TUI run, where
    /// outbound reachability probes (memory provider) are appropriate.
    pub fn interactive() -> Self {
        Self {
            step: 0,
            probe_network: true,
        }
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
            kind.run(config, &mut checks, self.probe_network).await;
            if kind.suppress_when_empty() && checks.is_empty() {
                continue;
            }
            return Some((kind.label(), checks));
        }
        None
    }
}

/// Run all diagnostic checks and return a report. This is the interactive
/// `borg doctor` CLI entry point, so it enables outbound reachability
/// probes (e.g. the memory provider health check).
pub async fn run_diagnostics(config: &Config) -> DiagnosticReport {
    run_diagnostics_with_registry_opts(config, None, true).await
}

/// Run all diagnostic checks plus the `Subprocesses` check when the
/// caller has a live registry. Used by the daemon's maintenance sweep and
/// `Admin.Heal` RPC — these are unattended, so network probes stay
/// disabled to avoid recurring background egress.
pub async fn run_diagnostics_with_registry(
    config: &Config,
    registry: Option<&ProcessRegistry>,
) -> DiagnosticReport {
    run_diagnostics_with_registry_opts(config, registry, false).await
}

/// Shared implementation. `probe_network` gates outbound reachability
/// probes so the unattended maintenance path never triggers egress while
/// the interactive CLI does.
async fn run_diagnostics_with_registry_opts(
    config: &Config,
    registry: Option<&ProcessRegistry>,
    probe_network: bool,
) -> DiagnosticReport {
    let mut runner = if probe_network {
        DiagnosticRunner::interactive()
    } else {
        DiagnosticRunner::new()
    };
    let mut checks = Vec::new();
    while let Some((_label, step_checks)) = runner.next_step(config).await {
        checks.extend(step_checks);
    }
    if let Some(reg) = registry {
        check_subprocesses(config, reg, &mut checks);
    }
    check_db_recovery(&mut checks);
    DiagnosticReport { checks }
}

/// Surface an automatic database recovery that happened earlier in this
/// process: the live DB was unreadable on open, so it was preserved aside and
/// rebuilt empty. Non-draining (`peek`) so the boot path's user-facing notice
/// still fires. Passes silently when no recovery occurred.
fn check_db_recovery(checks: &mut Vec<DiagnosticCheck>) {
    checks.push(db_recovery_check(
        crate::db::Database::peek_recovery_notice(),
    ));
}

/// Pure mapping from a recovery notice to a diagnostic check, split out so the
/// warn/pass rendering is testable without poking the process-global.
fn db_recovery_check(notice: Option<crate::db::RecoveryNotice>) -> DiagnosticCheck {
    match notice {
        Some(notice) => DiagnosticCheck::warn(
            "Database",
            "Auto-recovered",
            format!(
                "database was unreadable and rebuilt empty; the original is preserved at {}",
                notice.backup_path.display()
            ),
        ),
        None => DiagnosticCheck::pass("Database", "Integrity"),
    }
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

#[cfg(test)]
mod tests;

#[cfg(test)]
mod expanded_check_tests;
