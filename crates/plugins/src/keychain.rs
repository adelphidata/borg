use anyhow::Result;
use std::ffi::OsStr;
use std::path::Path;
use std::process::{Command, Output, Stdio};

/// Run a keychain helper subprocess (`security` / `secret-tool` / `cmdkey`)
/// with all three standard streams captured, never inherited.
///
/// The TUI client owns the terminal (alt-screen / inline viewport), and these
/// helpers are invoked from that same process. `security delete-generic-password`
/// prints `password has been deleted.` plus a keychain attribute dump to stdout,
/// and `add-generic-password` can print too. Inheriting stdio (via plain
/// `.status()`/`.spawn()`) paints that output over the TUI and corrupts the
/// screen. Capturing it here is the single chokepoint that keeps keychain writes
/// invisible. `stdin` bytes are piped in when present (Linux `secret-tool`).
fn run_captured<I, S>(program: &str, args: I, stdin: Option<&[u8]>) -> std::io::Result<Output>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut cmd = Command::new(program);
    cmd.args(args);
    match stdin {
        None => {
            // `Command::output` captures stdout+stderr and closes stdin.
            cmd.output()
        }
        Some(data) => {
            cmd.stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            let mut child = cmd.spawn()?;
            if let Some(mut si) = child.stdin.take() {
                use std::io::Write;
                si.write_all(data)?;
                drop(si); // close stdin so the child sees EOF
            }
            child.wait_with_output()
        }
    }
}

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

/// Argv for the macOS store helper. `security -i` reads its command from
/// stdin, so the secret never appears on the command line, where any local
/// user can read it with `ps` while the helper runs.
const MACOS_STORE_ARGV: [&str; 1] = ["-i"];

/// Longest command line `security -i` takes in one read. A longer line is
/// split, and the remainder runs as further commands; refusing it here keeps
/// a long value from turning into a truncated write.
const MACOS_INTERACTIVE_LINE_MAX: usize = 3584;

/// Quote one argument for the `security -i` command parser: wrap it in
/// double quotes and escape `\` and `"` with a backslash.
fn quote_interactive_arg(arg: &str) -> String {
    let mut quoted = String::with_capacity(arg.len() + 2);
    quoted.push('"');
    for c in arg.chars() {
        if c == '\\' || c == '"' {
            quoted.push('\\');
        }
        quoted.push(c);
    }
    quoted.push('"');
    quoted
}

/// Build the `security add-generic-password` command line that
/// `security -i` reads from stdin on macOS.
///
/// When `exe_path` is `Some`, includes `-T <path>` so the stored item's ACL
/// grants read access to that binary. Without `-T`, macOS restricts reads to
/// the exact process that created the item — which breaks the daemon, since
/// the TUI writes the keychain entry and the launchd-spawned daemon reads it.
///
/// Fails for a value with a line break (it would end the command early) or a
/// line too long for one read; the caller then uses the fallback file.
pub(crate) fn build_macos_store_command(
    service: &str,
    account: &str,
    value: &str,
    exe_path: Option<&Path>,
) -> Result<String> {
    let exe = exe_path.map(|p| p.to_string_lossy().into_owned());
    let fields = [service, account, value, exe.as_deref().unwrap_or("")];
    if fields.iter().any(|f| f.contains(['\n', '\r', '\0'])) {
        anyhow::bail!(
            "Cannot store a credential with a line break in macOS Keychain (service={service}, account={account})"
        );
    }
    let mut line = format!(
        "add-generic-password -s {} -a {} -w {} -U",
        quote_interactive_arg(service),
        quote_interactive_arg(account),
        quote_interactive_arg(value),
    );
    if let Some(path) = &exe {
        line.push_str(" -T ");
        line.push_str(&quote_interactive_arg(path));
    }
    line.push('\n');
    if line.len() > MACOS_INTERACTIVE_LINE_MAX {
        anyhow::bail!(
            "Credential too long for macOS Keychain helper (service={service}, account={account})"
        );
    }
    Ok(line)
}

