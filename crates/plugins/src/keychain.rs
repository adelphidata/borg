use anyhow::Result;
use std::path::Path;

/// Check if the OS keychain is available.
pub fn available() -> bool {
    if cfg!(target_os = "macos") {
        which::which("security").is_ok()
    } else if cfg!(target_os = "linux") {
        which::which("secret-tool").is_ok()
    } else if cfg!(target_os = "windows") {
        which::which("cmdkey").is_ok()
    } else {
        false
    }
}

/// Build the `security add-generic-password` argv for macOS.
///
/// When `exe_path` is `Some`, includes `-T <path>` so the stored item's ACL
/// grants read access to that binary. Without `-T`, macOS restricts reads to
/// the exact process that created the item — which breaks the daemon, since
/// the TUI writes the keychain entry and the launchd-spawned daemon reads it.
pub(crate) fn build_macos_store_args(
    service: &str,
    account: &str,
    value: &str,
    exe_path: Option<&Path>,
) -> Vec<String> {
    let mut args = vec![
        "add-generic-password".to_string(),
        "-s".to_string(),
        service.to_string(),
        "-a".to_string(),
        account.to_string(),
        "-w".to_string(),
        value.to_string(),
        "-U".to_string(),
    ];
    if let Some(path) = exe_path {
        args.push("-T".to_string());
        args.push(path.to_string_lossy().into_owned());
    }
    args
}

/// Store a credential in the OS keychain.
pub fn store(service: &str, account: &str, value: &str) -> Result<()> {
    if cfg!(target_os = "macos") {
        // Remove any existing entry so the new `-T` ACL fully replaces the old
        // one. `-U` alone updates the value but preserves the pre-existing ACL,
        // which means entries created before the ACL fix stay unreadable by the
        // daemon.
        remove(service, account);

        let exe = match std::env::current_exe() {
            Ok(p) => Some(p),
            Err(e) => {
                tracing::warn!(
                    "current_exe() failed; storing keychain item without -T ACL — daemon may not be able to read it: {e}"
                );
                None
            }
        };
        let args = build_macos_store_args(service, account, value, exe.as_deref());
        let status = std::process::Command::new("security")
            .args(&args)
            .status()?;
        if !status.success() {
            anyhow::bail!(
                "Failed to store in macOS Keychain (service={service}, account={account})"
            );
        }
    } else if cfg!(target_os = "linux") {
        let mut child = std::process::Command::new("secret-tool")
            .args([
                "store",
                "--label",
                &format!("Borg {account}"),
                "service",
                service,
                "account",
                account,
            ])
            .stdin(std::process::Stdio::piped())
            .spawn()?;
        if let Some(mut stdin) = child.stdin.take() {
            use std::io::Write;
            stdin.write_all(value.as_bytes())?;
            drop(stdin); // close stdin so secret-tool sees EOF
        }
        let status = child.wait()?;
        if !status.success() {
            anyhow::bail!("Failed to store via secret-tool (service={service}, account={account})");
        }
    } else if cfg!(target_os = "windows") {
        let target = format!("{service}/{account}");
        let status = std::process::Command::new("cmdkey")
            .arg(format!("/generic:{target}"))
            .arg(format!("/user:{account}"))
            .arg(format!("/pass:{value}"))
            .status()?;
        if !status.success() {
            anyhow::bail!("Failed to store in Windows Credential Manager (target={target})");
        }
    } else {
        anyhow::bail!("No keychain available on this platform");
    }
    Ok(())
}

/// Remove a credential from the OS keychain. Errors are silently ignored.
pub fn remove(service: &str, account: &str) {
    if cfg!(target_os = "macos") {
        let _ = std::process::Command::new("security")
            .args(["delete-generic-password", "-s", service, "-a", account])
            .status();
    } else if cfg!(target_os = "linux") {
        let _ = std::process::Command::new("secret-tool")
            .args(["clear", "service", service, "account", account])
            .status();
    } else if cfg!(target_os = "windows") {
        let target = format!("{service}/{account}");
        let _ = std::process::Command::new("cmdkey")
            .arg(format!("/delete:{target}"))
            .status();
    }
}

