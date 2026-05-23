use serde::{Deserialize, Serialize};

/// Declarative security policy for sandboxed execution.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SandboxPolicy {
    /// Whether network access is allowed.
    pub network: bool,
    /// Paths the sandboxed process may read.
    pub fs_read: Vec<String>,
    /// Paths the sandboxed process may write.
    pub fs_write: Vec<String>,
    /// Glob patterns to deny read access to (e.g. `["~/.borg/**"]`).
    /// Evaluated after fs_read allows; deny takes precedence.
    #[serde(default)]
    pub deny_read: Vec<String>,
    /// Paths to deny write access to, even if not in fs_write.
    /// Always includes `~/.borg/` to protect agent config from tool writes.
    #[serde(default)]
    pub deny_write: Vec<String>,
    /// Domain allowlist for network-enabled tools. When non-empty and `network` is true,
    /// traffic is routed through a local proxy that only permits connections to these domains.
    /// Supports exact matches and wildcard suffixes (e.g., ".github.com").
    #[serde(default)]
    pub allowed_domains: Vec<String>,
}

/// A command with its arguments, ready for execution (possibly sandbox-wrapped).
///
/// The optional `_resources` field carries OS handles or other RAII
/// guards whose lifetime must outlive `Command::spawn()`. The current
/// user is the Windows trampoline (Phase 1): `wrap_windows` allocates
/// an anonymous pipe, writes the framed `SpawnRequest` into it, marks
/// the read-end inheritable, embeds that handle's value on the
/// trampoline argv, and stashes the parent's copy here. The Drop
/// impl on the contained guard closes the parent's handle table entry
/// once the caller is done spawning. On non-Windows platforms the
/// field is always `None`.
///
/// Type-erasing via `Arc<dyn Any + Send + Sync>` keeps `SandboxCommand`
/// platform-neutral (no Win32 types leak into the cross-platform API)
/// and `Clone`-able (Arc clone shares the same drop guard, so the
/// handle stays open until the last clone drops).
#[derive(Default)]
pub struct SandboxCommand {
    /// The program to execute.
    pub program: String,
    /// Command-line arguments.
    pub args: Vec<String>,
    /// OS-side resources held for the lifetime of this command. See
    /// the type-level docs for the Phase 1 pipe-IPC use case.
    pub(crate) _resources: Option<std::sync::Arc<dyn std::any::Any + Send + Sync>>,
}

impl std::fmt::Debug for SandboxCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SandboxCommand")
            .field("program", &self.program)
            .field("args", &self.args)
            .finish()
    }
}

impl Clone for SandboxCommand {
    fn clone(&self) -> Self {
        Self {
            program: self.program.clone(),
            args: self.args.clone(),
            _resources: self._resources.clone(),
        }
    }
}

/// Check if a path matches any blocked path pattern.
/// Blocked patterns are matched against individual path components (split by `/`)
/// to avoid false positives from substring matching (e.g., `.aws_backup` matching `.aws`).
fn is_path_blocked(path: &str, blocked_paths: &[String]) -> bool {
    // Extract normal path components (skips `.`, `..`, root, and prefix components)
    let normalized = std::path::Path::new(path);
    let components: Vec<&str> = normalized
        .components()
        .filter_map(|c| match c {
            std::path::Component::Normal(s) => s.to_str(),
            _ => None,
        })
        .collect();

    for blocked in blocked_paths {
        // Check if any path component matches or starts with the blocked pattern
        for component in &components {
            if *component == blocked.as_str() || component.starts_with(&format!("{blocked}/")) {
                return true;
            }
        }
    }
    false
}

/// Filter out paths that match the security blocklist.
fn filter_blocked(paths: &[String], blocked_paths: &[String]) -> Vec<String> {
    paths
        .iter()
        .filter(|p| !is_path_blocked(p, blocked_paths))
        .cloned()
        .collect()
}

