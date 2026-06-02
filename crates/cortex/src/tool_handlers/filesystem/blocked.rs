//! Security blocklist enforcement: path-component matching with allow-list
//! override and symlink resolution. Shared by `patch`, `listing`, and `read`.
//!
//! `is_control_plane_write_denied` is a separate, non-configurable hard
//! invariant that blocks writes into Borg's own data directory (DB file, KEK
//! material, lifecycle-hooks config, sockets). User config cannot weaken or
//! override it — a prompt-injected agent rewriting `~/.borg/borg.db`,
//! `~/.borg/secrets/`, or `~/.borg/hooks.json` would silently subvert every
//! other security boundary.
//!
//! `shell_command_targets_control_plane` is the paired shell-side guard: the
//! file-write deny only covers the `fs_write`/`apply_patch` tools, so without
//! it a `run_shell` `echo >`, `tee`, `sed -i`, `cp`/`mv`, or `sqlite3` would
//! walk straight past the file-write boundary.

/// Check whether `path` is denied by the configured blocklist, taking the
/// allow list into account.
///
/// Matching rules:
/// 1. The path is canonicalized first (resolves symlinks, `..`, and
///    tilde-expanded entries on the allow list). If canonicalization fails
///    for a path that doesn't exist, the original path is used — this is
///    fine because nonexistent paths are handled separately by callers.
/// 2. If the canonical path starts with any entry in `allowed_paths`,
///    access is allowed regardless of blocklist matches. This is the
///    escape hatch for specific `.env` files or `.aws/` directories that
///    the user legitimately wants the agent to read.
/// 3. Otherwise, access is denied if **any path component** of the
///    canonical path matches an entry in `blocked`. Blocked entries may be
///    single components (e.g. `.ssh`) or multi-component suffixes (e.g.
///    `.config/gh`) — both forms are supported.
///
/// This deliberately does **not** jail access to `$HOME` or CWD: absolute
/// paths to non-sensitive files elsewhere on the system
/// (`/etc/hosts`, `/Users/you/other-project/...`, `../sibling/file.rs`)
/// remain fully accessible, as does cross-directory navigation via `@`.
pub fn is_blocked_path(path: &std::path::Path, blocked: &[String], allowed: &[String]) -> bool {
    // Canonicalize so we make decisions on the real on-disk path. If
    // canonicalization fails (path doesn't exist, permissions), fall back to
    // the literal path — callers handle nonexistent paths separately.
    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());

    // 1. Allow list: if the canonical path is under any allowed prefix,
    //    short-circuit to allow. Entries support tilde expansion.
    for raw_entry in allowed {
        let expanded = crate::util::unix_ext::expand_tilde(raw_entry);
        let entry_path = std::path::PathBuf::from(&expanded);
        // Canonicalize the allow entry too, so `~/work/.aws` and
        // `/Users/me/work/.aws` compare equal.
        let entry_canonical = entry_path.canonicalize().unwrap_or(entry_path);
        if canonical.starts_with(&entry_canonical) {
            return false;
        }
    }

    // 2. Block list: match any component or multi-component suffix against
    //    the canonical path's components.
    for raw_entry in blocked {
        if path_contains_blocked_entry(&canonical, raw_entry) {
            return true;
        }
    }

    false
}

/// Returns `true` if `path` contains `entry` as a contiguous sequence of
/// components. `entry` may itself be multi-component (e.g. `.config/gh`).
fn path_contains_blocked_entry(path: &std::path::Path, entry: &str) -> bool {
    // Split the entry into components, tolerating both forward slashes and
    // the platform separator.
    let entry_components: Vec<&std::ffi::OsStr> = std::path::Path::new(entry)
        .components()
        .filter_map(|c| match c {
            std::path::Component::Normal(s) => Some(s),
            _ => None,
        })
        .collect();

    if entry_components.is_empty() {
        return false;
    }

    let path_components: Vec<&std::ffi::OsStr> = path
        .components()
        .filter_map(|c| match c {
            std::path::Component::Normal(s) => Some(s),
            _ => None,
        })
        .collect();

    if path_components.len() < entry_components.len() {
        return false;
    }

    // Sliding window match.
    for start in 0..=path_components.len() - entry_components.len() {
        let window = &path_components[start..start + entry_components.len()];
        if window == entry_components.as_slice() {
            return true;
        }
    }

    false
}

