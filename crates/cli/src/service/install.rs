//! Platform service install / uninstall / status (macOS launchd + Linux systemd).
//!
//! This module owns everything related to registering the `borg daemon` as a
//! user-level background service. The daemon event loop itself lives in the
//! parent `service` module.

use std::path::PathBuf;

use anyhow::{Context, Result};
use borg_core::config::Config;

pub(super) const LAUNCHD_LABEL: &str = "com.borg.daemon";

pub(super) const LAUNCHD_PLIST_TEMPLATE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{{LABEL}}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{{BINARY_PATH}}</string>
        <string>daemon</string>
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

pub(super) const SYSTEMD_UNIT_TEMPLATE: &str = r#"[Unit]
Description=Borg AI Assistant Daemon
After=network.target

[Service]
Type=simple
ExecStart={{BINARY_PATH}} daemon
Restart=on-failure
RestartSec=5
Environment=HOME={{HOME}}

[Install]
WantedBy=default.target
"#;

/// Ensure the daemon service is installed and running.
pub fn ensure_service_running() -> Result<()> {
    ensure_service_installed()?;

    let is_running = if cfg!(target_os = "macos") {
        std::process::Command::new("launchctl")
            .args(["list", LAUNCHD_LABEL])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    } else if cfg!(target_os = "linux") {
        std::process::Command::new("systemctl")
            .args(["--user", "is-active", "--quiet", "borg.service"])
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    } else {
        return Ok(());
    };

    if !is_running {
        if cfg!(target_os = "macos") {
            if let Ok(plist) = launchd_plist_path() {
                match std::process::Command::new("launchctl")
                    .args(["load", &plist.to_string_lossy()])
                    .output()
                {
                    Ok(output) if !output.status.success() => {
                        let stderr = String::from_utf8_lossy(&output.stderr);
                        tracing::warn!("launchctl load failed: {stderr}");
                    }
                    Err(e) => tracing::warn!("Failed to run launchctl load: {e}"),
                    _ => {}
                }
            }
        } else if cfg!(target_os = "linux") {
            match std::process::Command::new("systemctl")
                .args(["--user", "start", "borg.service"])
                .output()
            {
                Ok(output) if !output.status.success() => {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    tracing::warn!("systemctl start failed: {stderr}");
                }
                Err(e) => tracing::warn!("Failed to run systemctl start: {e}"),
                _ => {}
            }
        }
    }

    Ok(())
}

/// Ensure the daemon service is installed, installing silently if needed.
pub fn ensure_service_installed() -> Result<()> {
    let already_installed = if cfg!(target_os = "macos") {
        launchd_plist_path().map(|p| p.exists()).unwrap_or(false)
    } else if cfg!(target_os = "linux") {
        systemd_unit_path().map(|p| p.exists()).unwrap_or(false)
    } else {
        return Ok(());
    };

    if !already_installed {
        install_service_inner()?;
    } else if service_binary_path_stale() {
        tracing::info!("Service binary path is stale, reinstalling service file");
        install_service_inner()?;
    }
    Ok(())
}

fn service_binary_path_stale() -> bool {
    // Canonicalize to resolve symlinks — prevents false positives when plist uses a
    // symlink path but current_exe() returns the resolved target.
    let current = match std::env::current_exe().and_then(|p| p.canonicalize()) {
        Ok(p) => p.to_string_lossy().to_string(),
        Err(_) => return false,
    };

    let service_content = if cfg!(target_os = "macos") {
        launchd_plist_path()
            .ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
    } else if cfg!(target_os = "linux") {
        systemd_unit_path()
            .ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
    } else {
        None
    };

    match service_content {
        Some(content) => !content.contains(&current),
        None => false,
    }
}

fn install_service_inner() -> Result<()> {
    let binary_path = find_binary_path()?;
    let home =
        dirs::home_dir().ok_or_else(|| anyhow::anyhow!("Could not determine home directory"))?;
    let log_dir = Config::logs_dir()?;
    std::fs::create_dir_all(&log_dir)?;

    if cfg!(target_os = "macos") {
        install_launchd(&binary_path, &home, &log_dir)
    } else if cfg!(target_os = "linux") {
        install_systemd(&binary_path, &home)
    } else {
        anyhow::bail!("Service installation is only supported on macOS and Linux")
    }
}

