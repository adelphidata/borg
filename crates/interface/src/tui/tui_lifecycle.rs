//! Terminal lifecycle wrapper for borg's inline-viewport TUI.
//!
//! This module owns:
//! - Entering/leaving raw mode + bracketed paste + keyboard enhancement.
//! - The xterm Alternate Scroll Mode (`?1007h`) escape used in alt-screen surfaces.
//! - The `Tui` wrapper around `custom_terminal::Terminal` that drives inline-viewport
//!   rendering, history flushing, and alt-screen transitions for full-screen popups.
//!
//! It is intentionally narrower than the upstream reference: borg has its own event loop,
//! frame ticking, signal handling, suspension, and notification stack, so those subsystems
//! are not ported here.

use std::fmt;
use std::io::stdout;
use std::io::Result;
use std::io::Stdout;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use crossterm::event::DisableBracketedPaste;
use crossterm::event::DisableFocusChange;
use crossterm::event::EnableBracketedPaste;
use crossterm::event::EnableFocusChange;
use crossterm::event::KeyboardEnhancementFlags;
use crossterm::event::PopKeyboardEnhancementFlags;
use crossterm::event::PushKeyboardEnhancementFlags;
use crossterm::execute;
use crossterm::terminal::disable_raw_mode;
use crossterm::terminal::enable_raw_mode;
use crossterm::terminal::EnterAlternateScreen;
use crossterm::terminal::LeaveAlternateScreen;
use crossterm::Command;
use ratatui::backend::Backend;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use ratatui::layout::Size;
use ratatui::text::Line;

use super::custom_terminal;
use super::custom_terminal::Terminal as CustomTerminal;

const DISABLE_KEYBOARD_ENHANCEMENT_ENV_VAR: &str = "BORG_TUI_DISABLE_KEYBOARD_ENHANCEMENT";

/// Type alias used by the TUI entry points and signal handlers.
pub type Terminal = CustomTerminal<CrosstermBackend<Stdout>>;

// ============================================================================
// IMPORTANT: Native text selection MUST work in this TUI. DO NOT regress.
// ============================================================================
// We enable ONLY xterm "Alternate Scroll Mode" (?1007h) when entering an
// alt-screen surface. The terminal itself translates wheel events into
// CUR_UP / CUR_DOWN key sequences. We never enable mouse tracking modes, so
// click+drag selection stays handled entirely by the terminal.
//
// FORBIDDEN — DO NOT ADD any of these (each one will regress text selection):
//   - ?1000h  Normal button tracking
//   - ?1002h  Button-event (drag) tracking
//   - ?1003h  Any-event tracking
//   - ?1006h  SGR extended coordinates
//   - crossterm::event::EnableMouseCapture / DisableMouseCapture
// ============================================================================

/// Enable xterm Alternate Scroll Mode (`?1007h`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct EnableAlternateScroll;

