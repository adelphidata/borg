use crate::constants::{SYSTEM_RO_BINDS, SYSTEM_TMPFS_MOUNTS};
use crate::policy::SandboxPolicy;
use std::path::Path;

/// Detected bubblewrap version for feature gating.
#[derive(Debug, Clone, PartialEq)]
pub struct BwrapVersion {
    /// Major version number.
    pub major: u32,
    /// Minor version number.
    pub minor: u32,
    /// Patch version number.
    pub patch: u32,
}

impl BwrapVersion {
    /// Returns true if this version supports `--die-with-parent` (added in 0.1.8).
    pub fn supports_die_with_parent(&self) -> bool {
        (self.major, self.minor, self.patch) >= (0, 1, 8)
    }

    /// Returns true if this version supports `--unshare-user` (added in 0.2.0).
    pub fn supports_unshare_user(&self) -> bool {
        (self.major, self.minor) >= (0, 2)
    }

    /// Returns true if this version supports `--cap-drop` (added in 0.3.0).
    pub fn supports_cap_drop(&self) -> bool {
        (self.major, self.minor) >= (0, 3)
    }
}

/// Detect the installed bwrap version by running `bwrap --version`.
/// Returns `None` if bwrap is not found or version cannot be parsed.
pub fn detect_bwrap_version() -> Option<BwrapVersion> {
    let output = std::process::Command::new("bwrap")
        .arg("--version")
        .output()
        .ok()?;
    parse_bwrap_version(&String::from_utf8_lossy(&output.stdout))
}

/// Parse a bwrap version string like "bubblewrap 0.4.1" or "0.6.2".
pub fn parse_bwrap_version(output: &str) -> Option<BwrapVersion> {
    let version_str = output.trim().rsplit(' ').next()?;
    let mut parts = version_str.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    Some(BwrapVersion {
        major,
        minor,
        patch,
    })
}

/// Build bwrap (Bubblewrap) argument list for Linux sandboxing
pub fn build_bwrap_args(policy: &SandboxPolicy, tool_dir: &Path) -> Vec<String> {
    build_bwrap_args_versioned(policy, tool_dir, detect_bwrap_version().as_ref())
}

fn push_bind(args: &mut Vec<String>, flag: &str, path: &str) {
    args.extend([flag.to_string(), path.to_string(), path.to_string()]);
}

/// [`push_bind`] that drops the entry when the source path is absent.
///
/// bwrap exits non-zero on a missing bind source ("Can't find source path …")
/// and runs nothing, so an allow-list entry for a path that happens not to
/// exist would fail the *entire* sandboxed invocation rather than simply
/// granting nothing. Skipping is safe by construction: these are allow rules,
/// so omitting one only ever narrows access. Deny rules use [`hide_path`],
/// which has its own missing-path handling.
fn push_bind_if_exists(args: &mut Vec<String>, flag: &str, path: &str) {
    if Path::new(path).exists() {
        push_bind(args, flag, path);
    } else {
        tracing::debug!(
            path,
            flag,
            "sandbox: skipping bind for a path that does not exist"
        );
    }
}

/// [`push_bind`] for a write grant, materializing the directory when absent.
///
/// Silently skipping a write grant is worse than either alternative: the path
/// often still *exists* inside the namespace — under `--tmpfs /tmp`, or beneath a
/// broader bind — so the tool creates it, writes its output, exits 0, and
/// everything it produced dies with the namespace. We would report success for
/// work that never landed on the host. Creating the directory keeps the grant
/// real; if that fails we warn and drop the entry rather than let bwrap refuse
/// the entire invocation over one stale config line.
fn push_write_bind(args: &mut Vec<String>, path: &str) {
    let p = Path::new(path);
    if !p.exists() {
        if let Err(e) = std::fs::create_dir_all(p) {
            tracing::warn!(
                path,
                error = %e,
                "sandbox: dropping write grant — path is absent and could not be created; \
                 writes there will not persist"
            );
            return;
        }
    }
    push_bind(args, "--bind", path);
}

