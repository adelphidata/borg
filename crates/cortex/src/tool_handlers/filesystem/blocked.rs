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
/// 1. The path is resolved in TWO forms and denied if **either** matches the
///    blocklist, so neither a symlinked parent nor a symlink named like a
///    blocked component can slip a write past the guard:
///    - `resolved`: the deepest existing ancestor canonicalized (resolving
///      symlinks and tilde) with the possibly-not-yet-existing tail
///      re-attached and `..` normalized. This catches an `apply_patch`
///      add-file whose parent is a symlink into a blocked dir — a naive
///      `canonicalize` fails on the missing leaf and would leave the parent
///      unresolved.
///    - `literal`: the tilde-expanded, lexically-normalized path with symlink
///      names intact, so a symlink whose own name is a blocked component but
///      which resolves elsewhere is still caught by name.
/// 2. If the resolved path starts with any entry in `allowed_paths`,
///    access is allowed regardless of blocklist matches. This is the
///    escape hatch for specific `.env` files or `.aws/` directories that
///    the user legitimately wants the agent to read.
/// 3. Otherwise, access is denied if **any path component** of *either* the
///    resolved or the literal form (see rule 1) matches an entry in `blocked`.
///    Blocked entries may be single components (e.g. `.ssh`) or multi-component
///    suffixes (e.g. `.config/gh`) — both forms are supported.
///
/// This deliberately does **not** jail access to `$HOME` or CWD: absolute
/// paths to non-sensitive files elsewhere on the system
/// (`/etc/hosts`, `/Users/you/other-project/...`, `../sibling/file.rs`)
/// remain fully accessible, as does cross-directory navigation via `@`.
///
/// **Caveat for future writers:** a symlinked *leaf* (the final component is
/// itself a symlink pointing into a blocked dir) is NOT caught here — `resolved`
/// resolves the existing *prefix*, not the leaf's target. On the `apply_patch`
/// path this is covered downstream by the applicator (`coding`'s `reject_symlink`
/// plus `fs_atomic::atomic_write`, which renames over the leaf rather than
/// following it). Any new writer that consults `is_blocked_path` without going
/// through that applicator must reject symlinked leaves on its own.
pub fn is_blocked_path(path: &std::path::Path, blocked: &[String], allowed: &[String]) -> bool {
    // Resolve the real on-disk path even when the leaf (or an intermediate
    // dir) does not exist yet — the `apply_patch` add-file case — so a
    // symlinked parent leading into a blocked dir cannot slip a write past the
    // guard. Keep the name-preserving literal form too (see doc comment).
    let resolved = canonicalize_write_target(path);
    let literal = lexically_normalized(path);

    // 0. Unconditional pseudo-file deny (checked BEFORE the allow list, so an
    //    allow-list entry cannot re-expose it): sensitive `/proc/<pid>/…` reads
    //    that leak ASLR/secrets and never pass through the sandbox. Both the
    //    canonicalized and name-preserving forms are checked so a symlinked
    //    parent leading into `/proc` cannot slip past.
    if is_sensitive_pseudo_path(&resolved) || is_sensitive_pseudo_path(&literal) {
        return true;
    }

    // 1. Allow list: if the resolved path is under any allowed prefix,
    //    short-circuit to allow. Entries support tilde expansion.
    for raw_entry in allowed {
        let expanded = crate::util::unix_ext::expand_tilde(raw_entry);
        let entry_path = std::path::PathBuf::from(&expanded);
        // Canonicalize the allow entry too, so `~/work/.aws` and
        // `/Users/me/work/.aws` compare equal.
        let entry_canonical = entry_path.canonicalize().unwrap_or(entry_path);
        if resolved.starts_with(&entry_canonical) {
            return false;
        }
    }

    // 2. Block list: deny if either form contains a blocked component or
    //    multi-component suffix.
    for raw_entry in blocked {
        if path_contains_blocked_entry(&resolved, raw_entry)
            || path_contains_blocked_entry(&literal, raw_entry)
        {
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

    // The last entry component is filename-like; all preceding ones must match
    // exactly.
    let Some((entry_last, entry_head)) = entry_components.split_last() else {
        return false;
    };

    // Sliding window match. Preceding components match exactly; the final
    // (filename-like) component also matches a "family" suffix so a blocked
    // `.env` / `private_key` / `credentials` entry covers `.env.local`,
    // `private_key.pem`, `credentials.json` etc. A component-suffix rule
    // (`entry` followed by a `.`) is used rather than a substring match so
    // unrelated files like `.environment` or `foo.ssh.txt` are not over-blocked.
    for start in 0..=path_components.len() - entry_components.len() {
        let window = &path_components[start..start + entry_components.len()];
        let Some((win_last, win_head)) = window.split_last() else {
            continue;
        };
        if win_head == entry_head && component_matches_family(win_last, entry_last) {
            return true;
        }
    }

    false
}

/// Whether a path component matches a blocked entry component, either exactly or
/// as a dotted "family" member (`entry` immediately followed by `.` and any
/// suffix). Family matching lets `.env` cover `.env.local`, `private_key` cover
/// `private_key.pem`, and `credentials` cover `credentials.json`, without the
/// false positives a substring match would produce (`.environment`, `.sshrc`).
fn component_matches_family(component: &std::ffi::OsStr, entry: &std::ffi::OsStr) -> bool {
    if component == entry {
        return true;
    }
    match (component.to_str(), entry.to_str()) {
        (Some(c), Some(e)) => c
            .strip_prefix(e)
            .is_some_and(|rest| rest.starts_with('.') && rest.len() > 1),
        _ => false,
    }
}

/// Canonicalize a path that may not exist yet (the `apply_patch` add-file
/// case), resolving symlinks anywhere in the existing prefix — including a
/// symlinked parent or grand-parent that leads into a blocked or control-plane
/// directory.
///
/// Delegates to [`crate::execution_context::synthesize_canonical_for_write`],
/// the single walk-up-to-deepest-existing-ancestor resolver shared with the
/// workspace-containment guard, so the two security checks cannot diverge. That
/// helper requires an absolute path and can error if the existing prefix won't
/// canonicalize; in either case fall back to tilde-expand + lexical
/// normalization so a raw `~/.borg/borg.db` or `./../.borg/borg.db` still
/// resolves close enough (with symlink names preserved) for comparison.
///
/// A **relative** input skips symlink resolution entirely (straight to lexical
/// normalization). This is not a live gap: every production caller anchors to an
/// absolute path before reaching here (`resolve_path_for_write` and friends), and
/// `synthesize_canonical_for_write` bails on relative by contract. Relative input
/// is exercised only by unit tests of the lexical fallback.
fn canonicalize_write_target(path: &std::path::Path) -> std::path::PathBuf {
    if path.is_absolute() {
        if let Ok(resolved) = crate::execution_context::synthesize_canonical_for_write(path) {
            return resolved;
        }
    }
    lexically_normalized(path)
}

/// Tilde-expand `path` and lexically normalize it (drop `.`, resolve `..`)
/// **without** touching the filesystem, so symlink names are preserved. Used
/// as the name-preserving form in [`is_blocked_path`] and as the last-resort
/// fallback in [`canonicalize_write_target`] when no ancestor exists on disk.
fn lexically_normalized(path: &std::path::Path) -> std::path::PathBuf {
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

/// `/proc/<pid>/…` pseudo-files that leak ASLR layout, raw process memory, or
/// secret environment/arguments. Reads of these are denied unconditionally —
/// independent of the configured `blocked_paths` — because `read_file` reads via
/// `std::fs` and never traverses the Linux sandbox, so a prompt-injected agent
/// could otherwise read `/proc/self/maps` to defeat ASLR of the Borg process,
/// `/proc/<pid>/environ` for another process's secrets, or `/proc/<pid>/mem`.
const SENSITIVE_PROC_LEAVES: &[&str] = &[
    "maps",
    "smaps",
    "smaps_rollup",
    "numa_maps",
    "mem",
    "environ",
    "cmdline",
    "auxv",
    "pagemap",
    "stack",
    "syscall",
    "wchan",
];

/// True if `path` targets a sensitive `/proc/<pid>/…` pseudo-file (see
/// [`SENSITIVE_PROC_LEAVES`]). Requires an **absolute** path rooted at `/proc`
/// so a real `/proc/self/maps` matches while a user directory that merely
/// happens to be named `proc` (`~/proc/maps` → `/Users/me/proc/maps`, or a
/// relative `proc/maps` in a project) does not. Keying on the leaf name plus a
/// `/proc` root also covers the per-thread alias `/proc/<pid>/task/<tid>/mem`,
/// which shares the same sensitive leaves at a deeper depth.
fn is_sensitive_pseudo_path(path: &std::path::Path) -> bool {
    let mut comps = path.components();
    if comps.next() != Some(std::path::Component::RootDir) {
        return false;
    }
    if comps.next() != Some(std::path::Component::Normal(std::ffi::OsStr::new("proc"))) {
        return false;
    }
    match path.file_name().and_then(|s| s.to_str()) {
        Some(leaf) => SENSITIVE_PROC_LEAVES.contains(&leaf),
        None => false,
    }
}

/// Case-insensitive basenames of agent-instruction files that are read into
/// future system prompts (see `crate::project_doc`) — plus the equivalent
/// files of other agent tooling the user may keep in the same tree. A write to
/// any of these persists into every later turn, which makes them the natural
/// persistence target for a prompt injection. Matched on the final path
/// component in ANY directory, so `.claude/CLAUDE.md`-style nesting is covered
/// automatically.
const INSTRUCTION_FILE_BASENAMES: &[&str] = &[
    "agents.md",
    "claude.md",
    "gemini.md",
    ".cursorrules",
    ".clinerules",
    ".windsurfrules",
];

/// Write deny for agent-instruction files, gated by
/// `security.allow_instruction_writes` (default false).
///
/// Both the canonicalized form (symlinks and `..` resolved via
/// [`canonicalize_write_target`], same double-check pattern as
/// [`is_blocked_path`]) and the name-preserving literal form are matched, so
/// neither a `sub/../AGENTS.md` traversal, a symlink whose target is an
/// instruction file, nor a symlink *named* like one slips past. Reads are
/// unaffected — only write paths consult this.
pub fn is_instruction_file_write_denied(path: &std::path::Path) -> bool {
    let resolved = canonicalize_write_target(path);
    let literal = lexically_normalized(path);
    [resolved.as_path(), literal.as_path()].iter().any(|p| {
        p.file_name().and_then(|n| n.to_str()).is_some_and(|name| {
            let lower = name.to_ascii_lowercase();
            INSTRUCTION_FILE_BASENAMES.contains(&lower.as_str())
        })
    })
}

/// Denial message for an instruction-file write: short, actionable, and
/// returned as a normal tool result (never a panic) so the model can read it.
pub(crate) fn instruction_write_denied_message(raw_path: &str) -> String {
    format!(
        "Access denied: {raw_path} is an agent-instruction file that steers future \
         agent behavior. Ask the user to edit it themselves, or have them set \
         `security.allow_instruction_writes` to true."
    )
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

/// Bare leaf names that identify a control-plane file *by final path component
/// alone*. Only consulted once the command has `cd`'d into a control-plane
/// directory (see [`command_cwd_is_control_plane`]); matched by exact
/// final-component equality (not substring) so an unrelated token like
/// `my-secrets.txt` isn't swept up.
const CONTROL_PLANE_BARE_LEAVES: &[&str] = &[
    "hooks.json",
    "borg.db",
    "borg.db-wal",
    "borg.db-shm",
    "kek.enc",
    "secrets",
    "borgd.sock",
    "borgd-pair.sock",
    "borgd.pid",
    "scheduler.tick.lock",
];

/// Substrings that identify a control-plane *directory* — the resolved
/// `$BORG_HOME`, its `secrets/` subtree, and the conventional `.borg` /
/// `.borg/secrets` relative forms. Used to detect a `cd` into control-plane
/// territory so a subsequent bare-leaf write (`cd ~/.borg && … > hooks.json`)
/// is still caught.
static CONTROL_PLANE_DIR_NEEDLES: std::sync::LazyLock<Vec<String>> =
    std::sync::LazyLock::new(|| {
        let mut needles = Vec::new();
        if let Ok(home) = crate::paths::resolve_borg_home_readonly(None) {
            let home = home.canonicalize().unwrap_or(home);
            needles.push(home.to_string_lossy().into_owned());
            needles.push(home.join("secrets").to_string_lossy().into_owned());
        }
        needles.push(".borg/secrets".to_string());
        needles.push(".borg".to_string());
        needles
    });

/// True if `token`, after tilde expansion, references a control-plane file.
/// Expansion (and its allocation) is skipped for the common case of a token
/// without a leading `~`, where `expand_tilde` is a no-op. When
/// `include_bare_leaves` is set (the caller has established the command runs
/// with a control-plane cwd) a token whose *final path component* is a
/// control-plane leaf also matches.
fn token_targets_control_plane_ext(token: &str, include_bare_leaves: bool) -> bool {
    let expanded = if token.starts_with('~') {
        std::borrow::Cow::Owned(crate::util::unix_ext::expand_tilde(token))
    } else {
        std::borrow::Cow::Borrowed(token)
    };
    if CONTROL_PLANE_NEEDLES
        .iter()
        .any(|n| expanded.contains(n.as_str()))
    {
        return true;
    }
    if include_bare_leaves {
        // Strip a trailing slash (`secrets/`) then compare the final component.
        let trimmed = expanded.trim_end_matches('/');
        let final_component = trimmed.rsplit('/').next().unwrap_or(trimmed);
        if CONTROL_PLANE_BARE_LEAVES.contains(&final_component) {
            return true;
        }
    }
    false
}

/// True if any `cd <dir>` fragment in the command changes into a control-plane
/// directory, so subsequent bare-leaf writes (`> hooks.json`) must be treated
/// as control-plane targets. Coarse by design — a single `cd` anywhere flips
/// the whole command into bare-leaf mode; over-approximation here only ever
/// denies *writes* to files that share a control-plane leaf name, never reads.
fn command_cwd_is_control_plane(command: &str) -> bool {
    crate::policy::command_fragments(command)
        .into_iter()
        .any(|frag| {
            frag.split('|').any(|stage| {
                let toks: Vec<&str> = stage.split_whitespace().collect();
                let mut idx = 0;
                // Skip leading `VAR=value` assignments (`FOO=bar cd …`).
                while toks.get(idx).is_some_and(|t| {
                    t.split_once('=')
                        .is_some_and(|(k, _)| !k.is_empty() && !k.contains('/'))
                }) {
                    idx += 1;
                }
                let Some(&verb) = toks.get(idx) else {
                    return false;
                };
                if command_basename(verb) != "cd" {
                    return false;
                }
                toks.get(idx + 1).is_some_and(|target| {
                    let expanded = if target.starts_with('~') {
                        std::borrow::Cow::Owned(crate::util::unix_ext::expand_tilde(target))
                    } else {
                        std::borrow::Cow::Borrowed(*target)
                    };
                    CONTROL_PLANE_DIR_NEEDLES
                        .iter()
                        .any(|n| expanded.contains(n.as_str()))
                })
            })
        })
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
    // A `cd` into `~/.borg` (or its `secrets/` subtree) means a subsequent bare
    // leaf (`> hooks.json`, `: > kek.enc`) still lands on a control-plane file,
    // so the whole command switches to final-component leaf matching.
    let bare_leaves = command_cwd_is_control_plane(command);
    crate::policy::command_fragments(command)
        .into_iter()
        .any(|frag| fragment_mutates_control_plane(frag, bare_leaves))
}

fn fragment_mutates_control_plane(frag: &str, bare_leaves: bool) -> bool {
    // Redirect: a control-plane path on the *write* side of `>` / `>>`. Reading
    // a control-plane file into a redirect elsewhere (`cat <cp> > /tmp/x`) is
    // not flagged because the needle sits before the operator. Checked on the
    // whole fragment since the operator binds within a pipeline stage anyway.
    if redirect_target_is_control_plane(frag, bare_leaves) {
        return true;
    }

    // Inspect each pipeline stage on its own, so the writing command can sit
    // anywhere in `echo x | tee <cp>` or `cat x | dd of=<cp>`.
    frag.split('|')
        .any(|stage| stage_verb_mutates_control_plane(stage, bare_leaves))
}

/// Per-pipeline-stage verb analysis: `borg settings set/unset`, `sqlite3` on
/// the DB, and the in-place / destination-mutating file verbs.
fn stage_verb_mutates_control_plane(stage: &str, bare_leaves: bool) -> bool {
    let raw: Vec<&str> = stage.split_whitespace().collect();
    if raw.is_empty() {
        return false;
    }

    // `borg settings set|unset …` — mutating the security config via the CLI.
    // `borg settings get|list` stay allowed. Checked on the raw tokens (before
    // wrapper stripping) so `borg` at any position is seen.
    for w in raw.windows(3) {
        if command_basename(w[0]) == "borg" && w[1] == "settings" && matches!(w[2], "set" | "unset")
        {
            return true;
        }
    }

    // An interpreter (`python -c 'open("…/.borg/hooks.json","w")…'`, `perl -e`,
    // `bash -c`) can open any referenced control-plane file for writing, and we
    // can't parse its intent from the outside. Treat any control-plane
    // reference from an interpreter stage as a denial — reads via `cat` stay
    // available, so the false-positive cost is a redirect the agent can phrase
    // differently.
    if crate::policy::stage_runs_interpreter(stage)
        && raw
            .iter()
            .any(|t| token_targets_control_plane_ext(t, bare_leaves))
    {
        return true;
    }

    // Strip leading privilege/no-op wrappers (`sudo`, `env`, `busybox`, …) so
    // `busybox tee <cp>` / `env dd of=<cp>` surface their real write verb.
    let mut owned: Vec<String> = raw.iter().map(ToString::to_string).collect();
    crate::policy::strip_leading_wrappers(&mut owned);
    let toks: Vec<&str> = owned.iter().map(String::as_str).collect();
    let Some(&first) = toks.first() else {
        return false;
    };

    let any_ref = || {
        toks.iter()
            .any(|t| token_targets_control_plane_ext(t, bare_leaves))
    };
    match command_basename(first) {
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
                .is_some_and(|p| token_targets_control_plane_ext(p, bare_leaves))
        }),
        // `cp`/`install` overwrite their *final* argument (the destination),
        // so a control-plane *source* (`cp <cp> /tmp`) is only a read and is
        // not flagged.
        "cp" | "install" => toks
            .last()
            .is_some_and(|d| token_targets_control_plane_ext(d, bare_leaves)),
        // `mv` is deletion-class on the *source* side too: `mv ~/.borg/borg.db
        // /tmp/x` removes the file from the control plane just as surely as
        // `rm` would. Match any referenced leaf or the data dir itself.
        "mv" => any_ref() || toks.iter().any(|t| token_targets_control_plane_dir(t)),
        // Deletion takes the control plane with it. Unlike the write verbs
        // above, these also match the control-plane *directory* — `rm -rf
        // ~/.borg` names no leaf, but removing the directory is the strictly
        // worse write, and the absolute-path form (`rm -rf /Users/x/.borg`)
        // dodges the `rm -rf ~*` policy globs entirely.
        "rm" | "unlink" | "shred" | "rmdir" => {
            any_ref() || toks.iter().any(|t| token_targets_control_plane_dir(t))
        }
        // `rsync --delete` prunes the destination to mirror the source, and
        // `--remove-source-files` deletes the source after transfer — both are
        // deletion by another name, so with either flag present any
        // control-plane reference (source or destination) is flagged.
        "rsync"
            if toks
                .iter()
                .any(|t| t.starts_with("--delete") || *t == "--remove-source-files") =>
        {
            any_ref() || toks.iter().any(|t| token_targets_control_plane_dir(t))
        }
        _ => false,
    }
}

/// True if `token` points at a control-plane *directory*: the resolved
/// `$BORG_HOME` (absolute needles) or any path with a literal `.borg`
/// component. Component matching — not substring — so `foo.borg`/`.borgrc`
/// don't trip it. The `.borg` component match deliberately over-approximates:
/// `rm -rf /some/other/project/.borg` trips it even though that dir is not
/// this agent's data dir, mirroring the same over-approximation the `cd`
/// tracking makes — a false deny on a foreign `.borg` dir is cheap, a false
/// allow on ours is not. Only deletion verbs consult this; the write verbs
/// need a file target and keep their leaf-needle matching.
fn token_targets_control_plane_dir(token: &str) -> bool {
    let expanded = if token.starts_with('~') {
        std::borrow::Cow::Owned(crate::util::unix_ext::expand_tilde(token))
    } else {
        std::borrow::Cow::Borrowed(token)
    };
    // Boundary-aware needle match: the home needle must be followed by `/` or
    // end-of-token, so `/Users/x/.borg` doesn't swallow `/Users/x/.borgrc`.
    let at_dir_boundary = |needle: &str| {
        expanded.match_indices(needle).any(|(i, _)| {
            expanded[i + needle.len()..]
                .chars()
                .next()
                .is_none_or(|c| c == '/')
        })
    };
    if CONTROL_PLANE_DIR_NEEDLES
        .iter()
        .filter(|n| n.starts_with('/'))
        .any(|n| at_dir_boundary(n))
    {
        return true;
    }
    expanded
        .trim_end_matches('/')
        .split('/')
        .any(|component| component == ".borg")
}

/// Report whether any `>`/`>>` redirect *target* in `frag` references a
/// control-plane file. Splitting on `>` puts the word right after each redirect
/// operator at the head of the following segment; a control-plane path *before*
/// a redirect (`cat <cp> > /tmp/x`) is a read and stays in the prior segment, so
/// it is not flagged. `>&` fd-dup targets (`&1`) never match a path needle.
fn redirect_target_is_control_plane(frag: &str, bare_leaves: bool) -> bool {
    frag.split('>').skip(1).any(|seg| {
        seg.split_whitespace()
            .next()
            .is_some_and(|t| token_targets_control_plane_ext(t, bare_leaves))
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
        let path = home.join(".env.local");
        let blocked = vec![".env".to_string()];
        // Family matching blocks `.env.local` by default (fail-secure)...
        assert!(is_blocked_path(&path, &blocked, &[]));
        // ...but an explicit allow entry for the file overrides the blocklist.
        let allowed = vec![path.to_string_lossy().to_string()];
        assert!(!is_blocked_path(&path, &blocked, &allowed));
    }

    #[test]
    fn is_blocked_path_family_match_covers_dotted_variants() {
        let home = dirs::home_dir().unwrap();
        // `.env` covers `.env.*`; `private_key`/`credentials` cover `*.pem`/`*.json`.
        let cases = [
            (".env", ".env.production"),
            (".env", ".env.local"),
            ("private_key", "private_key.pem"),
            ("credentials", "credentials.json"),
        ];
        for (entry, filename) in cases {
            let path = home.join(filename);
            assert!(
                is_blocked_path(&path, &[entry.to_string()], &[]),
                "{filename} should be blocked by family match on {entry}"
            );
        }
    }

    #[test]
    fn is_blocked_path_family_match_does_not_overblock() {
        let home = dirs::home_dir().unwrap();
        // Names that merely share a prefix (no dot boundary) must stay allowed.
        let cases = [
            (".env", ".environment"),
            (".env", ".envrc"), // covered by its own entry, NOT by `.env` family
            (".ssh", ".sshrc"),
            ("credentials", "credentialsstore"),
        ];
        for (entry, filename) in cases {
            let path = home.join(filename);
            assert!(
                !is_blocked_path(&path, &[entry.to_string()], &[]),
                "{filename} must NOT be blocked by family match on {entry}"
            );
        }
    }

    #[test]
    fn is_blocked_path_absolute_path_not_under_home() {
        let blocked = vec![".aws".to_string()];
        let path = std::path::Path::new("/etc/hosts");
        assert!(!is_blocked_path(path, &blocked, &[]));
    }

    #[test]
    fn is_blocked_path_denies_sensitive_proc_pseudo_files() {
        // /proc/<pid>/… memory/secret leakers must be denied regardless of the
        // configured blocklist — read_file bypasses the sandbox, so this is the
        // only guard. Covers `self`, a numeric pid, and the per-thread alias.
        let blocked: Vec<String> = vec![]; // empty on purpose: deny is unconditional
        let denied = [
            "/proc/self/maps",
            "/proc/self/smaps",
            "/proc/self/mem",
            "/proc/self/auxv",
            "/proc/self/pagemap",
            "/proc/1/environ",
            "/proc/1234/cmdline",
            "/proc/1234/task/5678/mem",
        ];
        for p in denied {
            assert!(
                is_blocked_path(std::path::Path::new(p), &blocked, &[]),
                "{p} must be denied"
            );
        }
    }

    #[test]
    fn is_blocked_path_allows_benign_proc_and_lookalikes() {
        // Non-sensitive /proc entries stay readable, and paths that merely
        // contain a `proc` component but are not the real /proc are not caught.
        let blocked: Vec<String> = vec![];
        let allowed = [
            "/proc/cpuinfo",          // benign, not a per-pid leaker
            "/proc/self/limits",      // not in the sensitive set
            "/Users/me/project/maps", // leaf named `maps` but not under /proc
            "/home/me/proc/self/mem", // `proc` mid-path, not the root
        ];
        for p in allowed {
            assert!(
                !is_blocked_path(std::path::Path::new(p), &blocked, &[]),
                "{p} must remain readable"
            );
        }
    }

    #[test]
    fn is_blocked_path_proc_deny_not_overridable_by_allowlist() {
        // The /proc deny is checked before the allow list, so even an explicit
        // allow entry for /proc cannot re-expose the sensitive pseudo-files.
        let allowed = vec!["/proc".to_string()];
        assert!(is_blocked_path(
            std::path::Path::new("/proc/self/maps"),
            &[],
            &allowed
        ));
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
    fn shell_guard_denies_cwd_relative_and_wrapped_and_interpreter_bypasses() {
        // Regression (H3): the control-plane guard must not be dodged by (a)
        // `cd`-ing into ~/.borg then writing a bare leaf, (b) prefixing the write
        // verb with a wrapper binary, or (c) driving the write through an
        // interpreter. All rely on the home-independent `.borg` relative needles.
        for cmd in [
            // (a) cwd-relative bare leaf after a cd into the control-plane dir.
            "cd ~/.borg && printf '{}' > hooks.json",
            "cd ~/.borg && echo x >> hooks.json",
            "cd ~/.borg/secrets && : > kek.enc",
            "cd ~/.borg && tee hooks.json",
            "cd ~/.borg/secrets && truncate -s0 kek.enc",
            // (b) wrapper binaries in front of the write verb.
            "busybox tee ~/.borg/hooks.json",
            "sudo tee ~/.borg/hooks.json",
            "env dd of=~/.borg/borg.db",
            "busybox dd of=~/.borg/secrets/kek.enc",
            // (c) interpreter opening a control-plane file for writing.
            "python3 -c 'open(\"~/.borg/hooks.json\",\"w\").write(x)'",
            "perl -e 'open(F,\">\",\"~/.borg/hooks.json\")'",
            "bash -c 'echo x > ~/.borg/hooks.json'",
        ] {
            assert!(
                shell_command_targets_control_plane(cmd),
                "expected control-plane guard to fire for bypass attempt: {cmd:?}"
            );
        }
    }

    #[test]
    fn shell_guard_cwd_relative_does_not_overblock() {
        // The cwd-into-control-plane heuristic must only catch *writes* to
        // control-plane leaf names — a read there, or a same-named file written
        // outside ~/.borg, stays allowed.
        for cmd in [
            // Read inside ~/.borg is fine (no write verb / redirect).
            "cd ~/.borg && cat hooks.json",
            // A file literally named hooks.json written outside the control
            // plane must not be swept up.
            "cd /tmp && echo x > hooks.json",
            // Unrelated write inside ~/.borg (leaf name is not control-plane).
            "cd ~/.borg && echo x > notes.md",
            // Interpreter with no control-plane reference stays allowed.
            "python3 -c 'print(1)'",
            // A token that merely contains 'secrets' is not a bare leaf.
            "cd ~/.borg && echo x > my-secrets.txt",
        ] {
            assert!(
                !shell_command_targets_control_plane(cmd),
                "control-plane guard should NOT fire for benign command: {cmd:?}"
            );
        }
    }

    #[test]
    fn shell_guard_denies_control_plane_deletion() {
        // Deleting control-plane files or the data dir itself is the strictly
        // worse write: `rm` was previously absent from the verb list, so
        // `rm ~/.borg/borg.db` (no -rf, dodging the policy globs) and the
        // absolute-path form of the home dir slipped through.
        for cmd in [
            "rm ~/.borg/borg.db",
            "rm -f ~/.borg/hooks.json",
            "rm -rf ~/.borg/secrets",
            "rm -rf ~/.borg",
            "rm -rf ~/.borg/",
            "unlink ~/.borg/hooks.json",
            "shred -u ~/.borg/secrets/kek.enc",
            "rmdir ~/.borg",
            // Relative and cd-based forms.
            "rm -rf .borg",
            "cd ~/.borg && rm hooks.json",
            "cd ~/.borg/secrets && rm -f kek.enc",
            // Wrapped / chained forms.
            "sudo rm -rf ~/.borg",
            "busybox rm ~/.borg/borg.db",
            "ls && rm -rf ~/.borg",
            // rsync --delete mirrors an (empty) source over the destination.
            "rsync -a --delete /tmp/empty/ ~/.borg/",
            "rsync -a --delete-after /tmp/empty/ ~/.borg/secrets",
            // Deletion by relocation: the source side leaves the control plane.
            "mv ~/.borg/borg.db /tmp/x",
            "mv ~/.borg /tmp/borg-backup",
            "rsync -a --remove-source-files ~/.borg/ /tmp/backup/",
        ] {
            assert!(
                shell_command_targets_control_plane(cmd),
                "expected control-plane guard to fire for deletion: {cmd:?}"
            );
        }
    }

    /// This guard fragments with `policy::command_fragments`, which drops the
    /// lines inside an inert heredoc body. A `<<` that is not really a
    /// redirection must therefore not read as a heredoc opener: an earlier
    /// version produced a phantom delimiter that matched no terminator and
    /// dropped every line after it, so a following `tee ~/.borg/hooks.json`
    /// was invisible here as well as to the catastrophic-command rules.
    ///
    /// A body a shell would actually execute must stay visible for the same
    /// reason.
    #[test]
    fn heredoc_handling_cannot_hide_a_control_plane_write() {
        for cmd in [
            "echo \"a << b\"\ntee ~/.borg/hooks.json",
            "# note <<EOF\ntee ~/.borg/hooks.json",
            "echo $((1 << 3))\ntee ~/.borg/hooks.json",
            "bash <<EOF\ntee ~/.borg/hooks.json\nEOF",
            "cat <<EOF | bash\ntee ~/.borg/hooks.json\nEOF",
        ] {
            assert!(
                shell_command_targets_control_plane(cmd),
                "heredoc handling must not hide a control-plane write: {cmd:?}"
            );
        }

        // The converse, so the guard is not just matching the substring: a
        // genuinely inert body really is treated as text.
        assert!(
            !shell_command_targets_control_plane(
                "cat > notes.txt <<'EOF'\ntee ~/.borg/hooks.json\nEOF"
            ),
            "text written into a file must not trip the control-plane guard"
        );
    }

    #[test]
    fn shell_guard_deletion_does_not_overblock() {
        // Deletion of unrelated paths — including names that merely *contain*
        // ".borg" as a substring — must keep auto-approving.
        for cmd in [
            "rm -rf /tmp/build",
            "rm foo.borg",
            "rm -rf ~/.borgrc",
            "rm -rf ./my.borg/cache",
            "unlink /tmp/x",
            "shred -u /tmp/secret-notes.txt",
            "rmdir /tmp/empty",
            // rsync without --delete only writes what it carries.
            "rsync -a /tmp/src/ /tmp/dst/",
            // rsync --delete elsewhere.
            "rsync -a --delete /tmp/a/ /tmp/b/",
            // mv of unrelated paths, including .borg-substring names.
            "mv /tmp/a /tmp/b",
            "mv foo.borg bar.borg",
            "mv ~/.borgrc /tmp/x",
            // rsync --remove-source-files elsewhere.
            "rsync -a --remove-source-files /tmp/a/ /tmp/b/",
        ] {
            assert!(
                !shell_command_targets_control_plane(cmd),
                "control-plane guard should NOT fire for benign deletion: {cmd:?}"
            );
        }
    }

    #[test]
    #[cfg(unix)]
    fn shell_guard_denies_absolute_home_deletion() {
        // `rm -rf /Users/<user>/.borg` names no `~` and no `.borg/<leaf>`
        // suffix, so it previously slipped both the policy globs and the leaf
        // needles. The resolved-home dir needles must catch it. Read the home
        // from the cached needles: they resolve once per process, so a fresh
        // `resolve_borg_home_readonly` here could disagree under parallel
        // tests that move $BORG_HOME.
        let Some(home) = CONTROL_PLANE_DIR_NEEDLES
            .iter()
            .find(|n| n.starts_with('/'))
            .cloned()
        else {
            return;
        };
        for cmd in [
            format!("rm -rf {home}"),
            format!("rm -rf {home}/"),
            format!("rm {home}/borg.db"),
            format!("shred -u {home}/secrets/kek.enc"),
        ] {
            assert!(
                shell_command_targets_control_plane(&cmd),
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
    fn is_blocked_path_denies_write_through_symlinked_parent_missing_leaf() {
        // Regression (security): an `apply_patch` add-file whose parent is a
        // symlink into a blocked dir must be denied even though the leaf does
        // not exist yet. `canonicalize` fails on the missing leaf; the old
        // literal fallback kept the un-resolved parent, so
        // `notes/docs/authorized_keys` (docs -> .ssh) slipped past the
        // blocklist and would have landed in ~/.ssh/authorized_keys.
        use std::os::unix::fs::symlink;
        let tmp = tempfile::tempdir().expect("tempdir");
        let blocked_dir = tmp.path().join(".ssh");
        std::fs::create_dir_all(&blocked_dir).expect("mkdir .ssh");
        let notes = tmp.path().join("notes");
        std::fs::create_dir_all(&notes).expect("mkdir notes");
        let docs = notes.join("docs");
        symlink(&blocked_dir, &docs).expect("symlink docs -> .ssh");

        let target = docs.join("authorized_keys");
        assert!(!target.exists(), "precondition: leaf must not exist yet");

        let blocked = vec![".ssh".to_string()];
        assert!(
            is_blocked_path(&target, &blocked, &[]),
            "add-file through a symlinked parent into .ssh must be blocked"
        );
    }

    #[test]
    #[cfg(unix)]
    fn is_blocked_path_denies_write_through_symlinked_ancestor_missing_intermediate() {
        // Deeper variant: the symlink is a grand-parent AND an intermediate dir
        // is missing, so resolving only the immediate parent would not reach
        // the blocked target. The walk-up to the deepest existing ancestor must
        // still resolve `data -> .aws` and block the write. A single-parent
        // resolver would fail this.
        use std::os::unix::fs::symlink;
        let tmp = tempfile::tempdir().expect("tempdir");
        let blocked_dir = tmp.path().join(".aws");
        std::fs::create_dir_all(&blocked_dir).expect("mkdir .aws");
        let link = tmp.path().join("data");
        symlink(&blocked_dir, &link).expect("symlink data -> .aws");

        // Neither `data/sub` nor the `credentials` leaf exists.
        let target = link.join("sub").join("credentials");
        let blocked = vec![".aws".to_string()];
        assert!(
            is_blocked_path(&target, &blocked, &[]),
            "add-file through a symlinked ancestor must be blocked despite a missing intermediate dir"
        );
    }

    #[test]
    #[cfg(unix)]
    fn is_blocked_path_denies_symlink_named_like_blocked_component() {
        // Defense-in-depth: a symlink whose own name matches a blocked
        // component (`.aws`) but which resolves to a harmless dir must still be
        // blocked by name — resolution alone would launder the blocked name
        // away. This exercises the literal (name-preserving) form.
        use std::os::unix::fs::symlink;
        let tmp = tempfile::tempdir().expect("tempdir");
        let harmless = tmp.path().join("harmless");
        std::fs::create_dir_all(&harmless).expect("mkdir harmless");
        let named = tmp.path().join(".aws");
        symlink(&harmless, &named).expect("symlink .aws -> harmless");

        let target = named.join("credentials");
        let blocked = vec![".aws".to_string()];
        assert!(
            is_blocked_path(&target, &blocked, &[]),
            "a symlink named like a blocked component must be blocked by name"
        );
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

    #[test]
    fn instruction_file_write_deny_table() {
        // Protected instruction-file basenames must be denied case-insensitively
        // in any directory; unrelated names must not be swept up. Table-driven
        // over (path, expected_denied).
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().canonicalize().expect("canonicalize");
        std::fs::create_dir_all(root.join("sub/deep")).expect("mkdir");
        let cases: &[(&str, bool)] = &[
            ("AGENTS.md", true),
            ("agents.md", true), // lowercase variant
            ("CLAUDE.md", true),
            ("sub/deep/AGENTS.md", true),    // nested dir
            ("sub/.claude/CLAUDE.md", true), // dot-dir nesting via basename
            ("sub/../AGENTS.md", true),      // traversal must not launder
            (".cursorrules", true),
            (".clinerules", true),
            (".windsurfrules", true),
            ("GEMINI.md", true),
            ("NOTES.md", false),      // unrelated markdown
            ("sub/README.md", false), // unrelated nested
            ("MYAGENTS.md", false),   // shares a suffix, not the basename
            ("agents.md.txt", false), // basename match is exact, not prefix
        ];
        for (rel, denied) in cases {
            let path = root.join(rel);
            assert_eq!(
                is_instruction_file_write_denied(&path),
                *denied,
                "unexpected verdict for {rel}"
            );
        }
    }

    #[test]
    #[cfg(unix)]
    fn instruction_file_write_deny_resolves_symlink() {
        // A symlink with an innocuous name whose target is an instruction file
        // must be denied via the canonicalized form — otherwise a prepared
        // symlink lets a write reach AGENTS.md under a laundered name.
        use std::os::unix::fs::symlink;
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().canonicalize().expect("canonicalize");
        let target = root.join("AGENTS.md");
        std::fs::write(&target, b"# rules").expect("seed");
        let link = root.join("innocuous.md");
        symlink(&target, &link).expect("symlink");
        assert!(
            is_instruction_file_write_denied(&link),
            "symlink to AGENTS.md must be denied via canonicalization"
        );
        // And the inverse: a symlink *named* AGENTS.md pointing elsewhere is
        // denied by the literal (name-preserving) form.
        let harmless = root.join("harmless.txt");
        std::fs::write(&harmless, b"x").expect("seed harmless");
        let named = root.join("sub_agents_dir");
        std::fs::create_dir_all(&named).expect("mkdir");
        let named_link = named.join("agents.md");
        symlink(&harmless, &named_link).expect("symlink named agents.md");
        assert!(is_instruction_file_write_denied(&named_link));
    }

    #[test]
    fn canonicalize_write_target_lexically_normalizes_when_no_ancestor_exists() {
        // Pure-lexical fallback: when no component of the path exists on disk
        // (a relative path with no real ancestor), `..` is resolved lexically
        // so it cannot pop away a blocked component. Guards the last-resort
        // branch that neither the walk-up nor the control-plane tests reach.
        let p = std::path::Path::new("no_such_dir_zqx/ghost/../.ssh/id_rsa");
        let out = canonicalize_write_target(p);
        assert!(
            out.components().any(|c| c.as_os_str() == ".ssh"),
            "lexical fallback must keep the blocked component: {out:?}"
        );
        assert!(
            !out.components().any(|c| c.as_os_str() == "ghost"),
            "`..` should have popped `ghost`: {out:?}"
        );
    }
}
