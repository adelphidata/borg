//! Async filesystem walker for the @-mention popup.
//!
//! The walker runs on a background tokio task so keystrokes never block the
//! render thread. The composer publishes queries via a `watch` channel
//! (newest value always wins); the service debounces 70ms, dispatches the
//! walk onto `spawn_blocking` (since `ignore::Walk` is synchronous), and
//! posts results back on an unbounded mpsc that the App drains every tick.

use std::path::{Path, PathBuf, MAIN_SEPARATOR};
use std::time::Duration;

use ignore::WalkBuilder;
use tokio::sync::{mpsc, watch};

use super::file_popup::{is_path_like, resolve_path_fragment, split_parent_fragment, FileMatch};

/// Results posted by the background task. `query` is the query these matches
/// correspond to — the popup uses it to drop stale results.
pub struct FileSearchResult {
    pub query: String,
    pub matches: Vec<FileMatch>,
}

/// Debounce window. New queries arriving within this window reset the timer.
const DEBOUNCE: Duration = Duration::from_millis(70);

/// Max results per query. Matches the pre-async behavior (50-entry cap).
const MAX_RESULTS: usize = 50;

/// Max recursion depth for fuzzy walks. Matches the pre-async behavior.
const MAX_FUZZY_DEPTH: usize = 8;

/// Owned by the App. Publishes queries, drains results. `None` channels
/// when no tokio runtime is available at construction time (e.g. sync unit
/// tests that build an `App` off-runtime); keeps `on_query` a cheap no-op
/// in that case rather than log-spamming once per keystroke.
pub struct FileSearchService {
    query_tx: Option<watch::Sender<String>>,
    result_rx: Option<mpsc::UnboundedReceiver<FileSearchResult>>,
}

impl FileSearchService {
    /// Spawn the background search task. Keeps running until the service is
    /// dropped (which drops the watch sender, which signals the task to exit).
    ///
    /// If no tokio runtime is available (e.g. synchronous unit tests that
    /// construct `App` without `#[tokio::test]`), the service is inert —
    /// `on_query` / `drain_results` are no-ops instead of panicking.
    pub fn spawn(cwd: PathBuf, blocked_paths: Vec<String>) -> Self {
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            tracing::debug!("file_search: no tokio runtime available, service will be inert");
            return Self {
                query_tx: None,
                result_rx: None,
            };
        };
        let (query_tx, mut query_rx) = watch::channel(String::new());
        // `watch::channel` marks its initial value as *unseen*, so the
        // first `changed().await` would fire immediately and trigger a
        // walk for the empty query on startup. Marking unchanged here
        // (synchronously, before any concurrent send is possible) skips
        // that spurious initial walk without risking that an early
        // keystroke gets silently consumed.
        query_rx.mark_unchanged();
        let (result_tx, result_rx) = mpsc::unbounded_channel();
        handle.spawn(run_search_loop(query_rx, result_tx, cwd, blocked_paths));
        Self {
            query_tx: Some(query_tx),
            result_rx: Some(result_rx),
        }
    }

    /// Publish a new query. Non-blocking. Older pending queries are
    /// superseded by the newest value (watch-channel semantics).
    pub fn on_query(&self, q: &str) {
        let Some(tx) = self.query_tx.as_ref() else {
            return;
        };
        if tx.send(q.to_string()).is_err() {
            // Normal during TUI mode-switch / shutdown.
            tracing::debug!("file_search: background task exited, query dropped");
        }
    }

    /// Drain any results the background task has posted since the last call.
    /// Called from the App tick loop.
    pub fn drain_results(&mut self) -> Vec<FileSearchResult> {
        let mut out = Vec::new();
        if let Some(rx) = self.result_rx.as_mut() {
            while let Ok(r) = rx.try_recv() {
                out.push(r);
            }
        }
        out
    }
}

async fn run_search_loop(
    mut query_rx: watch::Receiver<String>,
    result_tx: mpsc::UnboundedSender<FileSearchResult>,
    cwd: PathBuf,
    blocked_paths: Vec<String>,
) {
    loop {
        if query_rx.changed().await.is_err() {
            tracing::debug!("file_search: query channel closed, exiting");
            return;
        }

        // Debounce: keep resetting the timer as long as new queries arrive.
        loop {
            tokio::select! {
                res = query_rx.changed() => {
                    if res.is_err() {
                        tracing::debug!("file_search: query channel closed, exiting");
                        return;
                    }
                }
                _ = tokio::time::sleep(DEBOUNCE) => break,
            }
        }

        let query = query_rx.borrow_and_update().clone();
        let cwd_clone = cwd.clone();
        let blocked = blocked_paths.clone();
        let q_for_walk = query.clone();

        let matches =
            match tokio::task::spawn_blocking(move || run_walk(&q_for_walk, &cwd_clone, &blocked))
                .await
            {
                Ok(m) => m,
                Err(e) => {
                    tracing::warn!("file_search: blocking walk failed: {e}");
                    Vec::new()
                }
            };

        if result_tx.send(FileSearchResult { query, matches }).is_err() {
            tracing::debug!("file_search: result channel closed, exiting");
            return;
        }
    }
}

