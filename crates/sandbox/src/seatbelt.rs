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

/// Collect the `literal` deny targets that stop `path` from being renamed out
/// from under its `deny file-read*` / `deny file-write*` `subpath` rules.
///
/// A `subpath` deny is matched against the path the process opens, so it only
/// protects a file while the file still lives there. If `$HOME` is writable —
/// the `trusted` profile grants exactly that — then `mv ~/.ssh /tmp/loot`
/// relocates the whole directory to a path no deny rule mentions, and the next
/// `cat /tmp/loot/id_rsa` is an ordinary allowed read. Denying `file-write-unlink`
/// on the protected path *and on each of its ancestor directories* removes the
/// rename, because a rename needs unlink rights on its source.
///
/// The targets are `literal`, never `subpath`: `(deny file-write-unlink (literal
/// "/Users/me"))` names that one directory, so a sandboxed build is still free to
/// create and delete files *inside* it. Emitting `subpath` here would make the
/// tool dir unusable.
///
/// It does mean a guarded directory cannot be `rmdir`-ed either — `file-write-unlink`
/// covers removal as well as rename. So a `deny_read` entry at
/// `<workspace>/build/secret.txt` leaves `rm -rf build` able to empty the directory
/// but not to remove it. That is the intended trade: the alternative is letting the
/// directory be moved somewhere no deny rule names.
///
/// The root itself is skipped — it cannot be renamed, and a deny on `/` reads
/// like a much broader rule than it is.
fn rename_guard_targets(path: &str) -> Vec<String> {
    let mut targets = Vec::new();
    let mut current = Some(Path::new(path));
    while let Some(dir) = current {
        let text = dir.to_string_lossy();
        if text == "/" || text.is_empty() {
            break;
        }
        targets.push(text.into_owned());
        current = dir.parent();
    }
    targets
}

/// Emit the rename guards for every protected path, deduplicated.
///
/// Ancestors are shared heavily — every `SENSITIVE_HOME_PATHS` entry contributes
/// the same `$HOME`, `/Users`, … chain — so without the set a profile repeats one
/// rule a dozen times.
fn push_rename_guards(profile: &mut String, protected: &[String]) {
    let mut seen = std::collections::BTreeSet::new();
    for path in protected {
        for target in rename_guard_targets(path) {
            let resolved = resolve_symlinks_best_effort(Path::new(&target));
            for form in [target.clone(), resolved] {
                if !seen.insert(form.clone()) {
                    continue;
                }
                // The protected path itself already passed `validate_seatbelt_path`
                // via `push_subpath_rule`; an ancestor can still fail it (a
                // non-ASCII username, say). Skipping a *guard* is not the same
                // failure as skipping a deny — the deny still stands, this only
                // leaves the rename open — so warn and keep the rest rather than
                // refusing to build a sandbox at all.
                if validate_seatbelt_path(&form).is_err() {
                    tracing::warn!(
                        path = %form,
                        "Seatbelt rename guard skipped: path is not expressible in a profile"
                    );
                    continue;
                }
                profile.push_str(&format!("(deny file-write-unlink (literal \"{form}\"))\n"));
            }
        }
    }
}

