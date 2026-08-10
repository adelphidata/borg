//! Managed git worktrees: isolated checkouts the agent codes in.
//!
//! A worktree is a second working directory for the same repository, on its own
//! branch, with its own index. Coding inside one means an agent run never
//! touches the checkout the operator has open in their editor: no stashing, no
//! surprise branch switches, no half-applied patch on `main`.
//!
//! # Git is the source of truth
//!
//! Borg keeps **no** database table of worktrees. `git worktree list` already
//! records every checkout, survives an operator running `git worktree add` by
//! hand, and cannot drift from reality. A mirror table could — and the copy
//! that drifts is the one we would delete files from.
//!
//! What distinguishes a *managed* worktree from any other is only its location:
//! managed ones live under `<$BORG_HOME>-worktrees/<repo>/<name>` — a sibling
//! of the data dir, never inside it (see [`worktrees_root`]). That is a
//! path-prefix test ([`Worktree::is_managed`]), not a stored flag, so it stays
//! correct across a database restore or a fresh install.
//!
//! # Safety
//!
//! Removal is the only destructive operation here, and it refuses four cases
//! outright: the main checkout (that is the operator's repository), a worktree
//! holding uncommitted changes, a locked one, and any checkout Borg does not
//! manage when the target is a bare name or branch rather than a full path.
//! `force` overrides the dirty check only — the main checkout is never
//! removable through this module, at any force level.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use tracing::instrument;

use crate::git::run_git;
use crate::paths::resolve_borg_home_readonly;

/// Cap on a generated directory/branch component. Long enough for a real
/// branch name, short enough that the full path stays under the ~255-byte
/// per-component limit on ext4/APFS even after the repo prefix.
const MAX_SLUG_LEN: usize = 60;

/// One checkout of a repository, as reported by `git worktree list --porcelain`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Worktree {
    /// Absolute path to the checkout directory.
    pub path: PathBuf,
    /// Short branch name (`refs/heads/` stripped), or `None` when detached.
    pub branch: Option<String>,
    /// Full object id of `HEAD`, or `None` for a bare repo / unborn branch.
    pub head: Option<String>,
    /// True for the repository's primary checkout. Never removable.
    pub is_main: bool,
    /// True when the entry is a bare repository.
    pub is_bare: bool,
    /// True when `git worktree lock` has been applied. Blocks removal.
    pub locked: bool,
    /// True when git considers the entry stale (its directory is gone).
    pub prunable: bool,
}

impl Worktree {
    /// The trailing path component, used as the worktree's short name.
    pub fn name(&self) -> String {
        self.path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.path.display().to_string())
    }

    /// Whether this checkout lives under Borg's managed worktree root.
    ///
    /// Only managed worktrees are offered for removal by name in the CLI/TUI —
    /// a worktree the operator created elsewhere is theirs to manage, and
    /// deleting it because a name happened to match would be a surprise.
    pub fn is_managed(&self, worktrees_root: &Path) -> bool {
        // An empty root means the caller could not resolve one. Guarding here
        // rather than at each call site matters because `Path::starts_with("")`
        // is true for *every* path: an unresolvable root would silently promote
        // every hand-made checkout to "managed", which is the permissive
        // direction on a test that gates deletion.
        !worktrees_root.as_os_str().is_empty() && self.path.starts_with(worktrees_root)
    }
}

/// Root directory holding every managed worktree: a *sibling* of the data
/// directory, `~/.borg-worktrees` by default.
///
/// Deliberately **not** inside `$BORG_HOME`, for two reasons that both make the
/// feature non-functional or harmful there:
///
/// * The sandbox denies writes to the whole of `~/.borg` (see
///   `sandbox::policy::with_borg_dir_protected`). A worktree under it would be
///   a checkout the agent could read but never edit — the feature exists so the
///   agent can write somewhere safe.
/// * `borg backup` tarballs `$BORG_HOME`. Whole source checkouts would land in
///   every backup.
///
/// Following `$BORG_HOME` rather than hardcoding `~` keeps test fixtures and
/// alternate data dirs isolated. Read-only resolution — listing must work
/// before the directory exists, and [`add`] materializes it when it creates
/// one.
pub fn worktrees_root() -> Result<PathBuf> {
    let home = resolve_borg_home_readonly(None)?;
    match home.file_name() {
        Some(name) => {
            let sibling = format!("{}-worktrees", name.to_string_lossy());
            Ok(home.with_file_name(sibling))
        }
        // A data dir with no final component (`/`) has no sibling to name;
        // nesting is wrong here but a path we cannot form is worse.
        None => Ok(home.join("borg-worktrees")),
    }
}

/// Per-repository directory under [`worktrees_root`].
///
/// Keyed by the repo's directory name so `~/.borg-worktrees/` stays browsable.
/// A collision between two same-named repos in different parents is harmless:
/// the worktree names inside still have to be unique, and `git worktree add`
/// refuses to reuse a non-empty directory.
pub fn repo_worktrees_dir(root: &Path, repo_root: &Path) -> PathBuf {
    let repo_name = repo_root
        .file_name()
        .map(|s| slugify(&s.to_string_lossy()))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "repo".to_string());
    root.join(repo_name)
}

