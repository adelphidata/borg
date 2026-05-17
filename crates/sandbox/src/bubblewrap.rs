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

/// Build bwrap args with an explicit version for testability.
pub fn build_bwrap_args_versioned(
    policy: &SandboxPolicy,
    tool_dir: &Path,
    version: Option<&BwrapVersion>,
) -> Vec<String> {
    let mut args = Vec::new();

    push_bind(&mut args, "--ro-bind", &tool_dir.to_string_lossy());

    for path in &["/usr", "/lib", "/lib64", "/bin", "/sbin", "/etc"] {
        if Path::new(path).exists() {
            push_bind(&mut args, "--ro-bind", path);
        }
    }

    args.extend(["--tmpfs".to_string(), "/tmp".to_string()]);
    args.extend(["--proc".to_string(), "/proc".to_string()]);
    args.extend(["--dev".to_string(), "/dev".to_string()]);

    for path in &policy.fs_read {
        push_bind(&mut args, "--ro-bind", path);
    }

    for path in &policy.fs_write {
        let denied = policy.deny_write.iter().any(|d| path.starts_with(d));
        if !denied {
            push_bind(&mut args, "--bind", path);
        }
    }

    for path in &policy.deny_write {
        if Path::new(path).exists() {
            push_bind(&mut args, "--ro-bind", path);
        }
    }

    // Network isolation (unshare network namespace unless allowed)
    if !policy.network {
        args.push("--unshare-net".to_string());
    }

    // Unshare other namespaces for isolation
    args.push("--unshare-pid".to_string());

    // --die-with-parent requires bwrap >= 0.1.8
    let supports_die = version.is_none_or(BwrapVersion::supports_die_with_parent);
    if supports_die {
        args.push("--die-with-parent".to_string());
    }

    args
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_policy() -> SandboxPolicy {
        SandboxPolicy::default()
    }

    #[test]
    fn includes_tool_dir_ro_bind() {
        let args = build_bwrap_args(
            &default_policy(),
            Path::new("/home/user/.borg/tools/my-tool"),
        );
        assert!(args.windows(3).any(|w| w[0] == "--ro-bind"
            && w[1] == "/home/user/.borg/tools/my-tool"
            && w[2] == "/home/user/.borg/tools/my-tool"));
    }

    #[test]
    fn network_isolated_by_default() {
        let args = build_bwrap_args(&default_policy(), Path::new("/tmp/tool"));
        assert!(args.contains(&"--unshare-net".to_string()));
    }

    #[test]
    fn network_allowed_when_policy_permits() {
        let policy = SandboxPolicy {
            network: true,
            ..Default::default()
        };
        let args = build_bwrap_args(&policy, Path::new("/tmp/tool"));
        assert!(!args.contains(&"--unshare-net".to_string()));
    }

    #[test]
    fn additional_fs_read_paths() {
        let policy = SandboxPolicy {
            fs_read: vec!["/data/input".to_string()],
            ..Default::default()
        };
        let args = build_bwrap_args(&policy, Path::new("/tmp/tool"));
        assert!(args
            .windows(3)
            .any(|w| w[0] == "--ro-bind" && w[1] == "/data/input"));
    }

    #[test]
    fn additional_fs_write_paths() {
        let policy = SandboxPolicy {
            fs_write: vec!["/data/output".to_string()],
            ..Default::default()
        };
        let args = build_bwrap_args(&policy, Path::new("/tmp/tool"));
        assert!(args
            .windows(3)
            .any(|w| w[0] == "--bind" && w[1] == "/data/output"));
    }

    #[test]
    fn always_includes_pid_isolation() {
        let args = build_bwrap_args(&default_policy(), Path::new("/tmp/tool"));
        assert!(args.contains(&"--unshare-pid".to_string()));
        assert!(args.contains(&"--die-with-parent".to_string()));
    }

    #[test]
    fn deny_write_skips_writable_bind() {
        let policy = SandboxPolicy {
            fs_write: vec!["/home/user/.borg".to_string()],
            deny_write: vec!["/home/user/.borg".to_string()],
            ..Default::default()
        };
        let args = build_bwrap_args(&policy, Path::new("/tmp/tool"));
        // Should NOT have a writable --bind for the denied path
        let has_writable = args
            .windows(3)
            .any(|w| w[0] == "--bind" && w[1] == "/home/user/.borg");
        assert!(!has_writable);
    }

    #[test]
    fn includes_tmpfs_proc_dev() {
        let args = build_bwrap_args_versioned(&default_policy(), Path::new("/tmp/tool"), None);
        assert!(args.windows(2).any(|w| w[0] == "--tmpfs" && w[1] == "/tmp"));
        assert!(args.windows(2).any(|w| w[0] == "--proc" && w[1] == "/proc"));
        assert!(args.windows(2).any(|w| w[0] == "--dev" && w[1] == "/dev"));
    }

    // --- Version detection tests ---

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
        // --die-with-parent landed in 0.1.8; --unshare-user landed in 0.2.0.
        // A regression that shifts a cutoff (e.g. wrong inequality) flips
        // exactly one row here. Table-driven so a new gate is one row, not
        // a new test function.
        let cases: &[(u32, u32, u32, bool, bool)] = &[
            // major, minor, patch, supports_die_with_parent, supports_unshare_user
            (0, 1, 5, false, false),
            (0, 1, 7, false, false),
            (0, 1, 8, true, false), // die-with-parent cutoff
            (0, 1, 9, true, false),
            (0, 2, 0, true, true), // unshare-user cutoff
            (0, 4, 0, true, true),
        ];
        for (major, minor, patch, want_die, want_unshare) in cases.iter().copied() {
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
        }
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
            let args = build_bwrap_args_versioned(&default_policy(), Path::new("/tmp/tool"), ver);
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