/// Generate a macOS Seatbelt profile string for sandbox-exec.
///
/// `runtime_bin` is the absolute path to the runtime binary (e.g.
/// `/usr/bin/python3`) that the tool is allowed to execute. It is required:
/// the parameter used to be optional, and passing `None` emitted a blanket
/// `(allow process-exec)` that let the sandboxed process run anything at all.
/// No caller ever did — the escape hatch only existed for tests — so the type
/// now makes it unrepresentable.
pub fn generate_profile(
    policy: &SandboxPolicy,
    tool_dir: &Path,
    runtime_bin: &str,
) -> Result<String, String> {
    // Resolve symlinks (e.g. /var -> /private/var on macOS) so the emitted
    // `subpath` matches what the kernel sees. `canonicalize` fails outright when
    // the leaf does not exist yet, which used to leave the un-resolved path in
    // the profile — a write allow that silently never applied under
    // `(deny default)`.
    let tool_dir_str = resolve_symlinks_best_effort(tool_dir);
    validate_seatbelt_path(&tool_dir_str)?;

    let mut profile = String::from("(version 1)\n(deny default)\n");

    let bin = runtime_bin;
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
                if canon_parent_str != "/" && validate_seatbelt_path(&canon_parent_str).is_ok() {
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
    let mut protected: Vec<String> = Vec::new();
    if let Ok(home_str) = std::env::var("HOME") {
        for sensitive in SENSITIVE_HOME_PATHS {
            let path = format!("{home_str}/{sensitive}");
            push_subpath_rule(&mut profile, "deny", "file-read*", &path)?;
            // The read deny alone protects the key only while it sits at this
            // path. `mv ~/.ssh/id_rsa /tmp/loot` needs unlink on the *file*,
            // which the read deny does not cover and the rename guards below
            // do not either — they guard directories. `cat /tmp/loot` is then
            // an ordinary allowed read. `file-write*` also subsumes
            // `file-link`, so it closes `ln ~/.ssh/id_rsa /tmp/hard` in the
            // same stroke — a hardlink needs no unlink at all.
            //
            // Nothing legitimate writes into `~/.ssh`, `~/.aws`, `~/.gnupg`,
            // or `~/.config/gh` from inside a sandbox, so this costs nothing.
            push_subpath_rule(&mut profile, "deny", "file-write*", &path)?;
            protected.push(path);
        }
    }

    // Deny read access to specific paths (overrides the blanket allow).
    // Deny rules are security-critical — invalid paths are hard errors.
    for path in &policy.deny_read {
        push_subpath_rule(&mut profile, "deny", "file-read*", path)?;
        protected.push(path.clone());
    }

    // Deny write access to protected paths (e.g. ~/.borg/).
    for path in &policy.deny_write {
        push_subpath_rule(&mut profile, "deny", "file-write*", path)?;
        protected.push(path.clone());
    }

    // Last, so these survive last-match-wins against every write allow above.
    push_rename_guards(&mut profile, &protected);

    if policy.network {
        profile.push_str("(allow network*)\n");
    }

    Ok(profile)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_TOOL_DIR: &str = "/tmp/tool";
    /// Stand-in runtime for tests that assert on parts of the profile other
    /// than the exec allowances.
    const TEST_RUNTIME_BIN: &str = "/usr/bin/python3";

    fn default_policy() -> SandboxPolicy {
        SandboxPolicy::default()
    }

    /// `$HOME` for the tests that assert on the built-in sensitive-path rules,
    /// or `None` when it is not expressible in a Seatbelt profile (a non-ASCII
    /// username, say). Those tests would otherwise fail for a reason that has
    /// nothing to do with the behaviour they pin — the rules are correctly
    /// skipped with a warning in that case.
    fn expressible_home() -> Option<String> {
        let home = std::env::var("HOME").ok()?;
        validate_seatbelt_path(&home).ok()?;
        Some(home)
    }

    /// Spec: a `deny file-read* (subpath "…/.ssh")` rule only protects the key
    /// while the key still lives at that path. With a writable `$HOME` the
    /// sandbox can `mv ~/.ssh /tmp/loot` and then read `/tmp/loot/id_rsa`
    /// through the blanket `(allow file-read*)`. The deny on the directory
    /// itself is what removes the rename.
    #[test]
    fn a_protected_directory_cannot_be_renamed_out_from_under_its_deny() {
        let Some(home) = expressible_home() else {
            return;
        };
        let profile = generate_profile(
            &default_policy(),
            Path::new(TEST_TOOL_DIR),
            TEST_RUNTIME_BIN,
        )
        .unwrap();
        assert!(
            profile.contains(&format!(
                "(deny file-write-unlink (literal \"{home}/.ssh\"))"
            )),
            "renaming ~/.ssh must be denied:\n{profile}"
        );
    }

    /// Spec: the rename needs unlink rights on its *source*, and moving the home
    /// directory relocates every sensitive path under it at once. Guarding only
    /// the leaf leaves `mv ~ /tmp/loot` open.
    #[test]
    fn ancestors_of_a_protected_path_are_guarded_against_rename() {
        let Some(home) = expressible_home() else {
            return;
        };
        let profile = generate_profile(
            &default_policy(),
            Path::new(TEST_TOOL_DIR),
            TEST_RUNTIME_BIN,
        )
        .unwrap();
        assert!(
            profile.contains(&format!("(deny file-write-unlink (literal \"{home}\"))")),
            "renaming the home directory must be denied:\n{profile}"
        );
    }

    /// Spec: the guard is `literal`, not `subpath`. A `subpath` deny on `$HOME`
    /// would stop a sandboxed build from deleting any of its own files — the
    /// tool dir included — which is a far larger change than closing the rename.
    #[test]
    fn rename_guards_do_not_block_deletes_inside_a_guarded_directory() {
        let profile = generate_profile(
            &default_policy(),
            Path::new(TEST_TOOL_DIR),
            TEST_RUNTIME_BIN,
        )
        .unwrap();
        assert!(
            !profile.contains("(deny file-write-unlink (subpath"),
            "guards must stay literal so ordinary deletes keep working:\n{profile}"
        );
    }

    /// Spec: guarding the directory is not enough. `mv ~/.ssh/id_rsa /tmp/loot`
    /// needs unlink on the *file*, which no rename guard covers, and the read
    /// deny stops applying the moment the file leaves the path. Verified
    /// against `sandbox-exec`: with the read deny alone the key moves out and
    /// `cat` prints it; with this rule both the move and `ln` are refused.
    #[test]
    fn sensitive_home_paths_deny_writes_not_only_reads() {
        let Some(home) = expressible_home() else {
            return;
        };
        let profile = generate_profile(
            &default_policy(),
            Path::new(TEST_TOOL_DIR),
            TEST_RUNTIME_BIN,
        )
        .unwrap();
        assert!(
            profile.contains(&format!("(deny file-write* (subpath \"{home}/.ssh\"))")),
            "a read-only deny lets the key be moved or hardlinked out:\n{profile}"
        );
    }

    /// Spec: last-match-wins means the sensitive-path write deny is only worth
    /// anything if it follows the policy's own `fs_write` allows. A profile that
    /// grants write on `$HOME` (the `trusted` shape) must still refuse `~/.ssh`.
    #[test]
    fn the_sensitive_write_deny_outranks_a_broad_write_allow() {
        let Some(home) = expressible_home() else {
            return;
        };
        let policy = SandboxPolicy {
            fs_write: vec![home.clone()],
            ..Default::default()
        };
        let profile =
            generate_profile(&policy, Path::new(TEST_TOOL_DIR), TEST_RUNTIME_BIN).unwrap();
        let allow = profile
            .find(&format!("(allow file-write* (subpath \"{home}\"))"))
            .expect("the broad home write allow must be present");
        let deny = profile
            .find(&format!("(deny file-write* (subpath \"{home}/.ssh\"))"))
            .expect("the sensitive write deny must be present");
        assert!(
            deny > allow,
            "the deny must follow the allow or last-match-wins voids it:\n{profile}"
        );
    }

    /// Spec: a `deny_write` entry is protected against rename for the same
    /// reason a `deny_read` entry is — only `deny_read` was covered before.
    #[test]
    fn policy_deny_write_paths_also_get_rename_guards() {
        let policy = SandboxPolicy {
            deny_write: vec!["/opt/state/ledger".to_string()],
            ..Default::default()
        };
        let profile =
            generate_profile(&policy, Path::new(TEST_TOOL_DIR), TEST_RUNTIME_BIN).unwrap();
        for expected in [
            "(deny file-write-unlink (literal \"/opt/state/ledger\"))",
            "(deny file-write-unlink (literal \"/opt/state\"))",
        ] {
            assert!(profile.contains(expected), "missing {expected}:\n{profile}");
        }
    }

    /// Spec: an un-resolved `/tmp/…` guard protects nothing on macOS, where the
    /// kernel reports `/private/tmp/…`. Both forms must be emitted, the same
    /// rule `push_subpath_rule` follows for denies.
    #[cfg(target_os = "macos")]
    #[test]
    fn rename_guards_cover_both_raw_and_resolved_ancestors() {
        let policy = SandboxPolicy {
            deny_read: vec!["/tmp/vault/keys".to_string()],
            ..Default::default()
        };
        let profile =
            generate_profile(&policy, Path::new(TEST_TOOL_DIR), TEST_RUNTIME_BIN).unwrap();
        assert!(
            profile.contains("(deny file-write-unlink (literal \"/tmp/vault\"))"),
            "raw ancestor guard missing:\n{profile}"
        );
        assert!(
            profile.contains("(deny file-write-unlink (literal \"/private/tmp/vault\"))"),
            "resolved ancestor guard missing — the kernel reports this form:\n{profile}"
        );
    }

    /// Spec: guarding a directory also stops it being removed, because
    /// `file-write-unlink` covers `rmdir` as well as rename. Pinned so the
    /// trade-off is a decision rather than a surprise: `rm -rf build` can empty
    /// a guarded directory but not remove it.
    #[test]
    fn guarding_a_directory_also_prevents_removing_it() {
        let policy = SandboxPolicy {
            deny_read: vec!["/ws/build/secret.txt".to_string()],
            ..Default::default()
        };
        let profile =
            generate_profile(&policy, Path::new(TEST_TOOL_DIR), TEST_RUNTIME_BIN).unwrap();
        assert!(
            profile.contains("(deny file-write-unlink (literal \"/ws/build\"))"),
            "the containing directory must be guarded:\n{profile}"
        );
        assert!(
            !profile.contains("(deny file-write-unlink (subpath \"/ws/build\"))"),
            "files inside must stay deletable:\n{profile}"
        );
    }

    /// Spec: policy-supplied denies get the same treatment as the built-in
    /// sensitive-home list — the bypass does not care where the deny came from.
    #[test]
    fn policy_deny_paths_also_get_rename_guards() {
        let policy = SandboxPolicy {
            deny_read: vec!["/opt/vault/keys".to_string()],
            ..Default::default()
        };
        let profile =
            generate_profile(&policy, Path::new(TEST_TOOL_DIR), TEST_RUNTIME_BIN).unwrap();
        for expected in [
            "(deny file-write-unlink (literal \"/opt/vault/keys\"))",
            "(deny file-write-unlink (literal \"/opt/vault\"))",
            "(deny file-write-unlink (literal \"/opt\"))",
        ] {
            assert!(profile.contains(expected), "missing {expected}:\n{profile}");
        }
    }

    /// Spec: `/` cannot be renamed, and a deny on it reads like a far broader
    /// rule than it is. The walk stops before emitting one.
    #[test]
    fn the_filesystem_root_gets_no_rename_guard() {
        let policy = SandboxPolicy {
            deny_read: vec!["/opt/vault/keys".to_string()],
            ..Default::default()
        };
        let profile =
            generate_profile(&policy, Path::new(TEST_TOOL_DIR), TEST_RUNTIME_BIN).unwrap();
        assert!(
            !profile.contains("(deny file-write-unlink (literal \"/\"))"),
            "the root must not be guarded:\n{profile}"
        );
    }

    /// Spec: Seatbelt is last-match-wins, so a guard emitted before a broad
    /// `file-write*` allow is inert. Every guard must sit after the last allow.
    #[test]
    fn rename_guards_are_emitted_after_every_write_allow() {
        let policy = SandboxPolicy {
            fs_write: vec!["/opt".to_string()],
            deny_read: vec!["/opt/vault/keys".to_string()],
            ..Default::default()
        };
        let profile =
            generate_profile(&policy, Path::new(TEST_TOOL_DIR), TEST_RUNTIME_BIN).unwrap();
        let last_allow = profile
            .rfind("(allow file-write*")
            .expect("profile must contain a write allow");
        let first_guard = profile
            .find("(deny file-write-unlink")
            .expect("profile must contain a rename guard");
        assert!(
            first_guard > last_allow,
            "guards must follow every write allow or last-match-wins voids them:\n{profile}"
        );
    }

    /// Spec: every sensitive path shares the same `$HOME` / `/Users` chain, so
    /// the emitted guards must be deduplicated rather than repeated per entry.
    #[test]
    fn shared_ancestors_are_emitted_once() {
        let profile = generate_profile(
            &default_policy(),
            Path::new(TEST_TOOL_DIR),
            TEST_RUNTIME_BIN,
        )
        .unwrap();
        let Some(home) = expressible_home() else {
            return;
        };
        let rule = format!("(deny file-write-unlink (literal \"{home}\"))");
        assert_eq!(
            profile.matches(&rule).count(),
            1,
            "the shared home ancestor must appear once:\n{profile}"
        );
    }

    /// A tool dir that does not exist yet must still be resolved through the
    /// symlinked prefix. `canonicalize` fails on a missing leaf, and the old
    /// fallback emitted the raw path — so on macOS a `/var/…` write allow was
    /// inert under `(deny default)` because the kernel reports `/private/var/…`.
    #[cfg(target_os = "macos")]
    #[test]
    fn missing_tool_dir_still_resolves_symlinked_prefix() {
        let tool_dir = Path::new("/var/folders/borg-not-created-yet/tool");
        let profile = generate_profile(&default_policy(), tool_dir, TEST_RUNTIME_BIN).unwrap();
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
        let profile =
            generate_profile(&policy, Path::new(TEST_TOOL_DIR), TEST_RUNTIME_BIN).unwrap();
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
        let profile =
            generate_profile(&policy, Path::new(TEST_TOOL_DIR), TEST_RUNTIME_BIN).unwrap();
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
        let profile = generate_profile(
            &default_policy(),
            Path::new(TEST_TOOL_DIR),
            TEST_RUNTIME_BIN,
        )
        .unwrap();
        assert!(profile.contains("(deny default)"));
    }

    #[test]
    fn profile_includes_tool_dir() {
        let profile = generate_profile(
            &default_policy(),
            Path::new("/my/tool/dir"),
            TEST_RUNTIME_BIN,
        )
        .unwrap();
        // Tool dir should have write access
        assert!(profile.contains("(allow file-write* (subpath \"/my/tool/dir\"))"));
        // Reads are allowed globally
        assert!(profile.contains("(allow file-read*)"));
    }

    #[test]
    fn profile_no_network_by_default() {
        let profile = generate_profile(
            &default_policy(),
            Path::new(TEST_TOOL_DIR),
            TEST_RUNTIME_BIN,
        )
        .unwrap();
        assert!(!profile.contains("(allow network*)"));
    }

    #[test]
    fn profile_network_when_allowed() {
        let policy = SandboxPolicy {
            network: true,
            ..Default::default()
        };
        let profile =
            generate_profile(&policy, Path::new(TEST_TOOL_DIR), TEST_RUNTIME_BIN).unwrap();
        assert!(profile.contains("(allow network*)"));
    }

    #[test]
    fn profile_additional_read_paths() {
        let policy = SandboxPolicy {
            fs_read: vec!["/data/input".to_string()],
            ..Default::default()
        };
        let profile =
            generate_profile(&policy, Path::new(TEST_TOOL_DIR), TEST_RUNTIME_BIN).unwrap();
        assert!(profile.contains("(allow file-read* (subpath \"/data/input\"))"));
    }

    #[test]
    fn profile_additional_write_paths() {
        let policy = SandboxPolicy {
            fs_write: vec!["/data/output".to_string()],
            ..Default::default()
        };
        let profile =
            generate_profile(&policy, Path::new(TEST_TOOL_DIR), TEST_RUNTIME_BIN).unwrap();
        assert!(profile.contains("(allow file-write* (subpath \"/data/output\"))"));
    }

    #[test]
    fn profile_never_emits_a_blanket_exec_allowance() {
        // `runtime_bin` used to be optional, and `None` emitted a bare
        // `(allow process-exec)` that let the sandboxed process run anything.
        // The parameter is required now, so that rule must never appear —
        // every exec allowance has to name a literal or a subpath.
        let profile = generate_profile(
            &default_policy(),
            Path::new(TEST_TOOL_DIR),
            TEST_RUNTIME_BIN,
        )
        .unwrap();
        assert!(
            !profile.contains("(allow process-exec)"),
            "blanket process-exec is a sandbox escape:\n{profile}"
        );
        assert!(profile.contains(&format!(
            "(allow process-exec (literal \"{TEST_RUNTIME_BIN}\"))"
        )));
        assert!(profile.contains("(allow process-fork)"));
    }

    #[test]
    fn profile_restricts_exec_to_runtime_binary() {
        let profile = generate_profile(
            &default_policy(),
            Path::new(TEST_TOOL_DIR),
            "/usr/bin/python3",
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
        let result = generate_profile(&policy, Path::new(TEST_TOOL_DIR), TEST_RUNTIME_BIN);
        assert!(result.is_err());
    }

    #[test]
    fn rejects_quote_injection_in_fs_write() {
        let policy = SandboxPolicy {
            fs_write: vec!["/foo\"))\n(allow default)".to_string()],
            ..Default::default()
        };
        let result = generate_profile(&policy, Path::new(TEST_TOOL_DIR), TEST_RUNTIME_BIN);
        assert!(result.is_err());
    }

    #[test]
    fn rejects_relative_path() {
        let policy = SandboxPolicy {
            fs_read: vec!["relative/path".to_string()],
            ..Default::default()
        };
        let result = generate_profile(&policy, Path::new(TEST_TOOL_DIR), TEST_RUNTIME_BIN);
        assert!(result.is_err());
    }

    #[test]
    fn rejects_parentheses_in_path() {
        let policy = SandboxPolicy {
            fs_read: vec!["/foo(bar)".to_string()],
            ..Default::default()
        };
        let result = generate_profile(&policy, Path::new(TEST_TOOL_DIR), TEST_RUNTIME_BIN);
        assert!(result.is_err());
    }

    #[test]
    fn rejects_newline_in_path() {
        let policy = SandboxPolicy {
            fs_read: vec!["/foo\nbar".to_string()],
            ..Default::default()
        };
        let result = generate_profile(&policy, Path::new(TEST_TOOL_DIR), TEST_RUNTIME_BIN);
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
        let profile =
            generate_profile(&default_policy(), Path::new(TEST_TOOL_DIR), "/bin/bash").unwrap();
        assert!(profile.contains("(allow process-exec (subpath \"/bin\"))"));
        assert!(profile.contains("(allow process-exec (subpath \"/usr/bin\"))"));
        assert!(profile.contains("(allow process-exec (subpath \"/usr/local/bin\"))"));
    }

    #[test]
    fn profile_allows_broad_file_read() {
        let profile = generate_profile(
            &default_policy(),
            Path::new(TEST_TOOL_DIR),
            TEST_RUNTIME_BIN,
        )
        .unwrap();
        assert!(profile.contains("(allow file-read*)"));
    }

    #[test]
    fn profile_allows_dev_write_for_stdio() {
        let profile = generate_profile(
            &default_policy(),
            Path::new(TEST_TOOL_DIR),
            TEST_RUNTIME_BIN,
        )
        .unwrap();
        assert!(profile.contains("(allow file-write* (subpath \"/dev\"))"));
    }

    #[test]
    fn profile_allows_tmp_write() {
        let profile = generate_profile(
            &default_policy(),
            Path::new(TEST_TOOL_DIR),
            TEST_RUNTIME_BIN,
        )
        .unwrap();
        assert!(profile.contains("(allow file-write* (subpath \"/private/tmp\"))"));
        assert!(profile.contains("(allow file-write* (subpath \"/tmp\"))"));
    }

    #[test]
    fn profile_allows_tool_dir_write() {
        // /tmp may canonicalize to /private/tmp on macOS, so assert on
        // *either* path-component containing "my-tool". A `subpath "/"`
        // rule (matching the old weaker assertion) would be a sandbox
        // bypass, so we also assert the rule is NOT the bare root.
        let profile = generate_profile(
            &default_policy(),
            Path::new("/tmp/my-tool"),
            TEST_RUNTIME_BIN,
        )
        .unwrap();
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
            "/home/user/.local/bin/python3",
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
            "/usr/bin/python3",
        )
        .unwrap();
        assert!(profile.contains("(allow process-exec (subpath \"/opt/homebrew/bin\"))"));
    }

    #[test]
    fn profile_allows_opt_homebrew_runtime() {
        let profile = generate_profile(
            &default_policy(),
            Path::new(TEST_TOOL_DIR),
            "/opt/homebrew/bin/python3",
        )
        .unwrap();
        assert!(profile.contains("(allow process-exec (literal \"/opt/homebrew/bin/python3\"))"));
        assert!(profile.contains("(allow process-exec (subpath \"/opt/homebrew/bin\"))"));
    }

    #[test]
    fn profile_runtime_parent_skips_root() {
        let profile =
            generate_profile(&default_policy(), Path::new(TEST_TOOL_DIR), "/python3").unwrap();
        // Should NOT add (subpath "/") — that would be overly broad
        assert!(!profile.contains("(allow process-exec (subpath \"/\"))"));
        // But the literal should still be there
        assert!(profile.contains("(allow process-exec (literal \"/python3\"))"));
    }

    #[test]
    fn rejects_relative_runtime_binary() {
        let result = generate_profile(&default_policy(), Path::new(TEST_TOOL_DIR), "python3");
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
        let profile =
            generate_profile(&policy, Path::new(TEST_TOOL_DIR), TEST_RUNTIME_BIN).unwrap();
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
        let profile =
            generate_profile(&policy, Path::new(TEST_TOOL_DIR), TEST_RUNTIME_BIN).unwrap();
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
        let result = generate_profile(&policy, Path::new(TEST_TOOL_DIR), TEST_RUNTIME_BIN);
        assert!(result.is_err());
    }

    #[test]
    fn rejects_invalid_deny_write_path() {
        let policy = SandboxPolicy {
            deny_write: vec!["relative/path".to_string()],
            ..Default::default()
        };
        let result = generate_profile(&policy, Path::new(TEST_TOOL_DIR), TEST_RUNTIME_BIN);
        assert!(result.is_err());
    }

    #[test]
    fn profile_includes_deny_write_rules() {
        let policy = SandboxPolicy {
            deny_write: vec!["/home/user/.borg".to_string()],
            ..Default::default()
        };
        let profile =
            generate_profile(&policy, Path::new(TEST_TOOL_DIR), TEST_RUNTIME_BIN).unwrap();
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
        let profile = generate_profile(
            &default_policy(),
            Path::new(TEST_TOOL_DIR),
            TEST_RUNTIME_BIN,
        )
        .unwrap();
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
        let profile =
            generate_profile(&policy, Path::new(TEST_TOOL_DIR), TEST_RUNTIME_BIN).unwrap();
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