/// Check if a credential exists in the OS keychain.
pub fn check(service: &str, account: &str) -> bool {
    if cfg!(target_os = "macos") {
        std::process::Command::new("security")
            .args(["find-generic-password", "-s", service, "-a", account, "-w"])
            .output()
            .is_ok_and(|o| o.status.success())
    } else if cfg!(target_os = "linux") {
        std::process::Command::new("secret-tool")
            .args(["lookup", "service", service, "account", account])
            .output()
            .is_ok_and(|o| o.status.success())
    } else if cfg!(target_os = "windows") {
        let target = format!("{service}/{account}");
        std::process::Command::new("cmdkey")
            .arg(format!("/list:{target}"))
            .output()
            .is_ok_and(|o| {
                o.status.success() && !String::from_utf8_lossy(&o.stdout).contains("not found")
            })
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn available_returns_bool() {
        // On macOS CI, `security` binary should exist; on Linux, `secret-tool` may not.
        // This test just ensures the function doesn't panic.
        let _ = available();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn available_true_on_macos() {
        // macOS always has the `security` binary
        assert!(available());
    }

    #[test]
    fn check_nonexistent_credential_returns_false() {
        // A random service/account pair should not exist
        let result = check("borg-test-nonexistent-svc-12345", "nonexistent-account-xyz");
        assert!(!result);
    }

    #[test]
    fn remove_nonexistent_does_not_panic() {
        // Removing a credential that doesn't exist should silently succeed
        remove("borg-test-nonexistent-svc-12345", "nonexistent-account-xyz");
    }

    /// E2E keychain tests are gated behind this env var because they spawn
    /// a `security` subprocess that is not on the test binary's ACL, which
    /// triggers a macOS GUI keychain password prompt on every run. The tests
    /// are still valuable — they're the only layer that catches real ACL
    /// regressions — but they must be opt-in for local `cargo test`.
    ///
    /// Run them explicitly (e.g. before shipping keychain changes):
    ///
    /// ```sh
    /// BORG_E2E_KEYCHAIN=1 cargo test -p borg-plugins keychain:: -- --test-threads=1
    /// ```
    ///
    /// CI can set `BORG_E2E_KEYCHAIN=1` on macOS runners that have either
    /// (a) an unlocked ephemeral keychain, or (b) the `security` binary added
    /// to the partition list. Without the env var, these tests are skipped
    /// silently so `cargo test` never blocks on a password prompt.
    #[cfg(target_os = "macos")]
    fn e2e_keychain_enabled() -> bool {
        std::env::var("BORG_E2E_KEYCHAIN")
            .map(|v| !v.is_empty() && v != "0")
            .unwrap_or(false)
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn store_then_read_from_subprocess_without_prompt() {
        if !e2e_keychain_enabled() {
            eprintln!(
                "skipping: set BORG_E2E_KEYCHAIN=1 to run (will prompt for keychain password)"
            );
            return;
        }
        // Regression guard: the daemon resolves keychain creds by shelling out
        // to `security find-generic-password -w`, which runs in a *different*
        // process than the TUI that wrote the item. Before the `-T` ACL fix,
        // that read failed with "item could not be found" because macOS
        // restricted reads to the writing process. This test reads via a fresh
        // subprocess — the same path the daemon uses — so any ACL regression
        // surfaces immediately.
        let service = "borg-keychain-test-subprocess";
        let account = "test-subprocess-read";
        let value = "test-secret-value-subprocess";

        remove(service, account);
        store(service, account, value).expect("store should succeed on macOS");

        let output = std::process::Command::new("security")
            .args(["find-generic-password", "-s", service, "-a", account, "-w"])
            .output()
            .expect("security subprocess should spawn");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        remove(service, account);

        assert!(
            output.status.success(),
            "subprocess read failed (ACL regression?): stderr={stderr}"
        );
        assert_eq!(
            stdout.trim(),
            value,
            "subprocess read returned wrong value: stdout={stdout}"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn store_replaces_existing_entry_with_new_acl() {
        if !e2e_keychain_enabled() {
            eprintln!(
                "skipping: set BORG_E2E_KEYCHAIN=1 to run (will prompt for keychain password)"
            );
            return;
        }
        // Regression guard: if `store()` reverts to `-U` update-in-place, the
        // pre-existing ACL sticks around and old broken entries remain
        // unreadable by the daemon. This test stores twice and asserts the
        // second value wins — which only works if we remove-then-add.
        let service = "borg-keychain-test-replace";
        let account = "test-replace";

        remove(service, account);
        store(service, account, "first-value").expect("first store");
        store(service, account, "second-value").expect("second store replaces");

        let output = std::process::Command::new("security")
            .args(["find-generic-password", "-s", service, "-a", account, "-w"])
            .output()
            .expect("security subprocess should spawn");
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();

        remove(service, account);

        assert_eq!(stdout, "second-value", "store should replace prior value");
    }

    #[test]
    fn build_macos_store_args_includes_dash_t_when_exe_provided() {
        let exe = std::path::PathBuf::from("/usr/local/bin/borg");
        let args = build_macos_store_args("svc", "acct", "val", Some(&exe));

        // -T is the ACL flag that lets the borg binary (daemon + TUI + CLI)
        // read the stored item. Dropping it re-introduces the bug.
        let t_idx = args
            .iter()
            .position(|a| a == "-T")
            .expect("-T flag must be present");
        assert_eq!(
            args.get(t_idx + 1).map(String::as_str),
            Some("/usr/local/bin/borg"),
            "-T must be followed by the exe path"
        );
        assert!(args.contains(&"-U".to_string()), "-U still required");
    }

    #[test]
    fn build_macos_store_args_omits_dash_t_when_exe_missing() {
        let args = build_macos_store_args("svc", "acct", "val", None);
        assert!(
            !args.iter().any(|a| a == "-T"),
            "no -T when exe path unavailable"
        );
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    #[test]
    fn store_fails_on_unsupported_platform() {
        let result = store("svc", "acct", "val");
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("No keychain available"));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn available_true_on_windows() {
        // cmdkey.exe is always present on Windows
        assert!(available());
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_store_and_check_round_trip() {
        let service = "borg-keychain-test";
        let account = "test-round-trip-win";
        let value = "test-secret-value";

        // Clean up first in case of prior failed run
        remove(service, account);

        // Store
        store(service, account, value).expect("store should succeed on Windows");

        // Check exists
        assert!(
            check(service, account),
            "credential should exist after store"
        );

        // Clean up
        remove(service, account);
        assert!(
            !check(service, account),
            "credential should not exist after remove"
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_check_nonexistent_returns_false() {
        let result = check(
            "borg-test-nonexistent-svc-win-12345",
            "nonexistent-account-xyz",
        );
        assert!(!result);
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_remove_nonexistent_does_not_panic() {
        remove(
            "borg-test-nonexistent-svc-win-12345",
            "nonexistent-account-xyz",
        );
    }
}
