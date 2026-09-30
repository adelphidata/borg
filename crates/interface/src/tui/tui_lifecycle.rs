//! Terminal lifecycle wrapper for borg's inline-viewport TUI.
//!
//! This module owns:
//! - Entering/leaving raw mode + bracketed paste + keyboard enhancement.
//! - The xterm Alternate Scroll Mode (`?1007h`) escape used in alt-screen surfaces.
//! - The `Tui` wrapper around `custom_terminal::Terminal` that drives inline-viewport
//!   rendering, history flushing, and alt-screen transitions for full-screen popups.
//!
//! Event loop, frame ticking, signal handling, suspension, and notification stack live
//! elsewhere; this module owns terminal modes and the inline-viewport wrapper only.

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

/// Terminals whose handling of `REPORT_EVENT_TYPES` we have to special-case.
///
/// Only terminals that *misbehave* earn a variant; everything well-behaved is
/// [`TerminalKind::Other`] and keeps the full flag set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TerminalKind {
    Iterm2,
    Ghostty,
    Other,
}

impl TerminalKind {
    /// Whether this terminal leaks the release half of a shortcut it consumes
    /// into the parent shell.
    fn leaks_key_releases(self) -> bool {
        matches!(self, Self::Iterm2 | Self::Ghostty)
    }
}

/// Classify the host terminal from its environment.
///
/// `TERM` is consulted as a fallback because Ghostty sets `TERM=xterm-ghostty`
/// in launch paths where `TERM_PROGRAM` never reaches the process — over ssh,
/// and under some launchers — and the release-event defect is present there
/// just the same.
///
/// Pure function of its inputs so it is testable without touching process env.
fn terminal_kind_from(term_program: Option<&str>, term: Option<&str>) -> TerminalKind {
    if let Some(program) = term_program {
        if program.eq_ignore_ascii_case("iTerm.app") {
            return TerminalKind::Iterm2;
        }
        if program.eq_ignore_ascii_case("ghostty") {
            return TerminalKind::Ghostty;
        }
    }
    if term.is_some_and(|term| term.eq_ignore_ascii_case("xterm-ghostty")) {
        return TerminalKind::Ghostty;
    }
    TerminalKind::Other
}

/// Which kitty-protocol flags to request from the *current* terminal.
///
/// `DISAMBIGUATE_ESCAPE_CODES` and `REPORT_ALTERNATE_KEYS` are unconditional.
/// `REPORT_EVENT_TYPES` — which asks the terminal to report key *release* and
/// *repeat* in addition to press — is dropped on the transports that mishandle
/// it:
///
/// * **iTerm2 and Ghostty** leak the release half of a shortcut they consume
///   into the parent shell, so quitting borg types a stray character at your
///   prompt.
/// * **tmux with `extended-keys xterm`** loses Shift+Enter entirely when event
///   types are requested; that format cannot encode both.
///
/// Every other terminal (including tmux's `csi-u` format) keeps it, so repeat
/// events stay distinguishable from fresh presses.
///
/// Pure function of its inputs so it is unit-testable without a tty — the
/// terminal probing lives in [`detected_keyboard_transport`].
fn keyboard_enhancement_flags(
    terminal: TerminalKind,
    tmux_extended_keys_format: Option<&str>,
) -> KeyboardEnhancementFlags {
    let flags = KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
        | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS;
    if terminal.leaks_key_releases() || tmux_extended_keys_format == Some("xterm") {
        flags
    } else {
        flags | KeyboardEnhancementFlags::REPORT_EVENT_TYPES
    }
}

/// Probe the host terminal for the inputs [`keyboard_enhancement_flags`] needs.
fn detected_keyboard_transport() -> (TerminalKind, Option<String>) {
    let terminal = terminal_kind_from(
        std::env::var("TERM_PROGRAM").ok().as_deref(),
        std::env::var("TERM").ok().as_deref(),
    );
    let tmux_format = if std::env::var_os("TMUX").is_some() {
        read_tmux_extended_keys_format()
    } else {
        None
    };
    (terminal, tmux_format)
}

/// Read tmux's `extended-keys` option value (`on`/`off`/`always` in newer tmux,
/// `xterm`/`csi-u` for the key *format*). Best-effort: any failure yields `None`,
/// which keeps the default (event types requested).
fn read_tmux_extended_keys_format() -> Option<String> {
    let output = std::process::Command::new("tmux")
        .args(["show-option", "-gqv", "extended-keys-format"])
        .stdin(std::process::Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!value.is_empty()).then_some(value)
}