/// Construct a `SandboxCommand` that fails closed when Windows sandbox
/// setup cannot be completed. Used by `wrap_windows()` on
/// `current_exe()` resolution failure. Not `cfg(windows)`-gated because
/// `wrap_command` dispatches via the runtime `cfg!()` macro (not
/// `#[cfg]`), so the helper must compile on every target even though
/// it's only ever called when the running OS is Windows.
#[cfg(target_os = "windows")]
pub(crate) fn fail_closed_windows_public(context: &str, detail: &str) -> SandboxCommand {
    fail_closed_windows(context, detail)
}

fn fail_closed_windows(context: &str, detail: &str) -> SandboxCommand {
    tracing::error!("Windows sandbox refused to run unsandboxed: {context}: {detail}");
    SandboxCommand {
        program: "cmd.exe".to_string(),
        args: vec![
            "/c".to_string(),
            format!("echo Borg sandbox setup failed: {context}: {detail} 1>&2 && exit 1"),
        ],
        _resources: None,
    }
}

/// Expand tilde in all paths, returning Strings to preserve the
/// `Vec<String>` shape used by `SandboxPolicy::fs_read`/`fs_write`.
fn expand_tilde_paths(paths: &[String]) -> Vec<String> {
    paths
        .iter()
        .map(|p| crate::paths::expand_tilde(p).to_string_lossy().into_owned())
        .collect()
}

impl SandboxPolicy {
    /// Filter blocked paths from fs_read/fs_write, consuming self.
    pub fn with_blocked_paths_filtered(mut self, blocked_paths: &[String]) -> Self {
        self.fs_read = filter_blocked(&self.fs_read, blocked_paths);
        self.fs_write = filter_blocked(&self.fs_write, blocked_paths);
        self
    }

    /// Expand `~` to the user's home directory in all paths, consuming self.
    pub fn with_tildes_expanded(mut self) -> Self {
        self.fs_read = expand_tilde_paths(&self.fs_read);
        self.fs_write = expand_tilde_paths(&self.fs_write);
        self.deny_read = expand_tilde_paths(&self.deny_read);
        self.deny_write = expand_tilde_paths(&self.deny_write);
        self
    }

    /// Apply smart defaults based on policy properties, consuming self.
    /// For example, `network == true` implies TLS certificate paths should be readable.
    /// Idempotent: does not add paths that are already present.
    pub fn with_defaults_applied(mut self) -> Self {
        if self.network {
            for tls_path in &["/etc/ssl", "/etc/ssl/certs"] {
                let s = tls_path.to_string();
                if !self.fs_read.contains(&s) {
                    self.fs_read.push(s);
                }
            }
        }
        self
    }

    /// Apply the standard configuration chain: protect ~/.borg, expand tildes,
    /// apply network defaults, and filter blocked paths.
    pub fn configured(self, blocked_paths: &[String]) -> Self {
        self.with_borg_dir_protected()
            .with_tildes_expanded()
            .with_defaults_applied()
            .with_blocked_paths_filtered(blocked_paths)
    }

    /// Add `~/.borg/` to deny_write to protect agent config, consuming self.
    pub fn with_borg_dir_protected(mut self) -> Self {
        let borg_dir = dirs::home_dir()
            .map(|h| format!("{}/.borg", h.display()))
            .unwrap_or_else(|| {
                tracing::warn!("Could not determine home directory; using literal '~/.borg' which may not resolve");
                "~/.borg".to_string()
            });

        if !self.deny_write.iter().any(|p| p == &borg_dir) {
            self.deny_write.push(borg_dir);
        }
        self
    }

    /// Wrap a command with platform-specific sandbox isolation.
    pub fn wrap_command(
        &self,
        program: &str,
        args: &[String],
        tool_dir: &std::path::Path,
    ) -> SandboxCommand {
        if cfg!(target_os = "macos") {
            self.wrap_seatbelt(program, args, tool_dir)
        } else if cfg!(target_os = "linux") {
            self.wrap_bubblewrap(program, args, tool_dir)
        } else if cfg!(target_os = "windows") {
            self.wrap_windows(program, args, tool_dir)
        } else {
            tracing::warn!("Sandboxing not available on this platform; running unsandboxed");
            SandboxCommand {
                program: program.to_string(),
                args: args.to_vec(),
                _resources: None,
            }
        }
    }