/// Canonicalize a path that may not exist yet (the `apply_patch` add-file
/// case). `Path::canonicalize` fails for missing leaves; the workaround is to
/// canonicalize the *parent* and re-attach the file name. If even the parent
/// is absent (an attacker patching `~/.borg/new-dir/borg.db` where `new-dir`
/// doesn't exist), fall back to expanding tilde + lexical normalization so a
/// raw `~/.borg/borg.db` or `./../.borg/borg.db` still resolves close enough
/// for the control-plane comparison to fire.
fn canonicalize_write_target(path: &std::path::Path) -> std::path::PathBuf {
    if let Ok(real) = path.canonicalize() {
        return real;
    }
    if let (Some(parent), Some(name)) = (path.parent(), path.file_name()) {
        if let Ok(real_parent) = parent.canonicalize() {
            return real_parent.join(name);
        }
    }
    // No part of the path exists yet. Tilde-expand + lexically normalize
    // components so `~/.borg/borg.db` and `~/foo/../.borg/borg.db` still
    // collapse to the literal `$HOME/.borg/borg.db` for comparison.
    let expanded = crate::util::unix_ext::expand_tilde(&path.to_string_lossy());
    let mut out = std::path::PathBuf::new();
    for comp in std::path::Path::new(&expanded).components() {
        match comp {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Hard-coded write deny for Borg control-plane files inside `$BORG_HOME`.
///
/// Returns `true` if `path` resolves (after `canonicalize`) to:
/// - the SQLite DB file or its WAL/SHM siblings,
/// - any path under `$BORG_HOME/secrets/`,
/// - the lifecycle-hooks config `hooks.json` (executes arbitrary shell at every
///   hook point, so it is as security-sensitive as the config DB),
/// - the gRPC sockets, pairing socket, daemon PID file, or scheduler lock.
///
/// Resolution uses `canonicalize` on both sides so `..` traversal and symlink
/// chains cannot bypass the check. Falls back to the literal path if
/// canonicalize fails (path doesn't exist yet) — the agent shouldn't be
/// creating new files at those locations either.
///
/// Operator escape hatch is intentional absence: there is none. Use the
/// daemon's gRPC API or shell out manually if you need to touch control-plane
/// files from outside the agent loop.
pub fn is_control_plane_write_denied(path: &std::path::Path) -> bool {
    let Ok(home) = crate::paths::resolve_borg_home_readonly(None) else {
        return false;
    };
    let home = home.canonicalize().unwrap_or(home);
    let target = canonicalize_write_target(path);

    // SQLite DB plus its WAL/SHM siblings — `borg.db`, `borg.db-wal`, `borg.db-shm`.
    for sibling in ["borg.db", "borg.db-wal", "borg.db-shm"] {
        if target == home.join(sibling) {
            return true;
        }
    }

    // Whole-tree denies for secret material.
    if target.starts_with(home.join("secrets")) {
        return true;
    }

    // Lifecycle-hooks config plus sockets / pidfile / lockfile at the home
    // root. `hooks.json` runs arbitrary shell at every hook point, so it is the
    // security-policy file an agent must not be able to rewrite.
    for name in [
        "hooks.json",
        "borgd.sock",
        "borgd-pair.sock",
        "borgd.pid",
        "scheduler.tick.lock",
    ] {
        if target == home.join(name) {
            return true;
        }
    }

    false
}

/// Conventional control-plane file leaves under `$BORG_HOME`. `borg.db`'s
/// WAL/SHM siblings are covered by the `borg.db` substring; `secrets` matches
/// the whole KEK subtree.
const CONTROL_PLANE_LEAVES: &[&str] = &["hooks.json", "borg.db", "secrets"];

/// Substrings whose presence in a shell token means the token points at one of
/// Borg's own control-plane files. Built once (process-lifetime) from the
/// resolved `$BORG_HOME` *and* the conventional `.borg/<leaf>` suffix, so a
/// custom home, a `~/.borg/...` path, and a bare `.borg/...` relative path are
/// all caught. `$BORG_HOME` is fixed for the life of the process, so this is
/// cached to keep a `canonicalize` syscall and six allocations off the
/// per-shell-command hot path.
static CONTROL_PLANE_NEEDLES: std::sync::LazyLock<Vec<String>> = std::sync::LazyLock::new(|| {
    let mut needles = Vec::new();
    if let Ok(home) = crate::paths::resolve_borg_home_readonly(None) {
        let home = home.canonicalize().unwrap_or(home);
        for leaf in CONTROL_PLANE_LEAVES {
            needles.push(home.join(leaf).to_string_lossy().into_owned());
        }
    }
    for leaf in CONTROL_PLANE_LEAVES {
        needles.push(format!(".borg/{leaf}"));
    }
    needles
});

/// True if `token`, after tilde expansion, references a control-plane file.
/// Expansion (and its allocation) is skipped for the common case of a token
/// without a leading `~`, where `expand_tilde` is a no-op.
fn token_targets_control_plane(token: &str) -> bool {
    let expanded = if token.starts_with('~') {
        std::borrow::Cow::Owned(crate::util::unix_ext::expand_tilde(token))
    } else {
        std::borrow::Cow::Borrowed(token)
    };
    CONTROL_PLANE_NEEDLES
        .iter()
        .any(|n| expanded.contains(n.as_str()))
}

/// Basename of a command word, stripping any directory prefix (`/usr/bin/borg`
/// → `borg`, `./borg` → `borg`) so an absolute/relative path can't dodge a
/// verb match.
fn command_basename(word: &str) -> &str {
    word.rsplit('/').next().unwrap_or(word)
}

/// Shell-side companion to [`is_control_plane_write_denied`]: returns `true`
/// when `command` shows **write intent** against a Borg control-plane file, or
/// mutates security configuration via the CLI.
///
/// Without this paired check a prompt-injected agent could rewrite
/// `~/.borg/hooks.json` (arbitrary shell at every hook point), corrupt the
/// config DB, or overwrite KEK material with a plain `echo >`, `tee`, `sed -i`,
/// `cp`/`mv`, `dd`, `truncate`, or `sqlite3` — and could flip a security
/// setting with `borg settings set`/`unset`. Reads (`cat`, `borg settings
/// get`) stay allowed.
///
/// Best-effort, like [`crate::policy`]: the sandbox is the real boundary. This
/// is defense-in-depth that mirrors the existing `tee /etc/passwd` denials. The
/// `;`/`&&`/`||`/newline fragment split is shared with the policy layer via
/// [`crate::policy::command_fragments`] (pipelines stay whole so `… | tee <cp>`
/// is seen).
pub fn shell_command_targets_control_plane(command: &str) -> bool {
    crate::policy::command_fragments(command)
        .into_iter()
        .any(fragment_mutates_control_plane)
}

fn fragment_mutates_control_plane(frag: &str) -> bool {
    // Redirect: a control-plane path on the *write* side of `>` / `>>`. Reading
    // a control-plane file into a redirect elsewhere (`cat <cp> > /tmp/x`) is
    // not flagged because the needle sits before the operator. Checked on the
    // whole fragment since the operator binds within a pipeline stage anyway.
    if redirect_target_is_control_plane(frag) {
        return true;
    }

    // Inspect each pipeline stage on its own, so the writing command can sit
    // anywhere in `echo x | tee <cp>` or `cat x | dd of=<cp>`.
    frag.split('|').any(stage_verb_mutates_control_plane)
}

/// Per-pipeline-stage verb analysis: `borg settings set/unset`, `sqlite3` on
/// the DB, and the in-place / destination-mutating file verbs.
fn stage_verb_mutates_control_plane(stage: &str) -> bool {
    let toks: Vec<&str> = stage.split_whitespace().collect();
    if toks.is_empty() {
        return false;
    }

    // `borg settings set|unset …` — mutating the security config via the CLI.
    // `borg settings get|list` stay allowed.
    for w in toks.windows(3) {
        if command_basename(w[0]) == "borg" && w[1] == "settings" && matches!(w[2], "set" | "unset")
        {
            return true;
        }
    }

    let any_ref = || toks.iter().any(|t| token_targets_control_plane(t));
    match command_basename(toks[0]) {
        // `sqlite3 <control-plane db>` — the encrypted DB has no legitimate
        // agent use and sqlite3 opens read-write by default.
        "sqlite3" => any_ref(),
        // `tee`/`truncate` overwrite their referenced file args.
        "tee" | "truncate" => any_ref(),
        // `sed -i` / `sed -i.bak` / `sed --in-place` mutate the file in place.
        "sed"
            if toks
                .iter()
                .any(|t| t.starts_with("-i") || *t == "--in-place") =>
        {
            any_ref()
        }
        "dd" => toks.iter().any(|t| {
            t.strip_prefix("of=")
                .is_some_and(token_targets_control_plane)
        }),
        // `cp`/`mv`/`install` overwrite their *final* argument (the
        // destination), so a control-plane *source* (`cp <cp> /tmp`) is not
        // flagged.
        "cp" | "mv" | "install" => toks.last().is_some_and(|d| token_targets_control_plane(d)),
        _ => false,
    }
}

/// Report whether any `>`/`>>` redirect *target* in `frag` references a
/// control-plane file. Splitting on `>` puts the word right after each redirect
/// operator at the head of the following segment; a control-plane path *before*
/// a redirect (`cat <cp> > /tmp/x`) is a read and stays in the prior segment, so
/// it is not flagged. `>&` fd-dup targets (`&1`) never match a path needle.
fn redirect_target_is_control_plane(frag: &str) -> bool {
    frag.split('>').skip(1).any(|seg| {
        seg.split_whitespace()
            .next()
            .is_some_and(token_targets_control_plane)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serializes the tests that mutate the process-global `BORG_HOME` env var.
    /// `set_var`/`remove_var` are process-wide, so two such tests running on
    /// parallel threads can clobber each other's home mid-assertion (observed:
    /// one test's `remove_var` racing another's resolution). Hold this for the
    /// duration of any test that touches `BORG_HOME`.
    static BORG_HOME_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn is_blocked_path_matches_blocked_dir() {
        let home = dirs::home_dir().unwrap();
        let path = home.join(".ssh/id_rsa");
        let blocked = vec![".ssh".to_string()];
        assert!(is_blocked_path(&path, &blocked, &[]));
    }

    #[test]
    fn is_blocked_path_rejects_non_blocked() {
        let home = dirs::home_dir().unwrap();
        let path = home.join("Documents/safe.txt");
        let blocked = vec![".ssh".to_string(), ".aws".to_string()];
        assert!(!is_blocked_path(&path, &blocked, &[]));
    }

    #[test]
    fn is_blocked_path_nested_blocked() {
        let home = dirs::home_dir().unwrap();
        let path = home.join(".aws/credentials/secret");
        let blocked = vec![".aws".to_string()];
        assert!(is_blocked_path(&path, &blocked, &[]));
    }

    #[test]
    fn is_blocked_path_empty_blocked_list() {
        let home = dirs::home_dir().unwrap();
        let path = home.join(".ssh/id_rsa");
        let blocked: Vec<String> = vec![];
        assert!(!is_blocked_path(&path, &blocked, &[]));
    }

    #[test]
    fn is_blocked_path_outside_home() {
        let blocked = vec![".ssh".to_string()];
        let path = std::path::Path::new("/tmp/.ssh/id_rsa");
        assert!(is_blocked_path(path, &blocked, &[]));
    }

    #[test]
    fn is_blocked_path_allowed_overrides_blocked() {
        let home = dirs::home_dir().unwrap();
        let path = home.join(".env.example");
        let blocked = vec![".env".to_string()];
        // The path has ".env" as a substring of the component ".env.example"
        // but component matching should only match exact components
        assert!(!is_blocked_path(&path, &blocked, &[]));
    }

    #[test]
    fn is_blocked_path_absolute_path_not_under_home() {
        let blocked = vec![".aws".to_string()];
        let path = std::path::Path::new("/etc/hosts");
        assert!(!is_blocked_path(path, &blocked, &[]));
    }

    #[test]
    #[cfg(unix)]
    fn control_plane_denies_db_and_secrets_and_resolves_traversal() {
        // Point $BORG_HOME at a tempdir so the canonical comparison resolves
        // to a real path, then assert: (a) DB + WAL/SHM denied, (b) secrets/
        // subtree denied, (c) `..` traversal cannot escape the deny, (d) a
        // symlink into a denied region is also denied via canonicalize, and
        // (e) ordinary user paths under home are unaffected.
        let _env = BORG_HOME_GUARD.lock().unwrap_or_else(|p| p.into_inner());
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().canonicalize().expect("canonicalize tempdir");
        std::env::set_var("BORG_HOME", &home);
        std::fs::create_dir_all(home.join("secrets")).expect("mkdir secrets");
        std::fs::write(home.join("borg.db"), b"db").expect("seed db");

        assert!(is_control_plane_write_denied(&home.join("borg.db")));
        assert!(is_control_plane_write_denied(&home.join("borg.db-wal")));
        assert!(is_control_plane_write_denied(&home.join("borg.db-shm")));
        assert!(is_control_plane_write_denied(&home.join("secrets/kek.enc")));
        assert!(is_control_plane_write_denied(
            &home.join("secrets/sub/nested.bin")
        ));
        assert!(is_control_plane_write_denied(&home.join("borgd.sock")));
        // hooks.json executes arbitrary shell at every hook point, so it is as
        // security-sensitive as the DB and must be write-denied too.
        assert!(is_control_plane_write_denied(&home.join("hooks.json")));

        // `..`-traversal that still lands on the DB resolves to the DB path.
        let traversal = home
            .join("../")
            .join(home.file_name().unwrap())
            .join("borg.db");
        assert!(is_control_plane_write_denied(&traversal));

        // Symlink whose target is the real DB file must be denied.
        use std::os::unix::fs::symlink;
        let outside = tmp.path().join("dangling_link");
        symlink(home.join("borg.db"), &outside).expect("symlink");
        assert!(is_control_plane_write_denied(&outside));

        // Ordinary file under home — not a control-plane file — is allowed.
        assert!(!is_control_plane_write_denied(&home.join("notes.md")));

        std::env::remove_var("BORG_HOME");
    }

    #[test]
    #[cfg(unix)]
    fn control_plane_blocks_writes_to_files_that_do_not_exist_yet() {
        // Regression: `canonicalize` fails for missing leaves, which is
        // exactly the `apply_patch` add-file scenario. The previous fallback
        // returned the literal raw path and skipped every comparison, so a
        // prompt-injected agent could create `~/.borg/borg.db` (or a
        // not-yet-existing subdir under `secrets/`) and bypass the deny.
        let _env = BORG_HOME_GUARD.lock().unwrap_or_else(|p| p.into_inner());
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().canonicalize().expect("canonicalize tempdir");
        std::env::set_var("BORG_HOME", &home);
        std::fs::create_dir_all(home.join("secrets")).expect("mkdir secrets");

        // Files that don't exist but live under canonical control-plane paths.
        assert!(is_control_plane_write_denied(
            &home.join("borg.db") // no file yet
        ));
        assert!(is_control_plane_write_denied(
            &home.join("secrets").join("new-kek.enc")
        ));
        // Even when the leaf's parent doesn't exist yet.
        assert!(is_control_plane_write_denied(
            &home.join("secrets").join("not-yet-a-dir").join("kek.enc")
        ));

        // Lexical traversal through a non-existent prefix still resolves.
        let traversal = home.join("ghost").join("..").join("borg.db");
        assert!(is_control_plane_write_denied(&traversal));

        std::env::remove_var("BORG_HOME");
    }

    #[test]
    fn shell_guard_denies_control_plane_writes() {
        // Write-intent against a control-plane file (hooks.json / config DB /
        // secrets) on the shell side must be refused, regardless of the verb
        // used to express the write. These all rely on the always-present
        // `.borg/<leaf>` suffix needles, so they don't depend on $BORG_HOME.
        for cmd in [
            "echo pwned > ~/.borg/hooks.json",
            "echo pwned >> ~/.borg/hooks.json",
            "echo x | tee ~/.borg/hooks.json",
            "cp /tmp/evil ~/.borg/hooks.json",
            "mv /tmp/evil ~/.borg/borg.db",
            "sed -i 's/a/b/' ~/.borg/hooks.json",
            "sed -i.bak s/a/b/ ~/.borg/hooks.json",
            "dd if=/dev/zero of=~/.borg/borg.db",
            "truncate -s0 ~/.borg/secrets/kek.enc",
            "sqlite3 ~/.borg/borg.db 'UPDATE settings SET v=0'",
            "/usr/local/bin/borg settings set security.blocked_paths []",
            "borg settings unset mcp.allow_local_network",
            // Chained: the control-plane write hides after a benign prefix.
            "ls && echo x > ~/.borg/hooks.json",
        ] {
            assert!(
                shell_command_targets_control_plane(cmd),
                "expected control-plane guard to fire for: {cmd:?}"
            );
        }
    }

    #[test]
    fn shell_guard_allows_reads_and_unrelated_writes() {
        // Reads of control-plane files, settings *reads*, and writes that only
        // touch non-control-plane paths must keep auto-approving — over-blocking
        // here would break routine agent workflows.
        for cmd in [
            "cat ~/.borg/hooks.json",
            "borg settings get security.blocked_paths",
            "borg settings list",
            "echo x > ./notes.md",
            "tee /tmp/out.log",
            "cargo test 2>&1 | tee /tmp/t.log",
            // Copying a control-plane file *out* (it's the source, not the
            // destination) is a read, not a policy mutation.
            "cp ~/.borg/hooks.json /tmp/backup.json",
            // Reading the DB into a redirect elsewhere is not a write to it.
            "cat ~/.borg/borg.db > /tmp/db.bak",
        ] {
            assert!(
                !shell_command_targets_control_plane(cmd),
                "control-plane guard should NOT fire for benign command: {cmd:?}"
            );
        }
    }

    #[test]
    #[cfg(unix)]
    fn is_blocked_path_resolves_symlink_chain_into_blocked() {
        // Regression guard: protect against an attacker creating a chain of
        // symlinks (`a -> b -> .ssh/id_rsa`) under a non-blocked directory and
        // having the agent read the chain as if it were a benign file. This
        // pins the realpath-style resolution we rely on — if a future change
        // swaps `canonicalize()` for a single-hop resolver, this fails loudly.
        use std::os::unix::fs::symlink;
        let tmp = tempfile::tempdir().expect("tempdir");
        let blocked_dir = tmp.path().join(".ssh");
        std::fs::create_dir_all(&blocked_dir).expect("mkdir .ssh");
        let target = blocked_dir.join("id_rsa");
        std::fs::write(&target, b"PRIVATE").expect("seed target");

        let mid = tmp.path().join("mid_link");
        let entry = tmp.path().join("entry_link");
        symlink(&target, &mid).expect("mid symlink");
        symlink(&mid, &entry).expect("entry symlink");

        let blocked = vec![".ssh".to_string()];
        assert!(
            is_blocked_path(&entry, &blocked, &[]),
            "two-hop symlink into .ssh must canonicalize and be blocked"
        );
    }
}