impl Command for EnableAlternateScroll {
    fn write_ansi(&self, f: &mut impl fmt::Write) -> fmt::Result {
        // ?1007h — xterm Alternate Scroll Mode. DO NOT add any other mode here.
        f.write_str("\x1b[?1007h")
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> Result<()> {
        // Windows consoles deliver wheel events through input records without
        // needing an xterm-style mode switch; no-op is correct here.
        Ok(())
    }
}

/// Disable xterm Alternate Scroll Mode (`?1007l`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DisableAlternateScroll;

impl Command for DisableAlternateScroll {
    fn write_ansi(&self, f: &mut impl fmt::Write) -> fmt::Result {
        f.write_str("\x1b[?1007l")
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> Result<()> {
        Ok(())
    }
}

fn keyboard_enhancement_disabled() -> bool {
    let disable_env = std::env::var(DISABLE_KEYBOARD_ENHANCEMENT_ENV_VAR).ok();
    let is_wsl = running_in_wsl();
    let is_vscode_terminal = is_wsl && running_in_vscode_terminal();
    keyboard_enhancement_disabled_for(disable_env.as_deref(), is_wsl, is_vscode_terminal)
}

fn keyboard_enhancement_disabled_for(
    disable_env: Option<&str>,
    is_wsl: bool,
    is_vscode_terminal: bool,
) -> bool {
    if let Some(disabled) = parse_bool_env(disable_env) {
        return disabled;
    }
    is_wsl && is_vscode_terminal
}

fn parse_bool_env(value: Option<&str>) -> Option<bool> {
    match value.map(str::trim) {
        Some("1") => Some(true),
        Some(value) if value.eq_ignore_ascii_case("true") => Some(true),
        Some(value) if value.eq_ignore_ascii_case("yes") => Some(true),
        Some("0") => Some(false),
        Some(value) if value.eq_ignore_ascii_case("false") => Some(false),
        Some(value) if value.eq_ignore_ascii_case("no") => Some(false),
        _ => None,
    }
}

fn running_in_wsl() -> bool {
    #[cfg(target_os = "linux")]
    {
        // Heuristic: `/proc/sys/kernel/osrelease` contains "microsoft" on WSL.
        std::fs::read_to_string("/proc/sys/kernel/osrelease")
            .map(|s| s.to_ascii_lowercase().contains("microsoft"))
            .unwrap_or(false)
    }
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

fn running_in_vscode_terminal() -> bool {
    vscode_terminal_detected(
        std::env::var("TERM_PROGRAM").ok().as_deref(),
        windows_term_program().as_deref(),
    )
}

fn vscode_terminal_detected(
    linux_term_program: Option<&str>,
    windows_term_program: Option<&str>,
) -> bool {
    term_program_is_vscode(linux_term_program) || term_program_is_vscode(windows_term_program)
}

fn term_program_is_vscode(value: Option<&str>) -> bool {
    value.is_some_and(|value| value.eq_ignore_ascii_case("vscode"))
}

fn windows_term_program() -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        static WINDOWS_TERM_PROGRAM: std::sync::OnceLock<Option<String>> =
            std::sync::OnceLock::new();
        WINDOWS_TERM_PROGRAM
            .get_or_init(read_windows_term_program)
            .clone()
    }

    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

#[cfg(target_os = "linux")]
fn read_windows_term_program() -> Option<String> {
    let output = std::process::Command::new("cmd.exe")
        .args(["/d", "/s", "/c", "set TERM_PROGRAM"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| {
            line.trim_end_matches('\r')
                .strip_prefix("TERM_PROGRAM=")
                .map(str::to_string)
        })
        .filter(|value| !value.trim().is_empty())
}

/// Apply borg's input modes (raw mode, bracketed paste, focus change, optional kitty
/// keyboard enhancement) to the terminal. Inverse of [`restore`].
pub fn set_modes() -> Result<()> {
    execute!(stdout(), EnableBracketedPaste)?;

    enable_raw_mode()?;
    // Enable kitty keyboard enhancement so modifiers for keys like Enter are
    // disambiguated. Some terminals (legacy Windows consoles) do not support
    // the push; ignore failures and continue with default key reporting.
    if !keyboard_enhancement_disabled() {
        let _ = execute!(
            stdout(),
            PushKeyboardEnhancementFlags(
                KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                    | KeyboardEnhancementFlags::REPORT_EVENT_TYPES
                    | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS
            )
        );
    }

    let _ = execute!(stdout(), EnableFocusChange);
    Ok(())
}

fn restore_common(should_disable_raw_mode: bool) -> Result<()> {
    // Pop may fail on platforms that did not accept the push; ignore errors.
    let _ = execute!(stdout(), PopKeyboardEnhancementFlags);
    execute!(stdout(), DisableBracketedPaste)?;
    let _ = execute!(stdout(), DisableFocusChange);
    if should_disable_raw_mode {
        disable_raw_mode()?;
    }
    let _ = execute!(stdout(), crossterm::cursor::Show);
    Ok(())
}

/// Restore the terminal to its original state. Inverse of [`set_modes`].
pub fn restore() -> Result<()> {
    restore_common(true)
}

/// Inline-viewport TUI wrapper.
///
/// Owns the [`custom_terminal::Terminal`] plus the bookkeeping needed to insert
/// finalized history rows into terminal scrollback and to bounce in/out of an
/// alt-screen surface for full-screen popups.
pub struct Tui {
    pub(crate) terminal: Terminal,
    pending_history_lines: Vec<Line<'static>>,
    alt_saved_viewport: Option<Rect>,
    alt_screen_active: Arc<AtomicBool>,
    /// When false, [`Tui::enter_alt_screen`] is a no-op.
    /// Zellij and tmux fall back to scrollback newline emission, where toggling
    /// the alt screen would lose history.
    alt_screen_enabled: bool,
    /// True when running inside Zellij. Selects the newline-fallback escape path
    /// in [`super::insert_history`] because Zellij ignores `DECSTBM`.
    is_zellij: bool,
    /// True when running inside tmux. tmux DECSTBM behavior is inconsistent
    /// across versions; treat it the same as Zellij for safety.
    is_tmux: bool,
}

impl Tui {
    pub fn new(terminal: Terminal) -> Result<Self> {
        let is_zellij = std::env::var("ZELLIJ_SESSION_NAME").is_ok();
        let is_tmux = std::env::var("TMUX").is_ok();
        let alt_screen_enabled = !(is_zellij || is_tmux);
        Ok(Self {
            terminal,
            pending_history_lines: Vec::new(),
            alt_saved_viewport: None,
            alt_screen_active: Arc::new(AtomicBool::new(false)),
            alt_screen_enabled,
            is_zellij,
            is_tmux,
        })
    }

    pub fn is_alt_screen_active(&self) -> bool {
        self.alt_screen_active.load(Ordering::Relaxed)
    }

    /// Whether the terminal multiplexer requires the newline-fallback insert path.
    fn uses_newline_fallback(&self) -> bool {
        self.is_zellij || self.is_tmux
    }

    pub fn terminal_size(&self) -> Result<Size> {
        self.terminal.size()
    }

    /// Buffer `lines` to be flushed above the viewport on the next [`Tui::draw`].
    pub fn insert_history_lines(&mut self, lines: Vec<Line<'static>>) {
        self.pending_history_lines.extend(lines);
    }

    /// Purge terminal scrollback + visible screen and snap the inline viewport
    /// back to row 0. Used by `/clear` and `Ctrl+L`: in inline-viewport mode
    /// the terminal — not the app — owns finalized history, so clearing
    /// `app.cells` alone leaves the prior transcript visible in scrollback.
    pub fn clear_terminal_scrollback(&mut self) -> Result<()> {
        self.pending_history_lines.clear();
        self.terminal.clear_scrollback_and_visible_screen_ansi()?;
        let area = self.terminal.viewport_area;
        self.terminal
            .set_viewport_area(Rect::new(0, 0, area.width, area.height));
        Ok(())
    }

    /// Enter alternate screen and expand the viewport to full terminal size,
    /// saving the current inline viewport for restoration when leaving.
    pub fn enter_alt_screen(&mut self) -> Result<()> {
        if !self.alt_screen_enabled {
            return Ok(());
        }
        let _ = execute!(self.terminal.backend_mut(), EnterAlternateScreen);
        let _ = execute!(self.terminal.backend_mut(), EnableAlternateScroll);
        if let Ok(size) = self.terminal.size() {
            self.alt_saved_viewport = Some(self.terminal.viewport_area);
            self.terminal
                .set_viewport_area(Rect::new(0, 0, size.width, size.height));
            self.terminal.clear()?;
        }
        self.alt_screen_active.store(true, Ordering::Relaxed);
        Ok(())
    }

    /// Leave alternate screen and restore the previously saved inline viewport.
    pub fn leave_alt_screen(&mut self) -> Result<()> {
        if !self.alt_screen_enabled {
            return Ok(());
        }
        let _ = execute!(self.terminal.backend_mut(), DisableAlternateScroll);
        let _ = execute!(self.terminal.backend_mut(), LeaveAlternateScreen);
        if let Some(saved) = self.alt_saved_viewport.take() {
            self.terminal.set_viewport_area(saved);
        }
        // Invariant: leaving the alt-screen surface invalidates ratatui's
        // inline diff buffer. Without this, the next inline draw compares
        // against alt-screen geometry, treats the composer cells as
        // unchanged, and emits no output — typed characters stay invisible
        // until something else (Enter resizing the viewport, history line
        // flush) forces a full repaint. Mirrors the `terminal.clear()` call
        // in `enter_alt_screen`.
        self.terminal.invalidate_viewport();
        self.alt_screen_active.store(false, Ordering::Relaxed);
        Ok(())
    }

    fn alt_screen_viewport(size: Size) -> Rect {
        Rect::new(0, 0, size.width, size.height)
    }

    /// Resize the inline viewport to `height` rows, scrolling content above it
    /// if the viewport would extend past the bottom of the screen. Returns
    /// `true` when the caller must invalidate the diff buffer (newline-fallback
    /// path), since the scroll was performed with raw newlines that ratatui
    /// cannot track.
    fn update_inline_viewport(
        terminal: &mut Terminal,
        height: u16,
        newline_fallback: bool,
    ) -> Result<bool> {
        let size = terminal.size()?;
        let mut needs_full_repaint = false;

        let mut area = terminal.viewport_area;
        area.height = height.min(size.height);
        area.width = size.width;
        if area.bottom() > size.height {
            let scroll_by = area.bottom() - size.height;
            if newline_fallback {
                Self::scroll_newline_fallback_viewport(terminal, size, scroll_by)?;
                needs_full_repaint = true;
            } else {
                terminal
                    .backend_mut()
                    .scroll_region_up(0..area.top(), scroll_by)?;
            }
            area.y = size.height - area.height;
        }
        if area != terminal.viewport_area {
            terminal.clear()?;
            terminal.set_viewport_area(area);
        }

        Ok(needs_full_repaint)
    }

    /// Push content above the viewport upward by `scroll_by` rows using raw
    /// newlines at the screen bottom. Used when the active terminal multiplexer
    /// (Zellij, some tmux versions) does not honor `DECSTBM`.
    fn scroll_newline_fallback_viewport(
        terminal: &mut Terminal,
        size: Size,
        scroll_by: u16,
    ) -> Result<()> {
        crossterm::queue!(
            terminal.backend_mut(),
            crossterm::cursor::MoveTo(0, size.height.saturating_sub(1))
        )?;
        for _ in 0..scroll_by {
            crossterm::queue!(terminal.backend_mut(), crossterm::style::Print("\n"))?;
        }
        Ok(())
    }

    /// Write any buffered history lines above the viewport and clear the buffer.
    /// Returns `true` when newline-fallback mode was used, signaling that the
    /// caller must invalidate the diff buffer for a full repaint.
    fn flush_pending_history_lines(
        terminal: &mut Terminal,
        pending_history_lines: &mut Vec<Line<'static>>,
        newline_fallback: bool,
    ) -> Result<bool> {
        if pending_history_lines.is_empty() {
            return Ok(false);
        }

        super::insert_history::insert_history_lines_with_mode(
            terminal,
            pending_history_lines.clone(),
            super::insert_history::InsertHistoryMode::new(newline_fallback),
        )?;
        pending_history_lines.clear();
        Ok(newline_fallback)
    }

    /// Draw a single frame at the requested viewport `height`. Flushes any
    /// pending history lines above the viewport first, then resizes the inline
    /// viewport to `height`, then invokes `draw_fn` to populate the frame.
    pub fn draw(
        &mut self,
        height: u16,
        draw_fn: impl FnOnce(&mut custom_terminal::Frame),
    ) -> Result<()> {
        if self.is_alt_screen_active() {
            if let Ok(size) = self.terminal.size() {
                let full_area = Self::alt_screen_viewport(size);
                if self.terminal.viewport_area != full_area {
                    self.terminal.clear()?;
                    self.terminal.set_viewport_area(full_area);
                }
            }
            return self.terminal.draw(|frame| {
                draw_fn(frame);
            });
        }

        let newline_fallback = self.uses_newline_fallback();
        let terminal = &mut self.terminal;

        let mut needs_full_repaint =
            Self::update_inline_viewport(terminal, height, newline_fallback)?;
        needs_full_repaint |= Self::flush_pending_history_lines(
            terminal,
            &mut self.pending_history_lines,
            newline_fallback,
        )?;

        if needs_full_repaint {
            terminal.invalidate_viewport();
        }

        terminal.draw(|frame| {
            draw_fn(frame);
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyboard_enhancement_env_flag_parses_common_values() {
        assert_eq!(parse_bool_env(Some("1")), Some(true));
        assert_eq!(parse_bool_env(Some("true")), Some(true));
        assert_eq!(parse_bool_env(Some("YES")), Some(true));
        assert_eq!(parse_bool_env(Some("0")), Some(false));
        assert_eq!(parse_bool_env(Some("false")), Some(false));
        assert_eq!(parse_bool_env(Some("NO")), Some(false));
        assert_eq!(parse_bool_env(Some("unexpected")), None);
        assert_eq!(parse_bool_env(/*value*/ None), None);
    }

    #[test]
    fn keyboard_enhancement_auto_disables_for_vscode_in_wsl() {
        assert!(keyboard_enhancement_disabled_for(
            /*disable_env*/ None, /*is_wsl*/ true, /*is_vscode_terminal*/ true
        ));
    }

    #[test]
    fn keyboard_enhancement_auto_disable_requires_wsl_and_vscode() {
        assert!(!keyboard_enhancement_disabled_for(
            /*disable_env*/ None, /*is_wsl*/ true, /*is_vscode_terminal*/ false
        ));
        assert!(!keyboard_enhancement_disabled_for(
            /*disable_env*/ None, /*is_wsl*/ false, /*is_vscode_terminal*/ true,
        ));
    }

    #[test]
    fn keyboard_enhancement_env_flag_overrides_auto_detection() {
        assert!(!keyboard_enhancement_disabled_for(
            Some("0"),
            /*is_wsl*/ true,
            /*is_vscode_terminal*/ true,
        ));
        assert!(keyboard_enhancement_disabled_for(
            Some("1"),
            /*is_wsl*/ false,
            /*is_vscode_terminal*/ false,
        ));
    }

    #[test]
    fn vscode_terminal_detection_uses_linux_and_windows_term_program() {
        assert!(vscode_terminal_detected(
            Some("vscode"),
            /*windows_term_program*/ None,
        ));
        assert!(vscode_terminal_detected(
            /*linux_term_program*/ None,
            Some("vscode"),
        ));
        assert!(!vscode_terminal_detected(
            /*linux_term_program*/ None,
            Some("WindowsTerminal"),
        ));
        assert!(!vscode_terminal_detected(
            /*linux_term_program*/ None, /*windows_term_program*/ None,
        ));
    }

    #[test]
    fn enable_alternate_scroll_emits_xterm_alt_scroll_escape() {
        let mut buf = String::new();
        EnableAlternateScroll.write_ansi(&mut buf).unwrap();
        assert_eq!(buf, "\x1b[?1007h");
    }

    #[test]
    fn disable_alternate_scroll_emits_xterm_alt_scroll_reset() {
        let mut buf = String::new();
        DisableAlternateScroll.write_ansi(&mut buf).unwrap();
        assert_eq!(buf, "\x1b[?1007l");
    }

    #[test]
    fn alt_screen_viewport_covers_full_terminal() {
        let viewport = Tui::alt_screen_viewport(Size {
            width: 132,
            height: 41,
        });
        assert_eq!(viewport, Rect::new(0, 0, 132, 41));
    }

    /// Regression guard: leaving the alt-screen surface MUST invalidate the
    /// inline diff buffer. Without this, after closing a popup like
    /// `/settings`, ratatui compares the new inline frame against the stale
    /// alt-screen buffer, sees no diff for the composer cells, and emits no
    /// output — typed characters stay invisible until something else (Enter
    /// resizing the viewport, history-line flush) forces a full repaint.
    ///
    /// Source-grep style guard: `Tui` is hardcoded to a `CrosstermBackend<Stdout>`
    /// terminal, so a behavioral test would require an event-loop-level
    /// harness. We assert directly on the function body.
    #[test]
    fn leave_alt_screen_invalidates_viewport() {
        let src = include_str!("tui_lifecycle.rs");
        let start = src
            .find("pub fn leave_alt_screen")
            .expect("leave_alt_screen function present");
        let body_end = src[start..]
            .find("\n    }\n")
            .expect("leave_alt_screen has a body terminator");
        let body = &src[start..start + body_end];
        assert!(
            body.contains("invalidate_viewport"),
            "leave_alt_screen must call self.terminal.invalidate_viewport() to \
             reset ratatui's inline diff buffer; otherwise typed characters in \
             the composer are invisible until Enter forces a full repaint. \
             Body was:\n{body}"
        );
    }

    #[test]
    fn inline_viewport_height_is_not_used_for_alt_screen_area() {
        let size = Size {
            width: 120,
            height: 36,
        };
        let inline_height = 7;
        let alt_viewport = Tui::alt_screen_viewport(size);

        assert_eq!(alt_viewport.height, size.height);
        assert_ne!(
            alt_viewport.height, inline_height,
            "alt-screen popups must not be clipped to the inline viewport height"
        );
    }
}
