use crate::constants::{SENSITIVE_HOME_PATHS, STANDARD_EXEC_PATHS};
use crate::policy::SandboxPolicy;
use std::path::Path;

/// Validate that a path is safe for interpolation into a Seatbelt profile.
/// Rejects paths containing characters that could inject arbitrary Seatbelt rules.
fn validate_seatbelt_path(path: &str) -> Result<(), String> {
    if !path.starts_with('/') {
        return Err(format!(
            "Sandbox path must be absolute (start with /): {path}"
        ));
    }
    for ch in ['"', '(', ')', '\n', '\r', '\\'] {
        if path.contains(ch) {
            return Err(format!(
                "Sandbox path contains unsafe character {ch:?}: {path}"
            ));
        }
    }
    if !path.is_ascii() {
        return Err(format!(
            "Sandbox path contains non-ASCII characters: {path}"
        ));
    }
    Ok(())
}

/// Resolve `path` through symlinks as far as it exists on disk, re-attaching any
/// not-yet-created tail.
///
/// `Path::canonicalize` is all-or-nothing: it fails when the leaf is missing, so
/// callers that fell back to the raw path emitted rules against a path the
/// kernel never sees. On macOS `/var`, `/tmp`, and `/etc` are symlinks into
/// `/private/…`, so an un-resolved `/var/folders/x/tool` allow rule is inert
/// under `(deny default)` and a `/var/…` *deny* rule is worse — it silently
/// fails to protect anything. Walking up to the deepest existing ancestor and
/// canonicalizing that gives the right prefix in both cases.
fn resolve_symlinks_best_effort(path: &Path) -> String {
    let mut probe = path;
    loop {
        if let Ok(canonical) = probe.canonicalize() {
            let suffix = path.strip_prefix(probe).unwrap_or(Path::new(""));
            // `PathBuf::join("")` appends a separator, so joining the empty
            // suffix of an already-existing path would yield `/private/var/x/`.
            // Seatbelt's `subpath` is documented without a trailing separator,
            // and the extra form also made `push_subpath_rule` emit a duplicate
            // rule for every path that exists — which is the common case.
            if suffix.as_os_str().is_empty() {
                return canonical.to_string_lossy().into_owned();
            }
            return canonical.join(suffix).to_string_lossy().into_owned();
        }
        match probe.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => probe = parent,
            // Nothing on the way up resolved (relative path, or a root that
            // cannot be canonicalized). Emit the input unchanged — the
            // `validate_seatbelt_path` absolute-path check is the backstop.
            _ => return path.to_string_lossy().into_owned(),
        }
    }
}

/// Emit a `subpath` rule for `path`, plus a second rule for its symlink-resolved
/// form when that differs.
///
/// Both forms are emitted rather than only the resolved one, mirroring the
/// existing unconditional `/tmp` + `/private/tmp` pair below: we do not want a
/// correctness fix to narrow a rule that works today. For `deny` rules this is
/// the security-relevant direction — an un-resolved `/var/…` deny protects
/// nothing, since the kernel reports `/private/var/…`.
fn push_subpath_rule(
    profile: &mut String,
    effect: &str,
    operation: &str,
    path: &str,
) -> Result<(), String> {
    validate_seatbelt_path(path)?;
    profile.push_str(&format!("({effect} {operation} (subpath \"{path}\"))\n"));
    let resolved = resolve_symlinks_best_effort(Path::new(path));
    if resolved != path && validate_seatbelt_path(&resolved).is_ok() {
        profile.push_str(&format!(
            "({effect} {operation} (subpath \"{resolved}\"))\n"
        ));
    }
    Ok(())
}

