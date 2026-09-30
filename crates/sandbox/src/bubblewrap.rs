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

    /// Returns true if this version supports `--perms` (added in 0.5.0).
    pub fn supports_perms(&self) -> bool {
        (self.major, self.minor) >= (0, 5)
    }
}

/// Where WSL keeps its Windows-interop sockets. A process that can reach them
/// can launch Windows executables — outside every Linux namespace bwrap sets up.
const WSL_INTEROP_DIR: &str = "/run/WSL";

/// WSLg's view of the distro's own root filesystem. On WSLg it can be the very
/// same mount as `/`, so a bind of `/` exposes every file a second time under
/// this prefix — where no `deny_read` overlay names it.
pub const WSLG_DISTRO_DIR: &str = "/mnt/wslg/distro";

/// Host facts the argument builder depends on, gathered once per call so
/// tests can fake a WSL host on any machine.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct HostProbe {
    /// `/run/WSL` exists as a directory.
    pub wsl_interop_dir: bool,
    /// `/mnt/wslg/distro` is the same inode as `/` (see [`is_wslg_duplicate_root`]).
    pub wslg_duplicate_root: bool,
}

impl HostProbe {
    /// Probe the running host. Both facts are Linux-only; elsewhere they are
    /// false, which emits nothing extra.
    pub fn detect() -> Self {
        if cfg!(target_os = "linux") {
            Self {
                wsl_interop_dir: Path::new(WSL_INTEROP_DIR).is_dir(),
                wslg_duplicate_root: is_wslg_duplicate_root(),
            }
        } else {
            Self::default()
        }
    }
}

/// True when `/mnt/wslg/distro` is the same directory as `/` — same device,
/// same inode — which is how WSLg exposes the distro root.
pub fn is_wslg_duplicate_root() -> bool {
    same_inode(Path::new(WSLG_DISTRO_DIR), Path::new("/"))
}

/// True when both paths exist and name the same (dev, ino). A path that cannot
/// be stat-ed is not a duplicate: absent is the normal, non-WSLg case.
#[cfg(unix)]
fn same_inode(a: &Path, b: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (std::fs::metadata(a), std::fs::metadata(b)) {
        (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
        _ => false,
    }
}

#[cfg(not(unix))]
fn same_inode(_a: &Path, _b: &Path) -> bool {
    false
}

/// The first `fs_read`/`fs_write` entry at or under `/mnt/wslg/distro`, if any.
///
/// Such a grant can only be honored by binding the duplicate root itself, which
/// would re-expose every masked path through it. The caller refuses to run.
pub fn wslg_distro_grant(policy: &SandboxPolicy) -> Option<&str> {
    grant_under(policy, Path::new(WSLG_DISTRO_DIR))
}

/// The first `fs_read`/`fs_write` entry that resolves to `dir` or below it.
/// `..` segments and symlinks are resolved first, so `/mnt/wslg/x/../distro`
/// or a symlink into `dir` cannot slip past a plain prefix test.
fn grant_under<'a>(policy: &'a SandboxPolicy, dir: &Path) -> Option<&'a str> {
    let dir_canon = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    policy
        .fs_read
        .iter()
        .chain(&policy.fs_write)
        .find(|p| {
            let resolved = resolve_path(Path::new(p.as_str()));
            resolved.starts_with(dir) || resolved.starts_with(&dir_canon)
        })
        .map(String::as_str)
}

/// Collapse `.`/`..` lexically, then canonicalize the longest existing
/// ancestor and re-append the rest. A path that does not exist yet still
/// resolves through any symlinked parent.
fn resolve_path(path: &Path) -> std::path::PathBuf {
    use std::path::{Component, PathBuf};
    let mut lexical = PathBuf::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                lexical.pop();
            }
            other => lexical.push(other),
        }
    }
    let mut head = lexical.clone();
    let mut tail = Vec::new();
    loop {
        if let Ok(canon) = std::fs::canonicalize(&head) {
            return tail.iter().rev().fold(canon, |acc, part| acc.join(part));
        }
        match (
            head.file_name().map(std::ffi::OsStr::to_os_string),
            head.parent(),
        ) {
            (Some(name), Some(parent)) => {
                tail.push(name);
                head = parent.to_path_buf();
            }
            _ => return lexical,
        }
    }
}

