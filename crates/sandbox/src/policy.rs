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
    /// Paths to deny read access to (e.g. `["~/.borg/secrets"]`).
    /// Evaluated after fs_read allows; deny takes precedence.
    ///
    /// These are **paths, not globs**. Both backends deny a path and everything
    /// beneath it, so a trailing `/**` or `/*` is redundant — it is accepted and
    /// normalized away by [`SandboxPolicy::with_denies_normalized`]. Any other
    /// glob metacharacter cannot be expressed as a sandbox rule and is a hard
    /// error there, because silently dropping a deny rule fails *open*.
    #[serde(default)]
    pub deny_read: Vec<String>,
    /// Paths to deny write access to, even if not in fs_write.
    /// Always includes `~/.borg/` to protect agent config from tool writes.
    ///
    /// Same path-not-glob rule as [`SandboxPolicy::deny_read`].
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
/// guards whose lifetime must outlive `Command::spawn()` — the Windows
/// trampoline stashes its parent-side pipe handle here so the Drop
/// impl closes it after the caller has spawned the child. Type-erased
/// via `Arc<dyn Any>` so no Win32 types leak into the cross-platform
/// API and clones share the same drop guard.
#[derive(Default)]
pub struct SandboxCommand {
    /// The program to execute.
    pub program: String,
    /// Command-line arguments.
    pub args: Vec<String>,
    /// OS-side resources held for the lifetime of this command. See
    /// the type-level docs for the Windows trampoline pipe-IPC use case.
    pub(crate) _resources: Option<std::sync::Arc<dyn std::any::Any + Send + Sync>>,
}

impl SandboxCommand {
    /// Run `program` with no sandbox wrapper at all. Every unsandboxed escape
    /// hatch funnels through here so a security audit has a single grep-point
    /// for "where can borg run a tool outside the sandbox".
    pub(crate) fn passthrough(program: &str, args: &[String]) -> Self {
        Self {
            program: program.to_string(),
            args: args.to_vec(),
            _resources: None,
        }
    }
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
    let normalized = std::path::Path::new(path);
    let components: Vec<&str> = normalized
        .components()
        .filter_map(|c| match c {
            std::path::Component::Normal(s) => s.to_str(),
            _ => None,
        })
        .collect();

    for blocked in blocked_paths {
        let fold = blocked_entry_folds_case(blocked);
        for component in &components {
            if component_matches_family(component, blocked, fold) {
                return true;
            }
        }
    }
    false
}

/// `s` as a case-insensitive volume compares names: Unicode full case folding.
///
/// APFS (the macOS default) folds this way, so `.ßh` opens `.ssh` and `.conﬁg`
/// (with the `ﬁ` ligature) opens `.config`. [`char::to_lowercase`] covers the
/// simple folds, the Kelvin sign included. The table adds the full folds that
/// expand to ASCII, the only ones that can alias a blocklist entry or a
/// control-plane name, which are all ASCII.
pub fn fold_case(s: &str) -> std::borrow::Cow<'_, str> {
    if s.bytes().all(|b| b.is_ascii() && !b.is_ascii_uppercase()) {
        return std::borrow::Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            'ß' | 'ẞ' => out.push_str("ss"),
            'ſ' => out.push('s'),
            'ﬀ' => out.push_str("ff"),
            'ﬁ' => out.push_str("fi"),
            'ﬂ' => out.push_str("fl"),
            'ﬃ' => out.push_str("ffi"),
            'ﬄ' => out.push_str("ffl"),
            'ﬅ' | 'ﬆ' => out.push_str("st"),
            c => out.extend(c.to_lowercase()),
        }
    }
    std::borrow::Cow::Owned(out)
}

/// Whether a blocklist entry matches every case spelling of itself.
///
/// Only a dotted entry does. A dotted name is a fixed location that tools look
/// up by that name (`~/.aws/config`, `.envrc`). On a case-insensitive volume a
/// write that creates `~/.AWS/config` creates the real file, and a name that
/// does not exist yet keeps the caller's spelling through canonicalization. An
/// undotted entry (`credentials`, `private_key`) is a common word that also
/// names ordinary source files (`Credentials.java`). The secrets it guards
/// already exist, and canonicalization returns an existing name in its on-disk
/// case, so an exact match still catches each alias of a real one.
pub fn blocked_entry_folds_case(entry: &str) -> bool {
    entry.starts_with('.')
}

