//! `borg worktree` — isolated checkouts for coding.
//!
//! Thin operator surface over [`cortex::worktree`]. Everything here resolves
//! the repository from the current directory, so the commands read the same
//! way `git` does: run them from inside the repo you mean.
//!
//! The default action is `list`, because "what worktrees do I have?" is the
//! question an operator has far more often than any mutation.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::Subcommand;

use cortex::worktree::{self, AddOptions, Worktree};

#[derive(Subcommand)]
pub(crate) enum WorktreeAction {
    /// List every checkout of this repository
    List,
    /// Create a worktree on its own branch
    #[command(alias = "new")]
    Add {
        /// Short name for the worktree. Also the branch name by default.
        name: String,
        /// Branch to create (or check out with --existing). Defaults to NAME.
        #[arg(long, short = 'b')]
        branch: Option<String>,
        /// Commit, tag, or branch the new branch starts from. Defaults to HEAD.
        /// Meaningless with --existing, which checks a branch out at its own
        /// tip, so the two are refused together rather than one being ignored.
        #[arg(long, value_name = "COMMIT", conflicts_with = "existing")]
        from: Option<String>,
        /// Check out a branch that already exists instead of creating one
        #[arg(long)]
        existing: bool,
        /// Create the checkout here instead of under ~/.borg-worktrees
        #[arg(long, value_name = "DIR")]
        path: Option<PathBuf>,
        /// Print only the resulting path, for `cd "$(borg worktree add x --print-path)"`
        #[arg(long)]
        print_path: bool,
    },
    /// Delete a worktree checkout. The branch is kept.
    #[command(alias = "rm")]
    Remove {
        /// Worktree name, branch, or path
        target: String,
        /// Discard uncommitted changes in the worktree
        #[arg(long, short = 'f')]
        force: bool,
    },
    /// Forget worktrees whose directory is already gone
    Prune,
}

pub(crate) async fn dispatch(action: Option<WorktreeAction>) -> Result<()> {
    match action {
        None | Some(WorktreeAction::List) => run_list().await,
        Some(WorktreeAction::Add {
            name,
            branch,
            from,
            existing,
            path,
            print_path,
        }) => {
            run_add(
                &AddOptions {
                    name,
                    branch,
                    base: from,
                    path,
                    existing_branch: existing,
                },
                print_path,
            )
            .await
        }
        Some(WorktreeAction::Remove { target, force }) => run_remove(&target, force).await,
        Some(WorktreeAction::Prune) => run_prune().await,
    }
}