/// Default checkout path for a worktree called `name`.
pub fn default_worktree_path(root: &Path, repo_root: &Path, name: &str) -> Result<PathBuf> {
    let slug = slugify(name);
    if slug.is_empty() {
        bail!(
            "'{name}' has no usable characters for a worktree name — use letters, digits, or '-'"
        );
    }
    Ok(repo_worktrees_dir(root, repo_root).join(slug))
}

/// Reduce an arbitrary label to a safe single path component.
///
/// Lowercases, maps every run of non-alphanumeric characters to a single `-`
/// (so `feature/JIRA 123` and `feature-jira-123` land on the same slug), trims
/// leading/trailing dashes, and truncates to [`MAX_SLUG_LEN`]. The result can
/// be empty when the input had no alphanumerics at all; callers must reject
/// that rather than create a directory named `""`.
pub fn slugify(input: &str) -> String {
    let mut out = String::with_capacity(input.len().min(MAX_SLUG_LEN));
    let mut pending_dash = false;
    for ch in input.chars() {
        if ch.is_ascii_alphanumeric() {
            // Reserve room for the separator too, so truncation can never land
            // on a trailing dash or overshoot the component limit.
            let needed = if pending_dash && !out.is_empty() {
                2
            } else {
                1
            };
            if out.len() + needed > MAX_SLUG_LEN {
                break;
            }
            if needed == 2 {
                out.push('-');
            }
            pending_dash = false;
            out.push(ch.to_ascii_lowercase());
        } else {
            pending_dash = true;
        }
    }
    out
}

/// Parse the output of `git worktree list --porcelain`.
///
/// The format is stanzas separated by blank lines. The first line of each is
/// always `worktree <path>`; the rest are optional attribute lines. The first
/// stanza is the main checkout. Unknown attribute lines are ignored so a newer
/// git adding a field cannot break parsing.
fn parse_worktree_list(stdout: &str) -> Vec<Worktree> {
    let mut out: Vec<Worktree> = Vec::new();
    for stanza in stdout.split("\n\n") {
        let mut current: Option<Worktree> = None;
        for line in stanza.lines() {
            let line = line.trim_end();
            if let Some(path) = line.strip_prefix("worktree ") {
                current = Some(Worktree {
                    path: PathBuf::from(path),
                    branch: None,
                    head: None,
                    // The main checkout is whichever stanza comes first, which
                    // is git's documented ordering.
                    is_main: out.is_empty(),
                    is_bare: false,
                    locked: false,
                    prunable: false,
                });
                continue;
            }
            let Some(wt) = current.as_mut() else { continue };
            if let Some(head) = line.strip_prefix("HEAD ") {
                wt.head = Some(head.to_string());
            } else if let Some(branch) = line.strip_prefix("branch ") {
                wt.branch = Some(branch.trim_start_matches("refs/heads/").to_string());
            } else if line == "bare" {
                wt.is_bare = true;
            } else if line == "locked" || line.starts_with("locked ") {
                wt.locked = true;
            } else if line == "prunable" || line.starts_with("prunable ") {
                wt.prunable = true;
            }
        }
        if let Some(wt) = current {
            out.push(wt);
        }
    }
    out
}

/// List every checkout of the repository containing `repo_root`.
#[instrument(skip_all)]
pub async fn list(repo_root: &Path) -> Result<Vec<Worktree>> {
    let stdout = run_git(&["worktree", "list", "--porcelain"], repo_root)
        .await
        .context("listing git worktrees")?;
    Ok(parse_worktree_list(&stdout))
}

/// What to create, for [`add`].
#[derive(Debug, Clone, Default)]
pub struct AddOptions {
    /// Short label for the worktree. Also the default branch name.
    pub name: String,
    /// Branch to check out. Defaults to the slugified `name`.
    pub branch: Option<String>,
    /// Commit-ish the new branch starts from. Defaults to the current `HEAD`.
    pub base: Option<String>,
    /// Explicit checkout path, overriding [`default_worktree_path`].
    pub path: Option<PathBuf>,
    /// Check out an existing branch instead of creating one.
    pub existing_branch: bool,
}

/// Create a worktree for `repo_root` and return the resulting entry.
///
/// Creating a branch is the default because the whole point is isolation: two
/// checkouts of the same branch share no index and diverge silently. Pass
/// `existing_branch` to attach to a branch that already exists — git itself
/// then refuses if another worktree already holds it.
#[instrument(skip_all, fields(worktree.name = %opts.name))]
pub async fn add(repo_root: &Path, root: &Path, opts: &AddOptions) -> Result<Worktree> {
    let slug = slugify(&opts.name);
    if slug.is_empty() {
        bail!(
            "'{}' has no usable characters for a worktree name — use letters, digits, or '-'",
            opts.name
        );
    }
    let branch = match &opts.branch {
        Some(b) if !b.trim().is_empty() => b.trim().to_string(),
        _ => slug.clone(),
    };
    let path = match &opts.path {
        Some(p) => p.clone(),
        None => default_worktree_path(root, repo_root, &opts.name)?,
    };

    if path.exists() {
        let empty = std::fs::read_dir(&path)
            .with_context(|| format!("reading {}", path.display()))?
            .next()
            .is_none();
        if !empty {
            bail!(
                "{} already exists and is not empty — pick another name or remove it first",
                path.display()
            );
        }
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating worktree parent {}", parent.display()))?;
    }

    let path_str = path.to_string_lossy().to_string();
    let mut args: Vec<String> = vec!["worktree".into(), "add".into()];
    if opts.existing_branch {
        // Checking out an existing branch puts the worktree at that branch's
        // tip; there is no base to fork from. Accepting `base` silently here
        // would report success while leaving the tree on a different revision
        // than the operator asked for.
        if opts.base.as_deref().is_some_and(|b| !b.trim().is_empty()) {
            bail!(
                "a base commit cannot be combined with an existing branch — \
                 '{branch}' is checked out at its own tip"
            );
        }
        args.push(path_str);
        args.push(branch.clone());
    } else {
        args.push("-b".into());
        args.push(branch.clone());
        args.push(path_str);
        if let Some(base) = opts
            .base
            .as_deref()
            .map(str::trim)
            .filter(|b| !b.is_empty())
        {
            args.push(base.to_string());
        }
    }
    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
    run_git(&argv, repo_root)
        .await
        .with_context(|| format!("creating worktree '{}'", opts.name))?;

    // Re-read from git rather than synthesizing the entry: the checkout that
    // exists on disk is the thing to report, and git normalizes the path
    // (symlinks, trailing components) in ways a hand-built struct would not.
    let canonical = path.canonicalize().unwrap_or(path);
    list(repo_root)
        .await?
        .into_iter()
        .find(|wt| wt.path == canonical || wt.branch.as_deref() == Some(branch.as_str()))
        .with_context(|| format!("worktree '{}' was created but is not listed", opts.name))
}