/// Component-level match with dotted-family coverage: `.env` also matches
/// `.env.local`, `private_key` also matches `private_key.pem`. With `fold`
/// (see [`blocked_entry_folds_case`]) both sides go through [`fold_case`].
///
/// Deliberately a suffix-after-a-dot rule, not a substring one, so
/// `.environment` and `.sshrc` are not over-blocked.
///
/// `cortex::tool_handlers::filesystem::blocked` calls this too, so a policy
/// entry the tool-side blocklist rejects is never handed to the sandbox as an
/// allowed bind.
pub fn component_matches_family(component: &str, entry: &str, fold: bool) -> bool {
    let (component, entry) = if fold {
        (fold_case(component), fold_case(entry))
    } else {
        (component.into(), entry.into())
    };
    component == entry
        || component
            .strip_prefix(&*entry)
            .is_some_and(|rest| rest.starts_with('.') && rest.len() > 1)
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

/// Refuse to run at all, on whichever shell this platform has.
///
/// The counterpart to [`fail_closed_windows`] for failures that are not
/// Windows-specific — currently an unenforceable deny rule, which every backend
/// must reject rather than run without.
fn fail_closed(context: &str, detail: &str) -> SandboxCommand {
    if cfg!(target_os = "windows") {
        return fail_closed_windows(context, detail);
    }
    tracing::error!("Sandbox refused to run unsandboxed: {context}: {detail}");
    SandboxCommand {
        program: "/bin/sh".to_string(),
        // The message rides in as `$0`, never interpolated into the script.
        // `detail` quotes a config-supplied path, and a path holding a single
        // quote would otherwise close the literal and run as a command — in
        // the one code path whose whole job is to refuse to run anything.
        args: vec![
            "-c".to_string(),
            "printf '%s\n' \"$0\" >&2; exit 1".to_string(),
            format!("Borg sandbox setup failed: {context}: {detail}"),
        ],
        _resources: None,
    }
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

/// Glob metacharacters no sandbox backend can express as a deny rule.
///
/// `**`/`*` in *trailing* position is handled separately by
/// [`normalize_deny_entry`] — these are the ones with no path equivalent.
const GLOB_METACHARACTERS: &[char] = &['*', '?', '['];

/// Reduce a deny entry to the literal path both backends can actually enforce.
///
/// A trailing `/**` or `/*` is dropped: Seatbelt's `(subpath …)` and bwrap's
/// `--tmpfs`/`--ro-bind` overlays already cover a directory and everything under
/// it, so `~/.borg/**` and `~/.borg` deny exactly the same set.
///
/// Anything else containing a glob metacharacter is an error. It cannot be
/// turned into a rule, and the previous behaviour — bwrap's `hide_path` silently
/// returning on a `symlink_metadata` error — meant such an entry enforced
/// *nothing* while still reading like a deny in the config.
fn normalize_deny_entry(entry: &str) -> Result<String, String> {
    let trimmed = entry
        .strip_suffix("/**")
        .or_else(|| entry.strip_suffix("/*"))
        .unwrap_or(entry);

    if let Some(bad) = trimmed.chars().find(|c| GLOB_METACHARACTERS.contains(c)) {
        return Err(format!(
            "deny path {entry:?} contains the glob metacharacter {bad:?}; \
             deny entries are paths, not globs (a trailing `/**` is allowed). \
             Use a concrete path with a directory prefix instead"
        ));
    }

    if trimmed.is_empty() {
        return Err(format!(
            "deny path {entry:?} resolves to an empty path; \
             a deny rule anchored at the filesystem root cannot be expressed"
        ));
    }

    Ok(trimmed.to_string())
}

/// Normalize every entry in `paths`, failing on the first that cannot be
/// expressed. Fails closed by construction: the caller gets no policy at all
/// rather than one missing a rule.
fn normalize_deny_paths(paths: &[String]) -> Result<Vec<String>, String> {
    paths.iter().map(|p| normalize_deny_entry(p)).collect()
}

/// TLS trust-store roots a network-enabled policy needs to be able to read.
const TLS_READ_ROOTS: &[&str] = &["/etc/ssl", "/etc/ssl/certs"];

/// System location of the macOS Seatbelt helper. Addressed absolutely: it is
/// launched before the sandbox exists, so a PATH lookup could be shadowed by
/// a directory a sandboxed tool may write to.
#[cfg(target_os = "macos")]
const SEATBELT_HELPER: &str = "/usr/bin/sandbox-exec";

/// Append the TLS roots that `exists` reports as present, skipping duplicates.
///
/// Only existing paths are added: distros vary (`/etc/ssl/certs` is absent on
/// some minimal container images and on macOS), and bwrap fails the *entire*
/// spawn when a bind source is missing. Adding a path that isn't there turned a
/// cosmetic default into "this host cannot run a sandboxed command at all".
///
/// `exists` is a parameter so tests are deterministic instead of asserting
/// against whatever the build host happens to have in `/etc`.
fn add_tls_read_roots(fs_read: &mut Vec<String>, exists: impl Fn(&str) -> bool) {
    for tls_path in TLS_READ_ROOTS {
        let candidate = (*tls_path).to_string();
        if !fs_read.contains(&candidate) && exists(tls_path) {
            fs_read.push(candidate);
        }
    }
}

impl SandboxPolicy {
    /// Filter blocked paths from fs_read/fs_write, consuming self.
    pub fn with_blocked_paths_filtered(mut self, blocked_paths: &[String]) -> Self {
        self.fs_read = filter_blocked(&self.fs_read, blocked_paths);
        self.fs_write = filter_blocked(&self.fs_write, blocked_paths);
        self
    }

    /// Resolve component-style `blocked_paths` (e.g. `.env`, `credentials`) into
    /// concrete deny subpaths anchored at the **top level** of `$HOME` and each
    /// of `roots`, adding them to both `deny_read` and `deny_write`.
    ///
    /// This is intentionally narrower than the tool-level matcher: `is_path_blocked`
    /// blocks an entry as a path *component at any depth*, which an OS sandbox
    /// `(deny … (subpath …))` rule can't express. We anchor each entry one level
    /// under home and the workspace root (`~/.env`, `<workspace>/.env`, …), which
    /// covers the common locations as defense in depth on top of the allow-list
    /// scoping — it does NOT cover nested copies like `<workspace>/src/.env`.
    ///
    /// Every backend enforces the read side: Seatbelt (macOS) and Windows emit
    /// deny-read rules, and bwrap (Linux) masks each entry with a tmpfs or a
    /// `/dev/null` bind (see `hide_path` in `bubblewrap.rs`).
    pub fn with_blocked_paths_denied(mut self, blocked_paths: &[String], roots: &[String]) -> Self {
        let mut anchors: Vec<String> = Vec::new();
        if let Some(home) = dirs::home_dir() {
            anchors.push(home.to_string_lossy().into_owned());
        }
        anchors.extend(roots.iter().cloned());

        for entry in blocked_paths {
            let entry = entry.trim_matches('/');
            if entry.is_empty() {
                continue;
            }
            for anchor in &anchors {
                let full = format!("{}/{entry}", anchor.trim_end_matches('/'));
                if !self.deny_read.contains(&full) {
                    self.deny_read.push(full.clone());
                }
                if !self.deny_write.contains(&full) {
                    self.deny_write.push(full);
                }
            }
        }
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
            add_tls_read_roots(&mut self.fs_read, |p| std::path::Path::new(p).exists());
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

    /// Add `~/.borg/` to deny_write to protect agent config, and deny *read* of
    /// the credential/key material inside it, consuming self.
    ///
    /// The whole `~/.borg` is write-protected. For reads we deny the specific
    /// secret stores (`secrets/`, `identity/`, `borg.db`, `editor/`) rather
    /// than all of `~/.borg`, because the sandboxed tool's own directory lives
    /// under `~/.borg/tools/<tool>` and is bound in explicitly. These deny-read
    /// entries are emitted last by the backends, so they win even under a broad
    /// `fs_read` (e.g. the `trusted` profile's `/`), closing the gap where a
    /// sandboxed tool could `cat ~/.borg/secrets/kek.enc` and exfiltrate it.
    ///
    /// `editor/` holds `$EDITOR` scratch buffers — the user's unsent message.
    /// It is deny-read for the same reason: the text is the user's, not the
    /// model's, until they send it.
    pub fn with_borg_dir_protected(mut self) -> Self {
        let borg_dir = dirs::home_dir()
            .map(|h| format!("{}/.borg", h.display()))
            .unwrap_or_else(|| {
                tracing::warn!("Could not determine home directory; using literal '~/.borg' which may not resolve");
                "~/.borg".to_string()
            });

        if !self.deny_write.iter().any(|p| p == &borg_dir) {
            self.deny_write.push(borg_dir.clone());
        }

        for sensitive in ["secrets", "identity", "borg.db", "editor"] {
            let path = format!("{borg_dir}/{sensitive}");
            if !self.deny_read.iter().any(|p| p == &path) {
                self.deny_read.push(path);
            }
        }
        self
    }

    /// Reduce every `deny_read`/`deny_write` entry to a literal path, erroring
    /// on any that cannot be expressed as a sandbox rule.
    ///
    /// See [`SandboxPolicy::deny_read`] for the accepted shapes. Applied by
    /// [`SandboxPolicy::wrap_command`] on every backend, so an unexpressible
    /// deny refuses the command instead of quietly leaving the path readable.
    pub fn with_denies_normalized(&self) -> Result<Self, String> {
        Ok(Self {
            deny_read: normalize_deny_paths(&self.deny_read)?,
            deny_write: normalize_deny_paths(&self.deny_write)?,
            network: self.network,
            fs_read: self.fs_read.clone(),
            fs_write: self.fs_write.clone(),
            allowed_domains: self.allowed_domains.clone(),
        })
    }

    /// Wrap a command with platform-specific sandbox isolation.
    pub fn wrap_command(
        &self,
        program: &str,
        args: &[String],
        tool_dir: &std::path::Path,
    ) -> SandboxCommand {
        // Normalize first, on every backend: a deny rule we cannot express must
        // stop the command, never be dropped on the way to the backend. Done
        // here rather than in `configured()` so it also covers policies built
        // by hand, and so the callers of `configured()` keep their signatures.
        let policy = match self.with_denies_normalized() {
            Ok(policy) => policy,
            Err(e) => return fail_closed("unenforceable sandbox deny rule", &e),
        };

        if cfg!(target_os = "macos") {
            policy.wrap_seatbelt(program, args, tool_dir)
        } else if cfg!(target_os = "linux") {
            policy.wrap_bubblewrap(program, args, tool_dir)
        } else if cfg!(target_os = "windows") {
            policy.wrap_windows(program, args, tool_dir)
        } else {
            tracing::warn!("Sandboxing not available on this platform; running unsandboxed");
            SandboxCommand::passthrough(program, args)
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
    /// than running unsandboxed — silent un-sandboxing on a path-resolution
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
        let profile = match generate_profile(self, tool_dir, program) {
            Ok(p) => p,
            Err(e) => {
                tracing::error!(
                    "Failed to generate sandbox profile: {e}. Refusing to run unsandboxed."
                );
                // Fail closed: a stub that exits non-zero, never the raw program.
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
            // The helper runs *before* the sandbox exists, so it is addressed
            // by its system location rather than resolved through PATH, where
            // a tool-writable directory could shadow it.
            program: SEATBELT_HELPER.to_string(),
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
        SandboxCommand::passthrough(program, args)
    }

    #[cfg(target_os = "linux")]
    fn wrap_bubblewrap(
        &self,
        program: &str,
        args: &[String],
        tool_dir: &std::path::Path,
    ) -> SandboxCommand {
        use crate::bubblewrap::{
            build_bwrap_args, find_pre_sandbox_executable, wslg_distro_grant, WSLG_DISTRO_DIR,
        };
        // Deliberate divergence from `wrap_windows`, which fails closed: bwrap is
        // an optional distro package, and hard-failing here would brick every
        // tool call on a host that simply never installed it. The warn is the
        // operator's signal to install bubblewrap. The lookup also refuses a
        // `bwrap` a sandboxed tool could have planted (under an `fs_write`
        // grant, the tool dir, or a relative PATH entry): the helper and its
        // version probe are the one spawn this policy cannot confine.
        let Some(bwrap) = find_pre_sandbox_executable("bwrap", self, tool_dir) else {
            tracing::warn!("bwrap not found on a trusted PATH entry; running without sandbox");
            return SandboxCommand::passthrough(program, args);
        };
        // A grant under WSLg's duplicate root reaches, by a second name, the
        // very paths `deny_read` masks — and no overlay can mask both names
        // while honoring the grant. Refuse rather than silently widen it.
        if let Some(path) = wslg_distro_grant(self) {
            return fail_closed(
                "sandbox grant under the WSLg duplicate root",
                &format!(
                    "{path:?} is under {WSLG_DISTRO_DIR}, which mirrors `/`; \
                     grant the path by its real location instead"
                ),
            );
        }
        let mut bwrap_args = build_bwrap_args(self, tool_dir, &bwrap);
        bwrap_args.push(program.to_string());
        bwrap_args.extend(args.iter().cloned());

        SandboxCommand {
            program: bwrap.to_string_lossy().into_owned(),
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
        SandboxCommand::passthrough(program, args)
    }
}

#[cfg(test)]
mod sandbox_command_tests {
    use super::*;

    /// Spec: `passthrough` is the audited "run this outside the sandbox"
    /// escape hatch. It must run exactly the command it was handed — an empty
    /// or altered program here would break every unsandboxed platform path.
    #[test]
    fn passthrough_runs_exactly_the_requested_command() {
        let args = vec!["-c".to_string(), "print(1)".to_string()];
        let cmd = SandboxCommand::passthrough("python3", &args);
        assert_eq!(cmd.program, "python3");
        assert_eq!(cmd.args, args);
        assert!(
            cmd._resources.is_none(),
            "a passthrough holds no OS resources"
        );
    }

    /// Spec: the hand-written `Debug` exists so wrapped commands are auditable
    /// in logs. It must show the program and args it will execute.
    #[test]
    fn debug_output_names_the_program_and_args() {
        let cmd = SandboxCommand::passthrough("python3", &["run.py".to_string()]);
        let rendered = format!("{cmd:?}");
        assert!(
            rendered.contains("python3") && rendered.contains("run.py"),
            "Debug must expose the command for audit logs: {rendered}"
        );
    }

    /// Spec: `Clone` is hand-written because `_resources` is a type-erased
    /// drop guard — a clone must run the same command *and* share the same
    /// guard, so the OS handle stays open until the last copy drops.
    #[test]
    fn clone_preserves_the_command_and_shares_the_resource_guard() {
        let guard: std::sync::Arc<dyn std::any::Any + Send + Sync> = std::sync::Arc::new(7u8);
        let cmd = SandboxCommand {
            program: "tool".to_string(),
            args: vec!["arg".to_string()],
            _resources: Some(guard.clone()),
        };
        let cloned = cmd.clone();
        assert_eq!(cloned.program, "tool");
        assert_eq!(cloned.args, vec!["arg".to_string()]);
        let held = cloned
            ._resources
            .as_ref()
            .expect("the clone must keep the resource guard alive");
        assert!(
            std::sync::Arc::ptr_eq(held, &guard),
            "clones must share one guard, not invent a new one"
        );
    }
}

#[cfg(test)]
mod fail_closed_tests {
    use super::*;

    /// Spec: the Windows refusal stub must actually refuse — report the
    /// failure on stderr and exit non-zero via `cmd.exe`, never a runnable
    /// (or empty) command.
    #[test]
    fn the_windows_refusal_stub_reports_and_exits_nonzero() {
        let cmd = fail_closed_windows("setup context", "the detail");
        assert_eq!(cmd.program, "cmd.exe");
        assert!(
            cmd.args
                .iter()
                .any(|a| a.contains("Borg sandbox setup failed: setup context: the detail")),
            "the refusal must explain itself: {:?}",
            cmd.args
        );
        assert!(
            cmd.args.iter().any(|a| a.contains("exit 1")),
            "the stub must exit non-zero: {:?}",
            cmd.args
        );
    }
}

#[cfg(test)]
mod blocked_path_tests {
    use super::*;

    #[test]
    fn a_dotted_family_member_is_blocked_like_the_entry() {
        // Regression: the sandbox matcher was exact-component-equality only
        // (its second branch, `starts_with("{blocked}/")`, could never fire —
        // a `Component::Normal` contains no '/'). So a policy `fs_read` of
        // `~/work/.env.production` survived the filter and was emitted as an
        // `(allow file-read* …)` / `--ro-bind`, granting the sandboxed tool
        // exactly the file the blocklist names.
        let blocked = vec![".env".to_string(), "private_key".to_string()];
        for path in [
            "/home/u/work/.env",
            "/home/u/work/.env.local",
            "/home/u/work/.env.production",
            "/home/u/keys/private_key.pem",
        ] {
            assert!(
                is_path_blocked(path, &blocked),
                "{path} must be blocked by the dotted-family rule"
            );
        }
    }

    #[test]
    fn a_similarly_named_path_is_not_over_blocked() {
        // The other half: the rule is suffix-after-a-dot, not substring, so
        // ordinary files that merely start with a blocked name stay usable.
        let blocked = vec![".env".to_string(), ".ssh".to_string()];
        for path in [
            "/home/u/.environment",
            "/home/u/.sshrc",
            "/home/u/env",
            "/home/u/.env_backup",
        ] {
            assert!(
                !is_path_blocked(path, &blocked),
                "{path} must not be blocked — the rule is not a substring match"
            );
        }
    }

    /// Boundary of the dotted-family rule: the suffix after the dot must be
    /// non-empty. `.env.local` is a family member; a bare trailing dot
    /// (`.env.`) is not — the rule is "entry plus a real extension", so the
    /// empty-extension edge stays on the unblocked side with `.env_backup`.
    #[test]
    fn a_trailing_dot_with_no_extension_is_not_a_family_member() {
        assert!(component_matches_family(".env.local", ".env", true));
        assert!(!component_matches_family(".env.", ".env", true));
        assert!(!component_matches_family(
            "credentials.",
            "credentials",
            false
        ));
    }

    /// On APFS every spelling below opens the blocked name (checked on a real
    /// volume), so none may reach the sandbox as an allowed bind.
    #[test]
    fn dotted_entries_match_every_case_spelling() {
        let blocked = vec![
            ".aws".to_string(),
            ".env".to_string(),
            ".ssh".to_string(),
            ".config".to_string(),
            ".kube".to_string(),
        ];
        for path in [
            "/home/u/.AWS/credentials",
            "/home/u/.Env",
            "/home/u/.ENV.local",
            "/home/u/.awſ/config",    // long s
            "/home/u/.ßh/id_ed25519", // sharp s folds to "ss"
            "/home/u/.SSH/authorized_keys",
            "/home/u/.conﬁg/gh",           // "fi" ligature
            "/home/u/.\u{212A}ube/config", // Kelvin sign
        ] {
            assert!(is_path_blocked(path, &blocked), "{path} must be blocked");
        }
        // Folding must not widen the family rule. `.enéx` also puts a
        // multi-byte char where `.env` ends.
        for path in [
            "/home/u/.AWS_notes",
            "/home/u/.ENVIRONMENT",
            "/home/u/.enéx",
        ] {
            assert!(!is_path_blocked(path, &blocked), "{path} must stay allowed");
        }
    }

    /// An undotted entry is a common word that also names source files, so it
    /// matches as written: `Credentials.java` is not `credentials`.
    #[test]
    fn undotted_entries_match_as_written() {
        let blocked = vec!["credentials".to_string(), "private_key".to_string()];
        for path in ["/home/u/.aws/credentials", "/w/private_key.pem"] {
            assert!(is_path_blocked(path, &blocked), "{path} must be blocked");
        }
        for path in [
            "/w/src/Credentials.java",
            "/w/Sources/Credentials/Store.swift",
            "/w/Private_Key.md",
        ] {
            assert!(!is_path_blocked(path, &blocked), "{path} must stay allowed");
        }
    }

    #[test]
    fn fold_case_expands_the_full_folds_that_reach_ascii() {
        for (input, folded) in [
            (".ssh", ".ssh"),
            (".SSH", ".ssh"),
            ("ẞ", "ss"),
            ("ﬃ", "ffi"),
            ("ﬄ", "ffl"),
            ("ﬂ", "fl"),
            ("ﬀ", "ff"),
            ("ﬅﬆ", "stst"),
            ("É", "é"),
        ] {
            assert_eq!(fold_case(input), folded, "fold of {input:?}");
        }
    }

    #[test]
    fn filter_blocked_drops_only_the_blocked_entries() {
        let blocked = vec![".aws".to_string()];
        let paths = vec![
            "/home/u/project".to_string(),
            "/home/u/.aws/credentials".to_string(),
            "/home/u/.aws_notes".to_string(),
        ];
        assert_eq!(
            filter_blocked(&paths, &blocked),
            vec![
                "/home/u/project".to_string(),
                "/home/u/.aws_notes".to_string()
            ]
        );
    }
}

#[cfg(test)]
mod deny_normalization_tests {
    use super::*;

    /// Spec: the documented `~/.borg/**` spelling must reach the backends as
    /// the path it denies, not be dropped. Before this, bwrap's `hide_path`
    /// found no such file and emitted no rule at all — the deny failed open.
    #[test]
    fn a_trailing_recursive_glob_normalizes_to_its_directory() {
        for entry in ["/home/u/.borg/**", "/home/u/.borg/*"] {
            assert_eq!(
                normalize_deny_entry(entry).unwrap(),
                "/home/u/.borg",
                "{entry} must reduce to the directory it denies"
            );
        }
    }

    /// Spec: a pattern with no literal directory prefix cannot become a rule,
    /// so it must be an error rather than a silently missing deny.
    #[test]
    fn an_unexpressible_glob_is_rejected() {
        let cases = [
            "/**/*.env",
            "/home/*/.ssh",
            "/home/u/id_?sa",
            "/home/u/[abc]",
        ];
        for entry in cases {
            let err = normalize_deny_entry(entry)
                .expect_err("{entry} is not expressible and must not be accepted");
            assert!(
                err.contains("paths, not globs"),
                "{entry}: error must say why: {err}"
            );
        }
    }

    /// A bare `/**` strips to nothing; denying the whole root is not a rule
    /// either backend can express, so it must not silently become a no-op.
    #[test]
    fn an_entry_that_strips_to_empty_is_rejected() {
        assert!(normalize_deny_entry("/**").is_err());
    }

    #[test]
    fn a_literal_path_is_unchanged() {
        assert_eq!(
            normalize_deny_entry("/home/u/.ssh").unwrap(),
            "/home/u/.ssh"
        );
    }

    /// Both deny lists are normalized, and an error in either one loses the
    /// whole policy — there is no partial policy to accidentally run under.
    #[test]
    fn normalization_covers_both_deny_lists() {
        let policy = SandboxPolicy {
            deny_read: vec!["/a/**".to_string()],
            deny_write: vec!["/b/*".to_string()],
            ..Default::default()
        };
        let normalized = policy.with_denies_normalized().unwrap();
        assert_eq!(normalized.deny_read, vec!["/a".to_string()]);
        assert_eq!(normalized.deny_write, vec!["/b".to_string()]);

        let bad = SandboxPolicy {
            deny_write: vec!["/**/*.env".to_string()],
            ..Default::default()
        };
        assert!(bad.with_denies_normalized().is_err());
    }

    /// Spec: the refusal stub must never interpolate a config-supplied path
    /// into its shell script. `{:?}` escapes double quotes and backslashes but
    /// not single quotes, so a deny entry carrying one would close the literal
    /// and run as a command — inside the one code path whose entire job is to
    /// refuse to run anything.
    #[cfg(not(target_os = "windows"))]
    #[test]
    fn the_refusal_stub_never_interpolates_a_path_into_its_script() {
        let hostile = "/tmp/'; touch /tmp/borg-pwned; echo '*";
        let policy = SandboxPolicy {
            deny_read: vec![hostile.to_string()],
            ..Default::default()
        };
        let cmd = policy.wrap_command(
            "echo",
            &["hi".to_string()],
            std::path::Path::new("/tmp/tool"),
        );

        let script = &cmd.args[1];
        assert!(
            !script.contains("borg-pwned"),
            "the offending path must not reach the script body: {script}"
        );
        assert!(
            cmd.args.last().is_some_and(|m| m.contains("borg-pwned")),
            "the path belongs in the message argument, not the script"
        );

        // And the stub still refuses, loudly.
        let out = std::process::Command::new(&cmd.program)
            .args(&cmd.args)
            .output()
            .expect("the refusal stub must be runnable");
        assert!(
            !out.status.success(),
            "an unenforceable deny must not exit 0"
        );
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("Borg sandbox setup failed"),
            "the refusal must explain itself on stderr"
        );
        assert!(
            !std::path::Path::new("/tmp/borg-pwned").exists(),
            "the injected command must not have run"
        );
    }

    /// The whole point: an unenforceable deny must stop the command. A
    /// wrapper that ran the program anyway would be the fail-open bug in a
    /// new place.
    #[test]
    fn wrap_command_refuses_an_unenforceable_deny() {
        let policy = SandboxPolicy {
            fs_read: vec!["/".to_string()],
            deny_read: vec!["/**/*.env".to_string()],
            ..Default::default()
        };
        let cmd = policy.wrap_command(
            "cat",
            &["/etc/hosts".to_string()],
            std::path::Path::new("/tmp/tool"),
        );
        assert_ne!(cmd.program, "cat", "the real program must never run");
        assert!(
            cmd.args.iter().any(|a| a.contains("exit 1")),
            "the stub must exit non-zero: {:?}",
            cmd.args
        );
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

    // Used by the linux + generic-fallback tests; unused on the windows
    // cfg (which uses inline paths), so allow dead_code there rather than
    // splintering the cfg further.
    #[cfg(not(target_os = "macos"))]
    #[allow(dead_code)]
    const TEST_TOOL_DIR: &str = "/tmp/tool";

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_wraps_with_bwrap() {
        let policy = SandboxPolicy::default();
        let args = vec!["script.py".to_string()];
        let cmd = policy.wrap_command("python3", &args, Path::new(TEST_TOOL_DIR));
        match crate::bubblewrap::find_pre_sandbox_executable(
            "bwrap",
            &policy,
            Path::new(TEST_TOOL_DIR),
        ) {
            // Spec: the helper is launched by its resolved absolute path, never
            // re-looked-up on PATH at spawn time.
            Some(bwrap) => assert_eq!(cmd.program, bwrap.to_string_lossy()),
            None => assert_eq!(cmd.program, "python3"),
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
        // deny_read entries for the secret stores must also be de-duplicated.
        let secrets_count = protected
            .deny_read
            .iter()
            .filter(|p| p.ends_with("/.borg/secrets"))
            .count();
        assert_eq!(secrets_count, 1);
    }

    #[test]
    fn with_borg_dir_protected_denies_read_of_secret_stores() {
        // M2/M1: the credential/key material inside ~/.borg must be deny-read,
        // not just deny-write, so a sandboxed tool can't exfiltrate kek.enc /
        // borg.db even under a broad fs_read. `editor/` joins them: it holds
        // the $EDITOR scratch buffer, i.e. the user's unsent message, which is
        // theirs and not the model's until they send it.
        let protected = SandboxPolicy::default().with_borg_dir_protected();
        for suffix in [
            "/.borg/secrets",
            "/.borg/identity",
            "/.borg/borg.db",
            "/.borg/editor",
        ] {
            assert!(
                protected.deny_read.iter().any(|p| p.ends_with(suffix)),
                "deny_read missing {suffix}: {:?}",
                protected.deny_read
            );
        }
    }

    #[test]
    fn defaults_applied_adds_tls_for_network() {
        let mut fs_read = vec!["/data".to_string()];
        add_tls_read_roots(&mut fs_read, |_| true);
        assert!(
            fs_read.contains(&"/etc/ssl".to_string()),
            "network=true should add /etc/ssl when it exists"
        );
        assert!(
            fs_read.contains(&"/etc/ssl/certs".to_string()),
            "network=true should add /etc/ssl/certs when it exists"
        );
        assert!(
            fs_read.contains(&"/data".to_string()),
            "original paths preserved"
        );
    }

    /// Distros differ: `/etc/ssl/certs` is absent on some minimal container
    /// images and on macOS. Because bwrap fails the whole spawn on a missing
    /// bind source, an absent TLS root must never reach `fs_read` — otherwise
    /// enabling network makes every sandboxed command unrunnable on that host.
    #[test]
    fn defaults_applied_skips_tls_roots_that_do_not_exist() {
        let mut fs_read = Vec::new();
        add_tls_read_roots(&mut fs_read, |p| p == "/etc/ssl");
        assert_eq!(
            fs_read,
            vec!["/etc/ssl".to_string()],
            "only the existing TLS root should be added"
        );

        let mut none_exist = Vec::new();
        add_tls_read_roots(&mut none_exist, |_| false);
        assert!(
            none_exist.is_empty(),
            "no TLS roots present ⇒ nothing added: {none_exist:?}"
        );
    }

    #[test]
    fn defaults_applied_does_not_duplicate_seeded_tls_roots() {
        let mut fs_read = vec!["/etc/ssl".to_string()];
        add_tls_read_roots(&mut fs_read, |_| true);
        assert_eq!(
            fs_read.iter().filter(|p| *p == "/etc/ssl").count(),
            1,
            "seeded entry must not be duplicated"
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
        // Idempotence guard: applying defaults twice must never duplicate
        // entries. Without this, repeated config reloads would grow fs_read
        // unboundedly. (Seeded-entry dedup is covered host-independently by
        // `defaults_applied_does_not_duplicate_seeded_tls_roots`.)
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
    fn blocked_paths_denied_anchors_under_roots() {
        let policy = SandboxPolicy {
            fs_read: vec!["/ws".into()],
            fs_write: vec!["/ws".into()],
            ..Default::default()
        }
        .with_blocked_paths_denied(
            &[".env".to_string(), "credentials".to_string()],
            &["/ws".to_string()],
        );

        // Workspace-root anchoring is deterministic regardless of $HOME.
        for expected in ["/ws/.env", "/ws/credentials"] {
            assert!(
                policy.deny_read.contains(&expected.to_string()),
                "deny_read missing {expected}: {:?}",
                policy.deny_read
            );
            assert!(
                policy.deny_write.contains(&expected.to_string()),
                "deny_write missing {expected}: {:?}",
                policy.deny_write
            );
        }
        // Home anchoring applies when a home dir is resolvable.
        if let Some(home) = dirs::home_dir() {
            let home_env = format!("{}/.env", home.to_string_lossy());
            assert!(
                policy.deny_read.contains(&home_env),
                "deny_read missing home-anchored {home_env}: {:?}",
                policy.deny_read
            );
        }
    }

    #[test]
    fn blocked_paths_denied_skips_empty_and_dedups() {
        let policy = SandboxPolicy::default().with_blocked_paths_denied(
            &[
                "".to_string(),
                "/".to_string(),
                ".env".to_string(),
                ".env".to_string(),
            ],
            &["/ws".to_string(), "/ws/".to_string()],
        );
        let count = policy
            .deny_read
            .iter()
            .filter(|p| p.as_str() == "/ws/.env")
            .count();
        assert_eq!(
            count, 1,
            "duplicate anchors must collapse: {:?}",
            policy.deny_read
        );
        assert!(
            !policy.deny_read.iter().any(|p| p.ends_with("/")),
            "empty/root blocked entries must be skipped: {:?}",
            policy.deny_read
        );
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

        let policy = SandboxPolicy {
            fs_read: vec!["~/Library/Messages".into(), "/etc/ssl".into()],
            fs_write: vec!["~/.borg/channels/imessage".into()],
            ..Default::default()
        };
        let expanded = policy.with_tildes_expanded();

        // `~/rest` joins `rest` onto the home dir, so the separator after home
        // is the platform's (`\` on Windows).
        let under_home = |rest: &str| home.join(rest).to_string_lossy().into_owned();
        assert_eq!(
            expanded.fs_read,
            vec![under_home("Library/Messages"), "/etc/ssl".to_string()]
        );
        assert_eq!(
            expanded.fs_write,
            vec![under_home(".borg/channels/imessage")]
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
            result
                .deny_write
                .iter()
                .any(|p| std::path::Path::new(p).ends_with(".borg")),
            "borg dir should be protected"
        );
        assert!(
            result.fs_read.iter().any(|p| p.starts_with(&*home_str)),
            "tildes should be expanded"
        );
        // TLS roots are added only where they exist (none on Windows).
        assert_eq!(
            result.fs_read.contains(&"/etc/ssl".to_string()),
            std::path::Path::new("/etc/ssl").exists(),
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

    /// Spec: the Windows wrapper is only reachable through the runtime
    /// `cfg!(target_os = "windows")` dispatch, and its non-Windows trampoline
    /// stub panics by contract. On any other host it must never fabricate a
    /// command (empty or otherwise) that a caller could go on to spawn.
    #[cfg(not(target_os = "windows"))]
    #[test]
    #[should_panic(expected = "Windows-only")]
    fn wrap_windows_off_windows_panics_rather_than_fabricating_a_command() {
        let policy = SandboxPolicy::default();
        let args = vec!["script.py".to_string()];
        policy.wrap_windows("python3", &args, std::path::Path::new("/tmp/tool"));
    }

    /// Spec: on a non-Linux host the bubblewrap wrapper degrades to running
    /// the command directly — never an empty command that would spawn nothing.
    #[cfg(not(target_os = "linux"))]
    #[test]
    fn wrap_bubblewrap_off_linux_degrades_to_passthrough() {
        let policy = SandboxPolicy::default();
        let args = vec!["arg1".to_string(), "arg2".to_string()];
        let cmd = policy.wrap_bubblewrap("node", &args, std::path::Path::new("/tmp/tool"));
        assert_eq!(cmd.program, "node");
        assert_eq!(cmd.args, args);
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