/// Resolve symlinks when the path exists, otherwise pass it through.
///
/// Git reports canonical paths. A raw `current_dir()` or a `~/.borg` prefix can
/// traverse a symlink (macOS `/tmp` → `/private/tmp` is the common one), and an
/// uncanonicalized comparison would then decide the session is in no worktree
/// at all, or that a managed worktree is external.
fn canonical(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

/// The *main* repository root for the current directory.
///
/// `resolve_main_repo_root` rather than `find_repo_root`, because the latter
/// stops at the first `.git` and so returns the *worktree* when we are already
/// inside one. Every managed path is namespaced by this value, so the weaker
/// resolver would file `alpha`'s worktrees under the name of whichever worktree
/// the operator happened to be standing in — and two repos would then collide
/// on a name neither of them had used. It returns `None` for a checkout whose
/// `.git` points somewhere that is not a worktree (a submodule), so fall back
/// rather than refusing to work there.
fn repo_root() -> Result<PathBuf> {
    let cwd = std::env::current_dir().context("reading the current directory")?;
    cortex::git::resolve_main_repo_root(&cwd)
        .or_else(|| cortex::git::find_repo_root(&cwd))
        .context("this directory is not inside a git repository — cd into one first")
}

async fn run_list() -> Result<()> {
    let repo = repo_root()?;
    let worktrees = worktree::list(&repo).await?;
    let root = canonical(&worktree::worktrees_root().unwrap_or_default());

    if worktrees.len() <= 1 {
        println!("No worktrees yet. Create one with: borg worktree add <name>");
        return Ok(());
    }

    let width = worktrees
        .iter()
        .map(|wt| wt.name().chars().count())
        .max()
        .unwrap_or(4)
        .clamp(4, 32);
    for wt in &worktrees {
        println!("{}", format_row(wt, &root, width));
    }
    Ok(())
}

/// One listing line: `NAME  BRANCH  PATH  [tags]`.
///
/// Split out so the formatting is testable without a repository — the tags are
/// the part an operator acts on (a locked or stale entry explains why a removal
/// just failed), so they must not silently go missing.
fn format_row(wt: &Worktree, worktrees_root: &Path, name_width: usize) -> String {
    let mut tags: Vec<&str> = Vec::new();
    if wt.is_main {
        tags.push("main");
    }
    if wt.locked {
        tags.push("locked");
    }
    if wt.prunable {
        tags.push("stale");
    }
    if !wt.is_main && !wt.is_managed(worktrees_root) {
        tags.push("external");
    }
    let suffix = if tags.is_empty() {
        String::new()
    } else {
        format!("  [{}]", tags.join(", "))
    };
    format!(
        "{:<name_width$}  {:<24}  {}{}",
        wt.name(),
        wt.branch.as_deref().unwrap_or("(detached)"),
        wt.path.display(),
        suffix,
    )
}

/// Render the `/worktree` overview shown inside the TUI.
///
/// Answers the two questions a session has, in order: *where am I working*,
/// and *what else exists*. The "you are here" marker is the load-bearing part —
/// without it an operator cannot tell an isolated session from one editing the
/// main checkout, which is the mistake worktrees exist to prevent.
pub(crate) fn format_overview(current: &Path, worktrees: &[Worktree], root: &Path) -> String {
    // Longest matching prefix wins: a worktree created inside another
    // checkout's directory would otherwise resolve to the outer one.
    let here = worktrees
        .iter()
        .filter(|wt| current.starts_with(&wt.path))
        .max_by_key(|wt| wt.path.as_os_str().len());

    let mut out = String::new();
    match here {
        Some(wt) if wt.is_main => out.push_str(&format!(
            "Working in the main checkout ({})\n",
            wt.branch.as_deref().unwrap_or("detached")
        )),
        Some(wt) => out.push_str(&format!(
            "Working in worktree '{}' ({})\n",
            wt.name(),
            wt.branch.as_deref().unwrap_or("detached")
        )),
        None => out.push_str("Working outside this repository's checkouts\n"),
    }

    if worktrees.len() > 1 {
        let width = worktrees
            .iter()
            .map(|wt| wt.name().chars().count())
            .max()
            .unwrap_or(4)
            .clamp(4, 32);
        out.push('\n');
        for wt in worktrees {
            let marker = if here.map(|h| h.path == wt.path).unwrap_or(false) {
                " <- here"
            } else {
                ""
            };
            out.push_str(&format!("  {}{}\n", format_row(wt, root, width), marker));
        }
    }

    out.push('\n');
    out.push_str("/worktree new <name>     create an isolated checkout\n");
    out.push_str("/worktree remove <name>  delete one (keeps the branch)");
    out
}

async fn run_add(opts: &AddOptions, print_path: bool) -> Result<()> {
    let repo = repo_root()?;
    let root = worktree::worktrees_root()?;
    let wt = worktree::add(&repo, &root, opts).await?;

    if print_path {
        // Machine-readable mode: the path and nothing else, so it can be
        // substituted straight into a `cd`.
        println!("{}", wt.path.display());
        return Ok(());
    }
    println!(
        "Created worktree '{}' on branch '{}'",
        wt.name(),
        wt.branch.as_deref().unwrap_or("(detached)")
    );
    println!("  {}", wt.path.display());
    println!();
    println!("Work in it with:  borg --worktree {}", wt.name());
    println!("Or open it with:  cd {}", wt.path.display());
    Ok(())
}

async fn run_remove(target: &str, force: bool) -> Result<()> {
    let repo = repo_root()?;
    let root = canonical(&worktree::worktrees_root()?);
    let path = worktree::remove(&repo, &root, target, force).await?;
    println!("Removed worktree at {}", path.display());
    println!("The branch is untouched — delete it with: git branch -d <branch>");
    Ok(())
}

async fn run_prune() -> Result<()> {
    let repo = repo_root()?;
    let pruned = worktree::prune(&repo).await?;
    if pruned.is_empty() {
        println!("Nothing to prune — every worktree still has its directory.");
        return Ok(());
    }
    for path in &pruned {
        println!("Forgot {}", path.display());
    }
    Ok(())
}

/// Resolve `--worktree NAME` to a directory, creating the worktree when it does
/// not exist yet.
///
/// Find-or-create is the point: `borg --worktree fix-login` has to work the
/// first time and the tenth time with the same words. It attaches to any
/// existing checkout that matches by name, branch, or path — creating a second
/// one because the operator typed the branch instead of the directory would
/// quietly split their work across two trees.
pub(crate) async fn resolve_or_create(name: &str) -> Result<PathBuf> {
    let repo = repo_root()?;
    let existing = worktree::list(&repo).await?;
    // The slug is what `add` actually names the directory and branch, so a name
    // that needed slugifying ("Fix Login") only matches on the second lookup.
    // Without it, `borg -w "Fix Login"` would create on the first run and fail
    // as "already exists" on every run after.
    let slug = worktree::slugify(name);
    // `is_main` is excluded deliberately. `find` falls back to a branch match,
    // so `-w main` would otherwise resolve to the operator's live checkout and
    // run the agent there while reporting nothing — the precise accident this
    // feature exists to prevent. Excluded, the name falls through to `add`,
    // which fails loudly because git will not check a branch out twice.
    let attachable = |wt: &worktree::Worktree| !wt.is_main;
    if let Some(wt) = worktree::find_where(&existing, name, attachable)
        .or_else(|| worktree::find_where(&existing, &slug, attachable))
    {
        return Ok(wt.path.clone());
    }
    let root = worktree::worktrees_root()?;
    let wt = worktree::add(
        &repo,
        &root,
        &AddOptions {
            name: name.to_string(),
            ..Default::default()
        },
    )
    .await?;
    eprintln!(
        "Created worktree '{}' on branch '{}' at {}",
        wt.name(),
        wt.branch.as_deref().unwrap_or("(detached)"),
        wt.path.display()
    );
    Ok(wt.path)
}

/// Backing calls for the TUI's `/worktree` command.
///
/// The TUI renders text rather than exit codes, so these return the message to
/// show — including the failure text. A slash command that silently did nothing
/// on error would be worse than one that explains itself.
pub(crate) mod tui {
    use super::*;

    /// `/worktree` — where am I, and what else exists.
    pub(crate) async fn overview() -> String {
        let cwd = match std::env::current_dir() {
            Ok(c) => c,
            Err(e) => return format!("Cannot read the current directory: {e}"),
        };
        let Some(repo) = cortex::git::find_repo_root(&cwd) else {
            return "Not inside a git repository — worktrees need one.".to_string();
        };
        match worktree::list(&repo).await {
            Ok(wts) => {
                let root = canonical(&worktree::worktrees_root().unwrap_or_default());
                format_overview(&canonical(&cwd), &wts, &root)
            }
            Err(e) => format!("Could not list worktrees: {e:#}"),
        }
    }

    /// `/worktree new <name>`.
    pub(crate) async fn create(name: &str) -> String {
        let cwd = match std::env::current_dir() {
            Ok(c) => c,
            Err(e) => return format!("Cannot read the current directory: {e}"),
        };
        let Some(repo) = cortex::git::find_repo_root(&cwd) else {
            return "Not inside a git repository — worktrees need one.".to_string();
        };
        let root = match worktree::worktrees_root() {
            Ok(r) => r,
            Err(e) => return format!("Cannot resolve the worktree directory: {e:#}"),
        };
        let opts = AddOptions {
            name: name.to_string(),
            ..Default::default()
        };
        match worktree::add(&repo, &root, &opts).await {
            Ok(wt) => format!(
                "Created worktree '{}' on branch '{}'\n  {}\n\n\
                 This session keeps working where it is. Start one in the worktree with:\n  \
                 borg --worktree {}",
                wt.name(),
                wt.branch.as_deref().unwrap_or("detached"),
                wt.path.display(),
                wt.name(),
            ),
            Err(e) => format!("Could not create worktree '{name}': {e:#}"),
        }
    }

    /// `/worktree remove <name>`.
    pub(crate) async fn delete(target: &str, force: bool) -> String {
        let cwd = match std::env::current_dir() {
            Ok(c) => c,
            Err(e) => return format!("Cannot read the current directory: {e}"),
        };
        let Some(repo) = cortex::git::find_repo_root(&cwd) else {
            return "Not inside a git repository — worktrees need one.".to_string();
        };
        let root = match worktree::worktrees_root() {
            Ok(r) => canonical(&r),
            Err(e) => return format!("Cannot resolve the worktree directory: {e:#}"),
        };
        match worktree::remove(&repo, &root, target, force).await {
            Ok(path) => format!(
                "Removed worktree at {}\nThe branch is untouched.",
                path.display()
            ),
            Err(e) => format!("{e:#}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wt(path: &str, branch: Option<&str>) -> Worktree {
        Worktree {
            path: PathBuf::from(path),
            branch: branch.map(str::to_string),
            head: None,
            is_main: false,
            is_bare: false,
            locked: false,
            prunable: false,
        }
    }

    /// The tags are the whole diagnostic value of the listing: they are what
    /// tells an operator why `remove` refused. Dropping one would leave the
    /// refusal looking arbitrary.
    #[test]
    fn listing_tags_every_state_that_changes_what_remove_will_do() {
        let root = Path::new("/home/u/.borg-worktrees");
        let mut main = wt("/code/alpha", Some("main"));
        main.is_main = true;
        let managed = wt("/home/u/.borg-worktrees/alpha/fix", Some("fix"));
        let mut locked = wt("/home/u/.borg-worktrees/alpha/pinned", Some("pinned"));
        locked.locked = true;
        let mut stale = wt("/home/u/.borg-worktrees/alpha/stale", Some("stale"));
        stale.prunable = true;
        let external = wt("/elsewhere/hand-made", Some("hand"));
        let wts = [main, managed, locked, stale, external];

        let rows: Vec<String> = wts.iter().map(|w| format_row(w, root, 8)).collect();
        assert!(rows[0].contains("[main]"), "{}", rows[0]);
        assert!(
            !rows[1].contains('['),
            "an ordinary managed worktree needs no tag: {}",
            rows[1]
        );
        assert!(rows[2].contains("locked"), "{}", rows[2]);
        assert!(rows[3].contains("stale"), "{}", rows[3]);
        assert!(
            rows[4].contains("external"),
            "a worktree outside the managed root must be marked so removal is not assumed: {}",
            rows[4]
        );
    }

    /// The "you are here" marker is what tells an operator whether this session
    /// is isolated. If it pointed at the wrong checkout, a session editing the
    /// main tree would look safely sandboxed.
    #[test]
    fn the_overview_marks_the_worktree_the_session_is_actually_in() {
        let root = Path::new("/home/u/.borg-worktrees");
        let mut main = wt("/code/alpha", Some("main"));
        main.is_main = true;
        let fix = wt("/home/u/.borg-worktrees/alpha/fix", Some("fix"));
        let other = wt("/home/u/.borg-worktrees/alpha/spike", Some("spike"));
        let wts = [main, fix, other];

        let text = format_overview(
            Path::new("/home/u/.borg-worktrees/alpha/fix/src"),
            &wts,
            root,
        );
        assert!(
            text.starts_with("Working in worktree 'fix' (fix)"),
            "the header must name the worktree, got: {text}"
        );
        let marked: Vec<&str> = text.lines().filter(|l| l.contains("<- here")).collect();
        assert_eq!(marked.len(), 1, "exactly one row is 'here': {text}");
        assert!(marked[0].contains("/alpha/fix "), "{}", marked[0]);
    }

    /// Working in the main checkout is the unsafe-by-default case, so it must
    /// be stated plainly rather than shown as just another worktree.
    #[test]
    fn the_overview_says_when_the_session_is_in_the_main_checkout() {
        let mut main = wt("/code/alpha", Some("main"));
        main.is_main = true;
        let wts = [main, wt("/home/u/.borg-worktrees/alpha/fix", Some("fix"))];
        let text = format_overview(
            Path::new("/code/alpha/crates"),
            &wts,
            Path::new("/home/u/.borg-worktrees"),
        );
        assert!(
            text.starts_with("Working in the main checkout (main)"),
            "got: {text}"
        );
    }

    /// A worktree nested inside another checkout's directory must resolve to
    /// the innermost one — the outer prefix also matches, and picking it would
    /// report the wrong branch.
    #[test]
    fn the_overview_picks_the_innermost_matching_checkout() {
        let mut main = wt("/code/alpha", Some("main"));
        main.is_main = true;
        let nested = wt("/code/alpha/nested-wt", Some("nested"));
        let wts = [main, nested];
        let text = format_overview(
            Path::new("/code/alpha/nested-wt/src"),
            &wts,
            Path::new("/root"),
        );
        assert!(
            text.starts_with("Working in worktree 'nested-wt' (nested)"),
            "got: {text}"
        );
    }

    /// A repo with only its main checkout should not print an empty list — the
    /// header plus the two next actions is the whole useful answer.
    #[test]
    fn the_overview_skips_the_listing_when_there_is_only_one_checkout() {
        let mut main = wt("/code/alpha", Some("main"));
        main.is_main = true;
        let text = format_overview(Path::new("/code/alpha"), &[main], Path::new("/root"));
        assert!(!text.contains("<- here"), "nothing to disambiguate: {text}");
        assert!(text.contains("/worktree new <name>"), "{text}");
    }

    /// A detached HEAD has no branch. Printing an empty column would read as a
    /// rendering bug rather than a real state.
    #[test]
    fn a_detached_worktree_says_so_instead_of_printing_nothing() {
        let row = format_row(&wt("/wt/d", None), Path::new("/root"), 4);
        assert!(row.contains("(detached)"), "{row}");
    }

    /// The name column must line up so the listing stays scannable, including
    /// when a name is longer than the requested width.
    #[test]
    fn rows_pad_to_the_name_column_width() {
        let row = format_row(&wt("/code/a", Some("main")), Path::new("/root"), 10);
        assert!(
            row.starts_with("a         "),
            "short names must be padded to the column width: {row:?}"
        );

        let long = wt("/code/a-very-long-worktree-name", Some("main"));
        let row = format_row(&long, Path::new("/root"), 4);
        assert!(
            row.starts_with("a-very-long-worktree-name  "),
            "an over-long name must not be truncated: {row:?}"
        );
    }
}