/// Windows Credential Manager entry for `service`/`account`.
///
/// The target name is `{service}/{account}`, the name every earlier Borg
/// release wrote through `cmdkey`, so entries already on disk read back and
/// `cmdkey /list` and `/delete` still find what this writes.
pub fn windows_entry(service: &str, account: &str) -> keyring::Result<keyring::Entry> {
    // `store_status` performs the one-time platform store registration that
    // `keyring::Entry::new` would otherwise do; the modifier constructor lives
    // on the core entry type only.
    if let Err(e) = keyring::Entry::store_status() {
        return Err(keyring::Error::Invalid(
            "credential store".to_string(),
            format!("Windows Credential Manager unavailable: {e}"),
        ));
    }
    let target = windows_target(service, account);
    let modifiers = std::collections::HashMap::from([("target", target.as_str())]);
    let inner = keyring_core::Entry::new_with_modifiers(service, account, &modifiers)?;
    Ok(keyring::Entry { inner })
}

fn windows_target(service: &str, account: &str) -> String {
    format!("{service}/{account}")
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
        let command = build_macos_store_command(service, account, value, exe.as_deref())?;
        let output = run_captured("security", MACOS_STORE_ARGV, Some(command.as_bytes()))?;
        if !output.status.success() {
            anyhow::bail!(
                "Failed to store in macOS Keychain (service={service}, account={account}, status={})",
                output.status
            );
        }
    } else if cfg!(target_os = "linux") {
        let label = format!("Borg {account}");
        let output = run_captured(
            "secret-tool",
            [
                "store", "--label", &label, "service", service, "account", account,
            ],
            Some(value.as_bytes()),
        )?;
        if !output.status.success() {
            anyhow::bail!("Failed to store via secret-tool (service={service}, account={account})");
        }
    } else if cfg!(target_os = "windows") {
        // Written through the Credential Manager API, not `cmdkey /pass:`,
        // so the secret is never on a command line.
        windows_entry(service, account)
            .and_then(|entry| entry.set_password(value))
            .map_err(|e| {
                anyhow::anyhow!(
                    "Failed to store in Windows Credential Manager (target={}): {e}",
                    windows_target(service, account)
                )
            })?;
    } else {
        anyhow::bail!("No keychain available on this platform");
    }
    Ok(())
}