    /// Build a Windows-sandboxed command by re-execing the running `borg`
    /// binary with the `__windows-sandbox-spawn` hidden subcommand. The
    /// trampoline subcommand applies the restricted-token + DACL ACEs
    /// (Tier 1) or routes through the elevated runner (Tier 2) before
    /// spawning the real child.
    ///
    /// **Fails closed** on every error path. Both `current_exe()` failure
    /// (corrupt install, deleted-exe race, symlink games) and trampoline-
    /// argv construction failure return a `cmd.exe /c exit 1` stub rather
    /// than running unsandboxed. CLAUDE.md `Critical Invariants — Error
    /// Handling` is explicit: "if we report success to the user, it MUST
    /// actually succeed" — silent un-sandboxing on a path-resolution
    /// failure would let a local attacker who can perturb `current_exe()`
    /// resolution win a free sandbox bypass.
    fn wrap_windows(
        &self,
        program: &str,
        args: &[String],
        tool_dir: &std::path::Path,
    ) -> SandboxCommand {
        let self_exe = match std::env::current_exe() {
            Ok(p) => p,
            Err(e) => return fail_closed_windows("current_exe() failed", &e.to_string()),
        };
        crate::windows::build_trampoline_command(&self_exe, self, program, args, tool_dir)
    }

    #[cfg(target_os = "macos")]
    fn wrap_seatbelt(
        &self,
        program: &str,
        args: &[String],
        tool_dir: &std::path::Path,
    ) -> SandboxCommand {
        use crate::seatbelt::generate_profile;
        let profile = match generate_profile(self, tool_dir, Some(program)) {
            Ok(p) => p,
            Err(e) => {
                tracing::error!(
                    "Failed to generate sandbox profile: {e}. Refusing to run unsandboxed."
                );
                // Fail closed: return a command that immediately exits with error
                return SandboxCommand {
                    program: "/bin/sh".to_string(),
                    args: vec![
                        "-c".to_string(),
                        format!("echo 'Sandbox profile generation failed: {e}' >&2; exit 1"),
                    ],
                    _resources: None,
                };
            }
        };

        let mut sandbox_args = vec!["-p".to_string(), profile, program.to_string()];
        sandbox_args.extend(args.iter().cloned());

        SandboxCommand {
            program: "sandbox-exec".to_string(),
            args: sandbox_args,
            _resources: None,
        }
    }

    #[cfg(not(target_os = "macos"))]
    fn wrap_seatbelt(
        &self,
        program: &str,
        args: &[String],
        _tool_dir: &std::path::Path,
    ) -> SandboxCommand {
        SandboxCommand {
            program: program.to_string(),
            args: args.to_vec(),
            _resources: None,
        }
    }

    #[cfg(target_os = "linux")]
    fn wrap_bubblewrap(
        &self,
        program: &str,
        args: &[String],
        tool_dir: &std::path::Path,
    ) -> SandboxCommand {
        // Fall back to unsandboxed if bwrap is not installed
        if which::which("bwrap").is_err() {
            tracing::warn!("bwrap not found; running without sandbox");
            return SandboxCommand {
                program: program.to_string(),
                args: args.to_vec(),
                _resources: None,
            };
        }
        use crate::bubblewrap::build_bwrap_args;
        let mut bwrap_args = build_bwrap_args(self, tool_dir);
        bwrap_args.push(program.to_string());
        bwrap_args.extend(args.iter().cloned());

        SandboxCommand {
            program: "bwrap".to_string(),
            args: bwrap_args,
            _resources: None,
        }
    }

