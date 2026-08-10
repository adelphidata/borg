//! Launcher for the standalone `borgd` daemon binary.
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
#[cfg(unix)]
use cortex::util::unix_ext::set_unix_mode;

use crate::daemon_client;

// Single-sourced with the `borg doctor` service-install check (cortex).
const LAUNCHD_LABEL: &str = cortex::constants::LAUNCHD_SERVICE_LABEL;
#[cfg(test)]
const BORGD_INSTALL_PATH: &str = "/usr/local/bin/borgd";

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

/// Default systemd unit (user scope). Used when the file-encrypted KEK
/// backend is in effect (i.e. headless servers or non-Debian Linux).
/// Pulls `BORG_KEK_PASSPHRASE` from a 0600 EnvironmentFile alongside the
/// encrypted blob so the daemon can survive restarts unattended.
const SYSTEMD_UNIT_TEMPLATE: &str = r#"[Unit]
Description=Borg AI Assistant Daemon
After=network.target

[Service]
Type=simple
ExecStart={{BINARY_PATH}}
Restart=on-failure
RestartSec=5
Environment=HOME={{HOME}}
EnvironmentFile=-{{HOME}}/.borg/secrets/kek.env
EnvironmentFile=-{{HOME}}/.borg/.env

[Install]
WantedBy=default.target
"#;

/// System-wide systemd unit. Used when `borg service install` is run as
/// root (the modal "fresh DigitalOcean / Hetzner droplet, ssh in as root,
/// install borg" deploy shape). `systemctl --user` requires a logind
/// user manager that root SSH sessions don't reliably get, so we drop
/// to a `[Install] WantedBy=multi-user.target` system unit instead.
const SYSTEMD_SYSTEM_UNIT_TEMPLATE: &str = r#"[Unit]
Description=Borg AI Assistant Daemon
After=network.target

[Service]
Type=simple
User=root
ExecStart={{BINARY_PATH}}
Restart=on-failure
RestartSec=5
Environment=HOME={{HOME}}
EnvironmentFile=-{{HOME}}/.borg/secrets/kek.env
EnvironmentFile=-{{HOME}}/.borg/.env

[Install]
WantedBy=multi-user.target
"#;

/// `--secure` systemd unit. The unit owns its own dbus + keyring
/// lifecycle — no reliance on a graphical login session.
///
/// Pattern: `dbus-run-session` opens a fresh session bus and runs the
/// child shell *inside* it (so `DBUS_SESSION_BUS_ADDRESS` is set). The
/// shell then:
///   1. starts `gnome-keyring-daemon --foreground --components=secrets`
///      backgrounded with `&` so it stays alive for the daemon's life;
///      passphrase fed via stdin from `BORG_KEYRING_PASS`;
///   2. `exec`s `borgd`, replacing the shell — the dbus session bus
///      keeps running because `dbus-run-session` waits on the *exec'd*
///      process (i.e. borgd) rather than the original shell.
///
/// `BORG_KEYRING_PASS` is loaded from a 0600 `EnvironmentFile`, never
/// embedded in the unit text or written to a world-readable path.
///
/// **Verification gap:** the unit-template tests assert string presence,
/// not behavior. Verify on a real Debian/Ubuntu VPS before relying on
/// `--secure` for paired-device deploys; tracked in `docs/ROADMAP.md`
/// (Headless KEK — passphrase lifecycle).
const SYSTEMD_UNIT_SECURE_TEMPLATE: &str = r#"[Unit]
Description=Borg AI Assistant Daemon (secure / gnome-keyring)
After=network.target

[Service]
Type=simple
Environment=HOME={{HOME}}
EnvironmentFile={{HOME}}/.borg/secrets/keyring.env
EnvironmentFile=-{{HOME}}/.borg/.env
ExecStart=/usr/bin/dbus-run-session -- /bin/sh -c 'test -n "$BORG_KEYRING_PASS" || { echo "BORG_KEYRING_PASS unset; refusing to unlock keyring with empty passphrase" >&2; exit 78; }; printf "%s" "$BORG_KEYRING_PASS" | gnome-keyring-daemon --foreground --components=secrets --unlock >/dev/null 2>&1 & exec {{BINARY_PATH}}'
Restart=on-failure
RestartSec=5

[Install]
WantedBy=default.target
"#;

/// System-wide variant of the secure template. Same dbus / keyring
/// wiring; differs only in `User=root` and the install target.
const SYSTEMD_SYSTEM_UNIT_SECURE_TEMPLATE: &str = r#"[Unit]
Description=Borg AI Assistant Daemon (secure / gnome-keyring)
After=network.target

[Service]
Type=simple
User=root
Environment=HOME={{HOME}}
EnvironmentFile={{HOME}}/.borg/secrets/keyring.env
EnvironmentFile=-{{HOME}}/.borg/.env
ExecStart=/usr/bin/dbus-run-session -- /bin/sh -c 'test -n "$BORG_KEYRING_PASS" || { echo "BORG_KEYRING_PASS unset; refusing to unlock keyring with empty passphrase" >&2; exit 78; }; printf "%s" "$BORG_KEYRING_PASS" | gnome-keyring-daemon --foreground --components=secrets --unlock >/dev/null 2>&1 & exec {{BINARY_PATH}}'
Restart=on-failure
RestartSec=5

[Install]
WantedBy=multi-user.target
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