/// Remove a credential from the OS keychain. Errors are ignored: the backends
/// don't distinguish "not present" from "delete failed" in their exit codes, so
/// the caller (`installer::uninstall`) re-checks with `credential_store::check`
/// and warns on a genuine leftover instead.
///
/// Output is captured (not inherited) — `security delete-generic-password`
/// prints `password has been deleted.` and an attribute dump to stdout, which
/// would otherwise corrupt the TUI's alt-screen. See [`run_captured`].
pub fn remove(service: &str, account: &str) {
    if cfg!(target_os = "macos") {
        let _ = run_captured(
            "security",
            ["delete-generic-password", "-s", service, "-a", account],
            None,
        );
    } else if cfg!(target_os = "linux") {
        let _ = run_captured(
            "secret-tool",
            ["clear", "service", service, "account", account],
            None,
        );
    } else if cfg!(target_os = "windows") {
        let target = windows_target(service, account);
        let _ = run_captured("cmdkey", [format!("/delete:{target}")], None);
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
        let target = windows_target(service, account);
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

    #[cfg(target_os = "macos")]
    #[test]
    fn available_true_on_macos() {
        // macOS always has the `security` binary. Locked because other tests
        // temporarily rewrite PATH to hide the keychain helpers.
        let _lock = crate::test_support::env_lock();
        assert!(available());
    }

    #[cfg(unix)]
    #[test]
    fn store_result_reflects_helper_exit_status() {
        // `store` must mirror the helper's exit status: a failed write that
        // reports Ok makes the installer claim success while the credential
        // was never persisted (the fallback file only engages when the caller
        // sees the error).
        let _lock = crate::test_support::env_lock();
        {
            let _shim = crate::test_support::KeychainShim::failing();
            assert!(
                store("borg-shim-store", "acct", "v").is_err(),
                "helper failure must surface as an error"
            );
        }
        {
            // Succeed on writes/deletes only; drain stdin for the Linux
            // `secret-tool store` path which pipes the secret in.
            let _shim = crate::test_support::KeychainShim::fake_helper(concat!(
                "cat >/dev/null 2>/dev/null\n",
                "case \"$1\" in -i|delete-generic-password|store|clear) exit 0 ;; esac\n",
                "exit 1",
            ));
            store("borg-shim-store", "acct", "v").expect("helper success must map to Ok");
        }
    }

    #[cfg(unix)]
    #[test]
    fn remove_invokes_keychain_delete_helper() {
        // `remove` swallows errors by design, so the only observable contract
        // is that it actually issues a delete for the right service — a no-op
        // remove leaves secrets in the keychain after uninstall.
        let _lock = crate::test_support::env_lock();
        // Log every invocation, but only report success for deletes so a
        // concurrently running lookup test can't see a phantom credential.
        let shim = crate::test_support::KeychainShim::fake_helper(concat!(
            "printf '%s\\n' \"$*\" >> \"{{LOG}}\"\n",
            "case \"$1\" in delete-generic-password|clear) exit 0 ;; esac\n",
            "exit 1",
        ));

        remove("borg-shim-remove-svc", "acct-r");

        let log = std::fs::read_to_string(shim.log_path())
            .expect("remove must have invoked the keychain helper");
        assert!(
            log.lines().any(|l| l.contains("borg-shim-remove-svc")
                && (l.contains("delete-generic-password") || l.contains("clear"))),
            "remove must issue a delete for the service: {log}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn check_reports_presence_from_helper_lookup() {
        // `check` gates is_installed and the post-uninstall leftover warning;
        // it must reflect what the helper actually finds.
        let _lock = crate::test_support::env_lock();
        let _shim = crate::test_support::KeychainShim::find_succeeds();
        assert!(
            check(crate::test_support::SHIM_SERVICE, "any-acct"),
            "successful lookup must report the credential as present"
        );
        assert!(
            !check("borg-shim-other-svc", "any-acct"),
            "failed lookup must report absent"
        );
    }

    /// Regression: saving an API key in `/settings` corrupted the TUI because
    /// `store`/`remove` ran the `security` helper with inherited stdio, so its
    /// output (`password has been deleted.` + a keychain attribute dump) painted
    /// over the alt-screen. `run_captured` MUST capture stdout instead of letting
    /// it reach the terminal. We run a command that prints a sentinel: if the
    /// helper captures (correct), the sentinel lands in `output.stdout`; if a
    /// future edit reverts to inherited stdio, captured stdout is empty and this
    /// fails — the same condition that produced the on-screen corruption.
    #[test]
    fn run_captured_captures_stdout_instead_of_inheriting() {
        const SENTINEL: &str = "BORG_KEYCHAIN_STDOUT_SENTINEL";
        #[cfg(not(target_os = "windows"))]
        let output = run_captured("sh", ["-c", &format!("echo {SENTINEL}")], None)
            .expect("sh subprocess should spawn");
        #[cfg(target_os = "windows")]
        let output = run_captured("cmd", ["/C", &format!("echo {SENTINEL}")], None)
            .expect("cmd subprocess should spawn");

        assert!(output.status.success(), "sentinel command should succeed");
        assert!(
            String::from_utf8_lossy(&output.stdout).contains(SENTINEL),
            "stdout must be captured into Output (not inherited to the terminal)"
        );
    }

    /// `run_captured` pipes `stdin` to the child and still captures its streams.
    /// This is the Linux `secret-tool` path; verify the bytes reach the child by
    /// echoing them back through `cat`.
    #[cfg(not(target_os = "windows"))]
    #[test]
    fn run_captured_pipes_stdin_to_child() {
        let output = run_captured("cat", std::iter::empty::<&str>(), Some(b"piped-secret"))
            .expect("cat subprocess should spawn");
        assert!(output.status.success());
        assert_eq!(String::from_utf8_lossy(&output.stdout), "piped-secret");
    }

    #[test]
    fn check_nonexistent_credential_returns_false() {
        // A random service/account pair should not exist
        let result = check("borg-test-nonexistent-svc-12345", "nonexistent-account-xyz");
        assert!(!result);
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
    fn build_macos_store_command_includes_dash_t_when_exe_provided() {
        let exe = std::path::PathBuf::from("/usr/local/bin/borg");
        let line = build_macos_store_command("svc", "acct", "val", Some(&exe)).unwrap();

        // -T is the ACL flag that lets the borg binary (daemon + TUI + CLI)
        // read the stored item. Dropping it re-introduces the bug.
        assert_eq!(
            line,
            "add-generic-password -s \"svc\" -a \"acct\" -w \"val\" -U -T \"/usr/local/bin/borg\"\n"
        );
    }

    #[test]
    fn build_macos_store_command_omits_dash_t_when_exe_missing() {
        let line = build_macos_store_command("svc", "acct", "val", None).unwrap();
        assert!(!line.contains("-T"), "no -T when exe path unavailable");
        assert!(line.contains(" -U"), "-U still required");
    }

    /// Spec: `security -i` splits its line on whitespace outside double
    /// quotes and takes `\\` and `\"` as escapes. A value with spaces, quotes
    /// or backslashes must stay one argument, or the stored secret is wrong
    /// and the rest of the value runs as extra options.
    #[test]
    fn interactive_quoting_keeps_each_value_one_argument() {
        let cases = [
            ("plain", r#""plain""#),
            ("a b", r#""a b""#),
            (r#"x"y"#, r#""x\"y""#),
            (r"c:\d\", r#""c:\\d\\""#),
            ("$HOME 'q'", r#""$HOME 'q'""#),
        ];
        for (raw, quoted) in cases {
            assert_eq!(quote_interactive_arg(raw), quoted, "quoting {raw:?}");
        }
    }

    /// Spec: a line break would end the command early and run the rest as a
    /// second command, and an over-long line is split the same way. Both
    /// must fail so the caller falls back to the file instead of writing a
    /// truncated secret.
    #[test]
    fn build_macos_store_command_rejects_line_breaks_and_overlong_values() {
        for bad in ["a\nb", "a\rb"] {
            assert!(build_macos_store_command("svc", "acct", bad, None).is_err());
            assert!(build_macos_store_command(bad, "acct", "v", None).is_err());
        }
        let long = "A".repeat(MACOS_INTERACTIVE_LINE_MAX);
        let err = build_macos_store_command("svc", "acct", &long, None).unwrap_err();
        assert!(
            !err.to_string().contains(&long),
            "error must not echo the secret"
        );
        let fits = "A".repeat(2048);
        assert!(build_macos_store_command("svc", "acct", &fits, None).is_ok());
    }

    /// Regression: the store helper got the secret as an argument, so any
    /// local user could read it with `ps` while the helper ran. The secret
    /// must reach the helper on stdin only.
    #[cfg(target_os = "macos")]
    #[test]
    fn store_passes_secret_on_stdin_not_argv() {
        let _lock = crate::test_support::env_lock();
        let shim = crate::test_support::KeychainShim::fake_helper(concat!(
            "printf 'ARGV %s\\n' \"$*\" >> \"{{LOG}}\"\n",
            "if [ \"$1\" = -i ]; then printf 'STDIN ' >> \"{{LOG}}\"; cat >> \"{{LOG}}\"; fi\n",
            "exit 0",
        ));
        let secret = "sk-argv-leak-check-9d2f";

        store("borg-shim-argv-svc", "acct", secret).expect("shimmed store succeeds");

        let log = std::fs::read_to_string(shim.log_path()).expect("helper ran");
        for line in log.lines().filter(|l| l.starts_with("ARGV ")) {
            assert!(!line.contains(secret), "secret on argv: {line}");
        }
        assert!(
            log.lines()
                .any(|l| l.starts_with("STDIN add-generic-password") && l.contains(secret)),
            "secret must reach the helper on stdin: {log}"
        );
    }

    /// Spec: the Windows target name stays `{service}/{account}` so entries
    /// written by earlier releases (through `cmdkey`) read back, and
    /// `cmdkey /list` and `/delete` find new ones.
    #[test]
    fn windows_target_matches_the_cmdkey_layout() {
        assert_eq!(
            windows_target("borg-slack", "bot_token"),
            "borg-slack/bot_token"
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

        store(service, account, value).expect("store should succeed on Windows");

        assert!(
            check(service, account),
            "credential should exist after store"
        );

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
}
