//! Launcher for the standalone `borgd` daemon binary.
//!
//! Replaces the legacy `service::*` helpers that spawned the daemon via
//! `borg daemon`. Every helper in this module targets the `borgd` binary —
//! the `borg` CLI no longer hosts the daemon event loop itself.
//!
//! Resolution order for the `borgd` binary:
//! 1. `$BORG_DAEMON_BIN` environment variable.
//! 2. Sibling of the current executable (`current_exe().parent()/borgd`),
//!    which covers both `cargo run -p borg` (target/debug) and release
//!    installs that drop both binaries in the same prefix.
//! 3. `which borgd` (fall back to `PATH`).
//! 4. Bare `"borgd"` and trust the platform loader.
//!
//! All daemon control flows (stop/restart/status) prefer talking to the
//! running daemon over its UDS via `DaemonClient`, falling back to the
//! platform service manager (launchd / systemd) only when the gRPC handshake
//! fails.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use cortex::config::Config;

use crate::daemon_client;

const LAUNCHD_LABEL: &str = "com.borg.daemon";

const LAUNCHD_PLIST_TEMPLATE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{{LABEL}}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{{BINARY_PATH}}</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>StandardOutPath</key>
    <string>{{LOG_DIR}}/daemon.log</string>
    <key>StandardErrorPath</key>
    <string>{{LOG_DIR}}/daemon.err</string>
    <key>EnvironmentVariables</key>
    <dict>
        <key>HOME</key>
        <string>{{HOME}}</string>
    </dict>
</dict>
</plist>"#;

const SYSTEMD_UNIT_TEMPLATE: &str = r#"[Unit]
Description=Borg AI Assistant Daemon
After=network.target

[Service]
Type=simple
ExecStart={{BINARY_PATH}}
Restart=on-failure
RestartSec=5
Environment=HOME={{HOME}}

[Install]
WantedBy=default.target
"#;

/// Locate the `borgd` binary using the resolution order documented at the
/// module level.
fn find_borgd_binary() -> Result<PathBuf> {
    if let Ok(env_path) = std::env::var("BORG_DAEMON_BIN") {
        let p = PathBuf::from(env_path);
        if p.exists() {
            return Ok(p);
        }
        tracing::warn!(
            "BORG_DAEMON_BIN points to {} which does not exist; falling back to default lookup",
            p.display()
        );
    }

    if let Ok(exe) = std::env::current_exe() {
        let sibling = exe.with_file_name("borgd");
        if sibling.exists() {
            return Ok(sibling);
        }
    }

    if let Ok(path) = which::which("borgd") {
        return Ok(path);
    }

    // Last resort — let the OS resolve via PATH at exec time. If that fails,
    // the spawn error will surface a useful message.
    Ok(PathBuf::from("borgd"))
}

/// Probe the daemon UDS. Returns `true` if the socket exists and a
/// `DaemonClient` can connect.
async fn daemon_reachable() -> bool {
    let path = daemon_client::default_socket_path();
    if !path.exists() {
        return false;
    }
    daemon_client::connect(Some(&path)).await.is_ok()
}