/// Generate a macOS Seatbelt profile string for sandbox-exec.
///
/// `runtime_bin` is the absolute path to the runtime binary (e.g. `/usr/bin/python3`)
/// that the tool is allowed to execute. If `None`, process-exec is allowed broadly
/// (less secure, but backwards-compatible).
pub fn generate_profile(
    policy: &SandboxPolicy,
    tool_dir: &Path,
    runtime_bin: Option<&str>,
) -> Result<String, String> {
    // Resolve symlinks (e.g. /var -> /private/var on macOS) so the emitted
    // `subpath` matches what the kernel sees. `canonicalize` fails outright when
    // the leaf does not exist yet, which used to leave the un-resolved path in
    // the profile — a write allow that silently never applied under
    // `(deny default)`.
    let tool_dir_str = resolve_symlinks_best_effort(tool_dir);
    validate_seatbelt_path(&tool_dir_str)?;

    let mut profile = String::from("(version 1)\n(deny default)\n");

    if let Some(bin) = runtime_bin {
        validate_seatbelt_path(bin)?;
        profile.push_str(&format!("(allow process-exec (literal \"{bin}\"))\n"));

        // Allow the runtime binary's parent directory (handles non-standard install
        // paths like ~/.local/bin, ~/.pyenv/shims, /opt/homebrew/Cellar/...)
        if let Some(parent) = Path::new(bin).parent() {
            let parent_str = parent.to_string_lossy();
            if parent_str != "/" && validate_seatbelt_path(&parent_str).is_ok() {
                profile.push_str(&format!(
                    "(allow process-exec (subpath \"{parent_str}\"))\n"
                ));
            }
        }

        // Also allow the symlink-resolved binary and its directory
        if let Ok(canonical) = Path::new(bin).canonicalize() {
            let canon_str = canonical.to_string_lossy().to_string();
            if canon_str != bin {
                if validate_seatbelt_path(&canon_str).is_ok() {
                    profile.push_str(&format!("(allow process-exec (literal \"{canon_str}\"))\n"));
                }
                if let Some(canon_parent) = canonical.parent() {
                    let canon_parent_str = canon_parent.to_string_lossy();
                    if canon_parent_str != "/" && validate_seatbelt_path(&canon_parent_str).is_ok()
                    {
                        profile.push_str(&format!(
                            "(allow process-exec (subpath \"{canon_parent_str}\"))\n"
                        ));
                    }
                }
            }
        }

        // Allow standard system binaries that scripts may invoke (cat, grep, curl, etc.)
        for path in STANDARD_EXEC_PATHS {
            profile.push_str(&format!("(allow process-exec (subpath \"{path}\"))\n"));
        }
    } else {
        profile.push_str("(allow process-exec)\n");
    }
    profile.push_str("(allow process-fork)\n");
    profile.push_str("(allow sysctl-read)\n");
    profile.push_str("(allow mach-lookup)\n");

    // Allow unrestricted file reads — runtimes need to read from many system
    // paths for dynamic linking, config, locale, etc. The security boundary is
    // write/exec/network restriction. Sensitive paths are filtered at the
    // manifest level via the blocked_paths security config.
    profile.push_str("(allow file-read*)\n");

    // Allow writing to /dev (stdout/stderr file descriptors)
    profile.push_str("(allow file-write* (subpath \"/dev\"))\n");
    // Both spellings are needed: /tmp is a symlink to /private/tmp on macOS and
    // Seatbelt matches the literal path the process opens, not the resolved one.
    profile.push_str("(allow file-write* (subpath \"/private/tmp\"))\n");
    profile.push_str("(allow file-write* (subpath \"/tmp\"))\n");
    profile.push_str(&format!(
        "(allow file-write* (subpath \"{tool_dir_str}\"))\n"
    ));

    for path in &policy.fs_read {
        push_subpath_rule(&mut profile, "allow", "file-read*", path)?;
    }

    for path in &policy.fs_write {
        push_subpath_rule(&mut profile, "allow", "file-write*", path)?;
    }

    // Defense-in-depth: deny reads to well-known sensitive directories even
    // though file-read* is broadly allowed. Emitted AFTER the fs_read allows so
    // that under last-match-wins these denies still hold when a policy grants a
    // broad read root (e.g. the `trusted` profile's `/`).
    //
    // Routed through `push_subpath_rule` so an unrepresentable path is a hard
    // error, like every other deny. Skipping it silently was the wrong
    // failure: on a `$HOME` the validator rejects (e.g. a non-ASCII username),
    // these denies vanished while the broad `(allow file-read*)` above stayed,
    // leaving `~/.ssh` and `~/.aws` readable from inside the sandbox.
    if let Ok(home_str) = std::env::var("HOME") {
        for sensitive in SENSITIVE_HOME_PATHS {
            let path = format!("{home_str}/{sensitive}");
            push_subpath_rule(&mut profile, "deny", "file-read*", &path)?;
        }
    }

    // Deny read access to specific paths (overrides the blanket allow).
    // Deny rules are security-critical — invalid paths are hard errors.
    for path in &policy.deny_read {
        push_subpath_rule(&mut profile, "deny", "file-read*", path)?;
    }

    // Deny write access to protected paths (e.g. ~/.borg/).
    for path in &policy.deny_write {
        push_subpath_rule(&mut profile, "deny", "file-write*", path)?;
    }

    if policy.network {
        profile.push_str("(allow network*)\n");
    }

    Ok(profile)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_TOOL_DIR: &str = "/tmp/tool";

    fn default_policy() -> SandboxPolicy {
        SandboxPolicy::default()
    }

    /// A tool dir that does not exist yet must still be resolved through the
    /// symlinked prefix. `canonicalize` fails on a missing leaf, and the old
    /// fallback emitted the raw path — so on macOS a `/var/…` write allow was
    /// inert under `(deny default)` because the kernel reports `/private/var/…`.
    #[cfg(target_os = "macos")]
    #[test]
    fn missing_tool_dir_still_resolves_symlinked_prefix() {
        let tool_dir = Path::new("/var/folders/borg-not-created-yet/tool");
        let profile = generate_profile(&default_policy(), tool_dir, None).unwrap();
        assert!(
            profile.contains(
                "(allow file-write* (subpath \"/private/var/folders/borg-not-created-yet/tool\"))"
            ),
            "tool dir write allow must use the kernel-visible path:\n{profile}"
        );
    }

    /// Deny rules are the security-critical direction: an un-resolved `/var/…`
    /// deny protects nothing. Emit both forms so whichever path the kernel
    /// reports is covered.
    #[cfg(target_os = "macos")]
    #[test]
    fn deny_rules_cover_both_raw_and_resolved_paths() {
        let policy = SandboxPolicy {
            deny_read: vec!["/var/secrets".to_string()],
            ..Default::default()
        };
        let profile = generate_profile(&policy, Path::new(TEST_TOOL_DIR), None).unwrap();
        assert!(
            profile.contains("(deny file-read* (subpath \"/var/secrets\"))"),
            "raw deny path must be retained:\n{profile}"
        );
        assert!(
            profile.contains("(deny file-read* (subpath \"/private/var/secrets\"))"),
            "resolved deny path must also be emitted:\n{profile}"
        );
    }

    /// A path with no symlinks in it must produce exactly one rule — the
    /// dual-emission must not double every entry in the common case.
    #[test]
    fn unsymlinked_paths_emit_a_single_rule() {
        let tmp = tempfile::tempdir().unwrap();
        let plain = tmp.path().canonicalize().unwrap();
        let plain_str = plain.to_string_lossy().into_owned();
        let policy = SandboxPolicy {
            deny_write: vec![plain_str.clone()],
            ..Default::default()
        };
        let profile = generate_profile(&policy, Path::new(TEST_TOOL_DIR), None).unwrap();
        // Match on *any* rule mentioning the path, not the one exact string we
        // expect: a near-miss variant (e.g. a trailing separator from joining an
        // empty suffix) would leave an exact-string count at 1 and slip through.
        let referencing: Vec<&str> = profile
            .lines()
            .filter(|l| l.starts_with("(deny file-write* (subpath ") && l.contains(&plain_str))
            .collect();
        assert_eq!(
            referencing,
            vec![format!("(deny file-write* (subpath \"{plain_str}\"))")],
            "already-canonical path must produce exactly one rule:\n{profile}"
        );
    }

    /// An existing path resolves to its canonical form with no trailing
    /// separator. `PathBuf::join("")` appends one, which both doubled every
    /// emitted rule and left the tool dir's only write grant depending on an
    /// undocumented `subpath` spelling.
    #[test]
    fn resolving_an_existing_path_adds_no_trailing_separator() {
        let tmp = tempfile::tempdir().unwrap();
        let canonical = tmp.path().canonicalize().unwrap();
        assert_eq!(
            resolve_symlinks_best_effort(&canonical),
            canonical.to_string_lossy()
        );
    }

    #[test]
    fn profile_starts_with_deny_default() {
        let profile = generate_profile(&default_policy(), Path::new(TEST_TOOL_DIR), None).unwrap();
        assert!(profile.contains("(deny default)"));
    }

    #[test]
    fn profile_includes_tool_dir() {
        let profile = generate_profile(&default_policy(), Path::new("/my/tool/dir"), None).unwrap();
        // Tool dir should have write access
        assert!(profile.contains("(allow file-write* (subpath \"/my/tool/dir\"))"));
        // Reads are allowed globally
        assert!(profile.contains("(allow file-read*)"));
    }

    #[test]
    fn profile_no_network_by_default() {
        let profile = generate_profile(&default_policy(), Path::new(TEST_TOOL_DIR), None).unwrap();
        assert!(!profile.contains("(allow network*)"));
    }

    #[test]
    fn profile_network_when_allowed() {
        let policy = SandboxPolicy {
            network: true,
            ..Default::default()
        };
        let profile = generate_profile(&policy, Path::new(TEST_TOOL_DIR), None).unwrap();
        assert!(profile.contains("(allow network*)"));
    }

    #[test]
    fn profile_additional_read_paths() {
        let policy = SandboxPolicy {
            fs_read: vec!["/data/input".to_string()],
            ..Default::default()
        };
        let profile = generate_profile(&policy, Path::new(TEST_TOOL_DIR), None).unwrap();
        assert!(profile.contains("(allow file-read* (subpath \"/data/input\"))"));
    }

    #[test]
    fn profile_additional_write_paths() {
        let policy = SandboxPolicy {
            fs_write: vec!["/data/output".to_string()],
            ..Default::default()
        };
        let profile = generate_profile(&policy, Path::new(TEST_TOOL_DIR), None).unwrap();
        assert!(profile.contains("(allow file-write* (subpath \"/data/output\"))"));
    }

    #[test]
    fn profile_broad_exec_without_runtime() {
        let profile = generate_profile(&default_policy(), Path::new(TEST_TOOL_DIR), None).unwrap();
        assert!(profile.contains("(allow process-exec)"));
        assert!(profile.contains("(allow process-fork)"));
    }

    #[test]
    fn profile_restricts_exec_to_runtime_binary() {
        let profile = generate_profile(
            &default_policy(),
            Path::new(TEST_TOOL_DIR),
            Some("/usr/bin/python3"),
        )
        .unwrap();
        assert!(profile.contains("(allow process-exec (literal \"/usr/bin/python3\"))"));
        assert!(!profile.contains("(allow process-exec)\n"));
        assert!(profile.contains("(allow process-fork)"));
    }

    #[test]
    fn rejects_quote_injection_in_fs_read() {
        let policy = SandboxPolicy {
            fs_read: vec!["\"))\n(allow default)\n(deny file-read* (subpath \"".to_string()],
            ..Default::default()
        };
        let result = generate_profile(&policy, Path::new(TEST_TOOL_DIR), None);
        assert!(result.is_err());
    }

    #[test]
    fn rejects_quote_injection_in_fs_write() {
        let policy = SandboxPolicy {
            fs_write: vec!["/foo\"))\n(allow default)".to_string()],
            ..Default::default()
        };
        let result = generate_profile(&policy, Path::new(TEST_TOOL_DIR), None);
        assert!(result.is_err());
    }

    #[test]
    fn rejects_relative_path() {
        let policy = SandboxPolicy {
            fs_read: vec!["relative/path".to_string()],
            ..Default::default()
        };
        let result = generate_profile(&policy, Path::new(TEST_TOOL_DIR), None);
        assert!(result.is_err());
    }

    #[test]
    fn rejects_parentheses_in_path() {
        let policy = SandboxPolicy {
            fs_read: vec!["/foo(bar)".to_string()],
            ..Default::default()
        };
        let result = generate_profile(&policy, Path::new(TEST_TOOL_DIR), None);
        assert!(result.is_err());
    }

    #[test]
    fn rejects_newline_in_path() {
        let policy = SandboxPolicy {
            fs_read: vec!["/foo\nbar".to_string()],
            ..Default::default()
        };
        let result = generate_profile(&policy, Path::new(TEST_TOOL_DIR), None);
        assert!(result.is_err());
    }

    /// Table-driven check that seatbelt path validation accepts the path
    /// shapes Seatbelt's S-expression syntax can quote safely and rejects
    /// the ones that would inject. The validator must:
    /// - allow ASCII paths with `-`, `_`, `/`, `.`, digits
    /// - reject non-ASCII (Seatbelt's parser can mishandle multibyte chars)
    #[test]
    fn validate_seatbelt_path_accepts_safe_and_rejects_unsafe() {
        let valid = ["/usr/local/bin", "/tmp/my-tool_dir/v2"];
        let invalid = ["/tmp/tëst"];
        for path in valid {
            validate_seatbelt_path(path)
                .unwrap_or_else(|e| panic!("expected {path:?} to be accepted: {e}"));
        }
        for path in invalid {
            assert!(
                validate_seatbelt_path(path).is_err(),
                "expected {path:?} to be rejected"
            );
        }
    }

    #[test]
    fn profile_allows_system_binaries_with_runtime() {
        let profile = generate_profile(
            &default_policy(),
            Path::new(TEST_TOOL_DIR),
            Some("/bin/bash"),
        )
        .unwrap();
        assert!(profile.contains("(allow process-exec (subpath \"/bin\"))"));
        assert!(profile.contains("(allow process-exec (subpath \"/usr/bin\"))"));
        assert!(profile.contains("(allow process-exec (subpath \"/usr/local/bin\"))"));
    }

    #[test]
    fn profile_allows_broad_file_read() {
        let profile = generate_profile(&default_policy(), Path::new(TEST_TOOL_DIR), None).unwrap();
        assert!(profile.contains("(allow file-read*)"));
    }

    #[test]
    fn profile_allows_dev_write_for_stdio() {
        let profile = generate_profile(&default_policy(), Path::new(TEST_TOOL_DIR), None).unwrap();
        assert!(profile.contains("(allow file-write* (subpath \"/dev\"))"));
    }

    #[test]
    fn profile_allows_tmp_write() {
        let profile = generate_profile(&default_policy(), Path::new(TEST_TOOL_DIR), None).unwrap();
        assert!(profile.contains("(allow file-write* (subpath \"/private/tmp\"))"));
        assert!(profile.contains("(allow file-write* (subpath \"/tmp\"))"));
    }

    #[test]
    fn profile_allows_tool_dir_write() {
        // /tmp may canonicalize to /private/tmp on macOS, so assert on
        // *either* path-component containing "my-tool". A `subpath "/"`
        // rule (matching the old weaker assertion) would be a sandbox
        // bypass, so we also assert the rule is NOT the bare root.
        let profile = generate_profile(&default_policy(), Path::new("/tmp/my-tool"), None).unwrap();
        let has_tool_dir_rule = profile.contains("(allow file-write* (subpath \"/tmp/my-tool\"))")
            || profile.contains("(allow file-write* (subpath \"/private/tmp/my-tool\"))");
        assert!(
            has_tool_dir_rule,
            "expected an explicit tool-dir write rule; profile was: {profile}"
        );
        assert!(
            !profile.contains("(allow file-write* (subpath \"/\"))"),
            "tool-dir rule must not degenerate to root"
        );
    }

    #[test]
    fn profile_allows_runtime_parent_directory() {
        let profile = generate_profile(
            &default_policy(),
            Path::new(TEST_TOOL_DIR),
            Some("/home/user/.local/bin/python3"),
        )
        .unwrap();
        // Literal rule for the binary itself
        assert!(
            profile.contains("(allow process-exec (literal \"/home/user/.local/bin/python3\"))")
        );
        // Subpath rule for the parent directory
        assert!(profile.contains("(allow process-exec (subpath \"/home/user/.local/bin\"))"));
    }

    #[test]
    fn profile_allows_homebrew_path() {
        let profile = generate_profile(
            &default_policy(),
            Path::new(TEST_TOOL_DIR),
            Some("/usr/bin/python3"),
        )
        .unwrap();
        assert!(profile.contains("(allow process-exec (subpath \"/opt/homebrew/bin\"))"));
    }

    #[test]
    fn profile_allows_opt_homebrew_runtime() {
        let profile = generate_profile(
            &default_policy(),
            Path::new(TEST_TOOL_DIR),
            Some("/opt/homebrew/bin/python3"),
        )
        .unwrap();
        assert!(profile.contains("(allow process-exec (literal \"/opt/homebrew/bin/python3\"))"));
        assert!(profile.contains("(allow process-exec (subpath \"/opt/homebrew/bin\"))"));
    }

    #[test]
    fn profile_runtime_parent_skips_root() {
        let profile = generate_profile(
            &default_policy(),
            Path::new(TEST_TOOL_DIR),
            Some("/python3"),
        )
        .unwrap();
        // Should NOT add (subpath "/") — that would be overly broad
        assert!(!profile.contains("(allow process-exec (subpath \"/\"))"));
        // But the literal should still be there
        assert!(profile.contains("(allow process-exec (literal \"/python3\"))"));
    }

    #[test]
    fn rejects_relative_runtime_binary() {
        let result = generate_profile(&default_policy(), Path::new(TEST_TOOL_DIR), Some("python3"));
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("absolute"));
    }

    #[test]
    fn validate_seatbelt_path_rejects_backslash() {
        assert!(validate_seatbelt_path("/tmp/path\\injection").is_err());
    }

    #[test]
    fn validate_seatbelt_path_rejects_carriage_return() {
        assert!(validate_seatbelt_path("/tmp/path\rfoo").is_err());
    }

    #[test]
    fn profile_includes_deny_read_rules() {
        let policy = SandboxPolicy {
            deny_read: vec!["/secret/data".to_string()],
            ..Default::default()
        };
        let profile = generate_profile(&policy, Path::new(TEST_TOOL_DIR), None).unwrap();
        assert!(profile.contains("(deny file-read* (subpath \"/secret/data\"))"));
    }

    #[test]
    fn configured_blocked_path_becomes_deny_rule() {
        // `.env` is NOT one of the hardcoded SENSITIVE_HOME_PATHS, so without
        // routing blocked_paths through with_blocked_paths_denied it would be
        // readable inside a workspace. The profile must deny it once wired.
        let policy = SandboxPolicy {
            fs_read: vec!["/ws".to_string()],
            fs_write: vec!["/ws".to_string()],
            ..Default::default()
        }
        .with_blocked_paths_denied(&[".env".to_string()], &["/ws".to_string()]);
        let profile = generate_profile(&policy, Path::new(TEST_TOOL_DIR), None).unwrap();
        assert!(
            profile.contains("(deny file-read* (subpath \"/ws/.env\"))"),
            "configured blocked path must deny reads:\n{profile}"
        );
        assert!(
            profile.contains("(deny file-write* (subpath \"/ws/.env\"))"),
            "configured blocked path must deny writes:\n{profile}"
        );
    }

    #[test]
    fn rejects_invalid_deny_read_path() {
        let policy = SandboxPolicy {
            deny_read: vec!["/foo\"))\n(allow default)".to_string()],
            ..Default::default()
        };
        let result = generate_profile(&policy, Path::new(TEST_TOOL_DIR), None);
        assert!(result.is_err());
    }

    #[test]
    fn rejects_invalid_deny_write_path() {
        let policy = SandboxPolicy {
            deny_write: vec!["relative/path".to_string()],
            ..Default::default()
        };
        let result = generate_profile(&policy, Path::new(TEST_TOOL_DIR), None);
        assert!(result.is_err());
    }

    #[test]
    fn profile_includes_deny_write_rules() {
        let policy = SandboxPolicy {
            deny_write: vec!["/home/user/.borg".to_string()],
            ..Default::default()
        };
        let profile = generate_profile(&policy, Path::new(TEST_TOOL_DIR), None).unwrap();
        assert!(profile.contains("(deny file-write* (subpath \"/home/user/.borg\"))"));
    }

    #[test]
    fn profile_denies_sensitive_home_dirs() {
        // Defense-in-depth: even with file-read* broadly allowed, each
        // SENSITIVE_HOME_PATHS entry under $HOME must get an explicit
        // deny rule. The old assertion `(subpath "/"` matched any deny
        // rule and would pass even if all sensitive paths were dropped.
        let Ok(home) = std::env::var("HOME") else {
            // Production callers always have HOME set; skipping rather
            // than asserting on a path the function won't emit.
            eprintln!("skipping: HOME unset in test env");
            return;
        };
        let profile = generate_profile(&default_policy(), Path::new(TEST_TOOL_DIR), None).unwrap();
        for sensitive in [".ssh", ".aws", ".gnupg"] {
            let expected = format!("(deny file-read* (subpath \"{home}/{sensitive}\"))");
            assert!(
                profile.contains(&expected),
                "missing deny rule for {sensitive}; profile:\n{profile}"
            );
        }
    }

    #[test]
    fn sensitive_deny_wins_over_broad_fs_read() {
        // M2: under last-match-wins, the sensitive-home deny must be emitted
        // AFTER a broad fs_read allow (e.g. the `trusted` profile's `/`), or a
        // tool granted read of `/` could read ~/.ssh anyway.
        let Ok(home) = std::env::var("HOME") else {
            eprintln!("skipping: HOME unset in test env");
            return;
        };
        let policy = SandboxPolicy {
            fs_read: vec!["/".to_string()],
            deny_read: vec![format!("{home}/.borg/secrets")],
            ..Default::default()
        };
        let profile = generate_profile(&policy, Path::new(TEST_TOOL_DIR), None).unwrap();
        let allow_root = profile.find("(allow file-read* (subpath \"/\"))").unwrap();
        let deny_ssh = profile
            .find(&format!("(deny file-read* (subpath \"{home}/.ssh\"))"))
            .expect("sensitive .ssh deny must be present");
        let deny_secrets = profile
            .find(&format!(
                "(deny file-read* (subpath \"{home}/.borg/secrets\"))"
            ))
            .expect("borg secrets deny must be present");
        assert!(
            allow_root < deny_ssh && allow_root < deny_secrets,
            "sensitive/secret denies must come AFTER the broad fs_read allow"
        );
    }
}
