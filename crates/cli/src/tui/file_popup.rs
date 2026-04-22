use std::path::{Path, PathBuf};

use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;

use super::theme;

pub struct FileMatch {
    /// Human-readable path shown in the popup and inserted into the composer.
    /// Preserves the user's typed prefix form (e.g. `~/Documents/foo.txt`).
    pub display: String,
    /// Fully resolved absolute path used for reading file contents downstream.
    pub full_path: PathBuf,
    /// True when the entry is a directory. Directory selections keep the
    /// popup open so the user can drill further into the tree.
    pub is_dir: bool,
}

/// UI state for the @-mention file picker. All filesystem I/O happens on a
/// background task (see `file_search::FileSearchService`); this type owns
/// only the visible state and the three-field machinery (`pending_query`,
/// `display_query`, `waiting`) that lets it drop stale results.
pub struct FileSearchPopup {
    visible: bool,
    /// Latest query the user has typed. Used to drop stale results: if a
    /// result batch arrives for an older query, it's discarded.
    pending_query: String,
    /// Query whose results we're currently showing. Lags `pending_query`
    /// while a walk is in flight.
    display_query: String,
    /// True when `pending_query != display_query` — a walk is in flight
    /// and we haven't shown its results yet.
    waiting: bool,
    selected: usize,
    results: Vec<FileMatch>,
}

impl FileSearchPopup {
    pub fn new() -> Self {
        Self {
            visible: false,
            pending_query: String::new(),
            display_query: String::new(),
            waiting: false,
            selected: 0,
            results: Vec::new(),
        }
    }

    pub fn is_visible(&self) -> bool {
        self.visible
    }

    /// Record a new user query. Does NOT touch the filesystem — the caller
    /// must also push `q` into `FileSearchService::on_query`. Resets
    /// selection to the top and flips `waiting` when the new query differs
    /// from the currently-displayed one.
    pub fn set_pending_query(&mut self, q: &str) {
        self.visible = true;
        self.selected = 0;
        if self.pending_query != q {
            self.pending_query = q.to_string();
        }
        if self.pending_query != self.display_query {
            self.waiting = true;
        }
    }

    /// Apply a result batch posted by the background walker. Results for a
    /// query that no longer matches `pending_query` are dropped — this is
    /// the core stale-result guard.
    pub fn apply_results(&mut self, query: String, matches: Vec<FileMatch>) {
        if query != self.pending_query {
            return;
        }
        self.results = matches;
        self.display_query = query;
        self.waiting = false;
        if self.selected >= self.results.len() {
            self.selected = 0;
        }
    }

    pub fn move_up(&mut self) {
        if self.results.is_empty() {
            return;
        }
        if self.selected == 0 {
            self.selected = self.results.len() - 1;
        } else {
            self.selected -= 1;
        }
    }

    pub fn move_down(&mut self) {
        if self.results.is_empty() {
            return;
        }
        self.selected = (self.selected + 1) % self.results.len();
    }

    pub fn selected_file(&self) -> Option<&FileMatch> {
        self.results.get(self.selected)
    }

    pub fn dismiss(&mut self) {
        self.visible = false;
        self.selected = 0;
        self.pending_query.clear();
        self.display_query.clear();
        self.waiting = false;
        self.results.clear();
    }

    pub fn render(&self, frame: &mut Frame, composer_area: Rect) {
        if !self.visible {
            return;
        }

        // Empty + not waiting: display_query produced no matches. Show nothing.
        if self.results.is_empty() && !self.waiting {
            return;
        }

        let available_above = composer_area.y;
        if available_above < 3 {
            return;
        }

        // Searching placeholder when we have no prior results to show.
        let show_searching_only = self.results.is_empty() && self.waiting;

        let item_count = if show_searching_only {
            1u16
        } else {
            self.results.len() as u16
        };
        let popup_height = (item_count + 2).min(available_above);
        let max_visible = (popup_height - 2) as usize;
        let popup_width = composer_area.width.min(60);

        let popup_y = composer_area.y - popup_height;
        let popup_area = Rect::new(composer_area.x, popup_y, popup_width, popup_height);

        frame.render_widget(Clear, popup_area);

        let lines: Vec<Line<'_>> = if show_searching_only {
            vec![Line::from(Span::styled(" Searching…", theme::dim()))]
        } else {
            let scroll_offset = if self.selected >= max_visible {
                self.selected - max_visible + 1
            } else {
                0
            };
            self.results
                .iter()
                .skip(scroll_offset)
                .take(max_visible)
                .enumerate()
                .map(|(i, file)| {
                    let actual_index = i + scroll_offset;
                    let style = if actual_index == self.selected {
                        theme::popup_selected()
                    } else {
                        theme::dim()
                    };
                    Line::from(Span::styled(format!(" {}", file.display), style))
                })
                .collect()
        };

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(theme::dim())
            .title(" Files ");