/// Dispatch on the query shape: path-like goes through single-level
/// `read_dir` completion; bare names go through a recursive `ignore::Walk`.
pub(crate) fn run_walk(query: &str, cwd: &Path, blocked_paths: &[String]) -> Vec<FileMatch> {
    if is_path_like(query) {
        walk_path(query, cwd, blocked_paths)
    } else {
        walk_fuzzy(query, cwd, blocked_paths)
    }
}

fn walk_fuzzy(query: &str, cwd: &Path, blocked_paths: &[String]) -> Vec<FileMatch> {
    let query_lower = query.to_lowercase();
    let walker = WalkBuilder::new(cwd)
        .max_depth(Some(MAX_FUZZY_DEPTH))
        .hidden(true)
        .build();

    // Files whose own name matches are ranked ahead of files that match only
    // through a directory, before the cap applies: in walk order, a directory
    // named like the query would fill every slot and hide the files actually
    // named like it. The walk stops once enough names match.
    let mut by_name: Vec<(NameMatch, FileMatch)> = Vec::new();
    let mut by_dir: Vec<FileMatch> = Vec::new();
    for entry in walker.flatten() {
        if by_name.len() >= MAX_RESULTS {
            break;
        }
        if entry.file_type().is_some_and(|ft| ft.is_dir()) {
            continue;
        }
        let path = entry.path();
        let rel = path.strip_prefix(cwd).unwrap_or(path).to_string_lossy();
        if rel.is_empty() {
            continue;
        }
        if is_blocked(path, blocked_paths) {
            continue;
        }
        let found = || FileMatch {
            display: rel.to_string(),
            full_path: path.to_path_buf(),
            is_dir: false,
        };
        let name = entry.file_name().to_string_lossy().to_lowercase();
        if let Some(rank) = NameMatch::of(&name, &query_lower) {
            by_name.push((rank, found()));
        } else if by_dir.len() < MAX_RESULTS && rel.to_lowercase().contains(&query_lower) {
            by_dir.push(found());
        }
    }
    // Stable, so walk order holds within a rank.
    by_name.sort_by_key(|(rank, _)| *rank);
    by_name
        .into_iter()
        .map(|(_, found)| found)
        .chain(by_dir)
        .take(MAX_RESULTS)
        .collect()
}

/// How a file's own name matches a bare-name query, strongest first.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum NameMatch {
    Exact,
    Prefix,
    Contains,
}

impl NameMatch {
    /// Both sides already lowercased. An empty query is a prefix of every
    /// name, so it lists files in walk order.
    fn of(name: &str, query: &str) -> Option<Self> {
        if name == query {
            Some(Self::Exact)
        } else if name.starts_with(query) {
            Some(Self::Prefix)
        } else if name.contains(query) {
            Some(Self::Contains)
        } else {
            None
        }
    }
}

fn walk_path(query: &str, cwd: &Path, blocked_paths: &[String]) -> Vec<FileMatch> {
    let (parent_frag, name_frag) = split_parent_fragment(query);
    let Some(resolved_parent) = resolve_path_fragment(parent_frag, cwd) else {
        return Vec::new();
    };
    let read_dir = match std::fs::read_dir(&resolved_parent) {
        Ok(rd) => rd,
        Err(_) => return Vec::new(),
    };

    let name_lower = name_frag.to_lowercase();

    let display_parent: String = if name_frag.is_empty() {
        query.to_string()
    } else {
        query[..query.len() - name_frag.len()].to_string()
    };

    let mut entries: Vec<(String, PathBuf, bool)> = Vec::new();
    for entry in read_dir.flatten() {
        let file_name = entry.file_name();
        let name = file_name.to_string_lossy().to_string();
        if !name_lower.is_empty() && !name.to_lowercase().starts_with(&name_lower) {
            continue;
        }
        let full_path = entry.path();
        if is_blocked(&full_path, blocked_paths) {
            continue;
        }
        let is_dir = entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false);
        entries.push((name, full_path, is_dir));
    }

    entries.sort_by(|a, b| {
        b.2.cmp(&a.2)
            .then_with(|| a.0.to_lowercase().cmp(&b.0.to_lowercase()))
    });
    entries.truncate(MAX_RESULTS);

    entries
        .into_iter()
        .map(|(name, full_path, is_dir)| {
            let mut display = format!("{display_parent}{name}");
            if is_dir {
                display.push(MAIN_SEPARATOR);
            }
            FileMatch {
                display,
                full_path,
                is_dir,
            }
        })
        .collect()
}

