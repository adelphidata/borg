//! Host/system environment checks: settings validity, sandbox tooling,
//! skills, service install, plugins, docker, and browser automation.

use crate::config::Config;
use crate::db::Database;
use crate::doctor::{DiagnosticCheck, RemediationKind};
use crate::skills::load_all_skills;

/// Settings validity: runs the same `Config::validate()` the loader runs so a
/// fatal misconfiguration (out-of-range temperature, zero token caps) surfaces
/// as a failing check rather than a silent clamp at boot. Always applicable.
pub(in crate::doctor) fn check_settings(config: &Config, checks: &mut Vec<DiagnosticCheck>) {
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

pub(in crate::doctor) fn check_sandbox(checks: &mut Vec<DiagnosticCheck>) {
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

pub(in crate::doctor) async fn check_skills(config: &Config, checks: &mut Vec<DiagnosticCheck>) {
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

pub(in crate::doctor) fn check_service(checks: &mut Vec<DiagnosticCheck>) {
    let found = service_unit_paths().into_iter().find(|p| p.exists());
    if let Some(check) = classify_service(found.as_deref()) {
        checks.push(check);
    }
}

pub(in crate::doctor) async fn check_plugins(checks: &mut Vec<DiagnosticCheck>) {
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

pub(in crate::doctor) fn check_docker(config: &Config, checks: &mut Vec<DiagnosticCheck>) {
    if !docker_in_use(config) {
        return;
    }
    let binary_present = which::which("docker").is_ok();
    let daemon_up = binary_present && docker_daemon_up();
    checks.push(classify_docker(binary_present, daemon_up));
}

pub(in crate::doctor) fn check_browser(config: &Config, checks: &mut Vec<DiagnosticCheck>) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doctor::CheckStatus;

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
    fn browser_disabled_produces_warn() {
        let mut checks = Vec::new();
        let mut config = Config::default();
        config.browser.enabled = false;
        check_browser(&config, &mut checks);
        assert_eq!(checks.len(), 1);
        assert!(matches!(checks[0].status, CheckStatus::Warn(_)));
        assert_eq!(checks[0].category, "Browser");
    }
}