/// Directory a freshly spawned `borgd` should start in, or `None` to inherit.
///
/// Anchoring to the data dir matters because `borgd` outlives the client that
/// spawned it and falls back to its own cwd for any session that sends no
/// workspace. Inheriting the client's directory would let `borg --worktree x`
/// on a cold start pin the daemon inside that worktree for every later
/// heartbeat, scheduled, and web session — and leave it rooted in a directory
/// that no longer exists once the worktree is removed.
fn daemon_spawn_dir() -> Option<std::path::PathBuf> {
    cortex::paths::resolve_borg_home_readonly(None)
        .ok()
        .filter(|home| home.is_dir())
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
    if let Some(dir) = daemon_spawn_dir() {
        cmd.current_dir(dir);
    }
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
    // Bounded so a wedged daemon that never closes its socket still lets the
    // restart proceed rather than hanging the CLI indefinitely.
    let drain_deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < drain_deadline {
        if !daemon_reachable().await {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    ensure_running().await
}

/// Start the daemon under the platform service manager if a unit is
/// installed; otherwise spawn it directly via `ensure_running`. Idempotent
/// when the daemon is already up.
pub async fn start_service() -> Result<()> {
    if daemon_reachable().await {
        println!("Service already running.");
        return Ok(());
    }
    // Prefer the platform service manager when a unit/plist is installed —
    // that way subsequent `borg service status` sees the managed instance
    // and the daemon survives logout via systemd lingering / launchd auto-load.
    if platform_unit_installed() {
        if let Err(e) = platform_start() {
            tracing::warn!("platform_start failed, falling back to direct spawn: {e}");
            ensure_running().await?;
        }
        if wait_for_daemon(Duration::from_secs(10)).await {
            println!("Service started.");
            return Ok(());
        }
        anyhow::bail!("started service but its UDS never became reachable");
    }
    ensure_running().await?;
    println!("Service started (direct spawn — no installed unit detected).");
    Ok(())
}

/// State of the platform service manager's view of borgd. Combined with
/// UDS reachability this disambiguates "running normally" / "unit loaded
/// but the daemon is wedged" / "unit not installed" — three operator
/// runbooks that previously all looked like "not running".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlatformManagerState {
    /// Unit/plist installed AND the service manager reports it active.
    LoadedActive,
    /// Unit/plist installed but inactive (failed / stopped / never started).
    LoadedInactive,
    /// No unit/plist installed.
    NotInstalled,
    /// Platform probe failed (binary missing, command error) — usually a
    /// signal the service manager itself is unreachable, not borgd.
    Unknown,
}

/// Build the human-readable status line from already-gathered facts.
///
/// Pure function — no I/O. Tests cover the matrix so the runbook string
/// each operator sees never silently drifts.
pub fn format_service_status(uds_reachable: bool, platform: PlatformManagerState) -> String {
    match (uds_reachable, platform) {
        (true, PlatformManagerState::LoadedActive) => "Service status: running".into(),
        (true, PlatformManagerState::LoadedInactive) => {
            "Service status: running (orphaned — installed unit reports inactive; run `borg service install` to re-link)".into()
        }
        (true, PlatformManagerState::NotInstalled) => {
            "Service status: running (direct spawn — no installed unit; run `borg service install` to manage via systemd/launchd)".into()
        }
        (true, PlatformManagerState::Unknown) => "Service status: running".into(),
        (false, PlatformManagerState::LoadedActive) => {
            "Service status: loaded but unreachable (manager reports active but UDS down — daemon is wedged; try `borg service restart`)".into()
        }
        (false, PlatformManagerState::LoadedInactive) => {
            "Service status: stopped (unit installed; start with `borg service start`)".into()
        }
        (false, PlatformManagerState::NotInstalled) => {
            "Service status: not installed (install with `borg service install`)".into()
        }
        (false, PlatformManagerState::Unknown) => "Service status: not running".into(),
    }
}

/// Print the daemon status, combining UDS reachability with the platform
/// service manager's view.
pub async fn service_status() -> Result<()> {
    let uds = daemon_reachable().await;
    let platform = probe_platform_manager();
    println!("{}", format_service_status(uds, platform));
    if uds {
        if let Ok(mut client) = daemon_client::connect(None).await {
            match client.get_daemon_status().await {
                Ok(status) => {
                    // Explicit field render: avoids `{:?}` on a tonic-generated
                    // struct so future fields don't leak by default if the
                    // proto grows secret-bearing entries.
                    println!("  version:           {}", status.version);
                    println!("  uptime:            {}s", status.uptime_secs);
                    println!("  live sessions:     {}", status.live_session_count);
                    println!(
                        "  llm:               {} / {}",
                        status.llm_provider, status.llm_model
                    );
                    println!(
                        "  tasks:             {} active, {} paused",
                        status.active_tasks, status.paused_tasks
                    );
                    println!("  pending pairings:  {}", status.pending_pairings);
                }
                Err(e) => tracing::warn!("get_daemon_status failed: {e}"),
            }
        }
    }
    Ok(())
}

/// True when the relevant platform unit/plist file exists on disk.
fn platform_unit_installed() -> bool {
    if cfg!(target_os = "macos") {
        launchd_plist_path().map(|p| p.exists()).unwrap_or(false)
    } else if cfg!(target_os = "linux") {
        for scope in [SystemdScope::System, SystemdScope::User] {
            if let Ok(path) = scope.unit_path() {
                if path.exists() {
                    return true;
                }
            }
        }
        false
    } else {
        false
    }
}

/// Probe the platform service manager for borgd's state. Pure I/O wrapper;
/// the decision matrix lives in [`format_service_status`].
fn probe_platform_manager() -> PlatformManagerState {
    if !platform_unit_installed() {
        return PlatformManagerState::NotInstalled;
    }
    if cfg!(target_os = "macos") {
        probe_launchd()
    } else if cfg!(target_os = "linux") {
        probe_systemd()
    } else {
        PlatformManagerState::Unknown
    }
}

#[cfg(target_os = "macos")]
fn probe_launchd() -> PlatformManagerState {
    let Ok(output) = std::process::Command::new("launchctl").arg("list").output() else {
        return PlatformManagerState::Unknown;
    };
    if !output.status.success() {
        return PlatformManagerState::Unknown;
    }
    let listing = String::from_utf8_lossy(&output.stdout);
    // `launchctl list` rows look like:  PID  Status  Label
    // PID is `-` when loaded but not currently running. A live PID means
    // the agent IS running (or being relaunched by KeepAlive).
    for line in listing.lines() {
        if !line.contains(LAUNCHD_LABEL) {
            continue;
        }
        let mut cols = line.split_whitespace();
        let pid_col = cols.next().unwrap_or("-");
        return if pid_col == "-" {
            PlatformManagerState::LoadedInactive
        } else {
            PlatformManagerState::LoadedActive
        };
    }
    PlatformManagerState::LoadedInactive
}

#[cfg(not(target_os = "macos"))]
fn probe_launchd() -> PlatformManagerState {
    PlatformManagerState::Unknown
}

#[cfg(target_os = "linux")]
fn probe_systemd() -> PlatformManagerState {
    let scope = SystemdScope::detect();
    let mut cmd = std::process::Command::new("systemctl");
    cmd.args(scope.ctl_args())
        .args(["is-active", "borg.service"]);
    let Ok(output) = cmd.output() else {
        return PlatformManagerState::Unknown;
    };
    let state = String::from_utf8_lossy(&output.stdout).trim().to_string();
    // `is-active` exit codes: 0 = active, 3 = inactive/failed. Read the
    // stdout token so we treat `activating` and `reloading` as inactive
    // for the purposes of "should the operator wait or restart?".
    match state.as_str() {
        "active" => PlatformManagerState::LoadedActive,
        "" => PlatformManagerState::Unknown,
        _ => PlatformManagerState::LoadedInactive,
    }
}

#[cfg(not(target_os = "linux"))]
fn probe_systemd() -> PlatformManagerState {
    PlatformManagerState::Unknown
}

/// Start the daemon through the platform service manager (no direct spawn).
/// Returns `Err` if the manager command itself fails; the caller may then
/// fall back to direct spawn.
fn platform_start() -> Result<()> {
    if cfg!(target_os = "macos") {
        let plist = launchd_plist_path()?;
        let status = std::process::Command::new("launchctl")
            .args(["load", &plist.to_string_lossy()])
            .status()
            .context("launchctl load")?;
        if !status.success() {
            anyhow::bail!("launchctl load exited with {status}");
        }
        let status = std::process::Command::new("launchctl")
            .args(["start", LAUNCHD_LABEL])
            .status()
            .context("launchctl start")?;
        if !status.success() {
            anyhow::bail!("launchctl start {LAUNCHD_LABEL} exited with {status}");
        }
        Ok(())
    } else if cfg!(target_os = "linux") {
        let scope = SystemdScope::detect();
        let mut cmd = std::process::Command::new("systemctl");
        cmd.args(scope.ctl_args()).args(["start", "borg.service"]);
        let status = cmd.status().context("systemctl start")?;
        if !status.success() {
            anyhow::bail!("systemctl start borg.service exited with {status}");
        }
        Ok(())
    } else {
        anyhow::bail!("platform_start: unsupported OS")
    }
}

/// Install the platform service unit pointing at the `borgd` binary.
///
/// `secure` only affects Linux: when true, the installer attempts to set
/// up gnome-keyring + dbus-user-session so the daemon stores its KEK in
/// the OS secret-service backend (posture: `Default` / `Boosted`). On
/// macOS it is a no-op since the OS keychain is always available. On
/// non-Debian Linux, `secure = true` falls back to the file-encrypted
/// backend with a warning rather than failing — the install always
/// succeeds end-to-end.
pub fn install_service(secure: bool) -> Result<()> {
    let binary = find_borgd_binary()?;
    let binary_str = binary.to_string_lossy().to_string();
    let home =
        dirs::home_dir().ok_or_else(|| anyhow::anyhow!("Could not determine home directory"))?;
    let log_dir = Config::logs_dir()?;
    std::fs::create_dir_all(&log_dir)?;

    if cfg!(target_os = "macos") {
        install_launchd(&binary_str, &home, &log_dir)
    } else if cfg!(target_os = "linux") {
        let mode = if secure {
            resolve_secure_install_mode()
        } else {
            LinuxInstallMode::FileBackend
        };
        install_systemd(&binary_str, &home, mode)
    } else {
        anyhow::bail!("Service installation is only supported on macOS and Linux")
    }
}

/// Linux install variants. Affects which systemd unit is templated and
/// which auxiliary EnvironmentFile is provisioned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LinuxInstallMode {
    /// File-encrypted KEK backend. Always-works fallback.
    FileBackend,
    /// gnome-keyring backend. Only chosen when apt + gnome-keyring +
    /// dbus-user-session install successfully.
    SecureKeyring,
}