        let paragraph = Paragraph::new(lines).block(block);
        frame.render_widget(paragraph, popup_area);
    }
}

/// Returns true if the query looks like a filesystem path rather than a fuzzy
/// filename search. Triggers path-mode completion against a resolved parent
/// directory.
pub(crate) fn is_path_like(query: &str) -> bool {
    if query.is_empty() {
        return false;
    }
    if query.starts_with('~') || query.starts_with('/') {
        return true;
    }
    if query.starts_with("./") || query.starts_with("../") {
        return true;
    }
    if query.chars().any(std::path::is_separator) {
        return true;
    }
    // Windows drive prefix: `C:\` or `C:/`.
    let bytes = query.as_bytes();
    if bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'/' || bytes[2] == b'\\')
    {
        return true;
    }
    false
}

/// Split a path-like query into (parent_fragment, name_fragment) at the last
/// separator. If the query ends with a separator, the name fragment is empty.
/// If the query has no separator, the entire query is the name fragment and
/// the parent is empty.
pub(crate) fn split_parent_fragment(query: &str) -> (&str, &str) {
    let last_sep = query
        .char_indices()
        .rev()
        .find(|(_, c)| std::path::is_separator(*c));
    match last_sep {
        Some((idx, c)) => {
            let split = idx + c.len_utf8();
            (&query[..split], &query[split..])
        }
        None => ("", query),
    }
}