/// Uninstall the daemon service.
pub fn uninstall_service() -> Result<()> {
    if cfg!(target_os = "macos") {
        uninstall_launchd()
    } else if cfg!(target_os = "linux") {
        uninstall_systemd()
    } else {
        anyhow::bail!("Service management is only supported on macOS and Linux")
    }
}

/// Kill all other borg processes (daemon, gateway, etc.) except ourselves.
/// Best-effort — used during uninstall to prevent a running daemon from
/// recreating `~/.borg/` after we delete it.
pub fn kill_other_borg_processes() {
    let my_pid = std::process::id();
    let output = match std::process::Command::new("pgrep")
        .args(["-x", "borg"])
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
                tracing::info!("Killing borg process {pid}");
                unsafe {
                    libc::kill(pid as libc::pid_t, libc::SIGKILL);
                }
            }
        }
    }
    // Brief pause so the OS can release file handles
    std::thread::sleep(std::time::Duration::from_millis(200));
}

/// Stop the daemon service without uninstalling it.
pub fn stop_service() -> Result<()> {
    if cfg!(target_os = "macos") {
        stop_launchd()
    } else if cfg!(target_os = "linux") {
        stop_systemd()
    } else {
        anyhow::bail!("Service management is only supported on macOS and Linux")
    }
}

/// Restart the daemon service.
pub fn restart_service() -> Result<()> {
    if cfg!(target_os = "macos") {
        restart_launchd()
    } else if cfg!(target_os = "linux") {
        restart_systemd()
    } else {
        anyhow::bail!("Service management is only supported on macOS and Linux")
    }
}

/// Show the daemon service status.
pub fn service_status() -> Result<()> {
    if cfg!(target_os = "macos") {
        status_launchd()
    } else if cfg!(target_os = "linux") {
        status_systemd()
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
        let _ = std::process::Command::new("launchctl")
            .args(["unload", &plist_path.to_string_lossy()])
            .status();
        std::fs::remove_file(&plist_path)?;
    } else {
        println!("Borg not installed.");
    }
    Ok(())
}

fn status_launchd() -> Result<()> {
    let output = std::process::Command::new("launchctl")
        .args(["list", LAUNCHD_LABEL])
        .output()
        .context("Failed to run launchctl list")?;

    if output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        println!("Service status: running");
        println!("{stdout}");
    } else {
        println!("Service status: not running");
    }
    Ok(())
}