/// Decide which mode `--secure` resolves to on Linux. Falls back to
/// `FileBackend` (with a warning) on non-Debian or apt failure so the
/// install never errors out — matches the user's "always boots"
/// requirement.
fn resolve_secure_install_mode() -> LinuxInstallMode {
    if !is_debian_family() {
        tracing::warn!(
            "borg service install --secure: distro is not Debian/Ubuntu; \
             falling back to file-encrypted KEK backend (posture will be Reduced)."
        );
        eprintln!(
            "borg service install --secure: this distro is not Debian/Ubuntu. \
             Falling back to the file-encrypted KEK backend so the daemon still \
             boots. Posture will be Reduced (×0.85 XP)."
        );
        return LinuxInstallMode::FileBackend;
    }
    match install_secure_dependencies_apt() {
        Ok(()) => LinuxInstallMode::SecureKeyring,
        Err(e) => {
            tracing::warn!(
                "borg service install --secure: apt setup failed ({e}); \
                 falling back to file-encrypted KEK backend."
            );
            eprintln!(
                "borg service install --secure: apt failed ({e}). \
                 Falling back to the file-encrypted KEK backend. Posture will be Reduced."
            );
            LinuxInstallMode::FileBackend
        }
    }
}

/// True when `/etc/os-release` declares Debian or an `ID_LIKE` derivative.
fn is_debian_family() -> bool {
    let Ok(release) = std::fs::read_to_string("/etc/os-release") else {
        return false;
    };
    for line in release.lines() {
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let v = v.trim_matches('"');
        if k == "ID" && (v == "debian" || v == "ubuntu") {
            return true;
        }
        if k == "ID_LIKE" && (v.contains("debian") || v.contains("ubuntu")) {
            return true;
        }
    }
    false
}