/// The flags to push on this host, resolved once per call site.
fn resolved_keyboard_enhancement_flags() -> KeyboardEnhancementFlags {
    let (terminal, tmux_format) = detected_keyboard_transport();
    keyboard_enhancement_flags(terminal, tmux_format.as_deref())
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

/// Emit the bytes `set_modes` would write to stdout into a caller-supplied
/// writer. Pulled out so tests can capture the byte stream and assert the
/// "no mouse-tracking escape sequence" invariant at the bytes level — the
/// `include_str!` source-grep guards in `mod.rs` / `app.rs` only catch the
/// literal token in our own source, not a future crossterm version whose
/// `EnableBracketedPaste` / `EnableFocusChange` byte encoding regresses.
///
/// Skips the terminal ioctl (`enable_raw_mode`) since it emits no bytes.
/// Test-only: `set_modes` itself interleaves these emissions with the
/// raw-mode ioctl in a specific historical order, so it does not share this
/// helper — but the *bytes* emitted are identical, which is all the guard
/// needs to verify.
#[cfg(test)]
pub(crate) fn write_set_modes_sequence(
    out: &mut impl std::io::Write,
    suppress_keyboard_enhancement: bool,
) -> Result<()> {
    execute!(out, EnableBracketedPaste)?;
    if !suppress_keyboard_enhancement {
        let _ = execute!(
            out,
            PushKeyboardEnhancementFlags(resolved_keyboard_enhancement_flags())
        );
    }
    let _ = execute!(out, EnableFocusChange);
    Ok(())
}

/// Apply borg's input modes (raw mode, bracketed paste, focus change, optional kitty
/// keyboard enhancement) to the terminal. Inverse of [`restore`].
///
/// Order matters: bracketed paste before raw mode, matching the historical
/// sequence. The two are independent in practice (CSI vs tty ioctl) but
/// preserving the original interleave avoids subtle terminal-quirk regressions.
/// The bytes-level mouse-tracking guard in tests uses
/// [`write_set_modes_sequence`] to cover both branches without needing a tty.
pub fn set_modes() -> Result<()> {
    execute!(stdout(), EnableBracketedPaste)?;
    enable_raw_mode()?;
    if !keyboard_enhancement_disabled() {
        let _ = execute!(
            stdout(),
            PushKeyboardEnhancementFlags(resolved_keyboard_enhancement_flags())
        );
    }
    // Focus reporting is a nicety (it only pauses the throbber when unfocused);
    // terminals that reject the mode still give a fully working TUI.
    let _ = execute!(stdout(), EnableFocusChange);
    Ok(())
}

/// Emit the bytes `enter_alt_screen` would write to stdout into a writer.
/// See [`write_set_modes_sequence`] for the testability rationale.
pub(crate) fn write_enter_alt_screen_sequence(out: &mut impl std::io::Write) -> Result<()> {
    let _ = execute!(out, EnterAlternateScreen);
    let _ = execute!(out, EnableAlternateScroll);
    Ok(())
}

/// Write the teardown escape sequence. Split out from [`restore_common`] so the
/// byte-level guard in tests can assert what the terminal actually receives
/// without needing a tty.
pub(crate) fn write_restore_sequence(out: &mut impl std::io::Write) -> Result<()> {
    // Undo the alt-screen surface unconditionally. `Tui::leave_alt_screen` only
    // runs on the normal reconciliation path, so a panic (or `/uninstall`,
    // which enters the alt screen and returns without leaving it) exited with
    // the alt screen still active and `?1007h` still set — the user's wheel
    // then types arrow keys at their shell prompt. Both are harmless no-ops
    // when the alt screen was never entered, which is why they can be
    // unconditional here.
    let _ = execute!(out, DisableAlternateScroll);
    let _ = execute!(out, LeaveAlternateScreen);
    // Pop may fail on platforms that did not accept the push; ignore errors.
    let _ = execute!(out, PopKeyboardEnhancementFlags);
    execute!(out, DisableBracketedPaste)?;
    let _ = execute!(out, DisableFocusChange);
    Ok(())
}

fn restore_common(should_disable_raw_mode: bool) -> Result<()> {
    write_restore_sequence(&mut stdout())?;
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
    /// Last known terminal size, so an ordinary frame costs no ioctl.
    ///
    /// The size is queried at least twice per draw and cannot change without
    /// one of the events below, so the query was pure overhead on every frame
    /// of every stream. Correctness rests on invalidating it at each point the
    /// terminal can change behind us: a resize event, a Ctrl+Z/`fg` bounce,
    /// and an external program that owned the screen. `Cell` rather than a
    /// `&mut self` signature because reading the size is conceptually a query
    /// and every call site holds `&self`.
    cached_size: SizeCache,
}

/// One-slot cache for the terminal's dimensions.
///
/// Split from [`Tui`] because `Tui` is bound to a live `CrosstermBackend`,
/// which no unit test can construct — the caching rule is the part that must
/// not regress, so it lives where it can be tested.
#[derive(Debug, Default)]
pub struct SizeCache(std::cell::Cell<Option<Size>>);

impl SizeCache {
    /// Return the cached size, or compute and store one with `query`.
    fn get_or_query<E>(
        &self,
        query: impl FnOnce() -> std::result::Result<Size, E>,
    ) -> std::result::Result<Size, E> {
        if let Some(size) = self.0.get() {
            return Ok(size);
        }
        let size = query()?;
        self.0.set(Some(size));
        Ok(size)
    }

    /// Store a size known from a resize event, with no query.
    fn set(&self, size: Size) {
        self.0.set(Some(size));
    }

    /// Drop the stored size so the next read queries again.
    fn invalidate(&self) {
        self.0.set(None);
    }
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
            cached_size: SizeCache::default(),
        })
    }

    pub fn is_alt_screen_active(&self) -> bool {
        self.alt_screen_active.load(Ordering::Relaxed)
    }

    /// Whether the terminal multiplexer requires the newline-fallback insert path.
    fn uses_newline_fallback(&self) -> bool {
        self.is_zellij || self.is_tmux
    }

    /// The current terminal size, served from the cache when one is warm.
    ///
    /// A miss queries the backend and warms the cache. Every path that can
    /// change the real size calls [`Self::note_resize`] or
    /// [`Self::invalidate_size_cache`] first, so a hit is always current.
    pub fn terminal_size(&self) -> Result<Size> {
        self.cached_size.get_or_query(|| self.terminal.size())
    }

    /// Adopt the dimensions carried by a terminal resize event.
    ///
    /// The event is the authoritative notification, so the new size is taken
    /// from it rather than triggering another query.
    pub fn note_resize(&self, width: u16, height: u16) {
        self.cached_size.set(Size { width, height });
    }

    /// Forget the cached size, so the next read queries the terminal.
    ///
    /// Needed wherever something other than Borg owned the screen — a
    /// Ctrl+Z/`fg` bounce or an external program — because a resize during
    /// that window produces no event we ever see.
    pub fn invalidate_size_cache(&self) {
        self.cached_size.invalidate();
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
        // Idempotent: a second entry while already on the alt screen would
        // overwrite `alt_saved_viewport` with the *full-screen* rect, and
        // `leave_alt_screen` would then restore a full-height "inline"
        // viewport instead of the real one.
        if self.is_alt_screen_active() {
            return Ok(());
        }
        let _ = write_enter_alt_screen_sequence(self.terminal.backend_mut());
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
        // An alt-screen surface runs its own event loop, so a resize while it
        // was up was consumed there and never reached the main loop's cache.
        self.invalidate_size_cache();
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
    /// Returns `true` when the insert reports it may have written through the
    /// viewport, signaling that the caller must invalidate the diff buffer for
    /// a full repaint.
    fn flush_pending_history_lines(
        terminal: &mut Terminal,
        pending_history_lines: &mut Vec<Line<'static>>,
        newline_fallback: bool,
    ) -> Result<bool> {
        if pending_history_lines.is_empty() {
            return Ok(false);
        }

        let clobbered_viewport = super::insert_history::insert_history_lines_with_mode(
            terminal,
            pending_history_lines.clone(),
            super::insert_history::InsertHistoryMode::new(newline_fallback),
        )?;
        pending_history_lines.clear();
        Ok(clobbered_viewport)
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

    /// iTerm2 and Ghostty leak the *release* half of a shortcut they consume
    /// into the parent shell when event types are reported, so quitting borg
    /// types a stray character at the user's prompt. Disambiguation and
    /// alternate keys must survive — only event-type reporting is dropped.
    #[test]
    fn release_leaking_terminals_do_not_request_event_type_reporting() {
        for terminal in [TerminalKind::Iterm2, TerminalKind::Ghostty] {
            let flags =
                keyboard_enhancement_flags(terminal, /*tmux_extended_keys_format*/ None);
            assert!(
                !flags.contains(KeyboardEnhancementFlags::REPORT_EVENT_TYPES),
                "{terminal:?} must not request release events"
            );
            assert!(
                flags.contains(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES),
                "{terminal:?} still needs escape-code disambiguation"
            );
            assert!(
                flags.contains(KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS),
                "{terminal:?} still needs alternate-key reporting"
            );
        }
    }

    /// Ghostty is reachable by two independent signals. `TERM_PROGRAM` is
    /// absent over ssh and under some launchers, where `TERM` is the only
    /// evidence — missing that path would leave those sessions leaking.
    #[test]
    fn ghostty_is_detected_from_either_term_program_or_term() {
        assert_eq!(
            terminal_kind_from(Some("ghostty"), Some("xterm-256color")),
            TerminalKind::Ghostty,
            "TERM_PROGRAM alone must identify Ghostty"
        );
        assert_eq!(
            terminal_kind_from(/*term_program*/ None, Some("xterm-ghostty")),
            TerminalKind::Ghostty,
            "TERM alone must identify Ghostty when TERM_PROGRAM is absent"
        );
        assert_eq!(
            terminal_kind_from(Some("Ghostty"), /*term*/ None),
            TerminalKind::Ghostty,
            "detection must not depend on case"
        );
    }

    /// The probe must not mistake a well-behaved terminal for a leaking one:
    /// that would silently cost every user repeat/press discrimination.
    #[test]
    fn terminal_kind_classifies_other_terminals_as_other() {
        for (term_program, term) in [
            (Some("iTerm.app"), None),
            (None, Some("xterm-ghostty")),
            (Some("ghostty"), None),
        ] {
            assert_ne!(
                terminal_kind_from(term_program, term),
                TerminalKind::Other,
                "known-leaking terminal misclassified: {term_program:?}/{term:?}"
            );
        }
        for (term_program, term) in [
            (None, None),
            (Some("Apple_Terminal"), Some("xterm-256color")),
            (Some("WezTerm"), Some("wezterm")),
            (None, Some("xterm-kitty")),
            // Substring lookalikes must not trip detection.
            (Some("ghostty-preview"), Some("xterm-ghostty-ish")),
        ] {
            assert_eq!(
                terminal_kind_from(term_program, term),
                TerminalKind::Other,
                "well-behaved terminal misclassified: {term_program:?}/{term:?}"
            );
        }
    }

    /// Pin the bytes that actually reach the terminal. The flag set is an
    /// internal representation; what fixes the leak is the `>` parameter
    /// Ghostty receives, and only rendering the command proves it.
    #[test]
    fn ghostty_keyboard_enhancement_emits_no_event_type_bit() {
        let mut ghostty: Vec<u8> = Vec::new();
        crossterm::queue!(
            ghostty,
            PushKeyboardEnhancementFlags(keyboard_enhancement_flags(
                TerminalKind::Ghostty,
                /*tmux_extended_keys_format*/ None,
            ))
        )
        .expect("queue keyboard enhancement flags into an in-memory buffer");

        // 1 = disambiguate escape codes, 4 = report alternate keys. The
        // event-types bit (2) must be absent, so the sum is 5, not 7.
        assert_eq!(
            String::from_utf8_lossy(&ghostty),
            "\x1b[>5u",
            "Ghostty must be sent the flag set without event-type reporting"
        );

        let mut other: Vec<u8> = Vec::new();
        crossterm::queue!(
            other,
            PushKeyboardEnhancementFlags(keyboard_enhancement_flags(
                TerminalKind::Other,
                /*tmux_extended_keys_format*/ None,
            ))
        )
        .expect("queue keyboard enhancement flags into an in-memory buffer");
        assert_eq!(
            String::from_utf8_lossy(&other),
            "\x1b[>7u",
            "a well-behaved terminal must still receive event-type reporting"
        );
    }

    /// tmux's `xterm` extended-keys format cannot encode Shift+Enter alongside
    /// event types, and silently drops it. `csi-u` can, so it keeps them.
    #[test]
    fn tmux_event_type_reporting_depends_on_extended_keys_format() {
        let xterm = keyboard_enhancement_flags(TerminalKind::Other, Some("xterm"));
        assert!(
            !xterm.contains(KeyboardEnhancementFlags::REPORT_EVENT_TYPES),
            "tmux xterm format loses Shift+Enter when event types are requested"
        );

        let csi_u = keyboard_enhancement_flags(TerminalKind::Other, Some("csi-u"));
        assert!(
            csi_u.contains(KeyboardEnhancementFlags::REPORT_EVENT_TYPES),
            "csi-u encodes both, so repeat events must stay distinguishable"
        );
    }

    /// Everything else — Kitty, Alacritty, bare xterm, unknown — keeps the full
    /// flag set. This is the default path; narrowing it would lose repeat/press
    /// discrimination for every well-behaved terminal.
    #[test]
    fn other_terminals_keep_full_flag_set() {
        for tmux_format in [None, Some("csi-u")] {
            let flags = keyboard_enhancement_flags(TerminalKind::Other, tmux_format);
            assert_eq!(
                flags,
                KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                    | KeyboardEnhancementFlags::REPORT_EVENT_TYPES
                    | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS,
                "unexpected flags for tmux_format={tmux_format:?}"
            );
        }
    }

    /// Regression guard for the "Mouse Interaction — Hard Invariant" in
    /// AGENTS.md: the byte stream emitted by our terminal-init lifecycle
    /// (set_modes + enter_alt_screen) must contain ZERO mouse-tracking mode
    /// switches. The existing `include_str!` source-grep guards in
    /// `tui/mod.rs` only catch literal occurrences in our own .rs files;
    /// they cannot catch a future crossterm version whose
    /// `EnableBracketedPaste` / `EnableFocusChange` / `EnterAlternateScreen`
    /// emits a different byte sequence — including one that turns on mouse
    /// tracking. This test exercises the actual byte stream.
    ///
    /// If this test fires, native click+drag selection in the terminal will
    /// break. Do not weaken the assertion — investigate why the byte stream
    /// changed.
    #[test]
    fn terminal_init_byte_stream_contains_no_mouse_tracking_modes() {
        let mut buf: Vec<u8> = Vec::new();
        write_set_modes_sequence(&mut buf, /*suppress_keyboard_enhancement=*/ false)
            .expect("set_modes sequence writes to in-memory buffer");
        write_enter_alt_screen_sequence(&mut buf)
            .expect("enter_alt_screen sequence writes to in-memory buffer");
        // Also exercise the kitty-keyboard-suppressed path so the guard
        // covers both branches of set_modes.
        write_set_modes_sequence(&mut buf, /*suppress_keyboard_enhancement=*/ true)
            .expect("set_modes (suppressed) sequence writes to in-memory buffer");

        // The byte sequences are stable ASCII; lossy-decode for substring
        // search. Forbidden tokens are CSI private-mode SETs that, once on,
        // make the terminal capture mouse events instead of letting the
        // user select text.
        let s = String::from_utf8_lossy(&buf);
        for forbidden in ["?1000h", "?1002h", "?1003h", "?1006h"] {
            assert!(
                !s.contains(forbidden),
                "terminal init byte stream contains forbidden mouse-tracking \
                 escape `{forbidden}` — this will regress click+drag text \
                 selection. Captured bytes: {s:?}"
            );
        }
        // Positive assertion: the alt-screen path MUST emit ?1007h
        // (Alternate Scroll Mode). If a future refactor drops it, wheel
        // events stop being translated to arrow keys in transcript pager.
        assert!(
            s.contains("?1007h"),
            "alt-screen entry must enable xterm Alternate Scroll Mode \
             (?1007h); captured bytes: {s:?}"
        );
    }

    #[test]
    fn restore_sequence_leaves_alt_screen_and_disables_alternate_scroll() {
        // Regression: `restore()` is the ONLY teardown the panic hook and
        // `TerminalGuard::drop` run. It never emitted `?1007l` or
        // `LeaveAlternateScreen`, so panicking with a popup or the transcript
        // pager open — and every successful `/uninstall` — exited leaving the
        // terminal on the alt screen with Alternate Scroll Mode still set.
        // The wheel then types arrow keys at the user's shell prompt.
        let mut buf: Vec<u8> = Vec::new();
        write_restore_sequence(&mut buf).expect("restore sequence writes to buffer");
        let s = String::from_utf8_lossy(&buf);

        assert!(
            s.contains("?1007l"),
            "teardown must disable Alternate Scroll Mode; captured: {s:?}"
        );
        assert!(
            s.contains("?1049l"),
            "teardown must leave the alternate screen; captured: {s:?}"
        );
    }

    #[test]
    fn restore_sequence_enables_no_mouse_tracking_mode() {
        // Teardown must never turn a tracking mode *on*.
        let mut buf: Vec<u8> = Vec::new();
        write_restore_sequence(&mut buf).expect("restore sequence writes to buffer");
        let s = String::from_utf8_lossy(&buf);
        for forbidden in ["?1000h", "?1002h", "?1003h", "?1006h", "?1007h"] {
            assert!(
                !s.contains(forbidden),
                "teardown emitted mouse-tracking SET `{forbidden}`; captured: {s:?}"
            );
        }
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

    /// Counts how many times the backend was actually asked for its size.
    struct CountingProbe {
        size: std::cell::Cell<Size>,
        queries: std::cell::Cell<usize>,
    }

    impl CountingProbe {
        fn new(width: u16, height: u16) -> Self {
            Self {
                size: std::cell::Cell::new(Size { width, height }),
                queries: std::cell::Cell::new(0),
            }
        }

        fn query(&self) -> std::result::Result<Size, std::convert::Infallible> {
            self.queries.set(self.queries.get() + 1);
            Ok(self.size.get())
        }
    }

    #[test]
    fn repeated_reads_query_the_terminal_once() {
        // The size is read at least twice per frame; before the cache that was
        // an ioctl per read, on every frame of every stream.
        let probe = CountingProbe::new(120, 40);
        let cache = SizeCache::default();

        for _ in 0..10 {
            assert_eq!(
                cache.get_or_query(|| probe.query()).unwrap(),
                Size {
                    width: 120,
                    height: 40
                }
            );
        }

        assert_eq!(probe.queries.get(), 1, "ordinary frames must not re-query");
    }

    #[test]
    fn a_resize_event_updates_the_size_without_a_query() {
        // The event carries the new geometry, so asking the terminal again
        // would be a second round trip for an answer we already have.
        let probe = CountingProbe::new(120, 40);
        let cache = SizeCache::default();
        cache.get_or_query(|| probe.query()).unwrap();

        cache.set(Size {
            width: 80,
            height: 24,
        });

        assert_eq!(
            cache.get_or_query(|| probe.query()).unwrap(),
            Size {
                width: 80,
                height: 24
            }
        );
        assert_eq!(probe.queries.get(), 1, "the event is the answer");
    }

    #[test]
    fn invalidating_forces_the_next_read_to_ask_the_terminal() {
        // A Ctrl+Z bounce or an external program can be resized while Borg is
        // not reading events, so there is no notification to adopt — the only
        // safe move is to ask again.
        let probe = CountingProbe::new(120, 40);
        let cache = SizeCache::default();
        cache.get_or_query(|| probe.query()).unwrap();

        probe.size.set(Size {
            width: 200,
            height: 60,
        });
        cache.invalidate();

        assert_eq!(
            cache.get_or_query(|| probe.query()).unwrap(),
            Size {
                width: 200,
                height: 60
            },
            "a stale size after a resume would misplace the whole viewport"
        );
        assert_eq!(probe.queries.get(), 2);
    }

    /// Source guard: every path that hands the screen to something else must
    /// drop the cached geometry. A resize during those windows produces no
    /// event, so a missing invalidation is a silently wrong viewport — and it
    /// is invisible in a unit test of the cache itself.
    #[test]
    fn every_screen_handover_invalidates_the_size_cache() {
        let loop_src = include_str!("mod.rs");
        for context in ["AppAction::Suspend", "LaunchExternalEditor"] {
            let start = loop_src
                .find(context)
                .unwrap_or_else(|| panic!("{context} arm present"));
            let window = &loop_src[start..loop_src.len().min(start + 3000)];
            assert!(
                window.contains("invalidate_size_cache"),
                "{context} hands the screen to another program; it must drop the cached size"
            );
        }
        assert!(
            include_str!("tui_lifecycle.rs").contains("self.invalidate_size_cache()"),
            "leave_alt_screen must drop the cached size"
        );
    }
}