/// Wait for the daemon UDS to become reachable, polling at 50ms intervals.
async fn wait_for_daemon(timeout: Duration) -> bool {
    let start = std::time::Instant::now();
    while start.elapsed() < timeout {
        if daemon_reachable().await {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

/// Spawn the `borgd` binary as a detached background process and wait briefly
/// for its UDS to come up. Idempotent: if the daemon is already running, this
/// is a no-op.
pub async fn ensure_running() -> Result<()> {
    if daemon_reachable().await {
        return Ok(());
    }

    let binary = find_borgd_binary()?;
    let mut cmd = std::process::Command::new(&binary);
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    cmd.spawn()
        .with_context(|| format!("spawning {}", binary.display()))?;

    if wait_for_daemon(Duration::from_secs(5)).await {
        Ok(())
    } else {
        anyhow::bail!(
            "spawned {} but its UDS never became reachable",
            binary.display()
        )
    }
}

/// Stop the daemon. Tries `DaemonClient::shutdown()` over the UDS first; if
/// that fails, falls back to the platform service manager.
pub async fn stop_service() -> Result<()> {
    if daemon_reachable().await {
        match daemon_client::connect(None).await {
            Ok(mut client) => match client.shutdown().await {
                Ok(()) => {
                    println!("Service stopped.");
                    return Ok(());
                }
                Err(e) => {
                    tracing::warn!("DaemonClient.shutdown failed, falling back to platform: {e}")
                }
            },
            Err(e) => tracing::warn!("DaemonClient.connect failed, falling back to platform: {e}"),
        }
    }

    platform_stop()
}

/// Restart the daemon: stop (best-effort), wait for the UDS to drop, then
/// ensure a fresh instance is up. Without the wait, `ensure_running` could
/// observe the old daemon's still-open socket and short-circuit.
pub async fn restart_service() -> Result<()> {
    if let Err(e) = stop_service().await {
        tracing::warn!("Stop during restart failed: {e}");
    }
    let drain_deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < drain_deadline {
        if !daemon_reachable().await {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    ensure_running().await
}

/// Print and return the daemon status string.
pub async fn service_status() -> Result<()> {
    if daemon_reachable().await {
        println!("Service status: running");
        if let Ok(mut client) = daemon_client::connect(None).await {
            match client.get_daemon_status().await {
                Ok(status) => println!("{status:?}"),
                Err(e) => tracing::warn!("get_daemon_status failed: {e}"),
            }
        }
    } else {
        println!("Service status: not running");
    }
    Ok(())
}

/// Install the platform service unit pointing at the `borgd` binary.
pub fn install_service() -> Result<()> {
    let binary = find_borgd_binary()?;
    let binary_str = binary.to_string_lossy().to_string();
    let home =
        dirs::home_dir().ok_or_else(|| anyhow::anyhow!("Could not determine home directory"))?;
    let log_dir = Config::logs_dir()?;
    std::fs::create_dir_all(&log_dir)?;

    if cfg!(target_os = "macos") {
        install_launchd(&binary_str, &home, &log_dir)
    } else if cfg!(target_os = "linux") {
        install_systemd(&binary_str, &home)
    } else {
        anyhow::bail!("Service installation is only supported on macOS and Linux")
    }
}

/// Uninstall the platform service unit and kill any leftover daemon
/// processes.
pub fn uninstall_service() -> Result<()> {
    let result = if cfg!(target_os = "macos") {
        uninstall_launchd()
    } else if cfg!(target_os = "linux") {
        uninstall_systemd()
    } else {
        anyhow::bail!("Service management is only supported on macOS and Linux")
    };
    kill_other_borg_processes();
    result
}

/// Best-effort: kill any other `borg` / `borgd` processes (other than this
/// PID). Used during uninstall to prevent a still-running daemon from
/// recreating `~/.borg/` after we delete it.
pub fn kill_other_borg_processes() {
    #[cfg(unix)]
    {
        let my_pid = std::process::id();
        for name in ["borg", "borgd"] {
            let output = match std::process::Command::new("pgrep")
                .args(["-x", name])
                .output()
            {
                Ok(o) => o,
                Err(e) => {
                    tracing::debug!("pgrep not available: {e}");
                    return;
                }
            };
            let stdout = String::from_utf8_lossy(&output.stdout);
            for line in stdout.lines() {
                if let Ok(pid) = line.trim().parse::<u32>() {
                    if pid != my_pid {
                        tracing::info!("Killing {name} process {pid}");
                        unsafe {
                            libc::kill(pid as libc::pid_t, libc::SIGKILL);
                        }
                    }
                }
            }
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

// ── platform stop helpers ──

fn platform_stop() -> Result<()> {
    if cfg!(target_os = "macos") {
        stop_launchd()
    } else if cfg!(target_os = "linux") {
        stop_systemd()
    } else {
        anyhow::bail!("Service management is only supported on macOS and Linux")
    }
}

// ── macOS launchd ──

fn launchd_plist_path() -> Result<PathBuf> {
    let home =
        dirs::home_dir().ok_or_else(|| anyhow::anyhow!("Could not determine home directory"))?;
    Ok(home
        .join("Library")
        .join("LaunchAgents")
        .join(format!("{LAUNCHD_LABEL}.plist")))
}

fn install_launchd(
    binary_path: &str,
    home: &std::path::Path,
    log_dir: &std::path::Path,
) -> Result<()> {
    let plist_path = launchd_plist_path()?;
    if let Some(parent) = plist_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let content = LAUNCHD_PLIST_TEMPLATE
        .replace("{{LABEL}}", LAUNCHD_LABEL)
        .replace("{{BINARY_PATH}}", binary_path)
        .replace("{{LOG_DIR}}", &log_dir.to_string_lossy())
        .replace("{{HOME}}", &home.to_string_lossy());

    std::fs::write(&plist_path, &content)
        .with_context(|| format!("Failed to write plist to {}", plist_path.display()))?;

    let output = std::process::Command::new("launchctl")
        .args(["load", &plist_path.to_string_lossy()])
        .output()
        .context("Failed to run launchctl load")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        tracing::warn!("launchctl load returned non-zero: {stderr}");
    }

    Ok(())
}

fn uninstall_launchd() -> Result<()> {
    let plist_path = launchd_plist_path()?;

    if plist_path.exists() {
        match std::process::Command::new("launchctl")
            .args(["unload", &plist_path.to_string_lossy()])
            .status()
        {
            Ok(s) if !s.success() => tracing::warn!("launchctl unload returned non-zero status"),
            Err(e) => tracing::warn!("launchctl unload failed: {e}"),
            _ => {}
        }
        std::fs::remove_file(&plist_path)?;
    } else {
        println!("Borg not installed.");
    }
    Ok(())
}

fn stop_launchd() -> Result<()> {
    let plist_path = launchd_plist_path()?;
    if !plist_path.exists() {
        println!("Service not installed.");
        return Ok(());
    }
    let status = std::process::Command::new("launchctl")
        .args(["unload", &plist_path.to_string_lossy()])
        .status()
        .context("Failed to run launchctl unload")?;
    if status.success() {
        println!("Service stopped.");
    } else {
        println!("Failed to stop service (it may not be running).");
    }
    Ok(())
}

// ── Linux systemd ──

fn systemd_unit_path() -> Result<PathBuf> {
    let home =
        dirs::home_dir().ok_or_else(|| anyhow::anyhow!("Could not determine home directory"))?;
    Ok(home
        .join(".config")
        .join("systemd")
        .join("user")
        .join("borg.service"))
}

fn install_systemd(binary_path: &str, home: &std::path::Path) -> Result<()> {
    let unit_path = systemd_unit_path()?;
    if let Some(parent) = unit_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let content = SYSTEMD_UNIT_TEMPLATE
        .replace("{{BINARY_PATH}}", binary_path)
        .replace("{{HOME}}", &home.to_string_lossy());

    std::fs::write(&unit_path, &content)
        .with_context(|| format!("Failed to write unit to {}", unit_path.display()))?;

    match std::process::Command::new("systemctl")
        .args(["--user", "daemon-reload"])
        .output()
    {
        Ok(output) if !output.status.success() => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            tracing::warn!("systemctl daemon-reload failed: {stderr}");
        }
        Err(e) => tracing::warn!("Failed to run systemctl daemon-reload: {e}"),
        _ => {}
    }

    let output = std::process::Command::new("systemctl")
        .args(["--user", "enable", "--now", "borg.service"])
        .output()
        .context("Failed to enable service")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        tracing::warn!("systemctl enable --now failed: {stderr}");
    }

    Ok(())
}

fn uninstall_systemd() -> Result<()> {
    let unit_path = systemd_unit_path()?;

    match std::process::Command::new("systemctl")
        .args(["--user", "disable", "--now", "borg.service"])
        .status()
    {
        Ok(s) if !s.success() => {
            tracing::warn!("systemctl disable --now returned non-zero status")
        }
        Err(e) => tracing::warn!("systemctl disable --now failed: {e}"),
        _ => {}
    }

    if unit_path.exists() {
        std::fs::remove_file(&unit_path)?;
        match std::process::Command::new("systemctl")
            .args(["--user", "daemon-reload"])
            .status()
        {
            Ok(s) if !s.success() => {
                tracing::warn!("systemctl daemon-reload returned non-zero status")
            }
            Err(e) => tracing::warn!("systemctl daemon-reload failed: {e}"),
            _ => {}
        }
        println!("Borg decommissioned.");
    } else {
        println!("Borg not installed.");
    }
    Ok(())
}

fn stop_systemd() -> Result<()> {
    let status = std::process::Command::new("systemctl")
        .args(["--user", "stop", "borg.service"])
        .status()
        .context("Failed to run systemctl stop")?;
    if status.success() {
        println!("Service stopped.");
    } else {
        println!("Failed to stop service (it may not be running).");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launchd_template_targets_borgd_binary() {
        let content = LAUNCHD_PLIST_TEMPLATE
            .replace("{{LABEL}}", LAUNCHD_LABEL)
            .replace("{{BINARY_PATH}}", "/usr/local/bin/borgd")
            .replace("{{LOG_DIR}}", "/tmp/logs")
            .replace("{{HOME}}", "/Users/test");

        // Real failure mode: a regression to spawning `borg daemon` (the
        // removed subcommand) would leave installed services dead because
        // dispatch no longer matches `Daemon`. Also guard against the
        // deprecated `--insecure-uds` flag sneaking back in.
        assert!(content.contains("<string>/usr/local/bin/borgd</string>"));
        assert!(!content.contains("--insecure-uds"));
        assert!(!content.contains(">daemon<"));
        assert!(!content.contains("{{"));
    }

    #[test]
    fn systemd_template_targets_borgd_binary() {
        let content = SYSTEMD_UNIT_TEMPLATE
            .replace("{{BINARY_PATH}}", "/usr/local/bin/borgd")
            .replace("{{HOME}}", "/home/test");

        assert!(content.contains("ExecStart=/usr/local/bin/borgd"));
        assert!(!content.contains("--insecure-uds"));
        assert!(!content.contains("borgd daemon"));
        assert!(!content.contains("{{"));
    }

    /// Real-binary smoke: ensure_running() should be idempotent. Gated
    /// `#[ignore]` because it spawns an actual daemon process and binds the
    /// UDS — only meaningful in environments with a built `borgd`.
    #[tokio::test]
    #[ignore]
    async fn ensure_running_is_idempotent() {
        ensure_running().await.expect("first ensure_running");
        ensure_running()
            .await
            .expect("second ensure_running should be a no-op");
        assert!(daemon_reachable().await);
    }
}