fn is_blocked(path: &Path, blocked_paths: &[String]) -> bool {
    if blocked_paths.is_empty() {
        return false;
    }
    let path_str = path.to_string_lossy();
    blocked_paths.iter().any(|blocked| {
        path.components().any(|c| c.as_os_str() == blocked.as_str())
            || path_str.contains(blocked.as_str())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::Duration;
    use tempfile::TempDir;
    use tokio::time::timeout;

    async fn drain_until_query(
        svc: &mut FileSearchService,
        want: &str,
        budget: Duration,
    ) -> Option<FileSearchResult> {
        let deadline = tokio::time::Instant::now() + budget;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return None;
            }
            let rx = svc.result_rx.as_mut().expect("runtime is present");
            match timeout(remaining, rx.recv()).await {
                Ok(Some(r)) if r.query == want => return Some(r),
                Ok(Some(_)) => continue, // drop stale, keep waiting
                Ok(None) => return None,
                Err(_) => return None,
            }
        }
    }

    /// A directory named like the query must not crowd the files named like
    /// it out of the capped list: those rank first — exact name, then prefix,
    /// then contains — and directory-only matches fill what is left.
    #[test]
    fn fuzzy_walk_ranks_filename_matches_above_directory_matches() {
        let tmp = TempDir::new().unwrap();
        let crowd = tmp.path().join("viewmodels");
        fs::create_dir(&crowd).unwrap();
        for i in 0..MAX_RESULTS + 10 {
            fs::write(crowd.join(format!("item{i}.rs")), "x").unwrap();
        }
        let lib = tmp.path().join("lib");
        fs::create_dir(&lib).unwrap();
        for name in [
            "ChatViewModel.kt",
            "viewmodel_test.rs",
            "viewmodel",
            "other.rs",
        ] {
            fs::write(lib.join(name), "x").unwrap();
        }

        let found = run_walk("ViewModel", tmp.path(), &[]);
        let names: Vec<String> = found
            .iter()
            .map(|m| {
                m.full_path
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        assert_eq!(
            names[..3],
            ["viewmodel", "viewmodel_test.rs", "ChatViewModel.kt"],
            "{names:?}"
        );
        assert_eq!(found.len(), MAX_RESULTS, "directory matches fill the rest");
        assert!(found[3..].iter().all(|m| m.full_path.starts_with(&crowd)));
    }

    #[tokio::test]
    async fn end_to_end_search_returns_matches() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("alpha.txt"), "x").unwrap();
        fs::write(tmp.path().join("beta.txt"), "x").unwrap();
        let mut svc = FileSearchService::spawn(tmp.path().to_path_buf(), Vec::new());

        svc.on_query("alpha");
        let result = drain_until_query(&mut svc, "alpha", Duration::from_secs(2))
            .await
            .expect("background task produced results");

        assert_eq!(result.query, "alpha");
        assert!(
            result
                .matches
                .iter()
                .any(|m| m.display.ends_with("alpha.txt")),
            "expected alpha.txt in results, got {:?}",
            result
                .matches
                .iter()
                .map(|m| &m.display)
                .collect::<Vec<_>>(),
        );
        assert!(
            !result
                .matches
                .iter()
                .any(|m| m.display.ends_with("beta.txt")),
            "beta.txt should not match 'alpha' query",
        );
    }

    #[tokio::test]
    async fn newest_query_wins_under_rapid_typing() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("alpha.txt"), "x").unwrap();
        fs::write(tmp.path().join("zebra.txt"), "x").unwrap();
        let mut svc = FileSearchService::spawn(tmp.path().to_path_buf(), Vec::new());

        // Three queries within one debounce window: only the last should run.
        svc.on_query("a");
        svc.on_query("al");
        svc.on_query("zebra");

        let result = drain_until_query(&mut svc, "zebra", Duration::from_secs(2))
            .await
            .expect("background task produced results");

        assert_eq!(result.query, "zebra");
        assert!(result
            .matches
            .iter()
            .any(|m| m.display.ends_with("zebra.txt")));

        // No further result batches should be queued (debounce collapsed the
        // three on_query calls into one walk).
        let rx = svc.result_rx.as_mut().expect("runtime is present");
        let leftover = timeout(Duration::from_millis(200), rx.recv()).await;
        assert!(
            leftover.is_err(),
            "expected no additional result batches, got {:?}",
            leftover.ok().flatten().map(|r| r.query)
        );
    }
}