/// Find one worktree by name, path, or branch.
///
/// Matching is deliberately narrow-to-wide: an exact path wins, then the
/// trailing directory name, then the branch. A target that matches nothing
/// returns `None` so callers can phrase their own error.
pub fn find<'a>(worktrees: &'a [Worktree], target: &str) -> Option<&'a Worktree> {
    find_where(worktrees, target, |_| true)
}

/// [`find`], restricted to checkouts satisfying `accept`.
///
/// The filter applies to the *name* and *branch* passes only — an exact path
/// always resolves. That split is the whole point: a bare word is a guess about
/// which checkout the operator meant, and a guess must not be allowed to land on
/// the main checkout or on a tree Borg does not own. A full path is not a guess,
/// so it stays usable as the deliberate escape hatch.
pub fn find_where<'a>(
    worktrees: &'a [Worktree],
    target: &str,
    accept: impl Fn(&Worktree) -> bool,
) -> Option<&'a Worktree> {
    let as_path = PathBuf::from(target);
    let canonical = as_path.canonicalize().ok();
    if let Some(exact) = worktrees
        .iter()
        .find(|wt| Some(&wt.path) == canonical.as_ref() || wt.path == as_path)
    {
        return Some(exact);
    }
    worktrees
        .iter()
        .filter(|wt| accept(wt))
        .find(|wt| wt.name() == target)
        .or_else(|| {
            worktrees
                .iter()
                .filter(|wt| accept(wt))
                .find(|wt| wt.branch.as_deref() == Some(target))
        })
}

/// Remove a worktree checkout, returning the path that was removed.
///
/// Refuses the main checkout unconditionally, refuses a locked one, and
/// refuses one with uncommitted changes unless `force`. The branch is left
/// alone — deleting a branch is a separate, far less recoverable act than
/// deleting a checkout that git can recreate.
///
/// `target` resolves by name or branch only among worktrees Borg manages (see
/// [`Worktree::is_managed`]); a checkout the operator created elsewhere is
/// theirs, and deleting it because a bare word happened to match would be a
/// surprise. Their full path still resolves, so removing one is possible but
/// has to be deliberate.
#[instrument(skip_all, fields(worktree.target = %target))]
pub async fn remove(
    repo_root: &Path,
    worktrees_root: &Path,
    target: &str,
    force: bool,
) -> Result<PathBuf> {
    let worktrees = list(repo_root).await?;
    // Canonicalize here rather than trusting the caller: git reports resolved
    // paths, so a root that traverses a symlink (`/tmp`, `/var` on macOS)
    // would match nothing and make every managed worktree look external —
    // failing closed, but on every removal.
    let root = worktrees_root
        .canonicalize()
        .unwrap_or_else(|_| worktrees_root.to_path_buf());
    let wt = find_where(&worktrees, target, |wt| !wt.is_main && wt.is_managed(&root))
        .with_context(|| {
            format!(
                "no worktree Borg manages matches '{target}' — \
             `borg worktree list` shows them, and one tagged `external` \
             needs its full path"
            )
        })?
        .clone();

    if wt.is_main {
        bail!(
            "'{target}' is the main checkout at {} — Borg will not remove it",
            wt.path.display()
        );
    }
    if wt.locked {
        bail!(
            "worktree '{}' is locked — run `git worktree unlock {}` first",
            wt.name(),
            wt.path.display()
        );
    }
    if !force && !wt.prunable && has_uncommitted_changes(&wt.path).await? {
        bail!(
            "worktree '{}' has uncommitted changes — commit them, or pass force to discard",
            wt.name()
        );
    }

    let path_str = wt.path.to_string_lossy().to_string();
    let mut args = vec!["worktree", "remove"];
    if force {
        args.push("--force");
    }
    args.push(&path_str);
    run_git(&args, repo_root)
        .await
        .with_context(|| format!("removing worktree '{}'", wt.name()))?;
    Ok(wt.path)
}