/// `apt-get install -y` the gnome-keyring stack + dbus session bus, then
/// enable lingering for the current user so user systemd survives logout.
/// Each step uses sudo if not running as root. Returns `Err` on any
/// failure so the caller can fall back cleanly.
fn install_secure_dependencies_apt() -> Result<()> {
    fn run(cmd: &mut std::process::Command) -> Result<()> {
        let status = cmd.status().with_context(|| format!("running {cmd:?}"))?;
        if !status.success() {
            anyhow::bail!("{cmd:?} exited with {status}");
        }
        Ok(())
    }
    let sudo = if is_root() { vec![] } else { vec!["sudo"] };
    let mut env = sudo.clone();
    env.extend_from_slice(&["env", "DEBIAN_FRONTEND=noninteractive"]);

    // `apt-get update` is informational; treat its failure as non-fatal so
    // an offline cache + already-cached packages still works.
    let mut update = std::process::Command::new(env[0]);
    update.args(&env[1..]).arg("apt-get").arg("update");
    if let Err(e) = run(&mut update) {
        tracing::warn!("apt-get update failed (continuing): {e}");
    }

    let mut install = std::process::Command::new(env[0]);
    install.args(&env[1..]).args([
        "apt-get",
        "install",
        "-y",
        "--no-install-recommends",
        "gnome-keyring",
        "dbus-user-session",
        "libpam-gnome-keyring",
    ]);
    run(&mut install)?;

    // Enable lingering so the user systemd instance — and our user-level
    // borg.service — keep running after the SSH session that installed
    // them disconnects. Without this the daemon dies the moment the
    // installer logs out, which is the modal VPS deploy flow.
    let user = std::env::var("USER").unwrap_or_else(|_| "root".to_string());
    let mut linger = std::process::Command::new(if is_root() { "loginctl" } else { "sudo" });
    if !is_root() {
        linger.arg("loginctl");
    }
    linger.args(["enable-linger", &user]);
    if let Err(e) = run(&mut linger) {
        tracing::warn!("loginctl enable-linger failed (continuing): {e}");
    }

    Ok(())
}

#[cfg(unix)]
fn is_root() -> bool {
    // SAFETY: `geteuid` has no preconditions; it returns the effective UID
    // of the current process. No memory or thread invariants involved.
    unsafe { libc::geteuid() == 0 }
}
#[cfg(not(unix))]
fn is_root() -> bool {
    false
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

/// Provision the Tier 2 Windows sandbox (`--windows-sandbox`). Creates
/// the sandbox accounts, seals their passwords, installs the WFP
/// outbound block, and writes `setup.json`. Requires an elevated
/// (Administrator) terminal.
#[cfg(target_os = "windows")]
pub fn install_windows_sandbox() -> Result<()> {
    let installed_at = chrono::Local::now().to_rfc3339();
    sandbox::windows::elevated::install::install_windows_sandbox(&installed_at)
        .context("install Windows Tier 2 sandbox")?;
    println!(
        "Windows Tier 2 sandbox installed (BorgSandboxOnline / BorgSandboxOffline). \
         The daemon auto-promotes to Tier 2 on next launch."
    );
    Ok(())
}

/// Non-Windows: `--windows-sandbox` is unsupported.
#[cfg(not(target_os = "windows"))]
pub fn install_windows_sandbox() -> Result<()> {
    anyhow::bail!("--windows-sandbox is only supported on Windows")
}

/// Tear down the Tier 2 Windows sandbox (`--windows-sandbox` on
/// uninstall). Removes WFP rules, the sandbox accounts, sealed
/// credentials, and `setup.json`.
#[cfg(target_os = "windows")]
pub fn uninstall_windows_sandbox() -> Result<()> {
    sandbox::windows::elevated::install::uninstall_windows_sandbox()
        .context("uninstall Windows Tier 2 sandbox")?;
    println!("Windows Tier 2 sandbox removed; the daemon falls back to Tier 1.");
    Ok(())
}

/// Non-Windows: `--windows-sandbox` is unsupported.
#[cfg(not(target_os = "windows"))]
pub fn uninstall_windows_sandbox() -> Result<()> {
    anyhow::bail!("--windows-sandbox is only supported on Windows")
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
                        // SAFETY: `libc::kill` is a pure FFI call; passing a
                        // numeric pid + signal has no memory preconditions.
                        unsafe {
                            libc::kill(pid as libc::pid_t, libc::SIGKILL);
                        }
                    }
                }
            }
        }
        // SIGKILL is asynchronous: give the kernel a moment to reap before the
        // caller deletes `~/.borg/`, or a dying daemon can recreate it.
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
        let status = std::process::Command::new("launchctl")
            .args(["unload", &plist_path.to_string_lossy()])
            .status();
        warn_on_command_failure(status, "launchctl unload");
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

/// systemd scope. Root installs land in the system manager so the
/// daemon survives ssh logout without needing `loginctl enable-linger`;
/// non-root installs use the per-user manager.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SystemdScope {
    System,
    User,
}

impl SystemdScope {
    fn detect() -> Self {
        if is_root() {
            Self::System
        } else {
            Self::User
        }
    }

    fn unit_path(self) -> Result<PathBuf> {
        let unit = cortex::constants::SYSTEMD_UNIT_NAME;
        match self {
            Self::System => Ok(PathBuf::from("/etc/systemd/system").join(unit)),
            Self::User => {
                let home = dirs::home_dir()
                    .ok_or_else(|| anyhow::anyhow!("Could not determine home directory"))?;
                Ok(home.join(".config").join("systemd").join("user").join(unit))
            }
        }
    }

