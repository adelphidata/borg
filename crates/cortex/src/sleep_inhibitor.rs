//! Prevents the system from sleeping while long-running operations are active.
//!
//! On macOS, uses IOKit power assertions via `caffeinate` subprocess.
//! On Linux, uses `systemd-inhibit` if available, otherwise no-op.
//! On other platforms, this is a no-op.

use tracing::debug;
#[cfg(target_os = "macos")]
use tracing::warn;

/// RAII guard that prevents system sleep while held.
/// Dropping the guard re-enables sleep.
pub struct SleepInhibitor {
    #[cfg(target_os = "macos")]
    child: Option<std::process::Child>,
    #[cfg(target_os = "linux")]
    child: Option<std::process::Child>,
}

impl SleepInhibitor {
    /// Create a new sleep inhibitor with the given reason string.
    /// Returns a guard that prevents sleep until dropped.
    pub fn new(reason: &str) -> Self {
        #[cfg(target_os = "macos")]
        {
            Self::new_macos(reason)
        }
        #[cfg(target_os = "linux")]
        {
            Self::new_linux(reason)
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            let _ = reason;
            debug!("Sleep inhibitor not supported on this platform");
            Self {}
        }
    }

    #[cfg(target_os = "macos")]
    fn new_macos(reason: &str) -> Self {
        // `caffeinate -i` prevents idle sleep; -s prevents system sleep.
        // Using -i is sufficient for daemon workloads.
        match std::process::Command::new("caffeinate")
            .args(["-i", "-w", &std::process::id().to_string()])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            Ok(child) => {
                debug!(
                    "Sleep inhibitor active (caffeinate pid={}): {reason}",
                    child.id()
                );
                Self { child: Some(child) }
            }
            Err(e) => {
                warn!("Failed to start caffeinate: {e}");
                Self { child: None }
            }
        }
    }

    #[cfg(target_os = "linux")]
    fn new_linux(reason: &str) -> Self {
        // systemd-inhibit blocks sleep for the lifetime of the child process.
        // We spawn `sleep infinity` under it so it lives as long as this guard.
        match std::process::Command::new("systemd-inhibit")
            .args([
                "--what=sleep",
                &format!("--why={reason}"),
                "--who=borg",
                "--mode=block",
                "sleep",
                "infinity",
            ])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            Ok(child) => {
                debug!(
                    "Sleep inhibitor active (systemd-inhibit pid={}): {reason}",
                    child.id()
                );
                Self { child: Some(child) }
            }
            Err(e) => {
                debug!("systemd-inhibit not available: {e}");
                Self { child: None }
            }
        }
    }

    /// Returns true if the inhibitor is actively preventing sleep.
    pub fn is_active(&self) -> bool {
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        {
            self.child.is_some()
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            false
        }
    }
}

impl Drop for SleepInhibitor {
    fn drop(&mut self) {
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        {
            if let Some(mut child) = self.child.take() {
                debug!("Releasing sleep inhibitor (pid={})", child.id());
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inhibitor_lifecycle() {
        let inhibitor = SleepInhibitor::new("test");
        // On macOS/Linux CI, caffeinate/systemd-inhibit may or may not be available.
        // This test verifies the lifecycle doesn't panic.
        let _active = inhibitor.is_active();
        drop(inhibitor);
    }

    #[test]
    fn inhibitor_is_active_reflects_state() {
        let inhibitor = SleepInhibitor::new("test active check");
        #[cfg(target_os = "macos")]
        {
            // caffeinate should be available on macOS
            assert!(inhibitor.is_active());
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            assert!(!inhibitor.is_active());
        }
        drop(inhibitor);
    }

    #[test]
    fn inhibitor_drop_cleans_up() {
        let inhibitor = SleepInhibitor::new("cleanup test");
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        let pid = inhibitor.child.as_ref().map(|c| c.id());
        drop(inhibitor);

        // After drop, the child process should be killed
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        if let Some(pid) = pid {
            // Give a moment for cleanup
            std::thread::sleep(std::time::Duration::from_millis(50));
            // Check that process is no longer running (kill(0) returns error)
            unsafe {
                let result = libc::kill(pid as i32, 0);
                assert_eq!(result, -1, "Process should be dead after drop");
            }
        }
    }

    #[test]
    fn multiple_inhibitors() {
        let a = SleepInhibitor::new("test A");
        let b = SleepInhibitor::new("test B");
        // Both should work independently
        let _ = a.is_active();
        let _ = b.is_active();
        drop(a);
        drop(b);
    }
}