fn stop_launchd() -> Result<()> {
    ensure_service_installed()?;
    let plist_path = launchd_plist_path()?;
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

fn restart_launchd() -> Result<()> {
    ensure_service_installed()?;
    let plist_path = launchd_plist_path()?;
    let _ = std::process::Command::new("launchctl")
        .args(["unload", &plist_path.to_string_lossy()])
        .status();
    let status = std::process::Command::new("launchctl")
        .args(["load", &plist_path.to_string_lossy()])
        .status()
        .context("Failed to run launchctl load")?;
    if status.success() {
        println!("Service restarted.");
    } else {
        println!("Failed to restart service.");
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

    let _ = std::process::Command::new("systemctl")
        .args(["--user", "disable", "--now", "borg.service"])
        .status();

    if unit_path.exists() {
        std::fs::remove_file(&unit_path)?;
        let _ = std::process::Command::new("systemctl")
            .args(["--user", "daemon-reload"])
            .status();
        println!("Borg decommissioned.");
    } else {
        println!("Borg not installed.");
    }
    Ok(())
}

fn status_systemd() -> Result<()> {
    let output = std::process::Command::new("systemctl")
        .args(["--user", "status", "borg.service"])
        .output()
        .context("Failed to run systemctl status")?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    println!("{stdout}");
    Ok(())
}

fn stop_systemd() -> Result<()> {
    ensure_service_installed()?;
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

fn restart_systemd() -> Result<()> {
    ensure_service_installed()?;
    let status = std::process::Command::new("systemctl")
        .args(["--user", "restart", "borg.service"])
        .status()
        .context("Failed to run systemctl restart")?;
    if status.success() {
        println!("Service restarted.");
    } else {
        println!("Failed to restart service.");
    }
    Ok(())
}

fn find_binary_path() -> Result<String> {
    // Prefer current executable to avoid finding a different `borg` binary (e.g., BorgBackup)
    if let Ok(exe) = std::env::current_exe() {
        return Ok(exe.to_string_lossy().to_string());
    }

    // Fall back to PATH lookup
    if let Ok(path) = which::which("borg") {
        return Ok(path.to_string_lossy().to_string());
    }

    anyhow::bail!("Could not determine binary path")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launchd_template_substitution() {
        let content = LAUNCHD_PLIST_TEMPLATE
            .replace("{{LABEL}}", LAUNCHD_LABEL)
            .replace("{{BINARY_PATH}}", "/usr/local/bin/borg")
            .replace("{{LOG_DIR}}", "/tmp/logs")
            .replace("{{HOME}}", "/Users/test");

        assert!(content.contains("<string>/usr/local/bin/borg</string>"));
        assert!(content.contains("<string>/tmp/logs/daemon.log</string>"));
        assert!(content.contains("<string>/Users/test</string>"));
        assert!(!content.contains("{{"));
    }

    #[test]
    fn systemd_template_substitution() {
        let content = SYSTEMD_UNIT_TEMPLATE
            .replace("{{BINARY_PATH}}", "/usr/local/bin/borg")
            .replace("{{HOME}}", "/home/test");

        assert!(content.contains("ExecStart=/usr/local/bin/borg daemon"));
        assert!(content.contains("Environment=HOME=/home/test"));
        assert!(!content.contains("{{"));
    }

    #[test]
    fn launchd_plist_is_valid_xml_structure() {
        let content = LAUNCHD_PLIST_TEMPLATE
            .replace("{{LABEL}}", LAUNCHD_LABEL)
            .replace("{{BINARY_PATH}}", "/bin/borg")
            .replace("{{LOG_DIR}}", "/tmp")
            .replace("{{HOME}}", "/home");

        assert!(content.starts_with("<?xml"));
        assert!(content.contains("<plist version=\"1.0\">"));
        assert!(content.contains("</plist>"));
        assert!(content.contains("<key>Label</key>"));
        assert!(content.contains(&format!("<string>{LAUNCHD_LABEL}</string>")));
    }

    #[test]
    fn systemd_unit_has_required_sections() {
        assert!(SYSTEMD_UNIT_TEMPLATE.contains("[Unit]"));
        assert!(SYSTEMD_UNIT_TEMPLATE.contains("[Service]"));
        assert!(SYSTEMD_UNIT_TEMPLATE.contains("[Install]"));
        assert!(SYSTEMD_UNIT_TEMPLATE.contains("WantedBy=default.target"));
        assert!(SYSTEMD_UNIT_TEMPLATE.contains("Restart=on-failure"));
    }

    #[test]
    fn launchd_label_constant() {
        assert_eq!(LAUNCHD_LABEL, "com.borg.daemon");
    }

    #[test]
    fn stale_binary_path_detected_for_launchd() {
        // Test that a plist with a different binary path is detected as stale
        let content = LAUNCHD_PLIST_TEMPLATE
            .replace("{{LABEL}}", LAUNCHD_LABEL)
            .replace("{{BINARY_PATH}}", "/old/path/to/borg")
            .replace("{{LOG_DIR}}", "/tmp/logs")
            .replace("{{HOME}}", "/Users/test");

        // The content should NOT contain the current exe
        let current_exe = std::env::current_exe()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();
        assert!(
            !content.contains(&current_exe),
            "plist with old path should not contain current exe"
        );

        // Verify it does contain the old path
        assert!(content.contains("/old/path/to/borg"));
    }

    #[test]
    fn stale_binary_path_not_detected_when_matching() {
        let current_exe = std::env::current_exe()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| "/usr/local/bin/borg".to_string());

        let content = LAUNCHD_PLIST_TEMPLATE
            .replace("{{LABEL}}", LAUNCHD_LABEL)
            .replace("{{BINARY_PATH}}", &current_exe)
            .replace("{{LOG_DIR}}", "/tmp/logs")
            .replace("{{HOME}}", "/Users/test");

        assert!(
            content.contains(&current_exe),
            "plist with current path should contain current exe"
        );
    }
}