    fn ctl_args(self) -> &'static [&'static str] {
        match self {
            Self::System => &[],
            Self::User => &["--user"],
        }
    }
}

fn install_systemd(
    binary_path: &str,
    home: &std::path::Path,
    mode: LinuxInstallMode,
) -> Result<()> {
    let scope = SystemdScope::detect();
    let unit_path = scope.unit_path()?;
    if let Some(parent) = unit_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // Provision the appropriate EnvironmentFile *before* the unit goes
    // active. Both files end up at 0600 and live under `~/.borg/secrets/`.
    // The daemon's auto-resolver picks the right backend on first start
    // (and persists it via the borgd-side `persist_resolved_backend`); the
    // cli intentionally does not write `security.kek_backend` here so it
    // never opens the SQLite writer the daemon owns.
    match mode {
        LinuxInstallMode::FileBackend => {
            provision_kek_env_file(home)
                .context("provision file-KEK passphrase EnvironmentFile")?;
        }
        LinuxInstallMode::SecureKeyring => {
            provision_keyring_env_file(home)
                .context("provision keyring auto-unlock EnvironmentFile")?;
        }
    }

    let template = match (scope, mode) {
        (SystemdScope::User, LinuxInstallMode::FileBackend) => SYSTEMD_UNIT_TEMPLATE,
        (SystemdScope::User, LinuxInstallMode::SecureKeyring) => SYSTEMD_UNIT_SECURE_TEMPLATE,
        (SystemdScope::System, LinuxInstallMode::FileBackend) => SYSTEMD_SYSTEM_UNIT_TEMPLATE,
        (SystemdScope::System, LinuxInstallMode::SecureKeyring) => {
            SYSTEMD_SYSTEM_UNIT_SECURE_TEMPLATE
        }
    };
    let content = template
        .replace("{{BINARY_PATH}}", binary_path)
        .replace("{{HOME}}", &home.to_string_lossy());

    std::fs::write(&unit_path, &content)
        .with_context(|| format!("Failed to write unit to {}", unit_path.display()))?;

    let mut reload = std::process::Command::new("systemctl");
    reload.args(scope.ctl_args()).arg("daemon-reload");
    match reload.output() {
        Ok(output) if !output.status.success() => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            tracing::warn!("systemctl daemon-reload failed: {stderr}");
        }
        Err(e) => tracing::warn!("Failed to run systemctl daemon-reload: {e}"),
        _ => {}
    }

    let mut enable = std::process::Command::new("systemctl");
    enable
        .args(scope.ctl_args())
        .args(["enable", "--now", "borg.service"]);
    let output = enable.output().context("Failed to enable service")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "systemctl {}enable --now borg.service failed: {stderr}",
            if scope == SystemdScope::User {
                "--user "
            } else {
                ""
            }
        );
    }

    Ok(())
}

fn secrets_dir(home: &std::path::Path) -> std::path::PathBuf {
    home.join(".borg").join("secrets")
}

/// Log a warning when a best-effort service-control command (launchctl /
/// systemctl during install/uninstall) exits non-zero or fails to spawn.
/// `action` labels the command in the message, e.g. "systemctl daemon-reload".
fn warn_on_command_failure(status: std::io::Result<std::process::ExitStatus>, action: &str) {
    match status {
        Ok(s) if !s.success() => tracing::warn!("{action} returned non-zero status"),
        Err(e) => tracing::warn!("{action} failed: {e}"),
        _ => {}
    }
}

/// Provision `<secrets>/<filename>` with `<env_var>=<random>\n` at 0600,
/// creating the secrets dir at 0700. Idempotent: an existing file is left
/// alone so secrets stay stable across reinstalls.
fn provision_secret_env_file(home: &std::path::Path, filename: &str, env_var: &str) -> Result<()> {
    let secrets = secrets_dir(home);
    std::fs::create_dir_all(&secrets).with_context(|| format!("create {}", secrets.display()))?;
    #[cfg(unix)]
    set_unix_mode(&secrets, 0o700)?;
    let path = secrets.join(filename);
    if path.exists() {
        return Ok(());
    }
    let passphrase = random_passphrase();
    let body = format!("{env_var}={passphrase}\n");
    std::fs::write(&path, body).with_context(|| format!("write {}", path.display()))?;
    #[cfg(unix)]
    set_unix_mode(&path, 0o600)?;
    Ok(())
}

/// Provision `~/.borg/secrets/kek.env` with `BORG_KEK_PASSPHRASE=<random>`,
/// 0600. Idempotent: existing file is left alone so the encrypted blob
/// keeps decrypting across reinstalls.
fn provision_kek_env_file(home: &std::path::Path) -> Result<()> {
    provision_secret_env_file(home, "kek.env", "BORG_KEK_PASSPHRASE")
}

/// Provision `~/.borg/secrets/keyring.env` with `BORG_KEYRING_PASS=<random>`,
/// 0600. The unit's ExecStart pipes this passphrase into
/// `gnome-keyring-daemon --unlock` so the daemon boots without an
/// interactive password prompt. Idempotent: regenerating would invalidate
/// existing keyring contents.
fn provision_keyring_env_file(home: &std::path::Path) -> Result<()> {
    provision_secret_env_file(home, "keyring.env", "BORG_KEYRING_PASS")
}

/// True iff this process is running in a headless Linux context where
/// the OS keychain (secret-service via dbus) is almost certainly
/// unavailable: no `$DISPLAY`, no `$DBUS_SESSION_BUS_ADDRESS`, and (for
/// the canonical VPS deploy shape) running as root. macOS / Windows /
/// desktop Linux always return `false` so the helper is a no-op there.
pub fn is_headless_linux() -> bool {
    if !cfg!(target_os = "linux") {
        return false;
    }
    let display_empty = std::env::var_os("DISPLAY")
        .map(|v| v.is_empty())
        .unwrap_or(true);
    let dbus_empty = std::env::var_os("DBUS_SESSION_BUS_ADDRESS")
        .map(|v| v.is_empty())
        .unwrap_or(true);
    display_empty && dbus_empty && is_root()
}