/// Resolve a parent-path fragment to an absolute directory:
/// - `~` or `~/...` expand via `dirs::home_dir()`
/// - relative fragments resolve against `cwd`
/// - absolute fragments are returned as-is
/// - empty fragment means "current working directory"
pub(crate) fn resolve_path_fragment(fragment: &str, cwd: &Path) -> Option<PathBuf> {
    if fragment.is_empty() {
        return Some(cwd.to_path_buf());
    }

    if fragment == "~" {
        return dirs::home_dir();
    }
    if let Some(rest) = fragment
        .strip_prefix("~/")
        .or_else(|| fragment.strip_prefix("~\\"))
    {
        let home = dirs::home_dir()?;
        return Some(home.join(rest));
    }

    let path = Path::new(fragment);
    if path.is_absolute() {
        Some(path.to_path_buf())
    } else {
        Some(cwd.join(path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    #[test]
    fn new_popup_is_not_visible() {
        let popup = FileSearchPopup::new();
        assert!(!popup.is_visible());
    }

    #[test]
    fn set_pending_query_makes_visible_and_marks_waiting() {
        let mut popup = FileSearchPopup::new();
        popup.set_pending_query("foo");
        assert!(popup.is_visible());
        assert!(popup.waiting);
        assert_eq!(popup.pending_query, "foo");
        assert_eq!(popup.display_query, "");
    }

    #[test]
    fn apply_results_accepts_current_query() {
        let mut popup = FileSearchPopup::new();
        popup.set_pending_query("foo");
        popup.apply_results(
            "foo".to_string(),
            vec![FileMatch {
                display: "foo.txt".to_string(),
                full_path: PathBuf::from("/tmp/foo.txt"),
                is_dir: false,
            }],
        );
        assert!(!popup.waiting);
        assert_eq!(popup.display_query, "foo");
        assert_eq!(popup.results.len(), 1);
    }

    #[test]
    fn apply_results_drops_stale_batch() {
        let mut popup = FileSearchPopup::new();
        popup.set_pending_query("bar");
        // Stale batch from an earlier "foo" query.
        popup.apply_results(
            "foo".to_string(),
            vec![FileMatch {
                display: "foo.txt".to_string(),
                full_path: PathBuf::from("/tmp/foo.txt"),
                is_dir: false,
            }],
        );
        assert!(popup.results.is_empty(), "stale results must be dropped");
        assert!(popup.waiting, "still waiting for 'bar'");
        assert_eq!(popup.display_query, "");
    }

    #[test]
    fn dismiss_clears_all_state() {
        let mut popup = FileSearchPopup::new();
        popup.set_pending_query("foo");
        popup.apply_results(
            "foo".to_string(),
            vec![FileMatch {
                display: "foo.txt".to_string(),
                full_path: PathBuf::from("/tmp/foo.txt"),
                is_dir: false,
            }],
        );
        popup.dismiss();
        assert!(!popup.is_visible());
        assert!(popup.results.is_empty());
        assert_eq!(popup.pending_query, "");
        assert_eq!(popup.display_query, "");
        assert!(!popup.waiting);
    }

    #[test]
    fn up_down_wrapping() {
        let mut popup = FileSearchPopup::new();
        popup.set_pending_query("");
        popup.apply_results(
            String::new(),
            vec![
                FileMatch {
                    display: "a.txt".to_string(),
                    full_path: PathBuf::from("a.txt"),
                    is_dir: false,
                },
                FileMatch {
                    display: "b.txt".to_string(),
                    full_path: PathBuf::from("b.txt"),
                    is_dir: false,
                },
            ],
        );
        assert_eq!(popup.selected, 0);
        popup.move_up();
        assert_eq!(popup.selected, 1);
        popup.move_down();
        assert_eq!(popup.selected, 0);
    }

    #[test]
    fn render_shows_searching_when_waiting_and_empty() {
        let mut popup = FileSearchPopup::new();
        popup.set_pending_query("anything");
        assert!(popup.waiting && popup.results.is_empty());

        // Terminal tall enough to fit the popup above the composer.
        let backend = TestBackend::new(40, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                let composer_area = Rect::new(0, 8, 40, 2);
                popup.render(f, composer_area);
            })
            .unwrap();

        let buffer = terminal.backend().buffer().clone();
        let mut found = false;
        for y in 0..buffer.area().height {
            let mut row = String::new();
            for x in 0..buffer.area().width {
                row.push_str(buffer.cell((x, y)).unwrap().symbol());
            }
            if row.contains("Searching") {
                found = true;
                break;
            }
        }
        assert!(found, "expected 'Searching' row to render while waiting");
    }

    // ------------------- helper fn tests -------------------

    #[test]
    fn is_path_like_detects_tilde() {
        assert!(is_path_like("~"));
        assert!(is_path_like("~/"));
        assert!(is_path_like("~/Documents"));
    }

    #[test]
    fn is_path_like_detects_absolute() {
        assert!(is_path_like("/etc"));
        assert!(is_path_like("/etc/hosts"));
    }

    #[test]
    fn is_path_like_detects_relative_dot_prefix() {
        assert!(is_path_like("./foo"));
        assert!(is_path_like("../bar"));
    }

    #[test]
    fn is_path_like_detects_embedded_separator() {
        assert!(is_path_like("foo/bar"));
    }

    #[test]
    fn is_path_like_detects_windows_drive() {
        assert!(is_path_like("C:/Users"));
        assert!(is_path_like("C:\\Users"));
        assert!(is_path_like("d:/x"));
    }

    #[test]
    fn is_path_like_rejects_bare_names() {
        assert!(!is_path_like("foo"));
        assert!(!is_path_like("Cargo"));
        assert!(!is_path_like("my_file.txt"));
        assert!(!is_path_like(""));
    }

    #[test]
    fn split_no_separator() {
        assert_eq!(split_parent_fragment("foo"), ("", "foo"));
        assert_eq!(split_parent_fragment(""), ("", ""));
    }

    #[test]
    fn split_trailing_separator() {
        assert_eq!(split_parent_fragment("~/"), ("~/", ""));
        assert_eq!(split_parent_fragment("/etc/"), ("/etc/", ""));
        assert_eq!(split_parent_fragment("foo/bar/"), ("foo/bar/", ""));
    }

    #[test]
    fn split_mid_path() {
        assert_eq!(split_parent_fragment("~/Doc"), ("~/", "Doc"));
        assert_eq!(split_parent_fragment("/etc/ho"), ("/etc/", "ho"));
        assert_eq!(split_parent_fragment("./foo/ba"), ("./foo/", "ba"));
    }

    #[cfg(windows)]
    #[test]
    fn split_windows_backslash() {
        assert_eq!(split_parent_fragment("~\\Doc"), ("~\\", "Doc"));
        assert_eq!(
            split_parent_fragment("C:\\Users\\me\\fo"),
            ("C:\\Users\\me\\", "fo")
        );
    }

    #[test]
    fn resolve_empty_is_cwd() {
        let cwd = PathBuf::from("/tmp/cwd");
        assert_eq!(resolve_path_fragment("", &cwd), Some(cwd));
    }

    #[test]
    fn resolve_bare_tilde() {
        let cwd = PathBuf::from("/tmp");
        let resolved = resolve_path_fragment("~", &cwd);
        assert_eq!(resolved, dirs::home_dir());
    }

    #[test]
    fn resolve_tilde_slash_prefix() {
        let cwd = PathBuf::from("/tmp");
        let resolved = resolve_path_fragment("~/Documents/", &cwd).unwrap();
        let mut expected = dirs::home_dir().unwrap();
        expected.push("Documents/");
        assert_eq!(resolved, expected);
    }

    #[cfg(unix)]
    #[test]
    fn resolve_absolute_path() {
        let cwd = PathBuf::from("/tmp");
        assert_eq!(
            resolve_path_fragment("/etc/", &cwd),
            Some(PathBuf::from("/etc/"))
        );
    }

    #[test]
    fn resolve_relative_joins_cwd() {
        let cwd = PathBuf::from("/tmp/project");
        assert_eq!(
            resolve_path_fragment("src/", &cwd),
            Some(PathBuf::from("/tmp/project/src/"))
        );
    }
}
