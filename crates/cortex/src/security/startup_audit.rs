//! Warn-on-load security posture audit.
//!
//! Unlike the thorough `borg doctor` host audit ([`crate::host_audit`]) — which
//! the operator has to run explicitly — this fires once at daemon startup and
//! emits `tracing::warn!` for the highest-signal, cheapest-to-check exposure:
//!
//! 1. Running as **root** (POSIX euid 0): the agent's terminal/file tools then
//!    execute with full root privileges, so a single prompt-injection or exposed
//!    endpoint is a whole-host compromise.
//! 2. An SSH daemon with **password authentication** (or root login) enabled:
//!    a brute-forceable login that pairs badly with a root-capable agent box.
//! 3. A **group/world-writable data dir** (`~/.borg`): another user can tamper
//!    with `hooks.json` (whose `PreToolUse` scripts run on every tool call),
//!    skills, or the database.
//!
//! Advisory only — every check is independently fail-safe (a read error yields
//! no warning, never a panic), and the audit never blocks boot or returns an
//! error. Gated by `security.startup_audit` (default on). POSIX checks no-op on
//! non-unix hosts.

use std::path::Path;

/// Effective uid of the current process, or `None` on non-unix.
fn current_euid() -> Option<u32> {
    #[cfg(unix)]
    {
        // SAFETY: `geteuid` is async-signal-safe and has no preconditions.
        Some(unsafe { libc::geteuid() } as u32)
    }
    #[cfg(not(unix))]
    {
        None
    }
}

/// Warn when the process is running as root.
fn root_warning(euid: Option<u32>) -> Option<String> {
    match euid {
        Some(0) => Some(
            "running as ROOT — the agent's terminal/file tools execute with full root \
             privileges, so a single prompt-injection or exposed endpoint is a host-wide \
             compromise. Prefer a non-root user (sandboxed terminal backend / container)."
                .to_string(),
        ),
        _ => None,
    }
}

/// Warn when an SSH daemon has password auth or weak root login enabled, given
/// the `sshd_config` contents. Pure so it is unit-testable without `/etc`.
fn ssh_config_warning_from(content: &str) -> Option<String> {
    let issues = crate::host_audit::parse_ssh_config(content);
    if issues.is_empty() {
        return None;
    }
    Some(format!(
        "weak SSH daemon configuration — {}",
        issues.join("; ")
    ))
}

/// Read `/etc/ssh/sshd_config` (if present) and surface weak settings. Returns
/// `None` when SSH isn't installed or the file can't be read (fail-safe).
fn ssh_password_auth_warning() -> Option<String> {
    if cfg!(not(unix)) {
        return None;
    }
    let content = std::fs::read_to_string("/etc/ssh/sshd_config").ok()?;
    ssh_config_warning_from(&content)
}

/// Warn when `mode` (a unix permission bitset) grants group- or world-write.
/// Pure helper so the bit logic is testable without touching the filesystem.
fn group_or_world_writable(mode: u32) -> bool {
    // 0o020 = group write, 0o002 = other write.
    mode & 0o022 != 0
}

/// Warn when the data dir is group/world-writable (another local user could
/// tamper with hooks, skills, or the DB). `None` on non-unix or on stat error.
fn data_dir_perms_warning(data_dir: &Path) -> Option<String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(data_dir).ok()?.permissions().mode();
        if group_or_world_writable(mode) {
            return Some(format!(
                "data dir {} is group/world-writable (mode {:o}) — another local user could \
                 tamper with hooks.json, skills, or the database. Run `chmod 700 {}`.",
                data_dir.display(),
                mode & 0o777,
                data_dir.display()
            ));
        }
        None
    }
    #[cfg(not(unix))]
    {
        let _ = data_dir;
        None
    }
}

/// Collect all posture warnings for the current host. Each check is fail-safe.
pub fn startup_posture_warnings(data_dir: &Path) -> Vec<String> {
    [
        root_warning(current_euid()),
        ssh_password_auth_warning(),
        data_dir_perms_warning(data_dir),
    ]
    .into_iter()
    .flatten()
    .collect()
}

/// Run the audit and emit each finding via `tracing::warn!`. Advisory only —
/// never blocks boot. Callers gate this on `security.startup_audit`.
pub fn log_startup_posture(data_dir: &Path) {
    for warning in startup_posture_warnings(data_dir) {
        tracing::warn!(target: "borg.security.posture", "security posture: {warning}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_warning_fires_only_for_uid_zero() {
        assert!(root_warning(Some(0)).is_some());
        assert!(root_warning(Some(1000)).is_none());
        assert!(root_warning(None).is_none());
    }

    #[test]
    fn ssh_warning_flags_password_auth_and_root_login() {
        assert!(ssh_config_warning_from("PasswordAuthentication yes\n").is_some());
        assert!(ssh_config_warning_from("PermitRootLogin yes\n").is_some());
    }

    #[test]
    fn ssh_warning_silent_on_hardened_config() {
        let hardened = "PasswordAuthentication no\nPermitRootLogin no\n";
        assert!(ssh_config_warning_from(hardened).is_none());
    }

    #[test]
    fn group_or_world_writable_matches_only_write_bits() {
        assert!(group_or_world_writable(0o777)); // world+group write
        assert!(group_or_world_writable(0o770)); // group write
        assert!(group_or_world_writable(0o702)); // other write
        assert!(!group_or_world_writable(0o700)); // owner-only
        assert!(!group_or_world_writable(0o755)); // group/other read+exec, no write
    }
}