/// Print a multi-line recommendation block telling the user to upgrade
/// the headless install to the secure (gnome-keyring) backend. Called
/// at the end of onboarding when [`is_headless_linux`] returns true.
/// Output is plain stderr text — the onboarding TUI has already exited
/// by this point, so we want simple, copy-pasteable instructions.
pub fn print_headless_setup_hint() {
    eprintln!();
    eprintln!("─── Headless server detected ───────────────────────────");
    eprintln!("The daemon is currently using the file-encrypted KEK backend.");
    eprintln!("Posture: Reduced (×0.85 XP).");
    eprintln!();
    eprintln!("To upgrade to the OS keychain backend (Default × 1.00 XP, +100 XP");
    eprintln!("\"Hardened Host\" milestone), run on a Debian/Ubuntu host:");
    eprintln!();
    eprintln!("    borg service install --secure");
    eprintln!();
    eprintln!("This installs gnome-keyring + dbus-user-session and rewires the");
    eprintln!("systemd unit so borgd uses secret-service at rest. On non-Debian,");
    eprintln!("the command is a no-op and you stay on the file backend.");
    eprintln!("────────────────────────────────────────────────────────");
}

/// Generate a 32-byte URL-safe random passphrase. ~256 bits of entropy
/// per RFC 4648 base64url (no padding) — enough that brute-force on the
/// encrypted KEK file is not the weakest link.
fn random_passphrase() -> String {
    use base64::Engine;
    use rand::RngCore;
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn uninstall_systemd() -> Result<()> {
    // Try both scopes — a user may have installed under `--user` then
    // re-run as root (or vice versa). Cleanly tearing down whichever
    // exists keeps `borg service uninstall` idempotent across that
    // mistake.
    let mut tore_down_any = false;
    for scope in [SystemdScope::System, SystemdScope::User] {
        let Ok(unit_path) = scope.unit_path() else {
            continue;
        };
        if !unit_path.exists() {
            continue;
        }
        let mut disable = std::process::Command::new("systemctl");
        disable
            .args(scope.ctl_args())
            .args(["disable", "--now", "borg.service"]);
        warn_on_command_failure(disable.status(), "systemctl disable --now");
        std::fs::remove_file(&unit_path)?;
        let mut reload = std::process::Command::new("systemctl");
        reload.args(scope.ctl_args()).arg("daemon-reload");
        warn_on_command_failure(reload.status(), "systemctl daemon-reload");
        tore_down_any = true;
    }
    if tore_down_any {
        println!("Borg decommissioned.");
    } else {
        println!("Borg not installed.");
    }
    Ok(())
}

fn stop_systemd() -> Result<()> {
    let scope = SystemdScope::detect();
    let mut cmd = std::process::Command::new("systemctl");
    cmd.args(scope.ctl_args()).args(["stop", "borg.service"]);
    let status = cmd.status().context("Failed to run systemctl stop")?;
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

    /// The daemon must never be spawned into the client's current directory.
    /// It outlives the client and treats its cwd as the default workspace, so
    /// a `borg --worktree x` cold start would otherwise pin every later
    /// heartbeat, scheduled, and web session inside that worktree — and strand
    /// the daemon in a deleted directory once the worktree is removed.
    #[test]
    fn the_daemon_is_spawned_in_the_data_dir_not_the_clients_cwd() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().join("borg-home");
        std::fs::create_dir_all(&home).unwrap();
        let elsewhere = tmp.path().join("some-worktree");
        std::fs::create_dir_all(&elsewhere).unwrap();

        let previous = std::env::var_os("BORG_HOME");
        // SAFETY: single-threaded within this test; restored before returning.
        unsafe { std::env::set_var("BORG_HOME", &home) };
        let got = daemon_spawn_dir();
        match previous {
            Some(p) => unsafe { std::env::set_var("BORG_HOME", p) },
            None => unsafe { std::env::remove_var("BORG_HOME") },
        }

        assert_eq!(got.as_deref(), Some(home.as_path()));
        assert_ne!(
            got.as_deref(),
            Some(elsewhere.as_path()),
            "the spawn dir must not track wherever the client happens to be"
        );
    }

    /// The status string is the operator's only diagnostic surface for
    /// "is the daemon healthy?". Each cell of the (uds × platform-state)
    /// matrix must map to a distinguishable, actionable message. A test
    /// that just asserts `contains("running")` would let the
    /// "loaded but unreachable" → "running" regression slip through —
    /// the exact bug the new platform probe was added to surface.
    #[test]
    fn format_service_status_distinguishes_each_cell_of_the_matrix() {
        // Running normally
        let s = format_service_status(true, PlatformManagerState::LoadedActive);
        assert_eq!(s, "Service status: running");

        // Running but the manager doesn't know about it — operator should
        // be nudged to re-link via `borg service install`.
        let s = format_service_status(true, PlatformManagerState::NotInstalled);
        assert!(
            s.contains("direct spawn") && s.contains("borg service install"),
            "must nudge re-link: {s}"
        );

        // Running but the unit reports inactive — drift between disk and
        // runtime, surface it.
        let s = format_service_status(true, PlatformManagerState::LoadedInactive);
        assert!(
            s.contains("orphaned") && s.contains("inactive"),
            "must surface drift: {s}"
        );

        // The critical diagnostic case: manager says active, UDS is dead.
        // Operator needs to restart, not reinstall.
        let s = format_service_status(false, PlatformManagerState::LoadedActive);
        assert!(
            s.contains("loaded but unreachable") && s.contains("wedged"),
            "must surface wedged daemon: {s}"
        );
        assert!(
            s.contains("borg service restart"),
            "must suggest restart, not install: {s}"
        );

        // Cleanly stopped — operator runs start, not install.
        let s = format_service_status(false, PlatformManagerState::LoadedInactive);
        assert!(
            s.contains("stopped") && s.contains("borg service start"),
            "must suggest start: {s}"
        );

        // Never installed — operator runs install.
        let s = format_service_status(false, PlatformManagerState::NotInstalled);
        assert!(
            s.contains("not installed") && s.contains("borg service install"),
            "must suggest install: {s}"
        );

        // Probe failed but daemon is up — don't make the operator think
        // anything is wrong with the daemon when the manager probe is
        // what actually broke.
        let s = format_service_status(true, PlatformManagerState::Unknown);
        assert_eq!(s, "Service status: running");
        let s = format_service_status(false, PlatformManagerState::Unknown);
        assert_eq!(s, "Service status: not running");
    }

    /// All eight cells must yield distinct strings — duplicates mean the
    /// operator can't disambiguate runbooks, which is the bug this whole
    /// matrix exists to prevent.
    #[test]
    fn format_service_status_produces_distinct_strings_per_cell() {
        use std::collections::HashSet;
        let cells = [
            (true, PlatformManagerState::LoadedActive),
            (true, PlatformManagerState::LoadedInactive),
            (true, PlatformManagerState::NotInstalled),
            (true, PlatformManagerState::Unknown),
            (false, PlatformManagerState::LoadedActive),
            (false, PlatformManagerState::LoadedInactive),
            (false, PlatformManagerState::NotInstalled),
            (false, PlatformManagerState::Unknown),
        ];
        // Unknown collapses with LoadedActive when UDS is up (both render
        // as "running" — probe failure should not invent diagnostics when
        // the daemon is observably healthy). When UDS is down, all four
        // platform states render distinctly so each one points at its own
        // runbook: "wedged", "stopped", "not installed (install with...)",
        // and the bare "not running" fallback for an opaque probe. Expected
        // distinct count is therefore 7, not 8.
        let unique: HashSet<_> = cells
            .iter()
            .map(|(uds, p)| format_service_status(*uds, *p))
            .collect();
        assert_eq!(
            unique.len(),
            7,
            "matrix must collapse only (true, Unknown)→(true, LoadedActive); all other cells must point at their own runbook; got: {unique:?}"
        );
    }

    #[test]
    fn launchd_template_targets_borgd_binary() {
        let content = LAUNCHD_PLIST_TEMPLATE
            .replace("{{LABEL}}", LAUNCHD_LABEL)
            .replace("{{BINARY_PATH}}", BORGD_INSTALL_PATH)
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
            .replace("{{BINARY_PATH}}", BORGD_INSTALL_PATH)
            .replace("{{HOME}}", "/home/test");

        assert!(content.contains("ExecStart=/usr/local/bin/borgd"));
        assert!(!content.contains("--insecure-uds"));
        assert!(!content.contains("borgd daemon"));
        assert!(!content.contains("{{"));
    }

    /// System-scope unit (root install on a VPS) must target
    /// `multi-user.target`, run as root, and not depend on a per-user
    /// systemd manager. Regression target: a fresh DigitalOcean droplet
    /// SSH'd as root cannot use `systemctl --user`, so the install path
    /// MUST drop to a system unit when running as root. Otherwise
    /// `borg service install` silently no-ops and the daemon never
    /// starts — the exact failure the headless-VPS feature exists to fix.
    #[test]
    fn systemd_system_template_targets_multi_user_and_root() {
        let content = SYSTEMD_SYSTEM_UNIT_TEMPLATE
            .replace("{{BINARY_PATH}}", BORGD_INSTALL_PATH)
            .replace("{{HOME}}", "/root");
        assert!(content.contains("ExecStart=/usr/local/bin/borgd"));
        assert!(content.contains("User=root"));
        assert!(content.contains("WantedBy=multi-user.target"));
        assert!(content.contains("EnvironmentFile=-/root/.borg/secrets/kek.env"));
        assert!(!content.contains("WantedBy=default.target"));
        assert!(!content.contains("{{"));
    }

    /// System-scope secure unit must inherit the same dbus / keyring
    /// guards as the user-scope variant AND target the system manager.
    #[test]
    fn systemd_system_secure_template_keeps_dbus_guards_and_targets_root() {
        let content = SYSTEMD_SYSTEM_UNIT_SECURE_TEMPLATE
            .replace("{{BINARY_PATH}}", BORGD_INSTALL_PATH)
            .replace("{{HOME}}", "/root");
        assert!(content.contains("User=root"));
        assert!(content.contains("WantedBy=multi-user.target"));
        assert!(content.contains("dbus-run-session"));
        assert!(content.contains("--foreground"));
        assert!(!content.contains("--replace"));
        assert!(!content.contains("/tmp/"));
        // Empty-passphrase guard: refuses to unlock with no pass.
        assert!(content.contains("test -n \"$BORG_KEYRING_PASS\""));
        assert!(!content.contains("{{"));
    }

    /// Empty BORG_KEYRING_PASS would otherwise feed an empty string into
    /// `gnome-keyring-daemon --unlock`, which silently creates an
    /// unlocked keyring with no master password. The secure template
    /// MUST hard-fail with a clear error before reaching the unlock
    /// pipeline.
    #[test]
    fn secure_systemd_template_guards_empty_keyring_pass() {
        let content = SYSTEMD_UNIT_SECURE_TEMPLATE
            .replace("{{BINARY_PATH}}", BORGD_INSTALL_PATH)
            .replace("{{HOME}}", "/home/test");
        assert!(
            content.contains("test -n \"$BORG_KEYRING_PASS\""),
            "must check non-empty keyring passphrase before unlock"
        );
        assert!(content.contains("exit 78"), "must exit on empty pass");
    }

    /// File-backend template must reference the kek.env file so the
    /// passphrase reaches `borgd` via systemd EnvironmentFile rather than
    /// living in the unit text itself. Drift here would either expose the
    /// passphrase in `systemctl cat` or break headless boots.
    #[test]
    fn systemd_template_loads_kek_env_file() {
        let content = SYSTEMD_UNIT_TEMPLATE
            .replace("{{BINARY_PATH}}", BORGD_INSTALL_PATH)
            .replace("{{HOME}}", "/home/test");
        assert!(
            content.contains("EnvironmentFile=-/home/test/.borg/secrets/kek.env"),
            "missing EnvironmentFile in: {content}"
        );
        // The leading `-` makes the file optional so a fresh install
        // without the env file (e.g. `auto` mode that resolves to keychain)
        // doesn't fail to start.
        assert!(content.contains("EnvironmentFile=-"));
    }

    /// Secure template must wire dbus + gnome-keyring unlock and never
    /// embed the passphrase or leak env vars to a world-readable path.
    /// Regression targets:
    ///   - Inlining `BORG_KEYRING_PASS=secret` into the unit text would
    ///     leak the secret into `systemctl cat` and journalctl.
    ///   - Writing dbus env vars to `/tmp` would leak them to local
    ///     users (B3 in code review).
    ///   - Using `gnome-keyring-daemon --replace` (which daemonizes
    ///     itself) inside `dbus-run-session` would tear down the dbus
    ///     session before borgd starts (B2 in code review).
    #[test]
    fn secure_systemd_template_unlocks_keyring_via_env_file() {
        let content = SYSTEMD_UNIT_SECURE_TEMPLATE
            .replace("{{BINARY_PATH}}", BORGD_INSTALL_PATH)
            .replace("{{HOME}}", "/home/test");
        // No leading `-` on EnvironmentFile: secure mode requires the
        // passphrase file (otherwise `gnome-keyring-daemon --unlock`
        // would silently create an unlocked keyring with empty pass).
        // Surface the missing-file as a systemd error instead.
        assert!(content.contains("EnvironmentFile=/home/test/.borg/secrets/keyring.env"));
        assert!(content.contains("dbus-run-session"));
        assert!(content.contains("gnome-keyring-daemon"));
        assert!(content.contains("--components=secrets"));
        assert!(content.contains("--unlock"));
        // Foreground keeps the keyring alive for borgd's lifetime; the
        // self-daemonizing `--replace` would exit dbus-run-session early.
        assert!(content.contains("--foreground"));
        // Final exec is borgd. dbus-run-session waits on whatever its
        // child PID becomes after `exec`.
        assert!(content.contains("exec /usr/local/bin/borgd"));
        // Hard guards against regressions in the secure-mode unit-file shape.
        assert!(
            !content.contains("--replace"),
            "must not use --replace; it daemonizes and dbus-run-session exits"
        );
        assert!(
            !content.contains("/tmp/"),
            "must not leak dbus env vars to /tmp"
        );
        assert!(
            !content.contains("BORG_KEYRING_PASS=") || content.contains("\"$BORG_KEYRING_PASS\""),
            "passphrase must come from EnvironmentFile, never inlined"
        );
        assert!(!content.contains("{{"));
    }

    #[test]
    fn random_passphrase_is_high_entropy_and_url_safe() {
        // 32 bytes → 43 base64url chars (no padding). Pin the length
        // exactly so a regression to 24 or 16 source bytes is caught.
        let p = random_passphrase();
        assert_eq!(
            p.len(),
            43,
            "expected 43 chars (32 bytes b64url no-pad); got {} chars: {p}",
            p.len()
        );
        // base64url alphabet — no `+`, `/`, `=`.
        for c in p.chars() {
            assert!(
                c.is_ascii_alphanumeric() || c == '-' || c == '_',
                "non-base64url char in passphrase: {c}"
            );
        }
        // Two draws must differ — sanity check that we're actually
        // sampling RNG and not returning a constant.
        assert_ne!(random_passphrase(), p);
    }

    #[cfg(unix)]
    #[test]
    fn provision_kek_env_file_is_0600_under_secrets_0700() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().expect("tempdir");
        provision_kek_env_file(tmp.path()).expect("provision");
        let path = tmp.path().join(".borg").join("secrets").join("kek.env");
        let mode = std::fs::metadata(&path).expect("stat").permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let dir_mode = std::fs::metadata(path.parent().unwrap())
            .expect("stat dir")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(dir_mode, 0o700);
        // Body shape: `BORG_KEK_PASSPHRASE=<base64url>\n`.
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.starts_with("BORG_KEK_PASSPHRASE="));
        assert!(body.ends_with('\n'));
    }

    #[cfg(unix)]
    #[test]
    fn provision_kek_env_file_is_idempotent_across_reinstall() {
        // Reinstalls must NOT regenerate the passphrase — that would
        // invalidate the existing encrypted KEK and re-mint the CA on
        // every `borg service install`.
        let tmp = tempfile::tempdir().expect("tempdir");
        provision_kek_env_file(tmp.path()).expect("first");
        let body1 =
            std::fs::read_to_string(tmp.path().join(".borg/secrets/kek.env")).expect("read");
        provision_kek_env_file(tmp.path()).expect("second");
        let body2 =
            std::fs::read_to_string(tmp.path().join(".borg/secrets/kek.env")).expect("read");
        assert_eq!(body1, body2, "passphrase must persist across reinstall");
    }

    /// On non-Linux hosts, headless detection is always false — there's
    /// no fallback to set up because the OS keychain is reliable. macOS
    /// CI runs hit this path. Gated `#[cfg(not(target_os = "linux"))]`
    /// so it doesn't masquerade as a passing test on the primary CI
    /// target where the body is a no-op (review N4).
    #[cfg(not(target_os = "linux"))]
    #[test]
    fn is_headless_linux_returns_false_on_non_linux() {
        assert!(!is_headless_linux());
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