/// Whether `worktree_path` has staged, unstaged, or untracked changes.
async fn has_uncommitted_changes(worktree_path: &Path) -> Result<bool> {
    if !worktree_path.exists() {
        // Its directory is gone, so there is nothing to lose by removing the
        // administrative entry.
        return Ok(false);
    }
    let status = run_git(&["status", "--porcelain"], worktree_path)
        .await
        .with_context(|| format!("checking status of {}", worktree_path.display()))?;
    Ok(!status.trim().is_empty())
}

/// Drop administrative entries whose checkout directory no longer exists.
///
/// Returns the paths that disappeared from the listing. Purely bookkeeping —
/// it never deletes a directory that is still present.
///
/// `--expire now` is deliberate. Git's default (`gc.worktreePruneExpire`, three
/// months) exists so a worktree on an unmounted removable drive is not reaped;
/// but an operator who typed `prune` means "clear the stale entries I can see
/// listed", and a command that reports nothing and changes nothing reads as
/// broken. Locked worktrees are exempt from pruning in git itself, which is
/// where the removable-drive case is actually handled.
///
/// The result is derived by diffing the listing rather than by parsing
/// `--verbose`, whose output goes to stderr and is not a stable interface.
#[instrument(skip_all)]
pub async fn prune(repo_root: &Path) -> Result<Vec<PathBuf>> {
    let before = list(repo_root).await?;
    run_git(&["worktree", "prune", "--expire", "now"], repo_root)
        .await
        .context("pruning git worktrees")?;
    let kept: std::collections::HashSet<PathBuf> =
        list(repo_root).await?.into_iter().map(|w| w.path).collect();
    Ok(before
        .into_iter()
        .map(|w| w.path)
        .filter(|p| !kept.contains(p))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `git worktree list --porcelain` is the only record of what exists, so
    /// every attribute this module acts on — main-vs-linked, detached, locked,
    /// prunable — has to survive the parse. Getting `is_main` wrong in
    /// particular would let `remove` delete the operator's repository.
    #[test]
    fn porcelain_list_parses_every_attribute_removal_depends_on() {
        let stdout = "\
worktree /repo
HEAD abc123
branch refs/heads/main

worktree /repo/.borg/wt/feature
HEAD def456
branch refs/heads/feature-x

worktree /repo/.borg/wt/detached
HEAD 999aaa
detached

worktree /repo/.borg/wt/pinned
HEAD 111bbb
branch refs/heads/pinned
locked keeping this around

worktree /repo/.borg/wt/gone
HEAD 222ccc
branch refs/heads/gone
prunable gitdir file points to non-existent location
";
        let got = parse_worktree_list(stdout);
        assert_eq!(got.len(), 5, "one entry per stanza");

        assert!(got[0].is_main, "the first stanza is the main checkout");
        assert_eq!(got[0].branch.as_deref(), Some("main"));
        assert_eq!(got[0].head.as_deref(), Some("abc123"));

        assert!(!got[1].is_main, "linked checkouts must not read as main");
        assert_eq!(got[1].branch.as_deref(), Some("feature-x"));
        assert_eq!(got[1].name(), "feature");

        assert_eq!(got[2].branch, None, "a detached head has no branch");
        assert_eq!(got[2].head.as_deref(), Some("999aaa"));

        assert!(got[3].locked, "a locked worktree must block removal");
        assert!(!got[3].prunable);

        assert!(got[4].prunable);
        assert!(!got[4].locked);
    }

    /// A bare repository lists a `bare` stanza with no HEAD line. Treating it
    /// as an ordinary checkout would make it a removal candidate.
    #[test]
    fn a_bare_stanza_parses_without_a_head() {
        let got = parse_worktree_list("worktree /srv/repo.git\nbare\n");
        assert_eq!(got.len(), 1);
        assert!(got[0].is_bare);
        assert_eq!(got[0].head, None);
        assert_eq!(got[0].branch, None);
    }

    /// Git may add attribute lines in future versions. An unknown line must be
    /// skipped, not abort the stanza — otherwise a git upgrade silently hides
    /// worktrees from the listing.
    #[test]
    fn unknown_attribute_lines_are_ignored() {
        let got = parse_worktree_list(
            "worktree /repo\nHEAD abc\nbranch refs/heads/main\nsomething-new value\n",
        );
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].branch.as_deref(), Some("main"));
    }

    /// Trailing newlines produce empty stanzas; they must not become entries
    /// with an empty path.
    #[test]
    fn empty_stanzas_produce_no_entries() {
        assert!(parse_worktree_list("").is_empty());
        assert!(parse_worktree_list("\n\n\n").is_empty());
        assert_eq!(parse_worktree_list("worktree /repo\n\n\n").len(), 1);
    }

    /// The slug becomes a real directory name, so it must never contain a path
    /// separator, `..`, whitespace, or shell metacharacters no matter what the
    /// caller passes.
    #[test]
    fn slugify_produces_a_single_safe_path_component() {
        let cases = [
            ("feature/JIRA 123", "feature-jira-123"),
            ("../../etc/passwd", "etc-passwd"),
            ("Fix   the   bug", "fix-the-bug"),
            ("--leading-and-trailing--", "leading-and-trailing"),
            ("already-fine", "already-fine"),
            ("rm -rf $HOME; echo", "rm-rf-home-echo"),
        ];
        for (input, want) in cases {
            let got = slugify(input);
            assert_eq!(got, want, "slugify({input:?})");
            assert!(
                !got.contains(std::path::MAIN_SEPARATOR),
                "slug {got:?} must be one path component"
            );
            assert_eq!(
                Path::new(&got).components().count(),
                1,
                "slug {got:?} must be one path component"
            );
        }
    }

    /// Input with no alphanumerics has no safe slug. Returning empty (rather
    /// than a placeholder) forces callers to reject it, which is what keeps a
    /// directory literally named `""` from ever being attempted.
    #[test]
    fn slugify_returns_empty_when_nothing_is_usable() {
        assert_eq!(slugify(""), "");
        assert_eq!(slugify("///"), "");
        assert_eq!(slugify("   "), "");
    }

    /// Filesystems cap a path component around 255 bytes; a branch name pasted
    /// from a long ticket title must not blow past it.
    #[test]
    fn slugify_truncates_long_input() {
        let got = slugify(&"a".repeat(500));
        assert_eq!(got.len(), MAX_SLUG_LEN);
    }

    /// A slug that ends mid-truncation must not keep a trailing dash, and must
    /// still be a single component.
    #[test]
    fn slugify_truncation_stays_a_clean_component() {
        let got = slugify(&"word-".repeat(100));
        assert!(got.len() <= MAX_SLUG_LEN);
        assert!(!got.ends_with('-'), "got {got:?}");
        assert!(!got.starts_with('-'), "got {got:?}");
    }

    /// `default_worktree_path` must nest under the repo-specific directory so
    /// two repositories can each have a worktree called `fix`.
    #[test]
    fn default_paths_are_namespaced_per_repo() {
        let root = Path::new("/home/u/.borg-worktrees");
        let a = default_worktree_path(root, Path::new("/code/alpha"), "fix").unwrap();
        let b = default_worktree_path(root, Path::new("/code/beta"), "fix").unwrap();
        assert_eq!(a, root.join("alpha").join("fix"));
        assert_eq!(b, root.join("beta").join("fix"));
        assert_ne!(a, b, "same name in two repos must not collide");
    }

    /// A name that slugifies to nothing must be refused before any directory
    /// is created.
    #[test]
    fn default_path_rejects_an_unusable_name() {
        let err = default_worktree_path(Path::new("/root"), Path::new("/code/alpha"), "///")
            .expect_err("a name with no usable characters has no path");
        assert!(
            err.to_string().contains("no usable characters"),
            "error must say why: {err}"
        );
    }

    /// Worktrees must never live inside `$BORG_HOME`. The sandbox denies writes
    /// to the whole data dir, so a checkout there would be one the agent can
    /// read but never edit — the feature silently does nothing. `borg backup`
    /// would also tar up every checkout. Pinned here because both failures are
    /// invisible until someone actually tries to code in a sandboxed worktree.
    #[test]
    fn the_worktree_root_is_a_sibling_of_the_data_dir_not_inside_it() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join(".borg");
        // SAFETY: single-threaded within this test; the value is read
        // immediately and restored before returning.
        let previous = std::env::var_os("BORG_HOME");
        unsafe { std::env::set_var("BORG_HOME", &home) };
        let root = worktrees_root().expect("root resolves");
        match previous {
            Some(p) => unsafe { std::env::set_var("BORG_HOME", p) },
            None => unsafe { std::env::remove_var("BORG_HOME") },
        }

        assert!(
            !root.starts_with(&home),
            "{} must not be inside the data dir {}",
            root.display(),
            home.display()
        );
        assert_eq!(
            root,
            tmp.path().join(".borg-worktrees"),
            "the root is the data dir's sibling, so alternate BORG_HOMEs stay isolated"
        );
    }

    /// The managed/unmanaged split decides whether the CLI will offer to delete
    /// a directory. A worktree the operator created next to their repo must
    /// never read as managed.
    #[test]
    fn only_worktrees_under_the_managed_root_read_as_managed() {
        let root = Path::new("/home/u/.borg-worktrees");
        let managed = parse_worktree_list("worktree /home/u/.borg-worktrees/alpha/fix\n")
            .pop()
            .unwrap();
        let external = parse_worktree_list("worktree /code/alpha-fix\n")
            .pop()
            .unwrap();
        assert!(managed.is_managed(root));
        assert!(!external.is_managed(root));
    }

    fn sample() -> Vec<Worktree> {
        parse_worktree_list(
            "\
worktree /code/alpha
HEAD aaa
branch refs/heads/main

worktree /home/u/.borg-worktrees/alpha/fix-login
HEAD bbb
branch refs/heads/fix-login-bug
",
        )
    }

    /// Resolution must accept all three things an operator has in front of
    /// them: the directory name from a listing, the branch name, and the path.
    #[test]
    fn find_matches_by_name_path_and_branch() {
        let wts = sample();
        assert_eq!(
            find(&wts, "fix-login").unwrap().head.as_deref(),
            Some("bbb")
        );
        assert_eq!(
            find(&wts, "fix-login-bug").unwrap().head.as_deref(),
            Some("bbb")
        );
        assert_eq!(
            find(&wts, "/home/u/.borg-worktrees/alpha/fix-login")
                .unwrap()
                .head
                .as_deref(),
            Some("bbb")
        );
        assert!(find(&wts, "nope").is_none());
    }

    /// Name must beat branch: if one worktree's directory is called `x` and a
    /// different worktree sits on a branch called `x`, acting on the branch
    /// match would operate on the wrong checkout.
    #[test]
    fn find_prefers_the_directory_name_over_a_branch_of_the_same_name() {
        let wts = parse_worktree_list(
            "\
worktree /code/alpha
HEAD aaa
branch refs/heads/main

worktree /wt/shared
HEAD bbb
branch refs/heads/other

worktree /wt/second
HEAD ccc
branch refs/heads/shared
",
        );
        assert_eq!(
            find(&wts, "shared").unwrap().head.as_deref(),
            Some("bbb"),
            "the directory named 'shared' must win over the branch named 'shared'"
        );
    }

    // ---- End-to-end against a real repository -------------------------------
    //
    // The parse tests above pin the format; these pin the behavior, because the
    // guards that matter (never remove main, refuse a dirty tree) are only real
    // if `git worktree` actually agrees with what we think we asked for.

    fn init_repo(dir: &Path) {
        let run = |args: &[&str]| {
            let ok = std::process::Command::new("git")
                .args(args)
                .current_dir(dir)
                // Isolate from the developer's global/system git config so the
                // fixture is deterministic.
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .status()
                .expect("spawn git")
                .success();
            assert!(ok, "git {args:?} failed");
        };
        run(&["init", "-q"]);
        run(&["config", "user.email", "t@borg.test"]);
        run(&["config", "user.name", "borg-test"]);
        std::fs::write(dir.join("a.txt"), "hello\n").unwrap();
        run(&["add", "a.txt"]);
        run(&["commit", "-qm", "init"]);
    }

    /// A fresh temp repo plus an empty managed-worktree root.
    fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("alpha");
        std::fs::create_dir_all(&repo).unwrap();
        init_repo(&repo);
        let root = tmp.path().join("worktrees");
        (tmp, repo, root)
    }

    /// The core promise: a worktree lands at the managed path, on its own new
    /// branch, with the repository's files present — and the main checkout's
    /// branch is untouched.
    #[tokio::test]
    async fn add_creates_an_isolated_checkout_on_a_new_branch() {
        let (_tmp, repo, root) = fixture();

        let wt = add(
            &repo,
            &root,
            &AddOptions {
                name: "Fix Login".to_string(),
                ..Default::default()
            },
        )
        .await
        .expect("add worktree");

        assert_eq!(wt.branch.as_deref(), Some("fix-login"));
        assert!(!wt.is_main);
        assert_eq!(
            wt.path,
            root.join("alpha").join("fix-login").canonicalize().unwrap(),
            "the checkout must land under the managed root"
        );
        assert!(
            wt.path.join("a.txt").is_file(),
            "the worktree must hold the repository's files"
        );
        assert!(wt.is_managed(&root.canonicalize().unwrap()));

        let all = list(&repo).await.unwrap();
        assert_eq!(all.len(), 2, "main plus the new worktree");
        assert!(all[0].is_main);
        assert_ne!(
            all[0].branch.as_deref(),
            Some("fix-login"),
            "the main checkout must stay on its own branch"
        );
    }

    /// `borg --worktree "Fix Login"` is find-or-create, and the directory it
    /// created is named by the *slug*. So the slug of a display name must
    /// resolve back to the worktree — otherwise the second invocation misses,
    /// tries to create again, and fails as "already exists".
    #[tokio::test]
    async fn a_display_names_slug_resolves_back_to_its_worktree() {
        let (_tmp, repo, root) = fixture();
        let created = add(
            &repo,
            &root,
            &AddOptions {
                name: "Fix Login".to_string(),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let all = list(&repo).await.unwrap();
        assert!(
            find(&all, "Fix Login").is_none(),
            "the raw display name is not what the directory or branch is called"
        );
        let found = find(&all, &slugify("Fix Login"))
            .expect("the slug must resolve to the worktree that was created for it");
        assert_eq!(found.path, created.path);
    }

    /// `base` decides what the branch forks from. Without it a worktree would
    /// silently start from whatever HEAD happened to be, which is the bug that
    /// makes an agent's work unmergeable.
    #[tokio::test]
    async fn add_forks_the_new_branch_from_the_requested_base() {
        let (_tmp, repo, root) = fixture();
        let first = run_git(&["rev-parse", "HEAD"], &repo).await.unwrap();
        std::fs::write(repo.join("b.txt"), "second\n").unwrap();
        run_git(&["add", "b.txt"], &repo).await.unwrap();
        run_git(&["commit", "-qm", "second"], &repo).await.unwrap();

        let wt = add(
            &repo,
            &root,
            &AddOptions {
                name: "from-first".to_string(),
                base: Some(first.clone()),
                ..Default::default()
            },
        )
        .await
        .expect("add worktree");

        assert_eq!(wt.head.as_deref(), Some(first.as_str()));
        assert!(
            !wt.path.join("b.txt").exists(),
            "a worktree based on the first commit must not contain the second commit's file"
        );
    }

    /// Attaching to an existing branch is the "resume someone else's work"
    /// path; it must check that branch out rather than create a new one.
    #[tokio::test]
    async fn add_can_attach_to_an_existing_branch() {
        let (_tmp, repo, root) = fixture();
        run_git(&["branch", "handoff"], &repo).await.unwrap();

        let wt = add(
            &repo,
            &root,
            &AddOptions {
                name: "handoff".to_string(),
                existing_branch: true,
                ..Default::default()
            },
        )
        .await
        .expect("attach to existing branch");

        assert_eq!(wt.branch.as_deref(), Some("handoff"));
    }

    /// Reusing a name would either clobber a checkout or silently attach to the
    /// wrong one. Both are worse than an error.
    #[tokio::test]
    async fn add_refuses_a_name_that_is_already_checked_out() {
        let (_tmp, repo, root) = fixture();
        let opts = AddOptions {
            name: "dup".to_string(),
            ..Default::default()
        };
        add(&repo, &root, &opts).await.expect("first add");

        let err = add(&repo, &root, &opts)
            .await
            .expect_err("a second worktree at the same path must be refused");
        assert!(
            err.to_string().contains("already exists"),
            "error must name the collision: {err:#}"
        );
    }

    /// The single most destructive mistake this module could make: treating the
    /// operator's repository as a disposable worktree.
    #[tokio::test]
    async fn remove_never_deletes_the_main_checkout_even_with_force() {
        let (_tmp, repo, root) = fixture();
        add(
            &repo,
            &root,
            &AddOptions {
                name: "side".to_string(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let main_path = repo.canonicalize().unwrap();

        for force in [false, true] {
            let err = remove(&repo, &root, main_path.to_str().unwrap(), force)
                .await
                .expect_err("the main checkout must never be removable");
            assert!(
                err.to_string().contains("main checkout"),
                "error must explain the refusal: {err:#}"
            );
        }
        assert!(
            repo.join("a.txt").is_file(),
            "the repository must still be intact"
        );
    }

    /// A clean worktree removes cleanly, by name, and disappears from git's
    /// records as well as from disk.
    #[tokio::test]
    async fn remove_deletes_a_clean_worktree_by_name() {
        let (_tmp, repo, root) = fixture();
        let wt = add(
            &repo,
            &root,
            &AddOptions {
                name: "scratch".to_string(),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let removed = remove(&repo, &root, "scratch", false)
            .await
            .expect("remove");
        assert_eq!(removed, wt.path);
        assert!(!wt.path.exists(), "the directory must be gone");
        assert_eq!(
            list(&repo).await.unwrap().len(),
            1,
            "only the main checkout should remain"
        );
    }

    /// Uncommitted work in a worktree is unrecoverable once the directory is
    /// deleted, so removal has to refuse until the caller says otherwise.
    #[tokio::test]
    async fn remove_refuses_uncommitted_work_until_forced() {
        let (_tmp, repo, root) = fixture();
        let wt = add(
            &repo,
            &root,
            &AddOptions {
                name: "dirty".to_string(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        std::fs::write(wt.path.join("a.txt"), "edited\n").unwrap();

        let err = remove(&repo, &root, "dirty", false)
            .await
            .expect_err("a dirty worktree must not be removed silently");
        assert!(
            err.to_string().contains("uncommitted changes"),
            "error must say what would be lost: {err:#}"
        );
        assert!(wt.path.exists(), "nothing may be deleted on the refusal");

        remove(&repo, &root, "dirty", true)
            .await
            .expect("force must discard the changes");
        assert!(!wt.path.exists());
    }

    /// An untracked file is just as unrecoverable as a modified tracked one.
    #[tokio::test]
    async fn remove_counts_untracked_files_as_uncommitted_work() {
        let (_tmp, repo, root) = fixture();
        let wt = add(
            &repo,
            &root,
            &AddOptions {
                name: "untracked".to_string(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        std::fs::write(wt.path.join("notes.md").as_path(), "draft\n").unwrap();

        let err = remove(&repo, &root, "untracked", false)
            .await
            .expect_err("an untracked file is unsaved work too");
        assert!(err.to_string().contains("uncommitted changes"), "{err:#}");
    }

    /// A locked worktree is an explicit "do not touch" from the operator (or
    /// from a removable drive). Force must not override that.
    #[tokio::test]
    async fn remove_refuses_a_locked_worktree() {
        let (_tmp, repo, root) = fixture();
        let wt = add(
            &repo,
            &root,
            &AddOptions {
                name: "pinned".to_string(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        run_git(&["worktree", "lock", wt.path.to_str().unwrap()], &repo)
            .await
            .unwrap();

        let err = remove(&repo, &root, "pinned", true)
            .await
            .expect_err("a locked worktree must survive force");
        assert!(
            err.to_string().contains("locked"),
            "error must point at the lock: {err:#}"
        );
        assert!(wt.path.exists());
    }

    /// A target that matches nothing must be an error, not a no-op that reports
    /// success.
    #[tokio::test]
    async fn remove_reports_an_unknown_target() {
        let (_tmp, repo, root) = fixture();
        let err = remove(&repo, &root, "ghost", true)
            .await
            .expect_err("an unknown worktree is not a silent success");
        assert!(
            err.to_string().contains("no worktree Borg manages matches"),
            "{err:#}"
        );
    }

    /// A checkout the operator made themselves is theirs. A bare name or branch
    /// must not reach it — that is how `remove fix` deletes a review checkout
    /// nobody asked Borg to touch. Its full path still works, because typing
    /// the path is a deliberate act rather than a guess.
    #[tokio::test]
    async fn remove_refuses_an_external_checkout_by_name_but_allows_it_by_path() {
        let (tmp, repo, root) = fixture();
        let outside = tmp.path().join("hand-made");
        let external = add(
            &repo,
            &root,
            &AddOptions {
                name: "hand-made".to_string(),
                branch: Some("review".to_string()),
                path: Some(outside.clone()),
                ..Default::default()
            },
        )
        .await
        .expect("an external worktree can still be created with an explicit path");
        assert!(
            !external.is_managed(&root),
            "the fixture must actually be outside the managed root"
        );

        for target in ["hand-made", "review"] {
            let err = remove(&repo, &root, target, true)
                .await
                .expect_err("a bare name must not reach an external checkout")
                .to_string();
            assert!(
                err.contains("no worktree Borg manages matches"),
                "'{target}' must not reach an external checkout: {err}"
            );
        }
        assert!(external.path.exists(), "nothing may have been deleted");

        remove(&repo, &root, external.path.to_str().unwrap(), true)
            .await
            .expect("the full path is an explicit instruction and must work");
        assert!(!external.path.exists());
    }

    /// An unresolvable worktrees root must mean "Borg manages nothing", not
    /// "Borg manages everything". `Path::starts_with("")` is true for every
    /// path, so the naive test fails open on the check that gates deletion.
    #[test]
    fn an_empty_managed_root_marks_nothing_as_managed() {
        let wt = parse_worktree_list("worktree /code/alpha\n").pop().unwrap();
        assert!(
            !wt.is_managed(Path::new("")),
            "an empty root must not promote every checkout to managed"
        );
    }

    /// `--existing` checks a branch out at its own tip, so a base has nowhere
    /// to apply. Silently ignoring it would report success with the tree on a
    /// different revision than the operator named.
    #[tokio::test]
    async fn add_refuses_a_base_together_with_an_existing_branch() {
        let (_tmp, repo, root) = fixture();
        run_git(&["branch", "handoff"], &repo).await.unwrap();

        let err = add(
            &repo,
            &root,
            &AddOptions {
                name: "handoff".to_string(),
                base: Some("HEAD~0".to_string()),
                existing_branch: true,
                ..Default::default()
            },
        )
        .await
        .expect_err("a base plus an existing branch is a contradiction");
        assert!(
            err.to_string().contains("cannot be combined"),
            "the error must name the conflict: {err:#}"
        );
    }

    /// `find_where` must never let a bare word land on a filtered-out checkout,
    /// while an exact path still resolves to anything. That split is what keeps
    /// `-w main` from attaching to the operator's live tree.
    #[test]
    fn find_where_filters_bare_words_but_never_an_exact_path() {
        let wts = parse_worktree_list(
            "\
worktree /code/alpha
HEAD aaa
branch refs/heads/main

worktree /wt/side
HEAD bbb
branch refs/heads/side
",
        );
        let not_main = |wt: &Worktree| !wt.is_main;

        assert!(
            find_where(&wts, "main", not_main).is_none(),
            "the main checkout's branch must not resolve"
        );
        assert!(
            find_where(&wts, "alpha", not_main).is_none(),
            "the main checkout's directory name must not resolve"
        );
        assert_eq!(
            find_where(&wts, "side", not_main).map(|w| w.head.as_deref()),
            Some(Some("bbb")),
            "an allowed worktree still resolves by name"
        );
        assert_eq!(
            find_where(&wts, "/code/alpha", not_main).map(|w| w.head.as_deref()),
            Some(Some("aaa")),
            "an exact path is explicit and bypasses the filter"
        );
    }

    /// When a checkout directory is deleted out from under git, `prune` clears
    /// the stale record. Until it runs, the entry lingers in every listing.
    #[tokio::test]
    async fn prune_clears_records_for_deleted_directories() {
        let (_tmp, repo, root) = fixture();
        let wt = add(
            &repo,
            &root,
            &AddOptions {
                name: "vanishing".to_string(),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        std::fs::remove_dir_all(&wt.path).unwrap();
        assert_eq!(
            list(&repo).await.unwrap().len(),
            2,
            "git still records the entry before a prune"
        );

        let pruned = prune(&repo).await.expect("prune");
        assert_eq!(
            pruned,
            vec![wt.path.clone()],
            "prune must report exactly the entry it dropped"
        );
        assert_eq!(
            list(&repo).await.unwrap().len(),
            1,
            "the stale entry must be gone after pruning"
        );
    }

    /// Prune on a repository with nothing stale must succeed and report
    /// nothing, so a scheduled call is not a source of noise.
    #[tokio::test]
    async fn prune_is_a_quiet_no_op_when_nothing_is_stale() {
        let (_tmp, repo, root) = fixture();
        add(
            &repo,
            &root,
            &AddOptions {
                name: "live".to_string(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert!(prune(&repo).await.expect("prune").is_empty());
        assert_eq!(list(&repo).await.unwrap().len(), 2);
    }

    /// A worktree whose directory is gone should still be removable without
    /// force — there is no uncommitted work left to protect.
    #[tokio::test]
    async fn remove_does_not_demand_force_for_a_vanished_checkout() {
        let (_tmp, repo, root) = fixture();
        let wt = add(
            &repo,
            &root,
            &AddOptions {
                name: "gone".to_string(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        std::fs::remove_dir_all(&wt.path).unwrap();

        remove(&repo, &root, "gone", false)
            .await
            .expect("a missing checkout has no work to lose");
        assert_eq!(list(&repo).await.unwrap().len(), 1);
    }
}