    #[cfg(not(target_os = "linux"))]
    fn wrap_bubblewrap(
        &self,
        program: &str,
        args: &[String],
        _tool_dir: &std::path::Path,
    ) -> SandboxCommand {
        SandboxCommand {
            program: program.to_string(),
            args: args.to_vec(),
            _resources: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // `Path` is only needed by platform-specific tests (linux/windows/other);
    // gate the import so the macOS-only test build doesn't trip
    // `-D unused-imports`.
    #[cfg(not(target_os = "macos"))]
    use std::path::Path;

    #[cfg(not(target_os = "macos"))]
    const TEST_TOOL_DIR: &str = "/tmp/tool";

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_wraps_with_bwrap() {
        let policy = SandboxPolicy::default();
        let args = vec!["script.py".to_string()];
        let cmd = policy.wrap_command("python3", &args, Path::new(TEST_TOOL_DIR));
        if which::which("bwrap").is_ok() {
            assert_eq!(cmd.program, "bwrap");
        } else {
            assert_eq!(cmd.program, "python3");
        }
        assert!(cmd.args.contains(&"script.py".to_string()));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_preserves_original_args_order() {
        let policy = SandboxPolicy::default();
        let args = vec!["arg1".to_string(), "arg2".to_string()];
        let cmd = policy.wrap_command("node", &args, Path::new(TEST_TOOL_DIR));
        if which::which("bwrap").is_err() {
            assert_eq!(cmd.program, "node");
            assert_eq!(cmd.args, vec!["arg1", "arg2"]);
            return;
        }
        let node_pos = cmd.args.iter().position(|a| a == "node").unwrap();
        let arg1_pos = cmd.args.iter().position(|a| a == "arg1").unwrap();
        let arg2_pos = cmd.args.iter().position(|a| a == "arg2").unwrap();
        assert!(node_pos < arg1_pos);
        assert!(arg1_pos < arg2_pos);
    }

    #[test]
    fn filter_blocked_paths_removes_sensitive() {
        let blocked = vec![".ssh".into(), ".aws".into(), "credentials".into()];
        let policy = SandboxPolicy {
            fs_read: vec![
                "/home/user/.ssh".into(),
                "/data/public".into(),
                "/home/user/.aws/config".into(),
            ],
            fs_write: vec!["/tmp/output".into(), "/home/user/credentials".into()],
            ..Default::default()
        };
        let filtered = policy.with_blocked_paths_filtered(&blocked);
        assert_eq!(filtered.fs_read, vec!["/data/public".to_string()]);
        assert_eq!(filtered.fs_write, vec!["/tmp/output".to_string()]);
    }

    #[test]
    fn filter_blocked_paths_empty_blocklist() {
        let policy = SandboxPolicy {
            network: true,
            fs_read: vec!["/home/user/.ssh".into()],
            ..Default::default()
        };
        let expected_fs_read = policy.fs_read.clone();
        let filtered = policy.with_blocked_paths_filtered(&[]);
        assert_eq!(filtered.fs_read, expected_fs_read);
        assert!(filtered.network);
    }

    #[test]
    fn is_path_blocked_matches() {
        let blocked = vec![".ssh".into(), ".env".into()];
        assert!(is_path_blocked("/home/user/.ssh/id_rsa", &blocked));
        assert!(is_path_blocked("/project/.env", &blocked));
        assert!(!is_path_blocked("/home/user/code", &blocked));
    }

    #[test]
    fn is_path_blocked_no_false_positive_on_prefix() {
        let blocked = vec![".aws".into()];
        // `.aws_backup` is a different component and should NOT be blocked
        assert!(!is_path_blocked("/home/user/.aws_backup/data", &blocked));
        // `.aws` itself should still be blocked
        assert!(is_path_blocked("/home/user/.aws/config", &blocked));
    }

    #[test]
    fn with_borg_dir_protected_adds_deny_write() {
        let policy = SandboxPolicy::default();
        let protected = policy.with_borg_dir_protected();
        assert!(!protected.deny_write.is_empty());
        assert!(protected.deny_write[0].ends_with("/.borg"));
    }

    #[test]
    fn with_borg_dir_protected_is_idempotent() {
        let policy = SandboxPolicy::default();
        let protected = policy.with_borg_dir_protected().with_borg_dir_protected();
        // Should not duplicate the entry
        let borg_count = protected
            .deny_write
            .iter()
            .filter(|p| p.ends_with("/.borg"))
            .count();
        assert_eq!(borg_count, 1);
    }

    #[test]
    fn defaults_applied_adds_tls_for_network() {
        let policy = SandboxPolicy {
            network: true,
            fs_read: vec!["/data".into()],
            ..Default::default()
        };
        let applied = policy.with_defaults_applied();
        assert!(
            applied.fs_read.contains(&"/etc/ssl".to_string()),
            "network=true should add /etc/ssl"
        );
        assert!(
            applied.fs_read.contains(&"/etc/ssl/certs".to_string()),
            "network=true should add /etc/ssl/certs"
        );
        assert!(
            applied.fs_read.contains(&"/data".to_string()),
            "original paths preserved"
        );
    }

    #[test]
    fn defaults_applied_no_tls_without_network() {
        let policy = SandboxPolicy {
            network: false,
            ..Default::default()
        };
        let applied = policy.with_defaults_applied();
        assert!(
            !applied.fs_read.contains(&"/etc/ssl".to_string()),
            "network=false should not add TLS paths"
        );
    }

    #[test]
    fn defaults_applied_is_idempotent() {
        // Idempotence guard: applying defaults twice — whether the seed
        // already contains /etc/ssl or not — must never duplicate entries.
        // Without this, repeated config reloads would grow fs_read unboundedly.
        let seeded = SandboxPolicy {
            network: true,
            fs_read: vec!["/etc/ssl".into()],
            ..Default::default()
        };
        let seeded_once = seeded.with_defaults_applied();
        let ssl_count = seeded_once
            .fs_read
            .iter()
            .filter(|p| *p == "/etc/ssl")
            .count();
        assert_eq!(ssl_count, 1, "should not duplicate existing /etc/ssl");

        let empty = SandboxPolicy {
            network: true,
            ..Default::default()
        };
        let once = empty.with_defaults_applied();
        let twice = once.clone().with_defaults_applied();
        assert_eq!(
            once.fs_read.len(),
            twice.fs_read.len(),
            "applying defaults twice should not add duplicates"
        );
    }

    #[test]
    fn defaults_applied_preserves_other_fields() {
        let policy = SandboxPolicy {
            network: true,
            fs_read: vec!["/custom".into()],
            fs_write: vec!["/tmp/out".into()],
            deny_read: vec!["/secret".into()],
            deny_write: vec!["/protected".into()],
            ..Default::default()
        };
        let applied = policy.with_defaults_applied();
        assert!(applied.network);
        assert_eq!(applied.fs_write, vec!["/tmp/out".to_string()]);
        assert_eq!(applied.deny_read, vec!["/secret".to_string()]);
        assert_eq!(applied.deny_write, vec!["/protected".to_string()]);
    }

    #[test]
    fn defaults_then_blocked_filter_overrides_defaults() {
        let policy = SandboxPolicy {
            network: true,
            ..Default::default()
        };
        // Hypothetically block /etc/ssl
        let blocked = vec!["ssl".to_string()];
        let result = policy
            .with_defaults_applied()
            .with_blocked_paths_filtered(&blocked);
        assert!(
            !result.fs_read.contains(&"/etc/ssl".to_string()),
            "blocked paths filter should override defaults"
        );
    }

    #[test]
    fn with_tildes_expanded_applies_to_all_paths() {
        let home = dirs::home_dir().expect("home dir must exist for test");
        let home_str = home.to_string_lossy();

        let policy = SandboxPolicy {
            fs_read: vec!["~/Library/Messages".into(), "/etc/ssl".into()],
            fs_write: vec!["~/.borg/channels/imessage".into()],
            ..Default::default()
        };
        let expanded = policy.with_tildes_expanded();

        assert_eq!(
            expanded.fs_read,
            vec![
                format!("{home_str}/Library/Messages"),
                "/etc/ssl".to_string(),
            ]
        );
        assert_eq!(
            expanded.fs_write,
            vec![format!("{home_str}/.borg/channels/imessage"),]
        );
        assert!(!expanded.network);
    }

    #[test]
    fn deeply_nested_blocked_path() {
        let blocked = vec![".ssh".into()];
        assert!(is_path_blocked("/home/user/.ssh/keys/id_rsa", &blocked));
        assert!(is_path_blocked("/home/user/.ssh", &blocked));
        assert!(!is_path_blocked("/home/user/ssh_config", &blocked));
    }

    #[test]
    fn special_chars_in_path_not_falsely_blocked() {
        let blocked = vec![".ssh".into(), ".env".into()];
        assert!(!is_path_blocked("/home/user/my project/data", &blocked));
        assert!(!is_path_blocked("/home/user/données/fichier", &blocked));
        assert!(!is_path_blocked("/home/user/.env_backup", &blocked));
        assert!(is_path_blocked("/home/user/.ssh/id_rsa", &blocked));
        assert!(is_path_blocked("/home/user/.env", &blocked));
    }

    #[test]
    fn configured_chain_integration() {
        let home = dirs::home_dir().expect("home dir must exist for test");
        let home_str = home.to_string_lossy();

        let policy = SandboxPolicy {
            network: true,
            fs_read: vec!["~/data".into(), "/home/user/.ssh/keys".into()],
            fs_write: vec!["~/output".into()],
            deny_read: vec!["~/secrets".into()],
            deny_write: vec![],
            allowed_domains: vec![],
        };

        let blocked = vec![".ssh".into()];
        let result = policy.configured(&blocked);

        assert!(
            result.deny_write.iter().any(|p| p.ends_with("/.borg")),
            "borg dir should be protected"
        );
        assert!(
            result.fs_read.iter().any(|p| p.starts_with(&*home_str)),
            "tildes should be expanded"
        );
        assert!(
            result.fs_read.contains(&"/etc/ssl".to_string()),
            "TLS paths should be added for network"
        );
        assert!(
            !result.fs_read.iter().any(|p| p.contains(".ssh")),
            ".ssh paths should be filtered"
        );
        // deny_read tildes should be expanded
        assert!(
            result.deny_read.iter().any(|p| p.starts_with(&*home_str)),
            "deny_read tildes should be expanded"
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_wraps_via_trampoline_subcommand() {
        // Spec: on Windows wrap_command must re-exec the running borg
        // binary with the hidden __windows-sandbox-spawn subcommand,
        // *not* return the raw program. A regression to passthrough
        // would silently disable sandboxing for every Windows user.
        let policy = SandboxPolicy::default();
        let args = vec!["script.py".to_string()];
        let cmd = policy.wrap_command("python3", &args, Path::new(r"C:\tools\test"));

        let current_exe =
            std::env::current_exe().expect("test env should have a resolvable current_exe");
        assert_eq!(std::path::PathBuf::from(&cmd.program), current_exe);
        assert_eq!(cmd.args[0], crate::windows::SPAWN_SUBCOMMAND);

        // The wrapped program + args must round-trip through the
        // trampoline parser — otherwise the trampoline subcommand would
        // spawn the wrong thing or fail to spawn at all.
        let parsed = crate::windows::parse_trampoline_args(&cmd.args[1..])
            .expect("trampoline args must parse");
        assert_eq!(parsed.program, "python3");
        assert_eq!(parsed.args, vec!["script.py".to_string()]);
        assert_eq!(parsed.tool_dir, Path::new(r"C:\tools\test"));
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    #[test]
    fn unsupported_platform_passes_through() {
        let policy = SandboxPolicy {
            network: true,
            fs_read: vec!["/data".into()],
            ..Default::default()
        };
        let args = vec!["arg1".to_string()];
        let cmd = policy.wrap_command("node", &args, Path::new(TEST_TOOL_DIR));
        // Should pass through without modification
        assert_eq!(cmd.program, "node");
        assert_eq!(cmd.args, vec!["arg1"]);
    }
}