/// True when some `fs_read`/`fs_write` entry is `target` or one of its
/// ancestors — so `target` is visible inside the sandbox.
fn bind_covers(policy: &SandboxPolicy, target: &str) -> bool {
    policy
        .fs_read
        .iter()
        .chain(&policy.fs_write)
        .any(|p| Path::new(target).starts_with(p))
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

/// Shadow every unix socket at `path`, or directly inside it, with `/dev/null`.
///
/// A read-only bind does not stop `connect(2)`: Linux checks the read-only
/// mount flag only for regular files, directories and symlinks, never sockets.
/// So `--ro-bind ~/.borg` still lets a sandboxed child connect to
/// `~/.borg/borgd.sock` and drive the daemon as the user. Binding `/dev/null`
/// over the socket inode leaves nothing to connect to. A tmpfs over the whole
/// directory would also work but would hide the tool dir under
/// `~/.borg/tools`, which the sandbox needs.
///
/// Only the top level is scanned: borgd's sockets live there, and a recursive
/// walk of `~/.borg` on every spawn would cost more than it protects.
#[cfg(unix)]
fn hide_sockets(args: &mut Vec<String>, path: &str) {
    use std::os::unix::fs::FileTypeExt;
    let p = Path::new(path);
    let Ok(meta) = std::fs::symlink_metadata(p) else {
        return;
    };
    if meta.file_type().is_socket() {
        hide_path(args, path);
        return;
    }
    if !meta.is_dir() {
        return;
    }
    let entries = match std::fs::read_dir(p) {
        Ok(entries) => entries,
        Err(e) => {
            // Fail closed: sockets we cannot enumerate may still be live, so
            // hide the whole directory instead.
            tracing::warn!(
                path,
                error = %e,
                "sandbox: could not list a write-denied dir; masking it with a tmpfs"
            );
            args.extend(["--tmpfs".to_string(), path.to_string()]);
            return;
        }
    };
    for entry in entries.flatten() {
        if entry.file_type().is_ok_and(|t| t.is_socket()) {
            hide_path(args, &entry.path().to_string_lossy());
        }
    }
}

#[cfg(not(unix))]
fn hide_sockets(_args: &mut Vec<String>, _path: &str) {}

/// Build bwrap args with an explicit version for testability.
pub fn build_bwrap_args_versioned(
    policy: &SandboxPolicy,
    tool_dir: &Path,
    version: Option<&BwrapVersion>,
) -> Vec<String> {
    build_bwrap_args_probed(policy, tool_dir, version, HostProbe::detect())
}

/// [`build_bwrap_args_versioned`] with the host facts supplied by the caller.
pub fn build_bwrap_args_probed(
    policy: &SandboxPolicy,
    tool_dir: &Path,
    version: Option<&BwrapVersion>,
    host: HostProbe,
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
        // Component-wise, not string, prefix: a deny of `/home/u/.borg` must
        // not swallow a sibling grant like `/home/u/.borg-tools`.
        let denied = policy
            .deny_write
            .iter()
            .any(|d| Path::new(path).starts_with(d));
        if !denied {
            push_write_bind(&mut args, path);
        }
    }

    for path in &policy.deny_write {
        push_bind_if_exists(&mut args, "--ro-bind", path);
    }

    // After every bind that could expose them, so these overlays win.
    for path in &policy.deny_write {
        hide_sockets(&mut args, path);
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

    // WSL interop: the sockets under `/run/WSL` let a process start Windows
    // executables, which run outside this sandbox entirely. Mask them whenever
    // a bind makes them visible.
    if host.wsl_interop_dir && bind_covers(policy, WSL_INTEROP_DIR) {
        args.extend(["--tmpfs".to_string(), WSL_INTEROP_DIR.to_string()]);
    }

    // WSLg duplicate root: when `/mnt/wslg/distro` is `/` again, a bind of `/`
    // shows every masked path a second time under it. Replace it with an
    // empty, read-only, mode-000 tmpfs. Last among the filesystem args so no
    // later bind can land inside it.
    if host.wslg_duplicate_root && bind_covers(policy, WSLG_DISTRO_DIR) {
        if version.is_none_or(BwrapVersion::supports_perms) {
            args.extend(["--perms".to_string(), "000".to_string()]);
        }
        args.extend([
            "--tmpfs".to_string(),
            WSLG_DISTRO_DIR.to_string(),
            "--remount-ro".to_string(),
            WSLG_DISTRO_DIR.to_string(),
        ]);
    }

    if !policy.network {
        args.push("--unshare-net".to_string());
    }

    args.push("--unshare-pid".to_string());

    // A private IPC namespace. Without it the sandboxed child shares the host's
    // System V IPC and POSIX message queues, which are a filesystem-independent
    // channel: two sandboxed tools — or a sandboxed tool and an unsandboxed
    // process on the host — can pass data through a shared memory segment that
    // no bind mount, `deny_read`, or `--unshare-net` rule covers. Nothing borg
    // runs in a sandbox needs host IPC.
    //
    // Unlike `--die-with-parent` and `--cap-drop` this needs no version gate:
    // `--unshare-ipc` has been present since bwrap's first release, which is
    // older than the `BwrapVersion` floor the other two check against.
    args.push("--unshare-ipc".to_string());

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

    /// Spec: detection actually runs the `bwrap` binary found on `PATH` and
    /// parses its `--version` output — it must not be hardwired to `None`
    /// (which would silently disable every version-gated hardening flag).
    #[cfg(unix)]
    #[test]
    fn detect_bwrap_version_reads_the_binary_on_path() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let fake = dir.path().join("bwrap");
        std::fs::write(&fake, "#!/bin/sh\necho 'bubblewrap 0.7.3'\n").unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();

        let old_path = std::env::var("PATH").unwrap_or_default();
        // SAFETY: prepend-only PATH change, restored below; every other lookup
        // in the test binary still resolves through the untouched tail.
        unsafe {
            std::env::set_var("PATH", format!("{}:{old_path}", dir.path().display()));
        }
        let detected = detect_bwrap_version();
        // SAFETY: restore before asserting so a failure cannot leak the fake.
        unsafe {
            std::env::set_var("PATH", &old_path);
        }

        assert_eq!(
            detected,
            Some(BwrapVersion {
                major: 0,
                minor: 7,
                patch: 3
            })
        );
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

    /// Spec: System V IPC and POSIX message queues are a side channel that no
    /// bind mount, `deny_read` rule, or `--unshare-net` covers — a sandboxed
    /// tool could hand data to a host process through a shared memory segment.
    /// The namespace is unconditional: unlike `--die-with-parent` and
    /// `--cap-drop`, `--unshare-ipc` predates every bwrap version we support,
    /// so gating it on a detected version would only create a hole on old hosts.
    #[test]
    fn always_isolates_ipc_regardless_of_bwrap_version() {
        for version in [
            None,
            Some(BwrapVersion {
                major: 0,
                minor: 1,
                patch: 0,
            }),
            Some(BwrapVersion {
                major: 0,
                minor: 3,
                patch: 0,
            }),
        ] {
            let args = build_bwrap_args_versioned(
                &default_policy(),
                Path::new(TEST_TOOL_DIR),
                version.as_ref(),
            );
            assert!(
                args.contains(&"--unshare-ipc".to_string()),
                "IPC isolation missing for {version:?}: {args:?}"
            );
        }
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

    /// Regression: the deny check compared strings, so a deny of
    /// `/home/u/.borg` also swallowed the unrelated sibling write grant
    /// `/home/u/.borg-tools` — the tool ran, wrote into a throwaway view, and
    /// its output never reached the host.
    #[test]
    fn a_write_grant_beside_a_denied_dir_is_still_bound() {
        let tmp = tempfile::tempdir().unwrap();
        let denied = tmp.path().join(".borg");
        let sibling = tmp.path().join(".borg-tools");
        let nested = denied.join("tools");
        for dir in [&denied, &sibling, &nested] {
            std::fs::create_dir_all(dir).unwrap();
        }
        let s = |p: &Path| p.to_string_lossy().into_owned();
        let policy = SandboxPolicy {
            fs_write: vec![s(&sibling), s(&nested)],
            deny_write: vec![s(&denied)],
            ..Default::default()
        };
        let args = build_bwrap_args_versioned(&policy, Path::new(TEST_TOOL_DIR), None);
        let writable = |p: &str| args.windows(3).any(|w| w[0] == "--bind" && w[1] == p);
        assert!(
            writable(&s(&sibling)),
            "a sibling of a denied dir must stay writable: {args:?}"
        );
        assert!(
            !writable(&s(&nested)),
            "a path inside a denied dir must not become writable: {args:?}"
        );
    }

    /// Spec: a read-only bind does not stop `connect(2)` on a unix socket, so
    /// every socket directly inside a write-denied dir (borgd's control socket
    /// in `~/.borg`) is shadowed by `/dev/null` — after the dir's own bind, so
    /// the overlay wins. Regular files there stay readable, and a socket
    /// outside every denied dir is left alone.
    #[cfg(unix)]
    #[test]
    fn unlistable_write_denied_dir_is_masked_whole() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("locked");
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::read_dir(&dir).is_ok() {
            // Running as root: permissions do not stop the listing.
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
            return;
        }
        let path = dir.to_string_lossy().into_owned();
        let mut args = Vec::new();
        hide_sockets(&mut args, &path);
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(args, vec!["--tmpfs".to_string(), path]);
    }

    #[cfg(unix)]
    #[test]
    fn sockets_in_a_write_denied_dir_are_shadowed() {
        use std::os::unix::net::UnixListener;
        let tmp = tempfile::tempdir().unwrap();
        let denied = tmp.path().join("b");
        let open = tmp.path().join("o");
        std::fs::create_dir_all(&denied).unwrap();
        std::fs::create_dir_all(&open).unwrap();
        let daemon_sock = denied.join("d.sock");
        let open_sock = open.join("d.sock");
        let _l1 = UnixListener::bind(&daemon_sock).unwrap();
        let _l2 = UnixListener::bind(&open_sock).unwrap();
        let config = denied.join("config.toml");
        std::fs::write(&config, b"x").unwrap();

        let s = |p: &Path| p.to_string_lossy().into_owned();
        let policy = SandboxPolicy {
            fs_read: vec![s(tmp.path())],
            deny_write: vec![s(&denied)],
            ..Default::default()
        };
        let args = build_bwrap_args_versioned(&policy, Path::new(TEST_TOOL_DIR), None);
        let shadow_at = |p: &Path| {
            args.windows(3)
                .position(|w| w[0] == "--ro-bind" && w[1] == "/dev/null" && w[2] == s(p))
        };
        let dir_bind_at = args
            .windows(3)
            .position(|w| w[0] == "--ro-bind" && w[1] == s(&denied))
            .expect("the denied dir is bound read-only");

        let at = shadow_at(&daemon_sock)
            .unwrap_or_else(|| panic!("the socket in the denied dir must be shadowed: {args:?}"));
        assert!(
            at > dir_bind_at,
            "the shadow must follow the dir bind: {args:?}"
        );
        assert_eq!(shadow_at(&config), None, "regular files must stay readable");
        assert_eq!(
            shadow_at(&open_sock),
            None,
            "sockets outside must stay reachable"
        );
    }

    fn wsl_host() -> HostProbe {
        HostProbe {
            wsl_interop_dir: true,
            wslg_duplicate_root: true,
        }
    }

    /// Spec: `/run/WSL` is masked with a tmpfs exactly when the host has it and
    /// some read or write bind makes it visible. Masking when nothing binds it
    /// would make bwrap create a mount point in an unrelated tree.
    #[test]
    fn wsl_interop_is_masked_only_when_a_bind_reaches_it() {
        let cases: &[(bool, &[&str], &[&str], bool)] = &[
            // interop dir present, fs_read, fs_write, expect mask
            (true, &["/"], &[], true),
            (true, &["/run"], &[], true),
            (true, &[], &["/run/WSL"], true),
            (true, &["/home"], &[], false),
            (true, &["/run/WSLx"], &[], false),
            (false, &["/"], &[], false),
        ];
        for (present, read, write, want) in cases.iter().copied() {
            let policy = SandboxPolicy {
                fs_read: read.iter().map(|p| p.to_string()).collect(),
                fs_write: write.iter().map(|p| p.to_string()).collect(),
                ..Default::default()
            };
            let host = HostProbe {
                wsl_interop_dir: present,
                wslg_duplicate_root: false,
            };
            let args = build_bwrap_args_probed(&policy, Path::new(TEST_TOOL_DIR), None, host);
            let masked = args
                .windows(2)
                .any(|w| w[0] == "--tmpfs" && w[1] == WSL_INTEROP_DIR);
            assert_eq!(
                masked, want,
                "present={present} read={read:?} write={write:?}"
            );
        }
    }

    /// Spec: on a WSLg duplicate root, a bind of `/` gets an empty, read-only,
    /// mode-000 tmpfs over `/mnt/wslg/distro`, as the final filesystem args so
    /// no later bind lands inside it. `--perms` only on a bwrap that has it.
    #[test]
    fn wslg_duplicate_root_is_masked_as_the_last_filesystem_args() {
        let policy = SandboxPolicy {
            fs_read: vec!["/".to_string()],
            ..Default::default()
        };
        let old = BwrapVersion {
            major: 0,
            minor: 4,
            patch: 1,
        };
        let cases: &[(Option<&BwrapVersion>, bool)] = &[(None, true), (Some(&old), false)];
        for (version, want_perms) in cases.iter().copied() {
            let args =
                build_bwrap_args_probed(&policy, Path::new(TEST_TOOL_DIR), version, wsl_host());
            let mut expected: Vec<&str> = Vec::new();
            if want_perms {
                expected.extend(["--perms", "000"]);
            }
            expected.extend([
                "--tmpfs",
                WSLG_DISTRO_DIR,
                "--remount-ro",
                WSLG_DISTRO_DIR,
                "--unshare-net",
            ]);
            let start = args
                .windows(expected.len())
                .position(|w| w == expected.as_slice())
                .unwrap_or_else(|| panic!("no WSLg mask for {version:?}: {args:?}"));
            assert!(
                args[..start].iter().any(|a| a == WSL_INTEROP_DIR),
                "the interop mask comes first: {args:?}"
            );
            assert!(
                !args[start + expected.len()..]
                    .iter()
                    .any(|a| ["--bind", "--ro-bind", "--tmpfs"].contains(&a.as_str())),
                "no filesystem arg may follow the WSLg mask: {args:?}"
            );
        }
    }

    /// Spec: no mask when the duplicate root is absent or no bind covers it.
    #[test]
    fn wslg_mask_needs_both_the_duplicate_root_and_a_covering_bind() {
        let root = SandboxPolicy {
            fs_read: vec!["/".to_string()],
            ..Default::default()
        };
        let home = SandboxPolicy {
            fs_read: vec!["/home".to_string()],
            ..Default::default()
        };
        let plain = HostProbe::default();
        for (policy, host) in [(&root, plain), (&home, wsl_host())] {
            let args = build_bwrap_args_probed(policy, Path::new(TEST_TOOL_DIR), None, host);
            assert!(
                !args.iter().any(|a| a == WSLG_DISTRO_DIR),
                "unexpected WSLg mask for {host:?}: {args:?}"
            );
        }
    }

    /// Spec: any grant at or under `/mnt/wslg/distro` is reported, read or
    /// write, so the caller can refuse it; a lookalike sibling is not.
    /// Unix-only: WSLg is a Linux mount, and Windows resolves `/` to a drive.
    #[cfg(unix)]
    #[test]
    fn grants_under_the_wslg_distro_are_found() {
        let cases: &[(&[&str], &[&str], Option<&str>)] = &[
            (
                &["/mnt/wslg/distro/home/u"],
                &[],
                Some("/mnt/wslg/distro/home/u"),
            ),
            (&[], &["/mnt/wslg/distro"], Some("/mnt/wslg/distro")),
            (&["/mnt/wslg/distro-other"], &[], None),
            (&["/", "/mnt/wslg"], &["/home/u"], None),
        ];
        for &(read, write, want) in cases {
            let policy = SandboxPolicy {
                fs_read: read.iter().map(|p| p.to_string()).collect(),
                fs_write: write.iter().map(|p| p.to_string()).collect(),
                ..Default::default()
            };
            assert_eq!(
                wslg_distro_grant(&policy),
                want,
                "read={read:?} write={write:?}"
            );
        }
    }

    /// Spec: a grant that only reaches the protected dir through `..` or a
    /// symlink is still caught; an unrelated grant is not.
    #[cfg(unix)]
    #[test]
    fn grants_reaching_the_dir_through_dotdot_or_symlinks_are_found() {
        let tmp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let distro = root.join("distro");
        let other = root.join("other");
        std::fs::create_dir_all(distro.join("home")).unwrap();
        std::fs::create_dir(&other).unwrap();
        let link = root.join("link");
        std::os::unix::fs::symlink(&distro, &link).unwrap();
        let s = |p: std::path::PathBuf| p.to_string_lossy().into_owned();
        let cases = [
            (s(root.join("other/../distro/home")), true),
            (s(link.join("home")), true),
            (s(link.join("not-yet-created/file")), true),
            (s(other.join("x")), false),
            (s(root.join("distro-other")), false),
        ];
        for (grant, want) in cases {
            let policy = SandboxPolicy {
                fs_write: vec![grant.clone()],
                ..Default::default()
            };
            assert_eq!(
                grant_under(&policy, &distro).is_some(),
                want,
                "grant={grant}"
            );
        }
    }

    /// Spec: the duplicate-root probe compares (dev, ino). A directory matches
    /// itself through any spelling (a symlink here), not a different directory,
    /// and a missing path never matches.
    #[cfg(unix)]
    #[test]
    fn same_inode_compares_device_and_inode() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a");
        let b = tmp.path().join("b");
        std::fs::create_dir(&a).unwrap();
        std::fs::create_dir(&b).unwrap();
        let alias = tmp.path().join("alias");
        std::os::unix::fs::symlink(&a, &alias).unwrap();
        assert!(same_inode(&a, &alias));
        assert!(!same_inode(&a, &b));
        assert!(!same_inode(&a, &tmp.path().join("missing")));
    }

    #[test]
    fn perms_cutoff_is_0_5_0() {
        for (minor, want) in [(4, false), (5, true), (9, true)] {
            let v = BwrapVersion {
                major: 0,
                minor,
                patch: 0,
            };
            assert_eq!(v.supports_perms(), want, "0.{minor}.0");
        }
    }
}