/// Make `path` unreadable inside the sandbox by overlaying it: an empty tmpfs
/// for directories, a read-only bind of `/dev/null` for regular files. Skips
/// non-existent paths — bwrap errors on a mount target that doesn't exist.
fn hide_path(args: &mut Vec<String>, path: &str) {
    let p = Path::new(path);
    match std::fs::symlink_metadata(p) {
        Ok(meta) if meta.is_dir() => {
            args.extend(["--tmpfs".to_string(), path.to_string()]);
        }
        Ok(_) => {
            // Regular file or symlink: shadow it with /dev/null (unreadable
            // content) rather than a tmpfs, which cannot mount over a file.
            args.extend([
                "--ro-bind".to_string(),
                "/dev/null".to_string(),
                path.to_string(),
            ]);
        }
        Err(_) => {
            // Path doesn't exist — nothing to hide, and mounting over a missing
            // target would make bwrap fail the whole spawn.
        }
    }
}

/// Build bwrap args with an explicit version for testability.
pub fn build_bwrap_args_versioned(
    policy: &SandboxPolicy,
    tool_dir: &Path,
    version: Option<&BwrapVersion>,
) -> Vec<String> {
    let mut args = Vec::new();

    push_bind_if_exists(&mut args, "--ro-bind", &tool_dir.to_string_lossy());

    for path in SYSTEM_RO_BINDS {
        push_bind_if_exists(&mut args, "--ro-bind", path);
    }

    for mount in SYSTEM_TMPFS_MOUNTS {
        args.extend(["--tmpfs".to_string(), (*mount).to_string()]);
    }
    args.extend(["--proc".to_string(), "/proc".to_string()]);
    args.extend(["--dev".to_string(), "/dev".to_string()]);
    // `--dev` overlays a minimal device tree, which also hides `/dev/shm`.
    // Give the sandbox its own tmpfs there so POSIX shared memory keeps
    // working — Python's `multiprocessing` and anything using `/dev/shm` for
    // scratch space break without it. A *private* tmpfs, never a bind of the
    // host's `/dev/shm`: that directory is world-writable and host-visible, so
    // binding it would hand every sandboxed tool a writable channel outside
    // every `fs_write`/`deny_write` rule, plus read access to other processes'
    // shared-memory segments.
    args.extend(["--tmpfs".to_string(), "/dev/shm".to_string()]);

    for path in &policy.fs_read {
        push_bind_if_exists(&mut args, "--ro-bind", path);
    }

    for path in &policy.fs_write {
        let denied = policy.deny_write.iter().any(|d| path.starts_with(d));
        if !denied {
            push_write_bind(&mut args, path);
        }
    }

    for path in &policy.deny_write {
        push_bind_if_exists(&mut args, "--ro-bind", path);
    }

    // Deny reads. Emitted AFTER the fs_read `--ro-bind`s so these overlays win
    // when a deny_read path sits under a broadly-bound read root (e.g. the
    // `trusted` profile binds `/`). Directories are hidden with an empty tmpfs;
    // regular files are shadowed by a read-only bind of /dev/null.
    for path in &policy.deny_read {
        hide_path(&mut args, path);
    }

    // Defense-in-depth backstop: hide the well-known sensitive home directories
    // even if they were not explicitly in deny_read, matching the Seatbelt
    // backend's SENSITIVE_HOME_PATHS behavior.
    if let Some(home) = dirs::home_dir() {
        for sensitive in crate::constants::SENSITIVE_HOME_PATHS {
            let path = format!("{}/{sensitive}", home.to_string_lossy());
            hide_path(&mut args, &path);
        }
    }

    if !policy.network {
        args.push("--unshare-net".to_string());
    }

    args.push("--unshare-pid".to_string());

    // Deliberately NOT `--as-pid-1`. That flag reads like "make bwrap PID 1",
    // but bwrap's own help spells it "Do not install a reaper process": with
    // `--unshare-pid` bwrap ALREADY becomes PID 1 and reaps orphaned
    // descendants, and `--as-pid-1` gives that up by running the sandboxed
    // command as PID 1 instead. That would both strand zombies for the
    // sandbox's lifetime and make the command immune to a plain SIGTERM, since
    // PID 1 has no default signal dispositions — so the timeout path could no
    // longer stop it.

    let supports_die = version.is_none_or(BwrapVersion::supports_die_with_parent);
    if supports_die {
        args.push("--die-with-parent".to_string());
    }

    // Surrender every Linux capability before the command runs. A sandboxed
    // process needs none of them, and dropping the bounding set closes the
    // gap where a setuid or file-capability binary reachable through a bind
    // regains privilege inside the namespace — CAP_SYS_ADMIN there would let
    // it remount the very `--ro-bind`s and tmpfs overlays that enforce
    // `deny_read`/`deny_write`. Emitted last so it stays adjacent to the
    // command bwrap execs.
    let supports_cap_drop = version.is_none_or(BwrapVersion::supports_cap_drop);
    if supports_cap_drop {
        args.extend(["--cap-drop".to_string(), "ALL".to_string()]);
    }

    args
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_TOOL_DIR: &str = "/tmp/tool";

    fn default_policy() -> SandboxPolicy {
        SandboxPolicy::default()
    }

    #[test]
    fn includes_tool_dir_ro_bind() {
        // The tool dir must exist on disk: bwrap rejects a bind whose source is
        // missing, so `build_bwrap_args` only emits binds for real paths.
        let tmp = tempfile::tempdir().unwrap();
        let tool_dir = tmp.path().join("tools/my-tool");
        std::fs::create_dir_all(&tool_dir).unwrap();
        let tool_dir_str = tool_dir.to_string_lossy().into_owned();
        let args = build_bwrap_args(&default_policy(), &tool_dir);
        assert!(args
            .windows(3)
            .any(|w| w[0] == "--ro-bind" && w[1] == tool_dir_str && w[2] == tool_dir_str));
    }

    #[test]
    fn network_isolated_by_default() {
        let args = build_bwrap_args(&default_policy(), Path::new(TEST_TOOL_DIR));
        assert!(args.contains(&"--unshare-net".to_string()));
    }

    #[test]
    fn deny_read_dir_is_tmpfs_and_file_is_dev_null() {
        // M1: bwrap must actually enforce deny_read — a directory is hidden
        // with an empty tmpfs, a regular file is shadowed by /dev/null.
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("secretdir");
        std::fs::create_dir(&dir).unwrap();
        let file = tmp.path().join("secret.db");
        std::fs::write(&file, b"secret").unwrap();

        let policy = SandboxPolicy {
            fs_read: vec![tmp.path().to_string_lossy().into_owned()],
            deny_read: vec![
                dir.to_string_lossy().into_owned(),
                file.to_string_lossy().into_owned(),
            ],
            ..SandboxPolicy::default()
        };
        let args = build_bwrap_args(&policy, Path::new(TEST_TOOL_DIR));

        let dir_s = dir.to_string_lossy().into_owned();
        assert!(
            args.windows(2).any(|w| w[0] == "--tmpfs" && w[1] == dir_s),
            "deny_read directory must be tmpfs-hidden: {args:?}"
        );
        let file_s = file.to_string_lossy().into_owned();
        assert!(
            args.windows(3)
                .any(|w| w[0] == "--ro-bind" && w[1] == "/dev/null" && w[2] == file_s),
            "deny_read file must be shadowed by /dev/null: {args:?}"
        );
        // The overlay must come AFTER the broad read bind so it wins.
        let read_bind_idx = args.iter().position(|a| a == &tmp.path().to_string_lossy());
        let tmpfs_idx = args.iter().position(|a| a == &dir_s);
        assert!(
            read_bind_idx < tmpfs_idx,
            "hide must be emitted after fs_read bind"
        );
    }

    #[test]
    fn network_allowed_when_policy_permits() {
        let policy = SandboxPolicy {
            network: true,
            ..Default::default()
        };
        let args = build_bwrap_args(&policy, Path::new(TEST_TOOL_DIR));
        assert!(!args.contains(&"--unshare-net".to_string()));
    }

    #[test]
    fn additional_fs_read_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let input = tmp.path().join("input");
        std::fs::create_dir_all(&input).unwrap();
        let input_str = input.to_string_lossy().into_owned();
        let policy = SandboxPolicy {
            fs_read: vec![input_str.clone()],
            ..Default::default()
        };
        let args = build_bwrap_args(&policy, tmp.path());
        assert!(args
            .windows(3)
            .any(|w| w[0] == "--ro-bind" && w[1] == input_str));
    }

    #[test]
    fn additional_fs_write_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let output = tmp.path().join("output");
        std::fs::create_dir_all(&output).unwrap();
        let output_str = output.to_string_lossy().into_owned();
        let policy = SandboxPolicy {
            fs_write: vec![output_str.clone()],
            ..Default::default()
        };
        let args = build_bwrap_args(&policy, tmp.path());
        assert!(args
            .windows(3)
            .any(|w| w[0] == "--bind" && w[1] == output_str));
    }

    /// bwrap exits non-zero on a missing bind source and runs nothing, so a
    /// read entry for an absent path must be dropped rather than emitted —
    /// otherwise one stale config line makes every sandboxed command fail.
    /// Real trigger: `/etc/ssl/certs` is absent on some minimal container images.
    #[test]
    fn missing_read_path_is_skipped_without_dropping_present_ones() {
        let tmp = tempfile::tempdir().unwrap();
        let present_read = tmp.path().join("present-read");
        std::fs::create_dir_all(&present_read).unwrap();
        let present_read_str = present_read.to_string_lossy().into_owned();
        let missing_read = tmp
            .path()
            .join("missing-read")
            .to_string_lossy()
            .into_owned();

        let policy = SandboxPolicy {
            fs_read: vec![missing_read.clone(), present_read_str.clone()],
            ..Default::default()
        };
        let args = build_bwrap_args(&policy, tmp.path());

        assert!(
            !args.contains(&missing_read),
            "missing fs_read path must not be bound: {args:?}"
        );
        assert!(
            args.windows(3)
                .any(|w| w[0] == "--ro-bind" && w[1] == present_read_str),
            "a missing sibling must not suppress a real fs_read bind: {args:?}"
        );
    }

    /// A write grant must never be silently dropped. `/tmp` is an internal tmpfs
    /// inside the namespace, so a skipped `--bind` leaves the tool writing into
    /// scratch that evaporates with the sandbox while still exiting 0 — success
    /// reported for output that never reached the host. Materialize instead.
    #[test]
    fn missing_write_path_is_created_and_bound_rather_than_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        let missing_write = tmp.path().join("nested").join("output");
        let missing_write_str = missing_write.to_string_lossy().into_owned();
        assert!(!missing_write.exists());

        let policy = SandboxPolicy {
            fs_write: vec![missing_write_str.clone()],
            ..Default::default()
        };
        let args = build_bwrap_args(&policy, tmp.path());

        assert!(
            missing_write.is_dir(),
            "the write grant's target must exist on the host after arg building"
        );
        assert!(
            args.windows(3)
                .any(|w| w[0] == "--bind" && w[1] == missing_write_str),
            "an absent write grant must still be bound: {args:?}"
        );
    }

    /// A missing tool dir must not fail the spawn either. Tool dirs are created
    /// lazily, so the first invocation can race ahead of the directory.
    #[test]
    fn missing_tool_dir_is_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        let absent = tmp.path().join("not-created-yet");
        let absent_str = absent.to_string_lossy().into_owned();
        let args = build_bwrap_args(&default_policy(), &absent);
        assert!(
            !args.contains(&absent_str),
            "missing tool dir must not be bound: {args:?}"
        );
    }

    #[test]
    fn always_includes_pid_isolation() {
        let args = build_bwrap_args(&default_policy(), Path::new(TEST_TOOL_DIR));
        assert!(args.contains(&"--unshare-pid".to_string()));
        assert!(args.contains(&"--die-with-parent".to_string()));
    }

    #[test]
    fn deny_write_skips_writable_bind() {
        // Both paths exist, so the absence of a writable bind is attributable to
        // deny_write rather than to the missing-path skip.
        let tmp = tempfile::tempdir().unwrap();
        let protected = tmp.path().join("borg");
        std::fs::create_dir_all(&protected).unwrap();
        let protected_str = protected.to_string_lossy().into_owned();
        let policy = SandboxPolicy {
            fs_write: vec![protected_str.clone()],
            deny_write: vec![protected_str.clone()],
            ..Default::default()
        };
        let args = build_bwrap_args(&policy, tmp.path());
        assert!(
            !args
                .windows(3)
                .any(|w| w[0] == "--bind" && w[1] == protected_str),
            "denied path must not get a writable bind: {args:?}"
        );
        assert!(
            args.windows(3)
                .any(|w| w[0] == "--ro-bind" && w[1] == protected_str),
            "denied path must still be bound read-only: {args:?}"
        );
    }

    #[test]
    fn includes_tmpfs_proc_dev() {
        let args = build_bwrap_args_versioned(&default_policy(), Path::new(TEST_TOOL_DIR), None);
        assert!(args.windows(2).any(|w| w[0] == "--tmpfs" && w[1] == "/tmp"));
        assert!(args.windows(2).any(|w| w[0] == "--proc" && w[1] == "/proc"));
        assert!(args.windows(2).any(|w| w[0] == "--dev" && w[1] == "/dev"));
    }

    #[test]
    fn parse_version_table() {
        // (input, expected) — None means parse must fail.
        let cases: &[(&str, Option<BwrapVersion>)] = &[
            (
                "bubblewrap 0.4.1",
                Some(BwrapVersion {
                    major: 0,
                    minor: 4,
                    patch: 1,
                }),
            ),
            (
                "0.6.2",
                Some(BwrapVersion {
                    major: 0,
                    minor: 6,
                    patch: 2,
                }),
            ),
            (
                "bubblewrap 0.2",
                Some(BwrapVersion {
                    major: 0,
                    minor: 2,
                    patch: 0,
                }),
            ),
            ("not a version", None),
            ("", None),
        ];
        for (input, expected) in cases {
            assert_eq!(
                parse_bwrap_version(input),
                *expected,
                "parse_bwrap_version({input:?})"
            );
        }
    }

    #[test]
    fn version_feature_cutoffs_table() {
        // Spec: feature predicates fire at the exact cutoff version.
        // --die-with-parent landed in 0.1.8; --unshare-user landed in 0.2.0;
        // --cap-drop landed in 0.3.0. A regression that shifts a cutoff
        // (e.g. wrong inequality) flips exactly one row here. Table-driven so
        // a new gate is one row, not a new test function.
        let cases: &[(u32, u32, u32, bool, bool, bool)] = &[
            // major, minor, patch, die_with_parent, unshare_user, cap_drop
            (0, 1, 5, false, false, false),
            (0, 1, 7, false, false, false),
            (0, 1, 8, true, false, false), // die-with-parent cutoff
            (0, 1, 9, true, false, false),
            (0, 2, 0, true, true, false), // unshare-user cutoff
            (0, 2, 9, true, true, false),
            (0, 3, 0, true, true, true), // cap-drop cutoff
            (0, 3, 9, true, true, true),
            (0, 4, 0, true, true, true),
            (0, 6, 2, true, true, true),
        ];
        for (major, minor, patch, want_die, want_unshare, want_cap_drop) in cases.iter().copied() {
            let v = BwrapVersion {
                major,
                minor,
                patch,
            };
            assert_eq!(
                v.supports_die_with_parent(),
                want_die,
                "die_with_parent: {major}.{minor}.{patch}"
            );
            assert_eq!(
                v.supports_unshare_user(),
                want_unshare,
                "unshare_user: {major}.{minor}.{patch}"
            );
            assert_eq!(
                v.supports_cap_drop(),
                want_cap_drop,
                "cap_drop: {major}.{minor}.{patch}"
            );
        }
    }

    /// Spec: the sandboxed command must run with no Linux capabilities. Without
    /// `--cap-drop ALL` a setuid or file-capability binary reachable through a
    /// bind can regain privilege inside the namespace, and CAP_SYS_ADMIN there
    /// is enough to remount the `--ro-bind`s and tmpfs overlays that carry
    /// `deny_read`/`deny_write`. The flag must also be the last option, so bwrap
    /// still parses it as an option rather than part of the command.
    #[test]
    fn build_args_drops_all_capabilities_before_the_command() {
        for version in [
            Some(BwrapVersion {
                major: 0,
                minor: 3,
                patch: 0,
            }),
            Some(BwrapVersion {
                major: 0,
                minor: 6,
                patch: 2,
            }),
            // Undetectable version: assume a modern bwrap rather than
            // silently running with capabilities intact.
            None,
        ] {
            let args = build_bwrap_args_versioned(
                &default_policy(),
                Path::new(TEST_TOOL_DIR),
                version.as_ref(),
            );
            let idx = args
                .iter()
                .position(|a| a == "--cap-drop")
                .unwrap_or_else(|| panic!("--cap-drop missing for {version:?}: {args:?}"));
            assert_eq!(
                args.get(idx + 1).map(String::as_str),
                Some("ALL"),
                "--cap-drop must take ALL for {version:?}"
            );
            assert_eq!(
                idx + 2,
                args.len(),
                "--cap-drop ALL must be the final option for {version:?}: {args:?}"
            );
        }
    }

    /// Spec: bwrap builds before 0.3.0 reject `--cap-drop`, so emitting it there
    /// would fail every tool call instead of hardening it. Those hosts keep the
    /// pre-existing behavior.
    #[test]
    fn build_args_omits_cap_drop_on_bwrap_without_support() {
        let version = BwrapVersion {
            major: 0,
            minor: 2,
            patch: 9,
        };
        let args =
            build_bwrap_args_versioned(&default_policy(), Path::new(TEST_TOOL_DIR), Some(&version));
        assert!(
            !args.iter().any(|a| a == "--cap-drop"),
            "0.2.9 predates --cap-drop: {args:?}"
        );
    }

    /// Spec: `--unshare-pid` must be emitted WITHOUT `--as-pid-1`. The flag's
    /// name reads backwards: bwrap documents it as "do not install a reaper
    /// process", so passing it hands PID 1 to the sandboxed command and gives
    /// up the reaper `--unshare-pid` otherwise installs. Orphaned descendants
    /// would then linger as zombies, and the command — running as PID 1 with no
    /// default signal dispositions — would ignore the SIGTERM the timeout path
    /// sends.
    #[test]
    fn build_args_unshares_pid_without_giving_up_the_reaper() {
        for version in [
            Some(BwrapVersion {
                major: 0,
                minor: 3,
                patch: 9,
            }),
            Some(BwrapVersion {
                major: 0,
                minor: 6,
                patch: 2,
            }),
            None,
        ] {
            let args = build_bwrap_args_versioned(
                &default_policy(),
                Path::new(TEST_TOOL_DIR),
                version.as_ref(),
            );
            assert!(args.contains(&"--unshare-pid".to_string()));
            assert!(
                !args.contains(&"--as-pid-1".to_string()),
                "--as-pid-1 removes the reaper rather than installing one: {args:?}"
            );
        }
    }

    /// Spec: `--dev` shadows the host device tree, so `/dev/shm` must be
    /// restored or POSIX shared memory (Python `multiprocessing`, and anything
    /// using it as scratch) stops working inside the sandbox — and it must be
    /// restored as a PRIVATE tmpfs. Binding the host's `/dev/shm` would give
    /// every sandboxed tool a world-writable, host-visible directory that no
    /// `fs_write`/`deny_write` rule covers.
    #[test]
    fn build_args_restores_dev_shm_as_a_private_tmpfs() {
        let args = build_bwrap_args(&default_policy(), Path::new(TEST_TOOL_DIR));
        let dev = args
            .iter()
            .position(|a| a == "--dev")
            .expect("a minimal /dev is always mounted");
        let shm = args
            .iter()
            .enumerate()
            .position(|(i, a)| a == "--tmpfs" && args.get(i + 1).is_some_and(|p| p == "/dev/shm"))
            .unwrap_or_else(|| panic!("/dev/shm must be restored: {args:?}"));
        assert!(
            shm > dev,
            "the shm tmpfs must come after the --dev overlay or it is shadowed: {args:?}"
        );
        assert!(
            !args
                .windows(2)
                .any(|w| w[1] == "/dev/shm" && w[0] != "--tmpfs"),
            "/dev/shm must never be bound from the host: {args:?}"
        );
    }

    #[test]
    fn build_args_gates_die_with_parent_by_version() {
        // Spec: --die-with-parent only emitted when the detected bwrap
        // supports it. Unknown version (None) is treated as modern.
        // Other flags (--unshare-pid) must be present regardless so we
        // don't accidentally drop the rest of the namespace isolation
        // when the version gate trips.
        let old_version = BwrapVersion {
            major: 0,
            minor: 1,
            patch: 5,
        };
        let new_version = BwrapVersion {
            major: 0,
            minor: 4,
            patch: 1,
        };
        let cases: &[(Option<&BwrapVersion>, bool)] = &[
            (Some(&old_version), false),
            (Some(&new_version), true),
            (None, true), // unknown → assume modern
        ];
        for (ver, want_die_flag) in cases.iter().copied() {
            let args = build_bwrap_args_versioned(&default_policy(), Path::new(TEST_TOOL_DIR), ver);
            assert_eq!(
                args.contains(&"--die-with-parent".to_string()),
                want_die_flag,
                "die-with-parent presence for {ver:?}"
            );
            assert!(
                args.contains(&"--unshare-pid".to_string()),
                "--unshare-pid must always be present (ver={ver:?})"
            );
        }
    }
}
