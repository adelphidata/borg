use crate::config::Config;
use crate::process_registry::ProcessRegistry;

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
            Self::Settings => checks::system::check_settings(config, checks),
            Self::Secrets => checks::security::check_secrets(config, checks),
            Self::Keychain => checks::security::check_keychain(config, checks),
            Self::Sandbox => checks::system::check_sandbox(checks),
            Self::Skills => checks::system::check_skills(config, checks).await,
            Self::Memory => checks::memory::check_memory(config, checks, probe_network).await,
            Self::Embeddings => checks::memory::check_embeddings(config, checks).await,
            Self::Data => checks::data::check_data_dir(checks).await,
            Self::Locks => checks::runtime::check_daemon_lock(checks).await,
            Self::Gateway => checks::network::check_gateway(config, checks),
            Self::Tunnel => checks::network::check_tunnel(config, checks),
            Self::Heartbeat => checks::runtime::check_heartbeat(config, checks).await,
            Self::ScheduledTasks => checks::runtime::check_scheduled_tasks(checks).await,
            Self::Service => checks::system::check_service(checks),
            Self::Budget => checks::runtime::check_budget(config, checks).await,
            Self::PromptCache => checks::runtime::check_prompt_cache(config, checks).await,
            Self::Plugins => checks::system::check_plugins(checks).await,
            Self::Mcp => checks::network::check_mcp(config, checks),
            Self::Docker => checks::system::check_docker(config, checks),
            Self::Browser => checks::system::check_browser(config, checks),
            Self::Agents => checks::runtime::check_agents(config, checks).await,
            Self::Security => checks::security::check_config_security(config, checks),
            Self::SupplyChain => checks::security::check_supply_chain(checks),
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
        checks::runtime::check_subprocesses(config, reg, &mut checks);
    }
    checks::data::check_db_recovery(&mut checks);
    DiagnosticReport { checks }
}

#[cfg(test)]
mod tests;
